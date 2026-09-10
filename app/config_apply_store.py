"""mihomo 配置应用记录。调用方串行化投递，并在真实运行态读回后才调用 confirmed。"""
from dataclasses import dataclass
import re
import store


@dataclass(frozen=True)
class Ticket:
    revision: int
    routing_off: bool
    generation: int
    attempt: int
    digest: str


def snapshot(routing_off=False):
    """同一 SQLite 读事务获取代次、通道和规则；不把两次查询拼成混合版本。"""
    with store._c() as c:
        c.execute("BEGIN")
        revision = c.execute("SELECT source_revision FROM config_apply_state WHERE id=1").fetchone()[0]
        channels = [store._row(r) for r in c.execute("SELECT * FROM channels ORDER BY id")]
        rules = store._effective_rules(c, routing_off)
        return revision, channels, rules


def status(routing_off=False):
    with store._c() as c:
        state = dict(c.execute("SELECT * FROM config_apply_state WHERE id=1").fetchone())
    state.pop("id")
    if state["desired_routing_off"] is not None:
        state["desired_routing_off"] = bool(state["desired_routing_off"])
    state["pending"] = bool(
        not state["verified_at"] or state["last_error"]
        or state["source_revision"] != state["desired_revision"]
        or state["desired_routing_off"] != int(routing_off)
        or state["desired_generation"] != state["applied_generation"]
        or state["desired_hash"] != state["applied_hash"])
    return state


def public_status(routing_off=False):
    try:
        state = status(routing_off)
        keys = ("source_revision", "desired_revision", "desired_generation", "applied_generation", "verified_at", "last_error", "pending")
        return dict({key: state[key] for key in keys}, available=True, scope="managed_rules_proxies")
    except Exception:
        return {"available": False, "pending": True, "last_error": "state_unavailable"}


def prepare(revision, routing_off, digest):
    if not re.fullmatch(r"[0-9a-f]{64}", digest):
        raise ValueError("配置摘要无效")
    with store._c() as c:
        c.execute("BEGIN IMMEDIATE")
        row = c.execute("SELECT * FROM config_apply_state WHERE id=1").fetchone()
        if row["source_revision"] != revision:
            raise RuntimeError("配置来源已变化")
        generation = row["desired_generation"] + int(row["desired_hash"] != digest)
        attempt = row["attempt"] + 1
        c.execute("UPDATE config_apply_state SET desired_revision=?,desired_routing_off=?,"
                  "desired_generation=?,attempt=?,desired_hash=?,last_error='unconfirmed' WHERE id=1",
                  (revision, int(routing_off), generation, attempt, digest))
    return Ticket(revision, bool(routing_off), generation, attempt, digest)


def _matches(ticket):
    return ticket.revision, int(ticket.routing_off), ticket.generation, ticket.attempt, ticket.digest


def confirmed(ticket):
    """保留旧确认的历史含义；若源数据已更新，status 仍为 pending。"""
    _record_readback(ticket, None)


def observed(ticket):
    """运行态已读回，启动文件尚待持久化；崩溃后仍保留 pending。"""
    _record_readback(ticket, "unconfirmed")


def _record_readback(ticket, error):
    with store._c() as c:
        result = c.execute(
            "UPDATE config_apply_state SET applied_generation=desired_generation,applied_hash=desired_hash,"
            "verified_at=CAST(strftime('%s','now') AS INTEGER),last_error=? "
            "WHERE id=1 AND desired_revision=? AND desired_routing_off=? AND desired_generation=? AND attempt=? AND desired_hash=?",
            (error, *_matches(ticket)))
        if result.rowcount != 1:
            raise RuntimeError("配置应用代次已变化")


def failed(ticket, code):
    # 固定原因码；控制器错误正文可能包含配置，不能持久化或送回公开状态。
    if code not in ("write_failed", "delivery_failed", "reload_failed", "readback_failed", "readback_mismatch", "dns_flush_failed"):
        raise ValueError("配置错误码无效")
    with store._c() as c:
        result = c.execute(
            "UPDATE config_apply_state SET last_error=? WHERE id=1 AND desired_revision=? "
            "AND desired_routing_off=? AND desired_generation=? AND attempt=? AND desired_hash=?", (code, *_matches(ticket)))
        if result.rowcount != 1:
            raise RuntimeError("配置应用代次已变化")
