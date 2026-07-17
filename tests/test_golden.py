"""双栈 golden 契约回归护栏(本周组⑤):钉死 rebuild 的 mihomo rules 排序 + provider payload
顺序/格式。fixture(tests/fixtures/golden_rules.json)是 Python/Rust 两栈共同消费的只读契约,
本文件按其 rules_insertion_order 播种、逐项精确比对 expected_*。契约不符 = 断言写错,不改 fixture。"""
import json
import os

import yaml

import store

_FIXTURE = os.path.join(os.path.dirname(__file__), "fixtures", "golden_rules.json")


def _load_golden():
    with open(_FIXTURE, encoding="utf-8") as f:
        return json.load(f)


def _seed_from_golden(make_channel, g):
    """按 fixture 建通道(第1个=ch0、第2个=ch1)并按插入序 add_rule;返回 key→真实 id 映射。
    通道真实 id 故意异于 fixture key(a1b2.../e5f6...),以真正验证占位替换而非硬编码。"""
    idmap = {}
    real_ids = ["a1b2c3d4", "e5f6a7b8"]
    for i, cdef in enumerate(g["channels"]):
        cid = real_ids[i]
        store.add_channel(make_channel(cid, name=cdef["name"]))
        idmap[cdef["key"]] = cid
    for r in g["rules_insertion_order"]:
        cid = idmap[r["channel"]]
        rid = store.add_rule(cid, r["kind"], r["pattern"])
        if not r["enabled"]:                      # disabled 也入库,验证输出面把它排除
            store.set_rule_enabled(cid, rid, False)
    return idmap


def _subst(s, idmap):
    """把 expected_* 里的 {ch0}/{ch1} 占位替换成真实通道 id。"""
    for key, cid in idmap.items():
        s = s.replace("{" + key + "}", cid)
    return s


def test_mihomo_rules_golden(make_channel, monkeypatch):
    """rebuild 产出的 cfg["rules"] 逐项等于 golden expected_mihomo_rules(占位替换后)。
    ⚠️ 不用 client fixture:它把 manager.rebuild monkeypatch 成 lambda:204 会短路真实渲染;
    这里照 test_manager 模板只挡网络 requests.put。"""
    import manager
    monkeypatch.setattr(manager.requests, "put",
                        lambda *a, **k: type("R", (), {"status_code": 204})())
    g = _load_golden()
    idmap = _seed_from_golden(make_channel, g)

    code = manager.rebuild()
    assert code == 204
    with open(os.environ["MIHOMO_CONFIG_PATH"]) as f:
        cfg = yaml.safe_load(f)

    expected = [_subst(x, idmap) for x in g["expected_mihomo_rules"]]
    assert cfg["rules"] == expected          # 逐项精确相等(顺序含义:domain 序 + ip 前缀降序 + MATCH 末)


def test_provider_payload_golden(make_channel, client):
    """GET /clash/vpn-rules.yaml 的 payload 逐项等于 golden expected_provider_payload。
    provider 端点直读 store.all_rules、不经 rebuild,故可走 client fixture 的 HTTP 路径。"""
    g = _load_golden()
    _seed_from_golden(make_channel, g)

    resp = client.get("/clash/vpn-rules.yaml")
    assert resp.status_code == 200
    parsed = yaml.safe_load(resp.text)
    # provider payload 不带 channel id(占位无需替换),语义(解析后列表)逐项相等,不比字节
    assert parsed["payload"] == g["expected_provider_payload"]
