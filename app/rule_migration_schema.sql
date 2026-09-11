-- 两栈识别同一份离线整理记录；正常启动只建表，不整理用户规则。
CREATE TABLE IF NOT EXISTS rule_migration_state(
  id INTEGER PRIMARY KEY CHECK(id=1),
  version INTEGER NOT NULL,
  examined INTEGER NOT NULL,
  normalized INTEGER NOT NULL,
  quarantined INTEGER NOT NULL
);
-- 无类型亲和性的原字段保留 NULL/BLOB 等异常值，不强制转成字符串。
CREATE TABLE IF NOT EXISTS rule_migration_archive(
  id INTEGER PRIMARY KEY,
  channel_id, kind, pattern, enabled, note, locked,
  action TEXT NOT NULL CHECK(action IN ('normalized','quarantined'))
);
