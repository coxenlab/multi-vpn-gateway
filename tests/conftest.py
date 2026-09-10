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
        c.execute("DELETE FROM channels")
        c.execute("DELETE FROM rules")
        c.execute("DELETE FROM domains")
        c.execute("DELETE FROM mirrors")
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
def client(monkeypatch):
    import manager, replacement
    monkeypatch.setattr(manager, "rebuild", lambda: 204)
    def provision(ch, fields, force_start=False):
        assert not ch.get('container_id'), 'existing replacement requires its own fixture'
        store.set_container(ch['id'], 'cid_fake', 18080, 'running')
    monkeypatch.setattr(replacement, 'replace', provision)
    monkeypatch.setattr(manager, "stop", lambda cid: None)
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
