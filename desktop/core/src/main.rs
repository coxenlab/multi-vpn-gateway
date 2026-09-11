use vpnmgr_core::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--prepare-upgrade") {
        anyhow::ensure!(args.len() == 4, "用法: vpnmgr-core --prepare-upgrade 原数据目录 新副本目录");
        let report = vpnmgr_core::upgrade::prepare(std::path::Path::new(&args[2]), std::path::Path::new(&args[3]))?;
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
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
