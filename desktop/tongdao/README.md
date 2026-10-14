# 通道管家 macOS 外壳

通道管家是和旧应用（`desktop/native`，`com.vpnmgr.desktop`）并存安装的新应用。外壳只负责窗口、菜单栏面板、文件面板和 Rust core 子进程的生命周期；界面是 `app/ui` 的网页，由 WKWebView 承载，直接调用 core 的 HTTP 接口。最低 macOS 14。

## 身份

| 项 | 正式版 | 开发版（`dev.sh`） |
|---|---|---|
| 应用标识 | `com.tongdao.desktop` | 调试构建不查单实例 |
| 数据目录 | `~/Library/Application Support/com.tongdao.desktop` | `~/Library/Application Support/com.tongdao.desktop-dev`（可用 `TONGDAO_DEV_DATA_DIR` 覆盖） |
| 运行环境档名 | `tongdao` | `tongdao-dev` |
| 运行环境根目录 | `~/.tongdao/colima` | 同左 |
| 容器网络 | `tongdao_vpnnet` | `tongdao_dev_vpnnet` |
| 界面目录 | `<包>/Contents/Resources/ui` | `app/ui` |
| 助手 | `/Library/PrivilegedHelperTools/tongdao`，套接字 `/var/run/tongdao-helper.sock` | 不安装 |
| 虚拟网卡 | `utun226` | — |

core 和助手二进制仍叫 `vpnmgr-core`、`vpnmgr-helper`，靠目录和档名区分：档名是 `tongdao` 或以 `tongdao-` 开头，core 就按通道管家身份取上面这些值（`desktop/core/src/identity.rs`）。正式版启动前先核对随包组件（`NativeLaunchPlan.swift`），缺件时不创建数据、不启动 core；同一应用标识已有实例在运行时拒绝启动。继承的 `VPNMGR_*`、`MIHOMO_*`、`COLIMA_*`、`LIMA_*`、`DOCKER_*` 和端口、目录类变量一律清掉，改用随包路径。

不带升级切换：从旧应用搬配置只走导出和导入。

## 窗口和菜单栏

- 窗口默认 1280×800，最小 1100×720，标题栏透明；侧栏顶部 196×50 盖了一块原生拖拽条，那里不放可点的控件。
- 关窗默认只隐藏窗口，core 照常运行；设置里关掉「关闭窗口后继续运行」（`<DATA_DIR>/app-settings.json` 的 `keep_running_on_close` 为假）时关窗即退出，走完整的退出清理。
- 菜单栏图标旁显示在线通道数（本地服务没就绪时不显示数字）。
- 菜单栏面板：在线通道数、下载速度、分流总开关，每条通道一行，状态文字和主窗口同一套（已连接、待登录、出错、已停止）。已连接和已停止的给开关；待登录的通道要登录窗口时给「去登录」，不需要登录窗口的（无头通道）给「查看」；出错的给「查看」。底部「打开主窗口」「退出」。
- 数字和通道列表只在三个时候刷新：本地服务就绪时一次、面板打开期间每 3 秒一次、面板里的写操作之后一次；面板和主窗口都关着时不在后台刷新，数字可能过时。
- 主窗口页面还没加载完时在面板上点「去登录」「查看」，跳转先记下，页面加载完成后执行一次。主窗口关到菜单栏以后，应用从隐藏状态切回来不把页面标成可见，轮询保持暂停。导出文件时在保存面板里确认替换同名文件，外壳先删掉旧文件再交给下载。

## 页面和外壳之间

- 可见性：外壳调 `window.__tongdaoSetVisible(bool)`，页面的 `document.hidden` 随之变化，轮询暂停。
- 跳转：⌘N 调 `window.__tongdaoOpen('?sheet=add')`，⌘, 调 `window.__tongdaoOpen('#/settings')`，菜单栏面板调 `#/channels/<id>` 或 `#/channels/<id>?tab=login`，由 `app/ui/js/main.js` 定义。
- 版本号：文档开始加载时注入 `window.__tongdaoAppVersion`，取 `CFBundleShortVersionString`，`swift run` 下是「开发版」。
- 页面到外壳没有消息通道。选文件和下载走 WebKit 自带的打开、保存面板。

## 验证地址、接管状态和通道管理

- **验证地址对哪些类型是必填**：只有无头类型（OpenVPN、WireGuard、AnyConnect、GlobalProtect、Fortinet、Juniper、Pulse、openfortivpn，出口绑在隧道上）没填验证地址时，会用规则里第一条域名或单个地址来确认连通。EasyConnect、aTrust、山石 Secure Connect、自定义这 4 种出口没有绑在隧道上，没填验证地址就一直是「已启动，还没验证」，加规则也确认不了，要在「还没验证」页或「修改连接信息」里填一个连上以后才能打开的内网地址。检测没通过的地址不会保存。
- **接管状态**（`GET /api/takeover`）：`effective`、`domains_via` 按实际检测到的状态报。`domains_via` 是 `none` 表示域名规则现在没有生效（比如该由系统代理处理、系统代理却没有指向自己），规则页、测试地址会如实写并给出让它生效的动作。`action` 是 `retry_takeover` 表示接管执行过但没有生效，总览和设置页给「重试」；自动重试至少隔 5 分钟。已经用系统代理接管时，「启用虚拟网卡」只是一句建议，不算要处理的事。设置页「接管网络」是两个独立开关（虚拟网卡、系统代理），对应 `mode` 的 `clash`（都不开）、`tun`、`system_proxy`、`tun_system_proxy`，核心照开关执行；卡片顶部「现在的网络」三行按实际状态写内网地址、域名、其余流量各由谁接，Clash 在跑时在系统代理开关下提醒会让 Clash 接不到流量。通用的代理地址始终显示，给用其他代理软件的人手动接入。
- **通道管理**：通道详情头部「更多」菜单里有「修改连接信息」（名字、该类型的必填项、验证地址，密码留空不改）和「删除通道」（先确认，规则一起删掉）。改了设置、等待确认时详情页给「恢复上一次设置」。没有参与分流的通道（从旧应用导入时原样带过来）规则写「走原网络（不参与分流）」，规则分页给「恢复分流」。添加通道时配置文件超过 1 MB 不读取，直接提示太大。

## 验证环境

改代码时的编译和测试不起 VM、不碰日常在用的那一套。直接用默认环境运行 core 会落到旧身份（档名 `vpnmgr`、数据目录 `desktop/core/.data`、连 `~/.colima/vpnmgr/docker.sock`，规则一变就对 `127.0.0.1:9090` 发 `PUT /configs`），所以下面的命令都在隔离环境里执行。

- **隔离入口 `iso.sh`**（仓库外：`~/handoff/test_vpn/2026-09-28-implementation-plan/design/iso.sh`，在仓库根执行 `iso.sh <命令> [参数...]`）：`env -i`、临时 HOME 和数据目录、`VPNMGR_DEV_MODE=1`、`VPNMGR_MANAGED_VM=0`、档名 `tongdao-verify-*`、控制口 `http://127.0.0.1:1`、`DOCKER_HOST` 指向不存在的套接字，编译目录默认 `/tmp/tongdao-target/core`（`ISO_TARGET=<目录>` 可换）。

  ```sh
  ISO=~/handoff/test_vpn/2026-09-28-implementation-plan/design/iso.sh
  $ISO cargo test --locked --offline --no-fail-fast --manifest-path desktop/core/Cargo.toml
  $ISO cargo clippy --locked --offline --manifest-path desktop/core/Cargo.toml --all-targets -- -D warnings
  ISO_TARGET=/tmp/tongdao-target/helper $ISO cargo test --locked --offline --manifest-path desktop/helper/Cargo.toml
  $ISO swift build --package-path desktop/tongdao --scratch-path /tmp/tongdao-target/swift-tongdao -Xswiftc -warnings-as-errors
  $ISO swift test --package-path desktop/tongdao --scratch-path /tmp/tongdao-target/swift-tongdao
  $ISO python3 -m unittest discover -s desktop/tongdao/tool-tests -p 'test_*.py'
  $ISO sh -c 'node --test tests/ui/*.test.mjs'
  ```

- **本机模拟服务**：`node tests/ui/mock-server.mjs --scenario <场景> [--self-test]`。只用 Node 自带模块，只绑 `127.0.0.1:0`，伺服 `app/ui`，`/api/*` 从 `tests/ui/fixtures/<场景>/` 读 JSON，不编译 core。场景有 `default`（覆盖已连接、待登录、还没验证、出错、被阻止、已停止）、`empty`、`runtime-down`、`errors`、`slow`、`reset`、`many`、`resume`（启动时恢复在用通道）、`egress-dead`，以及各界面自己的场景（`home`、`channels`、`channels/manage`（更多菜单、修改、删除、恢复上一次设置、恢复分流）、`channel-states`、`add`、`add-progress` 及其下的 `add-progress/<create-fail|headless|preflight-fail|unverified|upload>`、`rules/<proxy|tun|tun-install>`、`settings/<auto|clash|dev|fallback|install|proxy|retry|tun-install>`、`advanced`）。已连接、流量这类画面只能在这里看。
- **隔离核心**：`bash tests/ui/preview.sh` 在仓库根直接运行即可，脚本自带和 `iso.sh` 相同的隔离（自己 `mktemp` 出 HOME 和数据目录，调用方传进来的同名变量一律不用），不需要外层再包一层。它编译 core 到 `/tmp/tongdao-target/core`，塞示例通道，以 `STATIC_DIR=app/ui` 起 core，导入 `tests/ui/fixtures/backup-example.json`，打印地址；浏览器打开 `<地址>/#/home`，Ctrl-C 结束，只停它自己起的进程。`--self-test` 只做隔离自检。没有运行环境，登录、探活、流量看不到，运行态的通道显示「状态待确认」，不把查询失败当作掉线。

需要真实 core 的 Swift 用例在缺少对应环境变量时跳过：`TONGDAO_NATIVE_LIFECYCLE_DIR`（目录须以 `/tmp/tongdao-native-session-` 开头）、`TONGDAO_NATIVE_FIXTURE_DIR`（core 接口输出夹具）。

## 开发版

```sh
desktop/tongdao/dev.sh
```

会编译 `desktop/core` 并起真实的开发用 VM（档名 `tongdao-dev`），`VPNMGR_BUNDLED_IMAGES_DIR` 指向 `desktop/app/bundled-images`，用来导入内置镜像。调试构建要求 `VPNMGR_DEV_MODE=1`、`VPNMGR_CORE_PATH`、非空 `DATA_DIR`，并且 `VPNMGR_VM_PROFILE` 以 `tongdao-` 开头，否则拒绝启动。开发版不接管网络（不装助手、不开系统代理、不写登录项）。只由用户手动运行，代理不运行它。

## 验证到哪一步

**代理已经验证过的**（隔离环境，2026-09-29 最后一次，第四波改动之后）：

- core 单测 387 过、6 项 ignore，集成测试 3 项，clippy 干净（都是 V3 重跑）；助手单测 13 过、`desktop/app` 的 `cargo check` 通过（第四波没改这两处，数字取自之前的包）。旧身份的引擎配置、plist、安装卸载脚本、路由、Clash 片段逐字节不变。
- 本外壳 `-warnings-as-errors` 构建通过（F3），测试 29 项、0 失败、3 项因缺真实 core 夹具跳过（V3 重跑）；发布脚本的工具测试 14 项通过（用替身，不真的编译、签名、打包；第四波没改，没有重跑）。
- 界面 node 单测 143 项通过；`preview.sh --self-test` 通过；接管状态、验证目标、修改、恢复、删除这些接口的真实返回已用隔离核心取过，和夹具核对。
- 除菜单栏面板外的 22 个界面，加上第四波新增的状态（域名规则没有生效、接管重试、更多菜单、修改弹层、删除确认、恢复上一次设置、不参与分流），用模拟服务和隔离核心在 1280×800、1100×720 下截图，和原型对照，页面文字扫过禁词。

以上都没有起 VM、没有真实容器，也没有打开真窗口。

2026-10-03 续作：空闲恢复等待助手完成对账，超时保留重试意图；规则四个视图按实际接管状态显示是否生效；通道列表一次读取运行状态，无法读取时显示「状态待确认」，不误报掉线。连接采样失败返回 503，界面不填假零，恢复第一帧重新建立速度基线。菜单栏同步识别未知状态，不把它计入在线数。

本轮隔离验证：核心 394 项通过、5 项 ignore，集成 3 项通过，clippy 通过；界面 146 项通过；Swift 29 项、3 项跳过、0 失败。规则视图与状态未知页已在独立 headless Chrome 和模拟服务中检查。真实 TUN、VPN 登录、网络接管和长时间稳定性仍未验收。

**要用户亲自做的**（截至 2026-09-29 这四项都还没做）：

1. **UP1 阶段零试验**：用管理员密码运行 `sudo /bin/bash ~/handoff/test_vpn/2026-09-28-implementation-plan/evidence/phase0-tun-dns.sh --out ~/handoff/test_vpn/2026-09-28-implementation-plan/evidence/phase0-tun-dns.md`，按提示在浏览器里打开测试地址。脚本只用 `utun229`、保留网段和虚构域名，不碰旧助手。结论决定「虚拟网卡管域名」做不做；在那之前虚拟网卡只管地址，域名规则由系统代理处理，规则页列出的不生效程序（`curl`、`git`、`ssh`）也等这个结论再定。
2. **UA 开发版首次实测**：在两个核心改动批次之间运行 `dev.sh`，核对 `~/.colima` 下没有新增 `tongdao-dev`、`~/.tongdao/colima` 已建好、两个运行环境的出站进程 PID 不同；内置镜像已导入（运行日志里没有 `bundled_images_failed`，环境检查里 `vpnmgr/oss-vpn:latest` 已就绪）；窗口标题栏和拖动、红绿灯、菜单栏面板（开关通道、「去登录」）、悬浮滚动条；23 个界面和原型对照、窗口拉伸、长名字截断、拼音输入法回车、只用键盘操作、关窗后轮询停止、规则列表和流向图一致。如果 colima 不遵守 `COLIMA_HOME`（两边共用出站进程），要决定接受这个风险还是暂停并行使用。
3. **UB 运行环境实测**：同样用 `dev.sh`，核对控制口 `/rules` 里阻止行的写法、访问被阻止的地址时入口日志有没有命中行；没填验证地址的通道停止再启动后能填地址；阻止或解除时另一条通道上的下载不断；Chrome 用 `--proxy-pac-url` 指向开发版的 PAC，看入口不在时会不会回退直连（PAC 命中分支现在仍带 `DIRECT` 回退）；curl 握手的退出码、单条连接有没有字节字段；测试地址、换配置文件、登录窗、今天流量、启动时恢复在用通道。另外先在旧应用里只导出一份备份，自己对它运行 `$ISO cargo run --locked --offline --manifest-path desktop/core/Cargo.toml --example rule_diff -- <导出文件>` 看规则差异（导出文件里有凭据，不交给代理）。结果决定是否改 PAC 回退和阻止行的读回比较。
4. **UC 候选包与实机验收**：先授权在 `feat/tongdao-app` 上本地提交（出包要求工作区已提交且干净）；运行 `python3 desktop/tongdao/build-release.py --check`，再用 `--output <仓库外、不存在的新目录>` 出候选包；把 `Tongdao.app` 装进 `/Applications`；在应用里「启用虚拟网卡」安装通道管家自己的助手（需要管理员密码）；核对接管方式（Clash 退出再打开、密码框点取消、不覆盖后来改的系统代理、虚拟网卡相关各项）、开机时自动打开、关窗和退出行为、Safari 的 PAC、新旧版本的内存和处理器占用、装回旧版能正常使用；用真实账号登录、逐条启动日常在用的通道、网段重叠的两条通道（先停掉旧应用里的同一条通道）。替换日常在用的应用要另外点头。

**并存期间的两条限制**：

- **两个应用不能同时接管网络。** 通道管家检测到旧应用的虚拟网卡 `utun225` 在跑，或任何一个网络服务（全部网络服务都看，不只当前在用的那个）的自动代理指向旧应用的 PAC，就拒绝打开虚拟网卡和系统代理（「旧版应用正在接管网络，请先在旧版里关掉」），空闲后重新连接时也不恢复系统代理；旧应用没有这层检测，所以要先关一个再开另一个。旧助手开机会按上次状态自动恢复 `utun225`，这时要先在旧应用里关掉虚拟网卡。
- **同一个 VPN 账号不要两边同时登录。** 有的网关会把先登录的一方踢下线。所以「打开后自动连接」默认关，两个应用里同一条通道只在一边启动。
