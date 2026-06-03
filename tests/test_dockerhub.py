import dockerhub

FAKE_PAGE = {"results": [
    {"name": "latest",        "images": [{"architecture": "amd64"}, {"architecture": "arm64"}]},
    {"name": "vncless",       "images": [{"architecture": "amd64"}]},
    {"name": "cli",           "images": [{"architecture": "amd64"}]},
    {"name": "7.6.3",         "images": [{"architecture": "amd64"}, {"architecture": "arm64"}]},
    {"name": "7.6.7",         "images": [{"architecture": "amd64"}, {"architecture": "arm64"}]},
    {"name": "vncless-7.6.3", "images": [{"architecture": "amd64"}]},
    {"name": "dev-7.6.7",     "images": [{"architecture": "amd64"}]},
    {"name": "actions-test",  "images": [{"architecture": "amd64"}]},
    {"name": "cron-test-7.6.3", "images": [{"architecture": "amd64"}]},
]}


class _Resp:
    status_code = 200
    def raise_for_status(self): pass
    def json(self): return FAKE_PAGE


def test_versions_keeps_only_semver_and_marks_arch(monkeypatch):
    monkeypatch.setattr(dockerhub.requests, "get", lambda *a, **k: _Resp())
    dockerhub._CACHE.clear()
    vs = dockerhub.versions("hagb/docker-easyconnect", host_arch="arm64",
                            fallback=["7.6.3"])
    tags = [v["tag"] for v in vs]
    assert tags == ["7.6.7", "7.6.3"]          # 倒序;CI/变体全过滤
    by = {v["tag"]: v for v in vs}
    assert by["7.6.3"]["usable_here"] is True
    assert set(by["7.6.3"]["arch"]) == {"amd64", "arm64"}


def test_versions_offline_falls_back(monkeypatch):
    def boom(*a, **k): raise dockerhub.requests.RequestException("offline")
    monkeypatch.setattr(dockerhub.requests, "get", boom)
    dockerhub._CACHE.clear()
    vs = dockerhub.versions("hagb/docker-easyconnect", host_arch="arm64",
                            fallback=["7.6.3", "7.6.7"])
    assert [v["tag"] for v in vs] == ["7.6.3", "7.6.7"]
    assert all(v["usable_here"] for v in vs)   # 兜底项默认可用
