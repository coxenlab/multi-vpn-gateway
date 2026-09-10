//! 通道操作串行、探活共享与 generation。慢探活不占操作锁,结果提交时重新核对代次。
use std::collections::HashMap;
use std::sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex as AsyncMutex, OwnedMutexGuard};
use serde_json::{json, Value};
use crate::{AppState, manager, store};

type ProbeResult = Result<Value, String>;
type ProbeWatch = watch::Receiver<Option<(u64, ProbeResult)>>;

#[derive(Default)]
pub struct Lifecycle {
    closing: AtomicBool,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

#[derive(Default)]
struct ProbeState {
    in_flight: Option<ProbeWatch>,
    cached: Option<(u64, Instant, Value)>,
    failures: u32,
}

#[derive(Default)]
pub struct Slot {
    operation: Arc<AsyncMutex<()>>,
    generation: AtomicU64,
    probe: AsyncMutex<ProbeState>,
}

impl Lifecycle {
    pub fn slot(&self, cid: &str) -> Arc<Slot> {
        self.slots.lock().unwrap().entry(cid.into()).or_default().clone()
    }

    pub async fn mutate(&self, cid: &str) -> anyhow::Result<OwnedMutexGuard<()>> {
        anyhow::ensure!(!self.closing.load(Ordering::SeqCst), "应用正在退出");
        let slot = self.slot(cid);
        let guard = slot.operation.clone().lock_owned().await;
        anyhow::ensure!(!self.closing.load(Ordering::SeqCst), "应用正在退出");
        slot.generation.fetch_add(1, Ordering::SeqCst);
        Ok(guard)
    }

    pub async fn quiesce(&self) {
        self.closing.store(true, Ordering::SeqCst);
        let slots: Vec<_> = self.slots.lock().unwrap().values().cloned().collect();
        for slot in slots {
            {
                let _guard = slot.operation.lock().await;
                slot.generation.fetch_add(1, Ordering::SeqCst);
            }
            let pending = slot.probe.lock().await.in_flight.clone();
            if let Some(mut receiver) = pending { let _ = wait_probe(&mut receiver).await; }
        }
    }
}

async fn wait_probe(receiver: &mut ProbeWatch) -> Result<(u64, ProbeResult), String> {
    loop {
        if let Some(result) = receiver.borrow().clone() { return Ok(result); }
        receiver.changed().await.map_err(|_| "探活任务已退出".to_string())?;
    }
}

/// /status 强制新鲜探活；/health 可在退避窗口内复用带时间戳的结果。
pub async fn sample(state: AppState, cid: String, fresh: bool) -> ProbeResult {
    sample_with(state, cid, fresh, |st: AppState, ch| async move {
        manager::probe(st.docker().as_ref(), &st.cfg, &ch).await
    }).await
}

async fn sample_with<F, Fut>(state: AppState, cid: String, fresh: bool, execute: F) -> ProbeResult
where
    F: Fn(AppState, store::ChannelPublic) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = (bool, Option<i64>)> + Send + 'static,
{
    let slot = state.lifecycle.slot(&cid);
    loop {
        if state.lifecycle.closing.load(Ordering::SeqCst) { return Err("应用正在退出".into()); }
        let (ch, generation) = {
            let _guard = slot.operation.lock().await;
            let ch = store::get_channel(&state.cfg.db_path(), &cid).map_err(|e| e.to_string())?
                .ok_or_else(|| "not found".to_string())?;
            (ch, slot.generation.load(Ordering::SeqCst))
        };
        if !matches!(ch.status.as_str(), "running" | "logged_in") {
            return Ok(json!({"status":ch.status,"connected":false,"latency_ms":null,"checked_at":null,"stale":false}));
        }
        let mut stale = None;
        let mut receiver = {
            let mut probe = slot.probe.lock().await;
            if generation != slot.generation.load(Ordering::SeqCst) { continue; }
            if !fresh {
                if let Some((cached_generation, until, value)) = &probe.cached {
                    if *cached_generation == generation && Instant::now() < *until { return Ok(value.clone()); }
                    if *cached_generation == generation {
                        let mut value = value.clone(); value["stale"] = json!(true); stale = Some(value);
                    }
                }
            }
            if let Some(receiver) = &probe.in_flight { receiver.clone() } else {
                let (sender, receiver) = watch::channel(None);
                probe.in_flight = Some(receiver.clone());
                let st = state.clone(); let slot = slot.clone(); let cid = cid.clone();
                let execute = execute.clone();
                tokio::spawn(async move {
                    let outcome = tokio::time::timeout(Duration::from_secs(32), execute(st.clone(), ch)).await;
                    let _guard = slot.operation.lock().await;
                    let current = generation == slot.generation.load(Ordering::SeqCst)
                        && !st.lifecycle.closing.load(Ordering::SeqCst);
                    let result = if current {
                        match outcome {
                            Ok((ok, ms)) => {
                                crate::ev!(debug, "manager", "probe", "通道探活完成",
                                    { "cid": cid.as_str(), "ok": ok, "latency_ms": ms });
                                let status = if ok { "logged_in" } else { "running" };
                                store::set_probe_result(&st.cfg.db_path(), &cid, status, ms)
                                    .map(|()| json!({"status":status,"connected":ok,"latency_ms":ms,
                                        "checked_at":chrono::Utc::now().timestamp_millis(),"stale":false}))
                                    .map_err(|e| e.to_string())
                            }
                            Err(_) => Err("探活超时,连通状态待确认".into()),
                        }
                    } else { Err("通道已变更,旧探活结果已丢弃".into()) };
                    let mut probe = slot.probe.lock().await;
                    if current {
                        if let Ok(value) = &result {
                            let ok = value["connected"].as_bool() == Some(true);
                            probe.failures = if ok { 0 } else { probe.failures.saturating_add(1) };
                            let seconds = if ok { 30 } else { (5u64 << probe.failures.saturating_sub(1).min(3)).min(30) };
                            probe.cached = Some((generation, Instant::now() + Duration::from_secs(seconds), value.clone()));
                        }
                    }
                    probe.in_flight = None;
                    let _ = sender.send(Some((generation, result)));
                });
                receiver
            }
        };
        if let Some(value) = stale { return Ok(value); }
        let (result_generation, result) = match wait_probe(&mut receiver).await {
            Ok(result) => result,
            Err(error) => {
                slot.probe.lock().await.in_flight = None;
                return Err(error);
            }
        };
        if result_generation != slot.generation.load(Ordering::SeqCst) { continue; }
        return result;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn fixture() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into(); cfg.dev_mode = true; cfg.vm_profile = "vpnmgr-test".into();
        store::init(&cfg.db_path()).unwrap();
        rusqlite::Connection::open(cfg.db_path()).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,probe_url,status) VALUES('c1','fixture','easyconnect','http://fixture.test','running')", []
        ).unwrap();
        let st = AppState {
            cfg: Arc::new(cfg), lifecycle: Default::default(), docker: Arc::new(std::sync::RwLock::new(None)),
            mihomo: crate::mihomo::Controller::new("http://127.0.0.1:1".into(), String::new()),
            health: crate::health::shared(), tunnel: crate::tunnel::handle(), novnc: crate::novnc::handle(),
            self_heal_enabled: Arc::new(AtomicBool::new(false)),
        };
        (dir, st)
    }

    #[tokio::test]
    async fn concurrent_requests_share_one_probe_and_request_cancellation_is_safe() {
        let (_dir, st) = fixture();
        let count = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let run = { let count=count.clone(); let gate=gate.clone(); move |_, _| {
            let count=count.clone(); let gate=gate.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                gate.acquire().await.unwrap().forget(); (true, Some(12))
            }
        }};
        let mut tasks=Vec::new();
        for _ in 0..8 { tasks.push(tokio::spawn(sample_with(st.clone(), "c1".into(), true, run.clone()))); }
        while count.load(Ordering::SeqCst)==0 { tokio::task::yield_now().await; }
        tokio::time::sleep(Duration::from_millis(20)).await;
        tasks.remove(0).abort();
        assert_eq!(count.load(Ordering::SeqCst),1);
        gate.add_permits(1);
        for t in tasks { assert_eq!(t.await.unwrap().unwrap()["connected"],true); }
        assert_eq!(count.load(Ordering::SeqCst),1);
    }

    #[tokio::test]
    async fn slow_probe_cannot_resurrect_stopped_channel() {
        let (_dir, st)=fixture();
        let (started_tx, mut started_rx)=watch::channel(false);
        let gate=Arc::new(tokio::sync::Semaphore::new(0));
        let run={let gate=gate.clone();move |_, _| {let gate=gate.clone(); let tx=started_tx.clone();async move {
            tx.send(true).unwrap(); gate.acquire().await.unwrap().forget();(true,Some(1))
        }}};
        let task=tokio::spawn(sample_with(st.clone(),"c1".into(),true,run));
        started_rx.changed().await.unwrap();
        {
            let _guard=st.lifecycle.mutate("c1").await.unwrap();
            store::set_status(&st.cfg.db_path(),"c1","stopped").unwrap();
        }
        gate.add_permits(1);
        assert_eq!(task.await.unwrap().unwrap()["status"],"stopped");
        assert_eq!(store::get_channel(&st.cfg.db_path(),"c1").unwrap().unwrap().status,"stopped");
    }

    #[tokio::test]
    async fn polling_uses_cache_but_manual_probe_is_fresh() {
        let (_dir,st)=fixture(); let count=Arc::new(AtomicUsize::new(0));
        let run={let count=count.clone();move |_,_| {let count=count.clone();async move {
            count.fetch_add(1,Ordering::SeqCst);(false,None)
        }}};
        sample_with(st.clone(),"c1".into(),false,run.clone()).await.unwrap();
        sample_with(st.clone(),"c1".into(),false,run.clone()).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst),1);
        sample_with(st,"c1".into(),true,run).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst),2);
    }

    #[tokio::test]
    async fn expired_health_returns_stale_while_refreshing() {
        let (_dir,st)=fixture();
        let first=sample_with(st.clone(),"c1".into(),false,|_,_| async {(true,Some(1))}).await.unwrap();
        let slot=st.lifecycle.slot("c1");
        slot.probe.lock().await.cached.as_mut().unwrap().1=Instant::now();
        let gate=Arc::new(tokio::sync::Semaphore::new(0));
        let run={let gate=gate.clone();move |_,_| {let gate=gate.clone();async move {
            gate.acquire().await.unwrap().forget();(false,None)
        }}};
        let stale=sample_with(st.clone(),"c1".into(),false,run).await.unwrap();
        assert_eq!(stale["stale"],true);
        assert_eq!(stale["checked_at"],first["checked_at"]);
        let mut pending=slot.probe.lock().await.in_flight.clone().unwrap();
        gate.add_permits(1);
        wait_probe(&mut pending).await.unwrap().1.unwrap();
        let refreshed=sample_with(st,"c1".into(),false,|_,_| async {panic!("cache missed")}).await.unwrap();
        assert_eq!(refreshed["stale"],false);
        assert_eq!(refreshed["connected"],false);
    }

    #[tokio::test]
    async fn shutdown_waits_for_mutation_and_rejects_new_work() {
        let (_dir,st)=fixture();
        let guard=st.lifecycle.mutate("c1").await.unwrap();
        let lifecycle=st.lifecycle.clone();
        let task=tokio::spawn(async move {lifecycle.quiesce().await});
        tokio::task::yield_now().await;
        assert!(!task.is_finished());
        assert!(st.lifecycle.mutate("c2").await.is_err());
        drop(guard);
        task.await.unwrap();
        assert!(sample(st,"c1".into(),true).await.is_err());
    }
}
