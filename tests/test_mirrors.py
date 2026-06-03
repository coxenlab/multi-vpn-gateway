import store


def test_mirrors_seeded_defaults():
    ms = store.list_mirrors()
    hosts = [m["host"] for m in ms]
    assert "docker.1ms.run" in hosts and "hub.rat.dev" in hosts
    assert [m["priority"] for m in ms] == sorted(m["priority"] for m in ms)


def test_add_list_del_mirror():
    mid = store.add_mirror("docker.example.com")
    ms = {m["host"]: m for m in store.list_mirrors()}
    assert "docker.example.com" in ms and ms["docker.example.com"]["enabled"] == 1
    store.del_mirror(mid)
    assert "docker.example.com" not in [m["host"] for m in store.list_mirrors()]


def test_set_mirror_enabled_and_priority():
    mid = store.add_mirror("docker.x.com")
    store.set_mirror(mid, enabled=False)
    m = [x for x in store.list_mirrors() if x["id"] == mid][0]
    assert m["enabled"] == 0
    store.set_mirror(mid, priority=0)
    m = [x for x in store.list_mirrors() if x["id"] == mid][0]
    assert m["priority"] == 0
    store.del_mirror(mid)
