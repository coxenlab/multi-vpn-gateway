//! 仅由离线升级准备调用。原规则先归档，整理与完成标记在副本的一次事务内提交。
use std::path::Path;
use anyhow::{ensure, Result};
use rusqlite::{params, types::Value, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct Report {
    pub version: u32,
    pub examined: i64,
    pub normalized: i64,
    pub quarantined: i64,
}

/// `store::init` 已在尚未启用的私有副本中完成 schema 准备。
pub(crate) fn prepare_copy(db: &Path) -> Result<Report> {
    let mut conn = Connection::open(db)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(report) = tx.query_row(
        "SELECT version,examined,normalized,quarantined FROM rule_migration_state WHERE id=1", [],
        |row| Ok(Report { version: row.get(0)?, examined: row.get(1)?, normalized: row.get(2)?, quarantined: row.get(3)? }),
    ).optional()? {
        ensure!(report.version == 1, "规则整理记录版本不受支持，请保留原数据");
        return Ok(report);
    }
    let columns = crate::store::table_columns(&tx, "rules")?;
    ensure!(columns.len() == 7 && ["id", "channel_id", "kind", "pattern", "enabled", "note", "locked"]
        .iter().all(|name| columns.contains(*name)), "规则表含当前版本无法保留的字段，副本未启用");
    let rows = {
        let mut statement = tx.prepare("SELECT id,kind,pattern FROM rules ORDER BY id")?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Value>(1)?, row.get::<_, Value>(2)?)))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut report = Report { version: 1, examined: rows.len() as i64, normalized: 0, quarantined: 0 };
    for (id, kind, pattern) in rows {
        let normalized = match (&kind, &pattern) {
            (Value::Text(kind), Value::Text(pattern)) => crate::webutil::normalize_stored_rule(kind, pattern),
            _ => None,
        };
        let unchanged = matches!((&normalized, &kind, &pattern),
            (Some((new_kind, new_pattern)), Value::Text(kind), Value::Text(pattern))
                if new_kind == kind && new_pattern == pattern);
        if unchanged { continue; }
        let action = if normalized.is_some() { "normalized" } else { "quarantined" };
        ensure!(tx.execute("INSERT INTO rule_migration_archive(id,channel_id,kind,pattern,enabled,note,locked,action) \
            SELECT id,channel_id,kind,pattern,enabled,note,locked,?1 FROM rules WHERE id=?2", params![action, id])? == 1,
            "原规则归档未完成，副本未启用");
        if let Some((kind, pattern)) = normalized {
            tx.execute("UPDATE rules SET kind=?1,pattern=?2 WHERE id=?3", params![kind, pattern, id])?;
            report.normalized += 1;
        } else {
            tx.execute("DELETE FROM rules WHERE id=?1", [id])?;
            report.quarantined += 1;
        }
    }
    tx.execute("INSERT INTO rule_migration_state(id,version,examined,normalized,quarantined) VALUES(1,?1,?2,?3,?4)",
        params![report.version, report.examined, report.normalized, report.quarantined])?;
    tx.commit()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_order_switches_notes_and_original_sqlite_values() {
        let root = tempfile::tempdir().unwrap(); let db = root.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let conn = Connection::open(&db).unwrap();
        conn.execute_batch("INSERT INTO rules(id,channel_id,kind,pattern,enabled,note,locked) VALUES
            (1,'c','domain','HTTPS://*.Example.COM/path',0,'keep note',1),
            (2,'c','ip','10.0.0.1',1,'',0),
            (3,'c','domain','fp.内网',1,'unchanged',0),
            (4,'c','domain','bad,REJECT',1,'keep invalid',1),
            (5,'c','ip',X'00FF',NULL,NULL,NULL),
            (6,'c','unknown','example.com',1,'',0);").unwrap();
        let revision: i64 = conn.query_row("SELECT source_revision FROM config_apply_state", [], |r| r.get(0)).unwrap();
        let report = prepare_copy(&db).unwrap();
        assert_eq!(report, Report { version: 1, examined: 6, normalized: 2, quarantined: 3 });
        let rules = crate::store::all_rules(&db).unwrap();
        assert_eq!(rules.iter().map(|r| r.id).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!((&rules[0].pattern, rules[0].enabled, &rules[0].note, rules[0].locked),
            (&"example.com".to_string(), 0, &"keep note".to_string(), 1));
        assert_eq!(rules[1].pattern, "10.0.0.1/32"); assert_eq!(rules[2].pattern, "fp.内网");
        let original: (Value, Value, Value) = conn.query_row("SELECT pattern,enabled,note FROM rule_migration_archive WHERE id=5", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(original, (Value::Blob(vec![0, 255]), Value::Null, Value::Null));
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM rule_migration_archive", [], |r| r.get::<_, i64>(0)).unwrap(), 5);
        let after: i64 = conn.query_row("SELECT source_revision FROM config_apply_state", [], |r| r.get(0)).unwrap();
        assert!(after > revision);
        assert_eq!(prepare_copy(&db).unwrap(), report);
        assert_eq!(conn.query_row("SELECT source_revision FROM config_apply_state", [], |r| r.get::<_, i64>(0)).unwrap(), after);
    }

    #[test]
    fn all_invalid_legacy_rules_stay_quarantined_after_initialization() {
        let root = tempfile::tempdir().unwrap(); let db = root.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let conn = Connection::open(&db).unwrap();
        conn.execute("INSERT INTO domains(channel_id,pattern) VALUES('c','bad,REJECT')", []).unwrap();
        crate::store::init(&db).unwrap();
        assert_eq!(prepare_copy(&db).unwrap().quarantined, 1);
        crate::store::init(&db).unwrap();
        assert!(crate::store::all_rules(&db).unwrap().is_empty());
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM domains", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn failed_archive_rolls_back_changes_and_completion_marker() {
        let root = tempfile::tempdir().unwrap(); let db = root.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap(); let conn = Connection::open(&db).unwrap();
        conn.execute_batch("INSERT INTO rules(id,channel_id,kind,pattern) VALUES(1,'c','domain','*.example.com'),(2,'c','domain','bad,REJECT');
            CREATE TRIGGER reject_archive BEFORE INSERT ON rule_migration_archive WHEN NEW.id=2
            BEGIN SELECT RAISE(ABORT,'synthetic archive failure'); END;").unwrap();
        assert!(prepare_copy(&db).is_err());
        assert_eq!(crate::store::all_rules(&db).unwrap()[0].pattern, "*.example.com");
        assert_eq!(crate::store::all_rules(&db).unwrap().len(), 2);
        for table in ["rule_migration_archive", "rule_migration_state"] {
            assert_eq!(conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        }
    }
}
