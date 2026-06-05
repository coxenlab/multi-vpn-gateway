use std::collections::HashSet;
use std::path::Path;
use rusqlite::Connection;

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
}
