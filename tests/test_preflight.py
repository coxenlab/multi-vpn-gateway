import docker
import preflight
import pytest
import uuid


def test_resolve_image_substitutes_ec_version():
    assert preflight.resolve_image("easyconnect", "7.6.7") == "hagb/docker-easyconnect:7.6.7"


def test_resolve_image_defaults_ec_version_when_missing():
    assert preflight.resolve_image("easyconnect", None) == "hagb/docker-easyconnect:7.6.3"


def test_resolve_image_literal_for_atrust():
    assert preflight.resolve_image("atrust", None) == "hagb/docker-atrust:latest"


def test_resolve_image_literal_for_oss():
    assert preflight.resolve_image("anyconnect", None) == "vpnmgr/oss-vpn:latest"


def test_known_repos_contains_upstream_and_selfbuilt():
    repos = preflight.known_repos()
    assert "hagb/docker-atrust" in repos
    assert "hagb/docker-easyconnect" in repos
    assert "vpnmgr/oss-vpn" in repos


def test_is_buildable_only_for_vpnmgr():
    assert preflight.is_buildable("vpnmgr/oss-vpn:latest") is True
    assert preflight.is_buildable("hagb/docker-atrust:latest") is False


class _FakeImages:
    def __init__(self, store):
        self._store = store        # {image_name: arch}
    def get(self, name):
        if name not in self._store:
            raise docker.errors.ImageNotFound(name)
        return type("Img", (), {"id": f"sha256:{name}-{self._store[name]}", "attrs": {"Architecture": self._store[name]}})()


class _FakeNetworks:
    def __init__(self, ok):
        self._ok = ok
    def get(self, name):
        if not self._ok:
            raise docker.errors.NotFound(name)
        return object()


class _FakeContainers:
    def __init__(self, tun_ok=True, raise_exc=None):
        self._tun_ok = tun_ok
        self._raise = raise_exc
        self.created = []
        self.removed = []
    def create(self, image, **kw):
        if self._raise:
            raise self._raise
        self.created.append((image, kw))
        parent = self
        class Probe:
            id = "probe-owned-id"
            attrs = {"Config": {"Labels": kw["labels"]}}
            def start(self):
                pass
            def wait(self, timeout):
                assert timeout == 10
                return {"StatusCode": 0 if parent._tun_ok else 1}
            def remove(self, **kwargs):
                parent.removed.append((self.id, kwargs))
        return Probe()
    def get(self, name):
        raise docker.errors.NotFound(name)


class _FakeDc:
    def __init__(self, ping=True, images=None, networks_ok=True, df=None, kw_tun=True, kw_raise=None):
        self._ping = ping
        self._id = uuid.uuid4().hex
        self.images = _FakeImages(images or {})
        self.networks = _FakeNetworks(networks_ok)
        self._df = df or {"LayersSize": 0}
        self.containers = _FakeContainers(tun_ok=kw_tun, raise_exc=kw_raise)
    def ping(self):
        if not self._ping:
            raise docker.errors.APIError("daemon down")
        return True
    def df(self):
        return self._df
    def version(self):
        return {"Version": "27.0.1"}
    def info(self):
        return {"ID": self._id}


def test_daemon_pass():
    r = preflight.check_docker_daemon(_FakeDc(ping=True))
    assert r["status"] == "pass"


def test_daemon_fail_has_tutorial_fix():
    r = preflight.check_docker_daemon(_FakeDc(ping=False))
    assert r["status"] == "fail"
    assert r["fix"]["kind"] == "tutorial"
    assert r["fix"]["action"] == "install_docker"


def test_image_present_pass():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"})
    r = preflight.check_image_present(dc, "hagb/docker-atrust:latest")
    assert r["status"] == "pass"


def test_image_missing_upstream_auto_pull():
    dc = _FakeDc(images={})
    r = preflight.check_image_present(dc, "hagb/docker-atrust:latest")
    assert r["status"] == "fail"
    assert r["fix"]["kind"] == "auto" and r["fix"]["action"] == "pull_image"
    assert r["fix"]["params"]["image"] == "hagb/docker-atrust:latest"


def test_image_missing_selfbuilt_shows_build_cmd_not_pull():
    dc = _FakeDc(images={})
    r = preflight.check_image_present(dc, "vpnmgr/oss-vpn:latest")
    assert r["status"] == "fail"
    assert r["fix"]["kind"] == "none"          # 自建镜像不自动拉
    assert "docker build" in r["detail"] and "images/oss" in r["detail"]


def test_arch_match_pass():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"})
    r = preflight.check_image_arch_match(dc, "hagb/docker-atrust:latest", "arm64")
    assert r["status"] == "pass"

def test_arch_mismatch_fail_auto_pull_with_arch():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "amd64"})
    r = preflight.check_image_arch_match(dc, "hagb/docker-atrust:latest", "arm64")
    assert r["status"] == "fail"
    assert r["fix"]["action"] == "pull_image"
    assert r["fix"]["params"]["arch"] == "arm64"

def test_arch_missing_image_skips():
    dc = _FakeDc(images={})
    r = preflight.check_image_arch_match(dc, "hagb/docker-atrust:latest", "arm64")
    assert r["status"] == "skip"

def test_arch_unknown_warns():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": ""})
    r = preflight.check_image_arch_match(dc, "hagb/docker-atrust:latest", "arm64")
    assert r["status"] == "warn"


def test_vpn_network_pass():
    r = preflight.check_vpn_network(_FakeDc(networks_ok=True), "vpnnet")
    assert r["status"] == "pass"

def test_vpn_network_missing_auto_create():
    r = preflight.check_vpn_network(_FakeDc(networks_ok=False), "vpnnet")
    assert r["status"] == "fail"
    assert r["fix"]["kind"] == "auto" and r["fix"]["action"] == "create_network"
    assert r["fix"]["params"]["name"] == "vpnnet"


def test_tun_pass_when_probe_exits_zero():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"}, kw_tun=True)
    r = preflight.check_dev_net_tun(dc, "hagb/docker-atrust:latest", image_ok=True)
    assert r["status"] == "pass"

def test_tun_warn_when_probe_nonzero():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"}, kw_tun=False)
    r = preflight.check_dev_net_tun(dc, "hagb/docker-atrust:latest", image_ok=True)
    assert r["status"] == "warn"

def test_tun_skip_when_image_absent():
    r = preflight.check_dev_net_tun(_FakeDc(), "hagb/docker-atrust:latest", image_ok=False)
    assert r["status"] == "skip"


@pytest.mark.parametrize("mode", ["wait_error", "empty", "bad_status", "embedded_error", "start_error", "cleanup_error", "lost_create", "foreign"])
def test_tun_errors_never_pass_and_cleanup_only_owned_probe(monkeypatch, mode):
    dc = _FakeDc(images={"fixture:tag": "arm64"})
    original = dc.containers.create
    def create(image, **kwargs):
        probe = original(image, **kwargs)
        def fail(*args, **kwargs):
            raise docker.errors.APIError("fixture failure")
        if mode == "start_error":
            probe.start = fail
        elif mode == "cleanup_error":
            probe.remove = fail
        elif mode == "wait_error":
            probe.wait = fail
        elif mode in ("empty", "bad_status", "embedded_error"):
            status = {"empty": {}, "bad_status": {"StatusCode": False},
                      "embedded_error": {"StatusCode": 0, "Error": {"Message": "wait failed"}}}[mode]
            probe.wait = lambda **kwargs: status
        if mode in ("lost_create", "foreign"):
            if mode == "foreign":
                probe.attrs = {"Config": {"Labels": {"com.vpnmgr.role": "other"}}}
            dc.containers.get = lambda name: probe
            fail()
        return probe
    monkeypatch.setattr(dc.containers, "create", create)
    check = preflight.check_dev_net_tun(dc, "fixture:tag", True)
    assert check["status"] == "warn"
    assert "无法判定" in check["detail"]
    image, kwargs = dc.containers.created[0]
    assert image == "sha256:fixture:tag-arm64"
    assert kwargs["name"].startswith("vpncore-tun-probe-") and len(kwargs["name"]) > 40
    assert kwargs["network_mode"] == "none"
    assert len(dc.containers.removed) == (0 if mode in ("foreign", "cleanup_error") else 1)
    assert all(item == ("probe-owned-id", {"force": True, "v": True}) for item in dc.containers.removed)


def test_tun_reuses_result_but_rechecks_manual_image_or_daemon_change(monkeypatch):
    now = [100.0]
    cache = preflight._TunChecks(clock=lambda: now[0])
    monkeypatch.setattr(preflight, "_tun_checks", cache)
    dc = _FakeDc(images={"fixture:tag": "arm64"})
    def check(fresh=False):
        return preflight.check_dev_net_tun(dc, "fixture:tag", True, fresh)
    assert check()["status"] == "pass"
    now[0] += 1
    assert "复用 1 秒前" in check()["detail"]
    assert len(dc.containers.created) == 1
    assert "复用" not in check(True)["detail"]
    dc.images._store["fixture:tag"] = "amd64"
    check()
    dc._id = "different-daemon"
    check()
    assert len(dc.containers.created) == 4
    now[0] += 60
    dc.containers._tun_ok = False
    assert check()["status"] == "warn"
    now[0] += 4
    assert "复用" in check()["detail"]
    now[0] += 1
    check()
    assert len(dc.containers.created) == 6
    assert len({kwargs["name"] for _, kwargs in dc.containers.created}) == 6
    # 新键不会让历史检测记录无限增长。
    for index in range(40):
        dc._id = f"daemon-{index}"
        check()
    assert len(cache.samples) == 32


def test_tun_overlapping_manual_checks_share_completed_probe():
    from concurrent.futures import ThreadPoolExecutor
    import threading
    entered, release, joined = threading.Event(), threading.Event(), threading.Event()
    clock_calls = 0
    def clock():
        nonlocal clock_calls
        clock_calls += 1
        if clock_calls == 3:  # 第二个请求开始等第一轮，尚未完成。
            joined.set()
        return float(clock_calls)
    cache = preflight._TunChecks(clock=clock)
    calls = []
    def probe():
        calls.append(True)
        entered.set()
        assert release.wait(3)
        return preflight._result("dev_net_tun", "运行条件", "TUN", "pass")
    with ThreadPoolExecutor(max_workers=2) as pool:
        first = pool.submit(cache.sample, ("daemon", "image"), True, probe)
        assert entered.wait(3)
        second = pool.submit(cache.sample, ("daemon", "image"), True, probe)
        try:
            assert joined.wait(3)
        finally:
            release.set()
        assert first.result(timeout=3)["status"] == "pass"
        assert "复用" in second.result(timeout=3)["detail"]
    assert len(calls) == 1

def test_disk_space_informational_pass():
    dc = _FakeDc(df={"LayersSize": 2 * 1024**3})
    r = preflight.check_disk_space(dc)
    assert r["status"] in ("pass", "warn")
    assert "GB" in r["detail"]


def test_run_checks_daemon_down_skips_dependents():
    out = preflight.run_checks(_FakeDc(ping=False), "atrust", None)
    assert out["overall"] == "fail"
    by = {c["id"]: c for c in out["checks"]}
    assert by["docker_daemon"]["status"] == "fail"
    # 守护进程挂 → 其余依赖项 skip
    assert by["image_present"]["status"] == "skip"
    assert by["vpn_network"]["status"] == "skip"

def test_run_checks_arch_mismatch_overall_fail():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "amd64"}, networks_ok=True)
    out = preflight.run_checks(dc, "atrust", None, host_arch="arm64", vpn_net="vpnnet")
    by = {c["id"]: c for c in out["checks"]}
    assert by["image_arch_match"]["status"] == "fail"
    assert out["overall"] == "fail"
    assert out["target_image"] == "hagb/docker-atrust:latest"

def test_run_checks_all_pass_overall_pass():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"}, networks_ok=True, kw_tun=True)
    out = preflight.run_checks(dc, "atrust", None, host_arch="arm64", vpn_net="vpnnet")
    assert out["overall"] in ("pass", "warn")   # disk/tun 可能 warn,但无 fail
    assert all(c["status"] != "fail" for c in out["checks"])


def test_pull_worker_first_mirror_ok_retags(monkeypatch):
    pulled = {}
    class Img:
        attrs = {"Architecture": "arm64"}
        def tag(self, repo, tag): pulled["tagged"] = f"{repo}:{tag}"
    class Imgs:
        def pull(self, ref, tag=None, platform=None):
            pulled["ref"] = f"{ref}:{tag}"; pulled["platform"] = platform; return Img()
        def remove(self, ref, force=False): pulled["removed"] = ref
    class Dc: images = Imgs()
    monkeypatch.setattr(preflight, "_mirror_reachable", lambda h, timeout=5: True)
    st = {"status": "running", "progress": "", "log_tail": [], "error": None}
    preflight._pull_worker(Dc(), "hagb/docker-atrust:latest", "arm64",
                           ["docker.1ms.run"], st)
    assert st["status"] == "done"
    assert pulled["ref"] == "docker.1ms.run/hagb/docker-atrust:latest"
    assert pulled["platform"] == "linux/arm64"
    assert pulled["tagged"] == "hagb/docker-atrust:latest"
    assert pulled["removed"] == "docker.1ms.run/hagb/docker-atrust:latest"

def test_pull_worker_all_mirrors_fail_errors(monkeypatch):
    monkeypatch.setattr(preflight, "_mirror_reachable", lambda h, timeout=5: False)
    st = {"status": "running", "progress": "", "log_tail": [], "error": None}
    preflight._pull_worker(object(), "hagb/docker-atrust:latest", "arm64",
                           ["docker.1ms.run", "hub.rat.dev"], st)
    assert st["status"] == "error"
    assert "国内源" in st["error"]

def test_start_pull_returns_task_id_and_get_task(monkeypatch):
    monkeypatch.setattr(preflight, "_TASKS", preflight._PullTasks())
    monkeypatch.setattr(preflight.threading.Thread, "start", lambda self: None)
    tid = preflight.start_pull(object(), "hagb/docker-atrust:latest", "arm64",
                               mirrors=["docker.1ms.run"])
    assert preflight.get_task(tid)["status"] == "running"
    assert preflight.get_task("nope") is None


def test_pull_tasks_dedup_and_bound_real_workers(monkeypatch):
    from concurrent.futures import ThreadPoolExecutor
    import threading
    from types import SimpleNamespace
    import pytest
    clock = [0.0]
    tasks = preflight._PullTasks(clock=lambda: clock[0])
    monkeypatch.setattr(preflight, "_TASKS", tasks)
    release = threading.Event()
    started, threads = [], []

    def worker(dc, image, arch, mirrors, state, publish):
        started.append(image)
        assert release.wait(5)
        state.update(status="done", progress="complete")

    monkeypatch.setattr(preflight, "_pull_worker", worker)
    def owned_thread(*args, **kw):
        thread = threading.Thread(*args, **kw); threads.append(thread); return thread
    monkeypatch.setattr(preflight, "threading", SimpleNamespace(Thread=owned_thread))
    try:
        with ThreadPoolExecutor(max_workers=8) as callers:
            tids = list(callers.map(lambda i: preflight.start_pull(object(), "hagb/docker-atrust" + (":latest" if i % 2 else ""), "arm64"), range(16)))
        assert len(set(tids)) == 1 and len(threads) == 1
        other = preflight.start_pull(object(), "hagb/docker-easyconnect:7.6.7", "arm64")
        clock[0] = 7200  # Even an old live worker still occupies its slot.
        with pytest.raises(preflight.PullBusyError):
            preflight.start_pull(object(), "hagb/docker-atrust", "amd64")
        assert preflight.get_task(tids[0])["status"] == "running"
        state = preflight.get_task(tids[0]); state["log_tail"].append("external mutation")
        assert preflight.get_task(tids[0])["log_tail"] == []
        assert preflight.start_pull(object(), "hagb/docker-atrust", "arm64") == tids[0]
    finally:
        release.set()
        for thread in threads: thread.join(5)
    assert all(not thread.is_alive() for thread in threads)
    assert len(started) == 2
    assert tasks.get(other)["status"] == "done"
    clock[0] += 3600
    assert tasks.get(tids[0]) is None


def test_pull_history_bound_and_unexpected_worker_exit(monkeypatch):
    tasks = preflight._PullTasks()
    monkeypatch.setattr(preflight, "_TASKS", tasks)
    for index in range(40):
        tid, _ = tasks.reserve(str(index), "arm64")
        state = tasks.get(tid); state["status"] = "done"
        tasks.publish(tid, state, finished=True)
    assert len(tasks.entries) == 32
    monkeypatch.setattr(preflight, "_pull_worker", lambda *a: (_ for _ in ()).throw(RuntimeError("fixture")))
    class InlineThread:
        def __init__(self, target, **kw): self.target = target
        def start(self): self.target()
    monkeypatch.setattr(preflight.threading, "Thread", InlineThread)
    tid = preflight.start_pull(object(), "fixture", "arm64")
    assert tasks.get(tid)["status"] == "error"
    assert "fixture" not in tasks.get(tid)["error"]
    assert preflight.start_pull(object(), "fixture", "arm64") != tid


def test_pull_worker_cannot_start_releases_slot(monkeypatch):
    tasks = preflight._PullTasks()
    monkeypatch.setattr(preflight, "_TASKS", tasks)
    def fail(thread): raise RuntimeError("fixture")
    monkeypatch.setattr(preflight.threading.Thread, "start", fail)
    for _ in range(3):
        tid = preflight.start_pull(object(), "fixture", "arm64")
        assert tasks.get(tid)["status"] == "error"


def test_docker_version_pass():
    r = preflight.check_docker_version(_FakeDc())
    assert r["status"] == "pass" and "27.0.1" in r["detail"]

def test_mirror_reachable_all_down_warns(monkeypatch):
    monkeypatch.setattr(preflight, "_mirror_reachable", lambda h, timeout=5: False)
    r = preflight.check_mirror_reachable(["a.com", "b.com"])
    assert r["status"] == "warn"

def test_mirror_reachable_one_up_pass(monkeypatch):
    monkeypatch.setattr(preflight, "_mirror_reachable", lambda h, timeout=5: h == "b.com")
    r = preflight.check_mirror_reachable(["a.com", "b.com"])
    assert r["status"] == "pass"

def test_mihomo_health(monkeypatch):
    assert preflight.check_mihomo(True)["status"] == "pass"
    assert preflight.check_mihomo(False)["status"] == "warn"

def test_run_checks_full_scope_has_extra_checks():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"}, networks_ok=True)
    out = preflight.run_checks(dc, "atrust", None, host_arch="arm64", vpn_net="vpnnet",
                               scope="full", mirrors=["docker.1ms.run"], mihomo_alive=True)
    ids = {c["id"] for c in out["checks"]}
    assert {"docker_version", "host_arch", "mirror_reachable", "mihomo_health"} <= ids

def test_run_checks_preflight_scope_excludes_extra():
    dc = _FakeDc(images={"hagb/docker-atrust:latest": "arm64"}, networks_ok=True)
    out = preflight.run_checks(dc, "atrust", None, host_arch="arm64", vpn_net="vpnnet")
    ids = {c["id"] for c in out["checks"]}
    assert "mihomo_health" not in ids and "docker_version" not in ids


def test_known_repos_contains_mihomo():
    repos = preflight.known_repos()
    assert "metacubex/mihomo" in repos


def test_infra_images_declared():
    imgs = {i["image"] for i in preflight.INFRA_IMAGES}
    assert preflight.MIHOMO_IMAGE in imgs
    assert "app" in imgs


def _inv(monkeypatch, images=None):
    import dockerhub
    monkeypatch.setattr(dockerhub, "versions",
                        lambda repo, arch, fb: [{"tag": "7.6.7", "arch": ["arm64"], "usable_here": True},
                                                {"tag": "7.6.3", "arch": ["amd64"], "usable_here": False}])
    dc = _FakeDc(images=images or {})
    out = preflight.image_inventory(dc, "arm64", ["docker.1ms.run"])
    return out, {e["image"]: e for e in out["images"]}


def test_inventory_top_level_shape(monkeypatch):
    out, _ = _inv(monkeypatch)
    assert out["host_arch"] == "arm64"
    assert out["mirrors"] == ["docker.1ms.run"]
    assert isinstance(out["images"], list) and out["images"]


def test_inventory_dedups_oss_collects_used_by(monkeypatch):
    _, by = _inv(monkeypatch)
    oss = by["vpnmgr/oss-vpn:latest"]
    assert oss["kind"] == "build"
    assert oss["build_context"] == "images/oss"
    assert len(oss["used_by"]) == 8


def test_inventory_ec_versioned_attaches_live_versions(monkeypatch):
    _, by = _inv(monkeypatch)
    ec = by["hagb/docker-easyconnect"]
    assert ec["versioned"] is True
    assert ec["kind"] == "pull"
    assert ec["present"] is None
    assert ec["versions"][0]["tag"] == "7.6.7"


def test_inventory_fixed_pull_has_present_and_single_version(monkeypatch):
    _, by = _inv(monkeypatch, images={"hagb/docker-atrust:latest": "arm64"})
    at = by["hagb/docker-atrust:latest"]
    assert at["kind"] == "pull" and at["versioned"] is False
    assert at["present"] is True
    assert at["versions"] == [{"tag": "latest", "arch": ["amd64", "arm64"], "usable_here": True}]


def test_inventory_includes_infra(monkeypatch):
    _, by = _inv(monkeypatch)
    assert by[preflight.MIHOMO_IMAGE]["role"] == "infra"
    app = by["app"]
    assert app["kind"] == "compose"
    assert app["present"] is None and app["versions"] == []


def test_inventory_present_false_when_missing(monkeypatch):
    _, by = _inv(monkeypatch, images={})
    assert by["hagb/docker-atrust:latest"]["present"] is False
    assert by["vpnmgr/oss-vpn:latest"]["present"] is False


def test_inventory_versioned_uses_real_fallback(monkeypatch):
    import dockerhub
    seen = {}
    def fake_versions(repo, arch, fb):
        seen["fb"] = fb
        return [{"tag": t, "arch": [], "usable_here": True} for t in fb]
    monkeypatch.setattr(dockerhub, "versions", fake_versions)
    out = preflight.image_inventory(_FakeDc(images={}), "arm64", [])
    by = {e["image"]: e for e in out["images"]}
    # easyconnect 在 adapters.yaml 里 fallback_versions: ["7.6.3", "7.6.7"]
    assert seen["fb"] == ["7.6.3", "7.6.7"]
    assert [v["tag"] for v in by["hagb/docker-easyconnect"]["versions"]] == ["7.6.3", "7.6.7"]


def test_mihomo_pull_uses_digest_and_rejects_content_before_tagging(monkeypatch):
    from types import SimpleNamespace
    from unittest.mock import Mock
    pinned = preflight.MIHOMO_SOURCE['platforms']['arm64']
    monkeypatch.setattr(preflight, '_mirror_reachable', lambda _: True)
    for image_id, architecture, succeeds in [(pinned['config'], 'arm64', True),
                                            (pinned['manifest'], 'arm64', True),
                                            (preflight.MIHOMO_SOURCE['index'], 'arm64', True),
                                            ('sha256:' + '0' * 64, 'arm64', False),
                                            (pinned['config'], 'amd64', False)]:
        image = SimpleNamespace(id=image_id, attrs={'Architecture': architecture}, tag=Mock())
        images = SimpleNamespace(pull=Mock(return_value=image), remove=Mock())
        state = {'status': 'running', 'log_tail': []}
        preflight._pull_worker(SimpleNamespace(images=images), preflight.MIHOMO_IMAGE, 'arm64', ['mirror.invalid'], state)
        images.pull.assert_called_once_with('mirror.invalid/metacubex/mihomo@' + pinned['manifest'], platform='linux/arm64')
        assert state['status'] == ('done' if succeeds else 'error')
        assert image.tag.call_count == (1 if succeeds else 0)
        images.remove.assert_not_called()


def test_compose_uses_same_pinned_mihomo_index():
    from pathlib import Path
    import yaml
    compose = yaml.safe_load((Path(__file__).resolve().parents[1]/'docker-compose.yml').read_text())
    assert compose['services']['mihomo']['image'] == preflight.MIHOMO_IMAGE + '@' + preflight.MIHOMO_SOURCE['index']
