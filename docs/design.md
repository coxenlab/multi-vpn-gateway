# 容器化企业 VPN 可视化管理网关 — 落地方案

> 面向单人(你自己)、跑在本机 macOS(已实测 Docker 29.3.1 / API 1.54 / arm64)或一台常开小主机。硬约束:不影响你现有 Clash 的正常使用。

> **历史快照**:本文是实现前的原始设计 / 落地方案,记录设计意图与权衡。部分技术选型已与最终实现不同(如前端实为原生 JS 而非 React/AntD;主密钥实为数据卷上的 `master.key` 而非 macOS 钥匙串)。当前实际架构以 [development.md](./development.md) 为准。

---

## 1. 一句话方案概述

**你现有的 Clash 留在最前面当总分发器(系统代理/TUN 一字不改),只把"被绑定的客户域名"用一个节点指给本工具自己的第二个 mihomo 实例;这个 mihomo 按域名把每个客户域名分流到对应的 docker-easyconnect / docker-atrust 容器的 SOCKS5 出口;一个自研的薄 Web 管理层负责容器起停、引导登录(含短信/SSO 的 noVNC 交互登录)、域名↔通道映射,把映射落成 mihomo 的 provider 文件后用 REST API 热加载——日常加删域名不重启、不断现有连接。**

---

## 2. 总体架构

### 2.1 组件图(文字版,全部 bind 127.0.0.1)

```
┌──────────────────────────── macOS 主机 / 常开小主机 ────────────────────────────┐
│                                                                                  │
│  系统代理 / TUN ───▶ [你现有的 Clash]   ← 唯一前置分发器,保持现状               │
│                          │                                                       │
│                          │ rule: 命中客户域名 → 节点 "vpn-router" (no-resolve)   │
│                          │ rule: 其余流量 → 你原有逻辑 (机场/DIRECT/广告拦截)    │
│                          ▼                                                        │
│              127.0.0.1:7899  [本工具 mihomo]  mixed-port  ← 第二跳, VPN 选择器    │
│                  ├ external-controller 127.0.0.1:9090 (secret=随机强密码)        │
│                  ├ dns: respect-rules + sniffer  ← 内网域名走 VPN 侧解析(命门)   │
│                  ├ rule-provider 通道A(domain) → RULE-SET → 节点 chanA-socks      │
│                  ├ rule-provider 通道B(domain) → RULE-SET → 节点 chanB-socks      │
│                  └ MATCH → DIRECT  ← 兜底直连,绝不回环到你的 Clash(防环路)      │
│                          │                    │                                  │
│                          ▼                    ▼                                  │
│              127.0.0.1:10800           127.0.0.1:10801   ← 端口分配器按通道错开   │
│              [容器A: EC]               [容器B: aTrust]                            │
│               socks5/http/noVNC         socks5/http/noVNC                         │
│               /root 卷持久化            /root 卷持久化                            │
│                          │                    │                                  │
│                          ▼                    ▼                                  │
│                   客户A 企业内网          客户B 企业内网                          │
│                                                                                  │
│  [编排后端 :8000] ──docker.sock──▶ Docker Engine (起停/exec/logs)               │
│       └── Bearer secret ──▶ 本工具 mihomo :9090 (热加载/探活/连接)  ← 唯一客户端  │
│       └── HTTP 暴露 /clash/vpn-domains.yaml  ← 你的 Clash 订阅这个 URL 自动同步    │
│  [Web UI] (React)  + [监控子页: 嵌 metacubexd, external-ui 指向 :9090]            │
└──────────────────────────────────────────────────────────────────────────────────┘
```

### 2.2 端到端数据流(访问一个客户内网域名时)

```
浏览器访问 crm-a.com
   │
   ▼ 系统代理
[你的 Clash] ── 命中 vpn-domains 规则(no-resolve, 不本地解析) ──▶ 节点 vpn-router
   │                                                              (= socks5 → 127.0.0.1:7899)
   ▼
[本工具 mihomo :7899] ── 域名 crm-a.com 命中 RULE-SET,chanA-domains ──▶ 节点 chanA-socks
   │                                                                    (= socks5 → 127.0.0.1:10800)
   ▼
[容器A SOCKS5 :10800] ── DNS 在 EC 隧道侧解析(关键)── EC 隧道 ──▶ 客户A 内网 ✅

未命中 vpn-domains 的流量 ── 在你的 Clash 内直接走原有逻辑 ──▶ 正常上网 ✅ (工具完全不介入)
```

**关键点(调研最初遗漏、已补正)**:mihomo 默认会在主机**本地解析域名**再交给 SOCKS5 节点。客户内网域名通常只在 VPN 侧 DNS 才能解析,本地解析会失败/解析错。两侧都要正确设置:
- **你的 Clash 侧**:客户域名规则带 `no-resolve`,把域名**原样**透传给 `vpn-router`,不抢先解析。
- **本工具 mihomo 侧**:开 `dns.respect-rules: true` + `sniffer`,目标以域名而非预解析 IP 交到容器 SOCKS5,由 EC/aTrust 隧道侧解析。
- 这是整条链路最容易翻车的地方,**每个客户上线时必须实测内网域名真的能打开**。

---

## 3. 复用什么 / 自研什么

| | 组件 | 用途 |
|---|---|---|
| **复用** | `hagb/docker-easyconnect` | EC 通道容器:SOCKS5(1080)/HTTP(8888)/noVNC(8080),`/root` 卷持久化登录态 |
| **复用** | `hagb/docker-atrust:latest`(arm64 原生) | aTrust 通道容器(独立镜像,非 tag) |
| **复用** | mihomo(Clash.Meta)核心 | 第二跳分流引擎,external-controller REST API 热加载 |
| **复用** | metacubexd(iframe) | 实时流量/连接/延迟/规则监控子页,省自研 |
| **复用** | docker-py 7.1.0 | Python 直连 Docker Engine API |
| **复用(后期)** | `kenvix/aTrustLogin` | aTrust cookie 复用免验证码 + TOTP + KeepAlive |
| **自研** | 编排后端(FastAPI) | 通道 CRUD、端口分配、容器起停、登录态轮询、生成 mihomo provider 文件并热加载、连通性探测、凭据加密 |
| **自研** | Web UI(React) | 主用户故事工作流:新建通道→填凭据→noVNC 登录→绑域名→状态视图 |
| **自研** | 通道配置→docker run 翻译层 | 按 type 选镜像、按登录方式拼参数 |
| **自研** | 双信号登录判定 | SOCKS5 探活(主)+ noVNC desktopname(辅) |
| **不用** | NPM / gluetun / sub-store / Tailscale / headscale / Portainer | 均已核实不适配;Portainer 对 MVP 单机纯增重,直连 docker.sock |

**核心缺口只有中间那层薄编排**:把通道+域名映射存成自己的数据模型,生成 mihomo proxies/rules 片段,写盘后调 REST API 热加载。现成的 mihomo 面板只能切节点/看规则,没有"新建通道→填凭据→绑定域名"的编辑工作流。

---

## 4. 技术栈选型与理由

| 层 | 选型 | 理由(基于已验证事实) |
|---|---|---|
| 后端 | **Python 3.11+ / FastAPI** | docker-py 7.1.0 官方维护,`exec_run`/`logs`/事件流齐全(健康检查必需);FastAPI 异步 + WebSocket 转发 mihomo `/traffic`、`/connections`。**注意**:本机系统 Python 是 3.9.6,需独立装 3.11+(uv/venv),不要用系统 Python。 |
| 容器编排 | **docker-py 直连 Engine API** | 不引入 Portainer。`base_url` 用默认即可(本机 `/var/run/docker.sock` 是指向 `~/.docker/run/docker.sock` 的可用软链,已实测);**跨环境保险起见**可显式传 `unix:///Users/<user>/.docker/run/docker.sock`。 |
| 分流引擎 | **独立 mihomo 实例** | 后端托管一个 mihomo 子进程 + 独立 config 目录,不复用你的 Clash 进程。 |
| 前端 | **React + Vite + TS + Ant Design** | 表单驱动(建通道/绑域名)是 AntD 强项;`<iframe>` 嵌 noVNC 与 metacubexd 都是原生 DOM,无需重型可视化库。 |
| 监控子页 | **嵌 metacubexd**(iframe → :9090) | 实时流量/连接/延迟/节点切换直接复用,不自研监控图表。 |
| 凭据加密 | **字段级 Fernet(cryptography)** + 主密钥存 macOS Keychain(`keyring`) | MVP 单人足够;团队版再升 SQLCipher 全库。 |
| 数据库 | **SQLite** | 单人单机,零运维。 |

---

## 5. 四个核心流程设计

### 5.1 建通道

```
1. UI 提交通道表单(name / vpn_type / server / login_method / 凭据)
2. 后端:
   - 生成短 id → 派生 fake_hwaddr(稳定)、端口(socks=10800+i, novnc=18080+i...)、volume_name
   - 凭据 Fernet 加密存 SQLite,主密钥在 Keychain
   - 落库 status=created(此时不起容器)
3. UI 显示"启动"按钮 → POST /start → 翻译层拼 docker 参数 → containers.run
```

**通道配置 → docker run 翻译层(核心自研)**:

```python
# 公共参数(所有通道)
devices=['/dev/net/tun:/dev/net/tun:rwm']           # 本机已实测可建 TUN
cap_add=['NET_ADMIN']                                # 本机已实测生效
restart_policy={'Name': 'unless-stopped'}
volumes={volume_name: {'bind': '/root', 'mode': 'rw'}}
ports={'1080/tcp': ('127.0.0.1', socks_port),
       '8080/tcp': ('127.0.0.1', novnc_port)}
hostname=channel_id                                  # 红队建议:固定 hostname, 消除又一易变标识
environment 含: FAKE_HWADDR=<该通道MAC>, EXIT="" (自动重连),
              PING_ADDR=<内网可达地址> (防无流量踢线)

# 按 type + 登录方式分发
EC + password    : image='hagb/docker-easyconnect:cli', EC_VER='7.6.7',
                   CLI_OPTS="-d {server} -u {user} -p {pass}"   # 无头, 跳过登录步骤
                   ⚠️ cli 镜像仅 amd64 → 本机 arm64 走 QEMU(慢) 或退回 GUI 镜像
EC + interactive : image='hagb/docker-easyconnect', USE_NOVNC=1, PASSWORD={vnc_password}
aTrust (只能交互): image='hagb/docker-atrust:latest', USE_NOVNC=1, PASSWORD={vnc_password},
                   sysctls={'net.ipv4.conf.default.route_localnet': '1'},
                   DISABLE_PKG_VERSION_XML=1,           # arm64 原生客户端冻结, 需绕版本校验
                   暴露 54631
```

> ⚠️ **VNC 密码必须 ≤8 字符**:已核实 `start.sh` 用 `tigervncpasswd -f`,tigervnc 密码只取前 8 字节(超长静默截断),否则 noVNC 的 `autoconnect&password=` 对不上,你会卡在密码框。

### 5.2 交互式登录(短信 / SSO / 验证码 / 设备指纹)

```
1. 后端起容器(USE_NOVNC=1)后返回 novnc_url
2. UI 用 iframe 嵌入(已核实 tinyproxy 无 X-Frame-Options/CSP, HTTP 层可嵌):
   http://127.0.0.1:{novnc_port}/vnc.html?path=websockify&autoconnect=true&resize=remote&password={vnc_password}
   (path=websockify 对应 tinyproxy 的 ReversePath /websockify, 已逐字核对一致)
3. 你在 iframe 里看到深信服登录页 → 输入短信码/扫码/SSO
4. 后端轮询登录态(双信号判定 ↓)
```

**登录成功判定 = 双信号,绝不能只看 VNC 连上**(VNC 连上的只是登录页):

```
主判据(唯一可靠): 后端 exec_run 在容器内跑
   curl --socks5-hostname 127.0.0.1:1080 <该通道内网探测URL>
   返回成功 → 真登录上(SOCKS5 隧道真通)
辅判据(可选, 更快反馈): noVNC desktopname 事件变化
```

> 🔴 **跨源陷阱(红队发现)**:noVNC 跑在容器端口、管理 UI 跑在另一端口 → **不同源**,父页面 JS **读不到** iframe 内的 RFB 事件,且 noVNC 无 postMessage 跨源桥。所以"监听 VNC 事件判断登录成功"在默认部署下**直接不可用**。MVP 一律以**后端 SOCKS5 探活**为准。想要 VNC 事件辅助,得把 noVNC 反代到与 UI 同源(v2,不值得在 MVP 做)。

### 5.3 绑定域名热生效(核心用户故事的"绑域名→自动走")

```
1. UI 给通道追加域名 pattern(如 +.crm-a.com)→ POST /channels/{id}/domains
2. 后端追加到该通道的 rule-provider 文件 ./prov/chanA.yaml
3. 后端调 PUT /providers/rules/chanA-domains  (内部触发 provider.Update() 重读文件)
   → 纯热加载, 即时生效, 不重启 mihomo, 不断现有连接
4. 后端同步更新 /clash/vpn-domains.yaml(并入全局清单)
5. 你的 Clash 靠 rule-provider 的 interval 自动拉取生效 → 零打扰
```

**两类热加载(已源码级核实)**:

| 操作 | 机制 | 是否断连 |
|---|---|---|
| 日常加/删域名 | 改 `./prov/chanX.yaml` → `PUT /providers/rules/{ch}` | **不断**(只重读文件) |
| 新建整条通道(新节点+新 rule-provider 块) | 重写 `config.yaml` → `PUT /configs?force=true` | **不断**(executor 无 `closeAllConnections`) |

> 🔴 **mihomo 没有"增删单个节点/单条 rule"的 REST 端点**,且维护者明确把"用 API 改本地配置文件"列为重大攻击面并拒绝。**文件 + provider-reload 是唯一支持路径,别去找别的 API。** 也别依赖 file-watch 自动重载(有 issue 报告 mtime 不一定触发),每次编辑后**显式调** reload。
> 想让旧连接立刻改走新通道,可选 `DELETE /connections/:id` 主动掐(MVP 可不做,等连接自然重建)。

### 5.4 状态健康检查

```
没有原生"是否登录"API → 自己用内网探测 URL 推导:

容器层(SDK healthcheck): 容器内 curl socks5 基础存活
应用层(后端轮询):
   - GET /providers/proxies/{name}/healthcheck?url=<内网探测URL>&timeout=5000  → 隧道活性/延迟
   - 或后端 exec_run curl --socks5-hostname 判"隧道真通"
   - GET /connections → 哪个域名当前走哪条通道
   - GET /proxies → 节点是否 alive

状态机: created → starting → running(容器起来,SOCKS5端口监听) → logged_in(探测真通) → down/stopped
注意: 纯 TCP 端口监听 ≠ VPN 连上(那只代表 danted 起来了)
```

---

## 6. 数据模型与凭据安全

```
channel  (通道 = 一个 VPN 容器)
  id            TEXT PK       # 短 id, 派生 MAC/端口/卷名/hostname 都用它
  name          TEXT
  vpn_type      TEXT          # easyconnect | atrust
  server        TEXT          # VPN 网关地址
  login_method  TEXT          # password | interactive
  ec_ver        TEXT NULL     # EC cli 必填版本, 如 7.6.7
  fake_hwaddr   TEXT          # 建通道时生成一次, 落库, 重建/迁移复用 → 不触发重绑
  socks_port    INT           # 主机端口, 分配器给, 绑 127.0.0.1
  novnc_port    INT
  vnc_password  TEXT(enc)     # ≤8 字符!
  volume_name   TEXT          # 每通道独立 docker volume → /root
  container_id  TEXT NULL
  status        TEXT          # created|starting|running|logged_in|down|stopped
  created_at / updated_at

credential  (与 channel 1:1, 单独表便于整表加密)
  channel_id    TEXT FK
  username      TEXT(enc)
  password      TEXT(enc)     # 仅 password 类注入 CLI_OPTS; interactive 类可空

domain_binding  (域名 ↔ 通道, N:1)
  id            INT PK
  channel_id    TEXT FK
  pattern       TEXT          # mihomo domain provider 行: +.intranet-a.com / *.foo.com
  enabled       BOOL
```

**凭据安全(MVP 可落地、不过度)**:
- `cryptography` 的 Fernet 对 `password`/`username`/`vnc_password` 字段级加密。
- 主密钥存 **macOS Keychain**(`keyring` 库),不落盘明文、不进 git、不进 SQLite。
- 数据库文件权限 `600`;明文凭据不进日志、不回显。

---

## 7. 关键风险与对策(吸收红队结论)

| # | 风险 | 是否成立 | 对策 |
|---|---|---|---|
| 1 | noVNC 登录 + 自动检测成功 | 登录✅;**检测有跨源陷阱** | 登录成功**一律靠后端 SOCKS5 探活**,不依赖任何 VNC 前端事件;VNC 密码≤8 字符;iframe 用 `path=websockify&autoconnect=true` |
| 2 | **重建容器触发重新设备绑定/被风控**(最被低估) | **风险真实存在** | 见下方详述 |
| 3 | 热加载断现有连接 | **不成立(不会断)** | 源码核实:`OnSuspend` 只翻状态位、`UpdateProxies` 只换 map、无 `closeAllConnections`。域名映射用 file rule-provider 热加;可选 `DELETE /connections` 主动切 |
| 4 | 不影响现有 Clash | 成立,**DNS 是命门** | 你的 Clash 对客户域名规则带 `no-resolve` 透传;本工具 mihomo 开 `respect-rules`+`sniffer`;只给你粘 1 节点 + 订阅 1 个 rule-provider,**不程序化改写你的 Clash** |
| 5 | aTrust 本机能跑 / CLI 无头登录 | aTrust 带枷锁;**CLI 无头不通用** | aTrust 有 arm64 原生但需 `DISABLE_PKG_VERSION_XML=1`(服务端升级后可能需更新镜像,UI 要标注);cli 镜像仅 amd64;aTrust 无 CLI → **noVNC 交互登录设为默认主路径** |

**风险 2 详述(最该重视,且核心证据链是断的)**:
- 已读 `fake-hwaddr.c`:**只 hook 了 `ioctl(SIOCGIFHWADDR)`**,即只伪造 MAC。grep 整个 `docker-root/` 确认**不伪造** `/etc/machine-id`、磁盘序列号、DMI/product_uuid、CPU 信息。
- 调研反复引用的"Linux EC 硬件码 = MAC 的 MD5"出自 issue #246,但那**是提问者的猜测、标注为未解之问,maintainer 没有权威确认**。把它当成"固定 MAC 就锁死硬件码"的地基是最脆的假设。
- aTrust 设备指纹比 EC 严得多且不公开,`aTrustAgent` 同样只被 fake-hwaddr 包(只管 MAC)。
- **对策**:
  1. 接受"无法保证不重绑",设计成**可恢复流程**:每通道固定 `FAKE_HWADDR` + 独立 `/root` 卷 + 固定 `hostname`。
  2. **MVP 必须内建"重新登录"通路**(就是 noVNC iframe),当常规操作而非异常——即使触发重绑,你在界面过一次验证码即可,不卡死。
  3. **先验证再承诺**:上线前对你真实的那家 aTrust 做一次破坏性测试——登录成功 → 删容器 → 同 MAC/卷/hostname 重建 → 看是否要求重新验证。**这条实测结果直接决定"新建通道后免维护"这个核心卖点能不能成立**,也决定登录编排要不要把"重新过验证码"设计成一等公民。别把未证实的 MAC-MD5 当成果。

**安全边界(MVP)**:所有监听只 bind 127.0.0.1;mihomo `secret` 设随机强密码、`external-controller` 绑 127.0.0.1、`external-ui-url` 留空(已知 RCE 向);改 provider 文件由后端**直接操作本地文件系统**,绝不通过 controller API 暴露文件路径;Web UI 是 controller 唯一客户端。

---

## 8. MVP 范围 + 分阶段路线图

### MVP 一句话目标
能跑通"新建通道 → 起容器 → 后端探测到 VPN 真连上 → 绑 1 个域名 → 浏览器访问该域名走这条 VPN",全程不影响你现有 Clash。

### MVP 做
- 后端 FastAPI + docker-py + 托管 1 个 mihomo 子进程。
- 通道 CRUD + start/stop(EC `:cli` 账密无头 **和** EC/aTrust noVNC 交互登录,**两条登录路径都要**——aTrust 无无头登录、你主力含短信)。
- 域名绑定 → 写 rule-provider 文件 → 热重载生效。
- SOCKS5 探活连通性判定 + 简单状态页。
- 凭据 Fernet 加密 + Keychain 主密钥。
- noVNC iframe 交互登录组件 + 双信号(实为 SOCKS5 探活)登录判定。
- 端口分配器(108xx/180xx 按通道序号错开,绑 127.0.0.1)。
- 一次性生成"你的 Clash 要粘的节点 + rule-provider URL"。

### MVP 不做(留 v2+)
多租户/多用户、Portainer、自研监控图表(直接嵌 metacubexd)、kenvix/aTrustLogin 自动化、换机迁移工具、远程访问鉴权反代、TOTP 自动填、批量导入。

### 分阶段路线图

**阶段 0 — 地基打通(脚本验证,先于写 UI)** ⬅️ 可行性闸门,过不了不写 UI
- docker-py 起一个 EC `:cli` 容器(账密),`exec_run` curl socks5 探活通。
- 手起本工具 mihomo,手写 config(1 socks5 节点 + 1 file rule-provider),`PUT /providers/rules/{name}` 改文件热生效验证。
- 你的 Clash 手加 1 节点指向 7899,验证目标域名走通(含内网域名 DNS 在 VPN 侧解析)、其余流量不受影响、无环路。
- **🔴 同时做风险 2 的破坏性实测**(真实 aTrust:登录→删容器→同 MAC/卷/hostname 重建→是否重新验证)。
- **完成标准**:命令行全链路通(浏览器访问内网域名经 Clash→mihomo→容器成功),现有 Clash 正常;且拿到 aTrust 重建实测结论。

**阶段 1 — MVP(可视化最小闭环)**
- **完成标准(四条全绿)**:
  1. `docker run` 实测一个 EC 容器,`curl --socks5-hostname 127.0.0.1:108xx <内网URL>` 返回 200。
  2. UI 建通道 → start → 状态页在 ≤30s 内显示 `logged_in`。
  3. UI 给该通道绑 `+.intranet-a.com` → 不重启 mihomo、不断现有连接 → 浏览器(经你的 Clash)访问该域名命中该通道(metacubexd connections 里看到走对应节点)。
  4. 全程未修改你的 Clash 配置(除一次性那行节点);你原有上网不受影响。

**阶段 2 — 稳定性与体验**
- aTrust 接 `kenvix/aTrustLogin`(cookie 复用免验证码 + TOTP + KeepAlive)。
- 健康检查双层化、掉线自动重连可视化、`PING_ADDR` 保活防误报。
- 换机/重建迁移工具:打包 `<通道卷>` + `fake_hwaddr` + hostname 一起搬。
- 嵌 metacubexd 做正式监控子页。
- **完成标准**:容器重启后会话不丢、自动重连可见;aTrust 通道首登后(实测周期内)无需再过验证码;迁移到另一台主机后通道不触发重绑(以阶段 0 实测结论为准)。

**阶段 3 — 团队化预留(MVP 明确不做,架构留口)**
- 鉴权反代 + 多用户(通道归属/权限);字段级加密升 SQLCipher 全库。
- 同一域名绑多通道的冲突检测策略、批量导入导出。
- **完成标准**:多用户隔离可用、对外暴露有鉴权、无明文凭据落盘。

---

## 9. 需你拍板的决策点

1. **后端语言**:Python + docker-py(推荐,健康检查/事件流 API 最全)还是 Node + dockerode(本机有 node v22)?默认按 Python 开工。
2. **EC 账密通道在本机(arm64)怎么走**:EC `:cli` 镜像仅 amd64 → 要么走 QEMU 模拟(慢、偶发不稳),要么 EC 账密也退回 GUI 镜像 noVNC 登一次。你的 EC 客户多是账密还是短信?决定要不要为 arm64 上 cli 的可用性买单。(aTrust 本就走 GUI,无此问题)
3. **你的 Clash 接入方式**:订阅式 rule-provider(本工具暴露 http URL,`interval` 自动同步,推荐)还是 `type: file` 本地文件(更省事但实时性差一点)?
4. **内网探测 URL**:每个客户给一个"只在该 VPN 侧可达"的探测地址(用于登录成功判定和健康检查)。这是 SOCKS5 探活的前提,需要你为每个通道提供。
5. **🔴 最该立刻做的一件事**:在写任何代码前,拿你真实的那家 aTrust 跑一遍"登录→删容器→同 MAC/卷/hostname 重建→是否要求重新验证"。这条结果决定整个产品的核心卖点("新建通道后免维护")能不能成立,也决定登录编排要不要把"重新过验证码"设计成一等公民。

---

**已核对的本机事实**:Docker 29.3.1 / API 1.54 / arm64;`/var/run/docker.sock` 是指向 `~/.docker/run/docker.sock` 的可用软链;`tinyproxy-novnc.conf` 无 framing 头(`ReversePath /websockify` 与 `path=websockify` 一致);`fake-hwaddr.c` 只 hook `SIOCGIFHWADDR`(不伪造 machine-id/序列号/UUID);`start.sh` 用 `tigervncpasswd -f`(VNC 密码 8 字节上限);`EXIT` 留空=自动重连;`CLI_OPTS="-d 地址 -u 用户 -p 密码"`。本机系统 Python 3.9.6 偏旧,后端需独立 3.11+。