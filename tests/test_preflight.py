import docker
import preflight


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
        return type("Img", (), {"attrs": {"Architecture": self._store[name]}})()


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
    def run(self, image, **kw):
        if self._raise:
            raise self._raise
        if not self._tun_ok:
            raise docker.errors.ContainerError(image, 1, kw.get("entrypoint"), image, b"")
        return b""        # detach=False 成功返回 logs(bytes)


class _FakeDc:
    def __init__(self, ping=True, images=None, networks_ok=True, df=None, kw_tun=True, kw_raise=None):
        self._ping = ping
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
    monkeypatch.setattr(preflight, "_pull_worker", lambda *a, **k: None)  # 不真跑线程体
    tid = preflight.start_pull(object(), "hagb/docker-atrust:latest", "arm64",
                               mirrors=["docker.1ms.run"])
    assert preflight.get_task(tid)["status"] == "running"
    assert preflight.get_task("nope") is None


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
    assert "metacubex/mihomo:latest" in imgs
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
    assert by["metacubex/mihomo:latest"]["role"] == "infra"
    app = by["app"]
    assert app["kind"] == "compose"
    assert app["present"] is None and app["versions"] == []


def test_inventory_present_false_when_missing(monkeypatch):
    _, by = _inv(monkeypatch, images={})
    assert by["hagb/docker-atrust:latest"]["present"] is False
    assert by["vpnmgr/oss-vpn:latest"]["present"] is False
