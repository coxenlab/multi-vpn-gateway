import dockerhub
import pytest

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
    def __init__(self, body=FAKE_PAGE, status=200, headers=None):
        self.body, self.status_code, self.headers = body, status, headers or {}
    def raise_for_status(self): pass
    def json(self): return self.body


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


def test_versions_follow_pages_sort_dedup_and_cache_only_complete_results(monkeypatch):
    dockerhub._CACHE.clear()
    calls = []
    pages = [
        {"next": "?page=2&page_size=100", "results": [{"name": f"ci-{i}"} for i in range(99)] + [
            {"name": "7.6.3", "images": [{"architecture": "amd64"}]}]},
        {"next": None, "results": [
            {"name": "7.6.7", "images": [{"architecture": "arm64"}]},
            {"name": "7.10.0", "images": [{"architecture": "arm64"}]},
            {"name": "7.6.3", "images": [{"architecture": "arm64"}]}]},
    ]
    def get(url, **kw):
        assert kw["allow_redirects"] is False and 0 < kw["timeout"] <= 8
        assert "hagb/docker-easyconnect" not in dockerhub._CACHE
        calls.append(url)
        return _Resp(pages[len(calls)-1])
    monkeypatch.setattr(dockerhub.requests, "get", get)
    result = dockerhub.versions("hagb/docker-easyconnect", "arm64", ["7.6.3"])
    assert len(calls) == 2 and "page=2" in calls[1]
    assert [v["tag"] for v in result] == ["7.10.0", "7.6.7", "7.6.3"]
    assert result[-1]["usable_here"] is False  # Preserve the first observation of a duplicate tag.
    assert dockerhub.versions("hagb/docker-easyconnect", "arm64", []) == result
    assert len(calls) == 2


def test_same_resource_canonical_redirect_is_followed_without_leaving_hub(monkeypatch):
    dockerhub._CACHE.clear(); calls = []
    def get(url, **kw):
        calls.append(url)
        if len(calls) == 1:
            return _Resp(status=301, headers={"Location": "/v2/namespaces/hagb/repositories/docker-easyconnect/tags/?page_size=100"})
        return _Resp()
    monkeypatch.setattr(dockerhub.requests, "get", get)
    assert dockerhub.versions("hagb/docker-easyconnect", "arm64", [])[0]["tag"] == "7.6.7"
    assert len(calls) == 2 and "/tags/?" in calls[1]


def test_later_page_failure_keeps_complete_stale_cache_and_architecture(monkeypatch):
    repo = "hagb/docker-easyconnect"
    old = (0, [{"tag": "7.6.3", "arch": ["amd64"]}])
    dockerhub._CACHE.clear(); dockerhub._CACHE[repo] = old
    calls = []
    def get(url, **kw):
        calls.append(url)
        if len(calls) == 2: raise dockerhub.requests.HTTPError("fixture page two failed")
        return _Resp({"next": "?page=2", "results": [{"name": "9.9", "images": []}]})
    monkeypatch.setattr(dockerhub.requests, "get", get)
    assert dockerhub.versions(repo, "arm64", ["1.0"]) == [{"tag": "7.6.3", "arch": ["amd64"], "usable_here": False}]
    assert dockerhub._CACHE[repo] == old and len(calls) == 2


@pytest.mark.parametrize("body", [
    {}, [], {"results": [], "next": 42},
    {"results": [], "next": "?page_size=100"},
    {"results": [], "next": "https://example.invalid/tags?page=2"},
    {"results": [], "next": "/v2/namespaces/other/repositories/other/tags?page=2"},
    {"results": [{"name": "7.6.7", "images": "invalid"}], "next": None},
])
def test_bad_or_cyclic_pagination_falls_back_without_caching(monkeypatch, body):
    dockerhub._CACHE.clear(); calls = []
    def get(url, **kw): calls.append(url); return _Resp(body)
    monkeypatch.setattr(dockerhub.requests, "get", get)
    assert dockerhub.versions("hagb/docker-easyconnect", "arm64", ["1.0"])[0]["tag"] == "1.0"
    assert not dockerhub._CACHE and len(calls) == 1


def test_pagination_budget_does_not_cache_partial_results(monkeypatch):
    dockerhub._CACHE.clear(); clock = iter([0, 1, 9]); calls = []
    monkeypatch.setattr(dockerhub.time, "monotonic", lambda: next(clock))
    def get(url, **kw):
        calls.append(url)
        return _Resp({"results": [{"name": "9.9"}], "next": "?page=2"})
    monkeypatch.setattr(dockerhub.requests, "get", get)
    assert dockerhub.versions("hagb/docker-easyconnect", "arm64", ["1.0"])[0]["tag"] == "1.0"
    assert not dockerhub._CACHE and len(calls) == 1
