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
    store::init(&cfg.db_path())?;
    let _ = store::master_key(&cfg.data_dir)?; // 确保 master key(真实 data_dir = 复用现有,零迁移)

    // docker 可选:连不上也照常伺服(uptime 降级,UI/system 仍工作)
    let docker = match docker::connect().await {
        Ok(d) => {
            eprintln!("docker: connected via {}", docker::docker_socket());
            Some(d)
        }
        Err(e) => {
            eprintln!("docker: not connected ({e}); uptime degraded — start colima for full data");
            None
        }
    };

    let ui_port = cfg.ui_port;
    let mihomo = Controller::new(cfg.mihomo_ctrl_url.clone(), cfg.mihomo_secret.clone());
    let state = AppState {
        cfg: Arc::new(cfg),
        docker: Arc::new(std::sync::RwLock::new(docker)),
        mihomo,
        health: crate::health::shared(),
    };

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ui_port)); // 命门 #4
    let listener = tokio::net::TcpListener::bind(addr).await?;
    Ok((listener, state))
}

/// 在已绑定 listener 上跑 axum 直到关闭。起头 spawn 分流口健康看门狗(bin 与 Tauri 壳共用此入口)。
pub async fn serve(listener: tokio::net::TcpListener, state: AppState) -> anyhow::Result<()> {
    crate::health::spawn(state.clone());
    let app = crate::server::build_router(state);
    axum::serve(listener, app).await?;
    Ok(())
}
