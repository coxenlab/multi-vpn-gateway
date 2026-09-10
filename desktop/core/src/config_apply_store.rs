//! mihomo 配置应用记录。调用方串行化投递，真实读回后才确认；摘要不包含配置正文。
use std::path::Path;
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::Serialize;
use crate::store::{ChannelPublic, Rule};

#[derive(Debug, Clone)]
pub struct Ticket {
    pub revision: i64,
    pub routing_off: bool,
    pub generation: i64,
    pub attempt: i64,
    pub digest: String,
}

pub struct Snapshot {
    pub revision: i64,
    pub channels: Vec<ChannelPublic>,
    pub rules: Vec<Rule>,
}

#[derive(Serialize, Debug)]
pub struct Status {
    pub source_revision: i64,
    pub desired_revision: i64,
    pub desired_routing_off: Option<bool>,
    pub desired_generation: i64,
    pub attempt: i64,
    pub desired_hash: String,
    pub applied_generation: i64,
    pub applied_hash: String,
    pub verified_at: Option<i64>,
    pub last_error: Option<String>,
    pub pending: bool,
}

/// 一次读事务获取来源版本、通道和有效规则，避免并发保存造成混合快照。
pub fn snapshot(db: &Path, routing_off: bool) -> Result<Snapshot> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction()?;
    let revision = tx.query_row("SELECT source_revision FROM config_apply_state WHERE id=1", [], |r| r.get(0))?;
    let channels = crate::store::list_channels_with(&tx)?;
    let rules = crate::store::effective_rules_with(&tx, routing_off)?;
    Ok(Snapshot { revision, channels, rules })
}

pub fn status(db: &Path, routing_off: bool) -> Result<Status> {
    let conn = Connection::open(db)?;
    let mut state = conn.query_row(
        "SELECT source_revision,desired_revision,desired_routing_off,desired_generation,desired_hash,\
         applied_generation,applied_hash,verified_at,last_error,attempt FROM config_apply_state WHERE id=1", [],
        |r| Ok(Status {
            source_revision: r.get(0)?, desired_revision: r.get(1)?, desired_routing_off: r.get(2)?,
            desired_generation: r.get(3)?, desired_hash: r.get(4)?, applied_generation: r.get(5)?,
            applied_hash: r.get(6)?, verified_at: r.get(7)?, last_error: r.get(8)?, attempt: r.get(9)?, pending: true,
        }))?;
    state.pending = state.verified_at.is_none() || state.last_error.is_some()
        || state.source_revision != state.desired_revision || state.desired_routing_off != Some(routing_off)
        || state.desired_generation != state.applied_generation || state.desired_hash != state.applied_hash;
    Ok(state)
}

pub fn prepare(db: &Path, revision: i64, routing_off: bool, digest: &str) -> Result<Ticket> {
    ensure!(digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)), "配置摘要无效");
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (current, generation, previous, attempt): (i64, i64, String, i64) = tx.query_row(
        "SELECT source_revision,desired_generation,desired_hash,attempt FROM config_apply_state WHERE id=1", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    ensure!(current == revision, "配置来源已变化");
    let generation = generation + i64::from(previous != digest);
    let attempt = attempt + 1;
    tx.execute("UPDATE config_apply_state SET desired_revision=?1,desired_routing_off=?2,\
        desired_generation=?3,desired_hash=?4,attempt=?5,last_error='unconfirmed' WHERE id=1",
        params![revision, routing_off, generation, digest, attempt])?;
    tx.commit()?;
    Ok(Ticket { revision, routing_off, generation, attempt, digest: digest.into() })
}

/// 确认的是此 ticket 的历史版本；源数据随后有变化时 status 仍然 pending。
pub fn confirmed(db: &Path, ticket: &Ticket) -> Result<()> {
    let conn = Connection::open(db)?;
    let changed = conn.execute("UPDATE config_apply_state SET applied_generation=desired_generation,\
        applied_hash=desired_hash,verified_at=CAST(strftime('%s','now') AS INTEGER),last_error=NULL \
        WHERE id=1 AND desired_revision=?1 AND desired_routing_off=?2 AND desired_generation=?3 AND desired_hash=?4 AND attempt=?5",
        params![ticket.revision, ticket.routing_off, ticket.generation, ticket.digest, ticket.attempt])?;
    ensure!(changed == 1, "配置应用代次已变化");
    Ok(())
}

pub fn failed(db: &Path, ticket: &Ticket, code: &str) -> Result<()> {
    // 只持久化原因码，避免控制器错误正文中的凭据流入公开状态。
    ensure!(matches!(code, "write_failed" | "delivery_failed" | "reload_failed" | "readback_failed" | "readback_mismatch"), "配置错误码无效");
    let conn = Connection::open(db)?;
    let changed = conn.execute("UPDATE config_apply_state SET last_error=?1 WHERE id=1 \
        AND desired_revision=?2 AND desired_routing_off=?3 AND desired_generation=?4 AND desired_hash=?5 AND attempt=?6",
        params![code, ticket.revision, ticket.routing_off, ticket.generation, ticket.digest, ticket.attempt])?;
    ensure!(changed == 1, "配置应用代次已变化");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        Connection::open(&db).unwrap().execute(
            "INSERT INTO channels(id,name,status) VALUES('a','A','running')", []).unwrap();
        crate::store::add_rule(&db, "a", "domain", "example.test").unwrap();
        (dir, db)
    }

    fn prepare_now(db: &Path, digest: char) -> Ticket {
        let snap = snapshot(db, false).unwrap();
        prepare(db, snap.revision, false, &digest.to_string().repeat(64)).unwrap()
    }

    #[test]
    fn saved_changes_pending_atomically_metadata_and_probe_do_not_dirty() {
        let (_dir, db) = db();
        let first = prepare_now(&db, 'a');
        confirmed(&db, &first).unwrap();
        assert!(!status(&db, false).unwrap().pending);
        let mut conn = Connection::open(&db).unwrap();
        conn.execute("UPDATE channels SET name='renamed',status='logged_in',latency_ms=12", []).unwrap();
        conn.execute("UPDATE rules SET note='keep',locked=1", []).unwrap();
        assert_eq!(snapshot(&db, false).unwrap().revision, first.revision);
        {
            let tx = conn.transaction().unwrap();
            tx.execute("UPDATE rules SET enabled=0", []).unwrap();
            let rev: i64 = tx.query_row("SELECT source_revision FROM config_apply_state", [], |r| r.get(0)).unwrap();
            assert!(rev > first.revision);
            tx.rollback().unwrap();
        }
        assert!(!status(&db, false).unwrap().pending);
        conn.execute("UPDATE channels SET status='stopped'", []).unwrap();
        assert!(status(&db, false).unwrap().pending);
        assert_eq!(snapshot(&db, false).unwrap().rules[0].enabled, 0);
        let stopped = snapshot(&db, false).unwrap().revision;
        conn.execute("UPDATE channels SET status='error'", []).unwrap();
        assert_eq!(snapshot(&db, false).unwrap().revision, stopped);
        assert!(prepare(&db, first.revision, false, &"a".repeat(64)).is_err());
    }

    #[test]
    fn obsolete_confirmation_cannot_overwrite_new_generation_or_same_hash_revision() {
        let (_dir, db) = db();
        let first = prepare_now(&db, 'a');
        confirmed(&db, &first).unwrap();
        Connection::open(&db).unwrap().execute("UPDATE rules SET enabled=0", []).unwrap();
        // 新来源可能渲染为同一内容，不必创建新应用代次；旧来源 ticket 仍须拒绝。
        let same = prepare_now(&db, 'a');
        assert_eq!(first.generation, same.generation);
        assert!(confirmed(&db, &first).is_err());
        assert!(failed(&db, &first, "reload_failed").is_err());
        confirmed(&db, &same).unwrap();
        let next = prepare_now(&db, 'b');
        assert_eq!(next.generation, same.generation + 1);
        failed(&db, &next, "readback_mismatch").unwrap();
        let state = status(&db, false).unwrap();
        assert!(state.pending);
        assert_eq!(state.applied_generation, first.generation);
        assert_eq!(state.applied_hash, first.digest);
        assert!(state.verified_at.is_some());
        assert!(confirmed(&db, &same).is_err());
        confirmed(&db, &next).unwrap();
        let retry = prepare_now(&db, 'b');
        assert_eq!(retry.generation, next.generation);
        assert!(retry.attempt > next.attempt);
        assert!(confirmed(&db, &next).is_err());
        confirmed(&db, &retry).unwrap();
        crate::store::init(&db).unwrap();
        assert!(!status(&db, false).unwrap().pending);
        assert!(status(&db, true).unwrap().pending);
    }

    #[test]
    fn new_source_during_readback_remains_pending_and_invalid_inputs_leave_state() {
        let (_dir, db) = db();
        let first = prepare_now(&db, 'a');
        Connection::open(&db).unwrap().execute("UPDATE channels SET routing_enabled=0", []).unwrap();
        confirmed(&db, &first).unwrap();
        assert!(status(&db, false).unwrap().pending);
        let before = serde_json::to_value(status(&db, false).unwrap()).unwrap();
        assert!(failed(&db, &first, "body with secret").is_err());
        assert!(prepare(&db, first.revision, false, "not-a-hash").is_err());
        assert_eq!(before, serde_json::to_value(status(&db, false).unwrap()).unwrap());
        assert_eq!(snapshot(&db, true).unwrap().rules[0].enabled, 0);
    }
}
