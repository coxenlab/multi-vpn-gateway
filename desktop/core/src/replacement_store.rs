//! 容器替换的持久化进度。候选准备期间不改通道配置；成功后配置、容器和卷一起提交。
//! payload 可能含待应用的凭据，只在内部解密，不实现 Debug/Serialize，也不返回 API。
use std::path::Path;
use anyhow::{anyhow, ensure, Result};
use fernet::Fernet;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Map, Value};

pub struct Record {
    pub channel_id: String,
    pub operation_id: String,
    pub phase: String,
    pub payload: Value,
}

pub struct AppliedRuntime {
    pub container_id: String,
    pub data_volume: String,
    pub novnc_port: Option<i64>,
    pub status: String,
    pub latency_ms: Option<i64>,
}

fn cipher(key: &str) -> Result<Fernet> { Fernet::new(key).ok_or_else(|| anyhow!("替换记录密钥无效")) }

fn check_operation(conn: &Connection, cid: &str, operation: &str, phase: &str) -> Result<()> {
    let stored: Option<(String, String)> = conn.query_row(
        "SELECT operation_id,phase FROM channel_replacements WHERE channel_id=?1", [cid],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional()?;
    ensure!(stored.as_ref().is_some_and(|(op, state)| op == operation && state == phase),
        "容器替换代次或阶段已变化");
    Ok(())
}

/// 第一次替换前沿用原卷名；后续使用成功提交的独立候选卷。
pub fn data_volume(db: &Path, cid: &str) -> Result<String> {
    let conn = Connection::open(db)?;
    Ok(conn.query_row("SELECT data_volume FROM channel_runtime WHERE channel_id=?1", [cid], |r| r.get(0))
        .optional()?.unwrap_or_else(|| format!("vpndata-{cid}")))
}

pub fn begin(db: &Path, key: &str, cid: &str, operation: &str, payload: &Value) -> Result<()> {
    let encrypted = cipher(key)?.encrypt(&serde_json::to_vec(payload)?);
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.query_row("SELECT COUNT(*) FROM channels WHERE id=?1", [cid], |r| r.get::<_, i64>(0))? == 1,
        "通道不存在");
    ensure!(tx.query_row("SELECT COUNT(*) FROM channel_replacements WHERE channel_id=?1", [cid], |r| r.get::<_, i64>(0))? == 0,
        "该通道已有未完成的容器替换");
    tx.execute("INSERT INTO channel_replacements(channel_id,operation_id,phase,payload_enc,created_at) VALUES(?1,?2,'preparing',?3,CAST(strftime('%s','now') AS INTEGER))",
        params![cid, operation, encrypted])?;
    tx.commit()?;
    Ok(())
}

pub fn list(db: &Path, key: &str) -> Result<Vec<Record>> {
    let f = cipher(key)?;
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT channel_id,operation_id,phase,payload_enc FROM channel_replacements ORDER BY created_at,channel_id")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    let mut records = Vec::new();
    for row in rows {
        let (channel_id, operation_id, phase, encrypted) = row?;
        let bytes = f.decrypt(&encrypted).map_err(|_| anyhow!("容器替换记录无法解密"))?;
        records.push(Record { channel_id, operation_id, phase, payload: serde_json::from_slice(&bytes)? });
    }
    Ok(records)
}

pub fn advance(db: &Path, key: &str, record: &Record, next: &str, payload: &Value) -> Result<()> {
    let legal = next == record.phase || matches!((record.phase.as_str(), next),
        ("preparing", "prepared" | "rolling_back") | ("prepared", "switching" | "rolling_back")
        | ("switching", "validating" | "rolling_back") | ("validating", "rolling_back"));
    ensure!(legal && !matches!(next, "committed" | "rolled_back"), "无效的容器替换阶段转换");
    let encrypted = cipher(key)?.encrypt(&serde_json::to_vec(payload)?);
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, &record.phase)?;
    tx.execute("UPDATE channel_replacements SET phase=?1,payload_enc=?2 WHERE channel_id=?3",
        params![next, encrypted, record.channel_id])?;
    tx.commit()?;
    Ok(())
}

fn apply_runtime(conn: &Connection, cid: &str, runtime: &AppliedRuntime) -> Result<()> {
    ensure!(matches!(runtime.status.as_str(), "running" | "logged_in" | "stopped"), "无效的已应用通道状态");
    let changed = conn.execute("UPDATE channels SET container_id=?1,novnc_port=?2,status=?3,latency_ms=?4 WHERE id=?5",
        params![runtime.container_id, runtime.novnc_port, runtime.status, runtime.latency_ms, cid])?;
    ensure!(changed == 1, "通道不存在");
    conn.execute("INSERT INTO channel_runtime(channel_id,data_volume) VALUES(?1,?2) ON CONFLICT(channel_id) DO UPDATE SET data_volume=excluded.data_volume",
        params![cid, runtime.data_volume])?;
    Ok(())
}

/// 只有候选已验证时调用。进程中断后只需读 phase 即可区分整笔提交与未提交。
pub fn commit(db: &Path, key: &str, record: &Record, fields: &Map<String, Value>, secrets: &[String], runtime: &AppliedRuntime) -> Result<()> {
    let f = cipher(key)?;
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "validating")?;
    crate::store::update_channel_on(&tx, &f, &record.channel_id, fields, secrets)?;
    apply_runtime(&tx, &record.channel_id, runtime)?;
    tx.execute("UPDATE channel_replacements SET phase='committed' WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

/// 原配置在准备期间从未改过；只记录已读回确认的恢复运行态，不覆盖期间保存的备注。
pub fn rolled_back(db: &Path, record: &Record, runtime: &AppliedRuntime) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "rolling_back")?;
    apply_runtime(&tx, &record.channel_id, runtime)?;
    tx.execute("UPDATE channel_replacements SET phase='rolled_back' WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

/// 仅在多余容器/卷清理完成后删除记录；未知写结果先 list 读回。
pub fn finish(db: &Path, cid: &str, operation: &str) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let row: Option<(String, String)> = tx.query_row("SELECT operation_id,phase FROM channel_replacements WHERE channel_id=?1", [cid], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    if let Some((op, phase)) = row {
        ensure!(op == operation && matches!(phase.as_str(), "committed" | "rolled_back"), "容器替换尚未完成或代次已变化");
        tx.execute("DELETE FROM channel_replacements WHERE channel_id=?1", [cid])?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn journal_commit_is_atomic_and_preserves_unrelated_notes() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,status,container_id) VALUES('c1','old','running','old-container')", []).unwrap();
        let fields = json!({"name":"new","password":"fixture-password"}).as_object().unwrap().clone();
        begin(&db, &key, "c1", "op1", &json!({"fields": fields})).unwrap();
        assert!(begin(&db, &key, "c1", "op2", &json!({})).is_err());
        let encrypted: String = conn.query_row("SELECT payload_enc FROM channel_replacements", [], |r| r.get(0)).unwrap();
        assert!(!encrypted.contains("fixture-password"));
        assert_eq!(crate::store::get_channel(&db, "c1").unwrap().unwrap().name, "old");
        assert!(finish(&db, "c1", "op1").is_err());
        for phase in ["prepared", "switching", "validating"] {
            let old = list(&db, &key).unwrap().remove(0);
            advance(&db, &key, &old, phase, &old.payload).unwrap();
            assert!(advance(&db, &key, &old, phase, &old.payload).is_err());
        }
        crate::store::set_config_field(&db, &key, "c1", "login_note", "new note", true).unwrap();
        let record = list(&db, &key).unwrap().remove(0);
        let runtime = AppliedRuntime { container_id: "new-container".into(), data_volume: "new-volume".into(), novnc_port: None, status: "running".into(), latency_ms: None };
        conn.execute_batch("CREATE TRIGGER fail_runtime BEFORE INSERT ON channel_runtime BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(commit(&db, &key, &record, &fields, &["password".into()], &runtime).is_err());
        assert_eq!(list(&db, &key).unwrap()[0].phase, "validating");
        assert_eq!(crate::store::get_channel(&db, "c1").unwrap().unwrap().name, "old");
        assert_eq!(data_volume(&db, "c1").unwrap(), "vpndata-c1");
        conn.execute_batch("DROP TRIGGER fail_runtime").unwrap();
        commit(&db, &key, &record, &fields, &["password".into()], &runtime).unwrap();
        assert_eq!(crate::store::get_password(&db, &key, "c1").unwrap(), "fixture-password");
        assert_eq!(crate::store::get_config(&db, &key, "c1").unwrap()["login_note"], "new note");
        assert_eq!(data_volume(&db, "c1").unwrap(), "new-volume");
        assert_eq!(list(&db, &key).unwrap()[0].phase, "committed");
        assert!(finish(&db, "c1", "op2").is_err());
        finish(&db, "c1", "op1").unwrap(); finish(&db, "c1", "op1").unwrap();
        assert!(list(&db, &key).unwrap().is_empty());
    }
}
