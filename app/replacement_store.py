"""容器替换记录。准备期间不改配置，凭据只加密留存于内部 payload。"""
from dataclasses import dataclass
import json
from typing import Optional
import store


@dataclass(repr=False)
class Record:
    channel_id: str
    operation_id: str
    phase: str
    payload: dict


@dataclass
class AppliedRuntime:
    container_id: str
    data_volume: str
    novnc_port: Optional[int]
    status: str
    latency_ms: Optional[int] = None


def _check(c, cid, operation, phase):
    row = c.execute("SELECT operation_id,phase FROM channel_replacements WHERE channel_id=?", (cid,)).fetchone()
    if not row or row["operation_id"] != operation or row["phase"] != phase:
        raise RuntimeError("容器替换代次或阶段已变化")


def data_volume(cid):
    with store._c() as c:
        row = c.execute("SELECT data_volume FROM channel_runtime WHERE channel_id=?", (cid,)).fetchone()
        return row[0] if row else f"vpndata-{cid}"


def begin(cid, operation, payload):
    encrypted = store.F.encrypt(json.dumps(payload, ensure_ascii=False).encode()).decode()
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        if not c.execute("SELECT 1 FROM channels WHERE id=?", (cid,)).fetchone():
            raise RuntimeError("通道不存在")
        if c.execute("SELECT 1 FROM channel_replacements WHERE channel_id=?", (cid,)).fetchone():
            raise RuntimeError("该通道已有未完成的容器替换")
        c.execute("INSERT INTO channel_replacements(channel_id,operation_id,phase,payload_enc,created_at) VALUES(?,?,'preparing',?,CAST(strftime('%s','now') AS INTEGER))",
                  (cid, operation, encrypted))


def records():
    with store._c() as c:
        rows = c.execute("SELECT channel_id,operation_id,phase,payload_enc FROM channel_replacements ORDER BY created_at,channel_id").fetchall()
    return [Record(r["channel_id"], r["operation_id"], r["phase"], json.loads(store.F.decrypt(r["payload_enc"].encode()))) for r in rows]


def get(cid):
    with store._c() as c:
        r = c.execute("SELECT operation_id,phase,payload_enc FROM channel_replacements WHERE channel_id=?", (cid,)).fetchone()
    return Record(cid, r["operation_id"], r["phase"], json.loads(store.F.decrypt(r["payload_enc"].encode()))) if r else None


def public_status(cid):
    with store._c() as c:
        r = c.execute("SELECT phase FROM channel_replacements WHERE channel_id=?", (cid,)).fetchone()
    return {"phase": r["phase"], "can_restore": r["phase"] not in ("committed", "rolled_back", "deleting")} if r else None


def advance(record, next_phase, payload):
    legal = next_phase == record.phase or (record.phase, next_phase) in {
        ("preparing", "prepared"), ("preparing", "rolling_back"),
        ("prepared", "switching"), ("prepared", "rolling_back"),
        ("switching", "validating"), ("switching", "rolling_back"),
        ("validating", "rolling_back"),
        ("awaiting_login", "rolling_back"),
    }
    if not legal or next_phase in ("committed", "rolled_back"):
        raise ValueError("无效的容器替换阶段转换")
    encrypted = store.F.encrypt(json.dumps(payload, ensure_ascii=False).encode()).decode()
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, record.phase)
        c.execute("UPDATE channel_replacements SET phase=?,payload_enc=? WHERE channel_id=?",
                  (next_phase, encrypted, record.channel_id))


def _apply_runtime(c, cid, runtime):
    if runtime.status not in ("running", "logged_in", "stopped"):
        raise ValueError("无效的已应用通道状态")
    row = c.execute("UPDATE channels SET container_id=?,novnc_port=?,status=?,latency_ms=? WHERE id=?",
                    (runtime.container_id, runtime.novnc_port, runtime.status, runtime.latency_ms, cid))
    if row.rowcount != 1:
        raise RuntimeError("通道不存在")
    c.execute("INSERT INTO channel_runtime(channel_id,data_volume) VALUES(?,?) ON CONFLICT(channel_id) DO UPDATE SET data_volume=excluded.data_volume",
              (cid, runtime.data_volume))


def commit(record, fields, secret_keys, runtime, awaiting_login=False):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, "validating")
        store._update_channel(c, record.channel_id, fields, secret_keys)
        _apply_runtime(c, record.channel_id, runtime)
        c.execute("UPDATE channel_replacements SET phase=? WHERE channel_id=?",
                  ("awaiting_login" if awaiting_login else "committed", record.channel_id))


def confirm(record):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, "awaiting_login")
        c.execute("UPDATE channel_replacements SET phase='committed' WHERE channel_id=?", (record.channel_id,))


def resumed(record, runtime):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, "awaiting_login")
        _apply_runtime(c, record.channel_id, runtime)


def rolled_back(record, runtime, fields=None, secret_keys=()):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, "rolling_back")
        if fields is not None:
            store._update_channel(c, record.channel_id, fields, secret_keys)
        _apply_runtime(c, record.channel_id, runtime)
        c.execute("UPDATE channel_replacements SET phase='rolled_back' WHERE channel_id=?", (record.channel_id,))


def restore_absent(record, volume, fields=None, secret_keys=(), stopped=False):
    """初次创建或旧实例丢失的补偿：只还原设置和卷，不编造运行中的实例。"""
    if record.payload.get('kind') != 'initial': raise ValueError('操作不是无旧实例创建')
    with store._c() as c:
        c.execute('BEGIN IMMEDIATE')
        _check(c, record.channel_id, record.operation_id, 'rolling_back')
        if fields is not None:
            store._update_channel(c, record.channel_id, fields, secret_keys)
        row = c.execute('UPDATE channels SET container_id=NULL,novnc_port=NULL,latency_ms=NULL,status=? WHERE id=?',
                        ('stopped' if stopped else 'error', record.channel_id))
        if row.rowcount != 1: raise RuntimeError('通道不存在')
        c.execute('INSERT INTO channel_runtime(channel_id,data_volume) VALUES(?,?) ON CONFLICT(channel_id) DO UPDATE SET data_volume=excluded.data_volume',
                  (record.channel_id, volume))
        c.execute("UPDATE channel_replacements SET phase='rolled_back' WHERE channel_id=?", (record.channel_id,))


def finish(cid, operation):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        row = c.execute("SELECT operation_id,phase FROM channel_replacements WHERE channel_id=?", (cid,)).fetchone()
        if row:
            if row["operation_id"] != operation or row["phase"] not in ("committed", "rolled_back"):
                raise RuntimeError("容器替换尚未完成或代次已变化")
            c.execute("DELETE FROM channel_replacements WHERE channel_id=?", (cid,))


def request_delete(record):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, record.phase)
        c.execute("UPDATE channel_replacements SET phase='deleting' WHERE channel_id=?", (record.channel_id,))


def deleted(record):
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        _check(c, record.channel_id, record.operation_id, 'deleting')
        for table, column in (("channels", "id"), ("domains", "channel_id"), ("rules", "channel_id"), ("channel_runtime", "channel_id"), ("channel_replacements", "channel_id")):
            c.execute(f"DELETE FROM {table} WHERE {column}=?", (record.channel_id,))
