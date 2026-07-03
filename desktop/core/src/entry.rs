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

/// 应用/清除本工具的系统自动代理(PAC)。`enable=true` 指向本地 PAC 并开启;false 关闭自动代理。
/// ⚠️ 改系统设置:只应由前端按钮显式触发。返回最终状态。
pub async fn system_proxy_apply(ui_port: &str, enable: bool) -> anyhow::Result<SystemProxyState> {
    if !cfg!(target_os = "macos") {
        anyhow::bail!("系统代理一键应用仅支持 macOS");
    }
    let svc = primary_service().await.ok_or_else(|| anyhow::anyhow!("找不到默认网络服务"))?;
    if enable {
        let url = pac_url(ui_port);
        run("networksetup", &["-setautoproxyurl", &svc, &url]).await?;
        run("networksetup", &["-setautoproxystate", &svc, "on"]).await?;
    } else {
        run("networksetup", &["-setautoproxystate", &svc, "off"]).await?;
    }
    Ok(system_proxy_status(ui_port).await)
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
pub const HELPER_VERSION: &str = "0.1.0";

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

/// 从 rules 提取启用的 IP 规则 → 去重排序的 (v4, v6) CIDR 集(跨通道去重:同网段可挂多通道,
/// 路由表按目的网段唯一)。域名规则不参与(Phase 2 拆分 DNS)。
pub fn route_sets(rules: &[crate::store::Rule]) -> (Vec<String>, Vec<String>) {
    // BTreeSet 一次到位:跨通道去重 + 排序(路由表按目的网段唯一,顺序稳定便于对账/测试)。
    let mut v4 = std::collections::BTreeSet::new();
    let mut v6 = std::collections::BTreeSet::new();
    for r in rules {
        if r.enabled == 0 || r.kind != "ip" {
            continue;
        }
        if r.pattern.contains(':') {
            v6.insert(r.pattern.clone());
        } else {
            v4.insert(r.pattern.clone());
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
    s.write_all(&payload).await?;
    s.shutdown().await?;
    let mut buf = Vec::new();
    tokio::time::timeout(Duration::from_secs(8), s.read_to_end(&mut buf))
        .await
        .map_err(|_| anyhow::anyhow!("等待 helper 响应超时"))??;
    Ok(serde_json::from_slice(&buf)?)
}

/// 层3 综合状态(驱动前端卡片;读-only)。
pub async fn tun_status(cfg: &crate::config::Config) -> serde_json::Value {
    let supported = cfg!(target_os = "macos");
    let resources = helper_res().is_some();
    let installed = std::path::Path::new(HELPER_PLIST).exists();
    let enabled = tun_enabled(&cfg.data_dir);
    let helper = helper_call(serde_json::json!({ "cmd": "status" })).await.ok();
    let (v4, v6) = crate::store::all_rules(&cfg.db_path())
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
    if enable {
        let rules = crate::store::all_rules(&cfg.db_path()).unwrap_or_default();
        let (v4, v6) = route_sets(&rules);
        helper_call(serde_json::json!({
            "cmd": "ensure",
            "config": tun_mihomo_config(&cfg.mihomo_host_port),
            "v4": v4,
            "v6": v6,
        }))
        .await
        .map_err(|e| anyhow::anyhow!("helper 不可达(未安装或未运行):{e}"))?;
        set_tun_enabled(&cfg.data_dir, true)?;
        Ok(tun_status(cfg).await)
    } else {
        set_tun_enabled(&cfg.data_dir, false)?;
        let stop_err = helper_call(serde_json::json!({ "cmd": "stop" })).await.err();
        let mut st = tun_status(cfg).await;
        if let Some(e) = stop_err {
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
    if !cfg!(target_os = "macos") || !tun_enabled(&cfg.data_dir) {
        return;
    }
    let rules = match crate::store::all_rules(&cfg.db_path()) {
        Ok(r) => r,
        Err(_) => return,
    };
    let (v4, v6) = route_sets(&rules);
    if let Err(e) = helper_call(serde_json::json!({
        "cmd": "ensure",
        "config": tun_mihomo_config(&cfg.mihomo_host_port),
        "v4": v4,
        "v6": v6,
    }))
    .await
    {
        eprintln!("[entry] TUN 路由对账失败(helper 不可达?): {e}");
    }
}

/// 写脚本到 data_dir 并经 osascript 管理员密码执行(阻塞至用户输完密码/取消)。
async fn run_privileged_script(data_dir: &std::path::Path, name: &str, content: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = data_dir.join(name);
    std::fs::write(&path, content)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    // 双层转义:内层 bash 单引号(sh_squote)+ 外层 AppleScript 双引号字符串(\ 和 " 转义)。
    // data_dir 含引号也不会破坏脚本或注入 root shell。
    let bash_cmd = format!("/bin/bash {}", sh_squote(&path.display().to_string()));
    let as_escaped = bash_cmd.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(r#"do shell script "{as_escaped}" with administrator privileges"#);
    let out = tokio::time::timeout(
        Duration::from_secs(180),
        Command::new("osascript").args(["-e", &script]).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("等待授权超时"))??;
    let _ = std::fs::remove_file(&path);
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
        if helper_call(serde_json::json!({ "cmd": "ping" })).await.is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(tun_status(cfg).await)
}

/// 卸载 helper(管理员密码)。顺带清启用标记。
pub async fn tun_uninstall(cfg: &crate::config::Config) -> anyhow::Result<serde_json::Value> {
    run_privileged_script(&cfg.data_dir, "helper-uninstall.sh", &uninstall_script()).await?;
    let _ = set_tun_enabled(&cfg.data_dir, false);
    Ok(tun_status(cfg).await)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(v4, vec!["10.0.0.0/8", "172.16.0.0/12"]);
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
