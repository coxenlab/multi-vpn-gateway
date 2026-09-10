-- 两栈共享应用代次；只存摘要与确认时间，不存配置正文或凭据。
CREATE TABLE IF NOT EXISTS config_apply_state(
  id INTEGER PRIMARY KEY CHECK(id=1),
  source_revision INTEGER NOT NULL DEFAULT 0,
  desired_revision INTEGER NOT NULL DEFAULT -1,
  desired_routing_off INTEGER,
  desired_generation INTEGER NOT NULL DEFAULT 0,
  attempt INTEGER NOT NULL DEFAULT 0,
  desired_hash TEXT NOT NULL DEFAULT '',
  applied_generation INTEGER NOT NULL DEFAULT 0,
  applied_hash TEXT NOT NULL DEFAULT '',
  verified_at INTEGER,
  last_error TEXT
);
INSERT OR IGNORE INTO config_apply_state(id) VALUES(1);

-- 触发器与业务写入同事务：保存后即待处理，不等重载任务拿到锁。
CREATE TRIGGER IF NOT EXISTS config_apply_channel_insert AFTER INSERT ON channels BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS config_apply_channel_delete AFTER DELETE ON channels BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
-- 同名新实例仍使用相同代理配置，但必须刷新 mihomo 的代理服务器 DNS 缓存。
CREATE TRIGGER IF NOT EXISTS config_apply_channel_runtime AFTER UPDATE OF container_id ON channels
WHEN OLD.container_id IS NOT NEW.container_id
BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS config_apply_channel_update AFTER UPDATE ON channels
WHEN OLD.id IS NOT NEW.id
  OR (COALESCE(OLD.routing_enabled,1)=0) IS NOT (COALESCE(NEW.routing_enabled,1)=0)
  OR COALESCE(OLD.status IN ('stopped','error'),0) IS NOT COALESCE(NEW.status IN ('stopped','error'),0)
BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS config_apply_rule_insert AFTER INSERT ON rules BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS config_apply_rule_delete AFTER DELETE ON rules BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
CREATE TRIGGER IF NOT EXISTS config_apply_rule_update AFTER UPDATE ON rules
WHEN OLD.id IS NOT NEW.id OR OLD.channel_id IS NOT NEW.channel_id
  OR OLD.kind IS NOT NEW.kind OR OLD.pattern IS NOT NEW.pattern
  OR OLD.enabled IS NOT NEW.enabled
BEGIN
  UPDATE config_apply_state SET source_revision=source_revision+1 WHERE id=1;
END;
