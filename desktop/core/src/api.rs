//! main.py 路由的 Rust 端 handler(薄壳)。对照 app/main.py。
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::{registry, manager, AppState};

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
