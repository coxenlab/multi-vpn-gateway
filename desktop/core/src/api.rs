//! main.py 路由的 Rust 端 handler(薄壳)。对照 app/main.py。
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use crate::store::NewChannel;
use crate::{registry, store, manager, webutil, entry, dockerhub, preflight, AppState};

static ROUTING_MUTATION_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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
    let lines = match st.docker().as_ref() {
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
) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch_name = match store::get_channel(&db, &cid) {
        Ok(Some(ch)) => ch.name,
        Ok(None) => return err404("channel not found"),
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("get_channel: {e}")),
    };
    let docker = match st.docker() {
        Some(d) => d,
        None => return err503("docker unavailable"),
    };
    let patterns: Vec<String> = b
        .get("patterns")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>())
        .filter(|v| !v.is_empty()) // 对照 main.py:空 [] 是 falsy → 回退到单个 pattern
        .or_else(|| b.get("pattern").and_then(|v| v.as_str()).map(|s| vec![s.to_string()]))
        .unwrap_or_default();
    let forced = b.get("kind").and_then(|v| v.as_str());
    let before_rules = match store::list_rules(&db, &cid) {
        Ok(rules) => rules,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("list_rules: {e}")),
    };
    let before_ids: std::collections::BTreeSet<i64> = before_rules.iter().map(|r| r.id).collect();
    let existing: Vec<(String, String)> =
        before_rules.iter().map(|r| (r.kind.clone(), r.pattern.clone())).collect();
    let audit_base = || {
        json!({
            "target_kind": "rules", "target_id": cid.as_str(), "target_name": ch_name.as_str(),
            "patterns": patterns.clone(), "forced_kind": forced,
        })
    };
    let plan = webutil::plan_rules(&patterns, forced, &existing);
    for (kind, pat) in &plan.to_add {
        if let Err(e) = store::add_rule(&db, &cid, kind, pat) {
            let mut detail = audit_base();
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            detail["failed_pattern"] = json!(pat.as_str());
            crate::events::audit_failed("rules_add", "绑定分流规则失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("add_rule: {e}"));
        }
    }
    let code = manager::rebuild(&st.cfg, Some(&docker), &db).await;
    if !reload_ok(&code) {
        let mut detail = audit_base();
        detail["result"] = json!("failed");
        detail["reload_status"] = json!(code.as_str());
        detail["error"] = json!(format!("mihomo reload failed: {code}"));
        detail["after"] = json!(store::list_rules(&db, &cid).map(|r| rule_snapshots(&r)).unwrap_or(Value::Null));
        crate::events::audit_failed("rules_add", "规则已入库但 mihomo 重载未达成", detail);
        return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {code}"));
    }
    let rs = match store::list_rules(&db, &cid) {
        Ok(rules) => rules,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("list_rules: {e}")),
    };
    {
        // before 是改前该通道的全部规则,after 只列本次真正新增的几条(回滚时删它们即可)。
        let added: Vec<store::Rule> =
            rs.iter().filter(|r| !before_ids.contains(&r.id)).cloned().collect();
        let mut detail = audit_base();
        detail["result"] = json!("ok");
        detail["reload_status"] = json!(code.as_str());
        detail["before"] = rule_snapshots(&before_rules);
        detail["after"] = rule_snapshots(&added);
        detail["added"] = json!(plan.added);
        detail["rejected"] = json!(plan.rejected);
        crate::events::audit("rules_add", "分流规则已绑定", detail);
    }
    let (domains, ips) = crate::routes::split_rules(rs);
    Json(json!({
        "reload_status": code,
        "domains": domains,
        "ips": ips,
        "added": plan.added,
        "rejected": plan.rejected,
    })).into_response()
}

pub async fn del_rule(State(st): State<AppState>, Path((cid, rid)): Path<(String, i64)>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch_name = match store::get_channel(&db, &cid) {
        Ok(Some(ch)) => ch.name,
        Ok(None) => return err404("channel not found"),
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("get_channel: {e}")),
    };
    let docker = match st.docker() {
        Some(d) => d,
        None => return err503("docker unavailable"),
    };
    // 改前快照必须在删之前取——删完就再也读不回「删掉的是哪条」了。
    let before = match store::get_rule(&db, rid) {
        Ok(Some(rule)) => rule_snapshot(&rule),
        _ => Value::Null,
    };
    let audit_base = || {
        json!({
            "target_kind": "rule", "target_id": rid, "channel_id": cid.as_str(),
            "target_name": ch_name.as_str(), "before": before.clone(), "after": null,
        })
    };
    match store::del_rule(&db, &cid, rid) {
        Ok(true) => {}
        Ok(false) => return err404("rule not found"),
        Err(e) => {
            let mut detail = audit_base();
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("rule_delete", "删除分流规则失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("del_rule: {e}"));
        }
    }
    let code = manager::rebuild(&st.cfg, Some(&docker), &db).await;
    let mut detail = audit_base();
    detail["reload_status"] = json!(code.as_str());
    if !reload_ok(&code) {
        detail["result"] = json!("failed");
        detail["error"] = json!(format!("mihomo reload failed: {code}"));
        crate::events::audit_failed("rule_delete", "规则已删除但 mihomo 重载未达成", detail);
        return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {code}"));
    }
    detail["result"] = json!("ok");
    crate::events::audit("rule_delete", "分流规则已删除", detail);
    Json(json!({ "ok": true, "reload_status": code })).into_response()
}

pub async fn patch_rule(
    State(st): State<AppState>,
    Path((cid, rid)): Path<(String, i64)>,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch_name = match store::get_channel(&db, &cid) {
        Ok(Some(ch)) => ch.name,
        Ok(None) => return err404("channel not found"),
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("get_channel: {e}")),
    };
    let enabled = match b.get("enabled") {
        None => None,
        Some(Value::Bool(v)) => Some(*v),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "enabled must be boolean"),
    };
    let note = match b.get("note") {
        None => None,
        Some(Value::String(v)) => Some(v.as_str()),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "note must be string"),
    };
    let locked = match b.get("locked") {
        None => None,
        Some(Value::Bool(v)) => Some(*v),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "locked must be boolean"),
    };
    if enabled.is_none() && note.is_none() && locked.is_none() {
        return err_detail(StatusCode::BAD_REQUEST, "one of enabled, note or locked is required");
    }
    let docker = if enabled.is_some() {
        match st.docker() {
            Some(d) => Some(d),
            None => return err503("docker unavailable"),
        }
    } else {
        None
    };
    let before = match store::get_rule(&db, rid) {
        Ok(Some(rule)) => rule_snapshot(&rule),
        _ => Value::Null,
    };
    let audit_base = |after: Value| {
        json!({
            "target_kind": "rule", "target_id": rid, "channel_id": cid.as_str(),
            "target_name": ch_name.as_str(), "before": before.clone(), "after": after,
            "fields": { "enabled": enabled, "note_changed": note.is_some(), "locked": locked },
        })
    };
    match store::update_rule(&db, &cid, rid, enabled, note, locked) {
        Ok(true) => {}
        Ok(false) => return err404("rule not found"),
        Err(e) => {
            let mut detail = audit_base(Value::Null);
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("rule_update", "编辑分流规则失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("update_rule: {e}"));
        }
    }
    let after = match store::get_rule(&db, rid) {
        Ok(Some(rule)) => rule_snapshot(&rule),
        _ => Value::Null,
    };
    let mut response = json!({ "ok": true });
    if let Some(docker) = docker.as_ref() {
        let code = manager::rebuild(&st.cfg, Some(docker), &db).await;
        if !reload_ok(&code) {
            let mut detail = audit_base(after);
            detail["result"] = json!("failed");
            detail["reload_status"] = json!(code.as_str());
            detail["error"] = json!(format!("mihomo reload failed: {code}"));
            crate::events::audit_failed("rule_update", "规则已改但 mihomo 重载未达成", detail);
            return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {code}"));
        }
        response["reload_status"] = json!(code);
    }
    let mut detail = audit_base(after);
    detail["result"] = json!("ok");
    crate::events::audit("rule_update", "分流规则已编辑", detail);
    if let Ok(Some(rule)) = store::get_rule(&db, rid) {
        response["rule"] = json!(rule);
    }
    Json(response).into_response()
}

pub async fn patch_rules(
    State(st): State<AppState>,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let enabled = match b.get("enabled").and_then(|v| v.as_bool()) {
        Some(v) => v,
        None => return err_detail(StatusCode::BAD_REQUEST, "enabled must be boolean"),
    };
    let raw_ids = match b.get("ids").and_then(|v| v.as_array()) {
        Some(v) if !v.is_empty() => v,
        _ => return err_detail(StatusCode::BAD_REQUEST, "ids must be a non-empty array"),
    };
    let mut ids = std::collections::BTreeSet::new();
    for value in raw_ids {
        match value.as_i64() {
            Some(id) if id > 0 => { ids.insert(id); }
            _ => return err_detail(StatusCode::BAD_REQUEST, "rule ids must be positive integers"),
        }
    }
    let ids: Vec<i64> = ids.into_iter().collect();
    let docker = match st.docker() {
        Some(d) => d,
        None => return err503("docker unavailable"),
    };
    let db = st.cfg.db_path();
    let before = store::rules_by_ids(&db, &ids).unwrap_or_default();
    let audit_base = |after: Value| {
        json!({
            "target_kind": "rules", "target_id": ids.clone(), "requested_enabled": enabled,
            "before": rule_snapshots(&before), "after": after,
        })
    };
    let result = match store::set_rules_enabled(&db, &ids, enabled) {
        Ok(Some(result)) => result,
        Ok(None) => return err404("one or more rules not found"),
        Err(e) => {
            let mut detail = audit_base(Value::Null);
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("rules_batch_update", "批量启停规则失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("set_rules_enabled: {e}"));
        }
    };
    let after = rule_snapshots(&store::rules_by_ids(&db, &ids).unwrap_or_default());
    let code = manager::rebuild(&st.cfg, Some(&docker), &db).await;
    let mut detail = audit_base(after);
    detail["updated"] = json!(result.updated);
    detail["skipped_locked"] = json!(result.skipped_locked);
    detail["reload_status"] = json!(code.as_str());
    if !reload_ok(&code) {
        detail["result"] = json!("failed");
        detail["error"] = json!(format!("mihomo reload failed: {code}"));
        crate::events::audit_failed("rules_batch_update", "规则已批量改但 mihomo 重载未达成", detail);
        return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {code}"));
    }
    detail["result"] = json!("ok");
    crate::events::audit("rules_batch_update", "分流规则已批量启停", detail);
    Json(json!({
        "ok": true,
        "updated": result.updated,
        "skipped_locked": result.skipped_locked,
        "reload_status": code,
    })).into_response()
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

// ── 审计快照(命门 #5:密码 / vnc_password / secret 字段 / 备注正文绝不进日志) ──

/// 通道字段快照,给审计的 before/after 用。`config` 直接取 `ChannelPublic.config`
/// ——store 已按 `_secret` 剥过密文字段(与 /api/channels 回前端的是同一份),
/// 密码与 vnc_password 本就不在 ChannelPublic 的可序列化面上。
fn channel_snapshot(ch: &store::ChannelPublic) -> Value {
    json!({
        "name": ch.name,
        "vpn_type": ch.vpn_type,
        "server": ch.server,
        "ec_ver": ch.ec_ver,
        "login_method": ch.login_method,
        "username": ch.username,
        "probe_url": ch.probe_url,
        "status": ch.status,
        "routing_enabled": ch.routing_enabled,
        "config": ch.config,
    })
}

/// 读回通道快照;读不到(已删 / db 错)回 null——审计里 null 即「此刻没有这个对象」。
fn channel_snapshot_of(db: &std::path::Path, cid: &str) -> Value {
    match store::get_channel(db, cid) {
        Ok(Some(ch)) => channel_snapshot(&ch),
        _ => Value::Null,
    }
}

fn rule_snapshot(r: &store::Rule) -> Value {
    json!({
        "id": r.id,
        "channel_id": r.channel_id,
        "kind": r.kind,
        "pattern": r.pattern,
        "enabled": r.enabled != 0,
        "note": r.note,
        "locked": r.locked != 0,
    })
}

fn rule_snapshots(rules: &[store::Rule]) -> Value {
    Value::Array(rules.iter().map(rule_snapshot).collect())
}

fn mirror_snapshot(m: &store::Mirror) -> Value {
    json!({ "id": m.id, "host": m.host, "priority": m.priority, "enabled": m.enabled != 0 })
}

/// TUN 入口状态快照:只留判断「改前改后/要不要回滚」需要的几个字段,
/// 不把整份 helper 明细灌进日志。
fn tun_snapshot(status: &Value) -> Value {
    json!({
        "installed": status.get("installed").cloned().unwrap_or(Value::Null),
        "enabled": status.get("enabled").cloned().unwrap_or(Value::Null),
        "config_current": status.get("config_current").cloned().unwrap_or(Value::Null),
    })
}

pub(crate) fn err500(msg: &str) -> axum::response::Response {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": msg }))).into_response()
}
pub(crate) fn err404(msg: &str) -> axum::response::Response {
    (StatusCode::NOT_FOUND, Json(json!({ "error": msg }))).into_response()
}
pub(crate) fn err503(msg: &str) -> axum::response::Response {
    (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "error": msg }))).into_response()
}
/// FastAPI 风格错误体 `{"detail": msg}`(未捕获异常时 FastAPI 即此形状)——db 读失败 / docker
/// 操作失败等新增的错误传播路径用它,与 Python 侧同类 5xx 对齐(前端 friendlyError 兼收 error/detail)。
pub(crate) fn err_detail(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(json!({ "detail": msg }))).into_response()
}
/// rebuild() 回的 reload_status:成功只能是完整的 2xx HTTP 状态码串,失败是错误串。
/// create/update/start/delete 仍只记录失败；规则变更端点据此返回非 2xx。
fn reload_ok(status: &str) -> bool {
    status
        .parse::<u16>()
        .ok()
        .map(|c| (200..300).contains(&c))
        .unwrap_or(false)
}
pub(crate) fn channel_json(db: &std::path::Path, cid: &str) -> axum::response::Response {
    match store::get_channel(db, cid) {
        Ok(Some(c)) => {
            let mut value = serde_json::to_value(&c).unwrap();
            match crate::replacement_store::public_status(db, cid) {
                Ok(pending) => value["replacement"] = json!(pending),
                Err(e) => return err500(&format!("replacement status: {e}")),
            }
            Json(value).into_response()
        }
        _ => err404("not found"),
    }
}

pub async fn restore_channel(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    detached("restore", async move {
        let _operation = match st.lifecycle.mutate(&cid).await { Ok(guard) => guard, Err(e) => return err503(&e.to_string()) };
        let db = st.cfg.db_path();
        match store::get_channel(&db, &cid) {
            Ok(Some(_)) => {},
            Ok(None) => return err404("not found"),
            Err(e) => return err500(&e.to_string()),
        }
        match crate::replacement_store::public_status(&db, &cid) {
            Ok(Some(pending)) if pending["can_restore"] == true => {},
            Ok(_) => return err_detail(StatusCode::CONFLICT, "没有可恢复的上一次设置"),
            Err(e) => return err500(&e.to_string()),
        }
        let before = channel_snapshot_of(&db, &cid);
        if let Err(e) = crate::replacement::recover(&st, &cid).await { return err500(&format!("恢复未完成: {e}")); }
        let reload = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
        crate::events::audit("channel_restore", "已恢复上一次设置，连通状态待检测", json!({"target_kind":"channel","target_id":cid,"before":before,"after":channel_snapshot_of(&db,&cid),"reload_status":reload,"result":"ok"}));
        channel_json(&db, &cid)
    }).await
}

/// create_channel + (oss)oss_connect。对照 main.py create_channel 的整体语义
/// (Phase 4 把 oss_connect 拆到调用方,命门 #5)。
async fn provision(
    st: &AppState,
    ch: &store::ChannelPublic,
    vnc_pwd: &str,
) -> anyhow::Result<(String, Option<i64>)> {
    let docker = st.docker().ok_or_else(|| anyhow::anyhow!("docker unavailable"))?;
    provision_with_docker(st, &docker, ch, vnc_pwd).await
}

async fn provision_with_docker(
    st: &AppState,
    docker: &bollard::Docker,
    ch: &store::ChannelPublic,
    vnc_pwd: &str,
) -> anyhow::Result<(String, Option<i64>)> {
    let (id, novnc) = manager::create_channel(st, docker, ch, vnc_pwd).await?;
    let spec = registry::get(&ch.vpn_type)?;
    if spec.runtime == "oss" {
        let key = store::master_key(&st.cfg.data_dir)?;
        let config = store::get_config(&st.cfg.db_path(), &key, &ch.id)?;
        let proto = spec.protocol.clone().unwrap_or_default();
        manager::oss_connect(docker, &ch.id, &proto, &config).await?;
    }
    Ok((id, novnc))
}

/// 改状态的处理函数(create/start/stop/delete)统一脱离 HTTP 请求生命周期:前端跳转 / 刷新会
/// 取消请求,axum 随之丢弃处理中的 future,留下「容器已动、库未写、无审计」的半完成状态
/// (2026-09-10 客户C实例)。spawn 后 await:客户端断开也不取消。
async fn detached(
    name: &str,
    fut: impl std::future::Future<Output = axum::response::Response> + Send + 'static,
) -> axum::response::Response {
    match tokio::spawn(fut).await {
        Ok(r) => r,
        Err(e) => err500(&format!("{name} task: {e}")),
    }
}

pub async fn create(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    detached("create", create_inner(st, b)).await
}

async fn create_inner(st: AppState, b: Value) -> axum::response::Response {
    let started = std::time::Instant::now();
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let cid = rand_hex(4);
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
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
        routing_enabled: true,
    };
    let sk = secret_keys_of(&vtype);
    if let Err(e) = store::add_channel(&db, &key, &nc, &cfg_in, &sk) {
        crate::events::audit_failed("channel_create", "通道创建失败", json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": nc.name.as_str(),
            "vpn_type": vtype.as_str(), "before": null, "after": null,
            "duration_ms": started.elapsed().as_millis() as u64, "result": "failed", "error": e.to_string()
        }));
        return err500(&format!("add_channel: {e}"));
    }
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => {
            crate::events::audit_failed("channel_create", "通道创建后未能读回", json!({
                "target_kind": "channel", "target_id": cid.as_str(), "target_name": nc.name.as_str(),
                "vpn_type": vtype.as_str(), "before": null, "after": null,
                "duration_ms": started.elapsed().as_millis() as u64, "result": "failed",
                "error": "missing after insert"
            }));
            return err500("get_channel after add");
        }
        Err(e) => {
            crate::events::audit_failed("channel_create", "通道创建后读回失败", json!({
                "target_kind": "channel", "target_id": cid.as_str(), "target_name": nc.name.as_str(),
                "vpn_type": vtype.as_str(), "before": null, "after": null,
                "duration_ms": started.elapsed().as_millis() as u64, "result": "failed",
                "error": e.to_string()
            }));
            return err500(&format!("get_channel after add: {e}"));
        }
    };
    match provision(&st, &ch, &vnc).await {
        Ok((container_id, novnc)) => {
            let state_persisted = match store::set_container(&db, &cid, &container_id, novnc, "running") {
                Ok(()) => true,
                Err(e) => {
                    crate::events::audit_failed("channel_create", "通道容器已创建但状态落库失败", json!({
                        "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
                        "vpn_type": ch.vpn_type.as_str(), "before": null, "after": channel_snapshot_of(&db, &cid),
                        "container_id": container_id.as_str(),
                        "duration_ms": started.elapsed().as_millis() as u64, "result": "failed", "error": e.to_string()
                    }));
                    false
                }
            };
            // 对照 Python create:响应回通道(不含 reload_status);重载未达成仅记日志,不阻断建通道。
            let reload = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
            if !reload_ok(&reload) {
                crate::ev!(error, "api", "mihomo_reload_failed", "创建通道后 mihomo 重载未达成",
                    { "operation": "create", "cid": cid.as_str(), "error": reload.as_str() });
            }
            if state_persisted {
                crate::events::audit("channel_create", "通道创建完成", json!({
                    "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
                    "vpn_type": ch.vpn_type.as_str(), "before": null, "after": channel_snapshot_of(&db, &cid),
                    "duration_ms": started.elapsed().as_millis() as u64, "result": "ok"
                }));
            }
            channel_json(&db, &cid)
        }
        Err(e) => {
            let _ = store::set_status(&db, &cid, "error");
            crate::events::audit_failed("channel_create", "通道创建失败", json!({
                "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
                "vpn_type": ch.vpn_type.as_str(), "before": null, "after": channel_snapshot_of(&db, &cid),
                "duration_ms": started.elapsed().as_millis() as u64, "result": "failed", "error": e.to_string()
            }));
            err500(&format!("{e}"))
        }
    }
}

pub async fn update(State(st): State<AppState>, Path(cid): Path<String>, Json(b): Json<Value>) -> axum::response::Response {
    detached("update", update_inner(st, cid, b)).await
}

async fn update_inner(st: AppState, cid: String, b: Value) -> axum::response::Response {
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    let db = st.cfg.db_path();
    let fields = b.as_object().cloned().unwrap_or_default();
    for col in ["name", "server", "username", "password", "ec_ver", "probe_url"] {
        if fields.get(col).is_some_and(|v| !v.is_string()) {
            return err_detail(StatusCode::BAD_REQUEST, &format!("{col} must be text"));
        }
    }
    if fields.get("routing_enabled").is_some_and(|v| !v.is_boolean()) {
        return err_detail(StatusCode::BAD_REQUEST, "routing_enabled must be boolean");
    }
    let _routing_guard = if fields.contains_key("routing_enabled") {
        Some(ROUTING_MUTATION_LOCK.lock().await)
    } else {
        None
    };
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    match crate::replacement_store::public_status(&db, &cid) {
        Ok(Some(pending)) if matches!(pending["phase"].as_str(), Some("committed" | "rolled_back")) => {
            if let Err(e) = crate::replacement::cleanup(&st, &cid).await { return err_detail(StatusCode::CONFLICT, &format!("上次操作的资源清理未完成: {e}")); }
        }
        Ok(Some(_)) => return err_detail(StatusCode::CONFLICT, "上次修改尚未验证，请先完成登录或恢复上一次设置"),
        Ok(None) => {},
        Err(e) => return err500(&format!("replacement status: {e}")),
    }
    let sk = secret_keys_of(&ch.vpn_type);
    let password_changed = if let Some(value) = fields.get("password") {
        match store::get_password(&db, &key, &cid) {
            Ok(old) => value.as_str().unwrap_or_default() != old,
            Err(e) => return err500(&format!("read existing password: {e}")),
        }
    } else { false };
    let touched = password_changed || ["server", "username", "ec_ver"].iter().any(|col| {
        fields.get(*col).is_some_and(|v| {
            let old = match *col { "server" => ch.server.as_str(), "username" => ch.username.as_str(), _ => ch.ec_ver.as_deref().unwrap_or_default() };
            store::clean_field(col, v.as_str().unwrap_or_default()) != old
        })
    });
    let routing_changed = fields.get("routing_enabled").and_then(|v| v.as_bool()).is_some_and(|v| v != ch.routing_enabled);
    // 审计:改前快照 + 提交了哪些字段。密码只记「改没改」,值绝不进日志(命门 #5)。
    let before = channel_snapshot(&ch);
    let changed_fields: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|k| *k != "password")
        .collect();
    let audit_detail = |result: &str, after: Value| {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
            "vpn_type": ch.vpn_type.as_str(), "changed_fields": changed_fields.clone(),
            "password_changed": password_changed, "reprovisioned": touched && ch.container_id.is_some(),
            "before": before.clone(), "after": after, "result": result,
        })
    };
    let rollback_routing = || {
        let mut rollback_fields = serde_json::Map::new();
        rollback_fields.insert("routing_enabled".into(), json!(ch.routing_enabled));
        store::update_channel(&db, &key, &cid, &rollback_fields, &sk)
    };
    let provisioned = touched && ch.container_id.is_some();
    let changed = if provisioned {
        crate::replacement::replace(&st, &ch, &fields, false).await
    } else {
        store::update_channel(&db, &key, &cid, &fields, &sk)
    };
    if let Err(e) = changed {
        // 自动恢复成功时保留确认过的运行态；只有未恢复的操作才落 error，避免继续分流。
        if provisioned {
            if crate::replacement_store::public_status(&db, &cid).ok().flatten().is_some_and(|p| !matches!(p["phase"].as_str(), Some("committed" | "rolled_back" | "awaiting_login"))) {
                let _ = store::set_status(&db, &cid, "error");
            }
            let _ = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
        }
        let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid));
        detail["error"] = json!(e.to_string());
        crate::events::audit_failed("channel_update", "通道更新未完成，保留恢复记录与原数据", detail);
        return err500(&format!("{e}"));
    }
    if provisioned || routing_changed {
        let reload = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
        if !reload_ok(&reload) {
            crate::ev!(error, "api", "mihomo_reload_failed", "更新通道后 mihomo 重载未达成",
                { "operation": "update", "cid": cid.as_str(), "error": reload.as_str() });
            if !routing_changed {
                // 字段已落库、只是分流面没跟上:仍算改成了,但把 reload_status 记进审计。
                let mut detail = audit_detail("ok", channel_snapshot_of(&db, &cid));
                detail["reload_status"] = json!(reload.as_str());
                crate::events::audit("channel_update", "通道已更新(mihomo 重载未达成)", detail);
                return channel_json(&db, &cid);
            }
            if let Err(error) = rollback_routing() {
                let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid));
                detail["reload_status"] = json!(reload.as_str());
                detail["rollback_error"] = json!(error.to_string());
                crate::events::audit_failed("channel_update", "切换通道分流失败且状态回滚失败", detail);
                return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("routing rollback failed: {error}"));
            }
            let rollback = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
            let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid));
            detail["reload_status"] = json!(reload.as_str());
            detail["rollback_status"] = json!(rollback.as_str());
            crate::events::audit_failed("channel_update", "切换通道分流未生效,已回滚字段", detail);
            return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {reload}"));
        }
    }
    crate::events::audit(
        "channel_update",
        "通道已更新",
        audit_detail("ok", channel_snapshot_of(&db, &cid)),
    );
    channel_json(&db, &cid)
}

// ── login / upload / status(命门 #1 探活、#5 上传安装器) ────────────────────

#[derive(Deserialize, Default)]
pub struct LoginQuery { pub viewer: Option<String> }

pub async fn login(State(st): State<AppState>, Path(cid): Path<String>, Query(q): Query<LoginQuery>) -> axum::response::Response {
    if q.viewer.as_deref().is_some_and(|v| !crate::novnc::valid_viewer(v)) {
        return err_detail(StatusCode::BAD_REQUEST, "无效的登录视图标识");
    }
    let _operation = match st.lifecycle.access(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    let db = st.cfg.db_path();
    match crate::replacement_store::public_status(&db, &cid) {
        Ok(Some(pending)) if !matches!(pending["phase"].as_str(), Some("awaiting_login" | "committed" | "rolled_back")) => return err_detail(StatusCode::CONFLICT, "上次操作尚未恢复，请先恢复上一次设置"),
        Err(e) => return err500(&e.to_string()),
        _ => {},
    }
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    if ch.login_method == "headless" {
        return Json(json!({ "login_mode": "headless" })).into_response();
    }
    let port = match crate::novnc::acquire(&st, &cid, q.viewer.as_deref()).await {
        Ok(port) => port,
        Err(e) => return err_detail(StatusCode::BAD_GATEWAY, &format!("noVNC forward: {e}")),
    };
    Json(json!({ "url": webutil::login_url(port, &ch.vnc_password.unwrap_or_default(), &ch.vpn_type),
        "viewer_id": q.viewer, "viewer_ttl_seconds": crate::novnc::VIEWER_TTL_SECONDS })).into_response()
}

pub async fn renew_login_viewer(State(st): State<AppState>, Path((cid, viewer)): Path<(String, String)>) -> axum::response::Response {
    if !crate::novnc::valid_viewer(&viewer) { return err_detail(StatusCode::BAD_REQUEST, "无效的登录视图标识"); }
    if crate::novnc::renew(&st, &cid, &viewer).await {
        Json(json!({ "ok": true })).into_response()
    } else { err404("登录视图已过期，请重新打开") }
}

pub async fn release_login_viewer(State(st): State<AppState>, Path((cid, viewer)): Path<(String, String)>) -> axum::response::Response {
    if !crate::novnc::valid_viewer(&viewer) { return err_detail(StatusCode::BAD_REQUEST, "无效的登录视图标识"); }
    crate::novnc::release(&st, &cid, &viewer).await;
    Json(json!({ "ok": true })).into_response()
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
    let docker = match st.docker() {
        Some(d) => d,
        None => return err500("docker unavailable"),
    };
    // 命门 #5:二进制经 put_archive 落数据卷,绝不入 SQLite/回传
    // 审计只记文件名与大小:安装器内容既不入库也不进日志(命门 #5)。
    let audit_base = || {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(),
            "filename": filename.as_str(), "size_bytes": blob.len() as u64,
        })
    };
    if let Err(e) = crate::docker::put_file(&docker, &format!("vpn-{cid}"), "/root", &filename, blob.as_ref()).await {
        let mut detail = audit_base();
        detail["result"] = json!("failed");
        detail["error"] = json!(e.to_string());
        crate::events::audit_failed("channel_upload", "通道安装文件投递失败", detail);
        return err500(&format!("{e}"));
    }
    let _ = store::set_config_field(&db, &key, &cid, "package", &filename, false);
    let mut detail = audit_base();
    detail["result"] = json!("ok");
    crate::events::audit("channel_upload", "安装包已投递到通道数据卷", detail);
    Json(json!({ "ok": true, "package": filename })).into_response()
}

/// 登录备注长度上限(字符),对照 main.py NOTE_MAX。
const NOTE_MAX: usize = 20000;

/// GET /api/channels/{cid}/note:登录信息备注(用户自记的账号/密码/联系人等)。
/// ⚠️ 命门 #5 的有意例外:备注 Fernet 加密落库,但本端点解密回传——用户记它就是
/// 为了下次交互登录照抄。/api/channels 列表仍不回传(secret 字段被剥除),仅此单点可读。
pub async fn note_get(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    match store::get_channel(&db, &cid) {
        Ok(Some(_)) => {}
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    }
    let note = match store::get_config(&db, &key, &cid) {
        Ok(cfg) => cfg.get("login_note").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        Err(e) => return err500(&format!("{e}")),
    };
    Json(json!({ "note": note })).into_response()
}

/// PUT /api/channels/{cid}/note:整体覆盖备注文本(Fernet 加密落 config_json)。
pub async fn note_put(
    State(st): State<AppState>,
    Path(cid): Path<String>,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    match store::get_channel(&db, &cid) {
        Ok(Some(_)) => {}
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    }
    let note = match b.get("note") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "note 须为字符串"),
    };
    if note.chars().count() > NOTE_MAX {
        return err_detail(StatusCode::BAD_REQUEST, &format!("备注过长(上限 {NOTE_MAX} 字符)"));
    }
    // ⚠️ 备注正文里通常就是账号密码:审计只记长度变化,正文一个字都不进日志(命门 #5)。
    let length_before = store::get_config(&db, &key, &cid)
        .ok()
        .and_then(|cfg| cfg.get("login_note").and_then(|v| v.as_str()).map(|s| s.chars().count()))
        .unwrap_or(0);
    let audit_base = || {
        json!({
            "target_kind": "note", "target_id": cid.as_str(),
            "before": { "length": length_before }, "after": { "length": note.chars().count() },
        })
    };
    if let Err(e) = store::set_config_field(&db, &key, &cid, "login_note", note, true) {
        let mut detail = audit_base();
        detail["result"] = json!("failed");
        detail["error"] = json!(e.to_string());
        crate::events::audit_failed("note_update", "登录信息备注保存失败", detail);
        return err500(&format!("{e}"));
    }
    let mut detail = audit_base();
    detail["result"] = json!("ok");
    crate::events::audit("note_update", "登录信息备注已更新", detail);
    Json(json!({ "ok": true })).into_response()
}

pub async fn status(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    probe_response(crate::lifecycle::sample(st, cid, true).await)
}

pub async fn channel_health(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    probe_response(crate::lifecycle::sample(st, cid, false).await)
}

fn probe_response(result: Result<Value, String>) -> axum::response::Response {
    match result {
        Ok(value) => Json(value).into_response(),
        Err(e) if e == "not found" => err404(&e),
        Err(e) => err503(&e),
    }
}

// ── start / stop / delete(byo 原地 start;hagb/oss 走重建) ──────────────────

pub async fn start(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    detached("start", start_inner(st, cid)).await
}

async fn start_inner(st: AppState, cid: String) -> axum::response::Response {
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    let started = std::time::Instant::now();
    let before = channel_snapshot(&ch);
    let audit_detail = |result: &str, after: Value, mode: &str| {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
            "vpn_type": ch.vpn_type.as_str(), "mode": mode,
            "before": before.clone(), "after": after, "result": result,
            "duration_ms": started.elapsed().as_millis() as u64,
        })
    };
    let docker = match st.docker() {
        Some(d) => d,
        None => {
            let mut detail = audit_detail("failed", Value::Null, "unknown");
            detail["error"] = json!("docker unavailable");
            crate::events::audit_failed("channel_start", "通道启动失败", detail);
            return err503("docker unavailable");
        }
    };
    match crate::replacement::resume(&st, &cid).await {
        Ok(true) => {
            let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
            let mut detail = audit_detail("ok", channel_snapshot_of(&db, &cid), "resume_pending");
            detail["reload_status"] = json!(reload);
            crate::events::audit("channel_start", "继续启动新设置，保留上一次设置供恢复", detail);
            return Json(json!({"ok":true})).into_response();
        }
        Ok(false) => {},
        Err(e) => return err500(&format!("启动未完成: {e}")),
    }
    let runtime = registry::get(&ch.vpn_type).map(|s| s.runtime).unwrap_or_default();
    if runtime == "byo" {
        // byo 客户端装在可写层,扛得住原地重启 → 不重建
        if let Err(e) = manager::start(&docker, &cid).await {
            let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid), "in_place");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("channel_start", "通道启动失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("start: {e}"));
        }
        if let Err(e) = store::set_status(&db, &cid, "running") {
            let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid), "in_place");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("channel_start", "通道已启动但状态落库失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("set_status: {e}"));
        }
        // 状态联动:byo 原地 start 不走重建路径,也要 rebuild 让停用期折叠掉的规则恢复生效。
        let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
        if !reload_ok(&reload) {
            crate::ev!(error, "api", "mihomo_reload_failed", "启动通道后 mihomo 重载未达成",
                { "operation": "start", "cid": cid.as_str(), "error": reload.as_str() });
        }
        crate::events::audit(
            "channel_start",
            "通道启动完成",
            audit_detail("ok", channel_snapshot_of(&db, &cid), "in_place"),
        );
        return Json(json!({ "ok": true })).into_response();
    }
    // 有旧实例时先准备独立候选；失败保留旧设置与数据。
    if ch.container_id.is_some() {
        if let Err(e) = crate::replacement::replace(&st, &ch, &Default::default(), true).await {
            if crate::replacement_store::public_status(&db, &cid).ok().flatten().is_some_and(|p| !matches!(p["phase"].as_str(), Some("committed" | "rolled_back" | "awaiting_login"))) {
                let _ = store::set_status(&db, &cid, "error");
            }
            let _ = manager::rebuild(&st.cfg, Some(&docker), &db).await;
            return err500(&format!("启动未完成: {e}"));
        }
        let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
        let mut detail = audit_detail("ok", channel_snapshot_of(&db, &cid), "replace");
        detail["reload_status"] = json!(reload);
        crate::events::audit("channel_start", "通道替换已应用，连通状态以探活为准", detail);
        return Json(json!({"ok":true})).into_response();
    }
    // 没有旧实例的初始化路径另行收敛；Docker outcome 确认前不改 DB。
    let vnc = ch.vnc_password.clone().unwrap_or_default();
    match provision_with_docker(&st, &docker, &ch, &vnc).await {
        Ok((container_id, novnc)) => {
            if let Err(e) = store::set_container(&db, &cid, &container_id, novnc, "running") {
                let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid), "recreate");
                detail["error"] = json!(e.to_string());
                detail["container_id"] = json!(container_id.as_str());
                crate::events::audit_failed("channel_start", "通道容器已启动但状态落库失败", detail);
                return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("set_container: {e}"));
            }
            // 对照 Python start:响应 {"ok": true}(不含 reload_status);重载未达成仅记日志。
            let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
            if !reload_ok(&reload) {
                crate::ev!(error, "api", "mihomo_reload_failed", "启动通道后 mihomo 重载未达成",
                    { "operation": "start", "cid": cid.as_str(), "error": reload.as_str() });
            }
            crate::events::audit(
                "channel_start",
                "通道启动完成",
                audit_detail("ok", channel_snapshot_of(&db, &cid), "recreate"),
            );
            Json(json!({ "ok": true })).into_response()
        }
        Err(e) => {
            let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid), "recreate");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("channel_start", "通道启动失败", detail);
            err500(&format!("{e}"))
        }
    }
}

pub async fn stop(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    detached("stop", stop_inner(st, cid)).await
}

async fn stop_inner(st: AppState, cid: String) -> axum::response::Response {
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(ch)) => ch,
        Ok(None) => return err404("not found"),
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("get_channel: {e}")),
    };
    let started = std::time::Instant::now();
    let before = channel_snapshot(&ch);
    let audit_detail = |result: &str, after: Value| {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
            "vpn_type": ch.vpn_type.as_str(), "before": before.clone(), "after": after,
            "result": result, "duration_ms": started.elapsed().as_millis() as u64,
        })
    };
    let docker = match st.docker() {
        Some(d) => d,
        None => {
            let mut detail = audit_detail("failed", Value::Null);
            detail["error"] = json!("docker unavailable");
            crate::events::audit_failed("channel_stop", "通道停止失败", detail);
            return err503("docker unavailable");
        }
    };
    if let Err(e) = crate::replacement::before_stop(&st, &cid).await {
        return err500(&format!("停止前恢复未完成: {e}"));
    }
    if let Err(e) = manager::stop(&docker, &cid).await {
        let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid));
        detail["error"] = json!(e.to_string());
        crate::events::audit_failed("channel_stop", "通道停止失败", detail);
        return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("stop: {e}"));
    }
    crate::novnc::drop_for(&st, &cid).await;
    if let Err(e) = store::set_status(&db, &cid, "stopped") {
        let mut detail = audit_detail("failed", channel_snapshot_of(&db, &cid));
        detail["error"] = json!(e.to_string());
        crate::events::audit_failed("channel_stop", "通道已停止但状态落库失败", detail);
        return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("set_status: {e}"));
    }
    // 状态联动:停止的通道其规则在 effective_rules 里自动失效,rebuild 一次把分流面
    // (mihomo/provider/PAC/TUN 路由)同步收掉,避免黑洞规则;重载未达成仅记日志。
    let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
    if !reload_ok(&reload) {
        crate::ev!(error, "api", "mihomo_reload_failed", "停止通道后 mihomo 重载未达成",
            { "operation": "stop", "cid": cid.as_str(), "error": reload.as_str() });
    }
    crate::events::audit(
        "channel_stop",
        "通道已停止",
        audit_detail("ok", channel_snapshot_of(&db, &cid)),
    );
    Json(json!({ "ok": true })).into_response()
}

pub async fn delete(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    detached("delete", delete_inner(st, cid)).await
}

async fn delete_inner(st: AppState, cid: String) -> axum::response::Response {
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    let db = st.cfg.db_path();
    let ch = match store::get_channel(&db, &cid) {
        Ok(Some(ch)) => ch,
        Ok(None) => return err404("not found"),
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("get_channel: {e}")),
    };
    let started = std::time::Instant::now();
    // 删除是最需要「事后据此判断怎么回滚」的一步:before 除通道字段外还带全部规则
    // (删完 db 里就没有了)。密码 / secret 不在快照里,重建仍需用户重新填(命门 #5)。
    let mut before = channel_snapshot(&ch);
    before["rules"] = rule_snapshots(&store::list_rules(&db, &cid).unwrap_or_default());
    let audit_detail = |result: &str| {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
            "vpn_type": ch.vpn_type.as_str(), "before": before.clone(), "after": null,
            "result": result, "duration_ms": started.elapsed().as_millis() as u64,
        })
    };
    let docker = match st.docker() {
        Some(d) => d,
        None => {
            let mut detail = audit_detail("failed");
            detail["error"] = json!("docker unavailable");
            crate::events::audit_failed("channel_delete", "通道删除失败", detail);
            return err503("docker unavailable");
        }
    };
    let handled = match crate::replacement::discard(&st, &cid).await {
        Ok(handled) => handled,
        Err(e) => return err500(&format!("删除尚未完成: {e}")),
    };
    if !handled {
        if let Err(e) = manager::remove(&docker, &cid).await {
            let mut detail = audit_detail("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("channel_delete", "通道删除失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("remove: {e}"));
        }
        crate::novnc::drop_for(&st, &cid).await;
        if let Err(e) = store::del_channel(&db, &cid) {
            let mut detail = audit_detail("failed");
            detail["error"] = json!(e.to_string());
            detail["container_removed"] = json!(true);
            crate::events::audit_failed("channel_delete", "通道容器已删除但配置落库失败", detail);
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("del_channel: {e}"));
        }
    }
    // 对照 Python delete:响应 {"ok": true}(不含 reload_status);重载未达成仅记日志。
    let reload = manager::rebuild(&st.cfg, Some(&docker), &db).await;
    if !reload_ok(&reload) {
        crate::ev!(error, "api", "mihomo_reload_failed", "删除通道后 mihomo 重载未达成",
            { "operation": "delete", "cid": cid.as_str(), "error": reload.as_str() });
    }
    crate::events::audit("channel_delete", "通道已删除", audit_detail("ok"));
    Json(json!({ "ok": true })).into_response()
}

// ── 分流总开关 / 自愈开关 ────────────────────────────────────────────────

pub async fn routing_get(State(st): State<AppState>) -> Json<Value> {
    Json(json!({ "off": store::routing_off(&st.cfg.data_dir) }))
}

pub async fn routing_set(
    State(st): State<AppState>,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let off = match b.get("off") {
        Some(Value::Bool(value)) => *value,
        _ => return err_detail(StatusCode::BAD_REQUEST, "off must be boolean"),
    };
    let _guard = ROUTING_MUTATION_LOCK.lock().await;
    let previous = store::routing_off(&st.cfg.data_dir);
    if let Err(e) = store::set_routing_off(&st.cfg.data_dir, off) {
        return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("set routing flag: {e}"));
    }
    let reload = manager::rebuild(&st.cfg, st.docker().as_ref(), &st.cfg.db_path()).await;
    let applied = reload_ok(&reload);
    if !applied {
        if let Err(error) = store::set_routing_off(&st.cfg.data_dir, previous) {
            crate::events::audit_failed("routing_toggle", "全局分流切换失败且标记回滚失败", json!({
                "target_kind": "system", "target_name": "routing",
                "before": { "off": previous }, "after": { "off": off },
                "requested_off": off, "reload_status": reload.as_str(),
                "result": "failed", "error": error.to_string()
            }));
            return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("routing rollback failed: {error}"));
        }
        let rollback = manager::rebuild(&st.cfg, st.docker().as_ref(), &st.cfg.db_path()).await;
        crate::events::audit_failed("routing_toggle", "全局分流切换未生效,已回滚标记", json!({
            "target_kind": "system", "target_name": "routing",
            "before": { "off": previous }, "after": { "off": previous },
            "requested_off": off, "reload_status": reload.as_str(), "rollback_status": rollback.as_str(),
            "result": "failed", "error": format!("mihomo reload failed: {reload}")
        }));
        return err_detail(StatusCode::BAD_GATEWAY, &format!("mihomo reload failed: {reload}"));
    }
    crate::events::audit("routing_toggle", "全局分流状态已切换", json!({
        "target_kind": "system", "target_name": "routing",
        "before": { "off": previous }, "after": { "off": off },
        "applied": true, "reload_status": reload.as_str(), "result": "ok"
    }));
    Json(json!({ "off": off, "applied": true, "reload_status": reload })).into_response()
}

pub async fn self_heal_set(
    State(st): State<AppState>,
    Json(b): Json<Value>,
) -> axum::response::Response {
    let enabled = match b.get("enabled") {
        Some(Value::Bool(value)) => *value,
        _ => return err_detail(StatusCode::BAD_REQUEST, "enabled must be boolean"),
    };
    let previous = st.self_heal_enabled();
    st.set_self_heal_enabled(enabled);
    crate::events::audit("self_heal_toggle", "自动修复状态已切换", json!({
        "target_kind": "system", "target_name": "self_heal",
        "before": { "enabled": previous }, "after": { "enabled": enabled }, "result": "ok"
    }));
    Json(json!({ "enabled": enabled })).into_response()
}

// ── Clash 接入 / 入口接入(命门 #2:IP 带 no-resolve、域名经 bare) ────────────

fn text_plain(body: String) -> axum::response::Response {
    ([(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")], body).into_response()
}

pub async fn clash_provider(State(st): State<AppState>) -> axum::response::Response {
    // ★最危险:db 读失败绝不能回空 200——那会让外层 Clash 静默丢分流、流量 DIRECT 裸奔。
    // 必须 5xx,让 Clash 保留上一份 provider(rule-provider 拉取失败时用旧副本)。
    let rules = match store::effective_rules(&st.cfg.db_path()) {
        Ok(r) => r,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("all_rules: {e}")),
    };
    text_plain(webutil::clash_provider_text(&rules))
}

pub async fn clash_snippet(State(st): State<AppState>) -> axum::response::Response {
    let rules = match store::effective_rules(&st.cfg.db_path()) {
        Ok(r) => r,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("all_rules: {e}")),
    };
    let ui = st.cfg.ui_port.to_string();
    text_plain(webutil::clash_snippet_text(&rules, &st.cfg.mihomo_host_port, &ui))
}

pub async fn entry_pac(State(st): State<AppState>) -> axum::response::Response {
    let rules = match store::effective_rules(&st.cfg.db_path()) {
        Ok(r) => r,
        Err(e) => return err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("all_rules: {e}")),
    };
    let pac = webutil::pac_text(&rules, &st.cfg.mihomo_host_port);
    ([(axum::http::header::CONTENT_TYPE, "application/x-ns-proxy-autoconfig")], pac).into_response()
}

pub async fn entry_setup_commands(State(st): State<AppState>) -> Json<Value> {
    let ui = st.cfg.ui_port.to_string();
    Json(webutil::setup_commands(&st.cfg.mihomo_host_port, &ui))
}

// ── 7c:宿主接管层「真执行」(层1 检测/Verge profile + 层2 系统代理 networksetup) ──
// 仅 Tauri/host 模型可用(Rust core 跑在宿主)。前端 feature-detect:404/失败则隐藏按钮。

/// 检测本机 Clash 客户端(读-only)。
pub async fn clash_detect() -> Json<Value> {
    Json(serde_json::to_value(entry::detect_clash().await).unwrap_or_else(|_| json!({})))
}

/// Clash Verge Rev 可导入的 Merge profile(text/yaml)。
pub async fn clash_merge_profile(State(st): State<AppState>) -> axum::response::Response {
    let ui = st.cfg.ui_port.to_string();
    let body = entry::verge_merge_profile(&st.cfg.mihomo_host_port, &ui);
    crate::ev!(info, "entry", "clash_profile_generated", "Clash Merge profile 已生成",
        { "client": "clash_verge_rev" });
    ([(axum::http::header::CONTENT_TYPE, "text/yaml; charset=utf-8")], body).into_response()
}

/// 读系统自动代理当前状态(读-only,安全)。
pub async fn system_proxy_get(State(st): State<AppState>) -> Json<Value> {
    let ui = st.cfg.ui_port.to_string();
    Json(serde_json::to_value(entry::system_proxy_status(&ui).await).unwrap_or_else(|_| json!({})))
}

/// 一键应用/清除系统自动代理(PAC)。body `{enable: bool}`。⚠️ 改系统设置,前端按钮显式触发。
pub async fn system_proxy_set(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    if !st.cfg.host_integrations_allowed() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "隔离实例禁用系统代理变更"}))).into_response();
    }
    let ui = st.cfg.ui_port.to_string();
    let enable = b.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
    match entry::system_proxy_apply(&ui, enable).await {
        Ok(state) => Json(json!({ "ok": true, "state": state })).into_response(),
        Err(e) => err500(&format!("{e}")),
    }
}

// ── 层3:TUN 入口(root helper + 宿主 mihomo#2;任务 #6) ─────────────────────

/// TUN 入口综合状态(读-only;前端 feature-detect:web 版无此路由 → 404 → 隐藏卡片)。
pub async fn tun_get(State(st): State<AppState>) -> Json<Value> {
    Json(entry::tun_status(&st.cfg).await)
}

/// 启用/停用 TUN 入口。body `{enable: bool}`。前端按钮显式触发。
pub async fn tun_set(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    if !st.cfg.host_integrations_allowed() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "隔离实例禁用宿主 TUN 和助手变更"}))).into_response();
    }
    let enable = b.get("enable").and_then(|v| v.as_bool()).unwrap_or(false);
    let before = tun_snapshot(&entry::tun_status(&st.cfg).await);
    match entry::tun_apply(&st.cfg, enable).await {
        Ok(state) => {
            crate::events::audit(
                "tun_toggle",
                if enable { "TUN 入口已启用" } else { "TUN 入口已停用" },
                json!({
                    "target_kind": "entry", "target_name": "tun", "requested_enable": enable,
                    "before": before, "after": tun_snapshot(&state), "result": "ok"
                }),
            );
            Json(json!({ "ok": true, "state": state })).into_response()
        }
        Err(e) => {
            crate::events::audit_failed("tun_toggle", "TUN 入口切换失败", json!({
                "target_kind": "entry", "target_name": "tun", "requested_enable": enable,
                "before": before, "after": null, "result": "failed", "error": e.to_string()
            }));
            err500(&format!("{e}"))
        }
    }
}

/// 安装/升级 helper(触发一次管理员密码弹窗)。
pub async fn tun_install(State(st): State<AppState>) -> axum::response::Response {
    if !st.cfg.host_integrations_allowed() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "隔离实例禁用宿主 TUN 和助手变更"}))).into_response();
    }
    let before = tun_snapshot(&entry::tun_status(&st.cfg).await);
    match entry::tun_install(&st.cfg).await {
        Ok(state) => {
            crate::events::audit("tun_install", "TUN 助手已安装/升级", json!({
                "target_kind": "entry", "target_name": "tun_helper",
                "before": before, "after": tun_snapshot(&state), "result": "ok"
            }));
            Json(json!({ "ok": true, "state": state })).into_response()
        }
        Err(e) => {
            crate::events::audit_failed("tun_install", "TUN 助手安装失败", json!({
                "target_kind": "entry", "target_name": "tun_helper",
                "before": before, "after": null, "result": "failed", "error": e.to_string()
            }));
            err500(&format!("{e}"))
        }
    }
}

/// 卸载 helper(管理员密码弹窗)。
pub async fn tun_uninstall(State(st): State<AppState>) -> axum::response::Response {
    if !st.cfg.host_integrations_allowed() {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "隔离实例禁用宿主 TUN 和助手变更"}))).into_response();
    }
    let before = tun_snapshot(&entry::tun_status(&st.cfg).await);
    match entry::tun_uninstall(&st.cfg).await {
        Ok(state) => {
            crate::events::audit("tun_uninstall", "TUN 助手已卸载", json!({
                "target_kind": "entry", "target_name": "tun_helper",
                "before": before, "after": tun_snapshot(&state), "result": "ok"
            }));
            Json(json!({ "ok": true, "state": state })).into_response()
        }
        Err(e) => {
            crate::events::audit_failed("tun_uninstall", "TUN 助手卸载失败", json!({
                "target_kind": "entry", "target_name": "tun_helper",
                "before": before, "after": null, "result": "failed", "error": e.to_string()
            }));
            err500(&format!("{e}"))
        }
    }
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
        st.docker().as_ref(),
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
            match st.docker().as_ref() {
                Some(d) => match crate::docker::create_bridge_network(d, &name).await {
                    Ok(_) => {
                        crate::events::audit("preflight_fix", "已创建 docker 网络", json!({
                            "target_kind": "system", "target_name": name.as_str(),
                            "action": "create_network", "before": null,
                            "after": { "network": name.as_str() }, "result": "ok"
                        }));
                        Json(json!({ "ok": true })).into_response()
                    }
                    Err(e) => {
                        crate::events::audit_failed("preflight_fix", "创建 docker 网络失败", json!({
                            "target_kind": "system", "target_name": name.as_str(),
                            "action": "create_network", "result": "failed", "error": e.to_string()
                        }));
                        err500(&format!("{e}"))
                    }
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
            let docker = match st.docker().as_ref() {
                Some(d) => d.clone(),
                None => return err500("docker unavailable"),
            };
            let mirrors = enabled_mirror_hosts(&st);
            let tid = preflight::start_pull(docker, &image, &registry::host_arch(), mirrors.clone());
            // 拉取是后台任务:审计只记「谁在什么时候要拉哪个镜像」,结果由任务状态端点看。
            crate::events::audit("preflight_fix", "已发起镜像拉取", json!({
                "target_kind": "system", "target_name": image.as_str(), "action": "pull_image",
                "task_id": tid.as_str(), "mirrors": mirrors, "result": "ok"
            }));
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
    Json(preflight::image_inventory(st.docker().as_ref(), &arch, &mirrors).await)
}

pub async fn mirrors_list(State(st): State<AppState>) -> axum::response::Response {
    match store::list_mirrors(&st.cfg.db_path()) {
        Ok(m) => Json(json!(m)).into_response(),
        Err(e) => err_detail(StatusCode::INTERNAL_SERVER_ERROR, &format!("list_mirrors: {e}")),
    }
}

/// 双栈镜像 host 契约:Python 当前 ASCII allowlist `[A-Za-z0-9._:/-]+`。add/test 共用。
fn valid_mirror_host(h: &str) -> bool {
    !h.is_empty() && h.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:/-".contains(&b))
}

pub async fn mirrors_add(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let host = b.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if host.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({ "error": "host required" }))).into_response();
    }
    // 命门②:堵 userinfo '@' / 逗号 / 空白 / 换行等注入字符(对照 Python HTTPException(400));不限私网。
    if !valid_mirror_host(&host) {
        return err_detail(StatusCode::BAD_REQUEST, "非法镜像源地址");
    }
    let db = st.cfg.db_path();
    match store::add_mirror(&db, &host) {
        Ok(mid) => match store::list_mirrors(&db).unwrap_or_default().into_iter().find(|m| m.id == mid) {
            Some(m) => {
                crate::events::audit("mirror_add", "已新增镜像源", json!({
                    "target_kind": "mirror", "target_id": mid, "target_name": m.host.as_str(),
                    "before": null, "after": mirror_snapshot(&m), "result": "ok"
                }));
                Json(serde_json::to_value(m).unwrap()).into_response()
            }
            None => err500("mirror added but not found"),
        },
        Err(e) => {
            crate::events::audit_failed("mirror_add", "新增镜像源失败", json!({
                "target_kind": "mirror", "target_name": host.as_str(),
                "before": null, "after": null, "result": "failed", "error": e.to_string()
            }));
            (StatusCode::BAD_REQUEST, Json(json!({ "error": "mirror already exists" }))).into_response()
        }
    }
}

/// 按 id 取一条镜像源快照(审计 before/after 用;没有就 null)。
fn mirror_snapshot_of(db: &std::path::Path, mid: i64) -> Value {
    store::list_mirrors(db)
        .unwrap_or_default()
        .iter()
        .find(|m| m.id == mid)
        .map(mirror_snapshot)
        .unwrap_or(Value::Null)
}

pub async fn mirrors_patch(State(st): State<AppState>, Path(mid): Path<i64>, Json(b): Json<Value>) -> Json<Value> {
    let db = st.cfg.db_path();
    let priority = b.get("priority").and_then(|v| v.as_i64());
    let enabled = b.get("enabled").and_then(|v| v.as_bool());
    let before = mirror_snapshot_of(&db, mid);
    let error = store::set_mirror(&db, mid, priority, enabled).err();
    let detail = json!({
        "target_kind": "mirror", "target_id": mid,
        "target_name": before.get("host").cloned().unwrap_or(Value::Null),
        "before": before, "after": mirror_snapshot_of(&db, mid),
    });
    match error {
        None => {
            let mut detail = detail;
            detail["result"] = json!("ok");
            crate::events::audit("mirror_update", "镜像源已更新", detail);
        }
        Some(e) => {
            let mut detail = detail;
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("mirror_update", "镜像源更新失败", detail);
        }
    }
    Json(json!({ "ok": true }))
}

pub async fn mirrors_del(State(st): State<AppState>, Path(mid): Path<i64>) -> Json<Value> {
    let db = st.cfg.db_path();
    let before = mirror_snapshot_of(&db, mid);
    let error = store::del_mirror(&db, mid).err();
    let detail = json!({
        "target_kind": "mirror", "target_id": mid,
        "target_name": before.get("host").cloned().unwrap_or(Value::Null),
        "before": before, "after": null,
    });
    match error {
        None => {
            let mut detail = detail;
            detail["result"] = json!("ok");
            crate::events::audit("mirror_delete", "镜像源已删除", detail);
        }
        Some(e) => {
            let mut detail = detail;
            detail["result"] = json!("failed");
            detail["error"] = json!(e.to_string());
            crate::events::audit_failed("mirror_delete", "镜像源删除失败", detail);
        }
    }
    Json(json!({ "ok": true }))
}

pub async fn mirrors_test(Json(b): Json<Value>) -> axum::response::Response {
    let host = b.get("host").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    // 命门②:非法 host(userinfo/逗号/空白/换行等)不发请求,直接 4xx——对齐 mirrors_add 的校验。
    if !valid_mirror_host(&host) {
        return err_detail(StatusCode::BAD_REQUEST, "非法镜像源地址");
    }
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
    Json(json!({ "reachable": ok, "latency_ms": ms })).into_response()
}

// ── 配置导出 / 导入(整站通道 + 规则的备份/迁移) ──────────────────────────────

/// GET /api/config/export:把全部通道(公开字段 + 解密后的 config + 规则)导出为一份 JSON 文档。
/// ⚠️ 命门 #5 的**有意例外**:headless 通道的 config 含 CLI 注入凭据(密码/私钥),随导出一并带出
/// ——否则导入到新机器后无法重连(用户明确要求)。交互登录密码不导出(导入后重新登录)。
/// 后人勿把此处「修回」去剥 headless 的密码:那会让 headless 通道导入后失联。
pub async fn config_export(State(st): State<AppState>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let channels = match store::list_channels(&db) {
        Ok(c) => c,
        Err(e) => return err500(&format!("list_channels: {e}")),
    };
    let mut out = Vec::new();
    for ch in &channels {
        // 命门 #5 例外:get_config 返回 secret 解密后的完整 map(含密码/私钥)。
        let mut config = match store::get_config(&db, &key, &ch.id) {
            Ok(m) => m,
            Err(e) => return err500(&format!("get_config {}: {e}", ch.id)),
        };
        // 交互登录密码不导出(导入后重新登录);byo 安装器文件名引用带不走(二进制在数据卷里)。
        if ch.login_method != "headless" {
            config.remove("password");
        }
        if ch.login_method == "byo" {
            config.remove("package");
        }
        // 登录备注一律不导出:「键入到容器」自动留档默认开,备注里大概率就是上面刚剥掉的
        // 交互登录密码(红队 D5),导出明文会绕过剥密;备注留在本机加密库。
        config.remove("login_note");
        let rules: Vec<Value> = store::list_rules(&db, &ch.id)
            .unwrap_or_default()
            .into_iter()
            .map(|r| json!({
                "kind": r.kind,
                "pattern": r.pattern,
                "enabled": r.enabled,
                "note": r.note,
                "locked": r.locked,
            }))
            .collect();
        out.push(json!({
            "name": ch.name,
            "vpn_type": ch.vpn_type,
            "server": ch.server,
            "ec_ver": ch.ec_ver.clone().unwrap_or_default(),
            "login_method": ch.login_method,
            "username": ch.username,
            "probe_url": ch.probe_url,
            "routing_enabled": ch.routing_enabled,
            "config": Value::Object(config),
            "rules": rules,
        }));
    }
    // 导出文档里带着 headless 通道的注入凭据(命门 #5 的有意例外),因此这一步本身
    // 就是敏感动作:审计记「导出了哪些通道」,不记任何 config 内容。
    crate::events::audit("config_export", "已导出整站配置", json!({
        "target_kind": "config", "target_name": "vpnmgr-export",
        "channel_count": channels.len(),
        "channels": channels.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        "result": "ok"
    }));
    Json(json!({
        "kind": "vpnmgr-export",
        "version": 1,
        "exported_at": chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string(),
        "channels": out,
    }))
    .into_response()
}

/// POST /api/config/import:吃一份 config_export 文档,逐条重建通道(新 id/mac/vnc)+ 规则。
/// **不起容器、不 provision、不 oss_connect**(有意):同步起 N 个容器要几分钟,缺镜像会整批失败;
/// 「启动」按钮本就是重建原语,导入后用户按需逐个启动。故所有导入的通道落 status="stopped"。
fn import_text(entry: &Value, key: &'static str, default: &str) -> Result<String, &'static str> {
    match entry.get(key) {
        None | Some(Value::Null) => Ok(default.to_string()),
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(key),
    }
}

pub async fn config_import(State(st): State<AppState>, Json(b): Json<Value>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let entries = match (
        b.get("kind").and_then(|v| v.as_str()),
        b.get("channels").and_then(|v| v.as_array()),
    ) {
        (Some("vpnmgr-export"), Some(arr)) => arr.clone(),
        _ => return (StatusCode::BAD_REQUEST, Json(json!({ "error": "不是有效的配置导出文件" }))).into_response(),
    };
    let mut names: std::collections::HashSet<String> = match store::list_channels(&db) {
        Ok(channels) => channels.into_iter().map(|c| c.name).collect(),
        Err(e) => return err500(&format!("list_channels: {e}")),
    };
    let mut imported: Vec<String> = Vec::new();
    let mut skipped: Vec<Value> = Vec::new();
    let mut plans: Vec<store::ImportChannel> = Vec::new();
    for entry in &entries {
        if !entry.is_object() {
            skipped.push(json!({ "name": "", "reason": "条目格式错误" }));
            continue;
        }
        let display_name = entry.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();

        let empty_rules = Vec::new();
        let (raw_rules, rules_shape_ok) = match entry.get("rules") {
            None | Some(Value::Null) => (&empty_rules, true),
            Some(Value::Array(rules)) => (rules, true),
            _ => {
                skipped.push(json!({ "name": display_name, "reason": "规则列表格式错误" }));
                (&empty_rules, false)
            }
        };
        let mut planned_rules = Vec::new();
        for (index, rule) in raw_rules.iter().enumerate() {
            let Some(obj) = rule.as_object() else {
                skipped.push(json!({ "name": display_name, "reason": format!("规则 #{} 格式错误", index + 1) }));
                continue;
            };
            let kind = obj.get("kind").and_then(|v| v.as_str());
            let pattern = obj.get("pattern").and_then(|v| v.as_str());
            let (Some(kind), Some(pattern)) = (kind, pattern) else {
                skipped.push(json!({ "name": display_name, "reason": format!("非法规则 {}", obj.get("pattern").unwrap_or(&Value::Null)) }));
                continue;
            };
            if kind != "domain" && kind != "ip" {
                skipped.push(json!({ "name": display_name, "reason": format!("非法规则 {pattern}") }));
                continue;
            }
            let enabled = match obj.get("enabled") {
                None => true,
                Some(Value::Bool(v)) => *v,
                Some(Value::Number(v)) if v.as_i64().is_some() => v.as_i64().unwrap_or(1) != 0,
                _ => {
                    skipped.push(json!({ "name": display_name, "reason": format!("规则 enabled 类型错误 {pattern}") }));
                    continue;
                }
            };
            let note = match obj.get("note") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(value)) => value.clone(),
                _ => {
                    skipped.push(json!({ "name": display_name, "reason": format!("规则 note 类型错误 {pattern}") }));
                    continue;
                }
            };
            let locked = match obj.get("locked") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                Some(Value::Number(value)) if value.as_i64().is_some() => value.as_i64().unwrap_or(0) != 0,
                _ => {
                    skipped.push(json!({ "name": display_name, "reason": format!("规则 locked 类型错误 {pattern}") }));
                    continue;
                }
            };
            let Some((kind, pattern)) = webutil::normalize_stored_rule(kind, pattern) else {
                skipped.push(json!({ "name": display_name, "reason": format!("非法规则 {pattern}") }));
                continue;
            };
            planned_rules.push(store::ImportRule { kind, pattern, enabled, note, locked });
        }

        type ImportTextFields = (String, String, String, String, String, String, String);
        let text = (|| -> Result<ImportTextFields, &'static str> {
            Ok((
                import_text(entry, "name", "")?,
                import_text(entry, "vpn_type", "")?,
                import_text(entry, "server", "")?,
                import_text(entry, "ec_ver", "")?,
                import_text(entry, "login_method", "interactive")?,
                import_text(entry, "username", "")?,
                import_text(entry, "probe_url", "")?,
            ))
        })();
        let (name, vtype, top_server, ec_ver, login_method, top_username, probe_url) = match text {
            Ok(fields) => fields,
            Err(field) => {
                skipped.push(json!({ "name": display_name, "reason": format!("字段 {field} 类型错误") }));
                continue;
            }
        };
        let cfg_in = match entry.get("config") {
            None | Some(Value::Null) => serde_json::Map::new(),
            Some(Value::Object(map)) => map.clone(),
            _ => {
                skipped.push(json!({ "name": display_name, "reason": "config 格式错误" }));
                continue;
            }
        };
        if !rules_shape_ok {
            continue;
        }
        let routing_enabled = match entry.get("routing_enabled") {
            None | Some(Value::Null) => true,
            Some(Value::Bool(value)) => *value,
            Some(Value::Number(value)) if value.as_i64().is_some() => value.as_i64().unwrap_or(1) != 0,
            _ => {
                skipped.push(json!({ "name": display_name, "reason": "字段 routing_enabled 类型错误" }));
                continue;
            }
        };
        let name = name.trim().to_string();
        if registry::get(&vtype).is_err() {
            skipped.push(json!({ "name": name, "reason": format!("未知类型 {vtype}") }));
            continue;
        }
        if names.contains(&name) {
            skipped.push(json!({ "name": name, "reason": "同名通道已存在" }));
            continue;
        }
        let server = {
            if top_server.is_empty() { cfg_in.get("server").and_then(|v| v.as_str()).unwrap_or("").into() } else { top_server }
        };
        let username = {
            if top_username.is_empty() { cfg_in.get("username").and_then(|v| v.as_str()).unwrap_or("").into() } else { top_username }
        };
        let cid = rand_hex(4);
        let imported_name = if name.is_empty() { cid.clone() } else { name.clone() };
        let nc = NewChannel {
            id: cid.clone(),
            name: imported_name.clone(),
            vpn_type: vtype.clone(),
            server,
            ec_ver,
            login_method: if login_method.is_empty() { "interactive".into() } else { login_method },
            username,
            password: String::new(), // 交互密码不在文件里,导入后重新登录
            vnc_password: rand_hex(4),
            mac: rand_mac(),
            probe_url,
            status: "stopped".into(),
            routing_enabled,
        };
        let mut sk = secret_keys_of(&vtype);
        sk.push("login_note".into()); // 登录备注不在 manifest inputs 里,导入时同样加密落库
        plans.push(store::ImportChannel { channel: nc, config: cfg_in, secret_keys: sk, rules: planned_rules });
        names.insert(imported_name.clone());
        imported.push(imported_name);
    }
    let key = match store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    if let Err(e) = store::import_channels(&db, &key, &plans) {
        crate::events::audit_failed("config_import", "导入配置失败", json!({
            "target_kind": "config", "target_name": "vpnmgr-export",
            "before": null, "after": null, "planned": imported, "skipped": skipped,
            "result": "failed", "error": e.to_string()
        }));
        return err500(&format!("config import: {e}"));
    }
    let reload = manager::rebuild(&st.cfg, st.docker().as_ref(), &db).await;
    // after 只列导入了哪些通道名(全部落 stopped、新 id/MAC/VNC 密码),凭据不进日志。
    crate::events::audit("config_import", "已导入配置", json!({
        "target_kind": "config", "target_name": "vpnmgr-export",
        "before": null,
        "after": { "imported": imported.clone(), "status": "stopped" },
        "imported_count": imported.len(), "skipped": skipped.clone(),
        "reload_status": reload.as_str(), "result": "ok"
    }));
    Json(json!({
        "ok": true,
        "reload_status": reload,
        "imported": imported,
        "skipped": skipped,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn detached_operation_finishes_after_request_is_cancelled() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let request = tokio::spawn(detached("test", async move {
            let _ = started_tx.send(());
            finish_rx.await.unwrap();
            let _ = done_tx.send(());
            Json(json!({"ok": true})).into_response()
        }));
        started_rx.await.unwrap();
        request.abort();
        assert!(request.await.unwrap_err().is_cancelled());
        finish_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), done_rx).await.unwrap().unwrap();
    }

    #[test]
    fn reload_ok_judges_put_file_failure_as_failure() {
        // D:put_file 失败时 rebuild 返回 "put_file failed: ..."(无裸 2xx),reload_ok 判 false;正常 2xx 判成功。
        // 终点:put_file 失败 → 端点不返回成功语义、前端不显示绑定成功(此前 "204 (put_file failed:...)" 被误判)。
        assert!(reload_ok("204"));
        assert!(reload_ok("200"));
        assert!(!reload_ok("204 (put_file failed: stale config)"));
        assert!(!reload_ok("put_file failed: error connecting to /nonexistent.sock"));
        assert!(!reload_ok("config read error: bad yaml"));
        assert!(!reload_ok("500"));
        assert!(!reload_ok(""));
    }

    #[test]
    fn valid_mirror_host_rejects_userinfo_and_danger() {
        // E:堵 userinfo '@' 与 DANGER(逗号/空白/换行/引号/反斜杠);允许 host:port/path 形态。
        assert!(valid_mirror_host("mirror.example.com"));
        assert!(valid_mirror_host("192.168.1.10:5000"));
        assert!(valid_mirror_host("host.io:5000/prefix"));
        assert!(!valid_mirror_host("user@127.0.0.1:8443/admin"));
        assert!(!valid_mirror_host("a,b"));
        assert!(!valid_mirror_host("a b"));
        assert!(!valid_mirror_host("a\nb"));
        assert!(!valid_mirror_host("host/path?query=1"));
        assert!(!valid_mirror_host("host/path#fragment"));
        assert!(!valid_mirror_host("镜像.example"));
        assert!(!valid_mirror_host(""));
    }
}
