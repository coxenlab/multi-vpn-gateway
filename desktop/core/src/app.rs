//! in-process 启动入口:bin(`main.rs`)与 Tauri 壳共用一套引导逻辑,零重复。
//!
//! `bootstrap` 做 `store::init` + master key + docker 连接(可选)+ 组 [`AppState`] +
//! 绑定 `127.0.0.1:ui_port`(命门 #4),返回**已绑定**的 listener——端口此刻已占好,
//! Tauri webview 可立即连过去而不会 connection-refused。`serve` 在该 listener 上跑 axum。

use std::sync::Arc;

use crate::{config::Config, docker, mihomo::Controller, store, AppState};

/// 引导:初始化库、连接 docker(可选)、绑定 127.0.0.1:ui_port。
///
/// 返回已绑定 listener 与组好的 [`AppState`]。docker 连不上不致命——照常伺服
/// (uptime 降级,UI/system 仍可用),与原 bin 行为一字不差。
pub async fn bootstrap(cfg: Config) -> anyhow::Result<(tokio::net::TcpListener, AppState)> {
    cfg.validate()?;
    crate::events::init(&cfg.data_dir);
    store::init(&cfg.db_path())?;
    let _ = store::master_key(&cfg.data_dir)?; // 确保 master key(真实 data_dir = 复用现有,零迁移)

    // docker 可选:连不上也照常伺服(uptime 降级,UI/system 仍工作)
    let socket = cfg.docker_socket().display().to_string();
    let docker = if cfg.managed_vm { None } else { match docker::connect_at(&socket).await {
        Ok(d) => {
            eprintln!("docker: connected via {socket}");
            Some(d)
        }
        Err(e) => {
            eprintln!("docker: not connected ({e}); uptime degraded — start colima for full data");
            None
        }
    }};

    // 命门 #4:只绑 127.0.0.1。持久化的 ui_port 被别的进程占了(AddrInUse)时重摇一个
    // 空闲口回写 infra.json 再绑(红队 F6:否则「重试」永远撞同一个口,且报错被归到 Docker 步)。
    let mut cfg = cfg;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], cfg.ui_port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            let new_port = crate::infra::reroll_ui_port(&cfg.data_dir)?;
            crate::ev!(warn, "boot", "ui_port_rerolled", "UI 端口被占,已重摇并持久化",
                { "old": cfg.ui_port, "new": new_port });
            cfg.ui_port = new_port;
            std::env::set_var("UI_PORT", new_port.to_string());
            tokio::net::TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], new_port))).await?
        }
        Err(e) => return Err(e.into()),
    };

    let mihomo = Controller::new(cfg.mihomo_ctrl_url.clone(), cfg.mihomo_secret.clone());
    let state = AppState {
        lifecycle: Default::default(),
        cfg: Arc::new(cfg),
        docker: Arc::new(std::sync::RwLock::new(docker)),
        mihomo,
        health: crate::health::shared(),
        tunnel: crate::tunnel::handle(),
        novnc: crate::novnc::handle(),
        self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    Ok((listener, state))
}

/// 在已绑定 listener 上跑 axum 直到关闭。起头 spawn 分流口健康看门狗(bin 与 Tauri 壳共用此入口)。
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    if !state.cfg.managed_vm {
        crate::stop_intents::recover_all(&state).await?;
        crate::replacement::recover_all(&state).await;
    }
    crate::health::spawn(state.clone());
    crate::runtime::spawn_idle(state.clone());
    let app = crate::server::build_router(state);
    axum::serve(listener, app).await?;
    Ok(())
}
