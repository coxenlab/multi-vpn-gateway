//! main.py 路由的 Rust 端 handler(薄壳)。对照 app/main.py。
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::store::NewChannel;
use crate::{registry, store, manager, webutil, dockerhub, preflight, AppState};

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
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
        .filter(|v| !v.is_empty()) // 对照 main.py:空 [] 是 falsy → 回退到单个 pattern
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
    let code = manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await;
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
    Json(json!({ "ok": true, "reload_status": manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await }))
}

pub async fn patch_rule(
    State(st): State<AppState>,
    Path((_cid, rid)): Path<(String, i64)>,
    Json(b): Json<Value>,
) -> Json<Value> {
    let db = st.cfg.db_path();
    let _ = store::set_rule_enabled(&db, rid, b.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false));
    Json(json!({ "ok": true, "reload_status": manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await }))
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
            let _ = manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await;
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
                let _ = manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await;
            }
            Err(e) => {
                let _ = store::set_status(&db, &cid, "error");
                return err500(&format!("{e}"));
            }
        }
    }
    channel_json(&db, &cid)
}

// ── login / upload / status(命门 #1 探活、#5 上传安装器) ────────────────────

pub async fn login(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    if ch.login_method == "headless" {
        return Json(json!({ "login_mode": "headless" })).into_response();
    }
    let docker = match st.docker.as_ref() {
        Some(d) => d,
        None => return err500("docker unavailable"),
    };
    let port = match manager::novnc_port(docker, &cid).await {
        Some(p) => p,
        None => return err404("no novnc port"),
    };
    if Some(port) != ch.novnc_port {
        let _ = store::set_novnc_port(&db, &cid, port);
    }
    manager::ensure_novnc_bridge(docker, &cid).await;
    Json(json!({ "url": webutil::login_url(port, &ch.vnc_password.unwrap_or_default()) })).into_response()
}

pub async fn upload(State(st): State<AppState>, Path(cid): Path<String>, mut mp: Multipart) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    if matches!(store::get_channel(&db, &cid), Ok(None)) {
        return err404("not found");
    }
    // 取第一个文件字段(对照 UploadFile = File(...) 的单文件语义)
    let (filename, blob) = match mp.next_field().await {
        Ok(Some(field)) => {
            let fname = field.file_name().map(String::from).unwrap_or_default();
            match field.bytes().await {
                Ok(b) => (fname, b),
                Err(e) => return err500(&format!("read upload: {e}")),
            }
        }
        Ok(None) => return err500("no file field"),
        Err(e) => return err500(&format!("multipart: {e}")),
    };
    let docker = match st.docker.as_ref() {
        Some(d) => d,
        None => return err500("docker unavailable"),
    };
    // 命门 #5:二进制经 put_archive 落数据卷,绝不入 SQLite/回传
    if let Err(e) = crate::docker::put_file(docker, &format!("vpn-{cid}"), "/root", &filename, blob.as_ref()).await {
        return err500(&format!("{e}"));
    }
    let _ = store::set_config_field(&db, &key, &cid, "package", &filename, false);
    Json(json!({ "ok": true, "package": filename })).into_response()
}

pub async fn status(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    let (ok, ms) = manager::probe(&ch).await; // 命门 #1
    let new = if ok {
        "logged_in"
    } else if ch.status == "logged_in" {
        "running"
    } else {
        ch.status.as_str()
    }
    .to_string();
    let _ = store::set_status(&db, &cid, &new);
    if let Some(m) = ms {
        let _ = store::set_latency(&db, &cid, m);
    }
    Json(json!({ "status": new, "connected": ok, "latency_ms": ms })).into_response()
}

// ── start / stop / delete(byo 原地 start;hagb/oss 走重建) ──────────────────

pub async fn start(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    let runtime = registry::get(&ch.vpn_type).map(|s| s.runtime).unwrap_or_default();
    if runtime == "byo" {
        // byo 客户端装在可写层,扛得住原地重启 → 不重建
        if let Some(d) = st.docker.as_ref() {
            let _ = manager::start(d, &cid).await;
        }
        let _ = store::set_status(&db, &cid, "running");
        return Json(json!({ "ok": true })).into_response();
    }
    // hagb/oss:原地 start 扛不住 → 重建
    let vnc = ch.vnc_password.clone().unwrap_or_default();
    match provision(&st, &ch, &vnc).await {
        Ok((container_id, novnc)) => {
            let _ = store::set_container(&db, &cid, &container_id, novnc, "running");
            let _ = manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await;
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => {
            let _ = store::set_status(&db, &cid, "error");
            err500(&format!("{e}"))
        }
    }
}

pub async fn stop(State(st): State<AppState>, Path(cid): Path<String>) -> Json<Value> {
    let db = st.cfg.db_path();
    if let Some(d) = st.docker.as_ref() {
        let _ = manager::stop(d, &cid).await;
    }
    let _ = store::set_status(&db, &cid, "stopped");
    Json(json!({ "ok": true }))
}

pub async fn delete(State(st): State<AppState>, Path(cid): Path<String>) -> Json<Value> {
    let db = st.cfg.db_path();
    if let Some(d) = st.docker.as_ref() {
        let _ = manager::remove(d, &cid).await;
    }
    let _ = store::del_channel(&db, &cid);
    let _ = manager::rebuild(&st.cfg, st.docker.as_ref(), &db).await;
    Json(json!({ "ok": true }))
}

// ── Clash 接入 / 入口接入(命门 #2:IP 带 no-resolve、域名经 bare) ────────────

fn text_plain(body: String) -> axum::response::Response {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

pub async fn clash_provider(State(st): State<AppState>) -> axum::response::Response {
    let rules = store::all_rules(&st.cfg.db_path()).unwrap_or_default();
    text_plain(webutil::clash_provider_text(&rules))
}

pub async fn clash_snippet(State(st): State<AppState>) -> axum::response::Response {
    let rules = store::all_rules(&st.cfg.db_path()).unwrap_or_default();
    let ui = st.cfg.ui_port.to_string();
    text_plain(webutil::clash_snippet_text(&rules, &st.cfg.mihomo_host_port, &ui))
}

pub async fn entry_pac(State(st): State<AppState>) -> axum::response::Response {
    let rules = store::all_rules(&st.cfg.db_path()).unwrap_or_default();
    let pac = webutil::pac_text(&rules, &st.cfg.mihomo_host_port);
    ([(axum::http::header::CONTENT_TYPE, "application/x-ns-proxy-autoconfig")], pac).into_response()
}

pub async fn entry_setup_commands(State(st): State<AppState>) -> Json<Value> {
    let ui = st.cfg.ui_port.to_string();
    Json(webutil::setup_commands(&st.cfg.mihomo_host_port, &ui))
}

// ── Phase 6:versions / preflight / images / mirrors ──────────────────────────

#[derive(Deserialize)]
pub struct PreflightQuery {
    pub vpn_type: Option<String>,
    pub version: Option<String>,
    #[serde(default = "default_scope")]
    pub scope: String,
}
fn default_scope() -> String {
    "preflight".into()
}

pub async fn vpn_versions(Path(vtype): Path<String>) -> axum::response::Response {
    let spec = match registry::get(&vtype) {
        Ok(s) => s,
        Err(_) => return err404("unknown type"),
    };
    if !spec.versioned {
        return Json(json!({ "versions": [] })).into_response();
    }
    let repo = spec.version_repo.clone().unwrap_or_default();
    let arch = registry::host_arch();
    let vs = dockerhub::versions(&repo, &arch, &spec.fallback_versions).await;
    Json(json!({ "versions": vs })).into_response()
}

fn enabled_mirror_hosts(st: &AppState) -> Vec<String> {
    store::list_mirrors(&st.cfg.db_path())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.enabled != 0)
        .map(|m| m.host)
        .collect()
}

pub async fn preflight_check(State(st): State<AppState>, Query(q): Query<PreflightQuery>) -> Json<Value> {
    let mirrors = enabled_mirror_hosts(&st);
    let mihomo_alive = if q.scope == "full" { Some(st.mihomo.alive().await) } else { None };
    let arch = registry::host_arch();
    let out = preflight::run_checks(
        st.docker.as_ref(),
        q.vpn_type.as_deref(),
        q.version.as_deref(),
        &arch,
        &st.cfg.vpn_net,
        &q.scope,
        &mirrors,
        mihomo_alive,
    )
    .await;
    Json(out)
}

pub async fn preflight_fix(State(st): State<AppState>, Path(action): Path<String>, Json(b): Json<Value>) -> axum::response::Response {
    match action.as_str() {
        "create_network" => {
            let name = b
                .get("name")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&st.cfg.vpn_net)
                .to_string();
            match st.docker.as_ref() {
                Some(d) => match crate::docker::create_bridge_network(d, &name).await {
                    Ok(_) => Json(json!({ "ok": true })).into_response(),
                    Err(e) => err500(&format!("{e}")),
                },
                None => err500("docker unavailable"),
            }
        }
        "pull_image" => {
            let image = b.get("image").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let repo = image.split(':').next().unwrap_or("").to_string();
            if !preflight::known_repos().contains(&repo) || preflight::is_buildable(&image) {
                return (StatusCode::BAD_REQUEST, Json(json!({ "error": "image not pullable" }))).into_response();
            }
            let docker = match st.docker.as_ref() {
                Some(d) => d.clone(),
                None => return err500("docker unavailable"),
            };
            let mirrors = enabled_mirror_hosts(&st);
            let tid = preflight::start_pull(docker, &image, &registry::host_arch(), mirrors);
            Json(json!({ "task_id": tid })).into_response()
        }
        _ => (StatusCode::BAD_REQUEST, Json(json!({ "error": "unknown action" }))).into_response(),
    }
}

pub async fn preflight_fix_status(Path(task_id): Path<String>) -> axum::response::Response {
    match preflight::get_task(&task_id) {
        Some(st) => Json(st).into_response(),
        None => err404("unknown task"),
    }
}

pub async fn images_inventory(State(st): State<AppState>) -> Json<Value> {
    let mirrors = enabled_mirror_hosts(&st);
    let arch = registry::host_arch();
    Json(preflight::image_inventory(st.docker.as_ref(), &arch, &mirrors).await)
}

pub async fn mirrors_list(State(st): State<AppState>) -> Json<Value> {
    Json(json!(store::list_mirrors(&st.cfg.db_path()).unwrap_or_default()))
}

pub async fn mirrors_add(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let host = b.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if host.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "host required" }))).into_response();
    }
    let db = st.cfg.db_path();
    match store::add_mirror(&db, &host) {
        Ok(mid) => match store::list_mirrors(&db).unwrap_or_default().into_iter().find(|m| m.id == mid) {
            Some(m) => Json(serde_json::to_value(m).unwrap()).into_response(),
            None => err500("mirror added but not found"),
        },
        Err(_) => (StatusCode::BAD_REQUEST, Json(json!({ "error": "mirror already exists" }))).into_response(),
    }
}

pub async fn mirrors_patch(State(st): State<AppState>, Path(mid): Path<i64>, Json(b): Json<Value>) -> Json<Value> {
    let priority = b.get("priority").and_then(|v| v.as_i64());
    let enabled = b.get("enabled").and_then(|v| v.as_bool());
    let _ = store::set_mirror(&st.cfg.db_path(), mid, priority, enabled);
    Json(json!({ "ok": true }))
}

pub async fn mirrors_del(State(st): State<AppState>, Path(mid): Path<i64>) -> Json<Value> {
    let _ = store::del_mirror(&st.cfg.db_path(), mid);
    Json(json!({ "ok": true }))
}

pub async fn mirrors_test(Json(b): Json<Value>) -> Json<Value> {
    let host = b.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let t0 = std::time::Instant::now();
    // 对照 Python mirrors_test:任何 HTTP 响应(不抛)即可达,不看状态码
    // (区别于 preflight 的 mirror_reachable 用 <500——那个对照 _mirror_reachable)
    let ok = reqwest::Client::new()
        .get(format!("https://{host}/v2/"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .is_ok();
    let ms = if ok { Some(t0.elapsed().as_millis() as i64) } else { None };
    Json(json!({ "reachable": ok, "latency_ms": ms }))
}
