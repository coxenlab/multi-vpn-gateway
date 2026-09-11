import os
import yaml
import store


def test_rebuild_emits_domain_and_ip_rules(make_channel, monkeypatch, mihomo_controller):
    import manager
    store.add_channel(make_channel("c1"))
    store.add_rule("c1", "domain", "weidu-crm.com")
    store.add_rule("c1", "ip", "10.20.0.0/16")
    code = manager.rebuild()
    assert code == 204
    with open(os.environ["MIHOMO_CONFIG_PATH"]) as f:
        cfg = yaml.safe_load(f)
    names = [p["name"] for p in cfg["proxies"]]
    assert "ch-c1" in names
    assert "DOMAIN-SUFFIX,weidu-crm.com,ch-c1" in cfg["rules"]
    assert "IP-CIDR,10.20.0.0/16,ch-c1,no-resolve" in cfg["rules"]
    assert cfg["rules"][-1] == "MATCH,DIRECT"


def test_rebuild_skips_disabled(make_channel, monkeypatch, mihomo_controller):
    import manager
    store.add_channel(make_channel("c1"))
    rid = store.add_rule("c1", "domain", "off.com")
    store.set_rule_enabled("c1", rid, False)
    manager.rebuild()
    with open(os.environ["MIHOMO_CONFIG_PATH"]) as f:
        cfg = yaml.safe_load(f)
    assert not any("off.com" in r for r in cfg["rules"])


def test_probe_returns_latency(monkeypatch):
    import manager
    monkeypatch.setattr(manager.requests, "get",
                        lambda *a, **k: type("R", (), {"status_code": 200})())
    ok, ms = manager.probe({"id": "c1", "probe_url": "http://p"})
    assert ok is True and isinstance(ms, int) and ms >= 0


def test_probe_no_url():
    import manager
    ok, ms = manager.probe({"id": "c1", "probe_url": ""})
    assert ok is False and ms is None


def test_dns_recovery_still_requires_real_probe(monkeypatch):
    import manager
    attempts = iter([(False, None), (False, None)])
    monkeypatch.setattr(manager, "_probe_once", lambda ch: next(attempts))
    monkeypatch.setattr(manager, "_recover_hagb_dns", lambda ch: True)
    assert manager.probe({"id": "c1"}) == (False, None)


def test_healthy_probe_does_not_restart_dns_proxy(monkeypatch):
    import manager
    monkeypatch.setattr(manager, "_probe_once", lambda ch: (True, 12))
    def forbidden(ch):
        raise AssertionError("healthy probe must not attempt recovery")
    monkeypatch.setattr(manager, "_recover_hagb_dns", forbidden)
    assert manager.probe({"id": "c1"}) == (True, 12)


def test_dns_recovery_retries_original_probe(monkeypatch):
    import manager
    attempts = iter([(False, None), (True, 42)])
    monkeypatch.setattr(manager, "_probe_once", lambda ch: next(attempts))
    monkeypatch.setattr(manager, "_recover_hagb_dns", lambda ch: True)
    assert manager.probe({"id": "c1"}) == (True, 42)


def test_channel_plan_uses_adapter_kwargs_and_gui_initialization(monkeypatch):
    import manager, adapters, registry
    execs = []
    class FakeContainer:
        def exec_run(self, cmd, **kw): execs.append((cmd, kw))
    ch = {"id": "abc123", "vpn_type": "easyconnect", "ec_ver": "7.6.3", "mac": "02:00:00:00:00:01"}
    plan = manager.channel_plan(ch, "vncpw01")
    assert plan == adapters.build_run_kwargs(ch, registry.get("easyconnect"), "vncpw01", manager.VPN_NET)
    manager.initialize_gui(FakeContainer(), registry.get("easyconnect"))
    assert any("gai.conf" in " ".join(cmd) for cmd, _kw in execs)


def test_oss_plan_and_connect_keep_password_out_of_argv(monkeypatch):
    import manager, adapters, registry
    execs = []
    class FakeContainer:
        def exec_run(self, cmd, **kw):
            execs.append({"cmd": cmd, "kw": kw}); return (0, b"")
    monkeypatch.setattr(manager.socket, "gethostbyname", lambda h: "10.0.0.9")
    ch = {"id": "oss1", "vpn_type": "anyconnect", "ec_ver": "", "mac": ""}
    spec = registry.get("anyconnect")
    plan = manager.channel_plan(ch, "vncpw01")
    expected = adapters.build_run_kwargs(ch, spec, "vncpw01", manager.VPN_NET)
    expected["dns"] = ["10.0.0.9"]
    assert plan == expected
    manager.oss_connect(FakeContainer(), spec, {"server":"https://gw", "username":"alice", "password":"s3cret"})
    assert execs, "expected an exec_run connect call"
    joined = " ".join(map(str, execs[0]["cmd"]))
    assert "s3cret" not in joined
    assert execs[0]["kw"].get("stdin") is True
    assert "alice" in joined or "anyconnect" in joined


def test_oss_wireguard_config_not_in_argv():
    import manager, registry
    execs = []
    class FakeContainer:
        def exec_run(self, cmd, **kw):
            execs.append({"cmd": cmd, "kw": kw}); return (0, b"")
    secret = "[Interface]\nPrivateKey=TOPSECRETKEY=\n"
    manager.oss_connect(FakeContainer(), registry.get("wireguard"), {"config_file":secret})
    for e in execs:
        assert "TOPSECRETKEY" not in " ".join(map(str, e["cmd"]))
    joined_all = " ".join(" ".join(map(str, e["cmd"])) for e in execs)
    assert "/config/wg0.conf" in joined_all and "wg-quick up" in joined_all


def test_feed_stdin_writes_secret_to_socket():
    """_feed_stdin 必须真把 secret 写进 socket(socket=True 取 res.output;命门 #5)。

    回归保护:旧实现用 stdin=True+detach=True,exec_run 返回 (exit, b''),既无可写
    socket 也非 socket,secret 被静默丢弃。这里用 ExecResult-like + 假 socket 钉死
    「socket=True 取到 socket 本体并 sendall + shutdown(SHUT_WR)」。
    """
    import socket as _socket
    from docker.models.containers import ExecResult
    import manager

    writes = []
    closed = {"shutdown": None, "close": False}

    class FakeSock:
        def sendall(self, data): writes.append(data)
        def shutdown(self, how): closed["shutdown"] = how
        def close(self): closed["close"] = True

    fake_sock = FakeSock()
    captured_kw = {}

    class C:
        def exec_run(self, cmd, **kw):
            captured_kw.update(kw)
            return ExecResult(None, fake_sock)  # socket=True → output 即 socket 本体

    manager._feed_stdin(C(), ["sh", "-c", "cat"], "s3cret")

    # 必须传 socket=True(否则拿不到可写 socket),且仍带 stdin=True
    assert captured_kw.get("socket") is True
    assert captured_kw.get("stdin") is True
    assert captured_kw.get("detach") in (None, False)  # detach 会让 output 变 bytes
    # secret 真的写进了 socket(末尾换行),写后关写端发 EOF
    assert writes == [b"s3cret\n"]
    assert closed["shutdown"] == _socket.SHUT_WR
    assert closed["close"] is True


def test_feed_stdin_unwraps_socketio_sock_attr():
    """docker SDK 的 socket 是 SocketIO 包装,底层 socket 在 ._sock —— 必须解包写它。"""
    from docker.models.containers import ExecResult
    import manager

    writes = []

    class RawSock:
        def sendall(self, data): writes.append(data)
        def shutdown(self, how): pass

    class SocketIOLike:
        _sock = RawSock()
        def close(self): pass

    class C:
        def exec_run(self, cmd, **kw):
            return ExecResult(None, SocketIOLike())

    manager._feed_stdin(C(), ["sh", "-c", "cat"], "pw")
    assert writes == [b"pw\n"]


def test_put_file_tars_blob_into_running_container(monkeypatch):
    import manager, tarfile, io as _io

    captured = {}

    class FakeContainer:
        def put_archive(self, path, data):
            captured["path"] = path
            captured["data"] = data.read()
            return True

    class FakeContainers:
        def get(self, name):
            captured["name"] = name
            return FakeContainer()

    class FakeDc:
        containers = FakeContainers()

    monkeypatch.setattr(manager, "dc", FakeDc())

    blob = b"\x7fELF\x00\x01\x02binary-not-text\xff\xfe"   # 故意非 UTF-8 二进制
    ref = manager.put_file("byo1", "/root", "installer.run", blob)

    # 取的是 vpn-{cid} 运行中容器
    assert captured["name"] == "vpn-byo1"
    # put_archive 收到 (dest_dir, file stream)
    assert captured["path"] == "/root"
    # 收到的是合法 tar,解出来的成员名是相对文件名、内容逐字节等于原二进制(绝不读成文本)
    tf = tarfile.open(fileobj=_io.BytesIO(captured["data"]))
    member = tf.getmembers()[0]
    assert member.name == "installer.run"
    assert member.size == len(blob)
    assert tf.extractfile(member).read() == blob
    # 返回容器内落点路径引用(供 config_json 存)
    assert ref == "/root/installer.run"


def test_put_file_reads_in_chunks_and_closes_failed_archive(monkeypatch):
    import manager, io, pytest
    from types import SimpleNamespace
    class BoundedFile(io.BytesIO):
        def read(self, n=-1):
            assert 0 <= n <= 65536
            return super().read(n)
    captured = []
    class Container:
        def put_archive(self, path, stream):
            captured.append(stream)
            while stream.read(65536): pass
            return False
    monkeypatch.setattr(manager, 'dc', SimpleNamespace(containers=SimpleNamespace(get=lambda name: Container())))
    data = b'\xff' * (3 * 1024 * 1024)
    with pytest.raises(RuntimeError):
        manager.put_file('fixture', '/root', 'client.run', BoundedFile(data), len(data))
    assert captured[0].closed
