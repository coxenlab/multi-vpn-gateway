//! 让开 VPN 服务器地址。容器的出站(VPN 客户端连自己的服务器)经 VM 网络进程从宿主发出;
//! 规则网段若包含这个服务器地址,宿主按虚拟网卡路由把它又送回本工具入口,隧道绕回自己,
//! 约 15 秒后断开(2026-10-22 锐捷实测)。桌面客户端的服务器填在客户端里,核心不知道,
//! 所以这里只读查看 VM 网络进程正在连的公网 IPv4,把它们从虚拟网卡路由里扣掉。
//! 只影响通道管家身份;旧身份的路由逐字节不变。
use std::collections::{BTreeMap, BTreeSet};
use std::net::Ipv4Addr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ipnet::Ipv4Net;

/// 最后一次看到之后保留多久。连接断开、通道停掉后过这么久,路由才恢复。
const KEEP: Duration = Duration::from_secs(15 * 60);

static SEEN: Mutex<BTreeMap<Ipv4Addr, Instant>> = Mutex::new(BTreeMap::new());

/// 只认公网单播地址:内网目标走隧道才对,扣掉会让宿主访问内网改走原网络。
pub fn is_candidate(ip: Ipv4Addr) -> bool {
    let [a, b, ..] = ip.octets();
    !(ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_multicast()
        || ip.is_broadcast() || ip.is_unspecified() || ip.is_documentation()
        || a == 0 || a >= 240
        || (a == 100 && (64..128).contains(&b))   // 运营商共享地址
        || (a == 198 && (b == 18 || b == 19)))    // 基准测试段,本工具占位段在这里
}

/// 从网段里扣掉若干个地址,返回剩下的最少网段(每扣一个最多多出 31 段)。
pub fn subtract(net: Ipv4Net, ips: &[Ipv4Addr]) -> Vec<Ipv4Net> {
    let net = net.trunc();
    let inside: Vec<_> = ips.iter().copied().filter(|ip| net.contains(ip)).collect();
    if inside.is_empty() { return vec![net]; }
    if net.prefix_len() == 32 { return vec![]; }
    net.subnets(net.prefix_len() + 1).expect("prefix below 32")
        .flat_map(|half| subtract(half, &inside)).collect()
}

fn current(now: Instant) -> Vec<Ipv4Addr> {
    SEEN.lock().unwrap().iter().filter(|(_, at)| now.duration_since(**at) < KEEP).map(|(ip, _)| *ip).collect()
}

/// 规则网段扣掉当前要让开的地址;没有落在网段里的地址时返回 None(保持原写法不变)。
pub fn carve(net: Ipv4Net) -> Option<Vec<Ipv4Net>> {
    let ips = current(Instant::now());
    ips.iter().any(|ip| net.contains(ip)).then(|| subtract(net, &ips))
}

fn record(seen: &mut BTreeMap<Ipv4Addr, Instant>, observed: &[Ipv4Addr], now: Instant) {
    for ip in observed.iter().copied().filter(|ip| is_candidate(*ip)) { seen.insert(ip, now); }
    seen.retain(|_, at| now.duration_since(*at) < KEEP);
}

fn inside_rules(ips: &[Ipv4Addr], rules: &[crate::store::Rule]) -> BTreeSet<Ipv4Addr> {
    let nets: Vec<Ipv4Net> = rules.iter()
        .filter(|r| r.enabled != 0 && r.kind == "ip")
        .filter_map(|r| r.pattern.parse::<Ipv4Net>().ok()).collect();
    ips.iter().copied().filter(|ip| nets.iter().any(|n| n.contains(ip))).collect()
}

/// 看门狗每拍调用:采样一次;落在规则网段里的让开地址有变化时记事件并重新对账路由。
pub async fn observe(cfg: &crate::config::Config) {
    if cfg.identity().tun_config_frozen || !cfg.host_integrations_allowed() { return; }
    let home = std::env::var("HOME").unwrap_or_default();
    let observed = match crate::usernet::remote_endpoints(std::path::Path::new(&home), &cfg.vm_profile).await {
        Ok(ips) => ips,
        Err(_) => return, // 查不到就维持上次的结果,到期自然恢复
    };
    let Ok(rules) = crate::store::effective_rules(&cfg.db_path()) else { return };
    let now = Instant::now();
    let before = inside_rules(&current(now), &rules);
    record(&mut SEEN.lock().unwrap(), &observed, now);
    let after = inside_rules(&current(now), &rules);
    if before == after { return; }
    let added: Vec<String> = after.difference(&before).map(|ip| ip.to_string()).collect();
    let removed: Vec<String> = before.difference(&after).map(|ip| ip.to_string()).collect();
    crate::ev!(info, "entry", "server_address_excluded",
        "规则网段包含通道正在连接的服务器地址，已从虚拟网卡路由里让开", {
            "added": added, "removed": removed });
    crate::entry::tun_sync(cfg).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> Ipv4Addr { s.parse().unwrap() }
    fn net(s: &str) -> Ipv4Net { s.parse().unwrap() }

    #[test]
    fn only_public_unicast_addresses_are_excluded() {
        for s in ["1.0.188.78", "8.8.8.8", "4.2.2.1"] { assert!(is_candidate(ip(s)), "{s}"); }
        for s in ["10.1.2.3", "172.16.0.1", "192.168.5.2", "127.0.0.1", "169.254.1.1", "100.64.0.1",
            "198.19.128.1", "198.18.0.1", "224.0.0.1", "0.0.0.0", "255.255.255.255", "240.0.0.1"] {
            assert!(!is_candidate(ip(s)), "{s}");
        }
    }

    #[test]
    fn subtract_removes_exact_host_and_splits_wider_nets() {
        assert!(subtract(net("1.0.188.78/32"), &[ip("1.0.188.78")]).is_empty());
        assert_eq!(subtract(net("10.0.0.0/8"), &[ip("1.0.188.78")]), vec![net("10.0.0.0/8")]);
        let parts = subtract(net("1.0.188.0/24"), &[ip("1.0.188.78")]);
        assert_eq!(parts.len(), 8);
        assert!(parts.iter().all(|p| !p.contains(&ip("1.0.188.78"))));
        let covered: u32 = parts.iter().map(|p| 1u32 << (32 - p.prefix_len())).sum();
        assert_eq!(covered, 255);
        for other in ["1.0.188.0", "1.0.188.77", "1.0.188.79", "1.0.188.255"] {
            assert!(parts.iter().any(|p| p.contains(&ip(other))), "{other}");
        }
        let two = subtract(net("1.0.0.0/16"), &[ip("1.0.188.78"), ip("1.0.1.1"), ip("1.1.1.1")]);
        let covered: u32 = two.iter().map(|p| 1u32 << (32 - p.prefix_len())).sum();
        assert_eq!(covered, 65536 - 2);
        // 非规范写法按网段处理
        assert_eq!(subtract(net("1.0.188.5/24"), &[]), vec![net("1.0.188.0/24")]);
    }

    #[test]
    fn records_expire_and_ignore_private_addresses() {
        let mut seen = BTreeMap::new();
        let t0 = Instant::now();
        record(&mut seen, &[ip("1.0.188.78"), ip("192.168.0.1")], t0);
        assert_eq!(seen.keys().copied().collect::<Vec<_>>(), vec![ip("1.0.188.78")]);
        record(&mut seen, &[], t0 + KEEP - Duration::from_secs(1));
        assert_eq!(seen.len(), 1);
        record(&mut seen, &[], t0 + KEEP);
        assert!(seen.is_empty());
    }

    #[test]
    fn tongdao_routes_skip_the_server_and_legacy_routes_stay_unchanged() {
        let server = ip("1.0.188.78");
        SEEN.lock().unwrap().insert(server, Instant::now());
        let rules = vec![crate::store::Rule { id: 1, channel_id: "c".into(), kind: "ip".into(),
            pattern: "1.0.188.78/32".into(), enabled: 1, note: String::new(), locked: 0 },
            crate::store::Rule { id: 2, channel_id: "c".into(), kind: "ip".into(),
            pattern: "1.0.188.64/28".into(), enabled: 1, note: String::new(), locked: 0 }];
        let (tongdao, _) = crate::entry::route_sets_for(&crate::identity::TONGDAO, &rules);
        let (legacy, _) = crate::entry::route_sets_for(&crate::identity::VPNMGR, &rules);
        SEEN.lock().unwrap().remove(&server);
        assert!(tongdao.iter().all(|r| !r.parse::<Ipv4Net>().unwrap().contains(&server)), "{tongdao:?}");
        assert!(tongdao.contains(&"1.0.188.79/32".to_string()) && tongdao.contains(&"1.0.188.64/29".to_string()));
        assert!(legacy.contains(&"1.0.188.78/32".to_string()) && legacy.contains(&"1.0.188.64/28".to_string()));
    }

    #[test]
    fn only_addresses_inside_enabled_ip_rules_count() {
        let rule = |id, pattern: &str, kind: &str, enabled| crate::store::Rule {
            id, channel_id: "c".into(), kind: kind.into(), pattern: pattern.into(), enabled,
            note: String::new(), locked: 0,
        };
        let rules = vec![rule(1, "1.0.188.0/24", "ip", 1), rule(2, "8.8.8.0/24", "ip", 0),
            rule(3, "example.com", "domain", 1)];
        let got = inside_rules(&[ip("1.0.188.78"), ip("8.8.8.8"), ip("1.1.1.1")], &rules);
        assert_eq!(got.into_iter().collect::<Vec<_>>(), vec![ip("1.0.188.78")]);
    }
}
