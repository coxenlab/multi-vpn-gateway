use axum::{extract::State, Json};
use serde_json::{json, Value};
use std::collections::HashMap;
use crate::{store, AppState};

pub async fn system(State(st): State<AppState>) -> Json<Value> {
    let alive = st.mihomo.alive().await;
    let mihomo_port = st.cfg.mihomo_host_port.parse::<u64>().ok().filter(|n| *n != 0);
    let controller = st.cfg.mihomo_ctrl_port.as_ref().map(|p| format!("127.0.0.1:{p}"));
    Json(json!({
        "mihomo_status": if alive { "running" } else { "down" },
        "mihomo_port": mihomo_port,
        "controller": controller,
        "ui_port": st.cfg.ui_port,
        "bound_ip": "127.0.0.1", // 命门 #4
    }))
}

pub async fn proxies(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "proxies": st.mihomo.proxies().await }))
}

pub async fn channels(State(st): State<AppState>) -> Json<Value> {
    let db = st.cfg.db_path();
    let chans = store::list_channels(&db).unwrap_or_default();

    let mut rules_map: HashMap<String, (Vec<Value>, Vec<Value>)> = HashMap::new();
    let mut uptime_map: HashMap<String, Option<String>> = HashMap::new();
    for c in &chans {
        let rules = store::list_rules(&db, &c.id).unwrap_or_default();
        rules_map.insert(c.id.clone(), split_rules(rules));
        let up = if c.status != "stopped" {
            crate::docker::uptime(st.docker.as_ref(), &c.id).await
        } else {
            None
        };
        uptime_map.insert(c.id.clone(), up);
    }
    Json(json!(build_channels_response(chans, &rules_map, &uptime_map)))
}

/// 纯函数:把通道 + 预取的 rules/uptime 组装成 /api/channels 输出(对照 main.py channels 路由)。
pub fn build_channels_response(
    channels: Vec<store::ChannelPublic>,
    rules_map: &HashMap<String, (Vec<Value>, Vec<Value>)>,
    uptime_map: &HashMap<String, Option<String>>,
) -> Vec<Value> {
    channels
        .into_iter()
        .map(|ch| {
            let id = ch.id.clone();
            let (domains, ips) = rules_map.get(&id).cloned().unwrap_or_default();
            let uptime = uptime_map.get(&id).cloned().flatten();
            let status = if uptime.is_none() && (ch.status == "running" || ch.status == "logged_in") {
                "down".to_string()
            } else {
                ch.status.clone()
            };
            let mut v = serde_json::to_value(&ch).unwrap();
            let o = v.as_object_mut().unwrap();
            o.insert("status".into(), json!(status));
            o.insert("domains".into(), json!(domains));
            o.insert("ips".into(), json!(ips));
            o.insert("volume_name".into(), json!(format!("vpndata-{id}")));
            o.insert("socks_proxy".into(), json!(format!("ch-{id}")));         // 命门 #7
            o.insert("socks_endpoint".into(), json!(format!("vpn-{id}:1080"))); // 命门 #7
            o.insert("uptime".into(), json!(uptime));
            v
        })
        .collect()
}

/// 对照 main.py:domains = kind=='domain',ips = kind=='ip'(其它 kind 两边都不进)。
pub fn split_rules(rules: Vec<store::Rule>) -> (Vec<Value>, Vec<Value>) {
    let mut domains = vec![];
    let mut ips = vec![];
    for r in rules {
        let v = serde_json::to_value(&r).unwrap();
        match r.kind.as_str() {
            "ip" => ips.push(v),
            "domain" => domains.push(v),
            _ => {}
        }
    }
    (domains, ips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn ch(id: &str, status: &str) -> crate::store::ChannelPublic {
        crate::store::ChannelPublic {
            id: id.into(), name: "n".into(), vpn_type: "easyconnect".into(),
            server: "s".into(), ec_ver: None, login_method: "interactive".into(),
            username: "u".into(), vnc_password: None, mac: None, novnc_port: None,
            probe_url: "".into(), status: status.into(), container_id: None,
            latency_ms: None, config: json!({}),
        }
    }

    #[test]
    fn down_override_when_running_but_no_uptime() {
        let chans = vec![ch("a", "running"), ch("b", "logged_in"), ch("c", "stopped")];
        let rules: HashMap<String, (Vec<serde_json::Value>, Vec<serde_json::Value>)> = HashMap::new();
        let mut up: HashMap<String, Option<String>> = HashMap::new();
        up.insert("a".into(), None);
        up.insert("b".into(), Some("3分钟".into()));
        up.insert("c".into(), None);

        let out = build_channels_response(chans, &rules, &up);
        assert_eq!(out[0]["status"], "down");
        assert_eq!(out[1]["status"], "logged_in");
        assert_eq!(out[1]["uptime"], "3分钟");
        assert_eq!(out[2]["status"], "stopped");
        assert_eq!(out[0]["socks_proxy"], "ch-a");
        assert_eq!(out[0]["socks_endpoint"], "vpn-a:1080");
        assert_eq!(out[0]["volume_name"], "vpndata-a");
    }

    #[test]
    fn splits_rules_by_kind() {
        let rules = vec![
            crate::store::Rule { id: 1, channel_id: "a".into(), kind: "domain".into(), pattern: "x.com".into(), enabled: 1 },
            crate::store::Rule { id: 2, channel_id: "a".into(), kind: "ip".into(), pattern: "10.0.0.0/8".into(), enabled: 1 },
        ];
        let (domains, ips) = split_rules(rules);
        assert_eq!(domains.len(), 1);
        assert_eq!(ips.len(), 1);
        assert_eq!(domains[0]["pattern"], "x.com");
        assert_eq!(ips[0]["pattern"], "10.0.0.0/8");
    }
}
