//! 桌面按需启动入口。UI 服务与 VM 独立，所有显式连接意图共享一轮底座初始化。
use anyhow::{anyhow, Result};
use crate::{docker, infra, manager, vm, AppState};
use crate::runtime_lifecycle::Phase;

pub const IDLE_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// 写请求完整执行期间保有许可，浏览器断开不使已经接受的资源变更失去保护。
/// preflight 的 GET 也会创建临时探针容器；普通状态轮询不续期空闲等待。
pub async fn track_request(
    axum::extract::State(state): axum::extract::State<AppState>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let read_only = matches!(*request.method(), axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS);
    if read_only && request.uri().path() != "/api/preflight" { return next.run(request).await; }
    let activity = if read_only {
        match state.lifecycle.runtime().maintenance() {
            Some(activity) => activity, None => return crate::api::err503("运行环境正在释放，请稍后重试"),
        }
    } else {
        match state.lifecycle.runtime().activity().await {
            Ok(activity) => activity, Err(error) => return crate::api::err503(&error),
        }
    };
    match tokio::spawn(async move { let _activity = activity; next.run(request).await }).await {
        Ok(response) => response,
        Err(_) => crate::api::err500("操作异常退出，请读取当前状态后重试"),
    }
}

/// 配置意图、持久恢复记录、登录租约和实际容器都必须同意释放。
/// 未识别的运行中容器也阻止释放，避免停掉同一 profile 内额外工作。
pub async fn can_release(state: &AppState) -> Result<bool> {
    let db = state.cfg.db_path();
    if crate::store::list_channels(&db)?.iter().any(|channel| channel.status != "stopped")
        || crate::replacement_store::has_active_replacements(&db)?
        || crate::stop_intents::any(&db)?
        || crate::config_apply_store::status(&db, crate::store::routing_off(&state.cfg.data_dir))?.pending
        || crate::novnc::has_viewers(state).await {
        return Ok(false);
    }
    let docker = state.docker().ok_or_else(|| anyhow!("容器状态不可确认，保留运行环境"))?;
    let containers = tokio::time::timeout(std::time::Duration::from_secs(5),
        docker.list_containers(None::<bollard::container::ListContainersOptions<String>>)).await??;
    Ok(containers.iter().all(|container| container.names.as_ref().is_some_and(|names|
        names.len() == 1 && names[0] == "/mihomo")))
}

async fn release(state: &AppState) -> Result<()> {
    // 已进入 Releasing，应用内新操作只能等待；再读一次避免慢检查后的外部容器变化。
    anyhow::ensure!(can_release(state).await?, "运行环境仍在使用，已取消释放");
    state.lifecycle.runtime().progress("正在释放空闲运行环境…");
    crate::entry::park_for_idle(&state.cfg).await?;
    crate::novnc::drop_all(state).await?;
    crate::tunnel::kill_confirmed(state).await?;
    // 停止命令结果不明时只读回，不重复发送 stop。
    let result = vm::stop(&state.cfg.vm_profile).await;
    anyhow::ensure!(vm::stopped_confirmed(&state.cfg.vm_profile).await?,
        "运行环境停止尚未确认: {:?}", result);
    state.set_docker(None);
    state.lifecycle.runtime().progress("空闲运行环境已释放，连接时会自动启动");
    crate::ev!(info, "runtime", "runtime_released", "空闲运行环境已释放", {"profile":state.cfg.vm_profile});
    Ok(())
}

pub fn spawn_idle(state: AppState) {
    if !state.cfg.managed_vm { return; }
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if state.lifecycle.runtime().snapshot().phase == Phase::Closing { break; }
            let owned = state.clone();
            if let Err(error) = state.lifecycle.runtime().release_if_idle(IDLE_GRACE,
                || async { can_release(&state).await.map_err(|e| e.to_string()) },
                move || async move { release(&owned).await.map_err(|e| e.to_string()) }).await {
                crate::ev!(warn, "runtime", "idle_release_unconfirmed", "空闲释放未确认，保留现场", {"error":error});
            }
        }
    });
}

pub fn serving(state: &AppState) -> bool {
    !state.cfg.managed_vm || matches!(state.lifecycle.runtime().snapshot().phase, Phase::Ready | Phase::Waiting)
}

/// 必须在获取通道操作锁之前调用；启动恢复会逐通道获取同一把锁。
pub async fn ensure(state: &AppState) -> Result<()> {
    if !state.cfg.managed_vm { return Ok(()); }
    let _activity = state.lifecycle.runtime().activity().await.map_err(anyhow::Error::msg)?;
    if serving(state) && vm::status(&state.cfg.vm_profile).await != vm::VmStatus::Running {
        state.lifecycle.runtime().unavailable("运行环境不可用，正在重新连接");
    }
    let owned = state.clone();
    state.lifecycle.runtime().ensure(move || async move {
        start(&owned).await.map_err(|error| error.to_string())
    }).await.map_err(anyhow::Error::msg)
}

pub async fn start_route(axum::extract::State(state): axum::extract::State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match ensure(&state).await {
        Ok(()) => axum::Json(serde_json::json!({"ok":true})).into_response(),
        Err(error) => crate::api::err_detail(axum::http::StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

async fn start(state: &AppState) -> Result<()> {
    let cfg = &state.cfg;
    cfg.validate()?;
    let progress = |detail: &str| state.lifecycle.runtime().progress(detail);
    progress("检查运行环境…");
    let mut rosetta = vm::rosetta_available().await;
    if vm::host_needs_rosetta() && !rosetta {
        progress("缺少 Rosetta 2，等待你的选择…");
        if vm::prompt_rosetta_install().await? {
            progress("正在安装 Rosetta 2，请在系统窗口中授权…");
            rosetta = vm::install_rosetta().await?;
        }
        if !rosetta {
            crate::ev!(warn, "runtime", "rosetta_skipped", "已跳过 Rosetta 2，x86 镜像暂不可用", {});
        }
    }
    if vm::status(&cfg.vm_profile).await != vm::VmStatus::Running {
        if !cfg.dev_mode {
            progress("检查内置运行环境镜像…");
            if let Err(error) = crate::vm_image_cache::seed_bundled().await {
                crate::ev!(warn, "runtime", "vm_image_cache_failed", "内置运行环境镜像准备失败，将尝试在线下载", {"error":error.to_string()});
            }
        }
        progress("正在启动运行环境，首次准备可能需要下载组件…");
        vm::start_with_progress(&cfg.vm_profile, rosetta, |detail| {
            crate::ev!(debug, "runtime", "vm_start_progress", "运行环境启动进度", {"detail":detail});
        }).await?;
    }
    progress("等待容器引擎…");
    // 已运行 VM 的管理链路失败不能据此重启全部通道；保留现场供显式重试/诊断。
    vm::wait_docker_ready(&cfg.vm_profile, 40).await?;
    let connection = docker::connect_at(&cfg.docker_socket().display().to_string()).await?;
    state.set_docker(Some(connection.clone()));
    docker::create_bridge_network(&connection, &cfg.vpn_net).await?;
    if !crate::health::ensure_egress_guard(state.clone(), true).await {
        return Err(anyhow!("基础网络防护尚未就绪，请重试"));
    }
    progress("确认已停用的通道…");
    crate::stop_intents::recover_all(state).await?;
    if let Some(images) = cfg.bundled_images_dir.as_ref().filter(|path| path.is_dir()) {
        progress("检查内置 VPN 镜像…");
        if let Err(error) = infra::ensure_bundled_images(&connection, images).await {
            crate::ev!(warn, "runtime", "bundled_images_failed", "内置镜像载入未完成，可在环境检查中重试", {"error":error.to_string()});
        }
    }
    progress("检查分流组件…");
    infra::ensure_mihomo_image_with_progress(&connection, cfg, |detail| {
        crate::ev!(debug, "runtime", "image_progress", "分流组件准备进度", {"detail":detail});
    }).await?;
    infra::ensure_mihomo(&connection, cfg).await?;
    progress("建立本地分流入口…");
    crate::tunnel::ensure(state).await?;
    infra::wait_mihomo_ctrl(cfg).await?;
    progress("恢复操作进度并同步规则…");
    crate::replacement::recover_all(state).await;
    let outcome = manager::rebuild(cfg, Some(&connection), &cfg.db_path()).await;
    if !outcome.parse::<u16>().ok().is_some_and(|status| (200..300).contains(&status)) {
        crate::ev!(warn, "runtime", "rules_pending", "运行环境已启动，规则仍待同步", {"error":outcome});
    }
    if !crate::config_apply_store::status(&cfg.db_path(), crate::store::routing_off(&cfg.data_dir))?.pending {
        crate::entry::resume_after_idle(cfg).await?;
    }
    progress("运行环境已就绪");
    crate::ev!(info, "runtime", "runtime_ready", "按需运行环境已就绪", {"profile":cfg.vm_profile});
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn confirm_rules(state: &AppState) {
        let db = state.cfg.db_path();
        let snapshot = crate::config_apply_store::snapshot(&db, false).unwrap();
        let ticket = crate::config_apply_store::prepare(&db, snapshot.revision, false, &"a".repeat(64)).unwrap();
        crate::config_apply_store::confirmed(&db, &ticket).unwrap();
    }

    #[tokio::test]
    async fn cancelled_http_write_keeps_activity_until_handler_finishes_and_reads_do_not_renew_idle() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.ui_port = 0; cfg.dev_mode = true;
        cfg.managed_vm = true; cfg.vm_profile = "vpnmgr-test".into();
        let (_ui, state) = crate::app::bootstrap(cfg).await.unwrap();
        let coordinator = state.lifecycle.runtime().clone();
        coordinator.ensure(|| async { Ok(()) }).await.unwrap();
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let finish = Arc::new(tokio::sync::Semaphore::new(0));
        let (entry, gate) = (entered.clone(), finish.clone());
        let router = axum::Router::new().route("/write", axum::routing::post(move || {
            let (entry, gate) = (entry.clone(), gate.clone());
            async move { entry.add_permits(1); gate.acquire().await.unwrap().forget(); "ok" }
        })).route("/read", axum::routing::get(|| async { "ok" }))
            .route_layer(axum::middleware::from_fn_with_state(state, track_request));
        let request = { let router = router.clone(); tokio::spawn(async move {
            router.oneshot(axum::http::Request::builder().method("POST").uri("/write")
                .body(axum::body::Body::empty()).unwrap()).await
        }) };
        entered.acquire().await.unwrap().forget(); request.abort(); let _ = request.await;
        assert_eq!(coordinator.snapshot().active_tasks, 1);
        assert!(!coordinator.release_if_idle(std::time::Duration::ZERO,
            || async { panic!("write still owns runtime") }, || async { Ok(()) }).await.unwrap());
        finish.add_permits(1);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while coordinator.snapshot().active_tasks != 0 { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(!coordinator.release_if_idle(IDLE_GRACE, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        let response = router.oneshot(axum::http::Request::builder().uri("/read")
            .body(axum::body::Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(coordinator.snapshot().phase, Phase::Waiting);
    }

    #[tokio::test]
    async fn idle_needs_explicit_stopped_intent_no_pending_work_and_actual_container_inventory() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.ui_port = 0; cfg.dev_mode = true;
        cfg.managed_vm = true; cfg.vm_profile = "vpnmgr-test".into();
        let (_ui, state) = crate::app::bootstrap(cfg).await.unwrap();
        let contents = Arc::new(Mutex::new(json!([{"Id":"infra","Names":["/mihomo"]}])));
        let response = contents.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let docker = bollard::Docker::connect_with_http(&format!("http://{}", listener.local_addr().unwrap()),
            2, bollard::API_DEFAULT_VERSION).unwrap();
        let router = axum::Router::new().fallback(move || {
            let response = response.clone(); async move { axum::Json(response.lock().unwrap().clone()) }
        });
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap(); });
        state.set_docker(Some(docker));
        assert!(!can_release(&state).await.unwrap(), "initial unconfirmed rules block release");
        confirm_rules(&state);
        assert!(can_release(&state).await.unwrap());
        let db = rusqlite::Connection::open(state.cfg.db_path()).unwrap();
        db.execute("INSERT INTO channels(id,name,status,routing_enabled) VALUES('c1','fixture','stopped',0)", []).unwrap();
        for status in ["running", "logged_in", "error", "unknown", "stopped"] {
            db.execute("UPDATE channels SET status=?1", [status]).unwrap(); confirm_rules(&state);
            assert_eq!(can_release(&state).await.unwrap(), status == "stopped",
                "routing disabled or failed probe does not replace stop intent");
        }
        db.execute("INSERT INTO channel_replacements VALUES('c1','fixture','awaiting_login','unread-fixture',0)", []).unwrap();
        assert!(!can_release(&state).await.unwrap());
        db.execute("DELETE FROM channel_replacements", []).unwrap();
        db.execute("INSERT INTO channel_stop_intents VALUES('c1','pending')", []).unwrap();
        assert!(!can_release(&state).await.unwrap());
        db.execute("DELETE FROM channel_stop_intents", []).unwrap();
        for inventory in [json!([{"Names":["/mihomo"]},{"Names":["/unrelated"]}]),
            json!([{"Names":["/vpn-c1"]}]), json!([{}])] {
            *contents.lock().unwrap() = inventory;
            assert!(!can_release(&state).await.unwrap(), "unrecognized live work must block release");
        }
        *contents.lock().unwrap() = json!([]);
        assert!(can_release(&state).await.unwrap());
        state.set_docker(None);
        assert!(can_release(&state).await.is_err(), "management failure must not imply idle");
        server.abort();
    }
}
