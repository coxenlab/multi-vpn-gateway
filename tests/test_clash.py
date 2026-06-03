import yaml


def _seed(client):
    b = {"name": "X", "vpn_type": "easyconnect", "server": "https://x",
         "ec_ver": "7.6.3", "login_method": "interactive", "probe_url": "http://p"}
    cid = client.post("/api/channels", json=b).json()["id"]
    client.post(f"/api/channels/{cid}/rules",
                json={"patterns": ["+.weidu-crm.com", "10.20.0.0/16"]})
    return cid


def test_clash_provider_payload(client):
    _seed(client)
    r = client.get("/clash/vpn-rules.yaml")
    assert r.status_code == 200
    cfg = yaml.safe_load(r.text)
    assert "DOMAIN-SUFFIX,weidu-crm.com" in cfg["payload"]   # 去掉 +. 前缀
    assert "IP-CIDR,10.20.0.0/16" in cfg["payload"]


def test_clash_provider_skips_disabled(client):
    cid = _seed(client)
    rid = client.get("/api/channels").json()[0]["domains"][0]["id"]
    client.patch(f"/api/channels/{cid}/rules/{rid}", json={"enabled": False})
    cfg = yaml.safe_load(client.get("/clash/vpn-rules.yaml").text)
    assert "DOMAIN-SUFFIX,weidu-crm.com" not in cfg["payload"]


def test_clash_snippet_has_node_ip_and_provider(client):
    _seed(client)
    txt = client.get("/api/clash-snippet").text
    assert "vpn-router" in txt
    assert "IP-CIDR,10.20.0.0/16,vpn-router,no-resolve" in txt
    assert "rule-providers" in txt and "vpn-rules.yaml" in txt
