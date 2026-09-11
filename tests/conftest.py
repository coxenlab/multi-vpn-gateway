import os
import tempfile
from pathlib import Path

# 必须在 import store/main/manager 之前设好环境(它们在模块级读 env)
_TMP = tempfile.mkdtemp(prefix="vpnmgr-test-")
os.environ["DATA_DIR"] = _TMP
os.environ["VPN_NET"] = "testnet"
os.environ["MIHOMO_CTRL_URL"] = "http://mihomo-test:9090"
os.environ["MIHOMO_SECRET"] = "test-secret"
os.environ["MIHOMO_CONFIG_PATH"] = os.path.join(_TMP, "config.yaml")
os.environ["MIHOMO_HOST_PORT"] = "48721"
os.environ["MIHOMO_CTRL_PORT"] = "20933"
os.environ["UI_PORT"] = "42411"
os.environ["DOCKER_HOST"] = "unix://" + os.path.join(_TMP, "absent-docker.sock")
assert Path(os.environ["DATA_DIR"]).resolve() == Path(_TMP).resolve()
assert Path(_TMP).resolve().is_relative_to(Path(tempfile.gettempdir()).resolve())

import sys
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "app"))

import pytest
import store

# manager 在 import 时创建 SDK 客户端。显式版本避免 SDK 为自动协商接触真实 daemon。
import docker
import requests
_import_patch = pytest.MonkeyPatch()
_import_patch.setattr(docker, "from_env", lambda **kw: docker.DockerClient(
    base_url=os.environ["DOCKER_HOST"], version="1.45"))


def pytest_unconfigure(config):
    _import_patch.undo()


@pytest.fixture(autouse=True)
def no_external_requests(monkeypatch):
    def blocked(*args, **kwargs):
        raise AssertionError("单元测试不得访问真实 Docker/网络；请注入替身")
    monkeypatch.setattr(requests.sessions.Session, "send", blocked)


@pytest.fixture(autouse=True)
def clean_db():
    import channel_state
    channel_state.shutdown()
    channel_state.startup()
    store.init()
    with store._c() as c:
        c.execute("DELETE FROM channel_replacements")
        c.execute("DELETE FROM channel_runtime")
        c.execute("DELETE FROM channel_stop_intents")
        c.execute("DELETE FROM channels")
        c.execute("DELETE FROM rules")
        c.execute("DELETE FROM domains")
        c.execute("DELETE FROM mirrors")
        c.execute("DELETE FROM config_apply_state")
    store.init()       # 重新播种默认镜像源
    yield
    channel_state.shutdown()


@pytest.fixture
def make_channel():
    def _mk(cid, **over):
        ch = {
            "id": cid, "name": cid, "vpn_type": "easyconnect", "server": "https://x",
            "ec_ver": "7.6.3", "login_method": "interactive", "username": "",
            "password": over.get("password", ""), "vnc_password": "vnc12345",
            "mac": "02:00:00:00:00:01", "probe_url": "http://p", "status": "running",
        }
        ch.update({k: v for k, v in over.items() if k in ch})
        return ch
    return _mk


@pytest.fixture
def mihomo_controller(monkeypatch, tmp_path):
    """控制器替身只验证调用协议与故障补偿；真实 mihomo 验收另行执行。"""
    import copy
    import manager
    import yaml
    path = tmp_path / "config.yaml"
    monkeypatch.setattr(manager, "CFG", str(path))
    monkeypatch.setenv("MIHOMO_CONFIG_PATH", str(path))
    state = {"config": {"proxies": [], "rules": ["MATCH,DIRECT"]}, "puts": 0, "flushes": 0, "reject": False, "lost_ack": False}

    class Response:
        def __init__(self, data, status=200):
            self.data, self.status_code = data, status
        def json(self): return copy.deepcopy(self.data)
        def raise_for_status(self):
            if self.status_code >= 400: raise requests.HTTPError("fixture rejection")

    def put(url, **kw):
        assert url.endswith('/configs') and kw['params'] == {'force': 'true'}
        assert 'payload' in kw['json'] and 'path' not in kw['json']
        state['puts'] += 1
        if state['reject']: return Response({}, 400)
        state['config'] = yaml.safe_load(kw['json']['payload'])
        if state['lost_ack']: raise requests.Timeout('fixture lost ACK')
        return Response({}, 204)

    def get(url, **kw):
        config = state['config']
        if url.endswith('/proxies'):
            return Response({'proxies': {p['name']: {'type': 'Socks5' if p['type'] == 'socks5' else 'Direct'} for p in config['proxies']}})
        if url.endswith('/configs'): return Response({'mode': config.get('mode', 'rule')})
        assert url.endswith('/rules')
        rules = []
        for value in config['rules']:
            parts = value.split(',')
            kind, payload, proxy = ('Match', '', parts[1]) if parts[0] == 'MATCH' else (
                'DomainSuffix' if parts[0] == 'DOMAIN-SUFFIX' else 'IPCIDR', parts[1], parts[2])
            rules.append({'type': kind, 'payload': payload, 'proxy': proxy, 'extra': {'disabled': state.get('disabled', False)}})
        return Response({'rules': rules})

    def post(url, **kw):
        assert url.endswith('/cache/dns/flush')
        state['flushes'] += 1
        return Response({}, 503 if state.get('reject_flush') else 204)

    monkeypatch.setattr(requests, "post", post)
    monkeypatch.setattr(requests, "put", put)
    monkeypatch.setattr(requests, "get", get)
    return state


@pytest.fixture
def client(monkeypatch):
    import manager, replacement
    monkeypatch.setattr(manager, "rebuild", lambda: 204)
    def provision(ch, fields, force_start=False):
        assert not ch.get('container_id'), 'existing replacement requires its own fixture'
        store.set_container(ch['id'], 'cid_fake', 18080, 'running')
    monkeypatch.setattr(replacement, 'replace', provision)
    from types import SimpleNamespace
    instances = {}
    class Container:
        def __init__(self, cid):
            self.id = 'cid_fake'
            self.attrs = {'Name': '/vpn-' + cid, 'State': {'Running': True}}
        def stop(self): self.attrs['State']['Running'] = False
    def get(identity):
        if identity.startswith('vpn-'):
            cid = identity[4:]
            ch = store.get_channel(cid)
            if ch and ch.get('container_id') == 'cid_fake':
                instances.setdefault(cid, Container(cid))
                return instances[cid]
        else:
            for instance in instances.values():
                if instance.id == identity: return instance
        raise docker.errors.NotFound('missing fixture container')
    monkeypatch.setattr(manager, 'dc', SimpleNamespace(containers=SimpleNamespace(get=get)))
    monkeypatch.setattr(manager, "novnc_port", lambda cid: 18080)        # 不碰真 docker:登录 url 用此端口
    monkeypatch.setattr(manager, "ensure_novnc_bridge", lambda cid: None)
    monkeypatch.setattr(manager, "probe", lambda ch: (True, 42))
    monkeypatch.setattr(manager, "uptime", lambda cid: "1分钟")
    monkeypatch.setattr(manager, "mihomo_alive", lambda: True)
    monkeypatch.setattr(manager, "logs", lambda cid, tail=200: ["line1", "line2"])
    monkeypatch.setattr(manager, "connections",
                        lambda: {"connections": [], "downloadTotal": 0, "uploadTotal": 0})
    import main
    from fastapi.testclient import TestClient
    return TestClient(main.app)
