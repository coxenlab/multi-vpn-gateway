use std::collections::HashSet;
use std::path::Path;
use rusqlite::Connection;
use rusqlite::OptionalExtension;

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
}
