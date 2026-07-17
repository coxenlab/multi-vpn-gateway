"""规则校验器 + 输出编码回归护栏(本周组②,Python 侧)。

两条防线各自成测:
  1. 注入拒绝(入口校验):恶意 pattern 经 _classify / _norm_domain 在入库前被挡,绝不落库。
  2. 输出编码(存量中和):即便脏数据已在库里(绕过 API 直插),各输出面靠 yaml/json 编码或
     二次校验中和它——不产生结构注入。
"""
import os
import sqlite3

import pytest
import yaml

import store


# 逗号/换行/双引号/空格 —— 四类会撕裂 mihomo/Clash 配置行的注入字符
MALICIOUS = ["evil.com,ch-x,no-resolve", "a\nb.com", 'a"b.com', "a b.com"]


# ── 1. 注入拒绝(入库前挡下) ─────────────────────────────────────────────

def test_rule_injection_rejected_autoclassify(make_channel, client):
    """自动识别路径(_classify):恶意 pattern 全进 rejected,一条都不落库。"""
    store.add_channel(make_channel("c1"))
    resp = client.post("/api/channels/c1/rules", json={"patterns": MALICIOUS})
    assert resp.status_code == 200
    body = resp.json()
    assert set(body["rejected"]) == set(MALICIOUS)
    assert body["added"] == {"domain": 0, "ip": 0}
    assert store.list_rules("c1") == []


def test_rule_injection_rejected_forced_domain(make_channel, client):
    """强制 kind=domain 路径(曾是最弱入口,只 strip 不校验):现也过 _norm_domain,全部 rejected。"""
    store.add_channel(make_channel("c1"))
    resp = client.post("/api/channels/c1/rules",
                       json={"patterns": MALICIOUS, "kind": "domain"})
    assert resp.status_code == 200
    body = resp.json()
    assert set(body["rejected"]) == set(MALICIOUS)
    assert store.list_rules("c1") == []


# ── 2. 导入校验(旁路曾可绕过入口校验) ───────────────────────────────────

def test_import_rejects_illegal_rules(client):
    """导入文档含一合法一非法规则:合法恢复、非法跳过(计入 skipped),不原样落库。"""
    doc = {
        "kind": "vpnmgr-export", "version": 1,
        "channels": [{
            "name": "导入客户", "vpn_type": "easyconnect", "server": "https://gw",
            "ec_ver": "7.6.3", "login_method": "interactive", "username": "u",
            "probe_url": "http://p", "config": {},
            "rules": [
                {"kind": "domain", "pattern": "good.example.com", "enabled": True},
                {"kind": "domain", "pattern": "bad.com,ch-x,no-resolve", "enabled": True},
            ],
        }],
    }
    resp = client.post("/api/config/import", json=doc)
    assert resp.status_code == 200
    body = resp.json()
    assert body["imported"] == ["导入客户"]

    chs = store.list_channels()
    assert len(chs) == 1
    pats = [r["pattern"] for r in store.list_rules(chs[0]["id"])]
    assert pats == ["good.example.com"]                 # 非法那条没落库
    assert any("bad.com" in s.get("reason", "") for s in body["skipped"])


# ── 3. 镜像源 host 校验 ──────────────────────────────────────────────────

def test_mirror_add_rejects_illegal_host(client):
    """POST /api/mirrors 非法 host(含空格+逗号,越出 [A-Za-z0-9._:/-])→ 400,不落库。"""
    bad = "reg.example.com, evil"
    resp = client.post("/api/mirrors", json={"host": bad})
    assert resp.status_code == 400
    assert bad not in [m["host"] for m in store.list_mirrors()]


# ── 4. 出口再校验(C):存量脏数据在所有输出面被跳过(不只是转义) ──────────
# 绕过 API 直插一条含逗号的脏 domain(模拟历史脏数据)。四个出口面(rebuild/provider/pac/
# snippet)都靠 rule_pattern_safe 二次校验跳过它——YAML/JSON 编码只防 YAML 结构注入,
# 挡不住 mihomo/Clash classical 规则行内逗号语法(victim.example,DIRECT 让第三字段变策略)。

DIRTY = "victim.example,DIRECT"


def _seed_dirty(make_channel):
    store.add_channel(make_channel("c1"))
    store.add_rule("c1", "domain", DIRTY)      # 绕过入口校验,模拟历史脏数据


def test_dirty_rule_rebuild_skipped(make_channel, monkeypatch):
    """rebuild:脏 pattern 过 rule_pattern_safe 被跳过,绝不写进 mihomo cfg["rules"]。
    ⚠️ 不用 client fixture(它把 rebuild 短路成 lambda:204);照 test_golden 只挡 requests.put。"""
    import manager
    monkeypatch.setattr(manager.requests, "put",
                        lambda *a, **k: type("R", (), {"status_code": 204})())
    _seed_dirty(make_channel)
    manager.rebuild()
    with open(os.environ["MIHOMO_CONFIG_PATH"]) as f:
        cfg = yaml.safe_load(f)
    assert not any("victim.example" in ln for ln in cfg["rules"])
    assert cfg["rules"] == ["MATCH,DIRECT"]     # 唯一(脏)规则被跳过 → 只剩兜底


def test_dirty_rule_provider_skipped(make_channel, client):
    """/clash/vpn-rules.yaml:脏 pattern 被跳过,payload 不含它。"""
    _seed_dirty(make_channel)
    parsed = yaml.safe_load(client.get("/clash/vpn-rules.yaml").text)
    assert parsed["payload"] == []             # 唯一脏规则被跳过 → 空 payload


def test_dirty_rule_pac_skipped(make_channel, client):
    """/entry/proxy.pac:脏 domain 被跳过,不进 DOMAINS 数组。"""
    _seed_dirty(make_channel)
    pac = client.get("/entry/proxy.pac").text
    assert "victim.example" not in pac
    assert "var DOMAINS = [];" in pac          # 唯一脏规则被跳过 → 空数组


def test_dirty_rule_snippet_skipped(make_channel, client):
    """/api/clash-snippet:脏 pattern 过 _norm_domain 二次校验被跳过,不进内联规则段。"""
    _seed_dirty(make_channel)
    snip = client.get("/api/clash-snippet").text
    assert "victim.example" not in snip
    assert "还没绑定任何规则" in snip           # 唯一(脏)规则被跳过 → 内联段为空占位


# ── 5. 域名规范化(A):放行 Unicode/下划线,拒 DANGER 字符(回归修复) ──────

def test_norm_domain_accepts_unicode_and_variants():
    """旧的 ASCII 白名单 [a-z0-9_.-] 误杀中文内网域名(回归);现放行 Unicode + 下划线。"""
    import main
    assert main._norm_domain("fp.内网") == "fp.内网"            # 中文内网域名不再被误杀
    assert main._norm_domain("_dmarc.x.com") == "_dmarc.x.com"  # 下划线放行
    assert main._norm_domain("Corp.COM") == "corp.com"          # ASCII 转小写
    assert main._norm_domain("*.y.com") == "y.com"              # 通配前缀剥除
    assert main._norm_domain("+.z.com") == "z.com"
    assert main._norm_domain("ÄBC.中国") == "äbc.中国"           # Unicode lowercase,非仅 ASCII
    assert main._norm_domain("example.ΟΣ") == "example.ος"       # 希腊词尾 sigma 上下文映射与 Rust 一致


def test_norm_domain_rejects_danger_and_nonstring():
    import main
    for bad in ["a,b.com", "a b.com", "a\nb.com", 'a"b.com', "a'b.com", "a\\b.com"]:
        assert main._norm_domain(bad) is None
    assert main._norm_domain(["x"]) is None                     # 非字符串(F)
    assert main._norm_domain(None) is None
    assert main._norm_domain("a\u00a0b.com") is None             # Unicode NBSP
    assert main._norm_domain("a\u2003b.com") is None             # Unicode em-space
    assert main._norm_domain(" Corp.COM ") is None               # 首尾空白也不被 trim 后放行


def test_fp_neiwang_stored_via_api(make_channel, client):
    """端到端:POST /rules 收 "fp.内网",落库原样存 "fp.内网"(命门②·A 回归修复)。"""
    store.add_channel(make_channel("c1"))
    resp = client.post("/api/channels/c1/rules", json={"patterns": ["fp.内网"]})
    assert resp.status_code == 200
    assert resp.json()["added"]["domain"] == 1
    assert "fp.内网" in [r["pattern"] for r in store.list_rules("c1")]


# ── 6. IP 规范化(B):拒 scope id / dotted mask / 越界前缀,裸 IP 补前缀 ─────

def test_norm_ip_accepts():
    import main
    assert main._norm_ip("10.0.0.0/8") == "10.0.0.0/8"
    assert main._norm_ip("10.0.0.5") == "10.0.0.5/32"          # 裸 v4 补 /32
    assert main._norm_ip("fd00::/8") == "fd00::/8"
    assert main._norm_ip("2001:db8::1") == "2001:db8::1/128"   # 裸 v6 补 /128


def test_norm_ip_rejects_scope_and_dotted_mask():
    import main
    assert main._norm_ip("2001:db8::1%eth0") is None                 # scope id
    assert main._norm_ip("2001:db8::1%eth0,MATCH,DIRECT") is None    # scope id 藏逗号注入
    assert main._norm_ip("10.0.0.1/255.255.255.0") is None          # dotted mask
    assert main._norm_ip("10.0.0.0/33") is None                     # 前缀越界(v4>32)
    assert main._norm_ip(["x"]) is None                             # 非字符串(F)


# ── 7. 导入类型健壮(F):非字符串 pattern 不 500,计入 skipped,通道其余正常 ──

def test_import_nonstring_pattern_no_500(client):
    doc = {
        "kind": "vpnmgr-export", "version": 1,
        "channels": [{
            "name": "健壮客户", "vpn_type": "easyconnect", "server": "https://gw",
            "ec_ver": "7.6.3", "login_method": "interactive", "username": "u",
            "probe_url": "http://p", "config": {},
            "rules": [
                {"kind": "domain", "pattern": ["x"], "enabled": True},       # 非字符串
                {"kind": "domain", "pattern": "ok.example.com", "enabled": True},
            ],
        }],
    }
    resp = client.post("/api/config/import", json=doc)
    assert resp.status_code == 200                     # 不 500
    body = resp.json()
    assert body["imported"] == ["健壮客户"]            # 通道其余正常落库
    assert len(body["skipped"]) >= 1                   # 非字符串那条计入 skipped
    chs = store.list_channels()
    assert [r["pattern"] for r in store.list_rules(chs[0]["id"])] == ["ok.example.com"]


# ── 8. 镜像源 host 校验(E):add 与 test 同拒 userinfo,test 非法不发请求 ──────

def test_mirror_add_rejects_userinfo_host(client):
    bad = "user@127.0.0.1:8443/admin"
    resp = client.post("/api/mirrors", json={"host": bad})
    assert resp.status_code == 400
    assert bad not in [m["host"] for m in store.list_mirrors()]


def test_mirror_test_rejects_illegal_host_no_request(client, monkeypatch):
    """POST /api/mirrors/test 非法 host → 400,且绝不发出请求(校验前置于 requests.get)。"""
    import main

    def _boom(*a, **k):
        raise AssertionError("非法 host 不该发出请求")

    monkeypatch.setattr(main.requests, "get", _boom)
    resp = client.post("/api/mirrors/test", json={"host": "user@127.0.0.1:8443/admin"})
    assert resp.status_code == 400


@pytest.mark.parametrize("host", ["mirror.example/path?query=1", "mirror.example/path#fragment"])
def test_mirror_add_and_test_reject_query_fragment(client, monkeypatch, host):
    import main

    monkeypatch.setattr(main.requests, "get",
                        lambda *a, **k: (_ for _ in ()).throw(AssertionError("must not request")))
    assert client.post("/api/mirrors", json={"host": host}).status_code == 400
    assert client.post("/api/mirrors/test", json={"host": host}).status_code == 400


def test_invalid_stored_rules_skipped_by_every_output(make_channel, monkeypatch):
    import main
    import manager
    from fastapi.testclient import TestClient

    store.add_channel(make_channel("c1"))
    store.add_rule("c1", "domain", "ÄBC.中国")
    store.add_rule("c1", "ip", "10.0.0.0/99")
    store.add_rule("c1", "domain", "bad..example")
    store.add_rule("c1", "unknown", "unknown.example")
    monkeypatch.setattr(manager.requests, "put",
                        lambda *a, **k: type("R", (), {"status_code": 204})())

    assert manager.rebuild() == 204
    with open(os.environ["MIHOMO_CONFIG_PATH"]) as f:
        mihomo_rules = yaml.safe_load(f)["rules"]
    client = TestClient(main.app)
    outputs = [
        yaml.safe_load(client.get("/clash/vpn-rules.yaml").text)["payload"],
        client.get("/api/clash-snippet").text,
        client.get("/entry/proxy.pac").text,
        mihomo_rules,
    ]
    for output in outputs:
        text = "\n".join(output) if isinstance(output, list) else output
        assert "10.0.0.0/99" not in text
        assert "bad..example" not in text
        assert "unknown.example" not in text
    assert "äbc.中国" in "\n".join(outputs[0])
    assert "äbc.中国" in outputs[1]
    assert "\\u00e4bc.\\u4e2d\\u56fd" in outputs[2]
    assert "äbc.中国" in "\n".join(outputs[3])


@pytest.mark.parametrize("rebuild_result", [500, "TimeoutError: reload failed", None])
def test_rule_mutations_fail_on_non_2xx_reload(make_channel, client, monkeypatch, rebuild_result):
    import main

    store.add_channel(make_channel("c1"))
    monkeypatch.setattr(main.manager, "rebuild", lambda: rebuild_result)
    assert client.post("/api/channels/c1/rules", json={"patterns": ["a.com"]}).status_code == 502
    rid = store.list_rules("c1")[0]["id"]
    assert client.patch(f"/api/channels/c1/rules/{rid}", json={"enabled": False}).status_code == 502
    assert client.delete(f"/api/channels/c1/rules/{rid}").status_code == 502


@pytest.mark.parametrize("method", ["add", "patch", "delete"])
def test_rule_mutations_propagate_db_errors(make_channel, client, monkeypatch, method):
    store.add_channel(make_channel("c1"))
    rid = store.add_rule("c1", "domain", "old.example")

    def fail(*_args, **_kwargs):
        raise sqlite3.OperationalError("forced failure")

    if method == "add":
        monkeypatch.setattr(store, "add_rule", fail)
        response = client.post("/api/channels/c1/rules", json={"patterns": ["new.example"]})
    elif method == "patch":
        monkeypatch.setattr(store, "set_rule_enabled", fail)
        response = client.patch(f"/api/channels/c1/rules/{rid}", json={"enabled": False})
    else:
        monkeypatch.setattr(store, "del_rule", fail)
        response = client.delete(f"/api/channels/c1/rules/{rid}")
    assert response.status_code == 500


def test_rule_patch_delete_enforce_channel_ownership(make_channel, client):
    store.add_channel(make_channel("c1"))
    store.add_channel(make_channel("c2"))
    rid = store.add_rule("c1", "domain", "owned.example")

    response = client.patch(f"/api/channels/c2/rules/{rid}", json={"enabled": False})
    assert response.status_code == 404
    assert store.get_rule(rid)["enabled"] == 1
    assert client.delete(f"/api/channels/c2/rules/{rid}").status_code == 404
    assert store.get_rule(rid) is not None


def test_import_reports_malformed_nested_values_and_rules(client):
    assert client.post("/api/config/import", json=[]).status_code == 400
    doc = {
        "kind": "vpnmgr-export", "version": 1,
        "channels": [
            {"name": "bad-config", "vpn_type": "easyconnect", "config": [],
             "rules": [{"kind": "mystery", "pattern": "x.example"}]},
            {"name": "good", "vpn_type": "easyconnect", "config": {},
             "rules": [42, {"kind": "ip", "pattern": "10.0.0.0/99"},
                       {"kind": "domain", "pattern": ["bad"]},
                       {"kind": "domain", "pattern": "bad-enabled.example", "enabled": []},
                       {"kind": "domain", "pattern": "OK.中国", "enabled": True}]},
        ],
    }
    response = client.post("/api/config/import", json=doc)
    assert response.status_code == 200
    body = response.json()
    assert body["imported"] == ["good"]
    assert len(body["skipped"]) >= 5
    channel = store.list_channels()[0]
    assert [r["pattern"] for r in store.list_rules(channel["id"])] == ["ok.中国"]


def test_import_write_failure_rolls_back_everything(client, monkeypatch):
    original = store._insert_rule

    def fail_second(connection, cid, kind, pattern):
        if pattern == "fail.example":
            raise sqlite3.OperationalError("forced insert failure")
        return original(connection, cid, kind, pattern)

    monkeypatch.setattr(store, "_insert_rule", fail_second)
    doc = {"kind": "vpnmgr-export", "version": 1, "channels": [
        {"name": "first", "vpn_type": "easyconnect", "config": {},
         "rules": [{"kind": "domain", "pattern": "ok.example"}]},
        {"name": "second", "vpn_type": "easyconnect", "config": {},
         "rules": [{"kind": "domain", "pattern": "fail.example"}]},
    ]}
    assert client.post("/api/config/import", json=doc).status_code == 500
    assert store.list_channels() == []
    assert store.all_rules() == []


def test_import_enabled_update_failure_rolls_back(client, monkeypatch):
    monkeypatch.setattr(store, "_set_rule_enabled", lambda *_args, **_kwargs: 0)
    doc = {"kind": "vpnmgr-export", "version": 1, "channels": [
        {"name": "disabled", "vpn_type": "easyconnect", "config": {},
         "rules": [{"kind": "domain", "pattern": "off.example", "enabled": False}]},
    ]}
    assert client.post("/api/config/import", json=doc).status_code == 500
    assert store.list_channels() == []
    assert store.all_rules() == []


def test_rebuild_replaces_config_with_mode_0600(make_channel, monkeypatch):
    import manager

    store.add_channel(make_channel("c1"))
    with open(manager.CFG, "w") as f:
        f.write("rules: []\n")
    os.chmod(manager.CFG, 0o644)
    monkeypatch.setattr(manager.requests, "put",
                        lambda *a, **k: type("R", (), {"status_code": 204})())
    assert manager.rebuild() == 204
    assert os.stat(manager.CFG).st_mode & 0o777 == 0o600
