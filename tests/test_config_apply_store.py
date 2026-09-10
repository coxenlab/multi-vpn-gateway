import pytest
import config_apply_store as apply
import store


def prepare(digest="a", off=False):
    revision, _, _ = apply.snapshot(off)
    return apply.prepare(revision, off, digest * 64)


def test_source_changes_atomic_and_metadata_keeps_version(make_channel):
    store.add_channel(make_channel("a"))
    store.add_rule("a", "domain", "example.test")
    first = prepare()
    apply.confirmed(first)
    assert not apply.status()["pending"]
    store.update_channel("a", {"name": "renamed", "probe_url": "http://new.test"})
    store.set_probe_result("a", "logged_in", 12)
    with store._c() as c:
        c.execute("UPDATE rules SET note='keep',locked=1")
    assert apply.snapshot()[0] == first.revision
    with store._c() as c:
        c.execute("BEGIN")
        c.execute("UPDATE rules SET enabled=0")
        assert c.execute("SELECT source_revision FROM config_apply_state").fetchone()[0] > first.revision
        c.rollback()
    assert not apply.status()["pending"]
    store.set_status("a", "stopped")
    assert apply.status()["pending"]
    stopped, _, rules = apply.snapshot()
    assert rules[0]["enabled"] == 0
    store.set_status("a", "error")
    assert apply.snapshot()[0] == stopped
    with pytest.raises(RuntimeError, match="来源已变化"):
        apply.prepare(first.revision, False, "a" * 64)


def test_old_confirmation_rejected_even_when_new_revision_has_same_hash(make_channel):
    store.add_channel(make_channel("a"))
    store.add_rule("a", "domain", "example.test")
    first = prepare()
    apply.confirmed(first)
    with store._c() as c:
        c.execute("UPDATE rules SET enabled=0")
    same = prepare()
    assert first.generation == same.generation
    with pytest.raises(RuntimeError, match="代次已变化"):
        apply.confirmed(first)
    with pytest.raises(RuntimeError, match="代次已变化"):
        apply.failed(first, "reload_failed")
    apply.confirmed(same)
    next_ticket = prepare("b")
    assert next_ticket.generation == same.generation + 1
    apply.failed(next_ticket, "readback_mismatch")
    state = apply.status()
    assert state["pending"] and state["verified_at"]
    assert state["applied_generation"] == first.generation
    assert state["applied_hash"] == first.digest
    with pytest.raises(RuntimeError, match="代次已变化"):
        apply.confirmed(same)
    apply.confirmed(next_ticket)
    retry = prepare("b")
    assert retry.generation == next_ticket.generation
    assert retry.attempt > next_ticket.attempt
    with pytest.raises(RuntimeError, match="代次已变化"):
        apply.confirmed(next_ticket)
    apply.confirmed(retry)
    store.init()
    assert not apply.status()["pending"]
    assert apply.status(True)["pending"]


def test_new_source_during_readback_stays_pending_and_errors_are_codes(make_channel):
    store.add_channel(make_channel("a"))
    store.add_rule("a", "domain", "example.test")
    first = prepare()
    with store._c() as c:
        c.execute("UPDATE channels SET routing_enabled=0")
    apply.confirmed(first)
    assert apply.status()["pending"]
    before = apply.status()
    with pytest.raises(ValueError, match="错误码无效"):
        apply.failed(first, "body with secret")
    with pytest.raises(ValueError, match="摘要无效"):
        apply.prepare(first.revision, False, "not-a-hash")
    assert apply.status() == before
    assert apply.snapshot(True)[2][0]["enabled"] == 0


def test_snapshot_keeps_rule_and_channel_from_same_transaction(make_channel, monkeypatch):
    store.add_channel(make_channel("a"))
    store.add_rule("a", "domain", "before.test")
    with store._c() as c:
        c.execute("PRAGMA journal_mode=WAL")
    original = store._row
    changed = []

    def save_during_read(row):
        if not changed:
            with store._c() as c:
                c.execute("BEGIN IMMEDIATE")
                c.execute("UPDATE channels SET status='stopped'")
                c.execute("UPDATE rules SET pattern='after.test'")
            changed.append(True)
        return original(row)

    monkeypatch.setattr(store, "_row", save_during_read)
    revision, channels, rules = apply.snapshot()
    assert channels[0]["status"] == "running"
    assert rules[0]["pattern"] == "before.test" and rules[0]["enabled"] == 1
    assert apply.snapshot()[0] > revision
    assert apply.snapshot()[2][0]["enabled"] == 0
