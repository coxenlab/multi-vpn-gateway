use std::sync::Arc;

use vpnmgr_core::{config::Config, events, mihomo::Controller, server, AppState};

#[tokio::test]
async fn events_api_is_incremental_capped_and_exportable() {
    let dir = tempfile::tempdir().unwrap();
    events::init(dir.path());
    vpnmgr_core::store::init(&dir.path().join("vpnmgr.db")).unwrap();

    let cfg = Config {
        vm_profile: "vpnmgr-test".into(),
        dev_mode: true,
        ui_port: 0,
        data_dir: dir.path().to_path_buf(),
        static_dir: std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../app/static"
        )),
        mihomo_ctrl_url: "http://127.0.0.1:1".into(),
        mihomo_secret: "".into(),
        mihomo_host_port: "7899".into(),
        mihomo_ctrl_port: Some("9090".into()),
        vpn_net: "vpnmgr_vpnnet".into(),
    };
    let state = AppState {
        lifecycle: Default::default(),
        cfg: Arc::new(cfg),
        docker: Arc::new(std::sync::RwLock::new(None)),
        mihomo: Controller::new("http://127.0.0.1:1".into(), "".into()),
        health: vpnmgr_core::health::shared(),
        tunnel: vpnmgr_core::tunnel::handle(),
        novnc: vpnmgr_core::novnc::handle(),
        self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
    };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, server::build_router(state))
            .await
            .unwrap();
    });
    let client = reqwest::Client::new();
    let base = format!("http://{addr}");
    let baseline = events::snapshot(0, 1_000, &events::Filter::default()).1;

    let empty: serde_json::Value = client
        .get(format!(
            "{base}/api/events?since_seq={baseline}&q=definitely-not-present"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(empty["events"].as_array().unwrap().len(), 0);

    for i in 0..1_005 {
        events::emit(
            events::Level::Debug,
            "api_integration",
            "tick",
            format!("tick {i}"),
            serde_json::json!({ "i": i }),
        );
    }
    let capped: serde_json::Value = client
        .get(format!(
            "{base}/api/events?since_seq={baseline}&limit=5000&src=api_integration"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(capped["events"].as_array().unwrap().len(), 1_000);

    let incremental: serde_json::Value = client
        .get(format!(
            "{base}/api/events?since_seq={}&src=api_integration",
            baseline + 1_000
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(incremental["events"].as_array().unwrap().len(), 5);

    events::emit(
        events::Level::Info,
        "api_integration",
        "export_probe",
        "export me",
        serde_json::json!({ "ok": true }),
    );
    let path = events::log_path_today().unwrap();
    for _ in 0..50 {
        if std::fs::read_to_string(&path).is_ok_and(|text| text.contains("export_probe")) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let mut persisted = std::fs::read_to_string(&path).unwrap();
    persisted.push_str(
        r#"{"seq":9999,"ts":"2026-08-06T09:41:07.812+08:00","ts_ms":1,"level":"error","src":"legacy","event":"failed","msg":"password=hunter2","detail":{"api_token":"abc"}}"#,
    );
    persisted.push('\n');
    std::fs::write(&path, persisted).unwrap();
    let response = client
        .get(format!("{base}/api/events/export?days=1"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .unwrap(),
        "text/plain; charset=utf-8"
    );
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    // days 模式也按算出来的区间命名,导出文件名自带取数范围。
    assert!(
        disposition.starts_with("attachment; filename=\"vpnmgr-events-"),
        "{disposition}"
    );
    let exported = response.text().await.unwrap();
    assert!(exported.contains("export_probe"));
    assert!(!exported.contains("hunter2"));
    assert!(!exported.contains("\"abc\""));

    // 区间反了 / 日期解析不出来 → 400,不给一份空文件让人以为「那几天什么都没发生」。
    for query in ["from=2026-01-02&to=2026-01-01", "from=notadate"] {
        let bad = client
            .get(format!("{base}/api/events/export?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST, "{query}");
    }

    // 记录总开关:关掉后新事件不入环,但「关掉」这件事本身必须留痕。
    let seq_before = events::snapshot(0, 1, &events::Filter::default()).1;
    let off: serde_json::Value = client
        .post(format!("{base}/api/events/enabled"))
        .json(&serde_json::json!({ "enabled": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(off["enabled"], false);
    events::emit(
        events::Level::Info,
        "api_integration",
        "muted",
        "muted",
        serde_json::json!({}),
    );
    let listed: serde_json::Value = client
        .get(format!("{base}/api/events?since_seq={seq_before}&limit=50"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["enabled"], false);
    let codes: Vec<&str> = listed["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| event["event"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"logging_disabled"), "{codes:?}");
    assert!(!codes.contains(&"muted"), "{codes:?}");
    assert!(dir.path().join("logs").join("disabled").exists());

    let on: serde_json::Value = client
        .post(format!("{base}/api/events/enabled"))
        .json(&serde_json::json!({ "enabled": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(on["enabled"], true);
    assert!(!dir.path().join("logs").join("disabled").exists());
    task.abort();
}
