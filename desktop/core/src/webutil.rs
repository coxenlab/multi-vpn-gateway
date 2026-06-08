//! main.py 内联纯逻辑的 Rust 端:规则分类 / Clash 文本 / PAC。对照 app/main.py。

/// 对照 _classify:('ip', cidr) / ('domain', host) / None。
pub fn classify(token: &str) -> Option<(String, String)> {
    let t = token.trim();
    if t.is_empty() {
        return None;
    }
    let addr = t.split('/').next().unwrap_or("");
    if addr.parse::<std::net::IpAddr>().is_err() {
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
}
