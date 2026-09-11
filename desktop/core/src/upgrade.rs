//! 升级准备只生成受限副本；源目录和 VM 资源始终不变。副本经单独切换验收后才能启用。
use std::{collections::BTreeMap, fs, io::Write, os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt}, path::Path};
use anyhow::{anyhow, ensure, Context, Result};
use rusqlite::{Connection, DatabaseName, OpenFlags, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

const FILES: &[&str] = &["master.key", "infra.json", "config.yaml", "routing_off", "tun-entry.json"];
const TABLES: &[&str] = &["channels", "rules", "domains", "mirrors", "channel_runtime", "channel_replacements", "channel_stop_intents"];
pub const PENDING: &str = "upgrade-pending";

#[derive(Debug, Serialize)]
pub struct Report {
    pub version: u32,
    pub counts: BTreeMap<String, i64>,
    pub encrypted_values_checked: usize,
    pub files: Vec<String>,
    pub database_sha256: String,
    pub runtime_verified: bool,
    pub ready_to_activate: bool,
}

fn read_file(root: &Path, name: &str, required: bool) -> Result<Option<Vec<u8>>> {
    let path = root.join(name);
    match fs::symlink_metadata(&path) {
        Ok(meta) => { ensure!(meta.file_type().is_file(), "{name} 必须是普通文件"); Ok(Some(fs::read(path)?)) },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(None),
        Err(_) => Err(anyhow!("缺少或无法读取 {name}，原数据保持不变")),
    }
}

fn materials(root: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for &name in FILES {
        if let Some(bytes) = read_file(root, name, matches!(name,"master.key"|"infra.json"|"config.yaml"))? {
            files.insert(name.into(),bytes);
        }
    }
    let infra: crate::infra::InfraParams = serde_json::from_slice(&files["infra.json"]).map_err(|_|anyhow!("运行参数格式无效"))?;
    ensure!(infra.mihomo_host_port != 0 && infra.mihomo_ctrl_port != 0 && infra.mihomo_host_port != infra.mihomo_ctrl_port && !infra.secret.is_empty(),"运行参数不完整");
    let config: serde_yaml::Value = serde_yaml::from_slice(&files["config.yaml"]).map_err(|_|anyhow!("启动配置格式无效"))?;
    ensure!(config.get("secret").and_then(|s|s.as_str()) == Some(infra.secret.as_str()),"运行参数与启动配置不匹配，请先核对原版本");
    Ok(files)
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",[name], |_|Ok(())).optional()?.is_some())
}

fn validate_db(path: &Path, key: &[u8]) -> Result<(BTreeMap<String,i64>,usize)> {
    let conn = Connection::open_with_flags(path,OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    ensure!(conn.query_row("PRAGMA quick_check", [], |r|r.get::<_,String>(0))?=="ok","数据库完整性检查失败");
    ensure!(table_exists(&conn,"channels")?,"缺少通道数据表");
    let key = std::str::from_utf8(key).map_err(|_|anyhow!("主密钥格式无效"))?.trim();
    let f = fernet::Fernet::new(key).ok_or_else(||anyhow!("主密钥格式无效"))?;
    let mut checked=0;
    let mut check = |token: &str| -> Result<()> {
        f.decrypt(token).map_err(|_|anyhow!("加密数据与主密钥不匹配，副本未启用"))?;
        checked+=1; Ok(())
    };
    let cols=crate::store::table_columns(&conn,"channels")?;
    ensure!(cols.contains("password_enc"),"通道数据格式不受支持");
    let mut statement=conn.prepare("SELECT password_enc FROM channels WHERE password_enc IS NOT NULL AND password_enc != ''")?;
    for token in statement.query_map([],|r|r.get::<_,String>(0))? { check(&token?)?; }
    if cols.contains("config_json") {
        let mut statement=conn.prepare("SELECT config_json FROM channels WHERE config_json IS NOT NULL AND config_json != ''")?;
        for raw in statement.query_map([],|r|r.get::<_,String>(0))? {
            let value:Value=serde_json::from_str(&raw?).map_err(|_|anyhow!("通道配置格式无效"))?;
            if let Some(secrets)=value["_secret"].as_array() {
                for field in secrets {
                    let field=field.as_str().ok_or_else(||anyhow!("加密字段标记无效"))?;
                    if let Some(token)=value["_fields"].get(field) {
                        check(token.as_str().ok_or_else(||anyhow!("加密字段格式无效"))?)?;
                    }
                }
            }
        }
    }
    if table_exists(&conn,"channel_replacements")? {
        let mut statement=conn.prepare("SELECT payload_enc FROM channel_replacements")?;
        for token in statement.query_map([],|r|r.get::<_,String>(0))? {check(&token?)?;}
    }
    let mut counts=BTreeMap::new();
    for &table in TABLES {
        counts.insert(table.into(),if table_exists(&conn,table)? {conn.query_row(&format!("SELECT COUNT(*) FROM {table}"),[],|r|r.get(0))?} else {0});
    }
    Ok((counts,checked))
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut f=fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    f.write_all(bytes)?; f.sync_all()?; Ok(())
}

/// destination 必须不存在。失败保留受限草稿与 pending 标记，禁止误作新数据目录启动。
/// SQLite backup 同时读取 WAL 已提交内容；复制期间源有变化则拒绝给出完成凭据。
pub fn prepare(source: &Path, destination: &Path) -> Result<Report> {
    let source=source.canonicalize().context("原数据目录不可读取")?;
    ensure!(source.is_dir(),"原数据目录无效");
    let parent=destination.parent().ok_or_else(||anyhow!("副本目录无效"))?.canonicalize()?;
    let destination=parent.join(destination.file_name().ok_or_else(||anyhow!("副本目录无效"))?);
    ensure!(!destination.starts_with(&source),"副本不能放在原数据目录中");
    read_file(&source,"vpnmgr.db",true)?;
    let original=materials(&source)?;
    let conn=Connection::open_with_flags(source.join("vpnmgr.db"),OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let before:i64=conn.query_row("PRAGMA data_version",[],|r|r.get(0))?;
    fs::DirBuilder::new().mode(0o700).create(&destination).context("副本目录必须尚不存在")?;
    write_private(&destination.join(PENDING),b"Upgrade review copy. Runtime activation is not authorized.\n")?;
    for (name,bytes) in &original {write_private(&destination.join(name),bytes)?;}
    let db=destination.join("vpnmgr.db");
    conn.backup(DatabaseName::Main,&db,None).context("数据库副本未完成，请保留原目录")?;
    fs::set_permissions(&db,fs::Permissions::from_mode(0o600))?;
    let (counts,checked)=validate_db(&db,&original["master.key"])?;
    let after:i64=conn.query_row("PRAGMA data_version",[],|r|r.get(0))?;
    ensure!(before==after && original==materials(&source)?,"复制期间原数据发生变化，请在原版本退出后重新准备");
    // 只在副本执行当前 schema 升级；检查后保留原有账号、MAC、卷及恢复操作引用。
    crate::store::init(&db)?;
    validate_db(&db,&original["master.key"])?;
    fs::File::open(&db)?.sync_all()?;
    let report=Report {version:1,counts,encrypted_values_checked:checked,
        files:original.keys().cloned().chain(std::iter::once("vpnmgr.db".into())).collect(),
        database_sha256:format!("{:x}",Sha256::digest(fs::read(&db)?)),runtime_verified:false,ready_to_activate:false};
    write_private(&destination.join("upgrade-review.json"),&serde_json::to_vec_pretty(&report)?)?;
    fs::File::open(&destination)?.sync_all()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source(root:&Path) -> (std::path::PathBuf,Connection) {
        let dir=root.join("original"); fs::create_dir(&dir).unwrap();
        crate::store::init(&dir.join("vpnmgr.db")).unwrap();
        let key=crate::store::master_key(&dir).unwrap();
        let f=fernet::Fernet::new(&key).unwrap();
        fs::write(dir.join("infra.json"),json!({"mihomo_host_port":41001,"mihomo_ctrl_port":41002,"ui_port":41003,"secret":"synthetic-control-key"}).to_string()).unwrap();
        fs::write(dir.join("config.yaml"),"secret: synthetic-control-key\nrules: [MATCH,DIRECT]\n").unwrap();
        fs::write(dir.join("routing_off"),b"1").unwrap();
        let conn=Connection::open(dir.join("vpnmgr.db")).unwrap();
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;").unwrap();
        conn.execute("INSERT INTO channels(id,name,vpn_type,status,container_id,mac,password_enc,config_json) VALUES('c1','fixture','easyconnect','stopped','owned-instance','02:01:02:03:04:05',?1,?2)",
            rusqlite::params![f.encrypt(b"synthetic-password"),json!({"_fields":{"login_note":f.encrypt(b"synthetic-private-note")},"_secret":["login_note"]}).to_string()]).unwrap();
        conn.execute("INSERT INTO channel_runtime VALUES('c1','owned-volume')",[]).unwrap();
        conn.execute("INSERT INTO channel_replacements VALUES('c1','queued-op','queued',?1,0)",[f.encrypt(b"{}")]).unwrap();
        conn.execute("INSERT INTO channel_stop_intents VALUES('c1','stop-op')",[]).unwrap();
        (dir,conn)
    }

    #[test]
    fn prepares_wal_snapshot_preserves_materials_and_cannot_boot() {
        let root=tempfile::tempdir().unwrap(); let (source,conn)=source(root.path());
        let target=root.path().join("review");
        let before=fs::read(source.join("vpnmgr.db")).unwrap();
        let wal=fs::read(source.join("vpnmgr.db-wal")).unwrap();
        let report=prepare(&source,&target).unwrap();
        assert_eq!(report.counts["channels"],1); assert_eq!(report.counts["channel_stop_intents"],1);
        assert_eq!(report.encrypted_values_checked,3); assert!(!report.ready_to_activate && !report.runtime_verified);
        assert_eq!(fs::read(source.join("vpnmgr.db")).unwrap(),before);
        assert_eq!(fs::read(source.join("vpnmgr.db-wal")).unwrap(),wal);
        assert_eq!(materials(&source).unwrap(),materials(&target).unwrap());
        assert_eq!(fs::metadata(&target).unwrap().permissions().mode() & 0o777,0o700);
        for name in report.files { assert_eq!(fs::metadata(target.join(name)).unwrap().permissions().mode() & 0o777,0o600); }
        let copy=Connection::open(target.join("vpnmgr.db")).unwrap();
        let fields=|db:&Connection|db.query_row("SELECT container_id,mac FROM channels",[],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).unwrap();
        assert_eq!(fields(&copy),fields(&conn));
        assert_eq!(copy.query_row("SELECT data_volume FROM channel_runtime",[],|r|r.get::<_,String>(0)).unwrap(),"owned-volume");
        let mut cfg=crate::config::Config::from_getter(|_|None);cfg.data_dir=target;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn invalid_materials_and_existing_destination_never_overwrite_source() {
        let root=tempfile::tempdir().unwrap();let (source,_conn)=source(root.path());
        let target=root.path().join("review");fs::create_dir(&target).unwrap();fs::write(target.join("keep"),b"original").unwrap();
        assert!(prepare(&source,&target).is_err());assert_eq!(fs::read(target.join("keep")).unwrap(),b"original");
        assert!(prepare(&source,&source.join("nested")).is_err());
        fs::write(source.join("master.key"),fernet::Fernet::generate_key()).unwrap();
        let broken=root.path().join("broken");
        assert!(prepare(&source,&broken).is_err());assert!(broken.join(PENDING).exists());assert!(!broken.join("upgrade-review.json").exists());
        fs::remove_file(source.join("infra.json")).unwrap();
        let missing=root.path().join("missing");assert!(prepare(&source,&missing).is_err());assert!(!missing.exists());
    }

    #[test]
    fn rejects_symlinked_database_and_config_mismatch() {
        let root=tempfile::tempdir().unwrap();let (source,_conn)=source(root.path());
        fs::write(source.join("config.yaml"),"secret: different\n").unwrap();
        assert!(prepare(&source,&root.path().join("different")).is_err());
        let other=root.path().join("other");fs::rename(source.join("vpnmgr.db"),&other).unwrap();
        std::os::unix::fs::symlink(&other,source.join("vpnmgr.db")).unwrap();
        assert!(prepare(&source,&root.path().join("symlink")).is_err());
    }
}
