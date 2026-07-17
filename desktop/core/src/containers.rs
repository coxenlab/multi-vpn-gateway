//! 容器管理:本系统依赖的全部容器一屏盘点 + 受限的简单管理(不用进 Docker)。
//! 桌面版 host-only 路由(web 版无此端点,前端 feature-detect 降级)。
//!
//! 角色与允许的操作(命门守护):
//! - infra(mihomo 分流核心):只读盘点;修复走既有 /api/system/heal-proxy 两级梯子。
//! - channel(vpn-{id} 通道容器):启停走既有 /api/channels/:cid/start|stop
//!   (hagb/oss 扛不住原地 docker restart,start=重建——此处绝不暴露裸 restart)。
//! - orphan(通道已删残留的 vpn-* / 泄漏的一次性探活容器):唯一允许 DELETE 的对象。

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::Json;
use bollard::container::ListContainersOptions;
use bollard::Docker;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::AppState;

/// 日志/删除只认本系统命名空间的容器,别的容器绝不碰。
/// (web 栈的 vpnmgr-app-1 / vpnmgr-mihomo-1 归 compose 管,不在桌面管辖内。)
pub fn is_managed_name(name: &str) -> bool {
    name == crate::infra::MIHOMO_CONTAINER
        || name.starts_with("vpn-")
        || name.starts_with("vpncore-probe-")
}

/// orphan 判定:通道已删残留的 vpn-*(id 不在通道表)或泄漏的一次性探活容器。
/// 在册通道容器与 mihomo 绝不算孤儿——它们的生命周期归 manager / infra 管。
pub fn is_orphan(name: &str, channel_ids: &[String]) -> bool {
    if let Some(id) = name.strip_prefix("vpn-") {
        return !channel_ids.iter().any(|c| c == id);
    }
    name.starts_with("vpncore-probe-")
}

/// inspect → 容器级元信息(与 diag 同口径:uptime 只在 running 给,exit_code 只在停止给)。
async fn inspect_meta(docker: &Docker, name: &str) -> Option<Value> {
    let info = docker.inspect_container(name, None).await.ok()?;
    let restart_count = info.restart_count.unwrap_or(0);
    let s = info.state.as_ref();
    let state = s
        .and_then(|x| x.status.as_ref())
        .map(|x| x.to_string())
        .unwrap_or_else(|| "unknown".into());
    let running = s.and_then(|x| x.running).unwrap_or(false);
    let uptime_secs = s
        .and_then(|x| x.started_at.clone())
        .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0))
        .filter(|_| running);
    let exit_code = s.and_then(|x| x.exit_code).filter(|_| !running);
    let image = info.config.as_ref().and_then(|c| c.image.clone());
    Some(json!({
        "state": state,
        "restart_count": restart_count,
        "uptime_secs": uptime_secs,
        "exit_code": exit_code,
        "crash_loop": crate::diag::is_crash_loop(&state, restart_count, uptime_secs),
        "image": image,
    }))
}

/// meta 并入基础字段;inspect 不到(容器不存在 / docker 不可用)→ state:"missing"。
fn merged(mut base: Value, meta: Option<Value>) -> Value {
    let o = base.as_object_mut().expect("base 恒为 object");
    match meta {
        Some(Value::Object(m)) => {
            for (k, v) in m {
                o.insert(k, v);
            }
        }
        _ => {
            o.insert("state".into(), json!("missing"));
        }
    }
    base
}

/// GET /api/containers —— 依赖容器全量盘点:infra + 每通道 + 孤儿。
/// docker 不可用时仍列出「应该有哪些」(state 全 missing),页面能解释依赖关系。
pub async fn list(State(st): State<AppState>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let chans = match crate::store::list_channels(&db) {
        Ok(c) => c,
        Err(e) => return crate::api::err_detail(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            &format!("list_channels: {e}"),
        ),
    };
    let docker = st.docker();

    // docker 侧真实存在的本系统容器名(all=true 含已停止),供孤儿发现
    let mut present: Vec<String> = Vec::new();
    if let Some(d) = docker.as_ref() {
        let opts = ListContainersOptions::<String> { all: true, ..Default::default() };
        if let Ok(cs) = d.list_containers(Some(opts)).await {
            for c in cs {
                for n in c.names.unwrap_or_default() {
                    let n = n.trim_start_matches('/').to_string();
                    if is_managed_name(&n) {
                        present.push(n);
                    }
                }
            }
        }
    }

    let mut out: Vec<Value> = Vec::new();

    // ① infra:分流核心(外层 Clash 里的 vpn-router 节点即它的分流口)
    let name = crate::infra::MIHOMO_CONTAINER;
    let meta = match docker.as_ref() {
        Some(d) => inspect_meta(d, name).await,
        None => None,
    };
    out.push(merged(
        json!({ "name": name, "role": "infra", "title": "分流核心 mihomo(vpn-router)" }),
        meta,
    ));

    // ② channel:每通道一个 vpn-{id}
    let ids: Vec<String> = chans.iter().map(|c| c.id.clone()).collect();
    for ch in &chans {
        let name = format!("vpn-{}", ch.id);
        let meta = match docker.as_ref() {
            Some(d) => inspect_meta(d, &name).await,
            None => None,
        };
        out.push(merged(
            json!({
                "name": name, "role": "channel",
                "channel_id": ch.id, "channel_name": ch.name,
                "vpn_type": ch.vpn_type, "channel_status": ch.status,
            }),
            meta,
        ));
    }

    // ③ orphan:docker 里有、通道表里没有的残留(唯一可清理对象)
    let mut seen = std::collections::BTreeSet::new();
    for name in present {
        if !seen.insert(name.clone()) || !is_orphan(&name, &ids) {
            continue;
        }
        let meta = match docker.as_ref() {
            Some(d) => inspect_meta(d, &name).await,
            None => None,
        };
        out.push(merged(json!({ "name": name, "role": "orphan" }), meta));
    }

    Json(json!({ "docker_available": docker.is_some(), "containers": out })).into_response()
}

/// GET /api/containers/:name/logs —— 原始 docker logs(本系统容器限定)。
pub async fn logs(
    State(st): State<AppState>,
    Path(name): Path<String>,
    Query(q): Query<crate::api::LogsQuery>,
) -> axum::response::Response {
    if !is_managed_name(&name) {
        return crate::api::err404("not found");
    }
    match st.docker().as_ref() {
        Some(d) => match crate::docker::raw_logs(d, &name, q.tail).await {
            Ok(lines) => Json(json!({ "lines": lines })).into_response(),
            Err(e) => crate::api::err500(&format!("{e}")),
        },
        None => crate::api::err500("docker 连接不可用"),
    }
}

/// DELETE /api/containers/:name —— 只删孤儿。在册通道容器与 mihomo 一律 409:
/// 通道容器请走通道启停/删除(状态机 + rebuild 一致),mihomo 请走「修复分流」。
pub async fn remove(State(st): State<AppState>, Path(name): Path<String>) -> axum::response::Response {
    if !is_managed_name(&name) {
        return crate::api::err404("not found");
    }
    let db = st.cfg.db_path();
    let ids: Vec<String> = crate::store::list_channels(&db)
        .unwrap_or_default()
        .into_iter()
        .map(|c| c.id)
        .collect();
    if !is_orphan(&name, &ids) {
        return (
            axum::http::StatusCode::CONFLICT,
            Json(json!({ "error": "非孤儿容器:通道容器请用通道的启动/停止/删除,分流核心请用「修复分流」" })),
        )
            .into_response();
    }
    match st.docker().as_ref() {
        Some(d) => match crate::docker::rm_force(d, &name).await {
            Ok(_) => Json(json!({ "ok": true })).into_response(),
            Err(e) => crate::api::err500(&format!("{e}")),
        },
        None => crate::api::err500("docker 连接不可用"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_namespace() {
        assert!(is_managed_name("mihomo"));
        assert!(is_managed_name("vpn-abc123"));
        assert!(is_managed_name("vpncore-probe-abc123"));
        // 别的容器绝不碰:web 栈 compose 容器、无关容器
        assert!(!is_managed_name("vpnmgr-app-1"));
        assert!(!is_managed_name("vpnmgr-mihomo-1"));
        assert!(!is_managed_name("nginx"));
    }

    #[test]
    fn orphan_rules() {
        let ids = vec!["live1".to_string()];
        assert!(!is_orphan("vpn-live1", &ids)); // 在册通道:绝不许删
        assert!(is_orphan("vpn-dead9", &ids)); // 通道已删的残留
        assert!(is_orphan("vpncore-probe-live1", &ids)); // 泄漏的一次性探活容器
        assert!(!is_orphan("mihomo", &ids)); // 分流核心:绝不许删
        assert!(!is_orphan("nginx", &ids));
    }

    #[test]
    fn merged_missing_state() {
        let v = merged(json!({ "name": "vpn-x", "role": "channel" }), None);
        assert_eq!(v["state"], "missing");
        let v = merged(json!({ "name": "mihomo" }), Some(json!({ "state": "running", "restart_count": 0 })));
        assert_eq!(v["state"], "running");
        assert_eq!(v["name"], "mihomo");
    }
}
