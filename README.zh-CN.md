# VPN 管理网关

[English](./README.md) | **中文**

> 自托管的 VPN 管理网关。把多家企业 VPN 同时跑起来——每家各关进一个 Docker 容器、暴露一个 SOCKS5 出口——再由一个独立的第二个 mihomo 实例按域名 / IP 把流量分流到各家。现有 Clash 一字不改,只加一个 `vpn-router` 节点 + 订阅一份分流规则。全程全 Docker,本机零新增依赖。

## 解决什么问题

要同时连多家企业 VPN,官方客户端会互相抢路由 / DNS、各装一堆驱动,而且通常一次只能连一家。这个网关把每家 VPN 隔离进各自的容器,对外统一成 SOCKS5 出口,再按域名 / IP 分流——多家内网同时可达,互不干扰。

## 架构

三层,流量自外向内:

```
你现有的 Clash ──(命中分流规则)──▶ vpn-router 节点
                                        │
              第二个 mihomo(本工具)── 按 域名 / IP 分流 ──▶ ch-1 / ch-2 / …
                                        │
        每家 VPN 一个容器(ch-{id})── EC / aTrust / openconnect / … ──▶ SOCKS5 出口 ──▶ 企业内网
```

- **登录**:有头客户端(EasyConnect / aTrust,或 BYO 桌面)跑 noVNC,在浏览器里完成企业 VPN 的交互式登录(支持「重新登录」应对设备重绑);无头客户端经注入凭据登录,无 noVNC。
- **判活**:后端经 SOCKS5 探活(`socks5h` 远程解析 `probe_url`)判定是否真连上内网——而非「VNC 连上了」。
- **热加载**:加 / 改分流规则后,mihomo 配置热重载,**不断现有连接**。

没有 Clash 时也能用「入口接入」:把系统 / 浏览器代理直接指向本工具 mihomo(`/entry/proxy.pac`,或 `/api/entry/setup-commands` 给的各平台一键命令),命中规则的流量走 VPN、其余直连。

## 快速开始

### 前置要求

Docker,且带 Compose v2 插件(`docker compose`)。除此之外什么都不用装——所有运行期依赖都在容器里。

### 启动

在仓库根目录:

```bash
./start.sh
```

`start.sh` 可重复执行,做三件事:

1. 首次运行:生成 `.env`,里面是随机高位端口(管理界面 / mihomo 分流 / mihomo 控制台)+ mihomo 密钥,之后保持不变。
2. 首次运行:用密钥从模板渲染 `mihomo/config.yaml`。已存在的配置(含你建好的通道)会保留。
3. 执行 `docker compose up -d --build`。

完成后会打印出各端点——**全部只绑 `127.0.0.1`**:

| 端点 | 环境变量 | 用途 |
|---|---|---|
| 管理界面(Web UI) | `UI_PORT` | 管理操作台,在浏览器打开 |
| mihomo 分流端口 | `MIHOMO_PORT` | 给你的 Clash 接(见界面里「Clash 配置」按钮) |
| mihomo 控制台 | `MIHOMO_CTRL_PORT` | mihomo 外部控制台 API |

具体端口见 `.env`。首次运行会拉 / 构建镜像,耗时较久。

### 停止

```bash
docker compose down
```

删某个 VPN 容器:在界面点删除,或 `docker rm -f vpn-<id>`。

### 只改前端

不想起后端 / Docker,只调界面:

```bash
cd app/static && python3 -m http.server 8080
```

### 跑测试

在宿主跑单测(不需要 FastAPI,依赖与 app 镜像分开):

```bash
pip install -r tests/requirements-dev.txt
pytest
```

## 支持的 VPN

适配器是声明式的(`app/adapters.yaml`),分三个家族:

| 家族 | 登录 | 客户端 |
|---|---|---|
| **hagb** | 交互式,经 noVNC | EasyConnect、aTrust(上游 `hagb/docker-easyconnect` / `hagb/docker-atrust` 镜像) |
| **oss** | 无头(注入凭据) | Cisco AnyConnect、GlobalProtect、Fortinet、Juniper/Pulse、Ivanti、openfortivpn、OpenVPN、WireGuard——共用自建镜像 `vpnmgr/oss-vpn`(`images/oss/`) |
| **byo** | 自带,经 noVNC | 一台 `custom` Linux 桌面(`vpnmgr/byo-desktop`,`images/byo/`),自己手动装任意 VPN GUI——长尾兜底,尽力而为 |

> BYO 兜底适用于「自带 tun、纯网络认证」的普通 Linux GUI/CLI 客户端;**不支持** systemd/dbus 守护进程客户端、需 host 缺失内核模块、硬件令牌 / 智能卡 / TPM 绑定、仅 Windows/macOS 的客户端。这类场景请优先用 hagb / oss 适配器。

## 仓库结构

```
.
├── README.md / README.zh-CN.md   # 本文件(中英双语)
├── LICENSE                       # MIT(项目自有代码)
├── NOTICE                        # 第三方 / 闭源软件免责声明
├── CONTRIBUTING.md
├── CHANGELOG.md
├── docker-compose.yml            # mihomo + app 两服务;端口全绑 127.0.0.1
├── start.sh                      # 一键启动
├── gen_env.py                    # 生成 .env(随机端口 + 密钥)
├── mihomo/config.template.yaml   # mihomo 配置模板(首次运行渲染)
├── images/                       # 自建容器镜像(oss / byo)
├── app/                          # FastAPI 后端 + 静态前端
├── tests/                        # pytest 单测 + smoke.sh
└── docs/
    ├── design.md                 # 完整设计意图(想懂「为什么」先读它)
    └── development.md            # 架构、命门、开发约定
```

## HTTP API

以 `app/main.py` 为准。

| 方法 | 路径 | 说明 |
|---|---|---|
| GET | `/api/vpn-types` | 适配器列表(驱动向导类型网格) |
| GET | `/api/vpn-types/{type}/versions` | `{versions:[{tag, arch, usable_here}]}`,实时 Docker Hub;非 versioned 适配器返回空列表 |
| GET | `/api/channels` | 通道列表(每条含 `domains[]` / `ips[]` / `socks_endpoint` / `uptime` 等) |
| POST | `/api/channels` | 建通道并起容器(`name, vpn_type, server, ec_ver, login_method, username, password, probe_url, config{}`) |
| GET | `/api/channels/{cid}/login` | noVNC 登录地址 `{url}`;无头适配器返回 `{login_mode:"headless"}` |
| POST | `/api/channels/{cid}/upload` | multipart 上传 → `{ok, package}`(BYO 安装器经 `put_archive` 落数据卷) |
| GET | `/api/channels/{cid}/status` | **跑 SOCKS5 探活** → `{status, connected, latency_ms}` |
| POST | `/api/channels/{cid}/rules` | 加分流规则(`patterns[]` 或 `pattern`,可选 `kind: domain\|ip`;裸 IP 自动补 `/32`、`/128`)→ `{reload_status, domains, ips, added, rejected}` |
| PATCH | `/api/channels/{cid}/rules/{rid}` | 启用 / 停用一条规则(`enabled`)→ `{ok, reload_status}` |
| DELETE | `/api/channels/{cid}/rules/{rid}` | 删规则 → `{ok, reload_status}` |
| POST | `/api/channels/{cid}/start` \| `/stop` | 起 / 停容器 → `{ok}` |
| DELETE | `/api/channels/{cid}` | 删通道 → `{ok}` |
| GET | `/api/channels/{cid}/logs?tail=200` | 容器日志 → `{lines}` |
| GET | `/api/system` | mihomo 状态 / 端口 / 控制台地址 |
| GET | `/api/connections` | mihomo 实时连接 |
| GET | `/api/proxies` | mihomo 代理 → `{proxies}` |
| GET | `/clash/vpn-rules.yaml` | 给 Clash 订阅的 rule-provider 清单(`text/plain`) |
| GET | `/api/clash-snippet` | 给用户 Clash 粘的节点 + 规则(`text/plain`) |
| GET | `/entry/proxy.pac` | 无 Clash 时入口接入的 PAC 文件 |
| GET | `/api/entry/setup-commands` | 各平台代理开 / 关一键命令 |

> 另有 `GET /` 与一个 catch-all 静态挂载,服务单页前端。

## 通道状态机

```
creating ──▶ running ──▶ logged_in     (另有 stopped、error)
```

- **running**:容器起来了,还没登录成功(待登录)
- **logged_in**:SOCKS5 探活通过(真连上内网)

## 安全

- 所有 host 端口只绑 `127.0.0.1`,**永不** `0.0.0.0`。
- 密码 Fernet 加密落库;`master.key` 权限 0600、存在数据卷;接口永不回传密文与 secret 字段。无头适配器经 stdin 注入凭据,绝不进命令行;BYO 安装器流式落数据卷,绝不入 SQLite。
- SOCKS5(1080)只在 Docker 内网暴露;noVNC 映射到 `127.0.0.1` 随机高位端口。

## 文档

- [`docs/design.md`](./docs/design.md) —— 原始设计 / 落地方案(实现前的快照,部分技术选型与最终实现不同;当前架构见 development.md)。
- [`docs/development.md`](./docs/development.md) —— 架构、绝不能破坏的命门、开发约定。
- [`CONTRIBUTING.md`](./CONTRIBUTING.md) —— 如何跑、测、贡献。

## 许可

MIT,见 [`LICENSE`](./LICENSE)。MIT 仅覆盖本项目自有代码;第三方 / 闭源软件免责声明见 [`NOTICE`](./NOTICE)。
