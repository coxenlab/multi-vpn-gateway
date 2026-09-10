import sqlite3
import pytest
import replacement_store as journal
import store


def test_staged_config_commits_with_runtime_and_keeps_new_notes(make_channel):
    store.add_channel(make_channel("c1", password="old"))
    fields = {"name": "new", "password": "fixture-password"}
    journal.begin("c1", "op1", {"fields": fields})
    with pytest.raises(RuntimeError, match="未完成"):
        journal.begin("c1", "op2", {})
    with store._c() as c:
        encrypted = c.execute("SELECT payload_enc FROM channel_replacements").fetchone()[0]
    assert "fixture-password" not in encrypted
    assert store.get_password("c1") == "old"
    with pytest.raises(RuntimeError, match="尚未完成"):
        journal.finish("c1", "op1")
    for phase in ("prepared", "switching", "validating"):
        before = journal.records()[0]
        journal.advance(before, phase, before.payload)
        with pytest.raises(RuntimeError, match="代次或阶段"):
            journal.advance(before, phase, before.payload)
    store.set_config_field("c1", "login_note", "new note", secret=True)
    record = journal.records()[0]
    runtime = journal.AppliedRuntime("new-container", "new-volume", None, "running")
    with store._c() as c:
        c.execute("CREATE TRIGGER fail_runtime BEFORE INSERT ON channel_runtime BEGIN SELECT RAISE(ABORT, 'injected'); END")
    try:
        with pytest.raises(sqlite3.IntegrityError, match="injected"):
            journal.commit(record, fields, ("password",), runtime)
        assert store.get_password("c1") == "old"
        assert store.get_channel("c1")["name"] == "c1"
        assert journal.records()[0].phase == "validating"
        assert journal.data_volume("c1") == "vpndata-c1"
    finally:
        with store._c() as c:
            c.execute("DROP TRIGGER fail_runtime")
    journal.commit(record, fields, ("password",), runtime)
    assert store.get_password("c1") == "fixture-password"
    assert store.get_config("c1")["login_note"] == "new note"
    assert journal.data_volume("c1") == "new-volume"
    with pytest.raises(RuntimeError, match="代次"):
        journal.finish("c1", "op2")
    journal.finish("c1", "op1")
    journal.finish("c1", "op1")
    assert journal.records() == []


def test_rollback_records_observed_runtime_without_applying_draft(make_channel):
    store.add_channel(make_channel("c1", password="old"))
    journal.begin("c1", "op1", {"fields": {"password": "unused"}})
    pending = journal.records()[0]
    journal.advance(pending, "rolling_back", pending.payload)
    store.set_config_field("c1", "login_note", "during replacement", secret=True)
    journal.rolled_back(journal.records()[0], journal.AppliedRuntime("restored", "vpndata-c1", None, "running"))
    assert store.get_password("c1") == "old"
    assert store.get_config("c1")["login_note"] == "during replacement"
    assert store.get_channel("c1")["container_id"] == "restored"
    assert journal.records()[0].phase == "rolled_back"
    journal.finish("c1", "op1")


def test_waiting_for_login_preserves_backup_and_can_restore_fields_atomically(make_channel):
    store.add_channel(make_channel("c1", password="old"))
    journal.begin("c1", "op1", {})
    for phase in ("prepared", "switching", "validating"):
        record = journal.get("c1")
        journal.advance(record, phase, record.payload)
    journal.commit(journal.get("c1"), {"password": "new"}, ("password",),
                   journal.AppliedRuntime("candidate", "new-volume", None, "running"), awaiting_login=True)
    assert journal.get("c1").phase == "awaiting_login"
    with pytest.raises(RuntimeError, match="尚未完成"):
        journal.finish("c1", "op1")
    store.set_config_field("c1", "login_note", "saved while logging in", secret=True)
    record = journal.get("c1")
    journal.advance(record, "rolling_back", record.payload)
    journal.rolled_back(journal.get("c1"), journal.AppliedRuntime("restored", "vpndata-c1", None, "running"),
                        {"password": "old"}, ("password",))
    assert store.get_password("c1") == "old"
    assert store.get_config("c1")["password"] == "old"
    assert store.get_config("c1")["login_note"] == "saved while logging in"
    with pytest.raises(RuntimeError, match="代次或阶段"):
        journal.confirm(record)


def test_delete_requires_replacement_cleanup_and_persists_delete_intent(make_channel):
    store.add_channel(make_channel("c1"))
    journal.begin("c1", "op1", {})
    with pytest.raises(RuntimeError, match="尚未清理"):
        store.del_channel("c1")
    record = journal.get("c1")
    journal.request_delete(record)
    assert journal.public_status("c1") == {"phase": "deleting", "can_restore": False}
    with pytest.raises(RuntimeError, match="代次或阶段"):
        journal.deleted(journal.Record("c1", "stale-operation", "deleting", {}))
    journal.deleted(journal.get("c1"))
    assert store.get_channel("c1") is None and journal.get("c1") is None


def test_restore_absent_is_atomic_and_keeps_notes(make_channel):
    store.add_channel(make_channel('c1', password='new'))
    store.set_container('c1', 'candidate', 18080, 'running')
    store.set_config_field('c1', 'login_note', 'saved during initial attempt', secret=True)
    journal.begin('c1', 'op1', {'kind': 'initial'})
    record = journal.get('c1')
    journal.advance(record, 'rolling_back', record.payload)
    record = journal.get('c1')
    with store._c() as c:
        c.execute("CREATE TRIGGER fail_initial BEFORE INSERT ON channel_runtime BEGIN SELECT RAISE(ABORT, 'injected'); END")
    with pytest.raises(sqlite3.IntegrityError):
        journal.restore_absent(record, 'vpndata-c1', {'password': 'old'}, ('password',))
    assert store.get_channel('c1')['container_id'] == 'candidate'
    assert store.get_password('c1') == 'new' and journal.get('c1').phase == 'rolling_back'
    with store._c() as c: c.execute('DROP TRIGGER fail_initial')
    journal.restore_absent(record, 'vpndata-c1', {'password': 'old'}, ('password',))
    assert store.get_channel('c1')['container_id'] is None and store.get_channel('c1')['status'] == 'error'
    assert journal.data_volume('c1') == 'vpndata-c1' and store.get_password('c1') == 'old'
    assert store.get_config('c1')['login_note'] == 'saved during initial attempt'
    with pytest.raises(RuntimeError): journal.restore_absent(record, 'vpndata-c1')
