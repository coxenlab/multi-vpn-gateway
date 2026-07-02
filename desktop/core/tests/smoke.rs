use std::sync::Arc;
use vpnmgr_core::{config::Config, mihomo::Controller, server, store, AppState};

#[tokio::test]
async fn boots_and_serves_system_and_index() {
    let dir = tempfile::tempdir().unwrap();
    store::init(&dir.path().join("vpnmgr.db")).unwrap();

    let cfg = Config {
        ui_port: 0, // OS picks a free port
        data_dir: dir.path().to_path_buf(),
        static_dir: std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/static")),
        mihomo_ctrl_url: "http://127.0.0.1:1".into(),
        mihomo_secret: "".into(),
        mihomo_host_port: "7899".into(),
        mihomo_ctrl_port: Some("9090".into()),
        vpn_net: "vpnmgr_vpnnet".into(),
    };
    let state = AppState {
        cfg: Arc::new(cfg),
        docker: Arc::new(std::sync::RwLock::new(None)),
        mihomo: Controller::new("http://127.0.0.1:1".into(), "".into()),
        health: vpnmgr_core::health::shared(),
    };

    // 命门 #4: bind 127.0.0.1 only
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    let app = server::build_router(state);
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });

    let base = format!("http://{addr}");
    let c = reqwest::Client::new();
    let sys: serde_json::Value = c.get(format!("{base}/api/system")).send().await.unwrap().json().await.unwrap();
    assert_eq!(sys["bound_ip"], "127.0.0.1");
    let html = c.get(format!("{base}/")).send().await.unwrap().text().await.unwrap();
    assert!(html.contains("<html"));
}
