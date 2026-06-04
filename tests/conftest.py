import os
import tempfile

# 必须在 import store/main/manager 之前设好环境(它们在模块级读 env)
_TMP = tempfile.mkdtemp(prefix="vpnmgr-test-")
os.environ.setdefault("DATA_DIR", _TMP)
os.environ.setdefault("VPN_NET", "testnet")
os.environ.setdefault("MIHOMO_CTRL_URL", "http://mihomo-test:9090")
os.environ.setdefault("MIHOMO_SECRET", "test-secret")
os.environ.setdefault("MIHOMO_CONFIG_PATH", os.path.join(_TMP, "config.yaml"))
os.environ.setdefault("MIHOMO_HOST_PORT", "48721")
os.environ.setdefault("MIHOMO_CTRL_PORT", "20933")
os.environ.setdefault("UI_PORT", "42411")

import sys
sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "app"))

import pytest
import store


@pytest.fixture(autouse=True)
def clean_db():
    store.init()
    with store._c() as c:
        c.execute("DELETE FROM channels")
        c.execute("DELETE FROM rules")
        c.execute("DELETE FROM domains")
        c.execute("DELETE FROM mirrors")
    store.init()       # 重新播种默认镜像源
    yield


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
    import manager
    monkeypatch.setattr(manager, "rebuild", lambda: 204)
    monkeypatch.setattr(manager, "create_channel", lambda ch, vnc: ("cid_fake", 18080))
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
