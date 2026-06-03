import registry
import adapters

VNET = "testnet"


def _ch(**over):
    ch = {"id": "abc123", "vpn_type": "easyconnect", "ec_ver": "7.6.3",
          "mac": "02:00:00:00:00:01"}
    ch.update(over)
    return ch


def test_easyconnect_kwargs_match_legacy():
    ch = _ch(ec_ver="7.6.3")
    kw = adapters.build_run_kwargs(ch, registry.get("easyconnect"), "vncpw01", VNET)
    assert kw == {
        "image": "hagb/docker-easyconnect:7.6.3",
        "name": "vpn-abc123",
        "detach": True,
        "devices": ["/dev/net/tun:/dev/net/tun:rwm"],
        "cap_add": ["NET_ADMIN"],
        "environment": {"USE_NOVNC": "1", "PASSWORD": "vncpw01", "EXIT": "",
                        "FAKE_HWADDR": "02:00:00:00:00:01", "EC_VER": "7.6.3",
                        "DISABLE_PKG_VERSION_XML": "1"},
        "hostname": "abc123",
        "volumes": {"vpndata-abc123": {"bind": "/root", "mode": "rw"}},
        "ports": {"8080/tcp": ("127.0.0.1", None)},
        "network": "testnet",
        "restart_policy": {"Name": "unless-stopped"},
    }


def test_easyconnect_version_7_6_7():
    ch = _ch(ec_ver="7.6.7")
    kw = adapters.build_run_kwargs(ch, registry.get("easyconnect"), "v", VNET)
    assert kw["image"] == "hagb/docker-easyconnect:7.6.7"
    assert kw["environment"]["EC_VER"] == "7.6.7"


def test_easyconnect_empty_ec_ver_omits_ec_ver_env():
    ch = _ch(ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("easyconnect"), "v", VNET)
    assert kw["image"] == "hagb/docker-easyconnect:7.6.3"   # 镜像仍回落 7.6.3
    assert "EC_VER" not in kw["environment"]                 # 但不注入 EC_VER(对齐 legacy)


def test_easyconnect_sets_disable_pkg_version_xml():
    ch = _ch(ec_ver="7.6.3")
    kw = adapters.build_run_kwargs(ch, registry.get("easyconnect"), "v", VNET)
    assert kw["environment"]["DISABLE_PKG_VERSION_XML"] == "1"


def test_atrust_kwargs_match_legacy():
    ch = _ch(vpn_type="atrust", ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("atrust"), "vncpw01", VNET)
    assert kw["image"] == "hagb/docker-atrust:latest"
    assert kw["sysctls"] == {"net.ipv4.conf.default.route_localnet": "1"}
    assert kw["environment"] == {
        "USE_NOVNC": "1", "PASSWORD": "vncpw01", "EXIT": "",
        "FAKE_HWADDR": "02:00:00:00:00:01", "DISABLE_PKG_VERSION_XML": "1"}
    assert "EC_VER" not in kw["environment"]


def test_oss_anyconnect_kwargs():
    ch = _ch(vpn_type="anyconnect", ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("anyconnect"), "vncpw01", VNET)
    # oss 家族:无 EC env/noVNC env;image 固定;1080 不映射到 host(无 ports 中的 1080)
    assert kw["image"] == "vpnmgr/oss-vpn:latest"
    assert kw["name"] == "vpn-abc123"
    assert kw["hostname"] == "abc123"
    assert kw["network"] == "testnet"
    assert kw["detach"] is True
    assert kw["cap_add"] == ["NET_ADMIN"]
    assert kw["devices"] == ["/dev/net/tun:/dev/net/tun:rwm"]
    assert kw["sysctls"] == {"net.ipv4.ip_forward": "1"}
    assert kw["restart_policy"] == {"Name": "unless-stopped"}
    assert kw["volumes"] == {"vpndata-abc123": {"bind": "/config", "mode": "rw"}}
    # 协议经 env 传给 entrypoint;密钥绝不进 env/命令行(由 oss_connect 经 stdin 注入)
    assert kw["environment"] == {"VPN_PROTOCOL": "anyconnect"}
    assert "PASSWORD" not in kw["environment"]
    # 命门 #4:1080 绝不出现在 host 端口映射;oss 无 noVNC,连 8080 也不映射
    assert "ports" not in kw or all("1080" not in str(k) for k in kw.get("ports", {}))
    assert "8080/tcp" not in kw.get("ports", {})


def test_oss_wireguard_protocol_env():
    ch = _ch(vpn_type="wireguard", ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("wireguard"), "v", VNET)
    assert kw["environment"] == {"VPN_PROTOCOL": "wireguard"}


def test_unsupported_runtime_still_raises():
    # 分派表对未知 runtime 仍抛 ValueError(回归硬拒契约)。byo 已受支持,改用真不存在的 runtime。
    import pytest
    bad = dict(registry.get("anyconnect"))
    bad["runtime"] = "nonesuch"
    with pytest.raises(ValueError):
        adapters.build_run_kwargs(_ch(vpn_type="anyconnect"), bad, "v", VNET)


def test_byo_kwargs_has_novnc_host_port_and_tun():
    ch = _ch(vpn_type="custom", ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("custom"), "vncpw01", VNET)
    assert kw["image"] == "vpnmgr/byo-desktop:latest"
    assert kw["name"] == "vpn-abc123"
    assert kw["hostname"] == "abc123"
    assert kw["network"] == "testnet"
    assert kw["detach"] is True
    # 命门 #6:caps/devices 走 manifest,byo 需 NET_ADMIN + MKNOD + /dev/net/tun
    assert kw["cap_add"] == ["NET_ADMIN", "MKNOD"]
    assert kw["devices"] == ["/dev/net/tun:/dev/net/tun:rwm"]
    # 命门 #4:byo 有 host noVNC 端口,绑 127.0.0.1 随机高位(None);1080 绝不映射
    assert kw["ports"] == {"8080/tcp": ("127.0.0.1", None)}
    assert all("1080" not in str(k) for k in kw["ports"])
    # byo 卷挂 /root(镜像 entrypoint 用),restart unless-stopped(同 hagb)
    assert kw["volumes"] == {"vpndata-abc123": {"bind": "/root", "mode": "rw"}}
    assert kw["restart_policy"] == {"Name": "unless-stopped"}
    # VNC 密码经 env 注入(让既有 login url 的 password= 生效);USE_NOVNC 开
    assert kw["environment"]["USE_NOVNC"] == "1"
    assert kw["environment"]["PASSWORD"] == "vncpw01"
    # byo 不带版本特例:env 里没有 EC_VER
    assert "EC_VER" not in kw["environment"]


def test_byo_no_sysctls_when_empty():
    ch = _ch(vpn_type="custom", ec_ver="")
    kw = adapters.build_run_kwargs(ch, registry.get("custom"), "v", VNET)
    # custom manifest sysctls: {} → 不应出现 sysctls 键(同 hagb 空 sysctls 行为)
    assert "sysctls" not in kw
