use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use crate::{store, AppState};
use crate::api::err_detail;

pub async fn system(State(st): State<AppState>) -> Json<Value> {
    let alive = crate::runtime::serving(&st) && st.mihomo.alive().await;
    let mihomo_port = st.cfg.mihomo_host_port.parse::<u64>().ok().filter(|n| *n != 0);
    let controller = st.cfg.mihomo_ctrl_port.as_ref().map(|p| format!("127.0.0.1:{p}"));
    // 分流口健康快照(看门狗维护):前端横幅据此区分「自愈中 / 已放弃 / VM 挂」。
    let h = st.health.lock().ok().map(|s| s.clone()).unwrap_or_default();
    Json(json!({
        "mihomo_status": if alive { "running" } else { "down" },
        "mihomo_port": mihomo_port,
        "controller": controller,
        "ui_port": st.cfg.ui_port,
        "bound_ip": "127.0.0.1", // 命门 #4
        "gateway_health": h.gateway_health,
        "gateway_checked_at_ms": h.gateway_checked_at_ms,
        "proxy_port_reachable": h.proxy_port_reachable,
        "healing": h.healing,
        "gave_up": h.gave_up,
        "vm_egress_dead": h.vm_egress_dead,
        "usernet": h.usernet,
        "egress_guard_checked_at": h.egress_guard_checked_at,
        "egress_guard_applied": h.egress_guard_applied,
        "self_heal_enabled": st.self_heal_enabled(),
        "vm_profile": st.cfg.vm_profile,
        "runtime": if st.cfg.managed_vm { serde_json::to_value(st.lifecycle.runtime().snapshot()).ok() } else { None },
        "host_integrations_available": st.cfg.host_integrations_allowed(),
        "probe_cache_available": true,
        "routing_off": store::routing_off(&st.cfg.data_dir),
        "config_application": crate::config_apply_store::public_status(&st.cfg.db_path(), store::routing_off(&st.cfg.data_dir)),
    }))
}

/// 手动修复分流口,与看门狗同一两级梯子(横幅按钮 / env-check 用):
/// 1. 重建 app 自持的 SSH 转发(秒级,不动容器、不需重登通道);
/// 2. 仍不过数据 → 重启 mihomo 容器,再按新 IP 重建转发。
///
/// 成功判据是端到端握手([`crate::health::proxy_serves`]),不是「端口能 connect」——
/// 后者在转发僵死时恒真,旧版据此反复回「已修复」而链路其实是死的(2026-08-04 事故)。
/// 不破命门 #1:不碰登录态;只修宿主→分流口这条转发链。
pub async fn heal_proxy(State(st): State<AppState>) -> Json<Value> {
    let Some(_repair) = st.lifecycle.try_gateway_repair() else {
        return Json(json!({"ok":false,"reachable":false,"pending":true,"error":"修复正在进行，请稍后查看连接状态"}));
    };
    if let Err(error) = crate::runtime::ensure(&st).await {
        return Json(json!({"ok":false,"reachable":false,"error":error.to_string()}));
    }
    let _activity = match st.lifecycle.runtime().activity().await {
        Ok(activity) => activity,
        Err(error) => return Json(json!({"ok":false,"reachable":false,"error":error})),
    };
    let started = std::time::Instant::now();
    crate::ev!(warn, "api", "heal_start", "开始手动修复分流链路", { "action": "manual" });
    let tunnel_err = crate::health::heal_transport(&st).await.err();
    let mut reachable = tunnel_err.is_none();
    let mut method = "tunnel";
    if !reachable {
        // 二级:转发重建救不回 → 多半是 mihomo 容器自身异常,重启后按新 IP 重挂转发。
        let restart_err = crate::health::restart_router(&st).await.err();
        if let Some(e) = restart_err {
            let pre = tunnel_err.map(|e| format!("重建转发失败:{e};")).unwrap_or_default();
            crate::events::audit_failed("heal_proxy", "手动修复分流链路失败", json!({
                "target_kind": "system", "target_name": crate::infra::MIHOMO_CONTAINER,
                "action": "manual", "before": { "reachable": false }, "after": { "reachable": false },
                "duration_ms": started.elapsed().as_millis() as u64,
                "result": "failed", "error": e.to_string()
            }));
            return Json(json!({"ok": false, "reachable": false,
                "error": format!("{pre}重启分流路由失败:{e}。建议退出并重新打开 app(将自动重建底座)")}));
        }
        method = "restart";
        reachable = crate::health::proxy_serves(&st.cfg.mihomo_host_port).await;
    }
    if reachable {
        // 入口握手已经恢复；完整健康态仍须等待看门狗下一次采样。
        if let Ok(mut snap) = st.health.lock() {
            // 这里只确认入口握手；完整网关状态仍由下一次看门狗采样核对。
            snap.gateway_checked_at_ms = None;
            snap.proxy_port_reachable = true;
            snap.healing = false;
            snap.gave_up = false;
        }
        crate::events::audit("heal_proxy", "手动修复分流链路完成", json!({
            "target_kind": "system", "target_name": crate::infra::MIHOMO_CONTAINER,
            "action": method, "before": { "reachable": false }, "after": { "reachable": true },
            "duration_ms": started.elapsed().as_millis() as u64, "result": "ok"
        }));
    } else {
        crate::events::audit_failed("heal_proxy", "手动修复动作完成但端到端探活仍失败", json!({
            "target_kind": "system", "target_name": crate::infra::MIHOMO_CONTAINER,
            "action": method, "before": { "reachable": false }, "after": { "reachable": false },
            "duration_ms": started.elapsed().as_millis() as u64,
            "result": "failed", "error": "端到端探活未通过"
        }));
    }
    Json(json!({"ok": reachable, "reachable": reachable, "method": method,
        "error": if reachable { None } else { Some("修复动作已结束，但分流链路仍未通过检测，请查看运行日志") }}))
}

pub async fn proxies(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "proxies": st.mihomo.proxies().await }))
}

pub async fn channels(State(st): State<AppState>) -> axum::response::Response {
    let db = st.cfg.db_path();
    // db 读失败 → 5xx(而非静默空表):空表会让概览误显示「没有通道」,用户以为配置丢了。
    let chans = match store::list_channels(&db) {
        Ok(c) => c,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("list_channels: {e}")),
    };

    let mut rules_map: HashMap<String, (Vec<Value>, Vec<Value>)> = HashMap::new();
    let snapshot = crate::docker::channel_snapshot(st.docker().as_ref()).await.ok();
    let mut observations = HashMap::new();
    for c in &chans {
        let rules = match store::list_rules(&db, &c.id) {
            Ok(r) => r,
            Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("list_rules {}: {e}", c.id)),
        };
        rules_map.insert(c.id.clone(), split_rules(rules));
        observations.insert(c.id.clone(), crate::docker::observation(snapshot.as_ref(), &c.id));
    }
    let mut response = build_channels_response(chans, &rules_map, &observations, &crate::lifecycle::lost_channels(&db));
    for channel in &mut response {
        let cid = channel["id"].as_str().unwrap_or_default().to_string();
        match crate::stop_intents::pending(&db, &cid) {
            Ok(value) => {
                channel["stop_pending"] = json!(value);
                if value { channel["status"] = json!("stopped"); channel["configured_status"] = json!("stopped"); channel["latency_ms"] = serde_json::Value::Null; channel["blocked"] = json!(false); }
            },
            Err(error) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string()),
        }
        match (crate::replacement_store::public_status(&db, &cid), crate::replacement_store::data_volume(&db, &cid)) {
            (Ok(pending), Ok(volume)) => {
                channel["replacement"] = json!(pending); channel["volume_name"] = json!(volume);
                if let Err(error) = crate::replacement_store::overlay_queued(&db, &cid, channel) {
                    return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &error.to_string());
                }
            },
            _ => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, "replacement runtime unavailable"),
        }
    }
    Json(json!(response)).into_response()
}

/// 纯函数:把通道 + 预取的 rules/uptime 组装成 /api/channels 输出(对照 main.py channels 路由)。
/// `blocked`:与内层入口共用同一判据（出错、掉线，custom 未验证）。
pub fn build_channels_response(
    channels: Vec<store::ChannelPublic>,
    rules_map: &HashMap<String, (Vec<Value>, Vec<Value>)>,
    observations: &HashMap<String, crate::docker::ContainerObservation>,
    lost: &HashSet<String>,
) -> Vec<Value> {
    channels
        .into_iter()
        .map(|ch| {
            let id = ch.id.clone();
            let (domains, ips) = rules_map.get(&id).cloned().unwrap_or_default();
            let observed = observations.get(&id).cloned().unwrap_or_default();
            let uptime = observed.uptime;
            let status = if observed.state == crate::docker::ContainerPresence::Absent && (ch.status == "running" || ch.status == "logged_in") {
                "down".to_string()
            } else {
                ch.status.clone()
            };
            // safe: ChannelPublic always serializes to a JSON object
            let mut v = serde_json::to_value(&ch).unwrap();
            let o = v.as_object_mut().unwrap();
            o.insert("configured_status".into(), json!(ch.status));
            o.insert("container_state".into(), json!(observed.state));
            o.insert("status".into(), json!(status));
            o.insert("domains".into(), json!(domains));
            o.insert("ips".into(), json!(ips));
            o.insert("volume_name".into(), json!(format!("vpndata-{id}")));
            o.insert("socks_proxy".into(), json!(format!("ch-{id}")));         // 命门 #7
            o.insert("socks_endpoint".into(), json!(format!("vpn-{id}:1080"))); // 命门 #7
            o.insert("uptime".into(), json!(uptime));
            o.insert("blocked".into(), json!(crate::manager::channel_blocked(&ch, lost)));
            v
        })
        .collect()
}

/// 对照 main.py:domains = kind=='domain',ips = kind=='ip'(其它 kind 两边都不进)。
pub fn split_rules(rules: Vec<store::Rule>) -> (Vec<Value>, Vec<Value>) {
    let mut domains = vec![];
    let mut ips = vec![];
    for r in rules {
        // safe: Rule always serializes to a JSON object
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
            latency_ms: None, config: json!({}), routing_enabled: true,
        }
    }

    #[test]
    fn down_override_only_when_container_is_confirmed_absent() {
        use crate::docker::{ContainerObservation, ContainerPresence};
        let chans = vec![ch("a", "running"), ch("b", "logged_in"), ch("c", "stopped"), ch("d", "creating"), ch("e", "logged_in"), ch("f", "logged_in")];
        let rules: HashMap<String, (Vec<serde_json::Value>, Vec<serde_json::Value>)> = HashMap::new();
        let mut up = HashMap::new();
        up.insert("a".into(), ContainerObservation { state: ContainerPresence::Absent, uptime: None });
        up.insert("b".into(), ContainerObservation { state: ContainerPresence::Running, uptime: Some("3分钟".into()) });
        up.insert("f".into(), ContainerObservation { state: ContainerPresence::Running, uptime: None });

        let out = build_channels_response(chans, &rules, &up, &HashSet::new());
        assert_eq!(out[0]["status"], "down");
        assert_eq!(out[1]["status"], "logged_in");
        assert_eq!(out[1]["uptime"], "3分钟");
        assert_eq!(out[2]["status"], "stopped");
        assert_eq!(out[3]["status"], "creating");  // mid-creation with no uptime does NOT flip to down
        assert_eq!(out[4]["status"], "logged_in");
        assert_eq!(out[4]["container_state"], "unknown");
        assert_eq!(out[5]["status"], "logged_in");
        assert_eq!(out[5]["container_state"], "running");
        assert_eq!(out[0]["socks_proxy"], "ch-a");
        assert_eq!(out[0]["socks_endpoint"], "vpn-a:1080");
        assert_eq!(out[0]["volume_name"], "vpndata-a");
    }

    #[test]
    fn blocked_marks_errored_and_lost_channels_only() {
        let chans = vec![ch("a", "logged_in"), ch("b", "running"), ch("c", "error"), ch("d", "stopped")];
        let up = chans.iter().map(|c| (c.id.clone(), crate::docker::ContainerObservation {
            state: crate::docker::ContainerPresence::Running, uptime: Some("1分钟".into()) })).collect();
        let lost: HashSet<String> = ["b".to_string()].into();
        let out = build_channels_response(chans, &HashMap::new(), &up, &lost);
        let blocked: Vec<bool> = out.iter().map(|c| c["blocked"].as_bool().unwrap()).collect();
        assert_eq!(blocked, vec![false, true, true, false]);
    }

    #[test]
    fn blocked_reports_custom_verification_gate() {
        let mut waiting = ch("waiting", "running");
        waiting.vpn_type = "custom".into();
        waiting.probe_url = "https://intra.example.test/health".into();
        let mut verified = waiting.clone();
        verified.id = "verified".into();
        verified.status = "logged_in".into();
        let mut no_target = verified.clone();
        no_target.id = "no-target".into();
        no_target.probe_url.clear();
        let mut stopped = no_target.clone();
        stopped.id = "stopped".into();
        stopped.status = "stopped".into();
        let chans = vec![waiting, verified, no_target, stopped];
        let up = chans.iter().map(|c| (c.id.clone(), crate::docker::ContainerObservation {
            state: crate::docker::ContainerPresence::Running, uptime: None })).collect();
        let out = build_channels_response(chans, &HashMap::new(), &up, &HashSet::new());
        assert_eq!(out.iter().map(|c| c["blocked"].as_bool().unwrap()).collect::<Vec<_>>(),
            vec![true, false, true, false]);
    }

    #[test]
    fn splits_rules_by_kind() {
        let rules = vec![
            crate::store::Rule { id: 1, channel_id: "a".into(), kind: "domain".into(), pattern: "x.com".into(), enabled: 1, note: String::new(), locked: 0 },
            crate::store::Rule { id: 2, channel_id: "a".into(), kind: "ip".into(), pattern: "10.0.0.0/8".into(), enabled: 1, note: String::new(), locked: 0 },
        ];
        let (domains, ips) = split_rules(rules);
        assert_eq!(domains.len(), 1);
        assert_eq!(ips.len(), 1);
        assert_eq!(domains[0]["pattern"], "x.com");
        assert_eq!(ips[0]["pattern"], "10.0.0.0/8");
    }
}
