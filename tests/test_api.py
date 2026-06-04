def _create(client, **over):
    body = {"name": "X", "vpn_type": "easyconnect", "server": "https://x",
            "ec_ver": "7.6.3", "login_method": "interactive", "probe_url": "http://p"}
    body.update(over)
    return client.post("/api/channels", json=body).json()


def test_create_channel(client):
    ch = _create(client, name="维度")
    assert ch["name"] == "维度"
    assert ch["status"] == "running"
    assert "password_enc" not in ch


def test_channels_enriched(client):
    cid = _create(client)["id"]
    client.post(f"/api/channels/{cid}/rules", json={"patterns": ["a.com", "10.0.0.0/8"]})
    ch = client.get("/api/channels").json()[0]
    assert ch["volume_name"] == f"vpndata-{cid}"
    assert ch["socks_proxy"] == f"ch-{cid}"
    assert ch["socks_endpoint"] == f"vpn-{cid}:1080"
    assert any(d["pattern"] == "a.com" for d in ch["domains"])
    assert any(i["pattern"] == "10.0.0.0/8" for i in ch["ips"])


def test_add_rules_autodetect_and_normalize(client):
    cid = _create(client)["id"]
    j = client.post(f"/api/channels/{cid}/rules",
                    json={"patterns": ["a.com", "10.0.0.5", "10.20.0.0/16"]}).json()
    assert j["added"] == {"domain": 1, "ip": 2}
    assert any(i["pattern"] == "10.0.0.5/32" for i in j["ips"])
    assert any(i["pattern"] == "10.20.0.0/16" for i in j["ips"])


def test_add_rules_forced_ip_rejects_nonip(client):
    cid = _create(client)["id"]
    j = client.post(f"/api/channels/{cid}/rules",
                    json={"patterns": ["nope.com"], "kind": "ip"}).json()
    assert "nope.com" in j["rejected"]
    assert j["added"] == {"domain": 0, "ip": 0}


def test_toggle_and_delete_rule(client):
    cid = _create(client)["id"]
    client.post(f"/api/channels/{cid}/rules", json={"patterns": ["a.com"]})
    rid = client.get("/api/channels").json()[0]["domains"][0]["id"]
    assert client.patch(f"/api/channels/{cid}/rules/{rid}", json={"enabled": False}).json()["ok"]
    assert client.get("/api/channels").json()[0]["domains"][0]["enabled"] == 0
    assert client.delete(f"/api/channels/{cid}/rules/{rid}").json()["ok"]
    assert client.get("/api/channels").json()[0]["domains"] == []


def test_system(client):
    j = client.get("/api/system").json()
    assert j["mihomo_status"] == "running"
    assert j["mihomo_port"] == 48721
    assert j["bound_ip"] == "127.0.0.1"


def test_status_latency(client):
    cid = _create(client)["id"]
    j = client.get(f"/api/channels/{cid}/status").json()
    assert j["connected"] is True and j["latency_ms"] == 42 and j["status"] == "logged_in"


def test_login_url(client):
    cid = _create(client)["id"]
    j = client.get(f"/api/channels/{cid}/login").json()
    assert "/vnc.html" in j["url"] and "127.0.0.1:18080" in j["url"]


def test_logs(client):
    cid = _create(client)["id"]
    assert client.get(f"/api/channels/{cid}/logs").json()["lines"] == ["line1", "line2"]


def test_connections(client):
    assert "connections" in client.get("/api/connections").json()


def test_vpn_types_lists_adapters(client):
    r = client.get("/api/vpn-types")
    assert r.status_code == 200
    data = r.json()
    by = {a["key"]: a for a in data}
    assert {"easyconnect", "atrust"} <= set(by)
    assert by["easyconnect"]["versioned"] is True
    assert by["atrust"]["versioned"] is False
    assert [i["key"] for i in by["easyconnect"]["inputs"]] == ["server", "username", "password"]


def test_versions_endpoint_for_easyconnect(client, monkeypatch):
    import dockerhub
    monkeypatch.setattr(
        dockerhub, "versions",
        lambda repo, host_arch, fallback: [
            {"tag": "7.6.7", "arch": ["amd64", "arm64"], "usable_here": True},
            {"tag": "7.6.3", "arch": ["amd64", "arm64"], "usable_here": True}])
    r = client.get("/api/vpn-types/easyconnect/versions")
    assert r.status_code == 200
    assert [v["tag"] for v in r.json()["versions"]] == ["7.6.7", "7.6.3"]


def test_versions_endpoint_atrust_empty(client):
    r = client.get("/api/vpn-types/atrust/versions")
    assert r.status_code == 200
    assert r.json()["versions"] == []   # versioned:false → 不拉


def test_versions_unknown_type_404(client):
    r = client.get("/api/vpn-types/nope/versions")
    assert r.status_code == 404


def test_create_oss_channel_encrypts_config(client):
    r = client.post("/api/channels", json={
        "name": "客户X", "vpn_type": "anyconnect",
        "login_method": "headless", "probe_url": "http://p",
        "config": {"server": "https://gw", "username": "alice", "password": "s3cret"},
    })
    assert r.status_code == 200
    body = r.json()
    # 回前端的 config 剥除 secret(命门 #5)
    assert body["config"]["server"] == "https://gw"
    assert "password" not in body["config"]
    # 落库密文不含明文
    import store
    assert "s3cret" not in store.get_config_raw(body["id"])
    assert store.get_config(body["id"])["password"] == "s3cret"


def test_oss_types_listed_in_vpn_types(client):
    by = {a["key"]: a for a in client.get("/api/vpn-types").json()}
    assert {"anyconnect", "openvpn", "wireguard", "openfortivpn"} <= set(by)
    assert by["anyconnect"]["login_modes"] == ["headless"]
    assert by["anyconnect"]["versioned"] is False


def test_login_headless_returns_mode(client, monkeypatch):
    import store
    r = client.post("/api/channels", json={
        "name": "客户X", "vpn_type": "anyconnect", "login_method": "headless",
        "probe_url": "http://p",
        "config": {"server": "https://gw", "username": "a", "password": "p"}})
    cid = r.json()["id"]
    # headless 无 noVNC:login 端点返回 {login_mode:"headless"},前端据此跳过 VNC 屏
    lr = client.get(f"/api/channels/{cid}/login")
    assert lr.status_code == 200
    assert lr.json() == {"login_mode": "headless"}


def test_byo_login_returns_novnc_url_not_headless(client):
    # byo 走 gui/noVNC 路径:login 返回 {url}(同 EC/aTrust),不返回 login_mode:headless
    r = client.post("/api/channels", json={
        "name": "兜底X", "vpn_type": "custom", "login_method": "byo",
        "probe_url": "http://p"})
    assert r.status_code == 200
    cid = r.json()["id"]
    lr = client.get(f"/api/channels/{cid}/login").json()
    assert "login_mode" not in lr
    assert "/vnc.html" in lr["url"] and "127.0.0.1:18080" in lr["url"]


def test_upload_puts_file_and_stores_ref_not_bytes(client, monkeypatch):
    import manager, store
    puts = {}
    monkeypatch.setattr(manager, "put_file",
                        lambda cid, d, name, blob: puts.update(
                            cid=cid, dest=d, name=name, blob=blob) or f"{d}/{name}")

    r = client.post("/api/channels", json={
        "name": "兜底X", "vpn_type": "custom", "login_method": "byo",
        "probe_url": "http://p"})
    cid = r.json()["id"]

    blob = b"\x7fELF\x00binary\xff"
    up = client.post(f"/api/channels/{cid}/upload",
                     files={"file": ("client.run", blob, "application/octet-stream")})
    assert up.status_code == 200
    # 二进制原样落到容器 /root,经 put_file(blob 是 bytes,绝不读成文本)
    assert puts["cid"] == cid and puts["name"] == "client.run" and puts["blob"] == blob
    # config_json 只存非密文件名引用;响应/get_channel 都不含二进制
    assert store.get_channel(cid)["config"]["package"] == "client.run"
    body = up.json()
    assert "package" in body or body.get("ok") is True
    assert "\x7fELF" not in (up.text)        # 命门 #5:二进制绝不回传前端
    assert b"\x7fELF" not in up.content


def test_preflight_endpoint_returns_aggregate(client, monkeypatch):
    import preflight
    monkeypatch.setattr(preflight, "run_checks",
                        lambda dc, vt, ver, **k: {"host_arch": "arm64",
                                                  "target_image": "hagb/docker-atrust:latest",
                                                  "overall": "fail", "checks": []})
    r = client.get("/api/preflight?vpn_type=atrust")
    assert r.status_code == 200
    b = r.json()
    assert b["overall"] == "fail"
    assert b["target_image"] == "hagb/docker-atrust:latest"

def test_preflight_passes_version_through(client, monkeypatch):
    import preflight
    seen = {}
    monkeypatch.setattr(preflight, "run_checks",
                        lambda dc, vt, ver, **k: seen.update(vt=vt, ver=ver) or
                        {"host_arch": "arm64", "target_image": None, "overall": "pass", "checks": []})
    client.get("/api/preflight?vpn_type=easyconnect&version=7.6.7")
    assert seen == {"vt": "easyconnect", "ver": "7.6.7"}


def test_fix_create_network_idempotent(client, monkeypatch):
    import main, docker
    created = {}
    class Nets:
        def get(self, n): raise docker.errors.NotFound(n)
        def create(self, n, driver=None): created.update(name=n, driver=driver)
    monkeypatch.setattr(main.manager, "dc", type("D", (), {"networks": Nets()})())
    r = client.post("/api/preflight/fix/create_network", json={"name": "vpnnet"})
    assert r.status_code == 200 and r.json()["ok"] is True
    assert created == {"name": "vpnnet", "driver": "bridge"}

def test_fix_pull_image_rejects_unknown_image(client):
    r = client.post("/api/preflight/fix/pull_image", json={"image": "evil/x:latest"})
    assert r.status_code == 400

def test_fix_pull_image_starts_task(client, monkeypatch):
    import preflight
    monkeypatch.setattr(preflight, "start_pull", lambda dc, img, arch, **k: "task42")
    r = client.post("/api/preflight/fix/pull_image",
                    json={"image": "hagb/docker-atrust:latest"})
    assert r.status_code == 200 and r.json()["task_id"] == "task42"

def test_fix_status_returns_task(client, monkeypatch):
    import preflight
    monkeypatch.setattr(preflight, "get_task",
                        lambda t: {"status": "done", "progress": "ok", "log_tail": [], "error": None})
    r = client.get("/api/preflight/fix/task42")
    assert r.status_code == 200 and r.json()["status"] == "done"

def test_fix_status_unknown_404(client, monkeypatch):
    import preflight
    monkeypatch.setattr(preflight, "get_task", lambda t: None)
    assert client.get("/api/preflight/fix/nope").status_code == 404


def test_mirrors_crud_api(client):
    r = client.get("/api/mirrors"); assert r.status_code == 200
    base = len(r.json())
    r = client.post("/api/mirrors", json={"host": "docker.added.com"})
    assert r.status_code == 200 and r.json()["host"] == "docker.added.com"
    mid = r.json()["id"]
    assert len(client.get("/api/mirrors").json()) == base + 1
    assert client.patch(f"/api/mirrors/{mid}", json={"enabled": False}).status_code == 200
    assert [m for m in client.get("/api/mirrors").json() if m["id"] == mid][0]["enabled"] == 0
    assert client.delete(f"/api/mirrors/{mid}").status_code == 200
    assert len(client.get("/api/mirrors").json()) == base


def test_mirror_test_endpoint(client, monkeypatch):
    import main
    monkeypatch.setattr(main.requests, "get",
                        lambda url, timeout=5: type("R", (), {"status_code": 200})())
    r = client.post("/api/mirrors/test", json={"host": "docker.1ms.run"})
    assert r.status_code == 200
    b = r.json()
    assert b["reachable"] is True and "latency_ms" in b


def test_pull_image_uses_db_mirrors(client, monkeypatch):
    import preflight, store
    seen = {}
    monkeypatch.setattr(preflight, "start_pull",
                        lambda dc, img, arch, mirrors=None: seen.update(mirrors=mirrors) or "t1")
    store.add_mirror("docker.custom.com")
    client.post("/api/preflight/fix/pull_image", json={"image": "hagb/docker-atrust:latest"})
    assert "docker.custom.com" in seen["mirrors"]


def test_preflight_full_scope_passes_mirrors_and_mihomo(client, monkeypatch):
    import preflight, main
    seen = {}
    monkeypatch.setattr(preflight, "run_checks",
                        lambda dc, vt, ver, **k: seen.update(k) or
                        {"host_arch": "arm64", "target_image": None, "overall": "pass", "checks": []})
    monkeypatch.setattr(main.manager, "mihomo_alive", lambda: True)
    client.get("/api/preflight?vpn_type=atrust&scope=full")
    assert seen.get("scope") == "full"
    assert isinstance(seen.get("mirrors"), list)


def test_add_duplicate_mirror_returns_400(client):
    assert client.post("/api/mirrors", json={"host": "docker.dup.com"}).status_code == 200
    assert client.post("/api/mirrors", json={"host": "docker.dup.com"}).status_code == 400


def test_preflight_default_scope_skips_mihomo_probe(client, monkeypatch):
    import main, preflight
    calls = {"n": 0}
    monkeypatch.setattr(main.manager, "mihomo_alive",
                        lambda: calls.__setitem__("n", calls["n"] + 1) or True)
    monkeypatch.setattr(preflight, "run_checks", lambda dc, vt, ver, **k:
                        {"host_arch": "arm64", "target_image": None, "overall": "pass", "checks": []})
    client.get("/api/preflight?vpn_type=atrust")          # 默认 scope=preflight
    assert calls["n"] == 0                                 # 向导 gate 不探 mihomo
    client.get("/api/preflight?vpn_type=atrust&scope=full")
    assert calls["n"] == 1                                 # full 才探


def test_start_recreates_not_inplace_docker_start(client, monkeypatch):
    """命门:EC/aTrust(hagb) 与 oss 扛不住原地 docker start(守护进程/exec 注入的隧道不重启)。
    /start 必须重建容器(docker run fresh),而非 container.start()。"""
    import manager, store
    cid = _create(client)["id"]                          # create_channel mocked → ("cid_fake", 18080)
    client.post(f"/api/channels/{cid}/stop")
    assert store.get_channel(cid)["status"] == "stopped"

    monkeypatch.setattr(manager, "create_channel", lambda c, vnc: ("fresh-cid", 29999))
    r = client.post(f"/api/channels/{cid}/start")
    assert r.status_code == 200
    row = store.get_channel(cid)
    assert row["container_id"] == "fresh-cid"             # 走了重建(docker run fresh),落全新容器
    assert row["novnc_port"] == 29999                     # 原地 docker start 不会换容器/端口
    assert row["status"] == "running"
