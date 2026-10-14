//! 换配置文件(openvpn / wireguard 的 `config_file`)。只落库,不在这里重建:
//! 界面接着调 `POST /api/channels/:cid/start`,重建时从库里读配置,新文件自然生效。
//! ⚠️ 命门 #5:内容 Fernet 加密落库,不进日志、事件、命令行,也不回传;审计只记长度和文件名。
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde_json::{json, Value};
use crate::api::{err404, err500, err503, err_detail};
use crate::AppState;

const FIELD: &str = "config_file";

/// 该类型的输入项里有没有配置文件。
pub fn accepts_config_file(spec: &crate::registry::AdapterSpec) -> bool {
    spec.inputs.iter().any(|i| i.key == FIELD)
}

/// `PUT /api/channels/:cid/config-file`,请求体 `{"content","filename"}`。
/// 脱离请求生命周期执行,和启停同理:前端跳走不会留下写了一半的状态。
pub async fn put(State(st): State<AppState>, Path(cid): Path<String>, Json(b): Json<Value>) -> axum::response::Response {
    match tokio::spawn(put_inner(st, cid, b)).await {
        Ok(r) => r,
        Err(e) => err500(&format!("config-file task: {e}")),
    }
}

async fn put_inner(st: AppState, cid: String, b: Value) -> axum::response::Response {
    let content = match b.get("content") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.as_str(),
        Some(Value::String(_)) | None | Some(Value::Null) => return err_detail(StatusCode::BAD_REQUEST, "配置文件内容为空"),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "content 须为字符串"),
    };
    let filename = match b.get("filename") {
        None | Some(Value::Null) => "",
        Some(Value::String(s)) => s.as_str(),
        Some(_) => return err_detail(StatusCode::BAD_REQUEST, "filename 须为字符串"),
    };
    let db = st.cfg.db_path();
    let ch = match crate::store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return err404("not found"),
        Err(e) => return err500(&format!("{e}")),
    };
    if !crate::registry::get(&ch.vpn_type).is_ok_and(|spec| accepts_config_file(&spec)) {
        return err_detail(StatusCode::BAD_REQUEST, "该类型没有配置文件");
    }
    let _operation = match st.lifecycle.mutate(&cid).await {
        Ok(guard) => guard,
        Err(e) => return err503(&e.to_string()),
    };
    // 判断方式同 update_inner:排队中的设置照常保存;已结束的记录先清理;其余未完成的替换 409。
    match crate::replacement_store::public_status(&db, &cid) {
        Ok(Some(pending)) if pending["phase"] == "queued" => {}
        Ok(Some(pending)) if matches!(pending["phase"].as_str(), Some("committed" | "rolled_back")) => {
            if let Err(e) = crate::replacement::cleanup(&st, &cid).await {
                return err_detail(StatusCode::CONFLICT, &format!("上次操作的资源清理未完成: {e}"));
            }
        }
        Ok(Some(_)) => return err_detail(StatusCode::CONFLICT, "上次修改尚未验证，请先完成登录或恢复上一次设置"),
        Ok(None) => {}
        Err(e) => return err500(&format!("replacement status: {e}")),
    }
    let key = match crate::store::master_key(&st.cfg.data_dir) {
        Ok(k) => k,
        Err(e) => return err500(&format!("master_key: {e}")),
    };
    let detail = |result: &str| {
        json!({
            "target_kind": "channel", "target_id": cid.as_str(), "target_name": ch.name.as_str(),
            "vpn_type": ch.vpn_type.as_str(), "filename": filename,
            "after": { "length": content.chars().count() }, "result": result,
        })
    };
    if let Err(e) = crate::store::set_config_field(&db, &key, &cid, FIELD, content, true) {
        let mut failed = detail("failed");
        failed["error"] = json!(e.to_string());
        crate::events::audit_failed("channel_config_file", "通道配置文件保存失败", failed);
        return err500(&format!("{e}"));
    }
    crate::events::audit("channel_config_file", "通道配置文件已更换，启动通道后生效", detail("ok"));
    Json(json!({ "ok": true })).into_response()
}
