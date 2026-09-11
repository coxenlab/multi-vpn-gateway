//! 停止意图先落盘；环境恢复后先确认停止，再开放入口。调用者持有通道变更锁。
use std::path::Path;
use anyhow::{anyhow, ensure, Result};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use crate::{store, AppState};

pub fn ticket(db: &Path, cid: &str) -> Result<Option<String>> {
    Ok(Connection::open(db)?.query_row("SELECT operation_id FROM channel_stop_intents WHERE channel_id=?1",[cid],|r|r.get(0)).optional()?)
}

pub fn pending(db: &Path, cid: &str) -> Result<bool> { Ok(ticket(db,cid)?.is_some()) }

pub fn any(db: &Path) -> Result<bool> {
    Ok(Connection::open(db)?.query_row("SELECT EXISTS(SELECT 1 FROM channel_stop_intents)",[],|r|r.get(0))?)
}

/// 已有实例/恢复记录才需运行面确认；未创建通道可直接停用。
pub fn request(db: &Path, cid: &str) -> Result<()> {
    let mut conn=Connection::open(db)?;
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let instance: Option<String> = tx.query_row("SELECT container_id FROM channels WHERE id=?1",[cid],|r|r.get(0))?;
    let phase: Option<String> = tx.query_row("SELECT phase FROM channel_replacements WHERE channel_id=?1",[cid],|r|r.get(0)).optional()?;
    ensure!(phase.as_deref()!=Some("deleting"),"通道删除已开始，请重试删除");
    if instance.is_some() || phase.is_some() {
        let operation:String=(0..16).map(|_|format!("{:02x}",rand::random::<u8>())).collect();
        tx.execute("INSERT OR IGNORE INTO channel_stop_intents(channel_id,operation_id) VALUES(?1,?2)",rusqlite::params![cid,operation])?;
    }
    tx.execute("UPDATE channels SET status='stopped',latency_ms=NULL WHERE id=?1",[cid])?;
    tx.commit()?;
    Ok(())
}

fn confirmed(db: &Path, cid: &str, operation: &str) -> Result<()> {
    let mut conn=Connection::open(db)?;
    let tx=conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    ensure!(tx.execute("DELETE FROM channel_stop_intents WHERE channel_id=?1 AND operation_id=?2",rusqlite::params![cid,operation])?==1,"停止操作代次已变化");
    ensure!(tx.execute("UPDATE channels SET status='stopped',latency_ms=NULL WHERE id=?1",[cid])?==1,"通道不存在");
    tx.commit()?;
    Ok(())
}

async fn inspect(docker:&bollard::Docker,id:&str)->Result<Option<bollard::models::ContainerInspectResponse>> {
    match docker.inspect_container(id,None).await {
        Ok(info)=>Ok(Some(info)),
        Err(bollard::errors::Error::DockerResponseServerError{status_code:404,..})=>Ok(None),
        Err(error)=>Err(error.into()),
    }
}

pub async fn apply(state:&AppState,cid:&str)->Result<()> {
    let db=state.cfg.db_path();
    let Some(operation)=ticket(&db,cid)? else {return Ok(());};
    let docker=state.docker().ok_or_else(||anyhow!("运行环境尚未连接，停止意图已保留"))?;
    crate::replacement::before_stop(state,cid).await?;
    let channel=store::get_channel(&db,cid)?.ok_or_else(||anyhow!("通道不存在"))?;
    let canonical=format!("vpn-{cid}");
    let actual=inspect(&docker,&canonical).await?;
    if let Some(actual)=actual {
        let expected=channel.container_id.as_deref().ok_or_else(||anyhow!("同名实例归属无法确认"))?;
        ensure!(actual.id.as_deref()==Some(expected) && actual.name.as_deref()==Some(&format!("/{canonical}")),"通道实例身份已变化，停止意图已保留");
        if actual.state.as_ref().and_then(|s|s.running)!=Some(false) {
            // 命令应答不明时只读回，不重复发送 stop。
            let _ = crate::docker::stop(&docker,expected).await;
        }
        if let Some(after)=inspect(&docker,expected).await? {
            ensure!(after.id.as_deref()==Some(expected) && after.state.and_then(|s|s.running)==Some(false),"通道停止尚未确认");
        }
    } else if let Some(expected)=channel.container_id.as_deref() {
        ensure!(inspect(&docker,expected).await?.is_none(),"原实例已改名，停止意图已保留");
    }
    crate::novnc::drop_for(state,cid).await;
    confirmed(&db,cid,&operation)
}

pub async fn recover_all(state:&AppState)->Result<()> {
    // 遍历已有通道并在锁内重读意图，涵盖 Docker 接通前刚接受的停止请求。
    for channel in store::list_channels(&state.cfg.db_path())? {
        let _guard=state.lifecycle.mutate(&channel.id).await?;
        apply(state,&channel.id).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use axum::{body::Body, http::{Request, StatusCode}, response::IntoResponse};
    use tower::ServiceExt;

    async fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.ui_port = 0; cfg.dev_mode = true;
        cfg.managed_vm = true; cfg.vm_profile = "vpnmgr-test".into();
        let (_, state) = crate::app::bootstrap(cfg).await.unwrap();
        Connection::open(state.cfg.db_path()).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,status,container_id) VALUES('c1','fixture','easyconnect','running',?1)", [&"a".repeat(64)]).unwrap();
        (dir, state)
    }

    #[tokio::test]
    async fn dormant_stop_survives_reopen_excludes_rules_and_never_starts_runtime() {
        let (_dir, state) = state().await;
        let db = state.cfg.db_path();
        store::add_rule(&db, "c1", "domain", "stop.example").unwrap();
        let app = crate::server::build_router(state.clone());
        for _ in 0..2 {
            let response = app.clone().oneshot(Request::builder().method("POST").uri("/api/channels/c1/stop").body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let value: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
            assert_eq!(value["stop_pending"], true);
        }
        let operation = ticket(&db,"c1").unwrap().unwrap();
        store::init(&db).unwrap();
        assert_eq!(ticket(&db,"c1").unwrap().as_deref(), Some(operation.as_str()));
        store::set_status(&db,"c1","running").unwrap(); // recovery may temporarily observe a running instance
        assert_eq!(store::effective_rules(&db).unwrap()[0].enabled, 0);
        assert_eq!(crate::lifecycle::sample(state.clone(),"c1".into(),true).await.unwrap()["stop_pending"], true);
        assert_eq!(state.lifecycle.runtime().snapshot().start_attempt, 0);
        assert!(confirmed(&db,"c1","obsolete").is_err());
        assert!(pending(&db,"c1").unwrap());
        store::del_channel(&db,"c1").unwrap();
        assert!(!any(&db).unwrap());
    }

    #[tokio::test]
    async fn confirmation_requires_owned_identity_and_actual_readback() {
        for mode in ["success", "lost_ack", "still_running", "foreign", "renamed", "missing", "read_error"] {
            let (_dir, state) = state().await;
            let db = state.cfg.db_path();
            request(&db,"c1").unwrap();
            let observed = Arc::new(Mutex::new((true, 0u32)));
            let capture = observed.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            state.set_docker(Some(bollard::Docker::connect_with_http(&format!("http://{}",listener.local_addr().unwrap()), 2, bollard::API_DEFAULT_VERSION).unwrap()));
            let router = axum::Router::new().fallback(move |request: Request<Body>| {
                let capture = capture.clone();
                async move {
                    let path = request.uri().path(); let mut s = capture.lock().unwrap();
                    if request.method() == "POST" {
                        assert!(path.contains(&"a".repeat(64)) && path.ends_with("/stop"));
                        s.1 += 1; if mode != "still_running" {s.0=false;}
                        return (if mode=="lost_ack" {StatusCode::INTERNAL_SERVER_ERROR} else {StatusCode::NO_CONTENT}, "").into_response();
                    }
                    assert!(path.ends_with("/json"));
                    if mode == "read_error" {return (StatusCode::SERVICE_UNAVAILABLE, "unavailable").into_response();}
                    if mode == "missing" || (mode=="renamed" && path.contains("vpn-c1")) {return (StatusCode::NOT_FOUND, axum::Json(json!({"message":"missing"}))).into_response();}
                    axum::Json(json!({"Id":if mode=="foreign" {"b".repeat(64)} else {"a".repeat(64)},"Name":if mode=="renamed" {"/external"} else {"/vpn-c1"},"State":{"Running":s.0}})).into_response()
                }
            });
            let server = tokio::spawn(async move {axum::serve(listener,router).await.unwrap();});
            let result = recover_all(&state).await;
            let expected = matches!(mode,"success"|"lost_ack"|"missing");
            assert_eq!(result.is_ok(),expected,"{mode}: {result:?}");
            assert_eq!(pending(&db,"c1").unwrap(),!expected,"{mode}");
            assert_eq!(observed.lock().unwrap().1, u32::from(matches!(mode,"success"|"lost_ack"|"still_running")),"{mode}");
            server.abort();
        }
    }
}
