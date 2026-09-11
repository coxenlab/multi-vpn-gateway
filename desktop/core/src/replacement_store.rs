//! 容器替换的持久化进度。候选准备期间不改通道配置；成功后配置、容器和卷一起提交。
//! payload 可能含待应用的凭据，只在内部解密，不实现 Debug/Serialize，也不返回 API。
use std::path::Path;
use anyhow::{anyhow, ensure, Result};
use fernet::Fernet;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde_json::{json, Map, Value};

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
    begin_after_queue(db, key, cid, operation, payload, None)
}

/// 接续离线保存时，以同一事务把指定代次的待应用设置交给替换流程。
pub fn begin_after_queue(db: &Path, key: &str, cid: &str, operation: &str, payload: &Value, queued: Option<&Record>) -> Result<()> {
    let encrypted = cipher(key)?.encrypt(&serde_json::to_vec(payload)?);
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.query_row("SELECT COUNT(*) FROM channels WHERE id=?1", [cid], |r| r.get::<_, i64>(0))? == 1,
        "通道不存在");
    if let Some(queued) = queued {
        ensure!(queued.channel_id == cid && queued.phase == "queued", "待应用设置与通道不匹配");
        check_operation(&tx, cid, &queued.operation_id, "queued")?;
        tx.execute("UPDATE channel_replacements SET operation_id=?2,phase='preparing',payload_enc=?3 WHERE channel_id=?1",
            params![cid, operation, encrypted])?;
    } else {
        ensure!(tx.query_row("SELECT COUNT(*) FROM channel_replacements WHERE channel_id=?1", [cid], |r| r.get::<_, i64>(0))? == 0,
            "该通道已有未完成的容器替换");
        tx.execute("INSERT INTO channel_replacements(channel_id,operation_id,phase,payload_enc,created_at) VALUES(?1,?2,'preparing',?3,CAST(strftime('%s','now') AS INTEGER))",
            params![cid, operation, encrypted])?;
    }
    tx.commit()?;
    Ok(())
}

/// 仅保存连接参数意图；原连接设置保持不变，供真正替换时回滚。
/// 名称、验证地址、分流开关即时保存；两部分共用一个事务。
pub fn queue_settings(db: &Path, key: &str, ch: &crate::store::ChannelPublic, fields: &Map<String, Value>, secrets: &[String]) -> Result<()> {
    let prior = get(db, key, &ch.id)?;
    ensure!(prior.as_ref().is_none_or(|r| r.phase == "queued"), "上次修改尚未完成");
    let mut desired = prior.as_ref().and_then(|r| r.payload["fields"].as_object()).cloned().unwrap_or_default();
    let mut immediate = fields.clone();
    for field in ["server", "username", "password", "ec_ver"] {
        if let Some(value) = immediate.remove(field) {
            ensure!(value.is_string(), "连接设置必须是文本");
            let raw = value.as_str().unwrap_or_default();
            desired.insert(field.into(), json!(if field == "password" { raw.to_string() } else { crate::store::clean_field(field, raw) }));
        }
    }
    let original = serde_json::to_value(ch)?;
    let password = crate::store::get_password(db, key, &ch.id)?;
    desired.retain(|field, value| {
        let before = if field == "password" { password.as_str() } else { original[field].as_str().unwrap_or_default() };
        value.as_str() != Some(before)
    });
    let payload = json!({"fields":desired,"old_id":ch.container_id,"old_volume":data_volume(db,&ch.id)?});
    let f = cipher(key)?;
    let encrypted = f.encrypt(&serde_json::to_vec(&payload)?);
    let operation: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.query_row("SELECT COUNT(*) FROM channels WHERE id=?1", [&ch.id], |r|r.get::<_,i64>(0))? == 1, "通道不存在");
    if let Some(prior) = prior.as_ref() { check_operation(&tx, &ch.id, &prior.operation_id, "queued")?; }
    else { ensure!(tx.query_row("SELECT COUNT(*) FROM channel_replacements WHERE channel_id=?1", [&ch.id], |r|r.get::<_,i64>(0))? == 0, "上次修改已变化"); }
    crate::store::update_channel_on(&tx, &f, &ch.id, &immediate, secrets)?;
    if desired.is_empty() {
        tx.execute("DELETE FROM channel_replacements WHERE channel_id=?1", [&ch.id])?;
    } else {
        tx.execute("INSERT INTO channel_replacements(channel_id,operation_id,phase,payload_enc,created_at) VALUES(?1,?2,'queued',?3,CAST(strftime('%s','now') AS INTEGER)) ON CONFLICT(channel_id) DO UPDATE SET operation_id=excluded.operation_id,payload_enc=excluded.payload_enc",
            params![ch.id,operation,encrypted])?;
    }
    tx.commit()?;
    Ok(())
}

pub fn cancel_queued(db: &Path, record: &Record) -> Result<()> {
    ensure!(record.phase == "queued", "该修改已经开始应用，请使用恢复流程");
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "queued")?;
    tx.execute("DELETE FROM channel_replacements WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

/// UI 展示已保存的意图，运行/分流层仍读取原设置。密码不进入响应。
pub fn overlay_queued(db: &Path, cid: &str, value: &mut Value) -> Result<()> {
    if value["replacement"]["phase"] != "queued" { return Ok(()); }
    let key = crate::store::master_key(db.parent().ok_or_else(||anyhow!("数据目录缺失"))?)?;
    let record = get(db, &key, cid)?.ok_or_else(||anyhow!("待应用设置已变化，请刷新"))?;
    ensure!(record.phase == "queued", "待应用设置已变化，请刷新");
    for field in ["server", "username", "ec_ver"] {
        if let Some(desired) = record.payload["fields"].get(field) {
            value[field] = desired.clone();
            if field != "ec_ver" {
                if let Some(config) = value["config"].as_object_mut() { config.insert(field.into(), desired.clone()); }
            }
        }
    }
    value["deferred"] = json!(true);
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

pub fn get(db: &Path, key: &str, cid: &str) -> Result<Option<Record>> {
    let conn = Connection::open(db)?;
    let row: Option<(String, String, String)> = conn.query_row(
        "SELECT operation_id,phase,payload_enc FROM channel_replacements WHERE channel_id=?1", [cid],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    ).optional()?;
    row.map(|(operation_id, phase, encrypted)| {
        let bytes = cipher(key)?.decrypt(&encrypted).map_err(|_| anyhow!("容器替换记录无法解密"))?;
        Ok(Record { channel_id: cid.into(), operation_id, phase, payload: serde_json::from_slice(&bytes)? })
    }).transpose()
}

/// 管理页面只读取阶段，不解密或返回内部配置。
pub fn public_status(db: &Path, cid: &str) -> Result<Option<Value>> {
    let conn = Connection::open(db)?;
    let phase: Option<String> = conn.query_row("SELECT phase FROM channel_replacements WHERE channel_id=?1", [cid], |r| r.get(0)).optional()?;
    Ok(phase.map(|phase| serde_json::json!({"can_restore":!matches!(phase.as_str(), "committed" | "rolled_back" | "deleting"),"phase":phase})))
}

pub fn has_active_replacements(db: &Path) -> Result<bool> {
    let conn = Connection::open(db)?;
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM channel_replacements WHERE phase!='queued')", [], |r| r.get(0))?)
}

/// 替换过程的旧实例/候选不能被孤儿清理抢先删除；仅读身份列。
pub fn protected_names(db: &Path) -> Result<std::collections::HashMap<String, String>> {
    let conn = Connection::open(db)?;
    let mut stmt = conn.prepare("SELECT channel_id,operation_id FROM channel_replacements")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut names = std::collections::HashMap::new();
    for row in rows {
        let (channel, operation) = row?;
        let owner = crate::replacement_docker::Owner { channel: channel.clone(), operation: operation.clone() }; owner.validate()?;
        for name in [format!("vpn-{channel}"), format!("vpn-{channel}-previous-{operation}"), owner.candidate_name(), owner.copy_name(), format!("vpn-{channel}-next-{operation}-restore")] {
            names.insert(name, channel.clone());
        }
    }
    Ok(names)
}

pub fn advance(db: &Path, key: &str, record: &Record, next: &str, payload: &Value) -> Result<()> {
    let legal = next == record.phase || matches!((record.phase.as_str(), next),
        ("preparing", "prepared" | "rolling_back") | ("prepared", "switching" | "rolling_back")
        | ("switching", "validating" | "rolling_back") | ("validating", "rolling_back")
        | ("awaiting_login", "rolling_back"));
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
    apply(db, key, record, fields, secrets, runtime, false)
}

/// GUI 仅确认容器可运行时保留旧实例；真实 SOCKS 探活成功后才进入可清理终态。
pub fn apply(db: &Path, key: &str, record: &Record, fields: &Map<String, Value>, secrets: &[String], runtime: &AppliedRuntime, awaiting_login: bool) -> Result<()> {
    let f = cipher(key)?;
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "validating")?;
    crate::store::update_channel_on(&tx, &f, &record.channel_id, fields, secrets)?;
    apply_runtime(&tx, &record.channel_id, runtime)?;
    tx.execute("UPDATE channel_replacements SET phase=?1 WHERE channel_id=?2",
        params![if awaiting_login { "awaiting_login" } else { "committed" }, record.channel_id])?;
    tx.commit()?;
    Ok(())
}

pub fn confirm(db: &Path, record: &Record) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "awaiting_login")?;
    tx.execute("UPDATE channel_replacements SET phase='committed' WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

pub fn resumed(db: &Path, record: &Record, runtime: &AppliedRuntime) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "awaiting_login")?;
    apply_runtime(&tx, &record.channel_id, runtime)?;
    tx.commit()?;
    Ok(())
}

/// 原配置在准备期间从未改过；只记录已读回确认的恢复运行态，不覆盖期间保存的备注。
pub fn rolled_back(db: &Path, record: &Record, runtime: &AppliedRuntime) -> Result<()> {
    restore(db, None, record, None, &[], runtime)
}

/// 已应用但尚待人工登录的修改也可补偿；只还原本次字段，不覆盖期间新增备注。
pub fn restore(db: &Path, key: Option<&str>, record: &Record, fields: Option<&Map<String, Value>>, secrets: &[String], runtime: &AppliedRuntime) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "rolling_back")?;
    if let Some(fields) = fields {
        let f = cipher(key.ok_or_else(|| anyhow!("恢复配置缺少密钥"))?)?;
        crate::store::update_channel_on(&tx, &f, &record.channel_id, fields, secrets)?;
    }
    apply_runtime(&tx, &record.channel_id, runtime)?;
    tx.execute("UPDATE channel_replacements SET phase='rolled_back' WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

/// 初次创建或旧实例丢失的补偿：只还原设置和卷，不编造运行中的实例。
pub fn restore_absent(db: &Path, key: &str, record: &Record, fields: Option<&Map<String, Value>>, secrets: &[String], volume: &str, stopped: bool) -> Result<()> {
    ensure!(record.payload["kind"] == "initial", "操作不是无旧实例创建");
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "rolling_back")?;
    if let Some(fields) = fields {
        crate::store::update_channel_on(&tx, &cipher(key)?, &record.channel_id, fields, secrets)?;
    }
    let changed = tx.execute("UPDATE channels SET container_id=NULL,novnc_port=NULL,latency_ms=NULL,status=?1 WHERE id=?2",
        params![if stopped { "stopped" } else { "error" }, record.channel_id])?;
    ensure!(changed == 1, "通道不存在");
    tx.execute("INSERT INTO channel_runtime(channel_id,data_volume) VALUES(?1,?2) ON CONFLICT(channel_id) DO UPDATE SET data_volume=excluded.data_volume",
        params![record.channel_id, volume])?;
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

pub fn request_delete(db: &Path, record: &Record) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, &record.phase)?;
    tx.execute("UPDATE channel_replacements SET phase='deleting' WHERE channel_id=?1", [&record.channel_id])?;
    tx.commit()?;
    Ok(())
}

/// 仅在替换关联容器全部确认已删除后调用；中断后 deleting 保留，不能恢复成一次登录。
pub fn deleted(db: &Path, record: &Record) -> Result<()> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    check_operation(&tx, &record.channel_id, &record.operation_id, "deleting")?;
    for (table, column) in [("channels", "id"), ("domains", "channel_id"), ("rules", "channel_id"), ("channel_runtime", "channel_id"), ("channel_replacements", "channel_id")] {
        tx.execute(&format!("DELETE FROM {table} WHERE {column}=?1"), [&record.channel_id])?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn queued_settings_preserve_original_secrets_and_allow_cancel() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,server,status,container_id) VALUES('c1','old','old.example','stopped','original')", []).unwrap();
        let secrets = vec!["password".into()];
        crate::store::update_channel(&db,&key,"c1",json!({"password":"old-secret"}).as_object().unwrap(),&secrets).unwrap();
        let ch = crate::store::get_channel(&db,"c1").unwrap().unwrap();
        queue_settings(&db,&key,&ch,json!({"server":"new.example","password":" new-secret ","name":"renamed"}).as_object().unwrap(),&secrets).unwrap();
        let current = crate::store::get_channel(&db,"c1").unwrap().unwrap();
        assert_eq!(current.server,"old.example"); assert_eq!(current.name,"renamed");
        assert_eq!(current.container_id.as_deref(),Some("original"));
        assert_eq!(crate::store::get_password(&db,&key,"c1").unwrap(),"old-secret");
        let queued = get(&db,&key,"c1").unwrap().unwrap();
        assert_eq!(queued.payload["fields"]["password"]," new-secret ");
        let encrypted: String = conn.query_row("SELECT payload_enc FROM channel_replacements", [], |r|r.get(0)).unwrap();
        assert!(!encrypted.contains("new-secret"));
        let mut public = json!({"replacement":public_status(&db,"c1").unwrap(),"server":"old.example"});
        overlay_queued(&db,"c1",&mut public).unwrap();
        assert_eq!(public["server"],"new.example"); assert!(!public.to_string().contains("new-secret"));
        assert!(!has_active_replacements(&db).unwrap());
        cancel_queued(&db,&queued).unwrap();
        assert!(get(&db,&key,"c1").unwrap().is_none());
        assert_eq!(crate::store::get_channel(&db,"c1").unwrap().unwrap().name,"renamed");
    }

    #[test]
    fn queued_settings_transition_is_atomic_and_rejects_stale_edits() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,server,password_enc) VALUES('c1','old','old.example','')", []).unwrap();
        let ch = crate::store::get_channel(&db,"c1").unwrap().unwrap();
        let fields = json!({"server":"first.example","name":"first"});
        conn.execute_batch("CREATE TRIGGER fail_queue BEFORE INSERT ON channel_replacements BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
        assert!(queue_settings(&db,&key,&ch,fields.as_object().unwrap(),&[]).is_err());
        assert_eq!(crate::store::get_channel(&db,"c1").unwrap().unwrap().name,"old");
        conn.execute_batch("DROP TRIGGER fail_queue").unwrap();
        queue_settings(&db,&key,&ch,fields.as_object().unwrap(),&[]).unwrap();
        let stale = get(&db,&key,"c1").unwrap().unwrap();
        queue_settings(&db,&key,&ch,json!({"server":"old.example","name":"kept"}).as_object().unwrap(),&[]).unwrap();
        assert!(get(&db,&key,"c1").unwrap().is_none());
        assert_eq!(crate::store::get_channel(&db,"c1").unwrap().unwrap().name,"kept");
        queue_settings(&db,&key,&ch,json!({"server":"second.example"}).as_object().unwrap(),&[]).unwrap();
        assert!(cancel_queued(&db,&stale).is_err());
        assert!(begin_after_queue(&db,&key,"c1","next",&json!({}),Some(&stale)).is_err());
        let current = get(&db,&key,"c1").unwrap().unwrap();
        assert_eq!(current.payload["fields"]["server"],"second.example");
        begin_after_queue(&db,&key,"c1","next",&json!({"prepared":true}),Some(&current)).unwrap();
        assert!(has_active_replacements(&db).unwrap());
        assert!(cancel_queued(&db,&current).is_err());
        assert_eq!(get(&db,&key,"c1").unwrap().unwrap().phase,"preparing");
    }

    #[test]
    fn restore_absent_is_atomic_and_keeps_notes() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,status,container_id) VALUES('c1','new','running','candidate')", []).unwrap();
        crate::store::set_config_field(&db,&key,"c1","login_note","saved during initial attempt",true).unwrap();
        begin(&db,&key,"c1","op1",&json!({"kind":"initial"})).unwrap();
        let record = get(&db,&key,"c1").unwrap().unwrap();
        advance(&db,&key,&record,"rolling_back",&record.payload).unwrap();
        let record = get(&db,&key,"c1").unwrap().unwrap();
        let fields = json!({"name":"old"});
        conn.execute_batch("CREATE TRIGGER fail_initial BEFORE INSERT ON channel_runtime BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(restore_absent(&db,&key,&record,fields.as_object(),&[],"vpndata-c1",false).is_err());
        let ch = crate::store::get_channel(&db,"c1").unwrap().unwrap();
        assert_eq!(ch.container_id.as_deref(),Some("candidate")); assert_eq!(ch.name,"new");
        conn.execute_batch("DROP TRIGGER fail_initial").unwrap();
        restore_absent(&db,&key,&record,fields.as_object(),&[],"vpndata-c1",false).unwrap();
        let ch = crate::store::get_channel(&db,"c1").unwrap().unwrap();
        assert!(ch.container_id.is_none()); assert_eq!(ch.status,"error"); assert_eq!(ch.name,"old");
        assert_eq!(data_volume(&db,"c1").unwrap(),"vpndata-c1");
        assert_eq!(crate::store::get_config(&db,&key,"c1").unwrap()["login_note"],"saved during initial attempt");
        assert!(restore_absent(&db,&key,&record,None,&[],"vpndata-c1",false).is_err());
    }

    #[test]
    fn deletion_keeps_its_intent_until_resources_are_confirmed_removed() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        Connection::open(&db).unwrap().execute("INSERT INTO channels(id,name) VALUES('c1','test')", []).unwrap();
        begin(&db,&key,"c1","op1",&json!({})).unwrap();
        assert!(crate::store::del_channel(&db,"c1").is_err());
        request_delete(&db,&get(&db,&key,"c1").unwrap().unwrap()).unwrap();
        assert_eq!(public_status(&db,"c1").unwrap().unwrap(),json!({"phase":"deleting","can_restore":false}));
        let mut record = get(&db,&key,"c1").unwrap().unwrap(); record.operation_id="stale".into();
        assert!(deleted(&db,&record).is_err());
        deleted(&db,&get(&db,&key,"c1").unwrap().unwrap()).unwrap();
        assert!(crate::store::get_channel(&db,"c1").unwrap().is_none());
        assert!(get(&db,&key,"c1").unwrap().is_none());
    }

    #[test]
    fn pending_login_restore_is_atomic_and_keeps_unrelated_notes() {
        let dir = tempfile::tempdir().unwrap(); let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let key = crate::store::master_key(dir.path()).unwrap();
        let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,status) VALUES('c1','old','running')", []).unwrap();
        begin(&db, &key, "c1", "op1", &json!({})).unwrap();
        for phase in ["prepared", "switching", "validating"] {
            let record = get(&db, &key, "c1").unwrap().unwrap();
            advance(&db, &key, &record, phase, &record.payload).unwrap();
        }
        let runtime = AppliedRuntime { container_id: "candidate".into(), data_volume: "new-volume".into(), novnc_port: None, status: "running".into(), latency_ms: None };
        apply(&db, &key, &get(&db, &key, "c1").unwrap().unwrap(), json!({"name":"new"}).as_object().unwrap(), &[], &runtime, true).unwrap();
        assert_eq!(public_status(&db, "c1").unwrap().unwrap()["can_restore"], true);
        assert!(protected_names(&db).unwrap().contains_key("vpn-c1-previous-op1"));
        assert!(finish(&db, "c1", "op1").is_err());
        crate::store::set_config_field(&db, &key, "c1", "login_note", "during login", true).unwrap();
        let pending = get(&db, &key, "c1").unwrap().unwrap();
        advance(&db, &key, &pending, "rolling_back", &pending.payload).unwrap();
        let record = get(&db, &key, "c1").unwrap().unwrap();
        conn.execute_batch("CREATE TRIGGER fail_restore BEFORE UPDATE OF container_id ON channels BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        assert!(restore(&db, Some(&key), &record, Some(json!({"name":"old"}).as_object().unwrap()), &[], &runtime).is_err());
        assert_eq!(crate::store::get_channel(&db, "c1").unwrap().unwrap().name, "new");
        conn.execute_batch("DROP TRIGGER fail_restore").unwrap();
        restore(&db, Some(&key), &record, Some(json!({"name":"old"}).as_object().unwrap()), &[], &runtime).unwrap();
        assert_eq!(crate::store::get_channel(&db, "c1").unwrap().unwrap().name, "old");
        assert_eq!(crate::store::get_config(&db, &key, "c1").unwrap()["login_note"], "during login");
        assert!(confirm(&db, &pending).is_err());
    }

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
