from pathlib import Path
import pytest
import config_apply as apply
import config_apply_store as journal
import manager
import store


def test_unchanged_effective_config_skips_reload_and_disk_write(make_channel, mihomo_controller, monkeypatch):
    store.add_channel(make_channel('a'))
    rid = store.add_rule('a', 'domain', 'example.test')
    assert manager.rebuild() == 204
    first = journal.status()
    file = Path(manager.CFG)
    initial = file.read_bytes(), file.stat().st_mtime_ns
    store.update_channel('a', {'name': 'renamed'})
    store.set_probe_result('a', 'logged_in', 11)
    assert manager.rebuild() == 204
    assert mihomo_controller['puts'] == 1
    assert initial == (file.read_bytes(), file.stat().st_mtime_ns)
    assert journal.status()['desired_generation'] == first['desired_generation']
    # 停用的规则改内容会增加来源 revision，但有效配置不变。
    store.set_rule_enabled('a', rid, False)
    assert manager.rebuild() == 204
    count = mihomo_controller['puts']
    with store._c() as c: c.execute("UPDATE rules SET pattern='changed.test'")
    assert manager.rebuild() == 204 and mihomo_controller['puts'] == count
    assert not journal.status()['pending']


def test_controller_restart_or_external_rule_change_is_not_hidden_by_hash(make_channel, mihomo_controller):
    store.add_channel(make_channel('a'))
    store.add_rule('a', 'domain', 'example.test')
    assert manager.rebuild() == 204
    generation = journal.status()['desired_generation']
    mihomo_controller['config'] = {'proxies': [], 'rules': ['MATCH,DIRECT']}
    assert manager.rebuild() == 204 and mihomo_controller['puts'] == 2
    assert journal.status()['desired_generation'] == generation
    mihomo_controller['config']['rules'] = ['MATCH,DIRECT']
    assert manager.rebuild() == 204 and mihomo_controller['puts'] == 3


def test_replacement_flushes_proxy_dns_without_reload_and_failed_flush_stays_pending(make_channel, mihomo_controller):
    store.add_channel(make_channel('a'))
    store.set_container('a', 'old', 18080, 'running')
    assert manager.rebuild() == 204
    first = journal.status()
    store.set_container('a', 'new', 18080, 'running')
    assert journal.status()['pending']
    mihomo_controller['reject_flush'] = True
    assert manager.rebuild() != 204
    assert journal.status()['pending'] and journal.status()['last_error'] == 'dns_flush_failed'
    mihomo_controller['reject_flush'] = False
    assert manager.rebuild() == 204
    assert journal.status()['desired_generation'] == first['desired_generation']
    assert mihomo_controller['puts'] == 1 and mihomo_controller['flushes'] == 2
    store.set_container('a', 'new', 18080, 'running')
    assert manager.rebuild() == 204
    assert mihomo_controller['flushes'] == 2 and not journal.status()['pending']


def test_rejected_candidate_keeps_last_boot_file_and_confirmed_version(make_channel, mihomo_controller):
    store.add_channel(make_channel('a'))
    assert manager.rebuild() == 204
    previous = journal.status()
    boot = Path(manager.CFG).read_bytes()
    store.add_rule('a', 'domain', 'new.test')
    mihomo_controller['reject'] = True
    assert manager.rebuild() != 204
    current = journal.status()
    assert current['pending'] and current['last_error'] == 'reload_failed'
    assert current['applied_generation'] == previous['applied_generation']
    assert Path(manager.CFG).read_bytes() == boot


def test_lost_ack_readback_is_partial_then_retry_does_not_reload(make_channel, mihomo_controller):
    store.add_channel(make_channel('a'))
    mihomo_controller['lost_ack'] = True
    assert manager.rebuild() != 204
    state = journal.status()
    assert state['applied_generation'] == state['desired_generation'] and state['pending']
    assert not Path(manager.CFG).exists()
    mihomo_controller['lost_ack'] = False
    assert manager.rebuild() == 204
    assert mihomo_controller['puts'] == 1 and Path(manager.CFG).exists()


def test_boot_write_failure_and_concurrent_save_remain_pending(make_channel, mihomo_controller, monkeypatch):
    store.add_channel(make_channel('a'))
    writer = manager._atomic_write_yaml
    def fail(*args): raise OSError('fixture disk full')
    monkeypatch.setattr(manager, '_atomic_write_yaml', fail)
    assert manager.rebuild() != 204
    assert journal.status()['last_error'] == 'write_failed'
    monkeypatch.setattr(manager, '_atomic_write_yaml', writer)
    assert manager.rebuild() == 204 and mihomo_controller['puts'] == 1
    store.add_rule('a', 'domain', 'before.test')
    def save_during_write(*args):
        writer(*args)
        store.add_rule('a', 'domain', 'later.test')
    monkeypatch.setattr(manager, '_atomic_write_yaml', save_during_write)
    assert manager.rebuild() != 204 and journal.status()['pending']


def test_fingerprint_order_types_and_malformed_readback():
    value = {'中文': [None, True, -2, 2**63, 1e-7, 'x:y'], 'z': {}}
    assert apply.fingerprint(value) == '4fe6b8c4681c29fec703a233f7b83f15fee96a4ca69842dc7fbb89b2392a3c56'
    assert apply.fingerprint(value) == apply.fingerprint(dict(reversed(list(value.items()))))
    assert apply.fingerprint([1]) != apply.fingerprint([1.0])
    with pytest.raises(ValueError): apply.fingerprint(float('nan'))
    with pytest.raises(ValueError): apply.fingerprint({1: 'bad-key'})
    ticket = journal.Ticket(0, False, 1, 1, 'a' * 64)
    config = manager.build_mihomo_config({}, [], [])
    assert not apply.matches(config, ticket, {}, {}, {})


def test_kernel_change_during_boot_file_commit_stays_pending(make_channel, mihomo_controller, monkeypatch):
    store.add_channel(make_channel('a'))
    writer = manager._atomic_write_yaml
    def reset_kernel(*args):
        writer(*args)
        mihomo_controller['config'] = {'proxies': [], 'rules': ['MATCH,DIRECT']}
    monkeypatch.setattr(manager, '_atomic_write_yaml', reset_kernel)
    assert manager.rebuild() != 204
    assert journal.status()['last_error'] == 'readback_mismatch'
    assert journal.status()['pending']
    monkeypatch.setattr(manager, '_atomic_write_yaml', writer)
    assert manager.rebuild() == 204 and mihomo_controller['puts'] == 2


def test_retry_api_exposes_partial_and_confirmed_state(make_channel, mihomo_controller, monkeypatch):
    import main
    from fastapi.testclient import TestClient
    monkeypatch.setattr(manager, 'mihomo_alive', lambda: True)
    client = TestClient(main.app)
    store.add_channel(make_channel('a'))
    mihomo_controller['lost_ack'] = True
    response = client.post('/api/config/retry')
    assert response.status_code == 502 and response.json()['config_application']['pending']
    state = client.get('/api/system').json()['config_application']
    assert state['scope'] == 'managed_rules_proxies' and state['pending']
    assert 'desired_hash' not in state
    mihomo_controller['lost_ack'] = False
    response = client.post('/api/config/retry')
    assert response.status_code == 200 and not response.json()['config_application']['pending']
    assert mihomo_controller['puts'] == 1
