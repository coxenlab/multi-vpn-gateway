use std::sync::Arc;
use vpnmgr_core::{config::Config, docker, mihomo::Controller, server, store, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    store::init(&cfg.db_path())?;
    let _ = store::master_key(&cfg.data_dir)?; // ensure master key (real data dir = reuse existing, zero-migration)

    // docker optional: if it can't connect, still serve (uptime degraded, UI/system still work)
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

    let mihomo = Controller::new(cfg.mihomo_ctrl_url.clone(), cfg.mihomo_secret.clone());
    let ui_port = cfg.ui_port;
    let state = AppState { cfg: Arc::new(cfg), docker, mihomo };

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], ui_port)); // 命门 #4
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!("vpnmgr-core listening on http://{addr}");
    let app = server::build_router(state);
    axum::serve(listener, app).await?;
    Ok(())
}
