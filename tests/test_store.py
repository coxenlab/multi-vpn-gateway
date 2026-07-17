import store


def test_add_and_list_rules(make_channel):
    store.add_channel(make_channel("c1"))
    rid = store.add_rule("c1", "domain", "a.com")
    rules = store.list_rules("c1")
    assert len(rules) == 1
    assert rules[0]["id"] == rid
    assert rules[0]["kind"] == "domain"
    assert rules[0]["pattern"] == "a.com"
    assert rules[0]["enabled"] == 1


def test_set_rule_enabled(make_channel):
    store.add_channel(make_channel("c1"))
    rid = store.add_rule("c1", "ip", "10.0.0.0/8")
    store.set_rule_enabled("c1", rid, False)
    assert store.get_rule(rid)["enabled"] == 0
    store.set_rule_enabled("c1", rid, True)
    assert store.get_rule(rid)["enabled"] == 1


def test_del_rule(make_channel):
    store.add_channel(make_channel("c1"))
    rid = store.add_rule("c1", "domain", "a.com")
    store.del_rule("c1", rid)
    assert store.list_rules("c1") == []


def test_del_channel_cascades_rules(make_channel):
    store.add_channel(make_channel("c1"))
    store.add_rule("c1", "domain", "a.com")
    store.del_channel("c1")
    assert store.all_rules() == []


def test_set_latency(make_channel):
    store.add_channel(make_channel("c1"))
    store.set_latency("c1", 55)
    assert store.get_channel("c1")["latency_ms"] == 55


def test_password_encrypted_not_returned(make_channel):
    store.add_channel(make_channel("c1", password="secret"))
    ch = store.get_channel("c1")
    assert "password_enc" not in ch
    assert store.get_password("c1") == "secret"


def test_migrate_domains_to_rules():
    with store._c() as c:
        c.execute("DELETE FROM rules")
        c.execute("INSERT INTO domains(channel_id,pattern) VALUES('c1','legacy.com')")
    store.init()  # 迁移触发条件:rules 空 & domains 有行
    migrated = [r for r in store.all_rules() if r["pattern"] == "legacy.com"]
    assert len(migrated) == 1
    assert migrated[0]["kind"] == "domain" and migrated[0]["enabled"] == 1


def test_config_json_encrypts_secret_fields_and_row_strips(make_channel):
    import store
    ch = make_channel("oss1")
    ch["vpn_type"] = "anyconnect"
    # secret_keys 标记哪些字段加密;非 secret 明文存(供展示)
    store.add_channel(ch, config={"server": "https://gw", "username": "alice",
                                   "password": "s3cret"},
                      secret_keys=["password"])
    # 落库的密文里不含明文密码
    raw = store.get_config_raw("oss1")
    assert "s3cret" not in raw
    # 解密回明文供容器注入
    cfg = store.get_config("oss1")
    assert cfg == {"server": "https://gw", "username": "alice", "password": "s3cret"}
    # 回前端的 row 剥除 secret 字段,保留非 secret
    row = store.get_channel("oss1")
    assert row["config"]["server"] == "https://gw"
    assert row["config"]["username"] == "alice"
    assert "password" not in row["config"]


def test_config_json_absent_is_empty(make_channel):
    import store
    store.add_channel(make_channel("ec1"))   # 老 hagb 路径不传 config
    assert store.get_config("ec1") == {}
    assert store.get_channel("ec1")["config"] == {}


def test_set_config_field_merges_nonsecret_and_visible_to_frontend(make_channel):
    import store
    ch = make_channel("byo1")
    ch["vpn_type"] = "custom"
    store.add_channel(ch)                       # byo 起容器时无 config
    # 上传后写入非密文件名引用(命门 #5:只存文件名,不存二进制)
    store.set_config_field("byo1", "package", "client-installer.run", secret=False)
    # 回前端的 row 含文件名(非密,允许展示已装包名)
    row = store.get_channel("byo1")
    assert row["config"]["package"] == "client-installer.run"
    # get_config 也能读回(round-trip)
    assert store.get_config("byo1")["package"] == "client-installer.run"
    # 落库 config_json 里只有文件名字符串,绝无二进制痕迹
    raw = store.get_config_raw("byo1")
    assert "client-installer.run" in raw


def test_set_config_field_secret_stripped_from_frontend(make_channel):
    import store
    ch = make_channel("byo2")
    ch["vpn_type"] = "custom"
    store.add_channel(ch)
    store.set_config_field("byo2", "token", "T0PSECRET", secret=True)
    # secret=True 字段:落库密文不含明文,回前端 row 剥除,get_config 解密可读
    assert "T0PSECRET" not in store.get_config_raw("byo2")
    assert "token" not in store.get_channel("byo2")["config"]
    assert store.get_config("byo2")["token"] == "T0PSECRET"
