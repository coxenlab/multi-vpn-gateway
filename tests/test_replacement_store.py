import sqlite3
import pytest
import replacement_store as journal
import store


def test_queued_settings_wait_for_explicit_start_and_survive_preparation_failure(make_channel, monkeypatch):
    import replacement, manager
    store.add_channel(make_channel('c1', server='old.example'))
    store.set_container('c1', 'original', None, 'stopped')
    journal.queue_settings(store.get_channel('c1'), {'server':'new.example'})
    original = journal.get('c1')
    def forbidden(*args, **kwargs): raise AssertionError('recovery must leave queued changes dormant')
    monkeypatch.setattr(manager, 'rebuild', forbidden)
    monkeypatch.setattr(replacement, '_get', forbidden)
    replacement.recover_all()
    replacement.before_stop('c1')
    assert journal.get('c1').operation_id == original.operation_id
    def unavailable(*args): raise RuntimeError('injected prepare failure')
    monkeypatch.setattr(replacement, '_get', unavailable)
    with pytest.raises(RuntimeError, match='prepare failure'): replacement.resume('c1')
    pending = journal.get('c1')
    assert pending.phase == 'queued' and pending.payload == original.payload
    assert store.get_channel('c1')['server'] == 'old.example'


def test_queued_settings_keep_originals_and_cancellation_keeps_metadata(make_channel):
    store.add_channel(make_channel('c1', password='old-secret', server='old.example'))
    ch = store.get_channel('c1')
    journal.queue_settings(ch, {'server':'new.example', 'password':' new-secret ', 'name':'renamed'}, ('password',))
    assert store.get_channel('c1')['server'] == 'old.example'
    assert store.get_channel('c1')['name'] == 'renamed'
    assert store.get_password('c1') == 'old-secret'
    queued = journal.get('c1')
    assert queued.payload['fields']['password'] == ' new-secret '
    with store._c() as c:
        assert 'new-secret' not in c.execute('SELECT payload_enc FROM channel_replacements').fetchone()[0]
    public = {'replacement':journal.public_status('c1'), 'server':'old.example'}
    journal.overlay_queued('c1', public)
    assert public['server'] == 'new.example' and 'new-secret' not in str(public)
    journal.cancel_queued(queued)
    assert journal.get('c1') is None and store.get_channel('c1')['name'] == 'renamed'


def test_queue_transition_and_metadata_are_atomic(make_channel):
    store.add_channel(make_channel('c1', server='old.example'))
    ch = store.get_channel('c1')
    with store._c() as c:
        c.execute("CREATE TRIGGER fail_queue BEFORE INSERT ON channel_replacements BEGIN SELECT RAISE(ABORT,'injected'); END")
    with pytest.raises(sqlite3.IntegrityError):
        journal.queue_settings(ch, {'server':'new.example', 'name':'wrong'})
    assert store.get_channel('c1')['name'] == 'c1'
    with store._c() as c: c.execute('DROP TRIGGER fail_queue')
    journal.queue_settings(ch, {'server':'first.example'})
    stale = journal.get('c1')
    journal.queue_settings(ch, {'server':'old.example', 'name':'kept'})
    assert journal.get('c1') is None and store.get_channel('c1')['name'] == 'kept'
    journal.queue_settings(ch, {'server':'second.example'})
    with pytest.raises(RuntimeError): journal.cancel_queued(stale)
    with pytest.raises(RuntimeError): journal.begin('c1', 'new', {}, queued=stale)
    current = journal.get('c1')
    assert current.payload['fields']['server'] == 'second.example'
    journal.begin('c1', 'new', {'prepared':True}, queued=current)
    assert journal.get('c1').phase == 'preparing'
    with pytest.raises(RuntimeError): journal.cancel_queued(current)


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
