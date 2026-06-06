use std::collections::HashSet;
use std::path::Path;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use fernet::Fernet;

/// 对照 store.py::init —— 建表 + 补列 + 一次性迁移 + 种子,全部幂等。
pub fn init(db: &Path) -> anyhow::Result<()> {
    if let Some(parent) = db.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(db)?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS channels(
          id TEXT PRIMARY KEY, name TEXT, vpn_type TEXT, server TEXT, ec_ver TEXT,
          login_method TEXT, username TEXT, password_enc TEXT, vnc_password TEXT,
          mac TEXT, novnc_port INTEGER, probe_url TEXT, status TEXT, container_id TEXT);
        CREATE TABLE IF NOT EXISTS domains(
          id INTEGER PRIMARY KEY AUTOINCREMENT, channel_id TEXT, pattern TEXT);
        CREATE TABLE IF NOT EXISTS rules(
          id INTEGER PRIMARY KEY AUTOINCREMENT, channel_id TEXT, kind TEXT, pattern TEXT,
          enabled INTEGER DEFAULT 1);
        CREATE TABLE IF NOT EXISTS mirrors(
          id INTEGER PRIMARY KEY AUTOINCREMENT, host TEXT UNIQUE, priority INTEGER,
          enabled INTEGER DEFAULT 1);
        "#,
    )?;

    let cols = table_columns(&conn, "channels")?;
    if !cols.contains("latency_ms") {
        conn.execute("ALTER TABLE channels ADD COLUMN latency_ms INTEGER", [])?;
    }
    if !cols.contains("config_json") {
        conn.execute("ALTER TABLE channels ADD COLUMN config_json TEXT", [])?;
    }

    let rules_n: i64 = conn.query_row("SELECT COUNT(*) FROM rules", [], |r| r.get(0))?;
    if rules_n == 0 {
        conn.execute(
            "INSERT INTO rules(channel_id,kind,pattern,enabled) \
             SELECT channel_id,'domain',pattern,1 FROM domains",
            [],
        )?;
    }

    let mirrors_n: i64 = conn.query_row("SELECT COUNT(*) FROM mirrors", [], |r| r.get(0))?;
    if mirrors_n == 0 {
        conn.execute(
            "INSERT INTO mirrors(host,priority,enabled) VALUES('docker.1ms.run',1,1),('hub.rat.dev',2,1)",
            [],
        )?;
    }
    Ok(())
}

/// 读取/创建 Fernet 主密钥(命门 #5:0600,缺失才生成)。返回 urlsafe-base64 字符串。
pub fn master_key(data_dir: &Path) -> anyhow::Result<String> {
    std::fs::create_dir_all(data_dir)?;
    let kf = data_dir.join("master.key");
    if !kf.exists() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let key = fernet::Fernet::generate_key();
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&kf)?;
        f.write_all(key.as_bytes())?;
    }
    Ok(std::fs::read_to_string(&kf)?.trim().to_string())
}

/// 解密 Python cryptography.Fernet 密文(命门 #5;用 decrypt() 不带 TTL → 零迁移)。
pub fn decrypt(key: &str, token: &str) -> anyhow::Result<Vec<u8>> {
    let f = fernet::Fernet::new(key).ok_or_else(|| anyhow::anyhow!("invalid fernet key"))?;
    f.decrypt(token).map_err(|e| anyhow::anyhow!("fernet decrypt: {e:?}"))
}

/// 取某通道明文密码(命门 #5:仅供容器注入,绝不接前端路由)。
pub fn get_password(db: &Path, key: &str, cid: &str) -> anyhow::Result<String> {
    let conn = Connection::open(db)?;
    let enc: Option<String> = conn
        .query_row("SELECT password_enc FROM channels WHERE id=?1", [cid], |r| r.get(0))
        .optional()?;
    match enc.filter(|s| !s.is_empty()) {
        Some(tok) => Ok(String::from_utf8_lossy(&decrypt(key, &tok)?).into_owned()),
        None => Ok(String::new()),
    }
}

pub(crate) fn table_columns(conn: &Connection, table: &str) -> anyhow::Result<HashSet<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols = stmt
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<Result<HashSet<_>, _>>()?;
    Ok(cols)
}

use serde::Serialize;
use serde_json::{json, Value};

/// 前端可见的通道(命门 #5:结构体里压根没有 password_enc 字段)。
#[derive(Serialize, Clone, Debug)]
pub struct ChannelPublic {
    pub id: String,
    pub name: String,
    pub vpn_type: String,
    pub server: String,
    pub ec_ver: Option<String>,
    pub login_method: String,
    pub username: String,
    pub vnc_password: Option<String>,
    pub mac: Option<String>,
    pub novnc_port: Option<i64>,
    pub probe_url: String,
    pub status: String,
    pub container_id: Option<String>,
    pub latency_ms: Option<i64>,
    pub config: Value,
}

#[derive(Serialize, Clone, Debug)]
pub struct Rule {
    pub id: i64,
    pub channel_id: String,
    pub kind: String,
    pub pattern: String,
    pub enabled: i64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Mirror {
    pub id: i64,
    pub host: String,
    pub priority: i64,
    pub enabled: i64,
}

/// 对照 _public_config:解析 config_json,丢掉 _secret 列出的字段,返回非 secret 明文 map。
fn public_config(raw: Option<String>) -> Value {
    let Some(raw) = raw.filter(|s| !s.is_empty()) else { return json!({}); };
    let Ok(obj) = serde_json::from_str::<Value>(&raw) else { return json!({}); };
    let secret: HashSet<String> = obj
        .get("_secret")
        .and_then(|s| s.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let mut out = serde_json::Map::new();
    if let Some(fields) = obj.get("_fields").and_then(|f| f.as_object()) {
        for (k, v) in fields {
            if !secret.contains(k) {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Value::Object(out)
}

const CH_COLS: &str =
    "id,name,vpn_type,server,ec_ver,login_method,username,vnc_password,mac,novnc_port,probe_url,status,container_id,latency_ms,config_json";

fn map_channel(row: &rusqlite::Row) -> rusqlite::Result<ChannelPublic> {
    let config_json: Option<String> = row.get(14)?;
    Ok(ChannelPublic {
        id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
        name: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        vpn_type: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        server: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        ec_ver: row.get(4)?,
        login_method: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
        username: row.get::<_, Option<String>>(6)?.unwrap_or_default(),
        vnc_password: row.get(7)?,
        mac: row.get(8)?,
        novnc_port: row.get(9)?,
        probe_url: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
        status: row.get::<_, Option<String>>(11)?.unwrap_or_default(),
        container_id: row.get(12)?,
        latency_ms: row.get(13)?,
        config: public_config(config_json),
    })
}

pub fn list_channels(db: &Path) -> anyhow::Result<Vec<ChannelPublic>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare(&format!("SELECT {CH_COLS} FROM channels"))?;
    let rows = stmt.query_map([], map_channel)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn get_channel(db: &Path, cid: &str) -> anyhow::Result<Option<ChannelPublic>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare(&format!("SELECT {CH_COLS} FROM channels WHERE id=?1"))?;
    let mut rows = stmt.query_map([cid], map_channel)?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

fn map_rule(row: &rusqlite::Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: row.get(0)?,
        channel_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        kind: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
        pattern: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
        enabled: row.get(4)?,
    })
}

pub fn list_rules(db: &Path, cid: &str) -> anyhow::Result<Vec<Rule>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT id,channel_id,kind,pattern,enabled FROM rules WHERE channel_id=?1")?;
    let rows = stmt.query_map([cid], map_rule)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

pub fn all_rules(db: &Path) -> anyhow::Result<Vec<Rule>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT id,channel_id,kind,pattern,enabled FROM rules")?;
    let rows = stmt.query_map([], map_rule)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

// ── config 字段级 Fernet 加解密 (命门 #5) ─────────────────────────────────────

fn fernet_for(key: &str) -> anyhow::Result<Fernet> {
    Fernet::new(key).ok_or_else(|| anyhow::anyhow!("invalid fernet key"))
}

/// 把 JSON 值转成 Python str(v) 等价的字符串(config 值实际几乎都是字符串)。
#[allow(dead_code)]
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// 对照 _clean_field:username/server 去全部空白;其余仅首尾 trim。密码绝不进此函数。
pub fn clean_field(key: &str, val: &str) -> String {
    if key == "username" || key == "server" {
        val.split_whitespace().collect()
    } else {
        val.trim().to_string()
    }
}

/// 对照 get_config_raw:返回通道的 config_json 原文(密文),空或 NULL 则空字符串。
pub fn get_config_raw(db: &Path, cid: &str) -> anyhow::Result<String> {
    let conn = Connection::open(db)?;
    let raw: Option<String> = conn
        .query_row("SELECT config_json FROM channels WHERE id=?1", [cid], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    Ok(raw.unwrap_or_default())
}

/// 命门 #5:完整解密的 config(secret 明文)——仅供容器注入,绝不接前端路由。
pub fn get_config(
    db: &Path,
    key: &str,
    cid: &str,
) -> anyhow::Result<serde_json::Map<String, Value>> {
    let raw = get_config_raw(db, cid)?;
    if raw.is_empty() {
        return Ok(serde_json::Map::new());
    }
    let obj: Value = serde_json::from_str(&raw)?;
    let secret: HashSet<String> = obj
        .get("_secret")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let f = fernet_for(key)?;
    let mut out = serde_json::Map::new();
    if let Some(fields) = obj.get("_fields").and_then(|v| v.as_object()) {
        for (k, v) in fields {
            if secret.contains(k) {
                let tok = v.as_str().unwrap_or_default();
                let pt = f.decrypt(tok).map_err(|e| anyhow::anyhow!("decrypt {k}: {e:?}"))?;
                out.insert(k.clone(), Value::String(String::from_utf8_lossy(&pt).into_owned()));
            } else {
                out.insert(k.clone(), v.clone());
            }
        }
    }
    Ok(out)
}

/// 对照 set_config_field:RMW config_json;secret=true → 密文 + 注册进 _secret(sorted);secret=false → 明文。
pub fn set_config_field(
    db: &Path,
    key: &str,
    cid: &str,
    field: &str,
    value: &str,
    secret: bool,
) -> anyhow::Result<()> {
    let f = fernet_for(key)?;
    let conn = Connection::open(db)?;
    let raw: Option<String> = conn
        .query_row("SELECT config_json FROM channels WHERE id=?1", [cid], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten();
    let mut obj: Value = raw
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({"_fields": {}, "_secret": []}));
    if !obj.get("_fields").map(|v| v.is_object()).unwrap_or(false) {
        obj["_fields"] = json!({});
    }
    if !obj.get("_secret").map(|v| v.is_array()).unwrap_or(false) {
        obj["_secret"] = json!([]);
    }
    let stored = if secret {
        f.encrypt(value.as_bytes())
    } else {
        clean_field(field, value)
    };
    obj["_fields"][field] = Value::String(stored);
    if secret {
        let mut set: std::collections::BTreeSet<String> = obj["_secret"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        set.insert(field.to_string());
        obj["_secret"] = json!(set.into_iter().collect::<Vec<_>>());
    }
    conn.execute(
        "UPDATE channels SET config_json=?1 WHERE id=?2",
        rusqlite::params![obj.to_string(), cid],
    )?;
    Ok(())
}

/// 对照 _enc_config:secret 字段 Fernet 密文,非 secret 走 clean_field;空 config → None;_secret sorted。
fn enc_config(
    f: &Fernet,
    config: &serde_json::Map<String, Value>,
    secret_keys: &[String],
) -> Option<String> {
    if config.is_empty() {
        return None;
    }
    let secret: HashSet<&str> = secret_keys.iter().map(|s| s.as_str()).collect();
    let mut fields = serde_json::Map::new();
    for (k, v) in config {
        let s = value_to_string(v);
        let stored = if secret.contains(k.as_str()) {
            f.encrypt(s.as_bytes())
        } else {
            clean_field(k, &s)
        };
        fields.insert(k.clone(), Value::String(stored));
    }
    let secret_sorted: std::collections::BTreeSet<String> = secret_keys.iter().cloned().collect();
    let secret_vec: Vec<String> = secret_sorted.into_iter().collect();
    Some(json!({ "_fields": fields, "_secret": secret_vec }).to_string())
}

/// add_channel 的输入(密码为明文,落库时 Fernet 加密)。
pub struct NewChannel {
    pub id: String,
    pub name: String,
    pub vpn_type: String,
    pub server: String,
    pub ec_ver: String,
    pub login_method: String,
    pub username: String,
    pub password: String,
    pub vnc_password: String,
    pub mac: String,
    pub probe_url: String,
    pub status: String,
}

/// 对照 add_channel:密码 Fernet(空→'');config 经 enc_config;name/server/username/probe_url 清洗。
pub fn add_channel(
    db: &Path,
    key: &str,
    ch: &NewChannel,
    config: &serde_json::Map<String, Value>,
    secret_keys: &[String],
) -> anyhow::Result<()> {
    let f = fernet_for(key)?;
    let pw_enc = if ch.password.is_empty() { String::new() } else { f.encrypt(ch.password.as_bytes()) };
    let cfg_json = enc_config(&f, config, secret_keys);
    let conn = Connection::open(db)?;
    conn.execute(
        "INSERT INTO channels(id,name,vpn_type,server,ec_ver,login_method,username,password_enc,vnc_password,mac,probe_url,status,config_json) \
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        rusqlite::params![
            ch.id,
            clean_field("name", &ch.name),
            ch.vpn_type,
            clean_field("server", &ch.server),
            ch.ec_ver,
            ch.login_method,
            clean_field("username", &ch.username),
            pw_enc,
            ch.vnc_password,
            ch.mac,
            clean_field("probe_url", &ch.probe_url),
            ch.status,
            cfg_json,
        ],
    )?;
    Ok(())
}

/// 对照 update_channel:仅改 fields 中出现的列(name/server/ec_ver/username/probe_url 清洗;password 加密);
/// 再把 server/username/password 镜像进 config_json(供 oss_connect 读)。
pub fn update_channel(
    db: &Path,
    key: &str,
    cid: &str,
    fields: &serde_json::Map<String, Value>,
    secret_keys: &[String],
) -> anyhow::Result<()> {
    let f = fernet_for(key)?;
    {
        let conn = Connection::open(db)?;
        for col in ["name", "server", "ec_ver", "username", "probe_url"] {
            if let Some(v) = fields.get(col) {
                let s = clean_field(col, &value_to_string(v));
                conn.execute(&format!("UPDATE channels SET {col}=?1 WHERE id=?2"), rusqlite::params![s, cid])?;
            }
        }
        if let Some(v) = fields.get("password") {
            let pw = value_to_string(v);
            let enc = if pw.is_empty() { String::new() } else { f.encrypt(pw.as_bytes()) };
            conn.execute("UPDATE channels SET password_enc=?1 WHERE id=?2", rusqlite::params![enc, cid])?;
        }
    }
    let secret: HashSet<&str> = secret_keys.iter().map(|s| s.as_str()).collect();
    for col in ["server", "username", "password"] {
        if let Some(v) = fields.get(col) {
            let is_secret = secret.contains(col);
            let raw = value_to_string(v);
            let value = if is_secret { raw } else { clean_field(col, &raw) };
            set_config_field(db, key, cid, col, &value, is_secret)?;
        }
    }
    Ok(())
}

pub fn set_container(db: &Path, cid: &str, container_id: &str, novnc: Option<i64>, status: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute(
        "UPDATE channels SET container_id=?1, novnc_port=?2, status=?3 WHERE id=?4",
        rusqlite::params![container_id, novnc, status, cid],
    )?;
    Ok(())
}

pub fn set_status(db: &Path, cid: &str, status: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("UPDATE channels SET status=?1 WHERE id=?2", rusqlite::params![status, cid])?;
    Ok(())
}

pub fn set_novnc_port(db: &Path, cid: &str, port: i64) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("UPDATE channels SET novnc_port=?1 WHERE id=?2", rusqlite::params![port, cid])?;
    Ok(())
}

pub fn set_latency(db: &Path, cid: &str, ms: i64) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("UPDATE channels SET latency_ms=?1 WHERE id=?2", rusqlite::params![ms, cid])?;
    Ok(())
}

/// 对照 del_channel:手动级联(无 FK)删 channels + domains + rules。
pub fn del_channel(db: &Path, cid: &str) -> anyhow::Result<()> {
    let conn = Connection::open(db)?;
    conn.execute("DELETE FROM channels WHERE id=?1", [cid])?;
    conn.execute("DELETE FROM domains WHERE channel_id=?1", [cid])?;
    conn.execute("DELETE FROM rules WHERE channel_id=?1", [cid])?;
    Ok(())
}

pub fn list_mirrors(db: &Path) -> anyhow::Result<Vec<Mirror>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT id,host,priority,enabled FROM mirrors ORDER BY priority")?;
    let rows = stmt.query_map([], |r| {
        Ok(Mirror {
            id: r.get(0)?,
            host: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            priority: r.get(2)?,
            enabled: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn init_creates_tables_seeds_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");

        init(&db).unwrap();

        let conn = rusqlite::Connection::open(&db).unwrap();
        for t in ["channels", "domains", "rules", "mirrors"] {
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [t],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {t} missing");
        }
        let cols = table_columns(&conn, "channels").unwrap();
        assert!(cols.contains("latency_ms"));
        assert!(cols.contains("config_json"));
        let m: i64 = conn.query_row("SELECT COUNT(*) FROM mirrors", [], |r| r.get(0)).unwrap();
        assert_eq!(m, 2);
        drop(conn);

        init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let m: i64 = conn.query_row("SELECT COUNT(*) FROM mirrors", [], |r| r.get(0)).unwrap();
        assert_eq!(m, 2);
    }

    #[test]
    fn migrates_legacy_domains_into_rules() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        {
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE domains(id INTEGER PRIMARY KEY AUTOINCREMENT, channel_id TEXT, pattern TEXT);
                 INSERT INTO domains(channel_id,pattern) VALUES('c1','example.com');",
            ).unwrap();
        }
        init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let (kind, pat): (String, String) = conn
            .query_row("SELECT kind,pattern FROM rules WHERE channel_id='c1'", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap();
        assert_eq!(kind, "domain");
        assert_eq!(pat, "example.com");
    }

    #[test]
    fn decrypts_python_fernet_fixture_zero_migration() {
        let key = include_str!("../tests/fixtures/fernet_key.txt").trim();
        let token = include_str!("../tests/fixtures/fernet_token.txt").trim();
        let plaintext = decrypt(key, token).unwrap();
        assert_eq!(plaintext, b"s3cr3t-password");
    }

    #[test]
    fn master_key_created_0600_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let k1 = master_key(dir.path()).unwrap();
        let kf = dir.path().join("master.key");
        let mode = std::fs::metadata(&kf).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "master.key must be 0600 (命门 #5)");
        let k2 = master_key(dir.path()).unwrap();
        assert_eq!(k1, k2);
        assert!(fernet::Fernet::new(&k1).is_some());
    }

    #[test]
    fn get_password_roundtrips_and_empty_is_blank() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        let f = fernet::Fernet::new(&key).unwrap();
        let enc = f.encrypt(b"hunter2");
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO channels(id,name,password_enc) VALUES('c1','n',?1)",
            [&enc],
        ).unwrap();
        conn.execute(
            "INSERT INTO channels(id,name,password_enc) VALUES('c2','n','')",
            [],
        ).unwrap();
        drop(conn);
        assert_eq!(get_password(&db, &key, "c1").unwrap(), "hunter2");
        assert_eq!(get_password(&db, &key, "c2").unwrap(), "");
    }

    #[test]
    fn list_channels_strips_password_and_secret_config() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let cfg = r#"{"_fields":{"server":"vpn.example.com","password":"ZW5j"},"_secret":["password"]}"#;
        conn.execute(
            "INSERT INTO channels(id,name,vpn_type,server,login_method,username,password_enc,status,config_json) \
             VALUES('abc','客户A','easyconnect','vpn.example.com','interactive','alice','SOME_CIPHER','running',?1)",
            [cfg],
        ).unwrap();
        drop(conn);

        let chans = list_channels(&db).unwrap();
        assert_eq!(chans.len(), 1);
        let v = serde_json::to_value(&chans[0]).unwrap();
        assert!(v.get("password_enc").is_none());
        let config = v.get("config").unwrap();
        assert_eq!(config.get("server").unwrap(), "vpn.example.com");
        assert!(config.get("password").is_none(), "secret config field must be stripped");
        assert_eq!(v.get("username").unwrap(), "alice");
        assert_eq!(v.get("status").unwrap(), "running");
    }

    #[test]
    fn rules_split_and_mirrors_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('c1','domain','a.com',1)", []).unwrap();
        conn.execute("INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('c1','ip','10.0.0.0/8',1)", []).unwrap();
        conn.execute("INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('c2','domain','b.com',0)", []).unwrap();
        drop(conn);

        let r1 = list_rules(&db, "c1").unwrap();
        assert_eq!(r1.len(), 2);
        let all = all_rules(&db).unwrap();
        assert_eq!(all.len(), 3);

        let mirrors = list_mirrors(&db).unwrap();
        assert_eq!(mirrors.len(), 2);
        assert_eq!(mirrors[0].host, "docker.1ms.run");
        assert_eq!(mirrors[0].priority, 1);
    }

    #[test]
    fn clean_field_strips_correctly() {
        assert_eq!(clean_field("username", " a b c "), "abc");
        assert_eq!(clean_field("server", "v p n.com"), "vpn.com");
        assert_eq!(clean_field("name", "  My VPN  "), "My VPN");
        assert_eq!(clean_field("probe_url", " http://x "), "http://x");
    }

    #[test]
    fn config_field_secret_encrypts_registers_and_decrypts() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name) VALUES('c1','n')", []).unwrap();
        drop(conn);

        set_config_field(&db, &key, "c1", "server", "vpn.example.com", false).unwrap();
        set_config_field(&db, &key, "c1", "password", "p@ss w0rd", true).unwrap();

        let raw = get_config_raw(&db, "c1").unwrap();
        let obj: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(obj["_secret"], serde_json::json!(["password"]));
        let pw_cipher = obj["_fields"]["password"].as_str().unwrap();
        assert_ne!(pw_cipher, "p@ss w0rd");
        assert_eq!(obj["_fields"]["server"], "vpn.example.com");

        let cfg = get_config(&db, &key, "c1").unwrap();
        assert_eq!(cfg["password"], "p@ss w0rd");
        assert_eq!(cfg["server"], "vpn.example.com");

        let pub_cfg = public_config(Some(raw));
        assert!(pub_cfg.get("password").is_none());
        assert_eq!(pub_cfg["server"], "vpn.example.com");
    }

    #[test]
    fn get_config_empty_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name) VALUES('c1','n')", []).unwrap();
        drop(conn);
        assert!(get_config(&db, &key, "c1").unwrap().is_empty());
    }

    fn new_ch(id: &str) -> NewChannel {
        NewChannel {
            id: id.into(), name: "客户A".into(), vpn_type: "easyconnect".into(),
            server: " vpn.example.com ".into(), ec_ver: "7.6.3".into(),
            login_method: "interactive".into(), username: " alice ".into(),
            password: "p@ss w0rd".into(), vnc_password: "vnc12345".into(),
            mac: "02:11:22:33:44:55".into(), probe_url: "https://intra/".into(),
            status: "creating".into(),
        }
    }

    #[test]
    fn add_channel_encrypts_password_and_strips_in_readers() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();

        let mut cfg = serde_json::Map::new();
        cfg.insert("server".into(), serde_json::json!("vpn.example.com"));
        cfg.insert("password".into(), serde_json::json!("p@ss w0rd"));
        add_channel(&db, &key, &new_ch("c1"), &cfg, &["password".to_string()]).unwrap();

        assert_eq!(get_password(&db, &key, "c1").unwrap(), "p@ss w0rd");
        let ch = get_channel(&db, "c1").unwrap().unwrap();
        assert_eq!(ch.username, "alice");
        assert_eq!(ch.server, "vpn.example.com");
        let v = serde_json::to_value(&ch).unwrap();
        assert!(v.get("password_enc").is_none());
        assert!(v["config"].get("password").is_none());
        assert_eq!(v["config"]["server"], "vpn.example.com");
        assert_eq!(ch.status, "creating");
    }

    #[test]
    fn add_channel_empty_password_is_blank_not_null() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        let mut ch = new_ch("c2"); ch.password = "".into();
        add_channel(&db, &key, &ch, &serde_json::Map::new(), &[]).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let pw: String = conn.query_row("SELECT password_enc FROM channels WHERE id='c2'", [], |r| r.get(0)).unwrap();
        assert_eq!(pw, "");
        assert_eq!(get_password(&db, &key, "c2").unwrap(), "");
    }

    #[test]
    fn update_channel_changes_present_fields_and_mirrors_config() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        add_channel(&db, &key, &new_ch("c3"), &serde_json::Map::new(), &[]).unwrap();

        let mut fields = serde_json::Map::new();
        fields.insert("username".into(), serde_json::json!(" bob "));
        fields.insert("password".into(), serde_json::json!("newpw"));
        update_channel(&db, &key, "c3", &fields, &["password".to_string()]).unwrap();

        let ch = get_channel(&db, "c3").unwrap().unwrap();
        assert_eq!(ch.username, "bob");
        assert_eq!(get_password(&db, &key, "c3").unwrap(), "newpw");
        let cfg = get_config(&db, &key, "c3").unwrap();
        assert_eq!(cfg["password"], "newpw");
        assert_eq!(cfg["username"], "bob");
        let v = serde_json::to_value(&ch).unwrap();
        assert!(v["config"].get("password").is_none());
    }

    #[test]
    fn channel_state_mutators_and_cascade_delete() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        init(&db).unwrap();
        let key = master_key(dir.path()).unwrap();
        add_channel(&db, &key, &new_ch("c1"), &serde_json::Map::new(), &[]).unwrap();

        set_container(&db, "c1", "deadbeefcid", Some(54321), "running").unwrap();
        set_latency(&db, "c1", 42).unwrap();
        let ch = get_channel(&db, "c1").unwrap().unwrap();
        assert_eq!(ch.container_id.as_deref(), Some("deadbeefcid"));
        assert_eq!(ch.novnc_port, Some(54321));
        assert_eq!(ch.status, "running");
        assert_eq!(ch.latency_ms, Some(42));

        set_status(&db, "c1", "logged_in").unwrap();
        set_novnc_port(&db, "c1", 60000).unwrap();
        let ch = get_channel(&db, "c1").unwrap().unwrap();
        assert_eq!(ch.status, "logged_in");
        assert_eq!(ch.novnc_port, Some(60000));

        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO domains(channel_id,pattern) VALUES('c1','x.com')", []).unwrap();
        conn.execute("INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('c1','domain','x.com',1)", []).unwrap();
        drop(conn);
        del_channel(&db, "c1").unwrap();
        assert!(get_channel(&db, "c1").unwrap().is_none());
        let conn = rusqlite::Connection::open(&db).unwrap();
        let d: i64 = conn.query_row("SELECT COUNT(*) FROM domains WHERE channel_id='c1'", [], |r| r.get(0)).unwrap();
        let r: i64 = conn.query_row("SELECT COUNT(*) FROM rules WHERE channel_id='c1'", [], |r| r.get(0)).unwrap();
        assert_eq!(d, 0);
        assert_eq!(r, 0);
    }
}
