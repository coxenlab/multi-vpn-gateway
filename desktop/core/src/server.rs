use axum::routing::get;
use axum::Router;
use tower::ServiceBuilder;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use crate::{routes, api, AppState};

/// 组路由:三个 API 路由先注册,静态 ServeDir 作 fallback(命门:顺序——API 不被静态盖住)。
/// 静态响应加 Cache-Control: no-cache(对照 _NoCacheStatic,防浏览器启发式缓存吃旧 UI)。
pub fn build_router(state: AppState) -> Router {
    let static_svc = ServiceBuilder::new()
        .layer(SetResponseHeaderLayer::overriding(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-cache"),
        ))
        .service(
            ServeDir::new(state.cfg.static_dir.clone()).append_index_html_on_directories(true),
        );

    Router::new()
        .route("/api/system", get(routes::system))
        .route("/api/system/heal-proxy", axum::routing::post(routes::heal_proxy))
        .route("/api/channels", get(routes::channels).post(api::create))
        .route("/api/channels/:cid", axum::routing::patch(api::update).delete(api::delete))
        .route("/api/channels/:cid/start", axum::routing::post(api::start))
        .route("/api/channels/:cid/stop", axum::routing::post(api::stop))
        .route("/api/proxies", get(routes::proxies))
        .route("/api/vpn-types", get(api::vpn_types))
        .route("/api/connections", get(api::connections))
        .route("/api/channels/:cid/logs", get(api::logs))
        .route("/api/channels/:cid/login", get(api::login))
        .route("/api/channels/:cid/upload", axum::routing::post(api::upload))
        .route("/api/channels/:cid/status", get(api::status))
        .route("/api/channels/:cid/rules", axum::routing::post(api::add_rules))
        .route(
            "/api/channels/:cid/rules/:rid",
            axum::routing::delete(api::del_rule).patch(api::patch_rule),
        )
        .route("/clash/vpn-rules.yaml", get(api::clash_provider))
        .route("/api/clash-snippet", get(api::clash_snippet))
        .route("/entry/proxy.pac", get(api::entry_pac))
        .route("/api/entry/setup-commands", get(api::entry_setup_commands))
        // 7c 宿主接管层(Tauri/host-only;前端 feature-detect)
        .route("/api/entry/clash-detect", get(api::clash_detect))
        .route("/api/entry/merge-profile", get(api::clash_merge_profile))
        .route(
            "/api/entry/system-proxy",
            get(api::system_proxy_get).post(api::system_proxy_set),
        )
        .route("/api/vpn-types/:vtype/versions", get(api::vpn_versions))
        .route("/api/preflight", get(api::preflight_check))
        // 同段 :x:GET→拉取任务状态(x=task_id),POST→修复(x=action),对照 main.py 同 path 不同方法
        .route("/api/preflight/fix/:x", get(api::preflight_fix_status).post(api::preflight_fix))
        .route("/api/images", get(api::images_inventory))
        .route("/api/mirrors", get(api::mirrors_list).post(api::mirrors_add))
        .route("/api/mirrors/:mid", axum::routing::patch(api::mirrors_patch).delete(api::mirrors_del))
        .route("/api/mirrors/test", axum::routing::post(api::mirrors_test))
        .fallback_service(static_svc)
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::mihomo::Controller;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tower::ServiceExt; // oneshot

    fn state_with_db(db_dir: &std::path::Path) -> AppState {
        let cfg = Config {
            ui_port: 8787,
            data_dir: db_dir.to_path_buf(),
            static_dir: std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/static")),
            mihomo_ctrl_url: "http://127.0.0.1:1".into(),
            mihomo_secret: "".into(),
            mihomo_host_port: "7899".into(),
            mihomo_ctrl_port: Some("9090".into()),
            vpn_net: "vpnmgr_vpnnet".into(),
        };
        AppState {
            cfg: Arc::new(cfg),
            docker: None,
            mihomo: Controller::new("http://127.0.0.1:1".into(), "".into()),
            health: crate::health::shared(),
        }
    }

    #[tokio::test]
    async fn system_route_shape() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app
            .oneshot(Request::builder().uri("/api/system").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["bound_ip"], "127.0.0.1");
        assert_eq!(v["mihomo_status"], "down");
        assert_eq!(v["mihomo_port"], 7899);
        assert_eq!(v["ui_port"], 8787);
    }

    #[tokio::test]
    async fn channels_route_strips_secrets_and_down_overrides() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        let cfg = r#"{"_fields":{"server":"vpn.x.com","password":"ZW5j"},"_secret":["password"]}"#;
        conn.execute(
            "INSERT INTO channels(id,name,vpn_type,login_method,username,password_enc,status,config_json) \
             VALUES('abc','A','easyconnect','interactive','alice','CIPHER','running',?1)",
            [cfg],
        ).unwrap();
        conn.execute("INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('abc','domain','x.com',1)", []).unwrap();
        drop(conn);

        let app = build_router(state_with_db(dir.path()));
        let resp = app
            .oneshot(Request::builder().uri("/api/channels").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let ch = &v[0];
        assert!(ch.get("password_enc").is_none());
        assert!(ch["config"].get("password").is_none());
        assert_eq!(ch["config"]["server"], "vpn.x.com");
        assert_eq!(ch["status"], "down");
        assert_eq!(ch["domains"][0]["pattern"], "x.com");
        assert_eq!(ch["socks_proxy"], "ch-abc");
    }

    #[tokio::test]
    async fn vpn_types_lists_adapters() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/vpn-types").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.is_array() && !v.as_array().unwrap().is_empty(), "至少有 easyconnect 等适配器");
        assert!(v.as_array().unwrap().iter().any(|a| a["key"] == "easyconnect"));
    }

    #[tokio::test]
    async fn logs_without_docker_returns_note_line() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/channels/x/logs").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["lines"][0].as_str().unwrap().contains("docker"), "docker 不可用时给单行说明");
    }

    #[tokio::test]
    async fn add_rules_classifies_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        // rebuild 写 mihomo 配置:指到 tempfile,别碰 /cfg
        std::env::set_var("MIHOMO_CONFIG_PATH", dir.path().join("m.yaml"));
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.execute("INSERT INTO channels(id,name,vpn_type,login_method,status) VALUES('c1','c','easyconnect','interactive','running')", []).unwrap();
        drop(conn);
        let app = build_router(state_with_db(dir.path()));
        let body = r#"{"patterns":["a.com","10.0.0.0/8","a.com"]}"#;
        let resp = app.oneshot(
            Request::builder().method("POST").uri("/api/channels/c1/rules")
                .header("content-type", "application/json").body(Body::from(body)).unwrap()
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["added"]["domain"], 1);
        assert_eq!(v["added"]["ip"], 1);
        assert_eq!(v["domains"][0]["pattern"], "a.com");
        assert_eq!(v["ips"][0]["pattern"], "10.0.0.0/8");
    }

    #[tokio::test]
    async fn create_without_docker_500s_and_marks_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        let app = build_router(state_with_db(dir.path())); // docker: None
        let body = r#"{"name":"t","vpn_type":"easyconnect","server":"vpn.x.com","probe_url":"https://oa.x.com"}"#;
        let resp = app.oneshot(
            Request::builder().method("POST").uri("/api/channels")
                .header("content-type", "application/json").body(Body::from(body)).unwrap()
        ).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let chans = crate::store::list_channels(&db).unwrap();
        assert_eq!(chans.len(), 1);
        assert_eq!(chans[0].status, "error");
    }

    #[tokio::test]
    async fn login_headless_skips_novnc() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        rusqlite::Connection::open(&db).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,login_method,status) VALUES('o1','o','anyconnect','headless','running')", []).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/channels/o1/login").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["login_mode"], "headless");
    }

    #[tokio::test]
    async fn stop_marks_stopped_even_without_docker() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        rusqlite::Connection::open(&db).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,login_method,status) VALUES('c1','c','easyconnect','interactive','running')", []).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().method("POST").uri("/api/channels/c1/stop").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(crate::store::get_channel(&db, "c1").unwrap().unwrap().status, "stopped");
    }

    #[tokio::test]
    async fn clash_snippet_served_as_text() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        rusqlite::Connection::open(&db).unwrap().execute(
            "INSERT INTO rules(channel_id,kind,pattern,enabled) VALUES('c','ip','10.0.0.0/8',1)", []).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/clash-snippet").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("content-type").unwrap().to_str().unwrap().starts_with("text/plain"));
        let body = String::from_utf8(axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap();
        assert!(body.contains("IP-CIDR,10.0.0.0/8,vpn-router,no-resolve"));
    }

    #[tokio::test]
    async fn vpn_versions_unknown_404() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/vpn-types/nope/versions").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn vpn_versions_non_versioned_empty() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/vpn-types/anyconnect/versions").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["versions"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn mirrors_crud() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.clone().oneshot(Request::builder().method("POST").uri("/api/mirrors")
            .header("content-type", "application/json").body(Body::from(r#"{"host":"my.mirror.io"}"#)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["host"], "my.mirror.io");
        let resp = app.oneshot(Request::builder().uri("/api/mirrors").body(Body::empty()).unwrap()).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert!(v.as_array().unwrap().iter().any(|m| m["host"] == "my.mirror.io"));
    }

    #[tokio::test]
    async fn preflight_no_docker_overall_fail() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app.oneshot(Request::builder().uri("/api/preflight?vpn_type=easyconnect&version=7.6.3").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(v["overall"], "fail");
    }

    #[tokio::test]
    async fn static_fallback_serves_index() {
        let dir = tempfile::tempdir().unwrap();
        crate::store::init(&dir.path().join("vpnmgr.db")).unwrap();
        let app = build_router(state_with_db(dir.path()));
        let resp = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-cache");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("<html"), "index.html should be served at /");
    }
}
