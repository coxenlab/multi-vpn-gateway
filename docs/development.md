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

3. **配置使用热加载**:`manager.rebuild()` 以一致数据库快照构造候选，通过 `PUT {CTRL}/configs?force=true` 的 `payload` 应用，读回匹配后才保存启动文件；规则变更不主动重启 mihomo。同一有效内容和运行态均匹配时免重载/免写盘。保持既有连接是回归要求，不能仅凭一次 2xx 推断所有协议与并发故障下绝不断连。

   配置应用由同进程锁及数据目录内的 `mihomo-apply.lock` 跨进程串行。两栈共用 `config_apply_schema.sql`：业务写入与来源 revision 同事务，内容 generation 与重试 attempt 独立；旧确认不能覆盖新处理，失败保留历史已确认版本，`/api/system.config_application` 给出 pending / 时间 / 原因码，`POST /api/config/retry` 只重新同步，不重做通道操作。规则已保存而同步失败的响应带 `saved:true`；通道操作另带 `config_application` / `rules_applied`。

   托管桌面未连接 Docker 时，规则增删/启停、分流开关与配置导入只保存设置，以 HTTP 202、`saved:true` / `deferred:true` 返回，应用记录保持 `pending` / `runtime_idle`；不写启动配置、不启动 VM，也不宣称规则已生效。页面提供“连接并同步”，该显式动作才准备底座并重新读回确认。普通名称编辑同样不启动 VM。已有实例的连接参数存入加密替换记录的 `queued` 阶段，原连接参数和实例 ID 不变；名称、验证地址等非连接字段同事务保存。列表/详情展示待应用的公开字段，密码不回传；详情可继续编辑或撤销，显式“应用并启动”才把对应代次原子交给替换流程，准备失败保留待应用内容。启动其他通道或恢复运行环境不会自动应用 queued。备份导出采用用户已保存的新设置，仍剥除交互密码与登录备注；Web 可读取、继续编辑、撤销和应用共享数据中的 queued。自装客户端仍在原客户端内改连接信息。休眠停止与删除尚需补齐。

   通道 `container_id` 变化也增加来源 revision：代理仍为 `vpn-{id}` 时配置正文不变，但旧的代理服务器 DNS 缓存可能继续指向旧实例。免重载路径遇到新来源或上次未确认时调用 `POST /cache/dns/flush`，失败保持 pending；全文重载会创建新的解析器。普通名称/备注/探活状态更新不触发刷新。该刷新影响缓存，不主动关闭已建立连接。

   未创建实例且无恢复记录的通道可在休眠时停用/删除，规则同步仍标 pending。已有实例或恢复记录的删除默认返回 `409 / runtime_required` 并保留数据；确认框说明需要启动环境，用户确认后通过 `DELETE ?prepare_runtime=true` 显式准备，再按既有流程核实清理。目标已不存在不启动环境；启动恢复已完成同一删除时读回确认后返回成功。已有实例的离线停止尚未补齐；实际准备环境并清理的新增入口仍需运行验收。

   内核读回核对内部 `__vpnmgr_cfg_*` 版本节点、托管节点名/类型、规则顺序/目标/禁用态及 mode；内部节点不被规则引用，也不进入 ch-* 通道输出。GET API 不暴露全部 SOCKS 参数和 no-resolve，因此这不是任意外部改写后的完整配置审计，更不是 DNS / TUN / VPN 认证验收。启动文件提交后再核对一次运行态，避免提交期间的内核重启被误报成功。桌面通过 0600 暂存文件、原子 rename 和内容/权限读回投递；Web 沿用目录挂载内的原子文件替换。控制器响应不明先读回，保持待确认，显式重试时可补文件而不重复热加载。

4. **所有 host 端口只绑 `127.0.0.1`**(compose + manager 均如此),永不 `0.0.0.0`。桌面专属 Colima 使用 `--port-forwarder none`，禁止来宾通配监听自动映射到宿主；固定 Lima 2.1.2 仍会把来宾回环 TCP 映射到宿主回环，不能把 `none` 理解成没有任何转发。Docker Unix socket 和 VM SSH 保留，分流/控制/noVNC 由 app 自持 SSH 转发。此参数在下次 VM 启动生效，不为改参数强制中断已运行实例；它不影响 usernet 出站 TCP/UDP，也不为宿主 SSH 入口新增 UDP 能力。
   - **oss**:1080 不映射 host(`_build_oss` 无 `ports` 项),仅 docker 内网 `vpn-{id}:1080` 可达;egress 由 dante `external: <tun>` pin 到隧道。
   - **byo**:1080 不映射 host(microsocks 仅 docker 内网可达)。Web 栈由 Docker 把 noVNC(8080)映射到 loopback；桌面栈不 publish 8080，由 app 为每条通道自持独立 SSH 转发到 127.0.0.1 随机高位。

5. **凭据安全**:密码 Fernet 加密落库(`store.py`),`_row()` 永不把 `password_enc` 与任何 secret 字段回传前端;`master.key` 权限 0600,存在数据卷里。
   - **oss**:`config_json` 列承载 per-adapter 参数,`secret:true` 字段(密码 / `.ovpn` / wg `.conf` 含私钥)字段级 Fernet 加密;凭据 / 私钥经 `manager.oss_connect` 的 `exec_run(stdin=True, socket=True)` 注入,**绝不进命令行**(`ps` 不可见)。
   - **byo**:上传安装器经 `POST /api/channels/{cid}/upload`(multipart)→ `manager.put_file` in-memory tar → `container.put_archive` 落数据卷,**绝不进 SQLite、绝不回传前端**;`config_json` 只存非密文件名引用(供前端展示已装包名)。

6. **容器细节**:SOCKS5(1080)只在 Docker 内网暴露。桌面栈 noVNC(8080)不 publish，由 app SSH 转发提供 127.0.0.1 随机高位入口；Web 栈仍用 Docker loopback 映射。aTrust 容器需 `sysctl net.ipv4.conf.default.route_localnet=1`;`DISABLE_PKG_VERSION_XML=1` 两种 hagb 类型均需。EasyConnect 镜像 tag 由 `dockerhub.py` 实时拉取(非硬编码),`ec_ver` 存用户选定的 tag。

7. **代理命名**:外层用户 Clash 里那个节点叫 `vpn-router`(= 整个 mihomo 实例的分流端口);内层 mihomo 里每条通道是 `ch-{id}` 的 socks5 代理。别混淆。

8. **VM 层私网出站守卫(桌面栈)**：`vm::ensure_egress_guard` 经独立 SSH，以 iptables-restore 原子替换 `VPNMGR_EGRESS`；启动在镜像处理前下发，看门狗每分钟实际核对，睡醒/换网重下发。仅覆盖来自本工具 VPN 网段的 IPv4 转发，拒绝未被 VPN 接管的私网目标；保留 Docker、VM 和宿主直连网段豁免。不覆盖 VM OUTPUT、其他 Docker 网络、IPv6、不可达公网或豁免内黑洞，替换 runtime 后须重新验收。基础防护不受暂停自动修复开关影响。旧 gvisor-tap-vsock v0.8.9 的 10 个在途拨号槽可被不可达目标占满，影响其他新建 TCP；这解释了一类故障，不是所有断链的统一原因。`usernet.rs` 按当前 profile 定位关联进程和版本，SYN_SENT 仅作候选证据，共享网络下来源待定位。不要改成容器内 `ip route add unreachable`，它会先于 EC 的 nat REDIRECT 拒绝合法路径；Web 栈不下发此 VM 守卫。

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

共享 Web 界面使用暖色浅色主题：象牙纸底、陶土橙 `--accent #ac4c22`，标题与正文共用系统字体栈，不加载网络字体；数据等宽使用系统等宽字体及本机可用回退。`--coral #d97757` 只作装饰，不作正文或按钮底色。所有 token 在 `app/static/css/app.css` 的 `:root`；沿用现有语义色和组件，不新增一套主题。

八个根页面只加载 `js/pages/<页面名>.js`，控制器通过显式 import 组合公共 API、反馈、预检、环境和 VNC 组件；公共模块暂保留 `window` 兼容出口，页面业务函数不再靠内联脚本共享全局变量。首页卡片使用事件委托，不生成内联 onclick。`page-data.js` 共用主体与系统状态的加载规则：主体失败进入可重试错误态，系统状态失败保留主体数据并以未知状态降级。`api.js` 负责 GET 请求合并和可见性轮询。修改页面逻辑时编辑对应模块，避免在 HTML 恢复内联控制器。

`vnc-lifecycle.js` 管理详情和新建向导的登录视图：仅在登录步骤可见时打开，切走、隐藏或 pagehide 时取消就绪探测并移除 iframe；返回后按需打开，过期请求不能覆盖新视图。“键入到容器”的临时 RFB 也接收同一取消信号，停止后续键入并尽力清空已写入的剪贴板。`waitNovncReady` 的单次请求上限 5 秒，整体受调用方时限约束。该层只释放浏览器连接，不停止 VPN 容器、GUI 客户端或改变连通判据；桌面后端 SSH 的回收由其自身生命周期管理。

桌面 `novnc.rs` 仅在 `/login` 请求时建立 SSH；创建/启动容器不再依赖显示入口就绪。新前端每次打开生成独立 `viewer`，后端确认 `viewer_id` 后每 20 秒续期，离开以 keepalive DELETE 释放；最后一个观看者离开即回收 SSH，未收到释放时按 60 秒有效期由 20 秒看门狗清理。只恢复仍有观看者的入口，回收不受暂停自愈影响。登录拉起、通道变更与退出按通道串行，不同通道的转发锁相互独立。只复用本进程持有的 SSH 子进程，不能把数据库旧端口上的其他服务当成登录入口。旧桌面客户端未传 viewer 时保留常驻语义；Web/旧后端不回 viewer_id 时前端不发送续期或释放。

每次入口拉起/恢复最多占用通道操作锁 20 秒；超时清理本次 SSH 并返回可重试错误，VPN 客户端继续启动。前端登录请求最多等待 45 秒（含排队），不能因 GUI 迟迟未就绪而把停止操作堵住数分钟。

容器替换的持久化基础在双栈 `replacement_store`：`channel_replacements` 用原主密钥加密内部操作参数，按通道、操作代次和阶段核对；候选准备期间不写通道配置。验证成功时，字段更新、加密配置、容器状态、当前数据卷与 committed 标记同事务提交；恢复时只写确认过的运行态，保留期间新增的备注。`channel_runtime` 保存替换后选定的数据卷，未有记录时回退原 `vpndata-<cid>`。双栈 `replacement_docker` 提供未启动候选和独立数据卷：操作 labels 核对归属，旧容器停止后，固定镜像 ID 的隔离复制容器以只读源卷复制；不删除旧容器、不写旧卷，创建/删除响应不明先读回。真实隔离数据复制与故障测试通过。`replacement` 编排函数将旧实例关闭自动重启并停止后复制，候选就位后才提交配置；GUI 进入 awaiting_login 保留旧资源，真实探活确认后可清理，恢复只补偿本次字段并保留备注。hagb/oss 恢复以原镜像/卷另建客户端，BYO 不进入重建。双栈隔离 Docker 的切换/恢复与复制中断测试已通过。等待登录的候选可继续启动并保留旧备份；停止异常操作先保存停用意图，避免恢复时短暂重登。删除先保存 deleting，确认所有关联实例清理后才同事务删除配置和进度；命名卷沿用现有保留策略。容器盘点将关联实例列为恢复资源，孤儿清理按通道锁重核进度并拒绝删除。双栈现有通道的修改、启动、停止与删除已接入该编排，Web/非托管 core 在服务启动时恢复，按需桌面在底座启动时恢复未完成记录；详情页显示公开阶段并提供 `/restore`，等待验证期间阻止重复编辑，备注仍可保存。探活只在当前代次上确认候选，再异步清理旧资源。真实隔离 API 与共享 Chrome 页面验证了编辑、恢复、页面保存备注及成功探活后的清理；未覆盖厂商 GUI/企业认证。首次创建与旧实例丢失也使用同一候选/日志流程：正式名称被外部占用则拒绝；残留源卷须实际存在且无人使用，复制前后重核使用者，源只读。无旧实例的失败补偿原子还原本次字段/原卷、清除失效容器 ID，并保留备注；状态为 error（明确停止则 stopped），可以重新启动。首次 GUI 创建没有可恢复原卷时，只确认运行态即结束创建记录，状态仍为 running；hagb headless 与 oss 均须原验证地址探活成功。自装客户端有旧 ID 时只按原完整 ID/正式名称确认后原地启动；旧实例丢失不自动重建安装。旧的先删同名再创建入口已移除。

## 隔离开发实例

`desktop/app/dev.sh` 使用独立 `vpnmgr-dev` VM、`vpnmgr_dev_vpnnet` 网络、Application Support/vpnmgr-dev 数据和 48878/48879/48880 端口。`--core` 只运行 Rust HTTP 核心；默认运行桌面壳，两者均启用按需 VM。开发实例禁用助手安装/替换/卸载、系统代理和 TUN 操作，自动对账与退出也不调用全局助手。非日常 profile 不挂载宿主目录、不改 SSH 配置和当前 Docker context。

桌面打开管理界面只准备本地组件、配置和 HTTP 服务，连接/创建/恢复、显式修复才经 `runtime.rs` 共享启动底座。`/api/system.runtime` 提供阶段、进度和错误；`POST /api/runtime/start` 可显式准备环境，创建向导在用户确认创建后、预检前调用。独立 core 由 `VPNMGR_MANAGED_VM=1` 选择该模式，默认仍连接现有引擎。尚未运行时不探活或启动看门狗修复，也不把旧登录记录报为当前连通；启动先核对网络守卫，再载入镜像、建立自持 SSH、恢复未完成操作和读回规则。管理链路失败不自动重启全部通道，显式重试保留现场；退出及 managed core 的 SIGINT/SIGTERM 等待已接受工作后清理。

按需底座在全通道明确 `stopped`、无有效登录租约/恢复记录/待同步规则、Docker 实际只有 mihomo 或无运行容器时，等待 60 秒后释放。新用户操作重置等待；维护检查只计入占用，不重置计时。写请求和 preflight 临时探针的执行不随 HTTP 断开取消，后台拉取独立持有活动许可；一旦开始释放，新操作等待完成。释放按 TUN/本实例 PAC→自持 noVNC/分流转发→本 profile VM 的顺序执行，入口、子进程和 VM 均须读回确认；任何失败保留失败状态，不循环重放停止。VM 停止命令运行期间保持释放状态，应用退出仍受总清理预算约束。再次连接只恢复本进程暂停过、URL 仍指向本实例的 PAC 服务及保留的 TUN 意图；不接管外部代理。旧助手若不能确认引擎、路由和 pending 状态，会阻止自动释放。休眠期间全部编辑/停止/清理入口、实际宿主入口切换、旧数据迁移与厂商通道验收仍未完成，不能据此认为 P5 完成。

`VPNMGR_VM_PROFILE` 默认 `vpnmgr`；Docker 连接、SSH、守卫和 shutdown 均从 Config 的 profile 取地址。DATA_DIR 显式设置优先；桌面调试构建使用开发默认，发布构建固定使用 Application Support。旧开发目录中的日常数据不自动迁移；升级交付前须备份并执行独立迁移/保留配置验收。

单元测试用 `.venv/bin/pytest tests -q`，不要从仓库根递归扫描 handoff 符号链接。测试强制使用本轮创建的临时目录，拒绝真实 Docker/requests 网络操作。

完整本地检查入口为 `./verify.sh`：Python 隔离单测、Rust core/helper 测试与 clippy、桌面壳检查，以及共享页面和脚本语法检查。入口强制使用临时 DATA_DIR、MIHOMO_CONFIG_PATH、不存在的 Docker socket 和独立 profile，结束后清理；测试自己构造 Config 时仍须显式使用临时目录与 dev_mode，不能绕过环境隔离去读日常默认目录。Cargo 使用锁文件和离线缓存；缺依赖时明确失败，不自动联网安装。该入口不启动 VM、不登录 VPN、不修改系统代理或助手；通过不等于真实运行与业务验收完成。需要真实 Docker 的旧 Rust 测试保持显式 ignored，按对应测试要求另行执行。

通道自动刷新使用 `/api/channels/{id}/health`：结果含 `checked_at` 和 `stale`，稳定状态复用 30 秒、失败有界退避，过期结果标待确认并后台更新。手动检测继续使用 `/status`，不读取过期缓存。同通道并发共享一次探活；通道变更的代次校验阻止旧结果回写。共享 `api.poll` 在上一轮完成后计时，页面隐藏后暂停；监控明细与低频通道目录分别刷新。

桌面出站诊断由 `usernet.rs` 随看门狗最多每 60 秒采样一次，整轮 10 秒上限、命令 3 秒上限、输出 64 KiB 上限；仅按当前 profile 的 Lima 网络配置和对应 PID 文件定位进程，并核验命令身份。`runtime_info.rs` 从映射文件一致的 Mach-O 构建信息读取依赖，按文件身份缓存；只读取 load commands 和 Go 构建信息段，不要求用户安装 Go。识别 gvisor-tap-vsock v0.8.9 默认 10 槽，以及明确版本 `v2.1.2-vpnmgr.698.8b4db4a` 的回移候选 128 槽；replacement、未知依赖或格式均不推断容量。版本识别是诊断参照，不是签名验证。来源：[Go 构建信息格式](https://go.dev/src/debug/buildinfo/buildinfo.go)、[v0.8.9 TCP forwarder](https://github.com/containers/gvisor-tap-vsock/blob/v0.8.9/pkg/services/forwarder/tcp.go)。

桌面 runtime 构建使用 `desktop/app/runtime-sources.lock.json` 固定 Lima 源码提交、Colima/Docker 官方资源 SHA256 和 Go 1.26.1。开发机需该 Go 版本及 Xcode；最终用户无需安装。`stage-runtime.sh` 默认构建 baseline，`VPNMGR_RUNTIME_VARIANT=gvisor698 ./build-dmg.sh` 显式选择候选。候选只回移 [上游 #698](https://github.com/containers/gvisor-tap-vsock/pull/698) 的三个库文件，校验改前/改后哈希，不更新 Lima 的 go.mod/go.sum；整版本替换另有 UDP API 不兼容及依赖升级，未采用。源码/构建缓存默认在 `~/Library/Caches/vpnmgr-build/runtime/`，缓存损坏则停止；暂存前验证所有产物、签名及配套 guestagent，附 manifest 和 Go 构建信息，下一次打包不会再用 Homebrew 文件静默覆盖候选。当前仅支持 macOS arm64；真实网络切换/Clash/睡醒和长时间验收通过前，候选不作为默认安装版本。

`/api/system` 只读缓存，设置的环境页可展开时间、SYN_SENT 总数、最多 16 个目标及最近守卫结果。共享 usernet 的连接无法唯一反查通道；多网卡配置也不证明 usernet 是默认出口。诊断只提供候选证据，不改变登录态、不触发自动重启，权限/超时/配置不明确时报告未知，历史采样保留时间标记。
