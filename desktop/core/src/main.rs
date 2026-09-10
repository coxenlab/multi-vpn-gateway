use vpnmgr_core::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut cfg = Config::load();
    if cfg.managed_vm {
        cfg.validate()?;
        vpnmgr_core::infra::ensure_params(&cfg.data_dir)?;
        cfg = Config::load();
    }
    let (listener, state) = app::bootstrap(cfg).await?;
    eprintln!("vpnmgr-core listening on http://{}", listener.local_addr()?);
    let cleanup = state.clone();
    let result = tokio::select! {
        result = app::serve(listener, state) => result,
        _ = shutdown_signal() => Ok(()),
    };
    if cleanup.cfg.managed_vm {
        vpnmgr_core::shutdown::shutdown_all(Some(&cleanup), &cleanup.cfg, true, vpnmgr_core::shutdown::NORMAL_BUDGET).await;
        vpnmgr_core::tunnel::kill(&cleanup).await;
    }
    result
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
