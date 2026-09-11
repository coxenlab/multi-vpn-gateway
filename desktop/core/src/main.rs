use vpnmgr_core::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let result = run().await;
    if result.is_err() && std::env::var("VPNMGR_NATIVE_CHILD").as_deref() == Ok("1") {
        use std::io::Write;
        println!("{}", serde_json::json!({"event":"boot_error", "message":"本地服务未能启动或已退出。请检查数据目录是否可用、是否已打开另一实例，然后重试。"}));
        let _ = std::io::stdout().flush();
    }
    result
}

async fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--prepare-upgrade") {
        anyhow::ensure!(args.len() == 4, "用法: vpnmgr-core --prepare-upgrade 原数据目录 新副本目录");
        let report = vpnmgr_core::upgrade::prepare(std::path::Path::new(&args[2]), std::path::Path::new(&args[3]))?;
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    let native_child = std::env::var("VPNMGR_NATIVE_CHILD").as_deref() == Ok("1");
    let mut cfg = Config::load();
    let _native_owner = if native_child {
        use std::os::unix::fs::{OpenOptionsExt, DirBuilderExt};
        cfg.validate()?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&cfg.data_dir)?;
        let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
            .open(cfg.data_dir.join("native-owner.lock"))?;
        file.try_lock().map_err(|_| anyhow::anyhow!("这个数据目录已有管理界面在使用，请返回已打开的窗口"))?;
        Some(file)
    } else { None };
    if cfg.managed_vm {
        cfg.validate()?;
        vpnmgr_core::infra::ensure_params(&cfg.data_dir)?;
        cfg = Config::load();
    }
    let (listener, state) = app::bootstrap(cfg).await?;
    eprintln!("vpnmgr-core listening on http://{}", listener.local_addr()?);
    if native_child {
        use std::io::Write;
        println!("{}", serde_json::json!({"event":"ready","ui_port":listener.local_addr()?.port()}));
        std::io::stdout().flush()?;
    }
    let cleanup = state.clone();
    let result = tokio::select! {
        result = app::serve(listener, state) => result,
        _ = shutdown_signal() => Ok(()),
        _ = owner_closed(native_child) => Ok(()),
    };
    if cleanup.cfg.managed_vm {
        vpnmgr_core::shutdown::shutdown_all(Some(&cleanup), &cleanup.cfg, true, vpnmgr_core::shutdown::NORMAL_BUDGET).await;
        vpnmgr_core::tunnel::kill(&cleanup).await;
    }
    result
}

async fn owner_closed(native_child: bool) {
    if !native_child { std::future::pending::<()>().await; return; }
    // A dedicated std thread avoids Tokio's non-cancellable stdin blocking pool holding shutdown open.
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut input = stdin.lock();
        let mut bytes = [0; 256];
        loop { match input.read(&mut bytes) { Ok(0) | Err(_) => break, Ok(_) => {} } }
        let _ = send.send(());
    });
    let _ = receive.await;
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
