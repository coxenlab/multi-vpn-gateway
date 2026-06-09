import os
import time
import uuid
import random
import secrets
import sqlite3
import ipaddress

import requests

import docker

from fastapi import FastAPI, Request, UploadFile, File, Body
from fastapi.responses import HTMLResponse, JSONResponse, PlainTextResponse
from fastapi.staticfiles import StaticFiles

import store
import manager
import registry
import dockerhub
import preflight

HERE = os.path.dirname(__file__)
MIHOMO_HOST_PORT = os.environ.get("MIHOMO_HOST_PORT", "?")

app = FastAPI(title="VPN 管理网关")
store.init()


def _classify(token):
    """返回 ('ip', cidr) / ('domain', token) / None。裸 IP 补 /32 或 /128;保留地址形态。"""
    t = (token or "").strip()
    if not t:
        return None
    addr = t.split("/")[0]
    try:
        ipaddress.ip_address(addr)
    except ValueError:
        # 域名:剥离 scheme / 路径 / userinfo / 端口,只留主机名
        # (否则 "https://oa.x.com/" 整串被当域名,生成的 DOMAIN-SUFFIX 永不命中)。
        host = t.split("://", 1)[-1].split("/", 1)[0].split("@")[-1]
        host = host.split(":", 1)[0].strip().strip(".").lower()
        return ("domain", host) if host else None
    if "/" in t:
        try:
            ipaddress.ip_network(t, strict=False)
        except ValueError:
            return None
        return ("ip", t)
    return ("ip", t + ("/128" if ":" in addr else "/32"))


@app.get("/", response_class=HTMLResponse)
def index():
    with open(os.path.join(HERE, "static", "index.html"), encoding="utf-8") as f:
        return f.read()


@app.get("/api/vpn-types")
def vpn_types():
    return registry.list_adapters()


@app.get("/api/vpn-types/{vtype}/versions")
def vpn_versions(vtype):
    try:
        spec = registry.get(vtype)
    except KeyError:
        return JSONResponse({"error": "unknown type"}, status_code=404)
    if not spec.get("versioned"):
        return {"versions": []}
    vs = dockerhub.versions(spec["version_repo"], registry.host_arch(),
                            spec.get("fallback_versions", []))
    return {"versions": vs}


@app.get("/api/channels")
def channels():
    out = []
    for c in store.list_channels():
        rs = store.list_rules(c["id"])
        c["domains"] = [r for r in rs if r["kind"] == "domain"]
        c["ips"] = [r for r in rs if r["kind"] == "ip"]
        c["volume_name"] = f"vpndata-{c['id']}"
        c["socks_proxy"] = f"ch-{c['id']}"
        c["socks_endpoint"] = f"vpn-{c['id']}:1080"
        up = manager.uptime(c["id"]) if c["status"] != "stopped" else None
        c["uptime"] = up
        # 状态对账:DB 记 running/logged_in 但容器实际不在(没容器 / 已停)→ 显示掉线,
        # 杜绝「概览看得到、实际没有容器」(起容器失败留下的 error 行,或容器被外部删/挂掉)。
        if up is None and c["status"] in ("running", "logged_in"):
            c["status"] = "down"
        out.append(c)
    return out


@app.post("/api/channels")
async def create(req: Request):
    b = await req.json()
    cid = uuid.uuid4().hex[:8]
    mac = "02:" + ":".join(f"{random.randint(0, 255):02x}" for _ in range(5))
    vnc = secrets.token_hex(4)  # 8 个十六进制字符 = tigervnc 8 字节上限内
    vtype = b.get("vpn_type", "easyconnect")
    cfg_in = b.get("config") or {}
    # 老 hagb 扁平入参兼容:server/username/password 仍从顶层取
    ch = {
        "id": cid, "name": b.get("name") or cid, "vpn_type": vtype,
        "server": b.get("server") or cfg_in.get("server", ""),
        "ec_ver": b.get("ec_ver", "7.6.3"),
        "login_method": b.get("login_method", "interactive"),
        "username": b.get("username") or cfg_in.get("username", ""),
        "password": b.get("password", ""),
        "vnc_password": vnc, "mac": mac, "probe_url": b.get("probe_url", ""),
        "status": "creating",
    }
    # 按 manifest inputs 决定哪些 config 字段是 secret(命门 #5)
    secret_keys = []
    try:
        spec = registry.get(vtype)
        secret_keys = [i["key"] for i in spec.get("inputs", []) if i.get("secret")]
    except KeyError:
        spec = {}
    store.add_channel(ch, config=cfg_in, secret_keys=secret_keys)
    try:
        container_id, novnc = manager.create_channel(ch, vnc)
    except Exception as e:
        store.set_status(cid, "error")
        return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)
    store.set_container(cid, container_id, novnc, "running")
    manager.rebuild()
    return store.get_channel(cid)


@app.patch("/api/channels/{cid}")
async def update(cid: str, req: Request):
    """编辑已有通道。接受 name/probe_url/server/username/password/ec_ver(只传要改的)。
    仅改 name/probe_url 不动容器;改了连接相关字段则重建容器使其生效(oss 无其它重连入口)。"""
    ch = store.get_channel(cid)
    if not ch:
        return JSONResponse({"error": "not found"}, status_code=404)
    b = await req.json()
    try:
        spec = registry.get(ch["vpn_type"])
        secret_keys = [i["key"] for i in spec.get("inputs", []) if i.get("secret")]
    except KeyError:
        secret_keys = []
    store.update_channel(cid, b, secret_keys=secret_keys)
    # 改了连接相关字段 → 重建容器(oss 凭据从 config 读,只有重建才会重连);否则原样返回
    if ({"server", "username", "password", "ec_ver"} & set(b.keys())) and ch.get("container_id"):
        ch2 = store.get_channel(cid)
        try:
            container_id, novnc = manager.create_channel(ch2, ch2["vnc_password"])
        except Exception as e:
            store.set_status(cid, "error")
            return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)
        store.set_container(cid, container_id, novnc, "running")
        manager.rebuild()
    return store.get_channel(cid)


@app.get("/api/channels/{cid}/login")
def login(cid):
    ch = store.get_channel(cid)
    if not ch:
        return JSONResponse({"error": "not found"}, status_code=404)
    if ch.get("login_method") == "headless":
        return {"login_mode": "headless"}   # 无头无 noVNC,前端据此跳过登录屏
    # 实时读容器当前映射端口:动态 host 端口在容器重启后会变,DB 存的会过期 → noVNC 连接被拒。
    port = manager.novnc_port(cid)
    if not port:
        return JSONResponse({"error": "no novnc port"}, status_code=404)
    if port != ch.get("novnc_port"):
        store.set_novnc_port(cid, port)    # 回写,保持概览/详情端口显示一致
    manager.ensure_novnc_bridge(cid)   # arm64 镜像 websockify 自愈,否则 noVNC 连不上
    # path 必须带尾斜杠:镜像内 tinyproxy 把 /websockify 301 重定向到 /websockify/,
    # 而 WebSocket 握手不跟随 301 → 不加斜杠会「无法连接到服务器」。
    url = (f"http://127.0.0.1:{port}/vnc.html"
           f"?path=websockify/&autoconnect=true&resize=remote&password={ch['vnc_password']}")
    return {"url": url}


@app.post("/api/channels/{cid}/upload")
async def upload(cid, file: UploadFile = File(...)):
    ch = store.get_channel(cid)
    if not ch:
        return JSONResponse({"error": "not found"}, status_code=404)
    blob = await file.read()              # bytes,绝不读成文本、绝不入 SQLite(命门 #5)
    try:
        manager.put_file(cid, "/root", file.filename, blob)
    except Exception as e:
        return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)
    # config_json 只存非密文件名引用(供前端展示已装包名);二进制留在数据卷
    store.set_config_field(cid, "package", file.filename, secret=False)
    return {"ok": True, "package": file.filename}


@app.get("/api/channels/{cid}/status")
def status(cid):
    ch = store.get_channel(cid)
    if not ch:
        return JSONResponse({"error": "not found"}, status_code=404)
    ok, ms = manager.probe(ch)
    new = "logged_in" if ok else ("running" if ch["status"] == "logged_in" else ch["status"])
    store.set_status(cid, new)
    if ms is not None:
        store.set_latency(cid, ms)
    return {"status": new, "connected": ok, "latency_ms": ms}


@app.post("/api/channels/{cid}/rules")
async def add_rules(cid, req: Request):
    b = await req.json()
    patterns = b.get("patterns") or ([b["pattern"]] if b.get("pattern") else [])
    forced = b.get("kind")
    added = {"domain": 0, "ip": 0}
    rejected = []
    existing = {(r["kind"], r["pattern"]) for r in store.list_rules(cid)}
    for tok in patterns:
        if forced == "domain":
            kind, pat = "domain", (tok or "").strip()
        elif forced == "ip":
            c = _classify(tok)
            if not c or c[0] != "ip":
                rejected.append(tok)
                continue
            kind, pat = c
        else:
            c = _classify(tok)
            if not c:
                rejected.append(tok)
                continue
            kind, pat = c
        if not pat or (kind, pat) in existing:
            continue
        store.add_rule(cid, kind, pat)
        existing.add((kind, pat))
        added[kind] += 1
    code = manager.rebuild()
    rs = store.list_rules(cid)
    return {"reload_status": code,
            "domains": [r for r in rs if r["kind"] == "domain"],
            "ips": [r for r in rs if r["kind"] == "ip"],
            "added": added, "rejected": rejected}


@app.delete("/api/channels/{cid}/rules/{rid}")
def del_rule(cid, rid: int):
    store.del_rule(rid)
    return {"ok": True, "reload_status": manager.rebuild()}


@app.patch("/api/channels/{cid}/rules/{rid}")
async def patch_rule(cid, rid: int, req: Request):
    b = await req.json()
    store.set_rule_enabled(rid, bool(b.get("enabled")))
    return {"ok": True, "reload_status": manager.rebuild()}


@app.post("/api/channels/{cid}/start")
def start(cid):
    # hagb(EC/aTrust)的守护进程与 oss 经 exec 注入的隧道都扛不住原地 docker start
    # (守护进程不重新初始化、注入的客户端进程丢失)→ 容器崩退码 1 或起来无隧道。
    # 「启动/重启」一律重建容器(docker run fresh,复用同卷/MAC/hostname),才是能恢复的原语。
    # 例外:byo 桌面容器的客户端是用户手动装在可写层(非 /root 卷),重建会抹掉 →
    #       原地 docker start(桌面+microsocks 在 entrypoint,扛得住原地重启)。
    ch = store.get_channel(cid)
    if not ch:
        return JSONResponse({"error": "not found"}, status_code=404)
    try:
        runtime = registry.get(ch["vpn_type"]).get("runtime")
    except KeyError:
        runtime = None
    if runtime == "byo":
        manager.start(cid)
        store.set_status(cid, "running")
        return {"ok": True}
    # 重建是同步的,aTrust/EC 要十几秒;期间先落「starting」,否则前端 8s 轮询拿到的
    # 还是 stopped → 卡片一直显示「已停止」(用户以为没生效)。
    store.set_status(cid, "starting")
    try:
        container_id, novnc = manager.create_channel(ch, ch["vnc_password"])
    except Exception as e:
        store.set_status(cid, "error")
        return JSONResponse({"error": f"{type(e).__name__}: {e}"}, status_code=500)
    store.set_container(cid, container_id, novnc, "running")
    manager.rebuild()
    return {"ok": True}


@app.post("/api/channels/{cid}/stop")
def stop(cid):
    manager.stop(cid)
    store.set_status(cid, "stopped")
    return {"ok": True}


@app.delete("/api/channels/{cid}")
def delete(cid):
    manager.remove(cid)
    store.del_channel(cid)
    manager.rebuild()
    return {"ok": True}


@app.get("/api/preflight")
def preflight_check(vpn_type: str = None, version: str = None, scope: str = "preflight"):
    mirrors = [m["host"] for m in store.list_mirrors() if m["enabled"]]
    # 只有 full(独立诊断屏)才需要 mihomo 状态;向导 gate(preflight)不用,别白跑 3s 探活
    mihomo_alive = manager.mihomo_alive() if scope == "full" else None
    return preflight.run_checks(manager.dc, vpn_type, version, scope=scope,
                                mirrors=mirrors, mihomo_alive=mihomo_alive)


@app.post("/api/preflight/fix/{action}")
def preflight_fix(action: str, body: dict = Body(default={})):
    if action == "create_network":
        name = body.get("name") or manager.VPN_NET
        try:
            manager.dc.networks.get(name)
        except docker.errors.NotFound:
            manager.dc.networks.create(name, driver="bridge")
        return {"ok": True}
    if action == "pull_image":
        image = body.get("image", "")
        repo = image.split(":", 1)[0]
        if repo not in preflight.known_repos() or preflight.is_buildable(image):
            return JSONResponse({"error": "image not pullable"}, status_code=400)
        mirrors = [m["host"] for m in store.list_mirrors() if m["enabled"]] or None
        tid = preflight.start_pull(manager.dc, image, registry.host_arch(), mirrors=mirrors)
        return {"task_id": tid}
    return JSONResponse({"error": "unknown action"}, status_code=400)


@app.get("/api/preflight/fix/{task_id}")
def preflight_fix_status(task_id: str):
    st = preflight.get_task(task_id)
    if st is None:
        return JSONResponse({"error": "unknown task"}, status_code=404)
    return st


@app.get("/api/images")
def images_inventory():
    mirrors = [m["host"] for m in store.list_mirrors() if m["enabled"]]
    return preflight.image_inventory(manager.dc, registry.host_arch(), mirrors)


@app.get("/api/system")
def system():
    ctrl_port = os.environ.get("MIHOMO_CTRL_PORT", "")
    return {
        "mihomo_status": "running" if manager.mihomo_alive() else "down",
        "mihomo_port": int(os.environ.get("MIHOMO_HOST_PORT") or 0) or None,
        "controller": f"127.0.0.1:{ctrl_port}" if ctrl_port else None,
        "ui_port": int(os.environ.get("UI_PORT") or 0) or None,
        "bound_ip": "127.0.0.1",
    }


@app.get("/api/channels/{cid}/logs")
def channel_logs(cid, tail: int = 200):
    return {"lines": manager.logs(cid, tail)}


@app.get("/api/connections")
def api_connections():
    return manager.connections()


@app.get("/api/proxies")
def api_proxies():
    return {"proxies": manager.proxies()}


def _bare(p):
    for pre in ("+.", "*."):
        if p.startswith(pre):
            return p[len(pre):]
    return p


@app.get("/clash/vpn-rules.yaml", response_class=PlainTextResponse)
def clash_provider():
    lines = ["payload:"]
    for r in store.all_rules():
        if not r["enabled"]:
            continue
        if r["kind"] == "ip":
            lines.append(f"  - IP-CIDR,{r['pattern']}")
        else:
            lines.append(f"  - DOMAIN-SUFFIX,{_bare(r['pattern'])}")
    return "\n".join(lines) + "\n"


@app.get("/api/clash-snippet", response_class=PlainTextResponse)
def clash_snippet():
    rules = [r for r in store.all_rules() if r["enabled"]]
    L = [
        "# ① 在你现有 Clash 的 proxies: 下加这个节点",
        "proxies:",
        "  - name: vpn-router",
        "    type: socks5",
        "    server: 127.0.0.1",
        f"    port: {MIHOMO_HOST_PORT}",
        "",
        "# ② 方式甲(推荐):订阅一份规则,绑新域名/IP 自动同步,之后不再动 Clash",
        "rule-providers:",
        "  vpn-rules:",
        "    type: http",
        "    behavior: classical",
        "    format: yaml",
        f"    url: http://127.0.0.1:{os.environ.get('UI_PORT','<UI端口>')}/clash/vpn-rules.yaml",
        "    interval: 60",
        "    path: ./providers/vpn-rules.yaml",
        "# rules: 顶部加一行引用(no-resolve 对清单内 IP-CIDR 生效)",
        "  - RULE-SET,vpn-rules,vpn-router,no-resolve",
        "",
        "# ② 方式乙:直接内联(不想用 provider 时)",
    ]
    if rules:
        for r in rules:
            if r["kind"] == "ip":
                L.append(f"  - IP-CIDR,{r['pattern']},vpn-router,no-resolve")
            else:
                L.append(f"  - DOMAIN-SUFFIX,{_bare(r['pattern'])},vpn-router,no-resolve")
    else:
        L.append("  # (还没绑定任何规则)")
    L += [
        "",
        "# ③ 无 Clash 时:把系统/浏览器代理指向 127.0.0.1:" + str(MIHOMO_HOST_PORT),
        "#    本工具 mihomo 自身分流:命中→VPN 容器,其余→直连。",
    ]
    return "\n".join(L)


@app.get("/entry/proxy.pac")
def entry_pac():
    """自动代理 PAC:命中 enabled 的域名/IP 走入口 SOCKS5,其余 DIRECT。
    经典 PAC 的 isInNet 仅支持 IPv4,IPv6 网段在此略过(可改用域名或其它接入方式)。"""
    port = MIHOMO_HOST_PORT
    proxy = f"SOCKS5 127.0.0.1:{port}; SOCKS 127.0.0.1:{port}; DIRECT"
    domains, nets = [], []
    for r in store.all_rules():
        if not r["enabled"]:
            continue
        if r["kind"] == "domain":
            d = _bare(r["pattern"]).strip().lower()
            if d:
                domains.append(d)
        else:
            try:
                net = ipaddress.ip_network(r["pattern"], strict=False)
            except ValueError:
                continue
            if net.version == 4:
                nets.append((str(net.network_address), str(net.netmask)))
    dom_js = ",".join('"%s"' % d for d in domains)
    net_js = ",".join('["%s","%s"]' % (a, m) for a, m in nets)
    pac = (
        "// 本工具自动生成 · 命中客户域名/IP 走入口,其余 DIRECT\n"
        'var PROXY = "' + proxy + '";\n'
        "var DOMAINS = [" + dom_js + "];\n"
        "var NETS = [" + net_js + "];\n"
        "function FindProxyForURL(url, host) {\n"
        "  host = (host || '').toLowerCase();\n"
        "  for (var i = 0; i < DOMAINS.length; i++) {\n"
        "    var d = DOMAINS[i];\n"
        "    if (host === d || host.slice(-(d.length + 1)) === '.' + d) return PROXY;\n"
        "  }\n"
        "  if (/^\\d+\\.\\d+\\.\\d+\\.\\d+$/.test(host)) {\n"
        "    for (var j = 0; j < NETS.length; j++) {\n"
        "      if (isInNet(host, NETS[j][0], NETS[j][1])) return PROXY;\n"
        "    }\n"
        "  }\n"
        "  return 'DIRECT';\n"
        "}\n"
    )
    return PlainTextResponse(pac, media_type="application/x-ns-proxy-autoconfig")


@app.get("/api/entry/setup-commands")
def entry_setup_commands():
    """各平台把流量指向入口的一键命令(开/关),供「入口接入」屏渲染。"""
    port = MIHOMO_HOST_PORT
    ui = os.environ.get("UI_PORT", "")
    pac_url = f"http://127.0.0.1:{ui}/entry/proxy.pac" if ui else "/entry/proxy.pac"
    return {
        "port": port,
        "pac_url": pac_url,
        "macos": {
            "socks_on": f"networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 {port}",
            "socks_off": "networksetup -setsocksfirewallproxystate Wi-Fi off",
            "pac_on": f"networksetup -setautoproxyurl Wi-Fi {pac_url}",
            "pac_off": "networksetup -setautoproxystate Wi-Fi off",
        },
        "windows": f"设置 → 网络和 Internet → 代理 → 手动设置代理填 127.0.0.1:{port};或「使用安装脚本」填 PAC URL",
        "env": {
            "socks": f"export ALL_PROXY=socks5h://127.0.0.1:{port}",
            "http": f"export HTTPS_PROXY=http://127.0.0.1:{port} HTTP_PROXY=http://127.0.0.1:{port}",
            "unset": "unset ALL_PROXY HTTPS_PROXY HTTP_PROXY",
        },
    }


@app.get("/api/mirrors")
def mirrors_list():
    return store.list_mirrors()


@app.post("/api/mirrors")
def mirrors_add(body: dict = Body(...)):
    host = (body.get("host") or "").strip()
    if not host:
        return JSONResponse({"error": "host required"}, status_code=400)
    try:
        mid = store.add_mirror(host)
    except sqlite3.IntegrityError:
        return JSONResponse({"error": "mirror already exists"}, status_code=400)
    return [m for m in store.list_mirrors() if m["id"] == mid][0]


@app.patch("/api/mirrors/{mid}")
def mirrors_patch(mid: int, body: dict = Body(...)):
    store.set_mirror(mid, priority=body.get("priority"), enabled=body.get("enabled"))
    return {"ok": True}


@app.delete("/api/mirrors/{mid}")
def mirrors_del(mid: int):
    store.del_mirror(mid)
    return {"ok": True}


@app.post("/api/mirrors/test")
def mirrors_test(body: dict = Body(...)):
    host = (body.get("host") or "").strip()
    t0 = time.monotonic()
    try:
        requests.get(f"https://{host}/v2/", timeout=5)
        return {"reachable": True, "latency_ms": int((time.monotonic() - t0) * 1000)}
    except Exception:
        return {"reachable": False, "latency_ms": None}


# 静态前端:把 app/static/ 整目录挂到根。上面的 /api/* 路由先注册、优先匹配;
# 其余路径(/css、/js、/*.html)由这里服务,html=True 让 "/" 返回 index.html。
#
# Cache-Control: no-cache —— StaticFiles 默认只发 etag/last-modified、不发 Cache-Control,
# 浏览器据此做「启发式缓存」:在新鲜窗口内不回源、直接吃旧副本。改了 HTML/JS 却看到旧界面
# (如某屏侧栏少了刚加的入口)就是这个坑。no-cache 强制每次带 If-None-Match 回源校验,
# 未变则 304(仍高效),变了立刻拿新版——本工具永远不该看到陈旧前端。
class _NoCacheStatic(StaticFiles):
    async def get_response(self, path, scope):
        resp = await super().get_response(path, scope)
        resp.headers["Cache-Control"] = "no-cache"
        return resp


app.mount("/", _NoCacheStatic(directory=os.path.join(HERE, "static"), html=True), name="static")
