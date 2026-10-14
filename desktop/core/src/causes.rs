//! 出错原因:只用现有判据,按顺序取第一个成立的。句子由界面按 `code` 取,核心不出中文句子。
//! 证书过期、账号密码错误没有真实日志样本,不写规则,一律落到 `unknown`。
//! 运行日志被用户关掉时事件里没有创建失败的记录,`image_pull_failed` 也落到 `unknown`。
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::Json;
use serde::Serialize;
use serde_json::Value;
use std::{collections::HashMap, path::PathBuf, sync::{LazyLock, Mutex}, time::{Duration, Instant}};
use crate::events::Event;
use crate::AppState;

/// 会重建通道的三种操作;最近一条是拉取镜像失败才算「客户端下载失败」。
const PROVISION_ACTIONS: [&str; 3] = ["channel_create", "channel_start", "channel_update"];
/// 拉取失败的错误文本前缀,见 [`crate::manager`] 的 `ensure_image_mirrored`。
const PULL_FAILED: &str = "拉取镜像";

#[derive(Clone, Serialize, Debug, PartialEq, Eq)]
pub struct Cause {
    /// `runtime_down` / `image_pull_failed` / `crash_loop` / `kicked` / `unknown`;已停止为 null。
    pub code: Option<&'static str>,
    /// `restart_runtime` / `retry` / `relogin` / `view_logs`。
    pub action: Option<&'static str>,
    /// 原因发生的时间(毫秒),算不出来时为 null。
    pub at: Option<i64>,
}

impl Cause {
    fn of(code: &'static str, action: &'static str, at: Option<i64>) -> Self {
        Self { code: Some(code), action: Some(action), at }
    }
}

type CacheKey = (PathBuf, String, String, Option<String>);
static CACHE: LazyLock<Mutex<HashMap<CacheKey, (Instant, Cause)>>> = LazyLock::new(Default::default);
const CACHE_TTL: Duration = Duration::from_secs(30);

fn cached(key: &CacheKey) -> Option<Cause> {
    let mut cache = CACHE.lock().unwrap();
    cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    cache.get(key).map(|(_, value)| value.clone())
}

/// 该通道最近一条创建、启动、更新审计是拉取镜像失败时,返回它的时间。
/// 之后又有成功的同类操作就不算(旧失败不能盖住现在的原因)。
pub fn image_pull_failed_at(cid: &str, audits: &[Event]) -> Option<i64> {
    let last = audits.iter().rev().find(|e| {
        e.src == "audit"
            && PROVISION_ACTIONS.contains(&e.event.as_str())
            && e.detail.get("target_id").and_then(Value::as_str) == Some(cid)
    })?;
    let failed = last.detail.get("result").and_then(Value::as_str) == Some("failed");
    let error = last.detail.get("error").and_then(Value::as_str).unwrap_or_default();
    (failed && error.contains(PULL_FAILED)).then_some(last.ts_ms as i64)
}

/// 纯函数:按 DESIGN 5.6 的顺序判断。`diag` 是 [`crate::diag::diag_one`] 的结果,拿不到时为 None。
pub fn judge(stopped: bool, serving: bool, pull_failed_at: Option<i64>, diag: Option<&Value>, now_ms: i64) -> Cause {
    if stopped {
        return Cause { code: None, action: None, at: None };
    }
    if !serving {
        return Cause::of("runtime_down", "restart_runtime", None);
    }
    if let Some(at) = pull_failed_at {
        return Cause::of("image_pull_failed", "retry", Some(at));
    }
    if diag.and_then(|d| d.get("crash_loop")).and_then(Value::as_bool) == Some(true) {
        return Cause::of("crash_loop", "retry", None);
    }
    if let Some(age) = diag.and_then(|d| d.get("ec_kick_age")).and_then(Value::as_i64) {
        return Cause::of("kicked", "relogin", Some(now_ms - age * 1000));
    }
    Cause::of("unknown", "view_logs", None)
}

/// `GET /api/channels/:cid/cause`。只读:不取通道操作锁,运行环境没起来时不碰 docker。
pub async fn cause(State(st): State<AppState>, Path(cid): Path<String>) -> axum::response::Response {
    let db = st.cfg.db_path();
    let ch = match crate::store::get_channel(&db, &cid) {
        Ok(Some(c)) => c,
        Ok(None) => return crate::api::err404("not found"),
        Err(e) => return crate::api::err_detail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    // 停止意图中的通道在 /api/channels 里显示为已停止,这里同样不给原因。
    let stopped = match crate::stop_intents::pending(&db, &cid) {
        Ok(pending) => pending || ch.status == "stopped",
        Err(e) => return crate::api::err_detail(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let serving = crate::runtime::serving(&st);
    let now_ms = chrono::Utc::now().timestamp_millis();
    if stopped || !serving { return Json(judge(stopped, serving, None, None, now_ms)).into_response(); }
    let filter = crate::events::Filter { src: Some("audit".into()), ..Default::default() };
    let (audits, _) = crate::events::snapshot(0, usize::MAX, &filter);
    let pull_failed_at = image_pull_failed_at(&cid, &audits);
    if pull_failed_at.is_some() { return Json(judge(false, true, pull_failed_at, None, now_ms)).into_response(); }
    let docker = st.docker();
    let snapshot = crate::docker::channel_snapshot(docker.as_ref()).await.ok();
    if crate::docker::observation(snapshot.as_ref(), &cid).state == crate::docker::ContainerPresence::Unknown {
        return Json(Cause::of("runtime_unknown", "view_logs", None)).into_response();
    }
    let key = (db, cid, ch.status.clone(), ch.container_id.clone());
    if let Some(value) = cached(&key) { return Json(value).into_response(); }
    let diag = match docker {
        Some(d) => tokio::time::timeout(Duration::from_secs(6), crate::diag::diag_one(&d, &ch)).await.ok(),
        None => None,
    };
    let value = judge(false, true, None, diag.as_ref(), now_ms);
    CACHE.lock().unwrap().insert(key, (Instant::now(), value.clone()));
    Json(value).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_cache_expires_and_does_not_cross_channel_or_container_identity() {
        let root = tempfile::tempdir().unwrap();
        let key = (root.path().join("db"), "a".into(), "error".into(), Some("container-a".into()));
        let value = Cause::of("crash_loop", "retry", None);
        CACHE.lock().unwrap().insert(key.clone(), (Instant::now(), value.clone()));
        assert_eq!(cached(&key), Some(value.clone()));
        let mut replaced = key.clone(); replaced.3 = Some("container-b".into());
        assert_eq!(cached(&replaced), None);
        CACHE.lock().unwrap().insert(key.clone(), (Instant::now() - CACHE_TTL, value));
        assert_eq!(cached(&key), None);
    }

    fn audit(seq: u64, action: &str, cid: &str, result: &str, error: Option<&str>) -> Event {
        let mut detail = json!({"target_kind": "channel", "target_id": cid, "result": result});
        if let Some(error) = error {
            detail["error"] = json!(error);
        }
        Event {
            seq,
            ts: String::new(),
            ts_ms: 1_000 * seq,
            level: if result == "failed" { crate::events::Level::Error } else { crate::events::Level::Info },
            src: "audit".into(),
            event: action.into(),
            msg: String::new(),
            detail,
        }
    }

    /// `diag_one` 在容器运行时的返回形状。
    fn diag(crash_loop: bool, ec_kick_age: Option<i64>) -> Value {
        json!({
            "id": "c1", "state": "running", "restart_count": 0, "uptime_secs": 30, "exit_code": null,
            "crash_loop": crash_loop, "tunnel_up": null, "tunnel_iface": null, "wg_handshake_age": null,
            "ec_client_alive": null, "ec_kick_age": ec_kick_age,
        })
    }

    const PULL: &str = "拉取镜像 vpnmgr/oss-vpn:latest 失败(所有镜像源):mirror.example.test: timeout";

    #[test]
    fn image_pull_failure_is_the_latest_provision_audit_of_this_channel() {
        let events = vec![
            audit(1, "channel_create", "c1", "failed", Some(PULL)),
            audit(2, "channel_start", "c2", "ok", None),
            audit(3, "note_update", "c1", "ok", None),
        ];
        assert_eq!(image_pull_failed_at("c1", &events), Some(1_000));
        // 别的通道、别的原因、别的操作都不算。
        assert_eq!(image_pull_failed_at("c2", &events), None);
        let other = vec![audit(1, "channel_start", "c1", "failed", Some("docker unavailable"))];
        assert_eq!(image_pull_failed_at("c1", &other), None);
        let unrelated = vec![audit(1, "channel_upload", "c1", "failed", Some(PULL))];
        assert_eq!(image_pull_failed_at("c1", &unrelated), None);
        // 之后启动成功过,旧失败不再算。
        let recovered = vec![audit(1, "channel_update", "c1", "failed", Some(PULL)), audit(2, "channel_start", "c1", "ok", None)];
        assert_eq!(image_pull_failed_at("c1", &recovered), None);
        // 运行日志关掉时没有事件。
        assert_eq!(image_pull_failed_at("c1", &[]), None);
    }

    #[test]
    fn start_pull_failure_is_judged_as_image_pull_failed() {
        // 从旧应用导入的通道没有创建审计、也没有容器,第一次启动时下载失败。
        let missing = json!({"id": "c1", "state": "missing"});
        let events = vec![audit(1, "channel_start", "c1", "failed", Some(PULL))];
        let at = image_pull_failed_at("c1", &events);
        assert_eq!(at, Some(1_000));
        assert_eq!(judge(false, true, at, Some(&missing), 5_000), Cause::of("image_pull_failed", "retry", Some(1_000)));
        // 创建成功过,之后点重试时下载失败。
        let events = vec![audit(1, "channel_create", "c1", "ok", None), audit(2, "channel_start", "c1", "failed", Some(PULL))];
        assert_eq!(image_pull_failed_at("c1", &events), Some(2_000));
    }

    #[test]
    fn causes_follow_the_design_order() {
        let now = 10_000_000;
        let crash = diag(true, None);
        let kicked = diag(false, Some(120));
        let quiet = diag(false, None);
        // 已停止:三个字段都为 null。
        assert_eq!(judge(true, false, Some(1), Some(&crash), now), Cause { code: None, action: None, at: None });
        // 运行环境没起来排在最前。
        assert_eq!(judge(false, false, Some(1), Some(&crash), now), Cause::of("runtime_down", "restart_runtime", None));
        assert_eq!(judge(false, true, Some(5_000), Some(&crash), now), Cause::of("image_pull_failed", "retry", Some(5_000)));
        assert_eq!(judge(false, true, None, Some(&crash), now), Cause::of("crash_loop", "retry", None));
        assert_eq!(judge(false, true, None, Some(&json!({"state": "restarting", "crash_loop": true})), now).code, Some("crash_loop"));
        assert_eq!(judge(false, true, None, Some(&kicked), now), Cause::of("kicked", "relogin", Some(now - 120_000)));
        // 踢线和崩溃同时成立时按顺序取崩溃。
        assert_eq!(judge(false, true, None, Some(&diag(true, Some(3))), now).code, Some("crash_loop"));
        // 认不出来的一律 unknown;容器不在、诊断超时、没有 docker 同样。
        for d in [Some(quiet), Some(json!({"id": "c1", "state": "missing"})), None] {
            assert_eq!(judge(false, true, None, d.as_ref(), now), Cause::of("unknown", "view_logs", None));
        }
        let body = serde_json::to_value(judge(false, true, None, None, now)).unwrap();
        assert_eq!(body, json!({"code": "unknown", "action": "view_logs", "at": null}));
    }
}
