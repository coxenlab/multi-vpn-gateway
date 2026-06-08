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
}
