use vpnmgr_core::{app, config::Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load();
    let (listener, state) = app::bootstrap(cfg).await?;
    eprintln!("vpnmgr-core listening on http://{}", listener.local_addr()?);
    app::serve(listener, state).await
}
