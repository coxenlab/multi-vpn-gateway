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
pub fn verge_merge_profile(mihomo_host_port: &str, ui_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let ui = if ui_port.is_empty() { "<UI端口>" } else { ui_port };
    format!(
        "# Clash Verge Rev「Merge」扩展 · 本工具自动生成\n\
         # 用法:Verge → 订阅 → 新建「Merge」类型 → 粘贴本文 → 拖到你的订阅之后启用\n\
         prepend-proxies:\n\
         \x20 - name: vpn-router\n\
         \x20   type: socks5\n\
         \x20   server: 127.0.0.1\n\
         \x20   port: {port}\n\
         rule-providers:\n\
         \x20 vpn-rules:\n\
         \x20   type: http\n\
         \x20   behavior: classical\n\
         \x20   format: yaml\n\
         \x20   url: http://127.0.0.1:{ui}/clash/vpn-rules.yaml\n\
         \x20   path: ./providers/vpn-rules.yaml\n\
         \x20   interval: 60\n\
         prepend-rules:\n\
         \x20 - RULE-SET,vpn-rules,vpn-router,no-resolve\n"
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
    let out = Command::new(cmd).args(args).output().await?;
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

/// 退出清理:遍历**所有**网络服务,凡自动代理指向本工具 PAC 的一律关闭。
/// 与 system_proxy_apply(false) 的区别:后者只动当前默认路由那一个服务——用户换过网
/// (Wi-Fi→有线)或退出时已离线的话,旧服务上会残留指向已死 127.0.0.1 PAC 的配置
/// (红队 D3)。返回关闭的服务数;全程 best-effort。
pub async fn system_proxy_park_all(ui_port: &str) -> usize {
    if !cfg!(target_os = "macos") {
        return 0;
    }
    let ours = pac_url(ui_port);
    let Ok(listing) = run("networksetup", &["-listallnetworkservices"]).await else { return 0 };
    let mut parked = 0;
    for line in listing.lines().skip(1) {
        // 已停用的服务带 '*' 前缀;服务名本身可含空格,整行即名字
        let svc = line.trim().trim_start_matches('*').trim();
        if svc.is_empty() {
            continue;
        }
        let Ok(out) = run("networksetup", &["-getautoproxyurl", svc]).await else { continue };
        let (url, enabled) = parse_autoproxy(&out);
        if enabled
            && url.as_deref() == Some(ours.as_str())
            && run("networksetup", &["-setautoproxystate", svc, "off"]).await.is_ok()
        {
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

/// 应用/清除本工具的系统自动代理(PAC)。`enable=true` 指向本地 PAC 并开启;false 关闭自动代理。
/// ⚠️ 改系统设置:只应由前端按钮显式触发。返回最终状态。
pub async fn system_proxy_apply(ui_port: &str, enable: bool) -> anyhow::Result<SystemProxyState> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("系统代理一键应用仅支持 macOS");
    }
    // 审计要能回答「改前是什么」:先取一份现状,失败路径也据此说明什么都没变。
    let before = system_proxy_status(ui_port).await;
    let snapshot = |st: &SystemProxyState| serde_json::to_value(st).unwrap_or(serde_json::Value::Null);
    let applied = async {
        let svc = primary_service().await.ok_or_else(|| anyhow::anyhow!("找不到默认网络服务"))?;
        if enable {
            let url = pac_url(ui_port);
            run("networksetup", &["-setautoproxyurl", &svc, &url]).await?;
            run("networksetup", &["-setautoproxystate", &svc, "on"]).await?;
        } else {
            run("networksetup", &["-setautoproxystate", &svc, "off"]).await?;
        }
        anyhow::Ok(())
    }
    .await;
    if let Err(e) = applied {
        crate::events::audit_failed("system_proxy_set", "系统自动代理设置失败", serde_json::json!({
            "target_kind": "entry", "target_name": "system_proxy", "requested_enable": enable,
            "before": snapshot(&before), "after": null, "result": "failed", "error": e.to_string()
        }));
        return Err(e);
    }
    let state = system_proxy_status(ui_port).await;
    // 显式开关覆盖 idle 暂停前的自动恢复意图。
    IDLE_PROXY_SERVICES.lock().await.clear();
    crate::events::audit("system_proxy_set",
        if enable { "系统自动代理已启用" } else { "系统自动代理已关闭" },
        serde_json::json!({
            "target_kind": "entry", "target_name": "system_proxy",
            "target_id": state.service.clone(), "requested_enable": enable,
            "before": snapshot(&before), "after": snapshot(&state), "result": "ok"
        }));
    Ok(state)
}

// ── 层3:TUN 入口(root helper + 宿主 mihomo#2)───────────────────────────────

/// LaunchDaemon label(= plist 文件名主体)。
pub const HELPER_LABEL: &str = "com.vpnmgr.helper";
/// helper IPC socket(helper 启动时建,0660 root:staff)。
pub const HELPER_SOCK: &str = "/var/run/vpnmgr-helper.sock";
/// helper/mihomo 二进制安装目录(root 属主——装用户可写路径 = 提权洞)。
pub const HELPER_DIR: &str = "/Library/PrivilegedHelperTools/vpnmgr";
/// LaunchDaemon plist 路径。
pub const HELPER_PLIST: &str = "/Library/LaunchDaemons/com.vpnmgr.helper.plist";
/// TUN 设备名,与 helper 侧 pin 死同名(desktop/helper DEVICE),路由管理零猜测。
pub const TUN_DEVICE: &str = "utun225";
/// 期望的 helper 版本(与 desktop/helper Cargo.toml 对齐;不符 → UI 提示重装升级)。
pub const HELPER_VERSION: &str = "0.2.0";
static TUN_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static TUN_PARKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
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
    {
        let _guard = TUN_MUTATION_LOCK.lock().await;
        TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
        if std::path::Path::new(HELPER_SOCK).exists() {
            let status = helper_call(serde_json::json!({"cmd":"status"})).await?;
            if !idle_helper_stopped(&status) {
                anyhow::ensure!(tun_enabled(&cfg.data_dir) && status["config"] == tun_mihomo_config(&cfg.mihomo_host_port),
                    "TUN 入口归属或停止状态待确认，保留运行环境");
                helper_mutation(serde_json::json!({"cmd":"stop"})).await?;
                let status = helper_call(serde_json::json!({"cmd":"status"})).await?;
                anyhow::ensure!(idle_helper_stopped(&status), "TUN 引擎或路由尚未确认释放");
            }
        } else {
            anyhow::ensure!(!tun_enabled(&cfg.data_dir), "TUN 助手不可达，无法确认路由已释放");
        }
    }
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

/// 再次连接只恢复本实例保留的入口意图；外部 URL 发生变化的服务不恢复。
pub async fn resume_after_idle(cfg: &crate::config::Config) -> anyhow::Result<()> {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") { return Ok(()); }
    if TUN_PARKED.load(std::sync::atomic::Ordering::SeqCst) && tun_enabled(&cfg.data_dir) {
        let status = tun_apply(cfg, true).await?;
        if status["helper"]["pending"] != false {
            TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
            anyhow::bail!("TUN 入口恢复仍待确认");
        }
    } else {
        TUN_PARKED.store(false, std::sync::atomic::Ordering::SeqCst);
    }
    let ours = pac_url(&cfg.ui_port.to_string());
    let mut parked = IDLE_PROXY_SERVICES.lock().await;
    while let Some(service) = parked.first().cloned() {
        let (url, enabled) = idle_proxy_status(&service).await?;
        if url.as_deref() == Some(&ours) && !enabled {
            let result = idle_networksetup(&["-setautoproxystate", &service, "on"]).await;
            let (url, enabled) = idle_proxy_status(&service).await?;
            anyhow::ensure!(url.as_deref() != Some(&ours) || enabled, "系统代理恢复尚未确认: {:?}", result.err());
        }
        parked.remove(0);
    }
    Ok(())
}

/// mihomo#2 冻结配置(唯一动态值 = 分流口)。要点全部来自实测/源码调研:
/// - `auto-route: false`:不抢默认路由,由 helper 按绑定 IP-CIDR 加最长前缀路由,与 ClashX TUN 共存;
/// - `dns-hijack: []`:默认值是劫持一切经 tun 的 :53,必须显式置空(否则绑定的内网 DNS 服务器被劫);
/// - `fake-ip-range: 198.19.0.1/16`:tun 地址派生自它(enable:false 也生效),默认 198.18.0.1/30
///   正撞 ClashX fake-ip 池;**此块永久冻结**——变更会导致 utun 重建、路由被内核清空;
/// - `stack: system`:官方推荐,macOS gvisor 栈有自代理回环 issue;
/// - MATCH 全量进 socks5(进 utun 的流量全是主动路由进来的),不留 DIRECT 回环语义。
pub fn tun_mihomo_config(mihomo_host_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "7899" } else { mihomo_host_port };
    format!(
        "# vpnmgr 生成 · mihomo#2(宿主 TUN 入口引擎)· 配置冻结:规则变更只动路由表\n\
         log-level: warning\n\
         mode: rule\n\
         tun:\n\
         \x20 enable: true\n\
         \x20 device: {TUN_DEVICE}\n\
         \x20 stack: system\n\
         \x20 auto-route: false\n\
         \x20 auto-detect-interface: false\n\
         \x20 dns-hijack: []\n\
         dns:\n\
         \x20 enable: false\n\
         \x20 fake-ip-range: 198.19.0.1/16\n\
         proxies:\n\
         \x20 - name: vpn-entry\n\
         \x20   type: socks5\n\
         \x20   server: 127.0.0.1\n\
         \x20   port: {port}\n\
         \x20   udp: true\n\
         rules:\n\
         \x20 - MATCH,vpn-entry\n"
    )
}

/// LaunchDaemon plist(root:wheel 644,由安装脚本落盘)。
pub fn helper_plist() -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key><string>{HELPER_LABEL}</string>
    <key>Program</key><string>{HELPER_DIR}/vpnmgr-helper</string>
    <key>RunAtLoad</key><true/>
    <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>
    <key>StandardErrorPath</key><string>{HELPER_DIR}/helper.log</string>
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
/// 源路径经 [`sh_squote`] 转义防注入;目标全是常量。
pub fn install_script(helper_src: &str, mihomo_src: &str, owner_uid: u32) -> String {
    format!(
        "#!/bin/bash\n\
         set -e\n\
         mkdir -p {HELPER_DIR}\n\
         cp {helper_q} {HELPER_DIR}/vpnmgr-helper\n\
         cp {mihomo_q} {HELPER_DIR}/mihomo\n\
         printf '%s' {owner_uid} > {HELPER_DIR}/owner.uid\n\
         xattr -c {HELPER_DIR}/vpnmgr-helper {HELPER_DIR}/mihomo 2>/dev/null || true\n\
         chown -R root:wheel {HELPER_DIR}\n\
         chmod 755 {HELPER_DIR} {HELPER_DIR}/vpnmgr-helper {HELPER_DIR}/mihomo\n\
         chmod 644 {HELPER_DIR}/owner.uid\n\
         cat > {HELPER_PLIST} <<'PLIST'\n\
         {plist}PLIST\n\
         chown root:wheel {HELPER_PLIST}\n\
         chmod 644 {HELPER_PLIST}\n\
         launchctl bootout system/{HELPER_LABEL} 2>/dev/null || true\n\
         launchctl bootstrap system {HELPER_PLIST}\n",
        helper_q = sh_squote(helper_src),
        mihomo_q = sh_squote(mihomo_src),
        plist = helper_plist()
    )
}

/// 卸载脚本:bootout(launchd 会连带杀掉 mihomo 子进程,pkill 兜底)+ 删 plist + 删目录。
/// utun 随 mihomo 退出销毁,内核自动清光挂在其上的路由——无残留。
pub fn uninstall_script() -> String {
    format!(
        "#!/bin/bash\n\
         launchctl bootout system/{HELPER_LABEL} 2>/dev/null || true\n\
         pkill -f {HELPER_DIR}/mihomo 2>/dev/null || true\n\
         rm -f {HELPER_PLIST}\n\
         rm -rf {HELPER_DIR}\n"
    )
}

/// mihomo#2 system 栈的 NAT 回程网段:tun 地址 198.19.0.1/30 的对端 198.19.0.2 是 tun2socket
/// 改写后的注入源,内核回 SYN-ACK 须有此路由才能送回 utun。macOS 点对点 utun 只自带
/// 198.19.0.1 主机路由,不生成 /30 连接路由;网络切换还会刷掉手工路由——所以必须由
/// helper 一并对账持有,否则症状 = 「探活绿但页面打不开」(SYN_SENT,output no route)。
const NAT_RETURN_V4: &str = "198.19.0.0/30";

/// 从 rules 提取启用的 IP 规则 → 去重排序的 (v4, v6) CIDR 集(跨通道去重:同网段可挂多通道,
/// 路由表按目的网段唯一)。域名规则不参与(Phase 2 拆分 DNS)。
/// v4 恒含 [`NAT_RETURN_V4`]:引擎自身的回程路由与规则路由同生命周期对账。
pub fn route_sets(rules: &[crate::store::Rule]) -> (Vec<String>, Vec<String>) {
    // BTreeSet 一次到位:跨通道去重 + 排序(路由表按目的网段唯一,顺序稳定便于对账/测试)。
    let mut v4 = std::collections::BTreeSet::new();
    v4.insert(NAT_RETURN_V4.to_string());
    let mut v6 = std::collections::BTreeSet::new();
    for r in rules {
        if r.enabled == 0 || r.kind != "ip" {
            continue;
        }
        // 命门②:分桶前先 parse::<IpNet>——按解析结果的 v4/v6 分桶(替代裸 contains(':')),
        // 畸形串告警跳过,绝不下发 helper 变成宿主路由。
        match r.pattern.parse::<ipnet::IpNet>() {
            Ok(ipnet::IpNet::V4(_)) => {
                v4.insert(r.pattern.clone());
            }
            Ok(ipnet::IpNet::V6(_)) => {
                v6.insert(r.pattern.clone());
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
pub async fn helper_call(req: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::UnixStream::connect(HELPER_SOCK),
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
async fn helper_mutation(mut req: serde_json::Value) -> anyhow::Result<serde_json::Value> {
    let stopping = req["cmd"] == "stop";
    let current = helper_call(serde_json::json!({"cmd":"status"})).await?;
    let Some(generation) = current.get("generation").and_then(|v| v.as_u64()) else {
        // 升级前也必须能停止旧助手,避免退出留下旧入口。
        if stopping {
            let response = helper_call(req).await?;
            ensure_helper_stopped(&response)?;
            return Ok(response);
        }
        anyhow::bail!("TUN 助手需要升级后才能应用路由");
    };
    let next = generation.checked_add(1).ok_or_else(|| anyhow::anyhow!("helper generation exhausted"))?;
    req["generation"] = serde_json::json!(next);
    let response = match helper_call(req.clone()).await {
        Ok(response) => response,
        Err(error) => {
            // 写结果不明时只读回确认,不能盲目重放 ensure/stop。
            match helper_call(serde_json::json!({"cmd":"status"})).await {
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
    let supported = cfg!(target_os = "macos");
    let resources = helper_res().is_some();
    let installed = std::path::Path::new(HELPER_PLIST).exists();
    let enabled = tun_enabled(&cfg.data_dir);
    let helper = helper_call(serde_json::json!({ "cmd": "status" })).await.ok();
    let (v4, v6) = crate::store::effective_rules(&cfg.db_path())
        .map(|r| route_sets(&r))
        .unwrap_or_default();
    let config_current = helper
        .as_ref()
        .and_then(|h| h.get("config").and_then(|c| c.as_str()))
        .map(|c| c == tun_mihomo_config(&cfg.mihomo_host_port));
    serde_json::json!({
        "supported": supported,
        "resources": resources,
        "installed": installed,
        "enabled": enabled,
        "helper": helper,
        "config_current": config_current,
        "expected_version": HELPER_VERSION,
        "device": TUN_DEVICE,
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
    let _guard = TUN_MUTATION_LOCK.lock().await;
    if enable {
        let rules = crate::store::effective_rules(&cfg.db_path()).unwrap_or_default();
        let (v4, v6) = route_sets(&rules);
        let desired_total = v4.len() + v6.len();
        let response = helper_mutation(serde_json::json!({
            "cmd": "ensure",
            "config": tun_mihomo_config(&cfg.mihomo_host_port),
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
        crate::ev!(info, "entry", "tun_ensure", "TUN 路由目标已下发", {
            "operation": "enable", "total": desired_total,
            "applied": response.get("applied").and_then(|v| v.as_u64()).unwrap_or(0),
            "shadowed": response.get("shadowed").and_then(|v| v.as_u64()).unwrap_or(0)
        });
        Ok(tun_status(cfg).await)
    } else {
        set_tun_enabled(&cfg.data_dir, false)?;
        let stop_err = helper_mutation(serde_json::json!({ "cmd": "stop" })).await.err();
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

/// 规则变更后的路由对账(挂在 manager::rebuild 末尾,best-effort)。
/// 只在用户已显式启用时动作;顺带自愈:分流口变了 → ensure 会带新配置让 helper 重拉 mihomo。
pub async fn tun_sync(cfg: &crate::config::Config) {
    let _guard = TUN_MUTATION_LOCK.lock().await;
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") || !tun_enabled(&cfg.data_dir)
        || TUN_PARKED.load(std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    let rules = match crate::store::effective_rules(&cfg.db_path()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let (v4, v6) = route_sets(&rules);
    let desired_total = v4.len() + v6.len();
    match helper_mutation(serde_json::json!({
        "cmd": "ensure",
        "config": tun_mihomo_config(&cfg.mihomo_host_port),
        "v4": v4,
        "v6": v6,
    }))
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

/// 退出清理:让 helper 停掉 mihomo#2 并回收路由,但**保留启用标记**——
/// 与 tun_apply(false) 的区别在于用户意图不变:下次启动 rebuild 尾部的 tun_sync
/// 会自动重新下发,无需再开一次开关。app 退出后 mihomo#2 的上游(分流口转发)
/// 已死,路由若残留会把命中网段变黑洞,故必须停。
pub async fn tun_park(cfg: &crate::config::Config) {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") || !tun_enabled(&cfg.data_dir) {
        return;
    }
    TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _guard = TUN_MUTATION_LOCK.lock().await;
    match helper_mutation(serde_json::json!({ "cmd": "stop" })).await {
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
    run_privileged_script(
        &cfg.data_dir,
        "helper-install.sh",
        &install_script(&helper_src, &mihomo_src, owner_uid),
    )
    .await?;
    // bootstrap 后 helper 起 socket 要一小会儿
    for _ in 0..10 {
        if let Ok(pong) = helper_call(serde_json::json!({ "cmd": "ping" })).await {
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
    run_privileged_script(&cfg.data_dir, "helper-uninstall.sh", &uninstall_script()).await?;
    TUN_PARKED.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = set_tun_enabled(&cfg.data_dir, false);
    Ok(tun_status(cfg).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn privileged_timeout_reaps_owned_process_and_removes_script() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("authorization-stub");
        let pid_file = root.path().join("pid");
        std::fs::write(&executable, format!("#!/bin/sh\nprintf '%s' $$ > {}\nexec /bin/sleep 60\n", sh_squote(&pid_file.display().to_string()))).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = run_privileged_script_with(root.path(), "helper-install.sh", "exit 0", executable.as_os_str(), Duration::from_millis(500)).await;
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
