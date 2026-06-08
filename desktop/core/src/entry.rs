//! 宿主接管层(spec §6 层1/层2 的「真执行」端)。Rust core 在 Tauri 模型下跑在宿主 macOS 上,
//! 故能**实际执行** `networksetup`(旧 Docker 模型里后端在容器内、只能展示命令)。
//!
//! - 层1 Clash 订阅:检测本机 Clash/mihomo 客户端 + 生成 Clash Verge Rev 可导入的 merge profile。
//! - 层2 系统代理:`networksetup -setautoproxyurl` 把系统自动代理指向本地 `/entry/proxy.pac`。
//!
//! 命门 #4:PAC/节点都指向 `127.0.0.1:{mihomo_host_port}`(VM 转发的 mihomo#1 mixed 口),不外联。
//! ⚠️ 仅 macOS;`apply` 类动作会改系统设置,只由前端按钮显式触发(用户知情),后端不自动应用。

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
}
