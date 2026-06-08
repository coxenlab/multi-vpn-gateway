//! main.py 内联纯逻辑的 Rust 端:规则分类 / Clash 文本 / PAC。对照 app/main.py。

/// 对照 _classify:('ip', cidr) / ('domain', host) / None。
pub fn classify(token: &str) -> Option<(String, String)> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    let addr = t.split('/').next().unwrap_or("");
    // 对照 Python ipaddress.ip_address:接受带 zone id 的 scoped IPv6(fe80::1%eth0);
    // std::net::IpAddr 不认 zone,故校验前剥掉(发出的 CIDR 仍用原 token,与 Python 一致)。
    let addr_no_zone = addr.split('%').next().unwrap_or("");
    if addr_no_zone.parse::<std::net::IpAddr>().is_err() {
        // 域名:剥 scheme / path / userinfo / port
        let after_scheme = t.splitn(2, "://").last().unwrap_or("");
        let no_path = after_scheme.split('/').next().unwrap_or("");
        let no_user = no_path.rsplit('@').next().unwrap_or("");
        let host = no_user
            .split(':')
            .next()
            .unwrap_or("")
            .trim()
            .trim_matches('.')
            .to_lowercase();
        return if host.is_empty() { None } else { Some(("domain".into(), host)) };
    }
    if t.contains('/') {
        if t.parse::<ipnet::IpNet>().is_err() {
            return None;
        }
        return Some(("ip".into(), t.to_string()));
    }
    let suffix = if addr.contains(':') { "/128" } else { "/32" };
    Some(("ip".into(), format!("{t}{suffix}")))
}

/// 对照 _bare:剥 `+.` / `*.` 前缀。
pub fn bare(p: &str) -> String {
    for pre in ["+.", "*."] {
        if let Some(rest) = p.strip_prefix(pre) {
            return rest.to_string();
        }
    }
    p.to_string()
}

use std::collections::HashMap;

/// plan_rules 的结果(对照 add_rules 的 added/rejected + 实际要落库的 (kind,pat))。
pub struct RulePlan {
    pub to_add: Vec<(String, String)>,
    pub added: HashMap<String, i64>,
    pub rejected: Vec<String>,
}

/// 对照 add_rules 的内联分类/去重逻辑(纯函数)。forced = Some("ip"|"domain") 或 None(自动 classify)。
/// existing = 已存在的 (kind,pattern);去重对全集(含本批已加)。
pub fn plan_rules(patterns: &[String], forced: Option<&str>, existing: &[(String, String)]) -> RulePlan {
    let mut seen: std::collections::HashSet<(String, String)> = existing.iter().cloned().collect();
    let mut to_add = Vec::new();
    let mut added: HashMap<String, i64> = HashMap::from([("domain".into(), 0), ("ip".into(), 0)]);
    let mut rejected = Vec::new();
    for tok in patterns {
        let (kind, pat) = match forced {
            Some("domain") => ("domain".to_string(), tok.trim().to_string()),
            Some("ip") => match classify(tok) {
                Some((k, p)) if k == "ip" => (k, p),
                _ => {
                    rejected.push(tok.clone());
                    continue;
                }
            },
            _ => match classify(tok) {
                Some((k, p)) => (k, p),
                None => {
                    rejected.push(tok.clone());
                    continue;
                }
            },
        };
        if pat.is_empty() || seen.contains(&(kind.clone(), pat.clone())) {
            continue;
        }
        seen.insert((kind.clone(), pat.clone()));
        *added.entry(kind.clone()).or_insert(0) += 1;
        to_add.push((kind, pat));
    }
    RulePlan { to_add, added, rejected }
}

/// 对照 main.py login 的 url 构造(命门 #4:端口是容器实时映射的 host 端口)。
/// path 须带尾斜杠:镜像内 tinyproxy 把 /websockify 301→/websockify/,WS 握手不跟随 301。
pub fn login_url(port: i64, vnc_password: &str) -> String {
    format!(
        "http://127.0.0.1:{port}/vnc.html?path=websockify/&autoconnect=true&resize=remote&password={vnc_password}"
    )
}

use crate::store::Rule;

/// 对照 clash_provider:behavior classical 的 rule-provider payload。命门 #2:域名经 bare。
pub fn clash_provider_text(rules: &[Rule]) -> String {
    let mut lines = vec!["payload:".to_string()];
    for r in rules.iter().filter(|r| r.enabled != 0) {
        if r.kind == "ip" {
            lines.push(format!("  - IP-CIDR,{}", r.pattern));
        } else {
            lines.push(format!("  - DOMAIN-SUFFIX,{}", bare(&r.pattern)));
        }
    }
    lines.join("\n") + "\n"
}

/// 对照 clash_snippet:节点 + provider 订阅 + 内联两种(二选一)。命门 #2:IP/域名都带 no-resolve(入口侧语义)。
pub fn clash_snippet_text(rules: &[Rule], mihomo_host_port: &str, ui_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let ui = if ui_port.is_empty() { "<UI端口>" } else { ui_port };
    let mut l = vec![
        "# ① 在你现有 Clash 的 proxies: 下加这个节点".to_string(),
        "proxies:".into(),
        "  - name: vpn-router".into(),
        "    type: socks5".into(),
        "    server: 127.0.0.1".into(),
        format!("    port: {port}"),
        "".into(),
        "# ② 方式甲(推荐):订阅一份规则,绑新域名/IP 自动同步,之后不再动 Clash".into(),
        "rule-providers:".into(),
        "  vpn-rules:".into(),
        "    type: http".into(),
        "    behavior: classical".into(),
        "    format: yaml".into(),
        format!("    url: http://127.0.0.1:{ui}/clash/vpn-rules.yaml"),
        "    interval: 60".into(),
        "    path: ./providers/vpn-rules.yaml".into(),
        "# rules: 顶部加一行引用(no-resolve 对清单内 IP-CIDR 生效)".into(),
        "  - RULE-SET,vpn-rules,vpn-router,no-resolve".into(),
        "".into(),
        "# ② 方式乙:直接内联(不想用 provider 时)".into(),
    ];
    let enabled: Vec<&Rule> = rules.iter().filter(|r| r.enabled != 0).collect();
    if enabled.is_empty() {
        l.push("  # (还没绑定任何规则)".into());
    } else {
        for r in enabled {
            if r.kind == "ip" {
                l.push(format!("  - IP-CIDR,{},vpn-router,no-resolve", r.pattern));
            } else {
                l.push(format!("  - DOMAIN-SUFFIX,{},vpn-router,no-resolve", bare(&r.pattern)));
            }
        }
    }
    l.push("".into());
    l.push(format!("# ③ 无 Clash 时:把系统/浏览器代理指向 127.0.0.1:{port}"));
    l.push("#    本工具 mihomo 自身分流:命中→VPN 容器,其余→直连。".into());
    l.join("\n")
}

/// 对照 entry_pac:命中域名/v4 网段走入口 SOCKS5,其余 DIRECT(v6 网段略过,isInNet 仅 v4)。
pub fn pac_text(rules: &[Rule], mihomo_host_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let proxy = format!("SOCKS5 127.0.0.1:{port}; SOCKS 127.0.0.1:{port}; DIRECT");
    let mut domains = Vec::new();
    let mut nets = Vec::new();
    for r in rules.iter().filter(|r| r.enabled != 0) {
        if r.kind == "domain" {
            let d = bare(&r.pattern).trim().to_lowercase();
            if !d.is_empty() {
                domains.push(d);
            }
        } else if let Ok(ipnet::IpNet::V4(n)) = r.pattern.parse::<ipnet::IpNet>() {
            nets.push((n.network().to_string(), n.netmask().to_string()));
        }
    }
    let dom_js = domains.iter().map(|d| format!("\"{d}\"")).collect::<Vec<_>>().join(",");
    let net_js = nets.iter().map(|(a, m)| format!("[\"{a}\",\"{m}\"]")).collect::<Vec<_>>().join(",");
    let mut s = String::new();
    s.push_str("// 本工具自动生成 · 命中客户域名/IP 走入口,其余 DIRECT\n");
    s.push_str(&format!("var PROXY = \"{proxy}\";\n"));
    s.push_str(&format!("var DOMAINS = [{dom_js}];\n"));
    s.push_str(&format!("var NETS = [{net_js}];\n"));
    s.push_str("function FindProxyForURL(url, host) {\n");
    s.push_str("  host = (host || '').toLowerCase();\n");
    s.push_str("  for (var i = 0; i < DOMAINS.length; i++) {\n");
    s.push_str("    var d = DOMAINS[i];\n");
    s.push_str("    if (host === d || host.slice(-(d.length + 1)) === '.' + d) return PROXY;\n");
    s.push_str("  }\n");
    s.push_str("  if (/^\\d+\\.\\d+\\.\\d+\\.\\d+$/.test(host)) {\n");
    s.push_str("    for (var j = 0; j < NETS.length; j++) {\n");
    s.push_str("      if (isInNet(host, NETS[j][0], NETS[j][1])) return PROXY;\n");
    s.push_str("    }\n");
    s.push_str("  }\n");
    s.push_str("  return 'DIRECT';\n");
    s.push_str("}\n");
    s
}

/// 对照 entry_setup_commands:各平台指向入口的开/关命令。
pub fn setup_commands(mihomo_host_port: &str, ui_port: &str) -> serde_json::Value {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let pac_url = if ui_port.is_empty() {
        "/entry/proxy.pac".to_string()
    } else {
        format!("http://127.0.0.1:{ui_port}/entry/proxy.pac")
    };
    serde_json::json!({
        "port": port,
        "pac_url": pac_url,
        "macos": {
            "socks_on": format!("networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 {port}"),
            "socks_off": "networksetup -setsocksfirewallproxystate Wi-Fi off",
            "pac_on": format!("networksetup -setautoproxyurl Wi-Fi {pac_url}"),
            "pac_off": "networksetup -setautoproxystate Wi-Fi off",
        },
        "windows": format!("设置 → 网络和 Internet → 代理 → 手动设置代理填 127.0.0.1:{port};或「使用安装脚本」填 PAC URL"),
        "env": {
            "socks": format!("export ALL_PROXY=socks5h://127.0.0.1:{port}"),
            "http": format!("export HTTPS_PROXY=http://127.0.0.1:{port} HTTP_PROXY=http://127.0.0.1:{port}"),
            "unset": "unset ALL_PROXY HTTPS_PROXY HTTP_PROXY",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_bare_ipv4_gets_32() {
        assert_eq!(classify("10.1.2.3"), Some(("ip".into(), "10.1.2.3/32".into())));
    }
    #[test]
    fn classify_bare_ipv6_gets_128() {
        assert_eq!(classify("fd00::1"), Some(("ip".into(), "fd00::1/128".into())));
    }
    #[test]
    fn classify_cidr_passthrough_strict_false() {
        assert_eq!(classify("10.0.0.0/8"), Some(("ip".into(), "10.0.0.0/8".into())));
        assert_eq!(classify("10.1.2.3/8"), Some(("ip".into(), "10.1.2.3/8".into())));
        assert_eq!(classify("10.0.0.0/40"), None);
    }
    #[test]
    fn classify_domain_strips_scheme_path_port_userinfo() {
        assert_eq!(classify("https://oa.x.com/login"), Some(("domain".into(), "oa.x.com".into())));
        assert_eq!(classify("user@MAIL.X.com:443"), Some(("domain".into(), "mail.x.com".into())));
        assert_eq!(classify("  Corp.Example.Com.  "), Some(("domain".into(), "corp.example.com".into())));
    }
    #[test]
    fn classify_empty_is_none() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("   "), None);
        assert_eq!(classify("https://"), None);
    }

    #[test]
    fn classify_zone_scoped_ipv6_is_ip_not_domain() {
        // 对照 Python ipaddress:scoped IPv6 仍是 IP(补 /128),不被当域名
        assert_eq!(classify("fe80::1%eth0"), Some(("ip".into(), "fe80::1%eth0/128".into())));
    }
    #[test]
    fn bare_strips_plus_and_star() {
        assert_eq!(bare("+.x.com"), "x.com");
        assert_eq!(bare("*.x.com"), "x.com");
        assert_eq!(bare("x.com"), "x.com");
    }

    #[test]
    fn plan_rules_auto_classifies_and_dedups() {
        let existing = vec![("domain".to_string(), "a.com".to_string())];
        let out = plan_rules(
            &["a.com".into(), "https://b.com/x".into(), "10.0.0.0/8".into(), "10.0.0.0/40".into()],
            None,
            &existing,
        );
        assert!(out.to_add.contains(&("domain".into(), "b.com".into())));
        assert!(out.to_add.contains(&("ip".into(), "10.0.0.0/8".into())));
        assert!(!out.to_add.iter().any(|(_, p)| p == "a.com"), "已存在不重复");
        assert_eq!(out.added.get("ip"), Some(&1));
        assert_eq!(out.added.get("domain"), Some(&1));
        assert!(out.rejected.contains(&"10.0.0.0/40".to_string()), "非法 CIDR 拒绝");
    }

    #[test]
    fn plan_rules_forced_ip_rejects_non_ip() {
        let out = plan_rules(&["not-an-ip".into(), "10.0.0.1".into()], Some("ip"), &[]);
        assert!(out.rejected.contains(&"not-an-ip".to_string()));
        assert!(out.to_add.contains(&("ip".into(), "10.0.0.1/32".into())));
    }

    #[test]
    fn plan_rules_forced_domain_keeps_raw_trimmed() {
        let out = plan_rules(&["  Corp.COM  ".into()], Some("domain"), &[]);
        assert!(out.to_add.contains(&("domain".into(), "Corp.COM".into())));
    }

    #[test]
    fn login_url_has_websockify_path_and_password() {
        let u = login_url(45678, "deadbeef");
        assert!(u.contains("127.0.0.1:45678/vnc.html"));
        assert!(u.contains("path=websockify/"), "尾斜杠不可少(tinyproxy 301 不被 WS 跟随)");
        assert!(u.contains("autoconnect=true"));
        assert!(u.contains("password=deadbeef"));
    }

    fn rl(kind: &str, pat: &str, en: i64) -> crate::store::Rule {
        crate::store::Rule { id: 0, channel_id: "c".into(), kind: kind.into(), pattern: pat.into(), enabled: en }
    }

    #[test]
    fn clash_provider_classical_skips_disabled() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "+.x.com", 1), rl("domain", "off.com", 0)];
        let out = clash_provider_text(&rs);
        assert!(out.starts_with("payload:"));
        assert!(out.contains("  - IP-CIDR,10.0.0.0/8"));
        assert!(out.contains("  - DOMAIN-SUFFIX,x.com")); // bare 去 +.
        assert!(!out.contains("off.com"));
    }

    #[test]
    fn clash_snippet_inline_has_no_resolve_and_node() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "*.y.com", 1)];
        let out = clash_snippet_text(&rs, "7899", "8787");
        assert!(out.contains("name: vpn-router"));
        assert!(out.contains("port: 7899"));
        assert!(out.contains("http://127.0.0.1:8787/clash/vpn-rules.yaml"));
        assert!(out.contains("IP-CIDR,10.0.0.0/8,vpn-router,no-resolve"));
        assert!(out.contains("DOMAIN-SUFFIX,y.com,vpn-router,no-resolve"));
    }

    #[test]
    fn pac_text_ipv4_netmask_and_domains() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "x.com", 1), rl("ip", "fd00::/8", 1)];
        let out = pac_text(&rs, "7899");
        assert!(out.contains("SOCKS5 127.0.0.1:7899"));
        assert!(out.contains(r#"["10.0.0.0","255.0.0.0"]"#), "v4 netmask 由 ipnet 算");
        assert!(out.contains(r#""x.com""#));
        assert!(!out.contains("fd00"), "v6 网段 PAC 略过(isInNet 仅 v4)");
    }
}
