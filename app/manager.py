"""容器编排 + mihomo 热加载 + SOCKS5 探活。"""
import os
import io
import re
import socket
import tarfile
import time
import requests
import urllib3
import yaml
import docker
from datetime import datetime, timezone

# 探活不校验证书(verify=False):内网目标多为自签证书,屏蔽随之而来的告警。
urllib3.disable_warnings(urllib3.exceptions.InsecureRequestWarning)

import store
import registry
import adapters

VPN_NET = os.environ["VPN_NET"]
CTRL = os.environ["MIHOMO_CTRL_URL"]
SECRET = os.environ["MIHOMO_SECRET"]
CFG = os.environ.get("MIHOMO_CONFIG_PATH", "/cfg/config.yaml")

dc = docker.from_env()


def create_channel(ch, vnc_pwd):
    """起一个 VPN 容器。

    hagb:noVNC(8080)由 Docker 分配高位随机端口映射到 127.0.0.1,返回该端口。
    oss:无 noVNC,SOCKS5(1080)仅 docker 内网;起容器后经 stdin 注入凭据并连接,
         novnc 返回 None。1080 绝不映射到 host(命门 #4)。
    """
    spec = registry.get(ch["vpn_type"])
    kw = adapters.build_run_kwargs(ch, spec, vnc_pwd, VPN_NET)

    # oss 家族:容器内 danted 直接解析目标域名,但宿主 Clash(TUN)会劫持容器明文 :53
    # 查询返回 fake-ip,致内网域名不可路由。改用 mihomo 的 DNS(DoH 上游+listen :53,
    # 绕开劫持),danted 即可拿到真实内网 IP。hagb/byo 自带客户端 DNS,不动。
    if spec.get("runtime") == "oss":
        try:
            kw["dns"] = [socket.gethostbyname("mihomo")]
        except OSError:
            pass

    try:
        dc.containers.get(kw["name"]).remove(force=True)
    except docker.errors.NotFound:
        pass

    c = dc.containers.run(**kw)
    c.reload()
    if spec.get("runtime") == "oss":
        oss_connect(c, spec, store.get_config(ch["id"]))
        return c.id, None
    novnc = int(c.ports["8080/tcp"][0]["HostPort"])
    return c.id, novnc


def oss_connect(c, spec, config):
    """对 oss 容器经 exec_run + stdin 注入凭据并发起连接(命门 #5:密钥不进命令行)。

    config 为已解密明文(server/username/password/config_file…)。容器 entrypoint
    阻塞等隧道接口出现后才 exec danted 成 PID1;此处经 exec_run 启动 VPN 客户端进程
    拉起隧道,密码/私钥经 stdin 喂入(绝不进 argv)。
    """
    proto = spec["protocol"]
    # server/username strip 首尾空白:用户输入易误带空格,会让网关认证/连接失败
    # (实测尾随空格的用户名被 FortiGate 拒登)。密码不 strip(密码可能合法含空格)。
    server = config.get("server", "").strip()
    user = config.get("username", "").strip()
    pwd = config.get("password", "")
    if proto in ("anyconnect", "gp", "fortinet", "nc", "pulse"):
        cmd = ("openconnect --protocol=%s --user=%s --passwd-on-stdin --non-inter "
               "--background --script /usr/share/vpnc-scripts/vpnc-script %s "
               ">/tmp/connect.log 2>&1" % (proto, _sh(user), _sh(server)))
        _feed_stdin(c, ["sh", "-c", cmd], pwd)
    elif proto == "openfortivpn":
        # 镜像内 openfortivpn(Debian 1.19.0)无 stdin 喂密码选项;密码经 -c 配置文件
        # 注入(与 openvpn/wg 一致,0600,绝不进 argv,命门 #5)。host:port/user 非密,留 argv。
        host = server.split("://", 1)[-1]
        # 自签网关:openssl 单次 TLS 握手抓证书指纹做 TOFU 信任(等同 FortiClient「信任此证书」)。
        # 只连一次网关 —— 不再用 openfortivpn 额外探一次,避免与真连撞 FortiGate 并发/限速。
        digest = _forti_cert_digest(c, host)
        cfg = "password = %s\n" % pwd
        if digest:
            cfg += "trusted-cert = %s\n" % digest
        _write_file(c, "/config/forti.conf", cfg)
        # _write_file 经 socket 异步落盘;连接前确认配置完整写入(规避读到半截/旧文件的竞态)。
        c.exec_run(["sh", "-c", "for i in 1 2 3 4 5 6; do grep -q '^password' /config/forti.conf "
                    "&& exit 0; sleep 0.3; done"])
        # --persistent:掉线后每 20s 自动重连(进程不退出),避免 FortiGate 会话超时后
        # 隧道断了没人重连、容器陷入「等 ppp0→超时→重启」死循环。
        c.exec_run(["sh", "-c", "openfortivpn %s -u %s -c /config/forti.conf --persistent=20 "
                    ">/tmp/connect.log 2>&1" % (_sh(host), _sh(user))], detach=True)
    elif proto == "openvpn":
        # .ovpn 配置经 stdin 落到 /config(私钥不进 argv);可选账密落 /config/auth.txt
        _write_file(c, "/config/client.ovpn", config.get("config_file", ""))
        auth = ""
        if user and pwd:
            _write_file(c, "/config/auth.txt", "%s\n%s\n" % (user, pwd))
            auth = "--auth-user-pass /config/auth.txt "
        c.exec_run(["sh", "-c", "openvpn --config /config/client.ovpn %s--daemon "
                    ">/tmp/connect.log 2>&1" % auth], detach=True)
    elif proto == "wireguard":
        # wg .conf(含私钥)经 stdin 落到 /config,绝不进 argv
        _write_file(c, "/config/wg0.conf", config.get("config_file", ""))
        c.exec_run(["sh", "-c", "wg-quick up /config/wg0.conf >/tmp/connect.log 2>&1"],
                   detach=True)
    else:
        raise ValueError(f"unknown oss protocol: {proto}")


def _sh(s):
    """单引号转义,供 sh -c 拼接非密钥参数(server/user 非机密)。"""
    return "'" + str(s).replace("'", "'\\''") + "'"


def _feed_stdin(c, cmd, secret):
    """exec_run 一条命令并把 secret 经 stdin 写入(密钥不进 argv,ps 不可见)。

    必须传 socket=True 才能拿到可写 socket:socket=True 时 exec_start 返回连接
    socket,exec_run 据此返回 ExecResult(None, <socket>),即 res.output 就是 socket
    本体。docker SDK 的 socket 是 SocketIO 包装(底层 socket 在 ._sock),少数传输
    下 res.output 直接就是裸 socket;两种都兜住。写完关写端发 EOF,让客户端
    (openconnect --passwd-on-stdin / cat>file)
    读到密码后继续。detach=True 拿不到 socket(output 是 bytes),会静默丢密码。
    """
    res = c.exec_run(cmd, stdin=True, socket=True)
    sock = getattr(res, "output", None)
    raw = getattr(sock, "_sock", sock)
    if raw is None or not hasattr(raw, "sendall"):
        return
    try:
        raw.sendall((secret + "\n").encode())
        # 关写端发 EOF;读端(容器进程)据此知道 stdin 结束。
        try:
            raw.shutdown(socket.SHUT_WR)
        except OSError:
            pass
    finally:
        try:
            sock.close()
        except Exception:
            pass


def _write_file(c, path, content):
    """把配置文件内容经 stdin 写入容器内文件(私钥/证书不进 argv;umask 077)。"""
    _feed_stdin(c, ["sh", "-c", "umask 077; cat > %s" % _sh(path)], content)


def _forti_cert_digest(c, host):
    """openssl 单次 TLS 握手取网关证书 sha256(DER)指纹,作 openfortivpn 的 trusted-cert(TOFU)。

    只做 TLS 握手 —— 不认证、不建隧道、不占 FortiGate 会话,与 openfortivpn 的
    trusted-cert 算法一致(实测同值)。拿不到(网关不可达等)返回 None。指纹非机密。
    """
    sni = host.rsplit(":", 1)[0]
    cmd = ("openssl s_client -connect %s -servername %s </dev/null 2>/dev/null "
           "| openssl x509 -outform DER 2>/dev/null | sha256sum" % (_sh(host), _sh(sni)))
    res = c.exec_run(["sh", "-c", cmd])
    out = res.output if hasattr(res, "output") else res
    text = out.decode("utf-8", "replace") if isinstance(out, (bytes, bytearray)) else str(out)
    m = re.search(r"\b([0-9a-fA-F]{64})\b", text)
    return m.group(1) if m else None


def put_file(cid, dest_dir, filename, blob):
    """把一个二进制文件塞进运行中容器的 dest_dir(byo 安装器上传用,命门 #5)。

    blob 为原始 bytes(绝不读成文本)。in-memory 打成单成员 tar,经 container.put_archive
    解到 dest_dir(dest_dir 须已存在,byo 镜像 mkdir 之)。返回容器内落点路径引用。
    """
    c = dc.containers.get(f"vpn-{cid}")
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        ti = tarfile.TarInfo(name=filename)   # name = 相对 dest_dir 的文件名,绝不带 dest 前缀
        ti.size = len(blob)
        ti.mode = 0o755                        # 可执行,便于用户在桌面里直接跑安装器
        tar.addfile(ti, io.BytesIO(blob))
    buf.seek(0)
    if not c.put_archive(dest_dir, buf.getvalue()):
        raise RuntimeError("put_archive failed")
    return f"{dest_dir.rstrip('/')}/{filename}"


def ensure_novnc_bridge(cid):
    """确保 noVNC 的 WS 后端(websockify)起着;arm64 自愈。幂等。

    hagb 镜像的 noVNC 链:tinyproxy-novnc(8080)→ 静态 busybox httpd(8081) +
    `/websockify` → websockify(8082)→ Xtigervnc(5901)。镜像用
    `su daemon -s /bin/sh -c 'websockify --daemon 127.0.0.1:8082 127.0.0.1:5901'`
    (见容器内 /usr/local/bin/novnc-min-size.sh)起 websockify,但 arm64 下
    `su -s /bin/sh` 报 "Permission denied" → websockify 永不启动 → `/websockify` 无后端
    → noVNC「无法连接到服务器」。这里以 root 直接拉起同形态的 websockify,绕开 su。
    注:镜像内 websockify 是 C 版(不支持 --web),故沿用镜像的 8082→5901 形态、由
    tinyproxy 合并静态+WS(login url 的 `path=websockify/` 即走这条),不要改成 --web 顶 8080。
    """
    try:
        c = dc.containers.get(f"vpn-{cid}")
        start = (
            "ss -tln 2>/dev/null | grep -q :8082 && exit 0; "            # WS 后端已起 → 跳过
            "for i in $(seq 1 30); do ss -tln 2>/dev/null | grep -q :5901 && break; sleep 0.3; done; "  # 等 VNC 就绪
            "websockify --daemon 127.0.0.1:8082 127.0.0.1:5901 >/tmp/novnc-bridge.log 2>&1"
        )
        c.exec_run(["sh", "-c", start], user="root", detach=True)
        # 等 8082(WS 后端)起来再返回,避免前端 iframe 抢跑连到无后端的 `/websockify`(红条「无法连接」)
        for _ in range(30):
            rc, _o = c.exec_run(["sh", "-c", "ss -tln 2>/dev/null | grep -q :8082"])
            if rc == 0:
                break
            time.sleep(0.2)
    except Exception:
        pass


def stop(cid):
    try:
        dc.containers.get(f"vpn-{cid}").stop()
    except docker.errors.NotFound:
        pass


def remove(cid):
    try:
        dc.containers.get(f"vpn-{cid}").remove(force=True)
    except docker.errors.NotFound:
        pass


def probe(ch):
    """经该通道 SOCKS5 访问内网探测地址。返回 (通否, 往返毫秒|None)。socks5h=远程解析。"""
    if not ch.get("probe_url"):
        return False, None
    px = f"socks5h://vpn-{ch['id']}:1080"
    try:
        t0 = time.monotonic()
        # verify=False:连通性探活,内网目标多为自签证书,不应因证书校验失败误判为不通。
        r = requests.get(ch["probe_url"], proxies={"http": px, "https": px},
                         timeout=6, verify=False)
        ms = int((time.monotonic() - t0) * 1000)
        return (r.status_code < 500), ms
    except Exception:
        return False, None


def rebuild():
    """按当前所有通道+规则重写 mihomo 配置并热加载(force reload,不断现有连接)。"""
    chs = store.list_channels()
    rules = store.all_rules()
    try:
        with open(CFG) as f:
            base = yaml.safe_load(f) or {}
    except FileNotFoundError:
        base = {}

    base["proxies"] = [
        {"name": f"ch-{c['id']}", "type": "socks5",
         "server": f"vpn-{c['id']}", "port": 1080, "udp": True}
        for c in chs
    ]
    base["proxy-groups"] = []
    out = []
    for r in rules:
        if not r["enabled"]:
            continue
        if r["kind"] == "ip":
            out.append(f"IP-CIDR,{r['pattern']},ch-{r['channel_id']},no-resolve")
        else:
            out.append(f"DOMAIN-SUFFIX,{r['pattern']},ch-{r['channel_id']}")
    out.append("MATCH,DIRECT")
    base["rules"] = out

    with open(CFG, "w") as f:
        yaml.safe_dump(base, f, allow_unicode=True, sort_keys=False)

    try:
        r = requests.put(
            f"{CTRL}/configs",
            params={"force": "true"},
            json={"path": CFG},
            headers={"Authorization": f"Bearer {SECRET}"},
            timeout=10,
        )
        return r.status_code
    except Exception as e:
        return f"{type(e).__name__}: {e}"


def _parse_docker_time(s):
    s = s.replace("Z", "+00:00")
    if "." in s:
        head, rest = s.split(".", 1)
        tz = ""
        for sep in ("+", "-"):
            if sep in rest:
                rest, tzpart = rest.split(sep, 1)
                tz = sep + tzpart
                break
        s = f"{head}.{rest[:6]}{tz}"
    return datetime.fromisoformat(s)


def uptime(cid):
    """容器已运行时长,人话字符串;停止/不存在返回 None。"""
    try:
        c = dc.containers.get(f"vpn-{cid}")
        st = c.attrs.get("State", {})
        if not st.get("Running"):
            return None
        secs = int((datetime.now(timezone.utc) - _parse_docker_time(st["StartedAt"])).total_seconds())
        if secs < 60:
            return f"{secs}秒"
        if secs < 3600:
            return f"{secs // 60}分钟"
        if secs < 86400:
            return f"{secs // 3600}小时{(secs % 3600) // 60}分"
        return f"{secs // 86400}天{(secs % 86400) // 3600}小时"
    except Exception:
        return None


def novnc_port(cid):
    """容器当前实际映射到 host 的 noVNC 端口(8080/tcp)。动态 host 端口在容器重启后会变,
    DB 存的会过期,故每次实时读。无容器 / 无映射返回 None。"""
    try:
        c = dc.containers.get(f"vpn-{cid}")
        c.reload()
        m = c.ports.get("8080/tcp")
        return int(m[0]["HostPort"]) if m and m[0].get("HostPort") else None
    except Exception:
        return None


def logs(cid, tail=200):
    try:
        c = dc.containers.get(f"vpn-{cid}")
        raw = c.logs(tail=tail).decode("utf-8", "replace").splitlines()
    except Exception as e:
        return [f"<no logs: {type(e).__name__}: {e}>"]
    # 折叠相邻完全相同的行(容器内 noVNC/websockify 反复刷同一句 DeprecationWarning),
    # 只留真信号;时间戳不同的行不会被折叠,故不丢日志。
    out, prev, n = [], None, 0
    for ln in raw:
        if ln == prev:
            n += 1
            continue
        if n > 1:
            out.append(f"  ⋯ 上一行重复 {n} 次")
        out.append(ln)
        prev, n = ln, 1
    if n > 1:
        out.append(f"  ⋯ 上一行重复 {n} 次")
    return out


def connections():
    """代理 mihomo /connections 给监控屏。"""
    try:
        r = requests.get(f"{CTRL}/connections",
                         headers={"Authorization": f"Bearer {SECRET}"}, timeout=5)
        return r.json()
    except Exception:
        return {"connections": [], "downloadTotal": 0, "uploadTotal": 0}


def proxies():
    """代理 mihomo /proxies,只回本工具通道节点 ch-* 的 {name,alive,type}。"""
    try:
        r = requests.get(f"{CTRL}/proxies",
                         headers={"Authorization": f"Bearer {SECRET}"}, timeout=5)
        allp = r.json().get("proxies", {})
        return [
            {"name": name, "alive": bool(p.get("alive")), "type": p.get("type")}
            for name, p in allp.items()
            if name.startswith("ch-")
        ]
    except Exception:
        return []


def mihomo_alive():
    try:
        r = requests.get(f"{CTRL}/version",
                         headers={"Authorization": f"Bearer {SECRET}"}, timeout=3)
        return r.status_code == 200
    except Exception:
        return False
