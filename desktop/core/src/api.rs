//! main.py 路由的 Rust 端 handler(薄壳)。对照 app/main.py。
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{registry, store, manager, webutil, AppState};

pub async fn vpn_types() -> Json<Value> {
    Json(json!(registry::list_adapters().unwrap_or_default()))
}

pub async fn connections(State(st): State<AppState>) -> Json<Value> {
    Json(st.mihomo.connections().await)
}

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_tail")]
    pub tail: i64,
}
fn default_tail() -> i64 {
    200
}

pub async fn logs(
    State(st): State<AppState>,
    Path(cid): Path<String>,
    Query(q): Query<LogsQuery>,
) -> Json<Value> {
    let lines = match st.docker.as_ref() {
        Some(d) => manager::logs(d, &cid, q.tail).await,
        None => vec!["<no logs: docker unavailable>".to_string()],
    };
    Json(json!({ "lines": lines }))
}

// ── 规则路由(命门 #3:增删改后 rebuild 热加载) ──────────────────────────────

pub async fn add_rules(
    State(st): State<AppState>,
    Path(cid): Path<String>,
    Json(b): Json<Value>,
) -> Json<Value> {
    let db = st.cfg.db_path();
    let patterns: Vec<String> = b
        .get("patterns")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .or_else(|| b.get("pattern").and_then(|v| v.as_str()).map(|s| vec![s.to_string()]))
        .unwrap_or_default();
    let forced = b.get("kind").and_then(|v| v.as_str());
    let existing: Vec<(String, String)> = store::list_rules(&db, &cid)
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.kind, r.pattern))
        .collect();
    let plan = webutil::plan_rules(&patterns, forced, &existing);
    for (kind, pat) in &plan.to_add {
        let _ = store::add_rule(&db, &cid, kind, pat);
    }
    let code = manager::rebuild(&st.cfg, &db).await;
    let rs = store::list_rules(&db, &cid).unwrap_or_default();
    let (domains, ips) = crate::routes::split_rules(rs);
    Json(json!({
        "reload_status": code,
        "domains": domains,
        "ips": ips,
        "added": plan.added,
        "rejected": plan.rejected,
    }))
}

pub async fn del_rule(State(st): State<AppState>, Path((_cid, rid)): Path<(String, i64)>) -> Json<Value> {
    let db = st.cfg.db_path();
    let _ = store::del_rule(&db, rid);
    Json(json!({ "ok": true, "reload_status": manager::rebuild(&st.cfg, &db).await }))
}

pub async fn patch_rule(
    State(st): State<AppState>,
    Path((_cid, rid)): Path<(String, i64)>,
    Json(b): Json<Value>,
) -> Json<Value> {
    let db = st.cfg.db_path();
    let _ = store::set_rule_enabled(&db, rid, b.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false));
    Json(json!({ "ok": true, "reload_status": manager::rebuild(&st.cfg, &db).await }))
}
