//! 宿主接管层(spec §6 层1/层2 的「真执行」端 + 层3 TUN 入口)。Rust core 在 Tauri 模型下跑在
//! 宿主 macOS 上,故能**实际执行** `networksetup` / `osascript`(旧 Docker 模型里后端在容器内、
//! 只能展示命令)。
//!
//! - 层1 Clash 订阅:检测本机 Clash/mihomo 客户端 + 生成 Clash Verge Rev 可导入的 merge profile。
//! - 层2 系统代理:`networksetup -setautoproxyurl` 把系统自动代理指向本地 `/entry/proxy.pac`。
//! - 层3 TUN 入口(独立于 Clash 的路由级入口):root helper(`desktop/helper`,ClashX Meta 同款
//!   一次性 sudo 安装的 LaunchDaemon)监管宿主 mihomo#2(TUN 引擎,`auto-route: false` 配置冻结)
//!   并把绑定的 IP-CIDR 对账进路由表——最长前缀比 ClashX TUN 的默认路由更具体,天然共存。
//!   规则变更只动路由表不动 mihomo#2 配置,utun 永不重建。域名规则(拆分 DNS)留 Phase 2。
//!
//! 命门 #4:PAC/节点都指向 `127.0.0.1:{mihomo_host_port}`(VM 转发的 mihomo#1 mixed 口),不外联;
//! mihomo#2 的唯一出站同样只指本机分流口。
//! ⚠️ 仅 macOS;`apply` 类动作会改系统设置,只由前端按钮显式触发(用户知情),后端不自动应用
//! (例外:层3 的**路由对账** `tun_sync` 挂在 rebuild 里自动跑——它只维护用户已显式启用的入口,
//! 不改任何系统设置)。

use serde::Serialize;
use std::time::Duration;

use tokio::process::Command;

// ── 层1:Clash 客户端检测 ────────────────────────────────────────────────────

/// 已知 Clash/mihomo 客户端的配置目录(相对 `$HOME`),存在即视作「装过」。
const CLASH_CONFIG_DIRS: &[&str] = &[
    ".config/clash",
    ".config/mihomo",
    "Library/Application Support/clash",
    "Library/Application Support/io.github.clash-verge-rev.clash-verge",
    "Library/Application Support/com.github.zzzgydi.clash", // 旧 Clash Verge
    "Library/Application Support/cn.clashdev.clashx",        // ClashX
];

#[derive(Debug, Clone, Serialize)]
pub struct ClashDetect {
    /// 9090 控制台是否有 Clash/mihomo 在应答。
    pub running: bool,
    /// 控制台返回的版本(若有)。
    pub version: Option<String>,
    /// 是否 mihomo/Clash.Meta(`meta: true`)。
    pub meta: bool,
    /// 探到的控制台地址(约定 127.0.0.1:9090)。
    pub controller: Option<String>,
    /// 本机存在的已知客户端配置目录(绝对路径)。
    pub config_dirs: Vec<String>,
}

/// 解析 `GET /version` 响应判定 Clash/mihomo(对照用户机实测:`{"meta":true,"version":"v1.19.24"}`)。
pub fn parse_version_json(body: &str) -> (Option<String>, bool) {
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return (None, false),
    };
    let ver = v.get("version").and_then(|x| x.as_str()).map(String::from);
    let meta = v.get("meta").and_then(|x| x.as_bool()).unwrap_or(false);
    (ver, meta)
}

/// 检测本机 Clash:探 127.0.0.1:9090/version(读-only,安全)+ 扫已知配置目录。
pub async fn detect_clash() -> ClashDetect {
    let mut out = ClashDetect {
        running: false,
        version: None,
        meta: false,
        controller: None,
        config_dirs: vec![],
    };
    let client = reqwest::Client::new();
    if let Ok(resp) = client
        .get("http://127.0.0.1:9090/version")
        .timeout(Duration::from_millis(800))
        .send()
        .await
    {
        if resp.status().is_success() {
            if let Ok(body) = resp.text().await {
                let (ver, meta) = parse_version_json(&body);
                out.running = true;
                out.version = ver;
                out.meta = meta;
                out.controller = Some("127.0.0.1:9090".into());
            }
        }
    }
    if let Some(home) = std::env::var("HOME").ok().filter(|s| !s.is_empty()) {
        for d in CLASH_CONFIG_DIRS {
            let p = std::path::Path::new(&home).join(d);
            if p.exists() {
                out.config_dirs.push(p.to_string_lossy().into_owned());
            }
        }
    }
    out
}

/// 生成 Clash Verge Rev「Merge」profile(导入为 Merge 类型,挂在订阅链上自动并入)。
/// `prepend-proxies`/`prepend-rules` 是 Verge 的列表前插扩展键;`rule-providers` 作字典直接深合并。
/// 命门 #2:RULE-SET 引用带 no-resolve(对清单内 IP-CIDR 生效,域名交 vpn-router 侧解析)。
/// 旧身份;按身份取节点名 / 规则集名用 [`verge_merge_profile_for`]。
pub fn verge_merge_profile(mihomo_host_port: &str, ui_port: &str) -> String {
    verge_merge_profile_for(&crate::identity::VPNMGR, mihomo_host_port, ui_port)
}

/// 节点名、规则集名(连同缓存路径)按身份取,两个应用的片段加进同一个 Clash 不撞名;
/// 订阅地址 `/clash/vpn-rules.yaml` 是各自端口上的路由,不随身份变。
pub fn verge_merge_profile_for(id: &crate::identity::Identity, mihomo_host_port: &str, ui_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let ui = if ui_port.is_empty() { "<UI端口>" } else { ui_port };
    let (proxy, provider) = (id.clash_proxy_name, id.clash_provider_name);
    let udp_note = if id.tun_config_frozen { "" } else { "# UDP 不经本工具，按你后面的规则处理。\n" };
    format!(
        "# Clash Verge Rev「Merge」扩展 · 本工具自动生成\n\
         # 用法:Verge → 订阅 → 新建「Merge」类型 → 粘贴本文 → 拖到你的订阅之后启用\n\
         {udp_note}prepend-proxies:\n\
         \x20 - name: {proxy}\n\
         \x20   type: socks5\n\
         \x20   server: 127.0.0.1\n\
         \x20   port: {port}\n\
         rule-providers:\n\
         \x20 {provider}:\n\
         \x20   type: http\n\
         \x20   behavior: classical\n\
         \x20   format: yaml\n\
         \x20   url: http://127.0.0.1:{ui}/clash/vpn-rules.yaml\n\
         \x20   path: ./providers/{provider}.yaml\n\
         \x20   interval: 60\n\
         prepend-rules:\n\
         \x20 - RULE-SET,{provider},{proxy},no-resolve\n"
    )
}

// ── 层2:系统代理(networksetup)──────────────────────────────────────────────

/// 本地 PAC 的 URL(指向 in-process axum 的 `/entry/proxy.pac`)。
pub fn pac_url(ui_port: &str) -> String {
    let ui = if ui_port.is_empty() { "8787" } else { ui_port };
    format!("http://127.0.0.1:{ui}/entry/proxy.pac")
}

/// 从 `route -n get default` 输出里取默认路由网卡(如 `en0`)。
pub fn parse_default_iface(route_out: &str) -> Option<String> {
    for line in route_out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("interface:") {
            let dev = rest.trim();
            if !dev.is_empty() {
                return Some(dev.to_string());
            }
        }
    }
    None
}

/// 从 `networksetup -listnetworkserviceorder` 输出里,按设备名(en0)反查网络服务名(Wi-Fi)。
/// 块形如:`(1) Wi-Fi` 紧跟 `(Hardware Port: Wi-Fi, Device: en0)`。
pub fn parse_service_for_device(listorder_out: &str, device: &str) -> Option<String> {
    let needle = format!("Device: {device})");
    let lines: Vec<&str> = listorder_out.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if line.contains(&needle) {
            // 服务名在前一行 "(N) Name"
            if i > 0 {
                let prev = lines[i - 1].trim();
                if let Some(idx) = prev.find(") ") {
                    return Some(prev[idx + 2..].trim().to_string());
                }
            }
        }
    }
    None
}

/// 解析 `networksetup -getautoproxyurl "<svc>"` 输出 → (url, enabled)。
pub fn parse_autoproxy(get_out: &str) -> (Option<String>, bool) {
    let mut url = None;
    let mut enabled = false;
    for line in get_out.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("URL:") {
            let u = rest.trim();
            if !u.is_empty() && u != "(null)" {
                url = Some(u.to_string());
            }
        } else if let Some(rest) = t.strip_prefix("Enabled:") {
            enabled = rest.trim().eq_ignore_ascii_case("yes");
        }
    }
    (url, enabled)
}

async fn run(cmd: &str, args: &[&str]) -> anyhow::Result<String> {
    run_with_budget(cmd, args, Duration::from_secs(15)).await
}

async fn run_with_budget(cmd: &str, args: &[&str], budget: Duration) -> anyhow::Result<String> {
    let out = tokio::time::timeout(budget, Command::new(cmd).args(args).kill_on_drop(true).output())
        .await.map_err(|_| {
            crate::ev!(warn, "entry", "command_timeout", "网络状态操作超时", {"command":cmd});
            anyhow::anyhow!("{cmd} 执行超时")
        })??;
    if !out.status.success() {
        anyhow::bail!("{cmd} {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// 当前默认路由对应的网络服务名(如 "Wi-Fi");失败 → None。
pub async fn primary_service() -> Option<String> {
    let route = run("route", &["-n", "get", "default"]).await.ok()?;
    let dev = parse_default_iface(&route)?;
    let order = run("networksetup", &["-listnetworkserviceorder"]).await.ok()?;
    parse_service_for_device(&order, &dev)
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemProxyState {
    /// 平台是否支持(仅 macOS)。
    pub supported: bool,
    /// 命中的网络服务名。
    pub service: Option<String>,
    /// 当前自动代理 URL。
    pub url: Option<String>,
    /// 自动代理是否启用。
    pub enabled: bool,
    /// URL 是否正指向本工具的本地 PAC(用于「已接管」判定)。
    pub is_ours: bool,
}

/// 读当前系统自动代理状态(读-only,安全)。
pub async fn system_proxy_status(ui_port: &str) -> SystemProxyState {
    let mut st = SystemProxyState {
        supported: cfg!(target_os = "macos"),
        service: None,
        url: None,
        enabled: false,
        is_ours: false,
    };
    if !st.supported {
        return st;
    }
    let svc = match primary_service().await {
        Some(s) => s,
        None => return st,
    };
    st.service = Some(svc.clone());
    if let Ok(out) = run("networksetup", &["-getautoproxyurl", &svc]).await {
        let (url, enabled) = parse_autoproxy(&out);
        st.is_ours = url.as_deref() == Some(pac_url(ui_port).as_str());
        st.url = url;
        st.enabled = enabled;
    }
    st
}

/// 本机 PAC 地址 `http://127.0.0.1:<端口>/entry/proxy.pac` 里的端口;别的形状 → None。
fn pac_port(url: &str) -> Option<u16> {
    url.strip_prefix("http://127.0.0.1:")?.strip_suffix("/entry/proxy.pac")?.parse().ok()
}

/// 是我们的 PAC:端口在自己身份的端口范围内。不只认当前 `ui_port`——界面端口会重选
/// (`app.rs`),异常退出后残留的是自己旧端口的 PAC,也要认得。
pub fn is_own_pac(url: &str, own_range: &std::ops::RangeInclusive<u16>) -> bool {
    pac_port(url).is_some_and(|port| own_range.contains(&port))
}

/// 是另一个应用的 PAC:同一路径、端口不在自己身份的范围内(旧应用、旧的 Web 栈都算)。
pub fn is_foreign_pac(url: &str, own_range: &std::ops::RangeInclusive<u16>) -> bool {
    pac_port(url).is_some_and(|port| !own_range.contains(&port))
}

/// 一个网络服务的自动代理:(服务名, URL, 是否启用)。
type ServiceProxy = (String, Option<String>, bool);

/// 要关的服务:自动代理已启用、且地址是我们的。
fn services_to_park(services: &[ServiceProxy], is_ours: impl Fn(&str) -> bool) -> Vec<String> {
    services.iter()
        .filter(|(_, url, enabled)| *enabled && url.as_deref().is_some_and(&is_ours))
        .map(|(svc, _, _)| svc.clone())
        .collect()
}

/// 遍历所有网络服务读自动代理(只读)。列不出服务 → Err;单个服务读不到就跳过。
async fn service_proxies() -> anyhow::Result<Vec<ServiceProxy>> {
    let listing = run("networksetup", &["-listallnetworkservices"]).await?;
    let mut out = Vec::new();
    for line in listing.lines().skip(1) {
        // 已停用的服务带 '*' 前缀;服务名本身可含空格,整行即名字
        let svc = line.trim().trim_start_matches('*').trim();
        if svc.is_empty() {
            continue;
        }
        let Ok(get) = run("networksetup", &["-getautoproxyurl", svc]).await else { continue };
        let (url, enabled) = parse_autoproxy(&get);
        out.push((svc.to_string(), url, enabled));
    }
    Ok(out)
}

/// 退出清理:遍历**所有**网络服务,凡自动代理指向本工具 PAC 的一律关闭。
/// 与 system_proxy_apply(false) 的区别:后者只动当前默认路由那一个服务——用户换过网
/// (Wi-Fi→有线)或退出时已离线的话,旧服务上会残留指向已死 127.0.0.1 PAC 的配置
/// (红队 D3)。返回关闭的服务数;全程 best-effort。
pub async fn system_proxy_park_all(ui_port: &str) -> usize {
    let _guard = PROXY_MUTATION_LOCK.lock().await;
    if !cfg!(target_os = "macos") {
        return 0;
    }
    let ours = pac_url(ui_port);
    let Ok(services) = service_proxies().await else { return 0 };
    let mut parked = 0;
    for svc in services_to_park(&services, |url| url == ours) {
        let svc = svc.as_str();
        if run("networksetup", &["-setautoproxystate", svc, "off"]).await.is_ok() {
            parked += 1;
            crate::events::audit("system_proxy_set", "退出清理:系统自动代理已关闭", serde_json::json!({
                "target_kind": "entry", "target_name": "system_proxy", "target_id": svc,
                "before": { "enabled": true, "is_ours": true }, "after": { "enabled": false },
                "operation": "park", "result": "ok"
            }));
        }
    }
    parked
}

/// 显式关闭时只关「已启用且地址是我们的」那些服务(通道管家),不碰用户后来改成的别的代理;
/// 旧身份保持原样:无条件关掉当前默认服务。
fn disable_parks_own_services(id: &crate::identity::Identity) -> bool {
    !std::ptr::eq(id, &crate::identity::VPNMGR)
}

/// 应用/清除本工具的系统自动代理(PAC)。`enable=true` 指向本地 PAC 并开启;false 关闭自动代理。
/// ⚠️ 改系统设置:只应由前端按钮显式触发。返回最终状态。
pub async fn system_proxy_apply(cfg: &crate::config::Config, enable: bool) -> anyhow::Result<SystemProxyState> {
    let _guard = PROXY_MUTATION_LOCK.lock().await;
    if enable { ensure_entry_open()?; }
    if !cfg!(target_os = "macos") {
        anyhow::bail!("系统代理一键应用仅支持 macOS");
    }
    let id = cfg.identity();
    let ui_port = cfg.ui_port.to_string();
    let parks_own = !enable && disable_parks_own_services(id);
    // 审计要能回答「改前是什么」:先取一份现状,失败路径也据此说明什么都没变。
    let before = system_proxy_status(&ui_port).await;
    let snapshot = |st: &SystemProxyState| serde_json::to_value(st).unwrap_or(serde_json::Value::Null);
    let applied = async {
        if enable {
            ensure_no_other_app(cfg, "system_proxy").await?;
        }
        if parks_own {
            let services = service_proxies().await?;
            let parked = services_to_park(&services, |url| is_own_pac(url, &id.port_range));
            for svc in &parked {
                run("networksetup", &["-setautoproxystate", svc, "off"]).await?;
            }
            return anyhow::Ok(parked);
        }
        let svc = primary_service().await.ok_or_else(|| anyhow::anyhow!("找不到默认网络服务"))?;
        if enable {
            let url = pac_url(&ui_port);
            ensure_entry_open()?;
            run("networksetup", &["-setautoproxyurl", &svc, &url]).await?;
            ensure_entry_open()?;
            run("networksetup", &["-setautoproxystate", &svc, "on"]).await?;
        } else {
            run("networksetup", &["-setautoproxystate", &svc, "off"]).await?;
        }
        anyhow::Ok(vec![])
    }
    .await;
    let parked = match applied {
        Ok(parked) => parked,
        Err(e) => {
            crate::events::audit_failed("system_proxy_set", "系统自动代理设置失败", serde_json::json!({
                "target_kind": "entry", "target_name": "system_proxy", "requested_enable": enable,
                "before": snapshot(&before), "after": null, "result": "failed", "error": e.to_string()
            }));
            return Err(e);
        }
    };
    let state = system_proxy_status(&ui_port).await;
    // 显式开关覆盖 idle 暂停前的自动恢复意图。
    IDLE_PROXY_SERVICES.lock().await.clear();
    let mut detail = serde_json::json!({
        "target_kind": "entry", "target_name": "system_proxy",
        "target_id": state.service.clone(), "requested_enable": enable,
        "before": snapshot(&before), "after": snapshot(&state), "result": "ok"
    });
    if parks_own {
        detail["parked_services"] = serde_json::json!(parked);
    }
    crate::events::audit("system_proxy_set",
        if enable { "系统自动代理已启用" } else { "系统自动代理已关闭" },
        detail);
    Ok(state)
}

// ── 互斥接管:另一个应用正在接管网络时不接管(只在通道管家身份下检测)──────────────

const OTHER_APP_TAKEOVER: &str = "旧版应用正在接管网络，请先在旧版里关掉";

/// 另一个应用正在接管网络的信号。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "signal", rename_all = "snake_case")]
pub enum OtherTakeover {
    /// 对方的虚拟网卡在跑(对方引擎停了网卡就会消失)。
    Tun { device: &'static str },
    /// 某个网络服务的自动代理已启用,且指向对方的 PAC。
    SystemProxy { service: String },
}

/// 由读到的现状判定:旧身份恒为「无」;通道管家看对方网卡、再看各服务的自动代理。
fn takeover_signal(id: &crate::identity::Identity, other_tun_up: bool, services: &[ServiceProxy]) -> Option<OtherTakeover> {
    let device = id.watch_other_tun?;
    if other_tun_up {
        return Some(OtherTakeover::Tun { device });
    }
    services.iter()
        .find(|(_, url, enabled)| *enabled && url.as_deref().is_some_and(|url| is_foreign_pac(url, &id.port_range)))
        .map(|(service, _, _)| OtherTakeover::SystemProxy { service: service.clone() })
}

/// 检测另一个应用是否正在接管网络。只读:不连对方的助手,也不读对方的数据目录。
/// 旧身份不检测,直接返回「无」(不执行任何命令)。
pub async fn other_app_takeover(cfg: &crate::config::Config) -> Option<OtherTakeover> {
    let id = cfg.identity();
    let device = id.watch_other_tun?;
    if !cfg!(target_os = "macos") {
        return None;
    }
    let other_tun_up = run("/sbin/ifconfig", &[device]).await.is_ok();
    let services = if other_tun_up { vec![] } else { service_proxies().await.unwrap_or_default() };
    takeover_signal(id, other_tun_up, &services)
}

/// 写任何东西之前的拦截:对方在接管 → 记一条事件并返回错误。
async fn ensure_no_other_app(cfg: &crate::config::Config, operation: &str) -> anyhow::Result<()> {
    if let Some(other) = other_app_takeover(cfg).await {
        crate::ev!(warn, "entry", "other_app_takeover", "旧版应用正在接管网络,本次不接管",
            { "operation": operation, "other": other });
        anyhow::bail!(OTHER_APP_TAKEOVER);
    }
    Ok(())
}

// ── 层3:TUN 入口(root helper + 宿主 mihomo#2)───────────────────────────────
// 服务名、套接字、安装目录(root 属主——装用户可写路径 = 提权洞)、plist、TUN 设备名、占位段、
// 回程路由都按身份取(`crate::identity`):设备名与 helper 侧身份表 pin 死同名,路由管理零猜测;
// 两个应用各连各的助手,绝不给对方的助手发指令。

/// 期望的 helper 版本(与 desktop/helper Cargo.toml 对齐;不符 → UI 提示重装升级)。
pub const HELPER_VERSION: &str = "0.5.0";
static TUN_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static PROXY_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static ENTRY_CLOSING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn begin_shutdown() {
    ENTRY_CLOSING.store(true, std::sync::atomic::Ordering::SeqCst);
}

fn ensure_entry_open() -> anyhow::Result<()> {
    anyhow::ensure!(!ENTRY_CLOSING.load(std::sync::atomic::Ordering::SeqCst), "应用正在退出，不再恢复网络接管");
    Ok(())
}
static TUN_PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// 只有恢复失败才允许后续规则对账重试；空闲释放和退出仍必须保持暂停。
static TUN_RESUME_PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static TUN_SYNC_REVISION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static TUN_SYNC_COMPLETED: tokio::sync::Mutex<u64> = tokio::sync::Mutex::const_new(0);
// 只记本进程为 idle 暂停的服务；恢复前仍核对 URL，外部改为其他代理时不夺回。
static IDLE_PROXY_SERVICES: tokio::sync::Mutex<Vec<String>> = tokio::sync::Mutex::const_new(Vec::new());

async fn idle_networksetup(args: &[&str]) -> anyhow::Result<String> {
    let output = tokio::time::timeout(Duration::from_secs(10),
        Command::new("networksetup").args(args).kill_on_drop(true).output()).await??;
    anyhow::ensure!(output.status.success(), "系统代理状态操作失败");
    Ok(String::from_utf8(output.stdout)?)
}

async fn idle_proxy_status(service: &str) -> anyhow::Result<(Option<String>, bool)> {
    let output = idle_networksetup(&["-getautoproxyurl", service]).await?;
    anyhow::ensure!(output.lines().any(|line| matches!(line.trim(), "Enabled: Yes" | "Enabled: No")),
        "系统代理状态无法确认");
    Ok(parse_autoproxy(&output))
}

fn idle_helper_stopped(value: &serde_json::Value) -> bool {
    value["running"] == false && value["alive"] == false && value["applied"] == 0 && value["pending"] == false
}

/// 空闲释放必须读回确认入口已撤下；与退出的 best-effort 清理不同，失败即保留 VM。
pub async fn park_for_idle(cfg: &crate::config::Config) -> anyhow::Result<()> {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") { return Ok(()); }
    let id = cfg.identity();
    {
        let _guard = TUN_MUTATION_LOCK.lock().await;
        TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
        if std::path::Path::new(id.helper_sock).exists() {
            let status = helper_call(id.helper_sock, serde_json::json!({"cmd":"status"})).await?;
            if !idle_helper_stopped(&status) {
                anyhow::ensure!(tun_enabled(&cfg.data_dir) && status["config"] == tun_mihomo_config_for(id, &cfg.mihomo_host_port),
                    "TUN 入口归属或停止状态待确认，保留运行环境");
                helper_mutation(id.helper_sock, serde_json::json!({"cmd":"stop"})).await?;
                let status = helper_call(id.helper_sock, serde_json::json!({"cmd":"status"})).await?;
                anyhow::ensure!(idle_helper_stopped(&status), "TUN 引擎或路由尚未确认释放");
            }
        } else {
            anyhow::ensure!(!tun_enabled(&cfg.data_dir), "TUN 助手不可达，无法确认路由已释放");
        }
    }
    let _proxy_guard = PROXY_MUTATION_LOCK.lock().await;
    let listing = idle_networksetup(&["-listallnetworkservices"]).await?;
    anyhow::ensure!(listing.lines().next().is_some_and(|line| line.contains("asterisk")), "无法确认系统网络服务列表");
    let ours = pac_url(&cfg.ui_port.to_string());
    let mut parked = IDLE_PROXY_SERVICES.lock().await;
    for service in listing.lines().skip(1).map(|s| s.trim().trim_start_matches('*').trim()).filter(|s| !s.is_empty()) {
        let (url, enabled) = idle_proxy_status(service).await?;
        if enabled && url.as_deref() == Some(&ours) {
            // 写前记住意图，响应丢失后先读回；失败后显式连接仍能恢复已暂停的部分。
            if !parked.iter().any(|s| s == service) { parked.push(service.into()); }
            let result = idle_networksetup(&["-setautoproxystate", service, "off"]).await;
            let (url, enabled) = idle_proxy_status(service).await?;
            anyhow::ensure!(!enabled || url.as_deref() != Some(&ours), "系统代理关闭尚未确认: {:?}", result.err());
        }
    }
    Ok(())
}

/// 空闲恢复时怎么处理一个暂停过的服务。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleResume {
    /// 重新打开自动代理(放行)。
    Enable,
    /// 另一个应用正在接管:这次不恢复,暂停记录留着,对方关掉后下一次连接还能恢复。
    Keep,
    /// 地址已经不是我们的(外部改过),或者已经开着:不动,记录清掉。
    Forget,
}

/// 纯函数:地址仍是我们的、当前未启用,并且另一个应用没在接管,才放行(`Enable`)。
/// 旧身份不检测对方,这一项恒放行。
fn idle_resume_step(id: &crate::identity::Identity, other_app: bool, ours: bool, enabled: bool) -> IdleResume {
    match (ours && !enabled, other_app && id.watch_other_tun.is_some()) {
        (false, _) => IdleResume::Forget,
        (true, true) => IdleResume::Keep,
        (true, false) => IdleResume::Enable,
    }
}

/// 再次连接只恢复本实例保留的入口意图；外部 URL 发生变化的服务不恢复。另一个应用正在接管时,
/// 系统代理也不恢复(`tun_apply` 会自己拦):记一条事件,暂停记录保留,对方关掉后下一次连接还能恢复。
pub async fn resume_after_idle(cfg: &crate::config::Config) -> anyhow::Result<()> {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") { return Ok(()); }
    ensure_entry_open()?;
    if TUN_PARKED.load(std::sync::atomic::Ordering::SeqCst) && tun_enabled(&cfg.data_dir) {
        let _guard = TUN_MUTATION_LOCK.lock().await;
        ensure_entry_open()?;
        if tun_enabled(&cfg.data_dir) {
            resume_tun_locked(cfg).await;
        }
    } else {
        TUN_PARKED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    let _proxy_guard = PROXY_MUTATION_LOCK.lock().await;
    ensure_entry_open()?;
    let ours = pac_url(&cfg.ui_port.to_string());
    let mut parked = IDLE_PROXY_SERVICES.lock().await;
    // 有要恢复的服务才检测(检测要跑 ifconfig / networksetup;旧身份不检测,直接是「无」)。
    let other_app = if parked.is_empty() { None } else { other_app_takeover(cfg).await };
    let mut kept = 0;
    while let Some(service) = parked.get(kept).cloned() {
        let (url, enabled) = idle_proxy_status(&service).await?;
        match idle_resume_step(cfg.identity(), other_app.is_some(), url.as_deref() == Some(&ours), enabled) {
            IdleResume::Keep => {
                kept += 1;
                continue;
            }
            IdleResume::Enable => {
                ensure_entry_open()?;
                let result = idle_networksetup(&["-setautoproxystate", &service, "on"]).await;
                let (url, enabled) = idle_proxy_status(&service).await?;
                anyhow::ensure!(url.as_deref() != Some(&ours) || enabled, "系统代理恢复尚未确认: {:?}", result.err());
            }
            IdleResume::Forget => {}
        }
        parked.remove(kept);
    }
    if let Some(other) = other_app.filter(|_| kept > 0) {
        crate::ev!(warn, "entry", "other_app_takeover", "旧版应用正在接管网络,本次不恢复系统代理",
            { "operation": "resume", "other": other, "kept": kept });
    }
    Ok(())
}

fn tun_request(cfg: &crate::config::Config) -> anyhow::Result<serde_json::Value> {
    let rules = crate::store::effective_rules(&cfg.db_path())?;
    // Phase-zero system resolver and VM DNS anti-loop gates are still pending.
    // Keep activation separate from preparing the complete DNS-capable request.
    tun_request_for_rules(cfg.identity(), &cfg.mihomo_host_port, &rules, None)
}

pub fn tun_request_for_rules(id: &crate::identity::Identity, host_port: &str,
    rules: &[crate::store::Rule], dns_port: Option<u16>) -> anyhow::Result<serde_json::Value> {
    let (mut v4, v6) = route_sets_for(id, rules);
    let mut request = serde_json::json!({"cmd":"ensure", "config":tun_mihomo_config_for(id, host_port), "v4":v4, "v6":v6});
    if let Some(port) = dns_port {
        anyhow::ensure!(!id.tun_config_frozen, "旧身份不支持 TUN 域名配置");
        let domains = tun_domains(rules);
        for rule_id in domains.skipped {
            crate::ev!(warn, "entry", "tun_domain_skipped", "域名规则不能交给系统解析器处理", {"rule_id":rule_id});
        }
        v4.push("198.19.128.0/17".into()); v4.sort(); v4.dedup();
        request["v4"] = serde_json::json!(v4);
        request["config"] = tun_mihomo_config_with_dns(id, host_port, port)?.into();
        request["dns_port"] = port.into();
        request["domains"] = serde_json::json!(domains.domains);
    }
    Ok(request)
}

pub struct TunDomains { pub domains: Vec<String>, pub skipped: Vec<i64> }

pub fn tun_domains(rules: &[crate::store::Rule]) -> TunDomains {
    let mut domains = std::collections::BTreeSet::new();
    let mut skipped = vec![];
    for rule in rules.iter().filter(|r| r.enabled != 0 && r.kind == "domain") {
        let pattern = rule.pattern.strip_prefix("*.").unwrap_or(&rule.pattern);
        let domain = (|| {
            // Reject URL syntax before IDNA conversion; this is a suffix, not a URL.
            if pattern.chars().any(|c| c.is_whitespace() || "*/\\:@?#%".contains(c)) { return None; }
            let url = reqwest::Url::parse(&format!("http://{pattern}" )).ok()?;
            url.host_str().map(String::from)
        })();
        let domain = domain.filter(|d| !d.is_empty() && d.len() <= 253 && d != "local" && !d.ends_with(".local")
            && d.parse::<std::net::IpAddr>().is_err()
            && d.split('.').all(|label| !label.is_empty() && label.len() <= 63
                && label.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')));
        if let Some(domain) = domain { domains.insert(domain); } else { skipped.push(rule.id); }
    }
    TunDomains { domains: domains.into_iter().collect(), skipped }
}

/// 调用方持入口锁。失败保留恢复意图，但不使 VM 启动失败，也不阻断代理恢复。
async fn resume_tun_locked(cfg: &crate::config::Config) {
    let result = async {
        if let Err(error) = ensure_no_other_app(cfg, "resume").await {
            if !cfg.identity().tun_config_frozen {
                let _ = helper_mutation(cfg.identity().helper_sock, serde_json::json!({"cmd":"stop"})).await;
            }
            return Err(error);
        }
        let request = tun_request(cfg)?;
        ensure_helper_applied(cfg.identity().helper_sock, request, Duration::from_secs(10), Duration::from_millis(500)).await
    }.await;
    TUN_PARKED.store(result.is_err(), std::sync::atomic::Ordering::SeqCst);
    TUN_RESUME_PENDING.store(result.is_err(), std::sync::atomic::Ordering::SeqCst);
    if let Err(error) = result {
        crate::ev!(warn, "entry", "tun_resume_deferred", "运行环境已恢复，虚拟网卡入口仍待确认", {"error":error.to_string()});
    }
}

/// 等待真实助手完成该代配置，整个 IPC 和轮询共享同一预算；超时不重放写请求。
async fn ensure_helper_applied(sock: &str, request: serde_json::Value, budget: Duration, interval: Duration) -> anyhow::Result<()> {
    tokio::time::timeout(budget, async {
        let mut status = helper_call(sock, serde_json::json!({"cmd":"status"})).await?;
        require_helper_lease(sock, &status)?;
        if !helper_has_desired(&status, &request) {
            status = helper_mutation(sock, request.clone()).await?;
        }
        loop {
            anyhow::ensure!(helper_has_desired(&status, &request), "助手目标已改变，等待下一次对账");
            if status["pending"] == false && status["alive"] == true { return Ok(()); }
            tokio::time::sleep(interval).await;
            status = helper_call(sock, serde_json::json!({"cmd":"status"})).await?;
        }
    }).await.map_err(|_| anyhow::anyhow!("TUN 入口恢复仍待确认"))?
}

fn helper_has_desired(status: &serde_json::Value, request: &serde_json::Value) -> bool {
    status["running"] == true && ["config", "v4", "v6"].iter().all(|key| status[*key] == request[*key])
        && match request.get("dns_port") {
            Some(port) => status["dns_port"] == *port && status["domains"] == request["domains"],
            None => status["dns_port"].as_u64().unwrap_or(0) == 0
                && status["domains"].as_array().is_none_or(|domains| domains.is_empty()),
        }
}

fn require_helper_lease(sock: &str, status: &serde_json::Value) -> anyhow::Result<()> {
    if sock == crate::identity::TONGDAO.helper_sock {
        anyhow::ensure!(status["version"] == HELPER_VERSION, "TUN 助手需要升级后才能启用租约保护");
    }
    Ok(())
}

fn renew_request(generation: u64, upstream_ok: bool) -> serde_json::Value {
    serde_json::json!({"cmd":"renew", "generation":generation, "lease_secs":90, "upstream_ok":upstream_ok})
}

async fn renew_helper(sock: &str, upstream_ok: bool) -> anyhow::Result<()> {
    let status = helper_call(sock, serde_json::json!({"cmd":"status"})).await?;
    anyhow::ensure!(status["version"] == HELPER_VERSION, "TUN 助手需要升级");
    let generation = status["generation"].as_u64().ok_or_else(|| anyhow::anyhow!("助手未返回代次"))?;
    let response = helper_call(sock, renew_request(generation, upstream_ok)).await?;
    anyhow::ensure!(response["generation"] == generation && response["upstream_ok"] == upstream_ok
        && response["lease_remaining"].as_u64().is_some_and(|secs| secs > 0), "助手租约续期尚未确认");
    Ok(())
}

/// 只续自己的当前代次，不修改路由意图；旧身份和已暂停的入口不续租。
pub async fn tun_renew(cfg: &crate::config::Config, upstream_ok: bool) {
    if cfg.identity().tun_config_frozen || !cfg.host_integrations_allowed() || !cfg!(target_os="macos") { return; }
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        let _guard = TUN_MUTATION_LOCK.lock().await;
        if ensure_entry_open().is_err() || TUN_PARKED.load(std::sync::atomic::Ordering::SeqCst) || !tun_enabled(&cfg.data_dir) { return anyhow::Ok(()); }
        let sock = cfg.identity().helper_sock;
        renew_helper(sock, upstream_ok).await
    }).await;
    match result {
        Ok(Ok(())) => {},
        other => { crate::ev!(warn, "entry", "tun_renew_failed", "虚拟网卡租约续期失败", {"error":format!("{other:?}")}); }
    }
}

/// mihomo#2 冻结配置(唯一动态值 = 分流口)。要点全部来自实测/源码调研:
/// - `auto-route: false`:不抢默认路由,由 helper 按绑定 IP-CIDR 加最长前缀路由,与 ClashX TUN 共存;
/// - `dns-hijack: []`:默认值是劫持一切经 tun 的 :53,必须显式置空(否则绑定的内网 DNS 服务器被劫);
/// - `fake-ip-range: 198.19.0.1/16`:tun 地址派生自它(enable:false 也生效),默认 198.18.0.1/30
///   正撞 ClashX fake-ip 池;**此块永久冻结**——变更会导致 utun 重建、路由被内核清空;
/// - `stack: system`:官方推荐,macOS gvisor 栈有自代理回环 issue;
/// - MATCH 全量进 socks5(进 utun 的流量全是主动路由进来的),不留 DIRECT 回环语义;
/// - `disable-icmp-forwarding: true`(只对 `tun_config_frozen == false` 的身份,见 [`tun_mihomo_config_for`]):
///   引擎默认把进 utun 的 ping 经 ICMP 套接字转发到真实目的地址,内核按助手加的 `/32` 等路由又送回 utun,
///   无限回环(2026-10-01 实测:utun 每秒 8 万多个 84 字节包、引擎常驻 180% CPU)。socks5 承载不了 ICMP,
///   转发在这个架构里本来就到不了内网,关掉没有功能损失。
///
/// 旧身份;设备名、占位段按身份取用 [`tun_mihomo_config_for`](两个应用的网卡与地址互不重叠)。
pub fn tun_mihomo_config(mihomo_host_port: &str) -> String {
    tun_mihomo_config_for(&crate::identity::VPNMGR, mihomo_host_port)
}

pub fn tun_mihomo_config_for(id: &crate::identity::Identity, mihomo_host_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "7899" } else { mihomo_host_port };
    let (name, device, fake_ip_range) = (id.name, id.tun_device, id.fake_ip_range);
    // 旧身份的配置文本逐字节冻结(助手按文本比对,变一个字节就重建网卡),ICMP 回环的修法只加给未冻结的身份。
    let icmp = if id.tun_config_frozen { "" } else { "  disable-icmp-forwarding: true\n" };
    // SSH -L 只承载 TCP；不能让 UDP 的 SOCKS 中继包再绕进本机 TUN。
    let udp = if id.tun_config_frozen { "" } else { "  - NETWORK,udp,REJECT\n" };
    format!(
        "# {name} 生成 · mihomo#2(宿主 TUN 入口引擎)· 配置冻结:规则变更只动路由表\n\
         log-level: warning\n\
         mode: rule\n\
         tun:\n\
         \x20 enable: true\n\
         \x20 device: {device}\n\
         \x20 stack: system\n\
         \x20 auto-route: false\n\
         \x20 auto-detect-interface: false\n\
         \x20 dns-hijack: []\n\
         {icmp}\
         dns:\n\
         \x20 enable: false\n\
         \x20 fake-ip-range: {fake_ip_range}\n\
         proxies:\n\
         \x20 - name: vpn-entry\n\
         \x20   type: socks5\n\
         \x20   server: 127.0.0.1\n\
         \x20   port: {port}\n\
         \x20   udp: true\n\
         rules:\n\
         {udp}\
         \x20 - MATCH,vpn-entry\n"
    )
}

/// Prepare the DNS-capable entry without changing the frozen legacy generator.
pub fn tun_mihomo_config_with_dns(id: &crate::identity::Identity, host_port: &str, dns_port: u16) -> anyhow::Result<String> {
    anyhow::ensure!(!id.tun_config_frozen && id.port_range.contains(&dns_port), "TUN DNS 端口或身份无效");
    let mut config: serde_yaml::Value = serde_yaml::from_str(&tun_mihomo_config_for(id, host_port))?;
    config["ipv6"] = false.into();
    config["dns"] = serde_yaml::to_value(serde_json::json!({
        "enable":true, "listen":format!("127.0.0.1:{dns_port}"),
        "enhanced-mode":"fake-ip", "fake-ip-range":id.fake_ip_range, "ipv6":false,
        "fake-ip-filter":[], "use-hosts":false, "use-system-hosts":false,
        // This listener only assigns synthetic IPv4 addresses. Never recurse into
        // macOS's resolver, which points the same domain back at this listener.
        "nameserver":["127.0.0.1:1"]
    }))?;
    config["profile"] = serde_yaml::to_value(serde_json::json!({"store-fake-ip":true}))?;
    Ok(serde_yaml::to_string(&config)?)
}

/// LaunchDaemon plist(root:wheel 644,由安装脚本落盘)。旧身份;按身份取用 [`helper_plist_for`]。
pub fn helper_plist() -> String {
    helper_plist_for(&crate::identity::VPNMGR)
}

/// 结构不随身份变,只换服务名与目录;助手按自己所在目录选身份(二进制名两边相同)。
pub fn helper_plist_for(id: &crate::identity::Identity) -> String {
    let (label, dir) = (id.helper_label, id.helper_dir);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{label}</string>
    <key>Program</key><string>{dir}/vpnmgr-helper</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>StandardErrorPath</key><string>{dir}/helper.log</string>
</dict>
</plist>
"#
    )
}

/// POSIX shell 单引号转义:把字符串包成 `'...'`,内部 `'` → `'\''`。
/// 中和 `$(...)`/反引号/双引号等一切元字符——.app 路径可被用户改名含 `$`/反引号,
/// 直接插进 root bash 会命令注入(命门:安装脚本经 osascript 以 root 跑)。
pub fn sh_squote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 一次性 sudo 安装脚本(osascript 管理员密码执行)。ClashX Meta 同款模型:
/// launchd 不校验 daemon 签名,ad-hoc app 也能装(SMAppService 在 ad-hoc 下是死路,Apple DTS 确认)。
/// `xattr -c` 清 quarantine(经 dmg 分发的二进制会被 Gatekeeper 拦 exec)。
/// `owner_uid` 落 owner.uid,helper 据此做 peer-uid 鉴权(只放行 root + 安装用户)。
/// 源路径经 [`sh_squote`] 转义防注入;目标全取自身份表常量。旧身份;按身份取用 [`install_script_for`]。
pub fn install_script(helper_src: &str, mihomo_src: &str, owner_uid: u32) -> String {
    install_script_for(&crate::identity::VPNMGR, helper_src, mihomo_src, owner_uid)
}

pub fn install_script_for(id: &crate::identity::Identity, helper_src: &str, mihomo_src: &str, owner_uid: u32) -> String {
    let (label, dir, plist_path) = (id.helper_label, id.helper_dir, id.helper_plist);
    format!(
        "#!/bin/bash\n\
         set -e\n\
         mkdir -p {dir}\n\
         cp {helper_q} {dir}/vpnmgr-helper\n\
         cp {mihomo_q} {dir}/mihomo\n\
         printf '%s' {owner_uid} > {dir}/owner.uid\n\
         xattr -c {dir}/vpnmgr-helper {dir}/mihomo 2>/dev/null || true\n\
         chown -R root:wheel {dir}\n\
         chmod 755 {dir} {dir}/vpnmgr-helper {dir}/mihomo\n\
         chmod 644 {dir}/owner.uid\n\
         cat > {plist_path} <<'PLIST'\n\
         {plist}PLIST\n\
         chown root:wheel {plist_path}\n\
         chmod 644 {plist_path}\n\
         launchctl bootout system/{label} 2>/dev/null || true\n\
         launchctl bootstrap system {plist_path}\n",
        helper_q = sh_squote(helper_src),
        mihomo_q = sh_squote(mihomo_src),
        plist = helper_plist_for(id)
    )
}

/// 卸载脚本:bootout(launchd 会连带杀掉 mihomo 子进程,pkill 兜底)+ 删 plist + 删目录。
/// utun 随 mihomo 退出销毁,内核自动清光挂在其上的路由——无残留。旧身份;按身份取用 [`uninstall_script_for`]。
pub fn uninstall_script() -> String {
    uninstall_script_for(&crate::identity::VPNMGR)
}

/// 只动本身份的服务、引擎、plist 与目录;两边 `pkill -f <目录>/mihomo` 的匹配串互不包含。
pub fn uninstall_script_for(id: &crate::identity::Identity) -> String {
    let (label, dir, plist_path) = (id.helper_label, id.helper_dir, id.helper_plist);
    if id.helper_label == crate::identity::TONGDAO.helper_label {
        return format!(
            "#!/bin/bash\n\
             set -e\n\
             launchctl bootout system/{label} 2>/dev/null || true\n\
             if launchctl print system/{label} >/dev/null 2>&1; then exit 1; fi\n\
             if [ -f {dir}/resolver-owned.json ]; then {dir}/vpnmgr-helper --cleanup-resolver; fi\n\
             pkill -f {dir}/mihomo 2>/dev/null || true\n\
             rm -f {plist_path}\n\
             rm -rf {dir}\n"
        );
    }
    format!(
        "#!/bin/bash\n\
         launchctl bootout system/{label} 2>/dev/null || true\n\
         pkill -f {dir}/mihomo 2>/dev/null || true\n\
         rm -f {plist_path}\n\
         rm -rf {dir}\n"
    )
}

/// 从 rules 提取启用的 IP 规则 → 去重排序的 (v4, v6) CIDR 集(跨通道去重:同网段可挂多通道,
/// 路由表按目的网段唯一)。域名规则不参与(Phase 2 拆分 DNS)。旧身份;按身份取用 [`route_sets_for`]。
pub fn route_sets(rules: &[crate::store::Rule]) -> (Vec<String>, Vec<String>) {
    route_sets_for(&crate::identity::VPNMGR, rules)
}

/// v4 恒含身份的 NAT 回程网段(`nat_return_v4`,旧身份 198.19.0.0/30):mihomo#2 system 栈的
/// tun 地址(如 198.19.0.1/30)的对端(198.19.0.2)是 tun2socket 改写后的注入源,内核回 SYN-ACK
/// 须有此路由才能送回 utun。macOS 点对点 utun 只自带本端主机路由,不生成 /30 连接路由;网络切换
/// 还会刷掉手工路由——所以必须由 helper 与规则路由同生命周期一并对账持有,否则症状 =
/// 「探活绿但页面打不开」(SYN_SENT,output no route)。两个身份的回程网段必须不同:同网段时后启用的
/// 一方被助手记成 shadowed,回包进了另一块网卡。
pub fn route_sets_for(id: &crate::identity::Identity, rules: &[crate::store::Rule]) -> (Vec<String>, Vec<String>) {
    // BTreeSet 一次到位:跨通道去重 + 排序(路由表按目的网段唯一,顺序稳定便于对账/测试)。
    let mut v4 = std::collections::BTreeSet::new();
    v4.insert(id.nat_return_v4.to_string());
    let mut v6 = std::collections::BTreeSet::new();
    for r in rules {
        if r.enabled == 0 || r.kind != "ip" {
            continue;
        }
        // 命门②:分桶前先 parse::<IpNet>——按解析结果的 v4/v6 分桶(替代裸 contains(':')),
        // 畸形串告警跳过,绝不下发 helper 变成宿主路由。
        match r.pattern.parse::<ipnet::IpNet>() {
            // 通道管家:让开通道正在连接的服务器地址,否则隧道经虚拟网卡绕回自己(见 server_guard)。
            Ok(ipnet::IpNet::V4(net)) => match (!id.tun_config_frozen).then(|| crate::server_guard::carve(net)).flatten() {
                Some(parts) => { v4.extend(parts.into_iter().map(|p| p.to_string())); }
                None => { v4.insert(r.pattern.clone()); }
            },
            Ok(ipnet::IpNet::V6(_)) => {
                if id.tun_config_frozen { v6.insert(r.pattern.clone()); }
            }
            Err(e) => {
                crate::ev!(warn, "entry", "route_cidr_skipped", "已跳过畸形 CIDR 规则",
                    { "rule_id": r.id, "reason": "invalid_cidr", "error": e.to_string() });
            }
        }
    }
    (v4.into_iter().collect(), v6.into_iter().collect())
}

/// helper 二进制资源目录(app 壳设 env `HELPER_RES_DIR` → Resources/runtime/helper;
/// dev 直跑 core 时可手动 export 指向构建产物)。
fn helper_res() -> Option<(String, String)> {
    let dir = std::env::var("HELPER_RES_DIR").ok().filter(|s| !s.is_empty())?;
    let helper = format!("{dir}/vpnmgr-helper");
    let mihomo = format!("{dir}/mihomo");
    (std::path::Path::new(&helper).exists() && std::path::Path::new(&mihomo).exists())
        .then_some((helper, mihomo))
}

/// TUN 入口启用标记(用户显式启用后 rebuild 才会自动对账路由)。
fn tun_flag_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("tun-entry.json")
}

pub fn tun_enabled(data_dir: &std::path::Path) -> bool {
    std::fs::read_to_string(tun_flag_path(data_dir))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("enabled").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

pub fn set_tun_enabled(data_dir: &std::path::Path, enabled: bool) -> anyhow::Result<()> {
    std::fs::write(tun_flag_path(data_dir), serde_json::json!({ "enabled": enabled }).to_string())?;
    Ok(())
}

/// 单次 IPC 调用:一行 JSON 请求 → 一行 JSON 响应。helper 不在 → Err(连接失败)。
/// 套接字必须由调用方按身份显式传入(`cfg.identity().helper_sock`),不设默认值:
/// 连错套接字就是给另一个应用的助手发指令。
pub async fn helper_call(sock: &str, req: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::UnixStream::connect(sock),
    )
    .await
    .map_err(|_| anyhow::anyhow!("连接 helper 超时"))??;
    let mut payload = serde_json::to_vec(&req)?;
    payload.push(b'\n');
    tokio::time::timeout(Duration::from_secs(2), async {
        s.write_all(&payload).await?;
        s.shutdown().await
    }).await.map_err(|_| anyhow::anyhow!("发送 helper 请求超时,状态待确认"))??;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("等待 helper 响应超时"))??;
    parse_helper_response(&buf)
}

fn parse_helper_response(buf: &[u8]) -> anyhow::Result<serde_json::Value> {
    let value: serde_json::Value = serde_json::from_slice(buf)?;
    anyhow::ensure!(value.get("ok").and_then(|v| v.as_bool()) == Some(true), "helper: {}",
        value.get("error").and_then(|v| v.as_str()).unwrap_or("invalid acknowledgement"));
    Ok(value)
}

/// 调用者持有 TUN_MUTATION_LOCK；读取当前代次后提交下一代,必须收到同代耐久 ACK。
async fn helper_mutation(sock: &str, mut req: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let stopping = req["cmd"] == "stop";
    if !stopping && sock == crate::identity::TONGDAO.helper_sock {
        req["lease_secs"] = 90.into();
        if req.get("upstream_ok").is_none() { req["upstream_ok"] = true.into(); }
    }
    let current = helper_call(sock, serde_json::json!({"cmd":"status"})).await?;
    if !stopping { require_helper_lease(sock, &current)?; }
    let Some(generation) = current.get("generation").and_then(|v| v.as_u64()) else {
        // 升级前也必须能停止旧助手,避免退出留下旧入口。
        if stopping {
            let response = helper_call(sock, req).await?;
            ensure_helper_stopped(&response)?;
            return Ok(response);
        }
        anyhow::bail!("TUN 助手需要升级后才能应用路由");
    };
    let next = generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("helper generation exhausted"))?;
    req["generation"] = serde_json::json!(next);
    let response = match helper_call(sock, req.clone()).await {
        Ok(response) => response,
        Err(error) => {
            // 写结果不明时只读回确认,不能盲目重放 ensure/stop。
            match helper_call(sock, serde_json::json!({"cmd":"status"})).await {
                Ok(response) if helper_matches_request(&response, &req, next) => response,
                _ => return Err(error),
            }
        }
    };
    anyhow::ensure!(response.get("generation").and_then(|v| v.as_u64()) == Some(next), "helper ACK 代次不匹配,状态待确认");
    if stopping { ensure_helper_stopped(&response)?; }
    Ok(response)
}

fn helper_matches_request(response: &serde_json::Value, req: &serde_json::Value, generation: u64) -> bool {
    response["generation"].as_u64() == Some(generation) && if req["cmd"] == "stop" {
        response["running"] == false
    } else {
        response["running"] == true && ["config", "v4", "v6"].iter().all(|key| response[*key] == req[*key])
    }
}

fn ensure_helper_stopped(value: &serde_json::Value) -> anyhow::Result<()> {
    anyhow::ensure!(value["running"] == false && value["alive"] == false, "助手已接收停止请求,但尚未确认引擎退出");
    Ok(())
}

/// 层3 综合状态(驱动前端卡片;读-only)。
pub async fn tun_status(cfg: &crate::config::Config) -> serde_json::Value {
    if !cfg.host_integrations_allowed() {
        return serde_json::json!({"supported": false, "enabled": false, "installed": false,
            "reason": "隔离实例禁用宿主 TUN 和助手变更"});
    }
    let id = cfg.identity();
    let supported = cfg!(target_os = "macos");
    let resources = helper_res().is_some();
    let installed = std::path::Path::new(id.helper_plist).exists();
    let enabled = tun_enabled(&cfg.data_dir);
    let helper = helper_call(id.helper_sock, serde_json::json!({ "cmd": "status" })).await.ok();
    let (v4, v6) = crate::store::effective_rules(&cfg.db_path())
        .map(|r| route_sets_for(id, &r))
        .unwrap_or_default();
    let config_current = helper
        .as_ref()
        .and_then(|h| h.get("config").and_then(|c| c.as_str()))
        .map(|c| c == tun_mihomo_config_for(id, &cfg.mihomo_host_port));
    serde_json::json!({
        "supported": supported,
        "resources": resources,
        "installed": installed,
        "enabled": enabled,
        "helper": helper,
        "config_current": config_current,
        "expected_version": HELPER_VERSION,
        "device": id.tun_device,
        "desired_v4": v4,
        "desired_v6": v6,
    })
}

/// 启用/停用 TUN 入口(前端按钮显式触发)。
/// 启用:**先** ensure 成功**才**落 enabled 标记——helper 不可达时不留「标记 on 却没生效」的假态;
/// 停用:先清标记(挡住 tun_sync 继续重放)再 stop;stop 失败不吞,回状态里挂 warning
/// (helper 用 state.json + KeepAlive 自恢复,停不掉时它会复活,须让用户知道)。
pub async fn tun_apply(cfg: &crate::config::Config, enable: bool) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(cfg.host_integrations_allowed(), "隔离实例禁用宿主 TUN 和助手变更");
    let id = cfg.identity();
    let _guard = TUN_MUTATION_LOCK.lock().await;
    if enable {
        ensure_entry_open()?;
        ensure_no_other_app(cfg, "enable").await?;
        let rules = crate::store::effective_rules(&cfg.db_path()).unwrap_or_default();
        let (v4, v6) = route_sets_for(id, &rules);
        let desired_total = v4.len() + v6.len();
        let response = helper_mutation(id.helper_sock, serde_json::json!({
            "cmd": "ensure",
            "config": tun_mihomo_config_for(id, &cfg.mihomo_host_port),
            "v4": v4,
            "v6": v6,
        }))
        .await
        .map_err(|e| {
            crate::ev!(error, "entry", "tun_helper_unreachable", "TUN 助手不可达",
                { "operation": "enable", "error": e.to_string() });
            anyhow::anyhow!("helper 不可达(未安装或未运行):{e}")
        })?;
        set_tun_enabled(&cfg.data_dir, true)?;
        TUN_PARKED.store(false, std::sync::atomic::Ordering::SeqCst);
        TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        crate::ev!(info, "entry", "tun_ensure", "TUN 路由目标已下发", {
            "operation": "enable", "total": desired_total,
            "applied": response.get("applied").and_then(|v| v.as_u64()).unwrap_or(0),
            "shadowed": response.get("shadowed").and_then(|v| v.as_u64()).unwrap_or(0)
        });
        Ok(tun_status(cfg).await)
    } else {
        set_tun_enabled(&cfg.data_dir, false)?;
        TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        let stop_err = helper_mutation(id.helper_sock, serde_json::json!({ "cmd": "stop" })).await.err();
        let mut st = tun_status(cfg).await;
        if let Some(e) = stop_err {
            crate::ev!(error, "entry", "tun_helper_unreachable", "TUN 停用标记已清除,但助手停止失败", {
                "operation": "disable", "error": e.to_string()
            });
            if let Some(m) = st.as_object_mut() {
                m.insert(
                    "warning".into(),
                    serde_json::json!(format!("已清除启用标记,但没能连上助手停止引擎(它可能自恢复):{e}")),
                );
            }
        }
        Ok(st)
    }
}

/// 对账放行:允许宿主集成、用户已启用、没有暂停,且另一个应用没在接管。
fn tun_sync_should_ensure(allowed: bool, enabled: bool, parked: bool, other_app: bool) -> bool {
    allowed && enabled && !parked && !other_app
}

/// 规则变更后的路由对账(挂在 manager::rebuild 末尾,best-effort)。
/// 只在用户已显式启用时动作;顺带自愈:分流口变了 → ensure 会带新配置让 helper 重拉 mihomo。
pub async fn tun_sync(cfg: &crate::config::Config) {
    use std::sync::atomic::Ordering::SeqCst;
    // 多次 rebuild 同时排队时，第一位读取最新规则，后面的不重复做宿主查询或写助手。
    let revision = TUN_SYNC_REVISION.fetch_add(1, SeqCst) + 1;
    let mut completed = TUN_SYNC_COMPLETED.lock().await;
    if revision <= *completed { return; }
    let observed = TUN_SYNC_REVISION.load(SeqCst);
    tun_sync_once(cfg).await;
    *completed = observed;
}

async fn tun_sync_once(cfg: &crate::config::Config) {
    let _guard = TUN_MUTATION_LOCK.lock().await;
    let allowed = cfg.host_integrations_allowed() && cfg!(target_os = "macos")
        && !ENTRY_CLOSING.load(std::sync::atomic::Ordering::SeqCst);
    let enabled = allowed && tun_enabled(&cfg.data_dir);
    if allowed && enabled && TUN_RESUME_PENDING.load(std::sync::atomic::Ordering::SeqCst) {
        resume_tun_locked(cfg).await;
        return;
    }
    let parked = TUN_PARKED.load(std::sync::atomic::Ordering::SeqCst);
    // 启动时的 rebuild 会走到这里(退出时保留了启用标记),不经过 tun_apply 的拦截,须自己检测;
    // 前三项放行了才检测(检测要跑 ifconfig / networksetup)。
    let other_app = tun_sync_should_ensure(allowed, enabled, parked, false)
        && ensure_no_other_app(cfg, "sync").await.is_err();
    if other_app && !cfg.identity().tun_config_frozen {
        TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
        TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
        if let Err(error) = helper_mutation(cfg.identity().helper_sock, serde_json::json!({"cmd":"stop"})).await {
            crate::ev!(warn, "entry", "tun_other_app_park_failed", "另一应用接管网络，本入口等待停止", {"error":error.to_string()});
        }
    }
    if !tun_sync_should_ensure(allowed, enabled, parked, other_app) {
        return;
    }
    let rules = match crate::store::effective_rules(&cfg.db_path()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let id = cfg.identity();
    let (v4, v6) = route_sets_for(id, &rules);
    let desired_total = v4.len() + v6.len();
    let request = serde_json::json!({
        "cmd": "ensure",
        "config": tun_mihomo_config_for(id, &cfg.mihomo_host_port),
        "v4": v4,
        "v6": v6,
    });
    match helper_ensure_if_changed(id.helper_sock, request)
    .await
    {
        Ok(response) => {
            crate::ev!(info, "entry", "tun_ensure", "TUN 路由目标已对账", {
                "operation": "sync", "total": desired_total,
                "applied": response.get("applied").and_then(|v| v.as_u64()).unwrap_or(0),
                "shadowed": response.get("shadowed").and_then(|v| v.as_u64()).unwrap_or(0)
            });
        }
        Err(e) => {
            crate::ev!(error, "entry", "tun_helper_unreachable", "TUN 路由对账失败,助手不可达",
                { "operation": "sync", "error": e.to_string() });
        }
    }
}

async fn helper_ensure_if_changed(sock: &str, request: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let status = helper_call(sock, serde_json::json!({"cmd":"status"})).await?;
    require_helper_lease(sock, &status)?;
    // 已接受但尚在应用的同一目标也不重发，否则代次会不停增长、永远 pending。
    if helper_has_desired(&status, &request) { return Ok(status); }
    helper_mutation(sock, request).await
}

/// 退出清理:让 helper 停掉 mihomo#2 并回收路由,但**保留启用标记**——
/// 与 tun_apply(false) 的区别在于用户意图不变:下次启动 rebuild 尾部的 tun_sync
/// 会自动重新下发,无需再开一次开关。app 退出后 mihomo#2 的上游(分流口转发)
/// 已死,路由若残留会把命中网段变黑洞,故必须停。
pub async fn tun_park(cfg: &crate::config::Config) {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") || !tun_enabled(&cfg.data_dir) {
        return;
    }
    TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
    TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _guard = TUN_MUTATION_LOCK.lock().await;
    // 等锁期间可能有恢复操作完成；停止操作拿到锁后再次固定暂停状态。
    TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
    TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
    match helper_mutation(cfg.identity().helper_sock, serde_json::json!({ "cmd": "stop" })).await {
        Ok(_) => {
            crate::ev!(info, "entry", "tun_parked",
                "退出清理:TUN 引擎已停、路由已回收(启用标记保留,下次启动自动恢复)",
                { "operation": "park" });
        }
        Err(e) => {
            crate::ev!(warn, "entry", "tun_helper_unreachable",
                "退出清理:TUN 助手停止失败(路由可能残留)",
                { "operation": "park", "error": e.to_string() });
        }
    }
}

/// 写脚本到 data_dir 并经 osascript 管理员密码执行(阻塞至用户输完密码/取消)。
async fn run_privileged_script(data_dir: &std::path::Path, name: &str, content: &str) -> anyhow::Result<()> {
    run_privileged_script_with(data_dir, name, content, std::ffi::OsStr::new("osascript"), Duration::from_secs(180)).await
}

async fn run_privileged_script_with(data_dir: &std::path::Path, name: &str, content: &str, executable: &std::ffi::OsStr, budget: Duration) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    // Each attempt owns a private, unique script. Drop removes it on errors and cancellation too.
    let mut temporary = tempfile::Builder::new().prefix(name).tempfile_in(data_dir)?;
    temporary.write_all(content.as_bytes())?;
    temporary.as_file().set_permissions(std::fs::Permissions::from_mode(0o700))?;
    let path = temporary.path();
    // 双层转义:内层 bash 单引号(sh_squote)+ 外层 AppleScript 双引号字符串(\ 和 " 转义)。
    // data_dir 含引号也不会破坏脚本或注入 root shell。
    let bash_cmd = format!("/bin/bash {}", sh_squote(&path.display().to_string()));
    let as_escaped = bash_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(r#"do shell script "{as_escaped}" with administrator privileges"#);
    let out = tokio::time::timeout(
        budget,
        Command::new(executable).args(["-e", &script]).kill_on_drop(true).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("等待授权超时"))??;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        if err.contains("User canceled") || err.contains("-128") {
            anyhow::bail!("用户取消了授权");
        }
        anyhow::bail!("特权脚本执行失败:{}", err.trim());
    }
    Ok(())
}

/// 安装/升级 helper(一次性管理员密码)。装完 ping 确认活了才算成。
pub async fn tun_install(cfg: &crate::config::Config) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(cfg.host_integrations_allowed(), "隔离实例禁用宿主 TUN 和助手变更");
    let _guard = TUN_MUTATION_LOCK.lock().await;
    if !cfg!(target_os = "macos") {
        anyhow::bail!("TUN 入口仅支持 macOS");
    }
    let (helper_src, mihomo_src) =
        helper_res().ok_or_else(|| anyhow::anyhow!("app 资源里缺 helper/mihomo 二进制(HELPER_RES_DIR)"))?;
    // owner uid = 运行本进程的用户(= data_dir 属主,创建时即我们的 uid);写进 owner.uid 供 helper 鉴权。
    use std::os::unix::fs::MetadataExt;
    let owner_uid = std::fs::metadata(&cfg.data_dir).map(|m| m.uid()).unwrap_or(0);
    let id = cfg.identity();
    run_privileged_script(
        &cfg.data_dir,
        "helper-install.sh",
        &install_script_for(id, &helper_src, &mihomo_src, owner_uid),
    )
    .await?;
    // bootstrap 后 helper 起 socket 要一小会儿
    for _ in 0..10 {
        if let Ok(pong) = helper_call(id.helper_sock, serde_json::json!({ "cmd": "ping" })).await {
            if pong.get("version").and_then(|v| v.as_str()) == Some(HELPER_VERSION) {
                return Ok(tun_status(cfg).await);
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    anyhow::bail!("安装脚本已执行,但未收到预期版本助手的确认;请检查助手状态后重试")
}

/// 卸载 helper(管理员密码)。顺带清启用标记。
pub async fn tun_uninstall(cfg: &crate::config::Config) -> anyhow::Result<serde_json::Value> {
    anyhow::ensure!(cfg.host_integrations_allowed(), "隔离实例禁用宿主 TUN 和助手变更");
    let _guard = TUN_MUTATION_LOCK.lock().await;
    run_privileged_script(&cfg.data_dir, "helper-uninstall.sh", &uninstall_script_for(cfg.identity())).await?;
    TUN_RESUME_PENDING.store(false, std::sync::atomic::Ordering::SeqCst);
    TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = set_tun_enabled(&cfg.data_dir, false);
    Ok(tun_status(cfg).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dns_request_normalizes_domains_preserves_fail_closed_routes_and_legacy() {
        let mut rules = identity_rules();
        let mut rule = crate::store::Rule { id: 101, channel_id: "a".into(), kind: "domain".into(),
            pattern: "äbc.中国".into(), enabled: 1, note: String::new(), locked: 0 };
        rules.push(rule.clone());
        rule.id = 102; rule.pattern = "XN--BC-UIA.XN--FIQS8S".into(); rules.push(rule.clone());
        rule.id = 103; rule.pattern = "*.corp.test".into(); rules.push(rule.clone());
        rule.id = 104; rule.pattern = "ignored.test".into(); rule.enabled = 0; rules.push(rule);
        let domains = tun_domains(&rules);
        assert!(domains.domains.contains(&"xn--bc-uia.xn--fiqs8s".into()));
        assert_eq!(domains.domains.iter().filter(|d| d.starts_with("xn--")).count(), 1);
        assert!(domains.domains.contains(&"corp.test".into()));
        assert!(!domains.domains.contains(&"ignored.test".into()));
        let request = tun_request_for_rules(&crate::identity::TONGDAO, "12345", &rules, Some(12000)).unwrap();
        assert_eq!(request["dns_port"], 12000);
        assert_eq!(request["domains"], serde_json::json!(domains.domains));
        assert!(request["v4"].as_array().unwrap().contains(&serde_json::json!("198.19.128.0/17")));
        let config: serde_yaml::Value = serde_yaml::from_str(request["config"].as_str().unwrap()).unwrap();
        assert_eq!(config["dns"]["listen"].as_str(), Some("127.0.0.1:12000"));
        assert_eq!(config["dns"]["enhanced-mode"].as_str(), Some("fake-ip"));
        assert_eq!(config["dns"]["nameserver"][0].as_str(), Some("127.0.0.1:1"));
        assert_eq!(config["profile"]["store-fake-ip"].as_bool(), Some(true));
        assert_eq!(config["tun"]["auto-route"].as_bool(), Some(false));
        assert_eq!(config["tun"]["disable-icmp-forwarding"].as_bool(), Some(true));
        assert!(config["tun"]["dns-hijack"].as_sequence().unwrap().is_empty());
        assert_eq!(config["rules"][0].as_str(), Some("NETWORK,udp,REJECT"));
        assert!(tun_request_for_rules(&crate::identity::VPNMGR, "12345", &rules, Some(12000)).is_err());
        let legacy = tun_request_for_rules(&crate::identity::VPNMGR, "12345", &rules, None).unwrap();
        assert_eq!(legacy["config"], tun_mihomo_config("12345"));
        assert!(legacy.get("dns_port").is_none());
    }

    #[test]
    fn dns_domain_extraction_rejects_special_names_and_url_syntax() {
        let patterns = ["local", "host.local", "LOCAL", "a..b", ".corp", "corp.", "x*y.test",
            "x/test", "x:123", "u@host", "x#part", "x?query", "%65xample.test", "a\\b", "127.0.0.1", "::1", " "];
        let rules: Vec<_> = patterns.iter().enumerate().map(|(i, p)| crate::store::Rule {
            id: i as i64, channel_id: "a".into(), kind: "domain".into(), pattern: (*p).into(),
            enabled: 1, note: String::new(), locked: 0 }).collect();
        let domains = tun_domains(&rules);
        assert!(domains.domains.is_empty(), "{:?}", domains.domains);
        assert_eq!(domains.skipped.len(), patterns.len());
    }

    #[test]
    fn helper_desired_comparison_includes_dns_and_its_removal() {
        let req = serde_json::json!({"config":"c", "v4":[], "v6":[], "dns_port":12000,"domains":["corp.test"]});
        let mut status = req.clone(); status["running"] = true.into();
        assert!(helper_has_desired(&status, &req));
        status["domains"] = serde_json::json!(["older.test"]);
        assert!(!helper_has_desired(&status, &req));
        let plain = serde_json::json!({"config":"c", "v4":[], "v6":[]});
        assert!(!helper_has_desired(&status, &plain));
        status["domains"] = serde_json::json!([]); status["dns_port"] = 0.into();
        assert!(helper_has_desired(&status, &plain));
    }

    async fn fake_helper(
        path: &std::path::Path,
        mut reply: impl FnMut(serde_json::Value) -> serde_json::Value + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::UnixListener::bind(path).unwrap();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                socket.read_to_end(&mut bytes).await.unwrap();
                let response = reply(serde_json::from_slice(&bytes).unwrap());
                if socket.write_all(&serde_json::to_vec(&response).unwrap()).await.is_ok() {
                    let _ = socket.shutdown().await;
                }
            }
        })
    }

    #[tokio::test]
    async fn renewal_uses_current_generation_and_does_not_replay_an_unconfirmed_write() {
        use std::sync::{Arc, Mutex};
        for bad in [false,true] {
            let root=tempfile::tempdir_in("/tmp").unwrap();let socket=root.path().join("h.sock");
            let calls=Arc::new(Mutex::new(Vec::new()));let captured=calls.clone();
            let task=fake_helper(&socket,move |req| {
                captured.lock().unwrap().push(req.clone());
                if req["cmd"]=="status" { return serde_json::json!({"ok":true,"version":HELPER_VERSION,"generation":11}); }
                serde_json::json!({"ok":true,"generation":if bad {10}else{11},"lease_remaining":89,"upstream_ok":false})
            }).await;
            assert_eq!(renew_helper(socket.to_str().unwrap(),false).await.is_err(),bad);
            let calls=calls.lock().unwrap();
            assert_eq!(calls.len(),2);
            assert_eq!(calls[1],serde_json::json!({"cmd":"renew","generation":11,"lease_secs":90,"upstream_ok":false}));
            task.abort();
        }
    }

    #[tokio::test]
    async fn resume_waits_for_helper_reconciliation_without_replaying_ensure() {
        use std::sync::{Arc, Mutex};
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let socket = root.path().join("h.sock");
        let request = serde_json::json!({"cmd":"ensure", "config":"fixture", "v4":["192.0.2.1/32"], "v6":[]});
        let calls = Arc::new(Mutex::new(Vec::new()));
        let recorded = calls.clone();
        let mut status = serde_json::json!({"ok":true,"generation":0,"running":false,"alive":false,"pending":false});
        let mut remaining = 3;
        let task = fake_helper(&socket, move |req| {
            recorded.lock().unwrap().push(req["cmd"].as_str().unwrap().to_string());
            if req["cmd"] == "ensure" {
                status = req;
                status["ok"] = true.into(); status["running"] = true.into();
                status["alive"] = false.into(); status["pending"] = true.into();
            } else if status["running"] == true {
                remaining -= 1;
                status["pending"] = (remaining > 0).into();
                status["alive"] = (remaining == 0).into();
            }
            status.clone()
        }).await;
        let result = ensure_helper_applied(socket.to_str().unwrap(), request, Duration::from_secs(2), Duration::from_millis(5)).await;
        task.abort(); let _ = task.await;
        assert!(result.is_ok(), "{result:?}");
        assert_eq!(calls.lock().unwrap().iter().filter(|cmd| *cmd == "ensure").count(), 1);
        assert!(calls.lock().unwrap().len() >= 6);
    }

    #[tokio::test]
    async fn unchanged_pending_helper_is_not_rewritten_and_resume_is_bounded() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        let root = tempfile::tempdir_in("/tmp").unwrap();
        let socket = root.path().join("h.sock");
        let request = serde_json::json!({"cmd":"ensure","config":"fixture","v4":[],"v6":[]});
        let writes = Arc::new(AtomicUsize::new(0));
        let recorded = writes.clone();
        let task = fake_helper(&socket, move |req| {
            if req["cmd"] != "status" { recorded.fetch_add(1, Ordering::SeqCst); }
            serde_json::json!({"ok":true,"generation":1,"running":true,"alive":true,"pending":true,"config":"fixture","v4":[],"v6":[]})
        }).await;
        assert!(helper_ensure_if_changed(socket.to_str().unwrap(), request.clone()).await.is_ok());
        let result = ensure_helper_applied(socket.to_str().unwrap(), request, Duration::from_millis(60), Duration::from_millis(5)).await;
        task.abort(); let _ = task.await;
        assert!(result.unwrap_err().to_string().contains("仍待确认"));
        assert_eq!(writes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn command_timeout_kills_its_child() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("pid");
        let script = format!("printf '%s' $$ > {}; exec /bin/sleep 60", sh_squote(&pid_file.display().to_string()));
        let result = run_with_budget("/bin/sh", &["-c", &script], Duration::from_secs(1)).await;
        assert!(result.unwrap_err().to_string().contains("超时"));
        let pid = std::fs::read_to_string(pid_file).unwrap();
        let mut alive = true;
        for _ in 0..50 {
            alive = Command::new("/bin/kill").args(["-0", &pid]).output().await.unwrap().status.success();
            if !alive { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if alive { let _ = Command::new("/bin/kill").args(["-KILL", &pid]).output().await; }
        assert!(!alive, "timed-out child must be reaped");
    }

    #[tokio::test]
    async fn privileged_timeout_reaps_owned_process_and_removes_script() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("authorization-stub");
        let pid_file = root.path().join("pid");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s' $$ > {}\nexec /bin/sleep 60\n", sh_squote(&pid_file.display().to_string()))).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        // macOS 首次执行一个新建的可执行文件要先过系统放行检查,实测约 0.4–0.5 秒(再次执行约 7 毫秒);
        // 预算须明显长于它,否则假脚本还没写出进程号就被超时杀掉。
        let result = run_privileged_script_with(root.path(), "helper-install.sh", "exit 0", executable.as_os_str(), Duration::from_secs(3)).await;
        assert!(result.is_err());
        let pid = std::fs::read_to_string(&pid_file).unwrap();
        let mut alive = true;
        for _ in 0..50 {
            alive = Command::new("/bin/kill").args(["-0", &pid]).output().await.unwrap().status.success();
            if !alive { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Clean a failing reproduction too; only the PID written by our stub is targeted.
        if alive { let _ = Command::new("/bin/kill").args(["-KILL", &pid]).output().await; }
        let script_left = std::fs::read_dir(root.path()).unwrap().any(|item| item.unwrap().file_name().to_string_lossy().starts_with("helper-install.sh"));
        assert!(!alive && !script_left, "authorization process alive={alive}, script retained={script_left}");
    }

    #[test]
    fn idle_requires_engine_exit_and_confirmed_route_removal() {
        let stopped = serde_json::json!({"running":false,"alive":false,"applied":0,"pending":false});
        assert!(idle_helper_stopped(&stopped));
        for (key, value) in [("running", serde_json::json!(true)), ("alive", serde_json::json!(true)),
            ("applied", serde_json::json!(1)), ("pending", serde_json::json!(true)),
            ("pending", serde_json::Value::Null)] {
            let mut status = stopped.clone(); status[key] = value;
            assert!(!idle_helper_stopped(&status));
        }
    }

    #[test]
    fn helper_negative_ack_and_unconfirmed_stop_are_errors() {
        assert!(parse_helper_response(br#"{"ok":false,"error":"persist failed"}"#).is_err());
        assert!(parse_helper_response(br#"{"version":"0.2.0"}"#).is_err());
        assert!(parse_helper_response(br#"{"ok":true,"generation":3}"#).is_ok());
        assert!(ensure_helper_stopped(&serde_json::json!({"running":false,"alive":true})).is_err());
        assert!(ensure_helper_stopped(&serde_json::json!({"running":false,"alive":false})).is_ok());
        let request = serde_json::json!({"cmd":"ensure","config":"fixture","v4":[],"v6":[]});
        let response = serde_json::json!({"generation":3,"running":true,"config":"fixture","v4":[],"v6":[]});
        assert!(helper_matches_request(&response, &request, 3));
        assert!(!helper_matches_request(&response, &request, 4));
        let other = serde_json::json!({"cmd":"ensure","config":"different","v4":[],"v6":[]});
        assert!(!helper_matches_request(&response, &other, 3));
    }

    #[test]
    fn parse_version_detects_meta() {
        let (v, m) = parse_version_json(r#"{"meta":true,"version":"v1.19.24"}"#);
        assert_eq!(v.as_deref(), Some("v1.19.24"));
        assert!(m);
        let (v2, m2) = parse_version_json("not json");
        assert!(v2.is_none() && !m2);
    }

    #[test]
    fn verge_profile_shape() {
        let p = verge_merge_profile("44942", "18080");
        assert!(p.contains("prepend-proxies:"));
        assert!(p.contains("name: vpn-router"));
        assert!(p.contains("port: 44942"));
        assert!(p.contains("rule-providers:"));
        assert!(p.contains("http://127.0.0.1:18080/clash/vpn-rules.yaml"));
        assert!(p.contains("RULE-SET,vpn-rules,vpn-router,no-resolve"), "命门 #2:no-resolve");
    }

    #[test]
    fn pac_url_uses_ui_port() {
        assert_eq!(pac_url("18080"), "http://127.0.0.1:18080/entry/proxy.pac");
    }

    #[test]
    fn parse_iface_from_route() {
        let out = "   route to: default\n  gateway: 192.168.1.1\n  interface: en0\n  flags: <UP>\n";
        assert_eq!(parse_default_iface(out).as_deref(), Some("en0"));
        assert_eq!(parse_default_iface("no iface here"), None);
    }

    #[test]
    fn parse_service_maps_device_to_name() {
        let out = "An asterisk (*) denotes that a network service is disabled.\n\
                   (1) Wi-Fi\n\
                   (Hardware Port: Wi-Fi, Device: en0)\n\
                   \n\
                   (2) Thunderbolt Ethernet\n\
                   (Hardware Port: Thunderbolt Ethernet, Device: en1)\n";
        assert_eq!(parse_service_for_device(out, "en0").as_deref(), Some("Wi-Fi"));
        assert_eq!(parse_service_for_device(out, "en1").as_deref(), Some("Thunderbolt Ethernet"));
        assert_eq!(parse_service_for_device(out, "en9"), None);
    }

    /// 真机只读 smoke(需 macOS;不改任何系统设置):检测 Clash + 读默认服务 + 读系统代理状态。
    #[tokio::test]
    #[ignore] // 真机只读,手动跑:cargo test --lib entry -- --ignored --nocapture
    async fn read_only_host_actions_smoke() {
        let det = detect_clash().await;
        eprintln!("clash-detect: {det:?}");
        let svc = primary_service().await;
        eprintln!("primary_service: {svc:?}");
        assert!(svc.is_some(), "应能识别默认网络服务");
        let st = system_proxy_status("18080").await;
        eprintln!("system-proxy: {st:?}");
        assert!(st.supported, "macOS 支持");
        assert_eq!(st.service, svc, "状态里的服务名 = primary_service");
    }

    #[test]
    fn parse_autoproxy_url_and_state() {
        let on = "URL: http://127.0.0.1:18080/entry/proxy.pac\nEnabled: Yes\n";
        let (u, e) = parse_autoproxy(on);
        assert_eq!(u.as_deref(), Some("http://127.0.0.1:18080/entry/proxy.pac"));
        assert!(e);
        let off = "URL: (null)\nEnabled: No\n";
        let (u2, e2) = parse_autoproxy(off);
        assert!(u2.is_none() && !e2);
    }

    // ── 层3 ──────────────────────────────────────────────────────────────────

    #[test]
    fn tun_config_frozen_essentials() {
        let c = tun_mihomo_config("37473");
        assert!(c.contains("device: utun225"), "设备名 pin 死");
        assert!(c.contains("auto-route: false"), "不抢默认路由");
        assert!(c.contains("dns-hijack: []"), "必须显式置空,默认劫持一切 :53");
        assert!(c.contains("fake-ip-range: 198.19.0.1/16"), "躲开 ClashX 198.18 池");
        assert!(c.contains("stack: system"));
        assert!(c.contains("port: 37473"), "唯一动态值 = 分流口");
        assert!(c.contains("MATCH,vpn-entry"), "全量进 socks5,不留 DIRECT 回环");
        assert!(c.contains("server: 127.0.0.1"), "命门 #4:只指本机");
    }

    #[test]
    fn tun_config_empty_port_fallback() {
        assert!(tun_mihomo_config("").contains("port: 7899"));
    }

    #[test]
    fn plist_shape() {
        let p = helper_plist();
        assert!(p.contains("<string>com.vpnmgr.helper</string>"));
        assert!(p.contains("/Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper"));
        assert!(p.contains("SuccessfulExit"), "崩溃自动重启、正常退出不复活");
    }

    #[test]
    fn install_script_shape() {
        let s = install_script("/res dir/vpnmgr-helper", "/res dir/mihomo", 501);
        assert!(s.contains("cp '/res dir/vpnmgr-helper'"), "源路径单引号包裹(.app 路径可含空格)");
        assert!(s.contains("printf '%s' 501 > /Library/PrivilegedHelperTools/vpnmgr/owner.uid"), "落 owner uid 供鉴权");
        assert!(s.contains("chown -R root:wheel"), "命门:root 属主目录,防提权洞");
        assert!(s.contains("xattr -c"), "清 quarantine");
        assert!(s.contains("launchctl bootstrap system"));
        assert!(s.contains("bootout system/com.vpnmgr.helper 2>/dev/null || true"), "重装先卸旧");
        assert!(s.contains("<key>Label</key>"), "plist 内嵌 heredoc");
    }

    #[test]
    fn sh_squote_neutralizes_metachars() {
        // 命令注入防线:$()、反引号、双引号、空格都被单引号包死
        assert_eq!(sh_squote("/Applications/x.app"), "'/Applications/x.app'");
        assert_eq!(sh_squote("/a $(touch /tmp/p)/b"), "'/a $(touch /tmp/p)/b'");
        assert_eq!(sh_squote("/a's b"), "'/a'\\''s b'"); // 内部单引号 → '\''
    }

    #[test]
    fn install_script_injection_path_is_quoted() {
        // .app 被改名成含 $(...) 的恶意路径:必须整体落进单引号,不被 root shell 展开
        let s = install_script("/Users/x/$(touch /tmp/pwned).app/h", "/m", 501);
        assert!(s.contains("cp '/Users/x/$(touch /tmp/pwned).app/h'"));
        assert!(!s.contains("cp \"/Users"), "不能再用双引号(留 $()/反引号 逃逸口)");
    }

    #[test]
    fn uninstall_script_shape() {
        let s = uninstall_script();
        assert!(s.contains("launchctl bootout"));
        assert!(s.contains("rm -f /Library/LaunchDaemons/com.vpnmgr.helper.plist"));
        assert!(s.contains("rm -rf /Library/PrivilegedHelperTools/vpnmgr"));
        assert!(s.contains("pkill"), "孤儿 mihomo 兜底");
    }

    #[test]
    fn tongdao_uninstall_preserves_journal_until_service_and_dns_cleanup_succeed() {
        use std::os::unix::fs::PermissionsExt;
        for (service_live, cleanup_fails) in [(true, false), (false, true), (false, false)] {
            let root = tempfile::tempdir().unwrap();
            let bin = root.path().join("bin"); std::fs::create_dir(&bin).unwrap();
            let helper = root.path().join("helper"); std::fs::create_dir(&helper).unwrap();
            let plist = root.path().join("helper.plist"); std::fs::write(&plist, "fixture").unwrap();
            let journal = helper.join("resolver-owned.json"); std::fs::write(&journal, "fixture").unwrap();
            for (path, contents) in [
                (bin.join("launchctl"), "#!/bin/sh\nif [ \"$1\" = print ]; then exit \"$PRINT_CODE\"; fi\nexit 0\n"),
                (bin.join("pkill"), "#!/bin/sh\nexit 0\n"),
                (helper.join("vpnmgr-helper"), "#!/bin/sh\n[ \"$1\" = --cleanup-resolver ] || exit 9\nexit \"$CLEAN_CODE\"\n"),
            ] {
                std::fs::write(&path, contents).unwrap();
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let id = &crate::identity::TONGDAO;
            let script = uninstall_script_for(id)
                .replace(id.helper_dir, helper.to_str().unwrap())
                .replace(id.helper_plist, plist.to_str().unwrap());
            assert!(!script.contains("/Library/"));
            let path = root.path().join("uninstall.sh"); std::fs::write(&path, script).unwrap();
            let status = std::process::Command::new("/bin/bash").arg(path).env_clear()
                .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
                .env("PRINT_CODE", if service_live { "0" } else { "1" })
                .env("CLEAN_CODE", if cleanup_fails { "1" } else { "0" }).status().unwrap();
            let should_remove = !service_live && !cleanup_fails;
            assert_eq!(status.success(), should_remove);
            assert_eq!(journal.exists(), !should_remove);
            assert_eq!(plist.exists(), !should_remove);
        }
    }

    #[test]
    fn route_sets_filters_dedups_splits() {
        use crate::store::Rule;
        let r = |kind: &str, pat: &str, en: i64, ch: &str| Rule {
            id: 0,
            channel_id: ch.into(),
            kind: kind.into(),
            pattern: pat.into(),
            enabled: en,
            note: String::new(),
            locked: 0,
        };
        let rules = vec![
            r("ip", "10.0.0.0/8", 1, "a"),
            r("ip", "10.0.0.0/8", 1, "b"),      // 跨通道同网段 → 去重
            r("ip", "192.168.5.0/24", 0, "a"),  // disabled → 不出
            r("domain", "corp.example.com", 1, "a"), // 域名 → Phase 2,不出
            r("ip", "fd12::/32", 1, "a"),       // v6 拆开
            r("ip", "172.16.0.0/12", 1, "c"),
        ];
        let (v4, v6) = route_sets(&rules);
        assert_eq!(v4, vec!["10.0.0.0/8", "172.16.0.0/12", "198.19.0.0/30"]);
        assert_eq!(v6, vec!["fd12::/32"]);
    }

    // ── 身份:旧身份逐字节钉死(改动前 HEAD 的输出)──────────────────────────────

    const LEGACY_TUN_CONFIG: &str = r#"# vpnmgr 生成 · mihomo#2(宿主 TUN 入口引擎)· 配置冻结:规则变更只动路由表
log-level: warning
mode: rule
tun:
  enable: true
  device: utun225
  stack: system
  auto-route: false
  auto-detect-interface: false
  dns-hijack: []
dns:
  enable: false
  fake-ip-range: 198.19.0.1/16
proxies:
  - name: vpn-entry
    type: socks5
    server: 127.0.0.1
    port: 7899
    udp: true
rules:
  - MATCH,vpn-entry
"#;

    const LEGACY_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>com.vpnmgr.helper</string>
    <key>Program</key><string>/Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>StandardErrorPath</key><string>/Library/PrivilegedHelperTools/vpnmgr/helper.log</string>
</dict>
</plist>
"#;

    const LEGACY_INSTALL_HEAD: &str = r#"#!/bin/bash
set -e
mkdir -p /Library/PrivilegedHelperTools/vpnmgr
cp '/res dir/vpnmgr-helper' /Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper
cp '/res dir/mihomo' /Library/PrivilegedHelperTools/vpnmgr/mihomo
printf '%s' 501 > /Library/PrivilegedHelperTools/vpnmgr/owner.uid
xattr -c /Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper /Library/PrivilegedHelperTools/vpnmgr/mihomo 2>/dev/null || true
chown -R root:wheel /Library/PrivilegedHelperTools/vpnmgr
chmod 755 /Library/PrivilegedHelperTools/vpnmgr /Library/PrivilegedHelperTools/vpnmgr/vpnmgr-helper /Library/PrivilegedHelperTools/vpnmgr/mihomo
chmod 644 /Library/PrivilegedHelperTools/vpnmgr/owner.uid
cat > /Library/LaunchDaemons/com.vpnmgr.helper.plist <<'PLIST'
"#;

    const LEGACY_INSTALL_TAIL: &str = r#"PLIST
chown root:wheel /Library/LaunchDaemons/com.vpnmgr.helper.plist
chmod 644 /Library/LaunchDaemons/com.vpnmgr.helper.plist
launchctl bootout system/com.vpnmgr.helper 2>/dev/null || true
launchctl bootstrap system /Library/LaunchDaemons/com.vpnmgr.helper.plist
"#;

    const LEGACY_UNINSTALL: &str = r#"#!/bin/bash
launchctl bootout system/com.vpnmgr.helper 2>/dev/null || true
pkill -f /Library/PrivilegedHelperTools/vpnmgr/mihomo 2>/dev/null || true
rm -f /Library/LaunchDaemons/com.vpnmgr.helper.plist
rm -rf /Library/PrivilegedHelperTools/vpnmgr
"#;

    const LEGACY_VERGE: &str = r#"# Clash Verge Rev「Merge」扩展 · 本工具自动生成
# 用法:Verge → 订阅 → 新建「Merge」类型 → 粘贴本文 → 拖到你的订阅之后启用
prepend-proxies:
  - name: vpn-router
    type: socks5
    server: 127.0.0.1
    port: 7899
rule-providers:
  vpn-rules:
    type: http
    behavior: classical
    format: yaml
    url: http://127.0.0.1:8787/clash/vpn-rules.yaml
    path: ./providers/vpn-rules.yaml
    interval: 60
prepend-rules:
  - RULE-SET,vpn-rules,vpn-router,no-resolve
"#;

    fn identity_rules() -> Vec<crate::store::Rule> {
        let r = |kind: &str, pat: &str, en: i64| crate::store::Rule {
            id: 0, channel_id: "c".into(), kind: kind.into(), pattern: pat.into(), enabled: en,
            note: String::new(), locked: 0,
        };
        vec![r("ip", "10.20.0.0/16", 1), r("ip", "192.0.2.0/24", 0), r("domain", "corp.example", 1), r("ip", "fd00::/8", 1)]
    }

    #[test]
    fn legacy_identity_outputs_are_byte_identical() {
        assert_eq!(tun_mihomo_config("7899"), LEGACY_TUN_CONFIG);
        assert_eq!(helper_plist(), LEGACY_PLIST);
        assert_eq!(install_script("/res dir/vpnmgr-helper", "/res dir/mihomo", 501),
            format!("{LEGACY_INSTALL_HEAD}{LEGACY_PLIST}{LEGACY_INSTALL_TAIL}"));
        assert_eq!(uninstall_script(), LEGACY_UNINSTALL);
        assert_eq!(route_sets(&identity_rules()), (vec!["10.20.0.0/16".to_string(), "198.19.0.0/30".into()], vec!["fd00::/8".to_string()]));
        assert_eq!(verge_merge_profile("7899", "8787"), LEGACY_VERGE);
        // 原签名就是旧身份的 `_for`。
        let legacy = &crate::identity::VPNMGR;
        assert_eq!(tun_mihomo_config_for(legacy, "7899"), LEGACY_TUN_CONFIG);
        assert_eq!(helper_plist_for(legacy), LEGACY_PLIST);
        assert_eq!(uninstall_script_for(legacy), LEGACY_UNINSTALL);
        assert_eq!(verge_merge_profile_for(legacy, "7899", "8787"), LEGACY_VERGE);
    }

    /// 通道管家的输出里不能出现任何旧身份取值(助手二进制名 `vpnmgr-helper` 两边共用,不算)。
    fn assert_no_legacy_value(text: &str) {
        let old = &crate::identity::VPNMGR;
        // 订阅地址是各自端口上的同名路由,不随身份变。
        let text = text.replace("/clash/vpn-rules.yaml", "/clash/<subscription>");
        for bad in [old.helper_label, old.helper_sock, old.helper_dir, old.helper_plist, old.tun_device,
            old.fake_ip_range, old.nat_return_v4, old.clash_proxy_name, old.clash_provider_name, "198.19.0.", "# vpnmgr "] {
            assert!(!text.contains(bad), "含旧值 {bad}:\n{text}");
        }
    }

    /// ICMP 回环(2026-10-01 实测):进 utun 的 ping 被引擎转发、又被 /32 路由送回 utun。
    /// 通道管家关掉转发;旧身份配置冻结,一个字节都不加。
    #[test]
    fn tongdao_tun_config_disables_icmp_forwarding_but_legacy_stays_frozen() {
        let tun = tun_mihomo_config_for(&crate::identity::TONGDAO, "7899");
        let tun_block = tun.split("\ndns:\n").next().expect("tun 段在 dns 段之前");
        assert!(tun_block.ends_with("\n  disable-icmp-forwarding: true"), "必须是 tun: 块的最后一行:\n{tun}");
        assert_eq!(tun.matches("disable-icmp-forwarding").count(), 1);
        assert!(!tun_mihomo_config("7899").to_lowercase().contains("icmp"), "旧身份逐字节冻结,不加");
    }

    #[test]
    fn tongdao_identity_outputs_use_own_values() {
        let id = &crate::identity::TONGDAO;
        let tun = tun_mihomo_config_for(id, "7899");
        assert_eq!(tun, LEGACY_TUN_CONFIG.replace("# vpnmgr 生成", "# tongdao 生成")
            .replace("utun225", "utun226").replace("198.19.0.1/16", "198.19.128.1/17")
            .replace("  dns-hijack: []\n", "  dns-hijack: []\n  disable-icmp-forwarding: true\n")
            .replace("  - MATCH,vpn-entry\n", "  - NETWORK,udp,REJECT\n  - MATCH,vpn-entry\n"),
            "只换身份值、关闭 ICMP 转发并拒绝 UDP");
        assert!(tun.contains("device: utun226") && tun.contains("fake-ip-range: 198.19.128.1/17"));

        let plist = helper_plist_for(id);
        assert_eq!(plist, LEGACY_PLIST.replace("com.vpnmgr.helper", "com.tongdao.helper")
            .replace("/Library/PrivilegedHelperTools/vpnmgr/", "/Library/PrivilegedHelperTools/tongdao/"));
        assert!(plist.contains("<string>/Library/PrivilegedHelperTools/tongdao/vpnmgr-helper</string>"));

        let install = install_script_for(id, "/res dir/vpnmgr-helper", "/res dir/mihomo", 501);
        let legacy_install = format!("{LEGACY_INSTALL_HEAD}{LEGACY_PLIST}{LEGACY_INSTALL_TAIL}");
        assert_eq!(install, legacy_install.replace("com.vpnmgr.helper", "com.tongdao.helper")
            .replace("/Library/PrivilegedHelperTools/vpnmgr", "/Library/PrivilegedHelperTools/tongdao"));
        assert!(install.contains("launchctl bootout system/com.tongdao.helper 2>/dev/null || true"));
        assert!(install.contains("cat > /Library/LaunchDaemons/com.tongdao.helper.plist <<'PLIST'"));

        let uninstall = uninstall_script_for(id);
        assert_eq!(uninstall, "#!/bin/bash\n\
            set -e\n\
            launchctl bootout system/com.tongdao.helper 2>/dev/null || true\n\
            if launchctl print system/com.tongdao.helper >/dev/null 2>&1; then exit 1; fi\n\
            if [ -f /Library/PrivilegedHelperTools/tongdao/resolver-owned.json ]; then /Library/PrivilegedHelperTools/tongdao/vpnmgr-helper --cleanup-resolver; fi\n\
            pkill -f /Library/PrivilegedHelperTools/tongdao/mihomo 2>/dev/null || true\n\
            rm -f /Library/LaunchDaemons/com.tongdao.helper.plist\n\
            rm -rf /Library/PrivilegedHelperTools/tongdao\n");

        let (v4, v6) = route_sets_for(id, &identity_rules());
        assert_eq!(v4, vec!["10.20.0.0/16", "198.19.128.0/30"]);
        assert!(v6.is_empty(), "新版没有 IPv6 回程能力，不抢占现有 IPv6 路由");

        let verge = verge_merge_profile_for(id, "7899", "8787");
        assert_eq!(verge, LEGACY_VERGE.replace("vpn-router", "tongdao-router").replace("vpn-rules", "tongdao-rules")
            .replace("/clash/tongdao-rules.yaml", "/clash/vpn-rules.yaml")
            .replace("prepend-proxies:", "# UDP 不经本工具，按你后面的规则处理。\nprepend-proxies:"));
        assert!(verge.contains("  - name: tongdao-router\n") && verge.contains("  tongdao-rules:\n"));
        assert!(verge.contains("path: ./providers/tongdao-rules.yaml"));
        assert!(verge.contains("RULE-SET,tongdao-rules,tongdao-router,no-resolve"));

        for text in [&tun, &plist, &install, &uninstall, &v4.join("\n"), &verge] {
            assert_no_legacy_value(text);
        }
    }

    /// 核心身份表与助手身份表(desktop/helper,独立 crate)在套接字、目录、网卡上逐字相同;
    /// 核心期望的助手版本 = 助手包版本。
    #[test]
    fn core_identity_matches_helper_table() {
        const HELPER_SRC: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../helper/src/main.rs"));
        const HELPER_TOML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../helper/Cargo.toml"));
        for (row, id) in [("LEGACY", &crate::identity::VPNMGR), ("TONGDAO", &crate::identity::TONGDAO)] {
            let start = HELPER_SRC.find(&format!("const {row}: Identity = Identity {{")).expect(row);
            let block = &HELPER_SRC[start..start + HELPER_SRC[start..].find("};").expect(row)];
            for (key, value) in [("sock_path", id.helper_sock), ("base_dir", id.helper_dir), ("device", id.tun_device)] {
                assert!(block.contains(&format!("{key}: \"{value}\",")), "{row}.{key} != {value}:\n{block}");
            }
        }
        assert!(HELPER_TOML.contains(&format!("\nversion = \"{HELPER_VERSION}\"\n")));
    }

    // ── 互斥接管与系统代理只关自己 ────────────────────────────────────────────────

    fn svc(name: &str, url: Option<&str>, enabled: bool) -> ServiceProxy {
        (name.into(), url.map(String::from), enabled)
    }

    #[test]
    fn foreign_pac_judged_by_own_port_range() {
        let own = &crate::identity::TONGDAO.port_range;
        // 自己当前的端口
        let current = "http://127.0.0.1:12345/entry/proxy.pac";
        assert!(!is_foreign_pac(current, own) && is_own_pac(current, own));
        // 自己范围内的另一个端口(异常退出残留),两端边界都算自己的
        for leftover in ["http://127.0.0.1:10000/entry/proxy.pac", "http://127.0.0.1:19999/entry/proxy.pac"] {
            assert!(!is_foreign_pac(leftover, own) && is_own_pac(leftover, own), "{leftover}");
        }
        // 旧应用范围的端口;旧的 Web 栈默认端口同路径,一并算外来
        for other in ["http://127.0.0.1:20000/entry/proxy.pac", "http://127.0.0.1:45000/entry/proxy.pac",
            "http://127.0.0.1:59999/entry/proxy.pac", "http://127.0.0.1:8787/entry/proxy.pac"] {
            assert!(is_foreign_pac(other, own) && !is_own_pac(other, own), "{other}");
        }
        // 别的路径 / 别的主机 / 不是端口:既不是对方的,也不是我们的
        for unrelated in ["http://127.0.0.1:45000/proxy.pac", "http://127.0.0.1:12345/entry/proxy.pac?x=1",
            "http://localhost:45000/entry/proxy.pac", "http://192.0.2.10:45000/entry/proxy.pac",
            "http://127.0.0.1:99999/entry/proxy.pac", "http://127.0.0.1:/entry/proxy.pac", ""] {
            assert!(!is_foreign_pac(unrelated, own) && !is_own_pac(unrelated, own), "{unrelated}");
        }
        // 当前 PAC 地址的写法与判定一致
        assert!(is_own_pac(&pac_url("12345"), own));
        assert!(is_foreign_pac(&pac_url("45000"), own));
    }

    #[test]
    fn other_app_signal_only_for_tongdao() {
        let id = &crate::identity::TONGDAO;
        let foreign = svc("Wi-Fi", Some("http://127.0.0.1:45000/entry/proxy.pac"), true);
        // 对方网卡在
        assert_eq!(takeover_signal(id, true, &[]), Some(OtherTakeover::Tun { device: "utun225" }));
        // 对方 PAC 已启用
        assert_eq!(takeover_signal(id, false, &[svc("Ethernet", None, false), foreign.clone()]),
            Some(OtherTakeover::SystemProxy { service: "Wi-Fi".into() }));
        // 自己当前端口、自己范围内的残留、别的路径、对方 PAC 未启用:都不算对方在接管
        let quiet = [
            svc("A", Some("http://127.0.0.1:12345/entry/proxy.pac"), true),
            svc("B", Some("http://127.0.0.1:19000/entry/proxy.pac"), true),
            svc("C", Some("http://127.0.0.1:45000/corp.pac"), true),
            svc("D", Some("http://127.0.0.1:45000/entry/proxy.pac"), false),
            svc("E", None, false),
        ];
        assert_eq!(takeover_signal(id, false, &quiet), None);
        // 旧身份恒为「无」
        let legacy = &crate::identity::VPNMGR;
        assert_eq!(takeover_signal(legacy, true, std::slice::from_ref(&foreign)), None);
        assert_eq!(takeover_signal(legacy, false, &[foreign, svc("F", Some("http://127.0.0.1:12345/entry/proxy.pac"), true)]), None);
    }

    #[test]
    fn park_only_enabled_services_pointing_to_own_pac() {
        let own = &crate::identity::TONGDAO.port_range;
        let services = [
            svc("Wi-Fi", Some("http://127.0.0.1:12345/entry/proxy.pac"), true),     // 自己当前的
            svc("Ethernet", Some("http://127.0.0.1:10001/entry/proxy.pac"), true),  // 自己的旧端口残留
            svc("USB LAN", Some("http://127.0.0.1:12345/entry/proxy.pac"), false),  // 已关的不再动
            svc("Dock", Some("http://127.0.0.1:45000/entry/proxy.pac"), true),      // 旧应用的
            svc("Office", Some("http://192.0.2.10/corp.pac"), true),                // 用户后来改成的别的
            svc("Bridge", None, false),
        ];
        assert_eq!(services_to_park(&services, |url| is_own_pac(url, own)), vec!["Wi-Fi", "Ethernet"]);
        // 退出清理(旧签名)仍只认当前 PAC 地址
        let ours = pac_url("12345");
        assert_eq!(services_to_park(&services, |url| url == ours), vec!["Wi-Fi"]);
    }

    #[test]
    fn explicit_disable_scope_follows_identity() {
        assert!(!disable_parks_own_services(&crate::identity::VPNMGR), "旧身份保持原样:关当前默认服务");
        assert!(disable_parks_own_services(&crate::identity::TONGDAO));
    }

    #[test]
    fn tun_sync_skips_ensure_when_other_app_holds() {
        assert!(tun_sync_should_ensure(true, true, false, false));
        assert!(!tun_sync_should_ensure(true, true, false, true), "启用标记为真、对方网卡在:不发 ensure");
        assert!(!tun_sync_should_ensure(false, true, false, false));
        assert!(!tun_sync_should_ensure(true, false, false, false));
        assert!(!tun_sync_should_ensure(true, true, true, false));
    }

    #[test]
    fn idle_resume_waits_while_other_app_holds() {
        use IdleResume::*;
        let (tongdao, legacy) = (&crate::identity::TONGDAO, &crate::identity::VPNMGR);
        // 对方在接管:不放行,暂停记录留着,对方关掉后下一次连接还能恢复。
        assert_eq!(idle_resume_step(tongdao, true, true, false), Keep);
        // 对方没在接管、地址仍是我们的、当前未启用:放行。
        assert_eq!(idle_resume_step(tongdao, false, true, false), Enable);
        // 地址被外部改过、或已经开着:不动,记录清掉(对方在不在接管都一样)。
        for other_app in [false, true] {
            assert_eq!(idle_resume_step(tongdao, other_app, false, false), Forget);
            assert_eq!(idle_resume_step(tongdao, other_app, false, true), Forget);
            assert_eq!(idle_resume_step(tongdao, other_app, true, true), Forget);
        }
        // 旧身份不检测对方:对方这一项恒放行,其余判断和改动前一样。
        for other_app in [false, true] {
            assert_eq!(idle_resume_step(legacy, other_app, true, false), Enable);
            assert_eq!(idle_resume_step(legacy, other_app, false, false), Forget);
            assert_eq!(idle_resume_step(legacy, other_app, true, true), Forget);
        }
    }

    #[test]
    fn tun_flag_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!tun_enabled(dir.path()), "缺文件 = 未启用");
        set_tun_enabled(dir.path(), true).unwrap();
        assert!(tun_enabled(dir.path()));
        set_tun_enabled(dir.path(), false).unwrap();
        assert!(!tun_enabled(dir.path()));
    }
}
