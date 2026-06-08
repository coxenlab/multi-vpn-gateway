//! main.py 路由的 Rust 端 handler(薄壳)。对照 app/main.py。
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::store::NewChannel;
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

// ── 通道创建/编辑(命门 #5:oss 凭据经 provision→oss_connect 注入) ──────────

fn rand_hex(n: usize) -> String {
    (0..n).map(|_| format!("{:02x}", rand::random::<u8>())).collect()
}
fn rand_mac() -> String {
    let b: [u8; 5] = rand::random();
    format!("02:{}", b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(":"))
}

fn secret_keys_of(vtype: &str) -> Vec<String> {
    registry::get(vtype)
        .map(|s| s.inputs.iter().filter(|i| i.secret).map(|i| i.key.clone()).collect())
        .unwrap_or_default()
}

fn js(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

pub(crate) fn err500(msg: &str) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": msg }))).into_response()
}
pub(crate) fn err404(msg: &str) -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg }))).into_response()
}
pub(crate) fn channel_json(db: &std::path::Path, cid: &str) -> axum::response::Response {
    match store::get_channel(db, cid) {
        Ok(Some(c)) => Json(serde_json::to_value(&c).unwrap()).into_response(),
        _ => err404("not found"),
    }
}

/// create_channel + (oss)oss_connect。对照 main.py create_channel 的整体语义
/// (Phase 4 把 oss_connect 拆到调用方,命门 #5)。
async fn provision(
    st: &AppState,
    ch: &store::ChannelPublic,
    vnc_pwd: &str,
) -> anyhow::Result<(String, Option<i64>)> {
    let docker = st.docker.as_ref().ok_or_else(|| anyhow::anyhow!("docker unavailable"))?;
    let (id, novnc) = manager::create_channel(docker, &st.cfg, ch, vnc_pwd).await?;
    let spec = registry::get(&ch.vpn_type)?;
    if spec.runtime == "oss" {
        let key = store::master_key(&st.cfg.data_dir)?;
        let config = store::get_config(&st.cfg.db_path(), &key, &ch.id)?;
        let proto = spec.protocol.clone().unwrap_or_default();
        manager::oss_connect(docker, &ch.id, &proto, &config).await?;
    }
    Ok((id, novnc))
}

pub async fn create(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let cid = rand_hex(4);
    let vnc = rand_hex(4);
    let vtype = {
        let t = js(&b, "vpn_type");
        if t.is_empty() { "easyconnect".into() } else { t }
    };
    let cfg_in: serde_json::Map<String, Value> =
        b.get("config").and_then(|v| v.as_object()).cloned().unwrap_or_default();
    let name = {
        let n = js(&b, "name");
        if n.is_empty() { cid.clone() } else { n }
    };
    let server = {
        let s = js(&b, "server");
        if s.is_empty() { cfg_in.get("server").and_then(|v| v.as_str()).unwrap_or("").into() } else { s }
    };
    let username = {
        let u = js(&b, "username");
        if u.is_empty() { cfg_in.get("username").and_then(|v| v.as_str()).unwrap_or("").into() } else { u }
    };
    let ec_ver = {
        let e = js(&b, "ec_ver");
        if e.is_empty() { "7.6.3".into() } else { e }
    };
    let login_method = {
        let l = js(&b, "login_method");
        if l.is_empty() { "interactive".into() } else { l }
    };
    let nc = NewChannel {
        id: cid.clone(),
        name,
        vpn_type: vtype.clone(),
        server,
        ec_ver,
        login_method,
        username,
        password: js(&b, "password"),
        vnc_password: vnc.clone(),
        mac: rand_mac(),
        probe_url: js(&b, "probe_url"),
        status: "creating".into(),
    };
    let sk = secret_keys_of(&vtype);
    if let Err(e) = store::add_channel(&db, &key, &nc, &cfg_in, &sk) {
        return err500(&format!("add_channel: {e}"));
    }
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        _ => return err500("get_channel after add"),
    };
    match provision(&st, &ch, &vnc).await {
        Ok((container_id, novnc)) => {
            let _ = store::set_container(&db, &cid, &container_id, novnc, "running");
            let _ = manager::rebuild(&st.cfg, &db).await;
            channel_json(&db, &cid)
        }
        Err(e) => {
            let _ = store::set_status(&db, &cid, "error");
            err500(&format!("{e}"))
        }
    }
}

pub async fn update(State(st): State<AppState>, Path(cid): Path<String>, Json(b): Json<Value>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    let fields = b.as_object().cloned().unwrap_or_default();
    let sk = secret_keys_of(&ch.vpn_type);
    if let Err(e) = store::update_channel(&db, &key, &cid, &fields, &sk) {
        return err500(&format!("update_channel: {e}"));
    }
    let touched = ["server", "username", "password", "ec_ver"].iter().any(|k| fields.contains_key(*k));
    if touched && ch.container_id.is_some() {
        let ch2 = match store::get_channel(&db, &cid) {
            Ok(Some(c)) => c,
            _ => return err500("get_channel"),
        };
        let vnc = ch2.vnc_password.clone().unwrap_or_default();
        match provision(&st, &ch2, &vnc).await {
            Ok((container_id, novnc)) => {
                let _ = store::set_container(&db, &cid, &container_id, novnc, "running");
                let _ = manager::rebuild(&st.cfg, &db).await;
            }
            Err(e) => {
                let _ = store::set_status(&db, &cid, "error");
                return err500(&format!("{e}"));
            }
        }
    }
    channel_json(&db, &cid)
}
