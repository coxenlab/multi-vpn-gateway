//! 测试地址:一个域名或地址现在走哪条通道。和内层入口同源([`crate::manager::ordered_rules`]),
//! 按首个命中取第一条。入口开了嗅探时,按 IP 访问的连接可能先命中域名行,这里算不出来。
use std::collections::{HashMap, HashSet};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use crate::manager::OrderedRule;
use crate::store::{ChannelPublic, Rule};
use crate::AppState;

#[derive(Serialize, Debug)]
pub struct Outcome {
    pub q: String,
    pub kind: String,
    pub normalized: String,
    /// `channel` / `blocked` / `direct`。
    pub result: String,
    pub rule: Option<MatchedRule>,
    pub channel: Option<MatchedChannel>,
    pub reachable: Option<Reachable>,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct MatchedRule {
    pub id: i64,
    pub kind: String,
    pub pattern: String,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
pub struct MatchedChannel {
    pub id: String,
    pub name: String,
    pub status: String,
}

#[derive(Serialize, Debug)]
pub struct Reachable {
    pub ok: bool,
    pub latency_ms: Option<i64>,
    pub port: Option<u16>,
}

enum Input {
    Domain(String),
    Ip(std::net::IpAddr),
}

/// 经 [`crate::webutil::classify`] 归一。含换行或逗号、网段、非法输入都拒绝。
fn parse(q: &str) -> Result<(Input, String, String), &'static str> {
    if q.contains(['\n', '\r', ',']) {
        return Err("一次只能测试一个域名或地址");
    }
    let Some((kind, normalized)) = crate::webutil::classify(q) else {
        return Err("不是有效的域名或地址");
    };
    if kind == "domain" {
        return Ok((Input::Domain(normalized.clone()), kind, normalized));
    }
    let net: ipnet::IpNet = normalized.parse().map_err(|_| "不是有效的域名或地址")?;
    if net.prefix_len() != net.max_prefix_len() {
        return Err("网段不能测试,请填一个地址");
    }
    Ok((Input::Ip(net.addr()), kind, normalized))
}

/// 域名只和域名行比(与 PAC 同写法),地址只和地址行比;IP 行带 no-resolve,域名不会解析后再比。
fn matches(input: &Input, r: &OrderedRule) -> bool {
    match input {
        Input::Domain(host) => r.kind == "domain" && (*host == r.pattern || host.ends_with(&format!(".{}", r.pattern))),
        Input::Ip(addr) => r.kind == "ip" && r.pattern.parse::<ipnet::IpNet>().is_ok_and(|n| n.contains(addr)),
    }
}

/// 纯函数:按入口的规则顺序取第一个命中。`rules` 是有效规则(已停止通道的已折叠),
/// `lost` 是掉线集合。`reachable` 恒为 `None`,由接口按命中通道的状态决定是否握手。
pub fn evaluate(q: &str, channels: &[ChannelPublic], rules: &[Rule], lost: &HashSet<String>) -> Result<Outcome, &'static str> {
    let (input, kind, normalized) = parse(q)?;
    let ordered = crate::manager::ordered_rules(channels, rules, lost);
    let hit = ordered.iter().find(|r| matches(&input, r));
    let result = match hit {
        Some(r) if r.target == "REJECT" => "blocked",
        Some(_) => "channel",
        None => "direct",
    };
    Ok(Outcome {
        q: q.to_string(),
        kind,
        normalized,
        result: result.into(),
        rule: hit.map(|r| MatchedRule { id: r.rule_id, kind: r.kind.clone(), pattern: r.pattern.clone() }),
        channel: hit.and_then(|r| channels.iter().find(|c| c.id == r.channel_id))
            .map(|c| MatchedChannel { id: c.id.clone(), name: c.name.clone(), status: c.status.clone() }),
        reachable: None,
    })
}

/// 命中已连接、没被阻止的通道时,握手用的主机名或地址;其他情况不发任何请求。
fn handshake_host(out: &Outcome) -> Option<String> {
    if out.result != "channel" || out.channel.as_ref()?.status != "logged_in" {
        return None;
    }
    Some(match out.kind.as_str() {
        "ip" => out.normalized.split('/').next().unwrap_or_default().to_string(),
        _ => out.normalized.clone(),
    })
}

/// `GET /api/rules/test?q=`。用 GET,不经过写请求的活动许可。
pub async fn test(State(st): State<AppState>, Query(params): Query<HashMap<String, String>>) -> axum::response::Response {
    let q = params.get("q").map(String::as_str).unwrap_or_default();
    let db = st.cfg.db_path();
    let snapshot = match crate::config_apply_store::snapshot(&db, crate::store::routing_off(&st.cfg.data_dir)) {
        Ok(s) => s,
        Err(e) => return crate::api::err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("snapshot: {e}")),
    };
    let mut out = match evaluate(q, &snapshot.channels, &snapshot.rules, &crate::lifecycle::lost_channels(&db)) {
        Ok(out) => out,
        Err(msg) => return crate::api::err_detail(StatusCode::BAD_REQUEST, msg),
    };
    if let (Some(host), Some(ch)) = (handshake_host(&out), out.channel.as_ref()) {
        let (result, port) = crate::verify_target::handshake(st.docker().as_ref(), &st.cfg, &ch.id, &host).await;
        out.reachable = match result {
            crate::manager::ProbeOutcome::Connected { latency } => Some(Reachable { ok: true, latency_ms: latency, port }),
            crate::manager::ProbeOutcome::NotConnected => Some(Reachable { ok: false, latency_ms: None, port: None }),
            crate::manager::ProbeOutcome::ProbeFailed { .. } => None,
        };
    }
    Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::Value as Yaml;

    /// 四个通道:甲、乙已连接,停已停止,错出错。返回数据库目录(测试结束前保持)。
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (id, name, status) in [("a", "甲", "logged_in"), ("b", "乙", "logged_in"), ("s", "停", "stopped"), ("e", "错", "error")] {
            conn.execute("INSERT INTO channels(id,name,status) VALUES(?1,?2,?3)", rusqlite::params![id, name, status]).unwrap();
        }
        for (cid, kind, pattern) in [
            ("a", "domain", "example.test"),
            ("a", "ip", "10.20.0.0/16"),
            ("a", "ip", "fd00::/64"),
            ("b", "domain", "*.corp.example.test"),
            ("b", "ip", "10.0.0.0/8"),
            ("s", "domain", "stopped.test"),
            ("s", "ip", "192.0.2.0/24"),
            ("e", "domain", "broken.test"),
            ("e", "ip", "10.40.0.0/16"),
        ] {
            crate::store::add_rule(&db, cid, kind, pattern).unwrap();
        }
        (dir, db)
    }

    /// 独立按文本读入口配置的首个命中,返回该行的目标;没有命中时是 `MATCH` 的 `DIRECT`。
    fn first_line_target(config: &Yaml, kind: &str, normalized: &str) -> String {
        let host = normalized.split('/').next().unwrap();
        for line in config["rules"].as_sequence().unwrap() {
            let parts: Vec<&str> = line.as_str().unwrap().split(',').collect();
            let hit = match parts[0] {
                "DOMAIN-SUFFIX" => kind == "domain" && (host == parts[1] || host.ends_with(&format!(".{}", parts[1]))),
                "IP-CIDR" => kind == "ip" && parts[1].parse::<ipnet::IpNet>().unwrap().contains(&host.parse::<std::net::IpAddr>().unwrap()),
                "MATCH" => return parts[1].to_string(),
                other => panic!("unexpected rule {other}"),
            };
            if hit {
                return parts[2].to_string();
            }
        }
        panic!("no MATCH line");
    }

    #[test]
    fn r12_hit_miss_stopped_and_errored_follow_the_entry_order() {
        let (_dir, db) = fixture();
        let snapshot = crate::config_apply_store::snapshot(&db, false).unwrap();
        let lost = HashSet::new();
        let config = crate::manager::build_mihomo_config_with(Yaml::Null, &snapshot.channels, &snapshot.rules, &lost);
        let cases = [
            // 命中:更具体的域名在前,宽后缀盖不住;链接只取主机名。
            ("wiki.corp.example.test", "channel", Some("b"), "wiki.corp.example.test"),
            ("https://Portal.Example.test/x", "channel", Some("a"), "portal.example.test"),
            ("example.test", "channel", Some("a"), "example.test"),
            ("10.20.1.2", "channel", Some("a"), "10.20.1.2/32"),
            ("10.99.1.1", "channel", Some("b"), "10.99.1.1/32"),
            ("10.20.1.2/32", "channel", Some("a"), "10.20.1.2/32"),
            ("fd00::1", "channel", Some("a"), "fd00::1/128"),
            // 未命中:走原网络。
            ("other.test", "direct", None, "other.test"),
            ("198.51.100.7", "direct", None, "198.51.100.7/32"),
            // 命中已停止通道:规则已折叠,走原网络。
            ("stopped.test", "direct", None, "stopped.test"),
            ("192.0.2.10", "direct", None, "192.0.2.10/32"),
            // 命中出错通道:被阻止,不落到更宽的 10/8。
            ("x.broken.test", "blocked", Some("e"), "x.broken.test"),
            ("10.40.0.9", "blocked", Some("e"), "10.40.0.9/32"),
        ];
        for (q, result, channel, normalized) in cases {
            let out = evaluate(q, &snapshot.channels, &snapshot.rules, &lost).unwrap();
            assert_eq!(out.result, result, "{q}");
            assert_eq!(out.normalized, normalized, "{q}");
            assert_eq!(out.channel.as_ref().map(|c| c.id.as_str()), channel, "{q}");
            assert_eq!(out.rule.is_some(), channel.is_some(), "{q}");
            assert!(out.reachable.is_none());
            let expected = match (result, channel) {
                ("channel", Some(cid)) => format!("ch-{cid}"),
                ("blocked", _) => "REJECT".into(),
                _ => "DIRECT".into(),
            };
            assert_eq!(first_line_target(&config, &out.kind, &out.normalized), expected, "{q}");
        }

        let out = evaluate("10.40.0.9", &snapshot.channels, &snapshot.rules, &lost).unwrap();
        let rid = crate::store::list_rules(&db, "e").unwrap().into_iter().find(|r| r.kind == "ip").unwrap().id;
        assert_eq!(out.rule, Some(MatchedRule { id: rid, kind: "ip".into(), pattern: "10.40.0.0/16".into() }));
        assert_eq!(out.channel, Some(MatchedChannel { id: "e".into(), name: "错".into(), status: "error".into() }));
        let out = evaluate("wiki.corp.example.test", &snapshot.channels, &snapshot.rules, &lost).unwrap();
        assert_eq!(out.rule.as_ref().unwrap().pattern, "corp.example.test");

        // 掉线集合里的已连接通道同样被阻止,和入口配置一致。
        let lost = HashSet::from(["b".to_string()]);
        let config = crate::manager::build_mihomo_config_with(Yaml::Null, &snapshot.channels, &snapshot.rules, &lost);
        let out = evaluate("10.99.1.1", &snapshot.channels, &snapshot.rules, &lost).unwrap();
        assert_eq!((out.result.as_str(), out.channel.unwrap().id.as_str()), ("blocked", "b"));
        assert_eq!(first_line_target(&config, "ip", "10.99.1.1/32"), "REJECT");
    }

    #[test]
    fn only_a_single_domain_or_address_is_accepted() {
        let lost = HashSet::new();
        for q in ["", "10.0.0.0/8", "fd00::/64", "a.example.test,b.example.test", "a.example.test\nb.example.test",
            "a.example.test\r", "10.1.2.3 10.1.2.4", "fe80::1%en0", "a..example.test"] {
            assert!(evaluate(q, &[], &[], &lost).is_err(), "{q:?}");
        }
        assert_eq!(evaluate("192.0.2.1", &[], &[], &lost).unwrap().result, "direct");
    }

    #[test]
    fn handshake_only_for_connected_unblocked_channel() {
        let (_dir, db) = fixture();
        let snapshot = crate::config_apply_store::snapshot(&db, false).unwrap();
        let lost = HashSet::new();
        let host = |q: &str, channels: &[ChannelPublic]| handshake_host(&evaluate(q, channels, &snapshot.rules, &lost).unwrap());
        assert_eq!(host("wiki.corp.example.test", &snapshot.channels).as_deref(), Some("wiki.corp.example.test"));
        assert_eq!(host("10.20.1.2", &snapshot.channels).as_deref(), Some("10.20.1.2"));
        assert_eq!(host("fd00::1", &snapshot.channels).as_deref(), Some("fd00::1"));
        assert_eq!(host("10.40.0.9", &snapshot.channels), None);
        assert_eq!(host("other.test", &snapshot.channels), None);
        // 待登录的通道:不握手。
        let mut channels = snapshot.channels.clone();
        channels.iter_mut().filter(|c| c.id == "a").for_each(|c| c.status = "running".into());
        assert_eq!(host("10.20.1.2", &channels), None);
    }

    #[test]
    fn custom_rule_test_matches_entry_verification_gate() {
        let lost = HashSet::new();
        let rules = vec![Rule { id: 1, channel_id: "byo".into(), kind: "domain".into(),
            pattern: "intra.example.test".into(), enabled: 1, note: String::new(), locked: 0 }];
        let mut ch = ChannelPublic {
            id: "byo".into(), name: "BYO".into(), vpn_type: "custom".into(), server: String::new(),
            ec_ver: None, login_method: "byo".into(), username: String::new(), vnc_password: None,
            mac: None, novnc_port: None, probe_url: "https://intra.example.test/health".into(),
            status: "running".into(), container_id: None, latency_ms: None,
            config: serde_json::json!({}), routing_enabled: true,
        };
        assert_eq!(evaluate("wiki.intra.example.test", &[ch.clone()], &rules, &lost).unwrap().result, "blocked");
        ch.status = "logged_in".into();
        assert_eq!(evaluate("wiki.intra.example.test", &[ch.clone()], &rules, &lost).unwrap().result, "channel");
        ch.probe_url.clear();
        assert_eq!(evaluate("wiki.intra.example.test", &[ch], &rules, &lost).unwrap().result, "blocked");
    }
}
