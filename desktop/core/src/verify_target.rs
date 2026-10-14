//! 内网验证目标自动确定。用户填了内网验证地址时照旧按网页探活(底线 1 不变);没填时,只有出口
//! 绑在隧道上的类型才用该通道规则里的域名或单个地址,经通道 SOCKS5 做端口握手,判据放宽为「端口能连上」。
use crate::config::Config;
use crate::store::{ChannelPublic, Rule};
use crate::manager::ProbeOutcome;

/// 握手依次尝试的端口,第一个能连上的即结果。
const HANDSHAKE_PORTS: [u16; 3] = [443, 80, 22];

/// 握手脚本:地址作为位置参数传入,不拼进脚本。`-w` 在失败时也会输出,末尾追加退出码。
const HANDSHAKE_SCRIPT: &str = "curl -s -o /dev/null -w '%{time_total}' --socks5-hostname \"$1\" --connect-timeout 3 --max-time 3 \"$2\"; echo \" rc=$?\"";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 用户填的内网验证地址,走 [`crate::manager::probe`]。
    Url(String),
    /// 规则里的域名或单个地址,走 [`handshake`]。
    Host(String),
}

impl Target {
    pub fn as_str(&self) -> &str {
        match self { Target::Url(s) | Target::Host(s) => s }
    }
}

/// 出口是否绑在隧道上:只有 oss 家族的 dante 用 `external: <tun>` 绑定出口
/// (`images/oss/sockd.conf.tmpl`)。hagb、byo 的 SOCKS 跟随容器路由表,没登录时握手也可能经公网
/// 成功,不能拿规则里的地址当验证目标;类型清单里查不到的同样按没绑处理。
fn egress_pinned(vpn_type: &str) -> bool {
    crate::registry::get(vpn_type).is_ok_and(|spec| spec.runtime == "oss")
}

/// `probe_url` 不为空 → `Url`;出口没绑在隧道上的类型 → `None`;否则该通道第一条启用的域名规则
/// (按 id)→ `Host`;否则第一条 `/32` 或 `/128` 地址规则 → `Host`(去掉前缀);都没有 → `None`。
/// 网段规则不算。
pub fn pick(ch: &ChannelPublic, rules: &[Rule]) -> Option<Target> {
    if !ch.probe_url.is_empty() {
        return Some(Target::Url(ch.probe_url.clone()));
    }
    if !egress_pinned(&ch.vpn_type) {
        return None;
    }
    let mut own: Vec<&Rule> = rules.iter().filter(|r| r.channel_id == ch.id && r.enabled != 0).collect();
    own.sort_by_key(|r| r.id);
    let normalized = |r: &&Rule| crate::webutil::normalize_stored_rule(&r.kind, &r.pattern);
    if let Some((_, host)) = own.iter().filter_map(normalized).find(|(kind, _)| kind == "domain") {
        return Some(Target::Host(host));
    }
    own.iter().filter_map(normalized)
        .filter(|(kind, _)| kind == "ip")
        .filter_map(|(_, pattern)| pattern.parse::<ipnet::IpNet>().ok())
        .find(|net| net.prefix_len() == net.max_prefix_len())
        .map(|net| Target::Host(net.addr().to_string()))
}

/// curl 退出码判定:5、6、7、28、97(解析失败、连不上、超时、代理握手失败)之外都算端口能连上。
pub fn reachable_from(rc: i32) -> bool {
    !matches!(rc, 5 | 6 | 7 | 28 | 97)
}

/// 没有有效退出码、缺少命令等执行故障不构成断线证据。
fn parse_handshake(out: &str) -> ProbeOutcome {
    let Some(rc) = out.split_whitespace().find_map(|t| t.strip_prefix("rc=")).and_then(|s| s.parse::<i32>().ok()) else {
        return ProbeOutcome::failed("握手探针未返回退出码");
    };
    if !reachable_from(rc) {
        return ProbeOutcome::NotConnected;
    }
    if !matches!(rc, 0 | 1 | 8 | 22 | 35 | 52 | 56) { return ProbeOutcome::failed("握手探针执行异常"); }
    let secs = out.split_whitespace().next().and_then(|s| s.parse::<f64>().ok()).filter(|s| s.is_finite() && *s >= 0.0);
    ProbeOutcome::Connected { latency: secs.map(|s| (s * 1000.0) as i64) }
}

fn handshake_url(host: &str, port: u16) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("http://[{host}]:{port}/")
    } else {
        format!("http://{host}:{port}/")
    }
}

/// 经通道 SOCKS5 依次握手 443、80、22,每个端口单独调用一次探针容器(各有 10 秒上限)。
/// 返回三态结果和成功端口；执行故障不冒充三个端口都不通。
pub async fn handshake(docker: Option<&bollard::Docker>, cfg: &Config, cid: &str, host: &str) -> (ProbeOutcome, Option<u16>) {
    let Some(docker) = docker else { return (ProbeOutcome::failed("容器管理连接不可用"), None) };
    let proxy = format!("vpn-{cid}:1080");
    for port in HANDSHAKE_PORTS {
        let url = handshake_url(host, port);
        let cmd = vec!["sh", "-c", HANDSHAKE_SCRIPT, "sh", proxy.as_str(), url.as_str()];
        match crate::docker::run_probe_capture(docker, cfg, crate::manager::PROBE_IMAGE, cmd).await {
            Ok(out) => match parse_handshake(&out) {
                outcome @ ProbeOutcome::Connected { .. } => return (outcome, Some(port)),
                outcome @ ProbeOutcome::ProbeFailed { .. } => return (outcome, None),
                ProbeOutcome::NotConnected => {},
            },
            Err(_) => return (ProbeOutcome::failed("握手探针执行失败"), None),
        }
    }
    (ProbeOutcome::NotConnected, None)
}

/// 按目标检测连通:`Url` 走原来的网页探活(含 hagb 域名缓存恢复),`Host` 走 [`handshake`]。
pub async fn probe(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic, target: &Target) -> ProbeOutcome {
    match target {
        Target::Url(_) => crate::manager::probe(docker, cfg, ch).await,
        Target::Host(host) => {
            handshake(docker, cfg, &ch.id, host).await.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(probe_url: &str) -> ChannelPublic {
        typed("wireguard", probe_url)
    }

    fn typed(vpn_type: &str, probe_url: &str) -> ChannelPublic {
        ChannelPublic {
            id: "c1".into(), name: "n".into(), vpn_type: vpn_type.into(), server: "".into(),
            ec_ver: None, login_method: "headless".into(), username: "".into(),
            vnc_password: None, mac: None, novnc_port: None, probe_url: probe_url.into(),
            status: "running".into(), container_id: None, latency_ms: None, config: serde_json::json!({}),
            routing_enabled: true,
        }
    }

    fn rule(id: i64, channel_id: &str, kind: &str, pattern: &str, enabled: i64) -> Rule {
        Rule { id, channel_id: channel_id.into(), kind: kind.into(), pattern: pattern.into(), enabled, note: String::new(), locked: 0 }
    }

    #[test]
    fn pick_prefers_url_then_domain_then_single_address() {
        // A-07:填了验证地址就用它,规则不看。
        let rules = vec![rule(1, "c1", "domain", "corp.example.test", 1)];
        assert_eq!(pick(&channel("http://portal.example.test/"), &rules), Some(Target::Url("http://portal.example.test/".into())));

        // 没填:第一条启用的域名规则(按 id),停用的和别的通道的不算,前缀已剥。
        let rules = vec![
            rule(9, "c1", "domain", "late.example.test", 1),
            rule(2, "c1", "ip", "10.1.2.3/32", 1),
            rule(3, "c1", "domain", "off.example.test", 0),
            rule(4, "c2", "domain", "other.example.test", 1),
            rule(5, "c1", "domain", "*.corp.example.test", 1),
        ];
        assert_eq!(pick(&channel(""), &rules), Some(Target::Host("corp.example.test".into())));

        // 没有域名规则:第一条单个地址,网段不算;IPv6 同样去掉前缀。
        let rules = vec![
            rule(1, "c1", "ip", "10.0.0.0/8", 1),
            rule(2, "c1", "ip", "fd00::1/128", 1),
            rule(3, "c1", "ip", "10.1.2.3/32", 1),
        ];
        assert_eq!(pick(&channel(""), &rules), Some(Target::Host("fd00::1".into())));

        // 只有网段或停用的规则:没有目标。
        let rules = vec![rule(1, "c1", "ip", "10.0.0.0/8", 1), rule(2, "c1", "ip", "192.0.2.1/32", 0)];
        assert_eq!(pick(&channel(""), &rules), None);
        assert_eq!(pick(&channel(""), &[]), None);
    }

    #[test]
    fn rule_targets_only_for_tunnel_pinned_types() {
        // 底线 1:hagb、byo 的出口没绑隧道,没填验证地址时不拿规则握手;oss 照常用规则里的目标。
        let rules = vec![rule(1, "c1", "domain", "corp.example.test", 1), rule(2, "c1", "ip", "10.1.2.3/32", 1)];
        for vpn_type in ["easyconnect", "atrust", "custom", "hillstone", "no-such-type"] {
            assert_eq!(pick(&typed(vpn_type, ""), &rules), None, "{vpn_type} has no tunnel-pinned egress");
        }
        for vpn_type in ["wireguard", "openvpn", "anyconnect", "openfortivpn"] {
            assert_eq!(pick(&typed(vpn_type, ""), &rules), Some(Target::Host("corp.example.test".into())), "{vpn_type}");
        }
        // 填了验证地址:三个家族都用它。
        for vpn_type in ["easyconnect", "custom", "wireguard"] {
            assert_eq!(pick(&typed(vpn_type, "http://portal.example.test/"), &rules),
                Some(Target::Url("http://portal.example.test/".into())), "{vpn_type}");
        }
    }

    #[test]
    fn exit_codes_decide_reachability() {
        for rc in [5, 6, 7, 28, 97] { assert!(!reachable_from(rc), "rc={rc} means unreachable"); }
        // 0 正常应答;1、8 应答不是 HTTP(22 端口的 SSH 横幅落在哪一个待实测);35 TLS;52 空应答;
        // 56 收包失败——对端都已接受连接。
        for rc in [0, 1, 8, 22, 35, 52, 56] { assert!(reachable_from(rc), "rc={rc} means reachable"); }
    }

    #[test]
    fn handshake_output_parsing() {
        assert_eq!(parse_handshake("0.123456 rc=0"), ProbeOutcome::Connected { latency: Some(123) });
        assert_eq!(parse_handshake("0.050000 rc=52\n"), ProbeOutcome::Connected { latency: Some(50) });
        assert_eq!(parse_handshake("3.001000 rc=28"), ProbeOutcome::NotConnected);
        assert_eq!(parse_handshake("0.000000 rc=7"), ProbeOutcome::NotConnected);
        assert_eq!(parse_handshake(" rc=1"), ProbeOutcome::Connected { latency: None });
        assert!(matches!(parse_handshake(""), ProbeOutcome::ProbeFailed { .. }));
        assert!(matches!(parse_handshake("OCI runtime exec failed"), ProbeOutcome::ProbeFailed { .. }));
        assert!(matches!(parse_handshake("sh: curl: not found rc=127"), ProbeOutcome::ProbeFailed { .. }));
    }

    #[test]
    fn handshake_url_brackets_ipv6_only() {
        assert_eq!(handshake_url("corp.example.test", 443), "http://corp.example.test:443/");
        assert_eq!(handshake_url("10.1.2.3", 80), "http://10.1.2.3:80/");
        assert_eq!(handshake_url("fd00::1", 22), "http://[fd00::1]:22/");
    }
}
