"""Runtime 家族:把适配器声明 + 家族常量合成 docker run 入参。

按 spec["runtime"] 分派:
- hagb(EC/aTrust):noVNC 交互登录,占位 {mac}/{vnc_password}/{version} 在此替换。
- oss(openconnect/openfortivpn/openvpn/wireguard):自建多客户端镜像,无头无 VNC,
  协议经 env(VPN_PROTOCOL)传给容器 entrypoint;凭据/配置由 manager.oss_connect()
  经 stdin/临时文件注入,绝不进 env/命令行(命门 #5)。SOCKS5 仅在 docker 内网 1080
  暴露,不映射到 host(命门 #4)。
"""


def _ctx(ch, vnc_pwd):
    return {
        "mac": ch.get("mac", ""),
        "vnc_password": vnc_pwd,
        "version": ch.get("ec_ver") or "7.6.3",
    }


def _build_hagb(ch, spec, vnc_pwd, vpn_net):
    """合成 hagb 家族(EC/aTrust)的 dc.containers.run 入参。纯函数。"""
    ctx = _ctx(ch, vnc_pwd)
    env = {k: str(v).format(**ctx) for k, v in spec.get("env", {}).items()}
    # 对齐 legacy:EC_VER 仅在显式指定版本时注入(空 ec_ver 不带 EC_VER)
    if "EC_VER" in env and not ch.get("ec_ver"):
        del env["EC_VER"]
    kw = {
        "image": spec["image"].format(**ctx),
        "name": f"vpn-{ch['id']}",
        "detach": True,
        "devices": list(spec.get("devices", [])),
        "cap_add": list(spec.get("caps", [])),
        "environment": env,
        "hostname": ch["id"],
        "volumes": {f"vpndata-{ch['id']}": {"bind": "/root", "mode": "rw"}},
        "ports": {"8080/tcp": ("127.0.0.1", None)},
        "network": vpn_net,
        "restart_policy": {"Name": "unless-stopped"},
    }
    sysctls = spec.get("sysctls") or {}
    if sysctls:
        kw["sysctls"] = dict(sysctls)
    return kw


def _build_oss(ch, spec, vnc_pwd, vpn_net):
    """合成 oss 家族的 dc.containers.run 入参。纯函数。

    - 无 noVNC:不映射任何 host 端口(SOCKS5 1080 仅 docker 内网,命门 #4)。
    - 协议名经 VPN_PROTOCOL 传给 entrypoint;凭据/配置不在此注入(命门 #5)。
    - 卷挂到 /config:OpenVPN/WireGuard 的上传文件由 oss_connect 落到此卷再连接。
    """
    kw = {
        "image": spec["image"],
        "name": f"vpn-{ch['id']}",
        "detach": True,
        "devices": list(spec.get("devices", [])),
        "cap_add": list(spec.get("caps", [])),
        "environment": {"VPN_PROTOCOL": spec["protocol"]},
        "hostname": ch["id"],
        "volumes": {f"vpndata-{ch['id']}": {"bind": "/config", "mode": "rw"}},
        "network": vpn_net,
        "restart_policy": {"Name": "unless-stopped"},
    }
    sysctls = spec.get("sysctls") or {}
    if sysctls:
        kw["sysctls"] = dict(sysctls)
    dcr = spec.get("device_cgroup_rules")     # openfortivpn:放行 /dev/ppp(major 108)
    if dcr:
        kw["device_cgroup_rules"] = list(dcr)
    return kw


def _build_byo(ch, spec, vnc_pwd, vpn_net):
    """合成 byo 家族(自定义桌面兜底)的 dc.containers.run 入参。纯函数。

    镜像 _build_hagb 而非 _build_oss:byo 有 host noVNC 端口(命门 #4),
    经 USE_NOVNC/PASSWORD env 让既有 login url 直接可用;caps/devices 走 manifest
    (NET_ADMIN+MKNOD + /dev/net/tun)。byo 不注入凭据(无 connect)、不带版本(无 EC_VER 特例)。
    """
    ctx = _ctx(ch, vnc_pwd)
    env = {k: str(v).format(**ctx) for k, v in spec.get("env", {}).items()}
    kw = {
        "image": spec["image"],
        "name": f"vpn-{ch['id']}",
        "detach": True,
        "devices": list(spec.get("devices", [])),
        "cap_add": list(spec.get("caps", [])),
        "environment": env,
        "hostname": ch["id"],
        "volumes": {f"vpndata-{ch['id']}": {"bind": "/root", "mode": "rw"}},
        "ports": {"8080/tcp": ("127.0.0.1", None)},
        "network": vpn_net,
        "restart_policy": {"Name": "unless-stopped"},
    }
    sysctls = spec.get("sysctls") or {}
    if sysctls:
        kw["sysctls"] = dict(sysctls)
    return kw


_BUILDERS = {"hagb": _build_hagb, "oss": _build_oss, "byo": _build_byo}


def build_run_kwargs(ch, spec, vnc_pwd, vpn_net):
    """按 runtime 分派合成 dc.containers.run(**kw) 入参。纯函数,无副作用。"""
    rt = spec.get("runtime")
    builder = _BUILDERS.get(rt)
    if builder is None:
        raise ValueError(f"unsupported runtime: {rt}")
    return builder(ch, spec, vnc_pwd, vpn_net)
