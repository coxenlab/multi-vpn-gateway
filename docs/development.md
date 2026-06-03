# 架构与开发约定

> 本文是代码现状、架构与「绝不能破坏的命门」的速查。完整设计意图见 [design.md](./design.md);跑起来 / 测试 / 提交约定见仓库根的 [README](../README.zh-CN.md) 与 [CONTRIBUTING](../CONTRIBUTING.md)。

## 这是什么

跑在本机的可视化 VPN 管理网关。每家企业 VPN 各关进一个 Docker 容器,每个容器暴露一个 SOCKS5 出口;一个独立的第二个 mihomo 实例按域名 / IP 分流;用户现有的 Clash 一字不改,只加一个 `vpn-router` 节点 + 订阅一份分流规则。全程全 Docker,本机零新增依赖。

## 仓库结构

```
.
├── docker-compose.yml          # mihomo + app 两服务;端口全绑 127.0.0.1
├── start.sh                    # 一键启动:gen_env → 渲染 mihomo 配置 → compose up
├── gen_env.py                  # 生成 .env(随机高位端口 + mihomo 密钥)
├── mihomo/
│   └── config.template.yaml    # mihomo 初始配置模板(__SECRET__ 占位)— 入库
│                               # config.yaml / cache.db 为运行态,已 gitignore
├── images/
│   ├── oss/                    # 自建多客户端镜像 vpnmgr/oss-vpn(oss 家族共用)
│   │   ├── Dockerfile          # Debian + openconnect/openfortivpn/openvpn/wireguard + dante
│   │   ├── entrypoint.sh       # 等隧道接口起来后 exec danted 占 PID1
│   │   └── sockd.conf.tmpl     # dante egress 模板(external: <tun> pin 到隧道)
│   └── byo/                    # 自建桌面镜像 vpnmgr/byo-desktop(byo 兜底)
│       ├── Dockerfile          # Debian + Xvfb/fluxbox/x11vnc/noVNC(8080)+ microsocks(1080)
│       └── entrypoint.sh       # 起桌面 + microsocks;用户经 noVNC 手动装任意 Linux VPN GUI
├── app/                        # FastAPI 后端
│   ├── main.py                 # 路由 + 末尾静态前端挂载
│   ├── manager.py              # Docker 编排 + mihomo 热加载 + SOCKS5 探活 + oss_connect + put_file
│   ├── store.py                # SQLite + Fernet 凭据加密(password_enc + config_json 字段级)
│   ├── adapters.yaml           # 适配器注册表(声明式,hagb + oss + byo 三家族)
│   ├── registry.py             # 加载 adapters.yaml;get(key) / list_adapters() / host_arch()
│   ├── adapters.py             # runtime 分派表(_build_hagb / _build_oss / _build_byo)
│   ├── dockerhub.py            # 实时拉取 EC 版本 tag(过滤 + arch 标记 + 缓存 + 离线兜底)
│   └── static/                 # 前端(已接真实 API:js/api.js 封装 fetch)
└── tests/                      # pytest 单测(独立于运行镜像)+ smoke.sh 栈冒烟
```

## 三层架构

流量自外向内:

```
你现有的 Clash ──(命中分流规则)──▶ vpn-router 节点
                                        │
              第二个 mihomo(本工具)── 按 域名 / IP 分流 ──▶ ch-1 / ch-2 / …
                                        │
        每家 VPN 一个容器(ch-{id})── EC / aTrust / openconnect / … ──▶ SOCKS5 出口 ──▶ 客户内网
```

无 Clash 时也可用「入口接入」:把系统 / 浏览器代理指向本工具 mihomo(`/entry/proxy.pac` 或 `/api/entry/setup-commands` 给出各平台一键命令),命中规则走 VPN、其余直连。

## 适配器层(`adapters.yaml` + `registry.py` + `adapters.py`)

`adapters.py` 是 runtime 分派表:`_BUILDERS = {"hagb": _build_hagb, "oss": _build_oss, "byo": _build_byo}`,`build_run_kwargs(ch, spec, vnc_pwd, vpn_net)` 按 `spec["runtime"]` 合成 `docker run` 入参(纯函数);未知 runtime 抛 `ValueError`。三家族:

- **hagb**(交互 / 有头):EasyConnect + aTrust,上游 `hagb/docker-easyconnect|atrust` 镜像,经 noVNC 登录,`login_modes` 含 `gui`。
- **oss**(无头):openconnect 系(anyconnect / globalprotect / fortinet-oc / juniper(nc)/ pulse)+ openfortivpn + openvpn + wireguard,共用自建镜像 `vpnmgr/oss-vpn:latest`(`images/oss/`),`login_modes: [headless]`、无 noVNC。每协议一条 manifest(固定 `protocol` 字段),共用 `_build_oss`;字段由 manifest `inputs` 驱动(含 `type: file` 的 `.ovpn` / wg `.conf`,标 `secret: true`)。
- **byo**(兜底 / 有头):`custom` 类型,一台 Xvfb+fluxbox+x11vnc+noVNC(8080)+ microsocks(1080)桌面容器(自建镜像 `vpnmgr/byo-desktop:latest`,`images/byo/`),`login_modes: [byo]`。用户经现有 noVNC 登录流在桌面里手动装任意 Linux VPN GUI 客户端并登录;无 connect、无凭据注入。`_build_byo` 镜像 `_build_hagb`(保留 host noVNC 端口),caps / devices 走 manifest(NET_ADMIN+MKNOD + `/dev/net/tun`)。

## 通道状态机(后端 canonical)

```
creating ──▶ running ──▶ logged_in        (另有 stopped、error)
```

- **running** = 容器起来了但还没登录成功(待登录)。
- **logged_in** = SOCKS5 探活通过(真连上内网)。

## 命门(开发中绝不能破坏)

1. **登录成功的唯一判据 = 后端 SOCKS5 探活**(`manager.probe`:经 `socks5h://vpn-{id}:1080` 访问 `probe_url`,`socks5h` = 远程解析)。**绝不能用「VNC 连上了」判定登录成功**(跨源读不到 VNC 事件)。
   - **oss**:无 VNC,登录成功仍只认 SOCKS5 探活;`GET /api/channels/{cid}/login` 对 headless 返回 `{login_mode:"headless"}`,前端据此跳过登录屏。
   - **byo**:无 connect、无「VNC 连上 / 安装完成即成功」信号;状态机不变(起容器落 `running`,探活通过才升 `logged_in`)。`login` 对 byo 返回与 EC/aTrust 相同的 noVNC `vnc.html` url(复用 gui 分支)。

2. **DNS 在 VPN 侧解析**:外层用户 Clash 的规则带 `no-resolve`(不解析、直接把域名交给 `vpn-router`);内层本工具 mihomo 靠 sniffer / respect-rules 还原域名。这是 `rebuild()` 里 `DOMAIN-SUFFIX` 规则不带 `no-resolve` 也能命中的原因。

3. **配置热加载、绝不断连**:`manager.rebuild()` 重写 mihomo 配置后 `PUT {CTRL}/configs?force=true`,不重启 mihomo、不断现有连接。

4. **所有 host 端口只绑 `127.0.0.1`**(compose + manager 均如此),永不 `0.0.0.0`。
   - **oss**:1080 不映射 host(`_build_oss` 无 `ports` 项),仅 docker 内网 `vpn-{id}:1080` 可达;egress 由 dante `external: <tun>` pin 到隧道。
   - **byo**:1080 不映射 host(microsocks 仅 docker 内网可达),noVNC(8080)映射到 127.0.0.1 随机高位。

5. **凭据安全**:密码 Fernet 加密落库(`store.py`),`_row()` 永不把 `password_enc` 与任何 secret 字段回传前端;`master.key` 权限 0600,存在数据卷里。
   - **oss**:`config_json` 列承载 per-adapter 参数,`secret:true` 字段(密码 / `.ovpn` / wg `.conf` 含私钥)字段级 Fernet 加密;凭据 / 私钥经 `manager.oss_connect` 的 `exec_run(stdin=True, socket=True)` 注入,**绝不进命令行**(`ps` 不可见)。
   - **byo**:上传安装器经 `POST /api/channels/{cid}/upload`(multipart)→ `manager.put_file` in-memory tar → `container.put_archive` 落数据卷,**绝不进 SQLite、绝不回传前端**;`config_json` 只存非密文件名引用(供前端展示已装包名)。

6. **容器细节**:SOCKS5(1080)只在 Docker 内网暴露;noVNC(8080)映射到 127.0.0.1 随机高位端口。aTrust 容器需 `sysctl net.ipv4.conf.default.route_localnet=1`;`DISABLE_PKG_VERSION_XML=1` 两种 hagb 类型均需。EasyConnect 镜像 tag 由 `dockerhub.py` 实时拉取(非硬编码),`ec_ver` 存用户选定的 tag。

7. **代理命名**:外层用户 Clash 里那个节点叫 `vpn-router`(= 整个 mihomo 实例的分流端口);内层 mihomo 里每条通道是 `ch-{id}` 的 socks5 代理。别混淆。

## HTTP API

以 `app/main.py` 为唯一事实源,完整端点表见 [README](../README.zh-CN.md#http-api)。要点:

- 分流规则走 `POST /api/channels/{cid}/rules`(`patterns[]` 或 `pattern`,可选 `kind: domain|ip`,裸 IP 自动补 `/32`、`/128`),`PATCH` / `DELETE .../rules/{rid}` 启停 / 删除单条。域名与 IP-CIDR 均已支持。
- `GET /clash/vpn-rules.yaml` 产出 `behavior: classical` 的 rule-provider 清单;`GET /api/clash-snippet` 给出 `vpn-router` 节点 + `RULE-SET,vpn-rules` 引用(或内联回退)。
- 改 API 必须同步改 README 的 API 表。

## 现状与下一步

- **已落地**:三家族适配器(hagb / oss / byo);域名 + IP-CIDR 分流规则、启停、rule-provider 与遥测;前端 5 屏接入真实 API(mock `data.js` 已移除);无 Clash 时的入口接入(PAC + 各平台一键命令);arm64 交互式 noVNC 登录。
- **TODO(Phase 4)**:逐家国产 VPN 厂商 GUI 的按需预装 / 程序化登录适配器。byo 是长尾兜底(任意客户端手动装),Phase 4 才是逐家专有客户端的一等适配(预装镜像 + 自动化登录)。
- **已推迟**:strongSwan / IKEv2——纯 IPsec 无具名 tun、dante 难 pin egress(须 route-based VTI/XFRMi),暂不收(见 `adapters.yaml` 末尾注释)。

### byo 失败模式(诚实标注,尽力而为)

适用于「自带 tun、纯网络认证」的普通 Linux GUI / CLI 客户端;**不支持** systemd/dbus 守护进程客户端(AnyConnect `vpnagentd`、GlobalProtect `panGPS`)、需 host 缺失内核模块、硬件令牌 / 智能卡 / TPM 绑定、仅 Windows/macOS 的客户端;split-tunnel 时「SOCKS = 全隧道」不成立。这类场景应优先用 hagb / oss 适配器。

## 前端设计系统

设计系统 **Neutral Modern**:浅色干净、钴蓝 `#2F6FEB` 点缀(每屏至多一处强调)、Inter(sans 作 display)、B2B 工具 / 操作台风格(不是落地页)。所有 token 在 `app/static/css/app.css` 的 `:root`;别在别处写裸 hex;不要 AI-slop(紫色渐变、emoji 图标、左边框卡片、给每个标题配图标等)。
