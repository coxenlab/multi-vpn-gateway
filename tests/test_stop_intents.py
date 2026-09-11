from types import SimpleNamespace
import pytest
import docker
import store
import stop_intents


def test_stop_persists_filters_rules_and_guards_confirmation(make_channel):
    store.add_channel(make_channel('c1'))
    store.set_container('c1', 'owned-id', 18080, 'running')
    store.add_rule('c1', 'domain', 'stop.example')
    stop_intents.request('c1')
    operation = stop_intents.ticket('c1')
    stop_intents.request('c1')
    store.init()
    assert stop_intents.ticket('c1') == operation
    store.set_status('c1', 'running')
    assert store.effective_rules()[0]['enabled'] == 0
    with pytest.raises(RuntimeError): stop_intents.confirmed('c1', 'obsolete')
    assert stop_intents.pending('c1')
    store.del_channel('c1')
    assert not stop_intents.pending('c1')


@pytest.mark.parametrize('mode', ['success', 'lost_ack', 'running', 'foreign', 'renamed', 'missing', 'read_error'])
def test_confirmation_reads_owned_instance_after_single_stop(make_channel, monkeypatch, mode):
    import manager
    store.add_channel(make_channel('c1'))
    store.set_container('c1', 'owned-id', 18080, 'running')
    stop_intents.request('c1')
    calls = []
    current = SimpleNamespace(id='foreign-id' if mode == 'foreign' else 'owned-id',
                              attrs={'Name': '/external' if mode == 'renamed' else '/vpn-c1', 'State': {'Running': True}})
    def stop():
        calls.append('stop')
        if mode != 'running': current.attrs['State']['Running'] = False
        if mode == 'lost_ack': raise docker.errors.APIError('lost ACK')
    current.stop = stop
    def get(identity):
        if mode == 'read_error': raise docker.errors.APIError('read failed')
        if mode == 'missing' or (mode == 'renamed' and identity == 'vpn-c1'):
            raise docker.errors.NotFound('missing')
        return current
    monkeypatch.setattr(manager, 'dc', SimpleNamespace(containers=SimpleNamespace(get=get)))
    success = mode in ('success', 'lost_ack', 'missing')
    if success: stop_intents.recover_all()
    else:
        with pytest.raises(Exception): stop_intents.recover_all()
    assert stop_intents.pending('c1') is not success
    assert len(calls) == (1 if mode in ('success', 'lost_ack', 'running') else 0)


def test_failed_stop_is_explicit_saved_and_not_probed(client, monkeypatch):
    import manager
    ch = client.post('/api/channels', json={'name': 'stop-fixture'}).json()
    def unavailable(identity): raise docker.errors.APIError('fixture offline')
    monkeypatch.setattr(manager.dc.containers, 'get', unavailable)
    response = client.post('/api/channels/'+ch['id']+'/stop')
    assert response.status_code == 202 and response.json()['stop_pending']
    assert client.get('/api/channels').json()[0]['stop_pending']
    assert client.get('/api/channels/'+ch['id']+'/health').json()['stop_pending']
    edit = client.patch('/api/channels/'+ch['id'], json={'server': 'https://changed.example'})
    assert edit.status_code == 202 and edit.json()['replacement']['phase'] == 'queued'
    client.post('/api/channels/'+ch['id']+'/restore')
    assert stop_intents.pending(ch['id'])
