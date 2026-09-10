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
│   └── static/                 # 共享 Web 前端，每页一个 ES module 入口
│       └── js/pages/           # 八个页面控制器；api / page-data / app / feedback 为共享模块
├── desktop/                    # macOS 桌面版(Tauri 壳 + Rust host-only core)
│   └── core/src/events.rs      # 运行事件内存环 + 脱敏 JSONL + 查询/导出 API
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
   - **byo**:1080 不映射 host(microsocks 仅 docker 内网可达)。Web 栈由 Docker 把 noVNC(8080)映射到 loopback；桌面栈不 publish 8080，由 app 为每条通道自持独立 SSH 转发到 127.0.0.1 随机高位。

5. **凭据安全**:密码 Fernet 加密落库(`store.py`),`_row()` 永不把 `password_enc` 与任何 secret 字段回传前端;`master.key` 权限 0600,存在数据卷里。
   - **oss**:`config_json` 列承载 per-adapter 参数,`secret:true` 字段(密码 / `.ovpn` / wg `.conf` 含私钥)字段级 Fernet 加密;凭据 / 私钥经 `manager.oss_connect` 的 `exec_run(stdin=True, socket=True)` 注入,**绝不进命令行**(`ps` 不可见)。
   - **byo**:上传安装器经 `POST /api/channels/{cid}/upload`(multipart)→ `manager.put_file` in-memory tar → `container.put_archive` 落数据卷,**绝不进 SQLite、绝不回传前端**;`config_json` 只存非密文件名引用(供前端展示已装包名)。

6. **容器细节**:SOCKS5(1080)只在 Docker 内网暴露。桌面栈 noVNC(8080)不 publish，由 app SSH 转发提供 127.0.0.1 随机高位入口；Web 栈仍用 Docker loopback 映射。aTrust 容器需 `sysctl net.ipv4.conf.default.route_localnet=1`;`DISABLE_PKG_VERSION_XML=1` 两种 hagb 类型均需。EasyConnect 镜像 tag 由 `dockerhub.py` 实时拉取(非硬编码),`ec_ver` 存用户选定的 tag。

7. **代理命名**:外层用户 Clash 里那个节点叫 `vpn-router`(= 整个 mihomo 实例的分流端口);内层 mihomo 里每条通道是 `ch-{id}` 的 socks5 代理。别混淆。

8. **VM 层私网出站守卫(桌面栈)**:`vm::ensure_egress_guard`(`desktop/core/src/vm.rs`)经 ssh 在 lima VM 的 `DOCKER-USER` 链(FORWARD 首跳)下发 `VPNMGR_EGRESS`:来自 VPN 网段、目标为 10/8、172.16/12、192.168/16、100.64/10 的转发 → REJECT(TCP 回 RST);豁免 docker 网段、VM 自身网段、宿主当前直连的局域网段。app 启动时强制下发,看门狗每分钟核对、睡醒 / 换网强制重下发(`health::ensure_egress_guard`)。**原因**:VM 出站走 lima usernet(gvisor-tap-vsock v0.8.9),其 TCP forwarder 硬编码在途上限 10、宿主侧拨号无超时,macOS 对不可达地址 connect 75s 才超时;容器里没被隧道 / 客户端代理接管的私网目标(aTrust 探测的内网备用线路、未登录通道的探活、绑定但未下发的网段)漏到 VM 出口每个占一个槽,10 个占满整 VM 新建 TCP 全丢 SYN、所有通道一起「时通时断」(2026-09-10 实测在途 9→3/3、10→0/3;上游 issue containers/gvisor-tap-vsock#676,修复 PR #698 已合并未发布)。**为什么在 VM 层而不是容器内**:一条规则覆盖全部容器、docker 自发的容器重启(unless-stopped 起来的容器网络命名空间是空的)、同 VPN 网段的探活容器,不依赖镜像有 iptables / NET_ADMIN。⚠️ 别改成容器内 `ip route add unreachable`:本机发包先查路由再过 nat OUTPUT,会把 EC 用 nat REDIRECT(→4440)接管的网段资源拒掉(客户A实测)。Web 栈跑在 Docker Desktop 上没有这个上限,不下发。

## HTTP API

Web 公共端点以 `app/main.py` 为事实源；桌面 host-only 端点以 `desktop/core/src/server.rs` 为事实源。完整端点表见 [README](../README.zh-CN.md#http-api)。要点:

- 分流规则走 `POST /api/channels/{cid}/rules`(`patterns[]` 或 `pattern`,可选 `kind: domain|ip`,裸 IP 自动补 `/32`、`/128`),`PATCH` / `DELETE .../rules/{rid}` 启停 / 删除单条。域名与 IP-CIDR 均已支持。
- `GET /clash/vpn-rules.yaml` 产出 `behavior: classical` 的 rule-provider 清单;`GET /api/clash-snippet` 给出 `vpn-router` 节点 + `RULE-SET,vpn-rules` 引用(或内联回退)。
- 桌面版 `GET /api/events` 按游标/级别/来源/事件码/消息查询运行事件；`GET /api/events/export?days=1` 导出本机脱敏 JSONL。web 版无此路由,前端按 404 隐藏「运行日志」。
- 改 API 必须同步改 README 的 API 表。

## 现状与下一步

- **已落地**:三家族适配器(hagb / oss / byo);域名 + IP-CIDR 分流规则、启停、rule-provider 与遥测;共享 Web 前端接入真实 API(mock `data.js` 已移除);无 Clash 时的入口接入(PAC + 各平台一键命令);arm64 交互式 noVNC 登录。
- **TODO(Phase 4)**:逐家国产 VPN 厂商 GUI 的按需预装 / 程序化登录适配器。byo 是长尾兜底(任意客户端手动装),Phase 4 才是逐家专有客户端的一等适配(预装镜像 + 自动化登录)。
- **已落地(2026-07-16)**:「mihomo 未运行」看门狗盲区手动修复入口。转发器半僵死(容器 Up、端口 TCP 通但零响应)时 `gateway_health=healthy` 但 `mihomo_status=down`——芯片红、横幅沉默、无处可点。修复:`feedback.js` 新增盲区态横幅(danger,「分流路由(mihomo)无响应」+ 复用「手动修复」按钮走 `/api/system/heal-proxy` 两级自愈),红态芯片本身可点、重开横幅;web 版无 `gateway_health` 字段不触发。注意安装版 app 静态资源随包分发,需重建 dmg 才到用户手上。
- **已落地(2026-08-06)**:桌面运行事件日志。关键启动、自愈、隧道、TUN、通道生命周期写入 `<data_dir>/logs/vpnmgr-YYYY-MM-DD.jsonl`(14 天、10MB/日、凭据键脱敏),「系统与接入 → 运行日志」支持 5 秒增量、筛选、搜索、复制与导出；web/旧桌面版 404 自动降级。
- **已推迟**:strongSwan / IKEv2——纯 IPsec 无具名 tun、dante 难 pin egress(须 route-based VTI/XFRMi),暂不收(见 `adapters.yaml` 末尾注释)。

### byo 失败模式(诚实标注,尽力而为)

适用于「自带 tun、纯网络认证」的普通 Linux GUI / CLI 客户端;**不支持** systemd/dbus 守护进程客户端(AnyConnect `vpnagentd`、GlobalProtect `panGPS`)、需 host 缺失内核模块、硬件令牌 / 智能卡 / TPM 绑定、仅 Windows/macOS 的客户端;split-tunnel 时「SOCKS = 全隧道」不成立。这类场景应优先用 hagb / oss 适配器。

## 前端设计系统

共享 Web 界面使用暖色浅色主题：象牙纸底、陶土橙 `--accent #ac4c22`，标题 Source Serif 4、正文 Inter、数据 JetBrains Mono。`--coral #d97757` 只作装饰，不作正文或按钮底色。所有 token 在 `app/static/css/app.css` 的 `:root`；沿用现有语义色和组件，不新增一套主题。

八个根页面只加载 `js/pages/<页面名>.js`，控制器通过显式 import 组合公共 API、反馈、预检、环境和 VNC 组件；公共模块暂保留 `window` 兼容出口，页面业务函数不再靠内联脚本共享全局变量。首页卡片使用事件委托，不生成内联 onclick。`page-data.js` 共用主体与系统状态的加载规则：主体失败进入可重试错误态，系统状态失败保留主体数据并以未知状态降级。`api.js` 负责 GET 请求合并和可见性轮询。修改页面逻辑时编辑对应模块，避免在 HTML 恢复内联控制器。

## 隔离开发实例

`desktop/app/dev.sh` 使用独立 `vpnmgr-dev` VM、`vpnmgr_dev_vpnnet` 网络、Application Support/vpnmgr-dev 数据和 48878/48879/48880 端口。`--core` 只运行 Rust HTTP 核心（VM 需事先启动）；默认运行桌面壳。开发实例禁用助手安装/替换/卸载、系统代理和 TUN 操作，自动对账与退出也不调用全局助手。非日常 profile 不挂载宿主目录、不改 SSH 配置和当前 Docker context。

`VPNMGR_VM_PROFILE` 默认 `vpnmgr`；Docker 连接、SSH、守卫和 shutdown 均从 Config 的 profile 取地址。DATA_DIR 显式设置优先；桌面调试构建使用开发默认，发布构建固定使用 Application Support。旧开发目录中的日常数据不自动迁移；升级交付前须备份并执行独立迁移/保留配置验收。

单元测试用 `.venv/bin/pytest tests -q`，不要从仓库根递归扫描 handoff 符号链接。测试强制使用本轮创建的临时目录，拒绝真实 Docker/requests 网络操作。

完整本地检查入口为 `./verify.sh`：Python 隔离单测、Rust core/helper 测试与 clippy、桌面壳检查，以及共享页面和脚本语法检查。Cargo 使用锁文件和离线缓存；缺依赖时明确失败，不自动联网安装。该入口不启动 VM、不登录 VPN、不修改系统代理或助手；通过不等于真实运行与业务验收完成。需要真实 Docker 的旧 Rust 测试保持显式 ignored，按对应测试要求另行执行。

通道自动刷新使用 `/api/channels/{id}/health`：结果含 `checked_at` 和 `stale`，稳定状态复用 30 秒、失败有界退避，过期结果标待确认并后台更新。手动检测继续使用 `/status`，不读取过期缓存。同通道并发共享一次探活；通道变更的代次校验阻止旧结果回写。共享 `api.poll` 在上一轮完成后计时，页面隐藏后暂停；监控明细与低频通道目录分别刷新。
