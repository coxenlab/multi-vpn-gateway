//! 通道操作串行、探活共享与 generation。慢探活不占操作锁,结果提交时重新核对代次。
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex as AsyncMutex, OwnedMutexGuard, Notify};
use axum::extract::{Path as UrlPath, State};
use axum::response::IntoResponse;
use serde_json::{json, Value};
use crate::{AppState, manager, store};
use crate::manager::ProbeOutcome;
use crate::verify_target::{self, Target};

type ProbeResult = Result<Value, String>;
type ProbeWatch = watch::Receiver<Option<(u64, ProbeResult)>>;

/// 连续探活失败到这个次数,运行态通道进掉线集合,内层入口改为阻止它的规则。
/// 第 1 次失败就会把已连接降为待登录;一次失败即阻止会让网络抖动切断其实还通的通道。
const LOST_AFTER: u32 = 3;
/// 后台采样间隔。窗口隐藏时也照常累计掉线、解除阻止。
const SAMPLE_EVERY: Duration = Duration::from_secs(15);
/// 延迟曲线只留最近一小时。
const LATENCY_WINDOW_MS: i64 = 3_600_000;

/// 掉线集合。进程级、不落库,重启后从空开始;键带数据库路径(config_apply 只拿得到 db),
/// 并行单测同名通道互不串扰。
static LOST: LazyLock<Mutex<HashSet<(PathBuf, String)>>> = LazyLock::new(Default::default);

/// 该数据库下处于掉线阻止中的通道 id,供 [`crate::config_apply::apply`] 拼内层入口配置。
pub fn lost_channels(db: &Path) -> HashSet<String> {
    LOST.lock().unwrap().iter().filter(|(path, _)| path == db).map(|(_, cid)| cid.clone()).collect()
}

#[derive(Default)]
pub struct Lifecycle {
    closing: AtomicBool,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
    runtime: Arc<crate::runtime_lifecycle::Coordinator>,
    gateway_repair: Arc<AsyncMutex<()>>,
    pub(crate) docker_tunnel: AsyncMutex<Option<tokio::process::Child>>,
    rebuild: Mutex<RebuildQueue>,
    rebuild_wake: Notify,
}

/// 通道锁先释放，再结束底座活动；排队中的通道操作同样阻止底座释放。
pub struct OperationGuard {
    _channel: OwnedMutexGuard<()>,
    _activity: crate::runtime_lifecycle::Activity,
}

#[derive(Default)]
struct ProbeState {
    in_flight: Option<ProbeWatch>,
    cached: Option<(u64, Instant, Value)>,
    failures: u32,
    background_requested: bool,
    /// 本通道在掉线集合里时记下所用的数据库路径,`mutate` 据此移出。
    lost: Option<PathBuf>,
    /// 移出了掉线集合但调用方未必重建,或者后台重建没有成功:下一次结果时补一次重建。
    resync: bool,
    /// 本进程里握手成功过的目标。规则里的名字常常没有解析或端口不开,从没成功过的握手目标
    /// 失败不计入掉线次数;应用重启后清空。
    handshake_ok: HashSet<String>,
}

impl ProbeState {
    /// 失败是否算数:用户填的地址总算;握手目标成功过一次之后才算。
    fn verified(&self, target: &Target) -> bool {
        match target { Target::Url(_) => true, Target::Host(host) => self.handshake_ok.contains(host) }
    }

    /// 在掉线集合里就移出、计数从头开始,并留 `resync` 补一次重建。返回是否移出。
    fn lift(&mut self, cid: &str) -> bool {
        let Some(db) = self.lost.take() else { return false };
        LOST.lock().unwrap().remove(&(db, cid.to_string()));
        self.failures = 0;
        self.resync = true;
        true
    }
}

#[derive(Default)]
struct RebuildQueue {
    pending: bool,
    running: bool,
    revision: u64,
    recovery: u64,
    failures: u32,
    due: Option<Instant>,
}

fn rebuild_delay(failures: u32) -> Duration {
    Duration::from_secs(match failures { 0 | 1 => 5, 2 => 10, 3 => 20, _ => 60 })
}

/// 一个应用数据库只有一个后台重建工作者；页面采样仅设置待处理标记，不重置退避。
fn spawn_rebuild(st: &AppState, _slot: &Arc<Slot>, probe: &mut ProbeState) {
    probe.resync = false;
    {
        let mut queue = st.lifecycle.rebuild.lock().unwrap();
        queue.pending = true;
        queue.revision = queue.revision.wrapping_add(1);
    }
    start_rebuild_worker(st);
}

fn start_rebuild_worker(st: &AppState) {
    {
        let mut queue = st.lifecycle.rebuild.lock().unwrap();
        if queue.running || !queue.pending || st.lifecycle.is_closing() { return; }
        queue.running = true;
    }
    let st = st.clone();
    tokio::spawn(async move {
        loop {
            let (revision, recovery, due) = {
                let mut queue = st.lifecycle.rebuild.lock().unwrap();
                if !queue.pending || st.lifecycle.is_closing() {
                    queue.running = false;
                    return;
                }
                (queue.revision, queue.recovery, queue.due)
            };
            if let Some(due) = due.filter(|due| *due > Instant::now()) {
                tokio::select! {
                    _ = tokio::time::sleep_until(due.into()) => {},
                    _ = st.lifecycle.rebuild_wake.notified() => {},
                }
                continue;
            }
            let Some(activity) = st.lifecycle.runtime.maintenance() else {
                st.lifecycle.rebuild.lock().unwrap().running = false;
                return;
            };
            let docker = st.docker();
            let status = manager::rebuild(&st.cfg, docker.as_ref(), &st.cfg.db_path()).await;
            drop(activity);
            let mut queue = st.lifecycle.rebuild.lock().unwrap();
            if crate::api::reload_ok(&status) {
                queue.pending = queue.revision != revision;
                queue.failures = 0;
                queue.due = None;
            } else {
                queue.failures = queue.failures.saturating_add(1);
                queue.due = (queue.recovery == recovery).then(|| Instant::now() + rebuild_delay(queue.failures));
            }
        }
    });
}

/// 仅在底座/链路恢复时越过旧退避；正常采样或手动检测不加速重建。
pub fn retry_rebuild_after_recovery(st: &AppState) {
    {
        let mut queue = st.lifecycle.rebuild.lock().unwrap();
        queue.due = None;
        queue.recovery = queue.recovery.wrapping_add(1);
    }
    st.lifecycle.rebuild_wake.notify_one();
    start_rebuild_worker(st);
}

#[derive(Default)]
pub struct Slot {
    operation: Arc<AsyncMutex<()>>,
    generation: AtomicU64,
    probe: AsyncMutex<ProbeState>,
    /// 最近一小时的检测结果 `(毫秒时间戳, 延迟)`,失败为 None。进程内,重启后从空开始。
    latency: Mutex<VecDeque<(i64, Option<i64>)>>,
}

/// 追加一条检测结果,并裁掉一小时以前的。
fn push_latency(points: &mut VecDeque<(i64, Option<i64>)>, at: i64, ms: Option<i64>) {
    points.push_back((at, ms));
    trim_latency(points, at);
}

fn trim_latency(points: &mut VecDeque<(i64, Option<i64>)>, now: i64) {
    while points.front().is_some_and(|(at, _)| *at < now - LATENCY_WINDOW_MS) { points.pop_front(); }
}

impl Lifecycle {
    pub fn runtime(&self) -> &Arc<crate::runtime_lifecycle::Coordinator> { &self.runtime }

    /// 手动与自动修复共用占用，不把重复点击排成一串后续重启。
    pub fn try_gateway_repair(&self) -> Option<OwnedMutexGuard<()>> {
        if self.closing.load(Ordering::SeqCst) { return None; }
        self.gateway_repair.clone().try_lock_owned().ok()
    }

    pub fn slot(&self, cid: &str) -> Arc<Slot> {
        self.slots.lock().unwrap().entry(cid.into()).or_default().clone()
    }

    pub fn is_closing(&self) -> bool { self.closing.load(Ordering::SeqCst) }

    /// 该通道最近一小时的检测结果,按时间先后。
    pub fn latency_points(&self, cid: &str) -> Vec<(i64, Option<i64>)> {
        let slot = self.slot(cid);
        let mut points = slot.latency.lock().unwrap();
        trim_latency(&mut points, chrono::Utc::now().timestamp_millis());
        points.iter().copied().collect()
    }

    /// 启停、重登、重建等操作后重新计数,并解除掉线阻止(否则重启后第一次失败就算第 4 次)。
    pub async fn mutate(&self, cid: &str) -> anyhow::Result<OperationGuard> {
        let guard = self.access(cid).await?;
        let slot = self.slot(cid);
        slot.generation.fetch_add(1, Ordering::SeqCst);
        let mut probe = slot.probe.lock().await;
        probe.failures = 0;
        probe.lift(cid);
        Ok(guard)
    }

    /// 调用方已持有该通道 operation 锁并原子保存了新验证目标后，作废旧目标的
    /// 在途/缓存结果和掉线计数。这里不重新获取 operation 锁，避免自锁。
    pub async fn target_confirmed_locked(&self, cid: &str) {
        let slot = self.slot(cid);
        slot.generation.fetch_add(1, Ordering::SeqCst);
        let mut probe = slot.probe.lock().await;
        probe.cached = None;
        probe.failures = 0;
        probe.lift(cid);
        // 调用方紧接着用新 DB 快照 rebuild，不需要后台队列再补一次。
        probe.resync = false;
    }

    /// 登录视图等资源操作与启停串行，但不使连通缓存失效。
    pub async fn access(&self, cid: &str) -> anyhow::Result<OperationGuard> {
        anyhow::ensure!(!self.closing.load(Ordering::SeqCst), "应用正在退出");
        let activity = self.runtime.activity().await.map_err(anyhow::Error::msg)?;
        let slot = self.slot(cid);
        let guard = slot.operation.clone().lock_owned().await;
        anyhow::ensure!(!self.closing.load(Ordering::SeqCst), "应用正在退出");
        Ok(OperationGuard { _channel: guard, _activity: activity })
    }

    /// 该目标的结论是否可信:用户填的地址总是;握手目标要本进程里成功过一次。
    pub async fn verified_target(&self, cid: &str, target: &Target) -> bool {
        self.slot(cid).probe.lock().await.verified(target)
    }

    pub async fn maintenance_access(&self, cid: &str) -> Option<OperationGuard> {
        let activity = self.runtime.maintenance()?;
        let guard = self.slot(cid).operation.clone().lock_owned().await;
        if self.closing.load(Ordering::SeqCst) { return None; }
        Some(OperationGuard { _channel: guard, _activity: activity })
    }

    pub fn begin_closing(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.runtime.begin_closing();
        self.rebuild_wake.notify_one();
    }

    pub async fn quiesce(&self) {
        self.begin_closing();
        self.runtime.quiesce().await;
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
/// 检测目标由 [`verify_target::pick`] 确定;没有目标时不探活。
pub async fn sample(state: AppState, cid: String, fresh: bool) -> ProbeResult {
    sample_with(state, cid, fresh, |st: AppState, ch, target| async move {
        if let Some(reason) = probe_unavailable(&st).await { return ProbeOutcome::failed(reason); }
        verify_target::probe(st.docker().as_ref(), &st.cfg, &ch, &target).await
    }).await
}

async fn probe_unavailable(state: &AppState) -> Option<&'static str> {
    if !crate::runtime::serving(state) { return Some("运行环境未就绪"); }
    if state.health.lock().unwrap().gateway_health != crate::health::GatewayHealth::Healthy {
        return Some("分流链路状态待确认");
    }
    let Some(docker) = state.docker() else { return Some("容器管理连接不可用"); };
    if !matches!(tokio::time::timeout(Duration::from_secs(2), docker.ping()).await, Ok(Ok(_))) {
        return Some("容器管理连接不可用");
    }
    None
}

/// 后台采样每 15 秒独立调度各运行态通道，共享在途探活但不复用旧缓存。
/// 只有后台结果累计失败和跨阈值；窗口隐藏不影响后台检测。
/// 没有运行环境连接(隔离核心、单测、空闲释放后)什么都不做,不读库、不发请求。
pub fn spawn_sampler(state: AppState) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SAMPLE_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            if state.lifecycle.closing.load(Ordering::SeqCst) { break; }
            if !crate::runtime::serving(&state) || state.docker().is_none() { continue; }
            let Ok(channels) = store::list_channels(&state.cfg.db_path()) else { continue };
            let unavailable = probe_unavailable(&state).await;
            for cid in sampled_channels(&channels) {
                let st = state.clone();
                tokio::spawn(async move {
                    let _ = sample_with_origin(st, cid, true, true, move |st, ch, target| async move {
                        if let Some(reason) = unavailable { return ProbeOutcome::failed(reason); }
                        verify_target::probe(st.docker().as_ref(), &st.cfg, &ch, &target).await
                    }).await;
                });
            }
        }
    });
}

/// 只采样运行态通道;全部停止时不采样,不影响空闲释放(释放要求全部通道已停止)。
fn sampled_channels(channels: &[store::ChannelPublic]) -> Vec<String> {
    channels.iter()
        .filter(|c| matches!(c.status.as_str(), "running" | "logged_in"))
        .map(|c| c.id.clone())
        .collect()
}

async fn sample_with<F, Fut>(state: AppState, cid: String, fresh: bool, execute: F) -> ProbeResult
where
    F: Fn(AppState, store::ChannelPublic, Target) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ProbeOutcome> + Send + 'static,
{
    sample_with_origin(state, cid, fresh, false, execute).await
}

async fn sample_with_origin<F, Fut>(state: AppState, cid: String, fresh: bool, background: bool, execute: F) -> ProbeResult
where
    F: Fn(AppState, store::ChannelPublic, Target) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ProbeOutcome> + Send + 'static,
{
    let slot = state.lifecycle.slot(&cid);
    let db = state.cfg.db_path();
    loop {
        if state.lifecycle.closing.load(Ordering::SeqCst) { return Err("应用正在退出".into()); }
        let (ch, generation, pending, stop_pending, target) = {
            let Ok(_guard) = slot.operation.try_lock() else {
                crate::ev!(debug, "manager", "probe_busy", "通道操作中，跳过本轮检测", {"cid":cid});
                return Err("通道操作中，检测稍后重试".into());
            };
            let ch = store::get_channel(&db, &cid).map_err(|e| e.to_string())?
                .ok_or_else(|| "not found".to_string())?;
            let pending = crate::replacement_store::public_status(&db, &cid).map_err(|e| e.to_string())?;
            let target = verify_target::pick(&ch, &store::list_rules(&db, &cid).map_err(|e| e.to_string())?);
            (ch, slot.generation.load(Ordering::SeqCst), pending, crate::stop_intents::pending(&db, &cid).map_err(|e|e.to_string())?, target)
        };
        // 与内层入口同一判据，包括 custom 未通过明确验证目标的情况。
        let lost = lost_channels(&db);
        let blocked = crate::manager::channel_blocked(&ch, &lost);
        let target_text = target.as_ref().map(|t| t.as_str().to_string());
        if stop_pending {
            return Ok(json!({"status":"stopped","connected":false,"latency_ms":null,"checked_at":null,"stale":false,"replacement":pending,"stop_pending":true,"blocked":false,
                "awaiting_target":false,"target":target_text}));
        }
        if pending.as_ref().is_some_and(|p| !matches!(p["phase"].as_str(), Some("awaiting_login" | "committed" | "rolled_back"))) {
            return Ok(json!({"status":"error","connected":false,"latency_ms":null,"checked_at":null,"stale":true,"replacement":pending,"blocked":blocked,
                "awaiting_target":false,"target":target_text}));
        }
        if !matches!(ch.status.as_str(), "running" | "logged_in") {
            return Ok(json!({"status":ch.status,"connected":false,"latency_ms":null,"checked_at":null,"stale":false,"replacement":pending,"blocked":blocked,
                "awaiting_target":false,"target":target_text}));
        }
        // 没有验证目标:不探活,也不计失败次数(否则会被当成掉线阻止)。阻止的依据已经不在,在掉线
        // 集合里就移出;这里等不到探活结果,移出和 mutate 留下的 resync 在这里补重建。
        let Some(target) = target else {
            let Ok(_operation) = slot.operation.try_lock() else { return Err("通道操作中，检测稍后重试".into()); };
            if generation != slot.generation.load(Ordering::SeqCst) { continue; }
            let latest = store::get_channel(&db, &cid).map_err(|e| e.to_string())?.ok_or("not found")?;
            if !matches!(latest.status.as_str(), "running" | "logged_in") { continue; }
            let verification_was_lost = latest.status == "logged_in";
            if verification_was_lost {
                store::set_probe_result(&db, &cid, "running", None).map_err(|e| e.to_string())?;
                crate::ev!(info, "manager", "verification_target_missing", "验证目标已移除，通道等待重新验证", {"cid":cid});
            }
            let blocked = {
                let mut probe = slot.probe.lock().await;
                if generation != slot.generation.load(Ordering::SeqCst) { continue; }
                probe.cached = None;
                probe.failures = 0;
                if probe.lift(&cid) {
                    crate::ev!(info, "manager", "channel_unblocked", "通道没有验证目标，已解除阻止", { "cid": cid.as_str() });
                }
                if verification_was_lost && latest.vpn_type == "custom" { probe.resync = true; }
                if probe.resync { spawn_rebuild(&state, &slot, &mut probe); }
                let current = store::get_channel(&db, &cid).map_err(|e| e.to_string())?.ok_or("not found")?;
                crate::manager::channel_blocked(&current, &lost_channels(&db))
            };
            return Ok(json!({"status":"running","connected":false,"latency_ms":null,"checked_at":null,"stale":false,
                "replacement":pending,"blocked":blocked,"awaiting_target":true,"target":null}));
        };
        if !crate::runtime::serving(&state) {
            return Ok(json!({"status":ch.status,"connected":null,"probe_failed":true,"probe_reason":"运行环境未就绪",
                "latency_ms":null,"checked_at":null,"stale":true,"runtime_idle":true,"replacement":pending,
                "blocked":blocked,"awaiting_target":false,"target":target_text}));
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
            if probe.in_flight.is_some() {
                probe.background_requested |= background;
                probe.in_flight.as_ref().unwrap().clone()
            } else {
                probe.background_requested = background;
                let activity = state.lifecycle.runtime.activity().await?;
                let (sender, receiver) = watch::channel(None);
                probe.in_flight = Some(receiver.clone());
                let st = state.clone(); let slot = slot.clone(); let cid = cid.clone();
                let execute = execute.clone();
                tokio::spawn(async move {
                    let _activity = activity;
                    let stored_status = ch.status.clone();
                    let custom = ch.vpn_type == "custom";
                    let outcome = tokio::time::timeout(Duration::from_secs(32), execute(st.clone(), ch, target.clone())).await
                        .unwrap_or_else(|_| ProbeOutcome::failed("探活超时，连通状态待确认"));
                    let Ok(_guard) = slot.operation.try_lock() else {
                        slot.probe.lock().await.in_flight = None;
                        let _ = sender.send(Some((generation, Err("通道操作中，旧探活结果已丢弃".into()))));
                        return;
                    };
                    let current = generation == slot.generation.load(Ordering::SeqCst)
                        && !st.lifecycle.closing.load(Ordering::SeqCst);
                    if current && !matches!(outcome, ProbeOutcome::ProbeFailed { .. }) {
                        let ms = match &outcome { ProbeOutcome::Connected { latency } => *latency, _ => None };
                        push_latency(&mut slot.latency.lock().unwrap(), chrono::Utc::now().timestamp_millis(), ms);
                    }
                    let mut result = if current {
                        match outcome {
                            ProbeOutcome::ProbeFailed { reason } => Ok(json!({"status":stored_status,"connected":null,
                                "probe_failed":true,"probe_reason":reason,"latency_ms":null,
                                "checked_at":chrono::Utc::now().timestamp_millis(),"stale":false,
                                "replacement":pending,"awaiting_target":false,"target":target.as_str()})),
                            outcome => {
                                let (ok, ms) = match outcome { ProbeOutcome::Connected { latency } => (true, latency), _ => (false, None) };
                                crate::ev!(debug, "manager", "probe", "通道探活完成",
                                    { "cid": cid.as_str(), "ok": ok, "latency_ms": ms });
                                let status = if ok { "logged_in" } else { "running" };
                                (|| -> ProbeResult {
                                    store::set_probe_result(&st.cfg.db_path(), &cid, status, ms).map_err(|e| e.to_string())?;
                                    if ok {
                                        if let Err(error) = crate::replacement::confirmed(&st, &cid) {
                                            crate::ev!(warn,"replacement","confirm_pending","替换确认待重试",{"cid":cid,"error":error.to_string()});
                                        }
                                    }
                                    let pending = crate::replacement_store::public_status(&st.cfg.db_path(), &cid).map_err(|e| e.to_string())?;
                                    if pending.as_ref().is_some_and(|p| matches!(p["phase"].as_str(), Some("committed" | "rolled_back"))) {
                                        crate::replacement::schedule_cleanup(&st, &cid);
                                    }
                                    Ok(json!({"status":status,"connected":ok,"latency_ms":ms,
                                        "checked_at":chrono::Utc::now().timestamp_millis(),"stale":false,"replacement":pending,
                                        "awaiting_target":false,"target":target.as_str()}))
                                })()
                            }
                        }
                    } else { Err("通道已变更,旧探活结果已丢弃".into()) };
                    let mut probe = slot.probe.lock().await;
                    if current {
                        if let Ok(value) = &mut result {
                            if value["probe_failed"] == true {
                                let blocked = store::get_channel(&st.cfg.db_path(), &cid).ok().flatten()
                                    .map(|current| crate::manager::channel_blocked(&current, &lost_channels(&st.cfg.db_path())))
                                    .unwrap_or(true);
                                value["blocked"] = json!(blocked);
                                probe.cached = Some((generation, Instant::now() + Duration::from_secs(5), value.clone()));
                                probe.in_flight = None;
                                let _ = sender.send(Some((generation, result)));
                                return;
                            }
                            let ok = value["connected"].as_bool() == Some(true);
                            if ok {
                                if let Target::Host(host) = &target { probe.handshake_ok.insert(host.clone()); }
                            }
                            // 从没成功过的握手目标,失败不计数:状态照常是待登录,但不会被阻止;它也不能
                            // 作为阻止的依据,通道因之前的目标在掉线集合里就移出。
                            let counted = probe.verified(&target);
                            if probe.background_requested {
                                probe.failures = if ok { 0 } else if counted { probe.failures.saturating_add(1) } else { probe.failures };
                            }
                            if !counted && probe.lift(&cid) {
                                crate::ev!(info, "manager", "channel_unblocked", "通道的验证目标还没成功过，已解除阻止", { "cid": cid.as_str() });
                            }
                            // 只在跨过阈值(进、出掉线集合)或有没补上的重建时热加载,不是每次探活都重载。
                            let crossed = if ok && probe.background_requested {
                                probe.lost.take().is_some_and(|path| LOST.lock().unwrap().remove(&(path, cid.clone())))
                            } else if probe.background_requested && probe.failures >= LOST_AFTER && probe.lost.is_none() {
                                let db = st.cfg.db_path();
                                probe.lost = Some(db.clone());
                                LOST.lock().unwrap().insert((db, cid.clone()))
                            } else { false };
                            if crossed {
                                if ok {
                                    crate::ev!(info, "manager", "channel_unblocked", "通道检测恢复，已恢复分流", { "cid": cid.as_str() });
                                } else {
                                    crate::ev!(warn, "manager", "channel_blocked", "通道连续检测失败，已暂时阻止它的规则", { "cid": cid.as_str(), "failures": probe.failures });
                                }
                            }
                            // custom 的默认 SOCKS 路由不能证明 VPN 已连接：首次成功才放行，
                            // 明确不通立即收紧。探针执行故障在上面已提前返回，不会误阻断。
                            let custom_gate_changed = custom && ((ok && stored_status != "logged_in") || (!ok && stored_status == "logged_in"));
                            if crossed || probe.resync || custom_gate_changed { spawn_rebuild(&st, &slot, &mut probe); }
                            let blocked = store::get_channel(&st.cfg.db_path(), &cid).ok().flatten()
                                .map(|current| crate::manager::channel_blocked(&current, &lost_channels(&st.cfg.db_path())))
                                .unwrap_or(true);
                            value["blocked"] = json!(blocked);
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

/// `GET /api/channels/:cid/latency` → `{"points":[[毫秒时间戳, 延迟毫秒|null], …]}`,最近一小时。
pub async fn latency(State(st): State<AppState>, UrlPath(cid): UrlPath<String>) -> axum::response::Response {
    match store::get_channel(&st.cfg.db_path(), &cid) {
        Ok(Some(_)) => axum::Json(json!({"points": st.lifecycle.latency_points(&cid)})).into_response(),
        Ok(None) => crate::api::err404("not found"),
        Err(e) => crate::api::err_detail(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
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
        // 掉线阻止会触发热加载;控制口显式指向不可达地址,不依赖默认值。
        cfg.mihomo_ctrl_url = "http://127.0.0.1:1".into();
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
        let run = { let count=count.clone(); let gate=gate.clone(); move |_, _, _| {
            let count=count.clone(); let gate=gate.clone(); async move {
                count.fetch_add(1, Ordering::SeqCst);
                gate.acquire().await.unwrap().forget(); ProbeOutcome::Connected { latency: Some(12) }
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
    async fn execution_failures_preserve_status_counts_and_rules() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        store::add_rule(&db, "c1", "domain", "corp.example.test").unwrap();
        store::set_probe_result(&db, "c1", "logged_in", Some(17)).unwrap();
        let before = attempts(&db);
        for _ in 0..5 {
            let value = sample_with_origin(st.clone(), "c1".into(), true, true,
                |_, _, _| async { ProbeOutcome::failed("fixture exec unavailable") }).await.unwrap();
            assert_eq!(value["connected"], Value::Null);
            assert_eq!(value["probe_failed"], true);
            assert_eq!(value["status"], "logged_in");
            assert_eq!(value["blocked"], false);
        }
        let slot = st.lifecycle.slot("c1");
        let probe = slot.probe.lock().await;
        assert_eq!(probe.failures, 0);
        let remaining = probe.cached.as_ref().unwrap().1.duration_since(Instant::now());
        assert!(remaining <= Duration::from_secs(5));
        assert!(remaining > Duration::from_secs(4));
        assert_eq!(store::get_channel(&db, "c1").unwrap().unwrap().latency_ms, Some(17));
        assert!(st.lifecycle.latency_points("c1").is_empty());
        assert_eq!(attempts(&db), before);
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,ch-c1");
    }

    #[tokio::test]
    async fn custom_rules_open_only_after_success_and_close_on_confirmed_failure() {
        let (_dir, st) = fixture();
        let st = settling(st).await;
        let db = st.cfg.db_path();
        rusqlite::Connection::open(&db).unwrap().execute(
            "UPDATE channels SET vpn_type='custom',probe_url='https://intra.example.test/health',status='running' WHERE id='c1'", []).unwrap();
        store::add_rule(&db, "c1", "domain", "intra.example.test").unwrap();
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,intra.example.test,REJECT");
        let unknown = sample_with_origin(st.clone(), "c1".into(), true, true,
            |_, _, _| async { ProbeOutcome::failed("fixture exec unavailable") }).await.unwrap();
        assert_eq!((unknown["status"].as_str(), unknown["blocked"].as_bool()), (Some("running"), Some(true)));

        let before = attempts(&db);
        let connected = probe_once(&st, true).await;
        assert_eq!((connected["status"].as_str(), connected["blocked"].as_bool()), (Some("logged_in"), Some(false)));
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,intra.example.test,ch-c1");
        wait_rebuild(&db, before).await;
        let unknown = sample_with_origin(st.clone(), "c1".into(), true, true,
            |_, _, _| async { ProbeOutcome::failed("fixture exec unavailable") }).await.unwrap();
        assert_eq!((unknown["status"].as_str(), unknown["blocked"].as_bool()), (Some("logged_in"), Some(false)));

        let before = attempts(&db);
        let disconnected = probe_once(&st, false).await;
        assert_eq!((disconnected["status"].as_str(), disconnected["blocked"].as_bool()), (Some("running"), Some(true)));
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,intra.example.test,REJECT");
        wait_rebuild(&db, before).await;
        st.lifecycle.begin_closing();
    }

    #[tokio::test]
    async fn confirming_new_target_discards_old_inflight_probe_result() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        let started = Arc::new(tokio::sync::Semaphore::new(0));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let run = { let started = started.clone(); let gate = gate.clone(); move |_, _, _| {
            let started = started.clone(); let gate = gate.clone(); async move {
                started.add_permits(1);
                gate.acquire().await.unwrap().forget();
                ProbeOutcome::NotConnected
            }
        }};
        let old = tokio::spawn(sample_with(st.clone(), "c1".into(), true, run));
        started.acquire().await.unwrap().forget();

        let operation = tokio::time::timeout(Duration::from_secs(1), st.lifecycle.access("c1")).await
            .expect("old probe retained operation lock").unwrap();
        store::confirm_probe_target(&db, "c1", None, "https://new.example.test/", Some(7)).unwrap();
        tokio::time::timeout(Duration::from_secs(1), st.lifecycle.target_confirmed_locked("c1")).await
            .expect("old probe retained probe-state lock");
        drop(operation);
        gate.add_permits(1);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let channel = store::get_channel(&db, "c1").unwrap().unwrap();
        assert_eq!((channel.probe_url.as_str(), channel.status.as_str(), channel.latency_ms),
            ("https://new.example.test/", "logged_in", Some(7)));
        old.abort();
    }

    #[tokio::test]
    async fn page_probes_do_not_accelerate_background_failure_threshold() {
        let (_dir, st) = fixture();
        let st = settling(st).await;
        for _ in 0..5 {
            let value = sample_with(st.clone(), "c1".into(), true, |_, _, _| async { ProbeOutcome::NotConnected }).await.unwrap();
            assert_eq!(value["blocked"], false);
        }
        assert_eq!(st.lifecycle.slot("c1").probe.lock().await.failures, 0);
        for _ in 0..2 { assert_eq!(probe_once(&st, false).await["blocked"], false); }
        sample_with(st.clone(), "c1".into(), true, |_, _, _| async { ProbeOutcome::NotConnected }).await.unwrap();
        assert_eq!(st.lifecycle.slot("c1").probe.lock().await.failures, 2);
        assert_eq!(probe_once(&st, false).await["blocked"], true);
        let page_success = sample_with(st.clone(), "c1".into(), true,
            |_, _, _| async { ProbeOutcome::Connected { latency: Some(3) } }).await.unwrap();
        assert_eq!(page_success["blocked"], true, "page result does not cross the background threshold");
        assert_eq!(st.lifecycle.slot("c1").probe.lock().await.failures, 3);
        assert_eq!(probe_once(&st, true).await["blocked"], false);
        st.lifecycle.begin_closing();
    }

    #[tokio::test]
    async fn locked_channel_is_skipped_without_holding_other_samples() {
        let (_dir, st) = fixture();
        rusqlite::Connection::open(st.cfg.db_path()).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,probe_url,status) VALUES('c2','other','atrust','http://fixture.test','running')", []
        ).unwrap();
        let _guard = st.lifecycle.access("c1").await.unwrap();
        let busy = sample_with_origin(st.clone(), "c1".into(), true, true, |_, _, _| async { panic!("busy channel probed") }).await;
        assert!(busy.unwrap_err().contains("操作中"));
        let other = sample_with_origin(st.clone(), "c2".into(), true, true,
            |_, _, _| async { ProbeOutcome::Connected { latency: Some(8) } }).await.unwrap();
        assert_eq!(other["connected"], true);
    }

    #[tokio::test]
    async fn missing_target_clears_stored_login_once_even_when_runtime_is_idle() {
        let (_dir, mut st) = fixture();
        Arc::make_mut(&mut st.cfg).managed_vm = true;
        let db = st.cfg.db_path();
        add_untargeted(&st, "g1-target");
        store::set_probe_result(&db, "g1-target", "logged_in", Some(9)).unwrap();
        let events = || crate::events::snapshot(0, usize::MAX, &crate::events::Filter::default()).0.into_iter()
            .filter(|e| e.event == "verification_target_missing" && e.detail["cid"] == "g1-target").count();
        let before = events();
        for _ in 0..3 {
            let value = sample_with(st.clone(), "g1-target".into(), true, |_, _, _| async { panic!("missing target probed") }).await.unwrap();
            assert_eq!(value["status"], "running");
            assert_eq!(value["awaiting_target"], true);
        }
        let ch = store::get_channel(&db, "g1-target").unwrap().unwrap();
        assert_eq!(ch.status, "running");
        assert_eq!(ch.latency_ms, None);
        assert_eq!(events() - before, 1);
    }

    #[tokio::test]
    async fn management_fault_gate_does_not_run_probe_containers() {
        let (_dir, st) = fixture();
        st.health.lock().unwrap().gateway_health = crate::health::GatewayHealth::TransportDegraded;
        assert_eq!(probe_unavailable(&st).await, Some("分流链路状态待确认"));
        st.health.lock().unwrap().gateway_health = crate::health::GatewayHealth::Healthy;
        let socket = st.cfg.data_dir.join("absent.sock");
        std::fs::write(&socket, b"not a socket").unwrap();
        st.set_docker(Some(bollard::Docker::connect_with_unix(socket.to_str().unwrap(), 1, bollard::API_DEFAULT_VERSION).unwrap()));
        assert_eq!(probe_unavailable(&st).await, Some("容器管理连接不可用"));
        let result = sample(st.clone(), "c1".into(), true).await.unwrap();
        assert_eq!(result["probe_failed"], true);
        assert_eq!(st.lifecycle.slot("c1").probe.lock().await.failures, 0);
    }

    #[test]
    fn rebuild_retries_have_bounded_backoff() {
        let delays: Vec<_> = (1..=6).map(|n| rebuild_delay(n).as_secs()).collect();
        assert_eq!(delays, [5, 10, 20, 60, 60, 60]);
    }

    #[tokio::test]
    async fn rebuild_requests_merge_while_application_is_locked() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        let hold = crate::config_apply::application_lock(&db).await.unwrap();
        let slot = st.lifecycle.slot("c1");
        let before = attempts(&db);
        for _ in 0..20 {
            spawn_rebuild(&st, &slot, &mut *slot.probe.lock().await);
            tokio::task::yield_now().await;
        }
        assert!(st.lifecycle.rebuild.lock().unwrap().running);
        assert_eq!(attempts(&db), before);
        drop(hold);
        wait_resync(&st, "c1").await;
        assert_eq!(attempts(&db), before + 1, "one failed application, not twenty queued reloads");
        assert_eq!(st.lifecycle.rebuild.lock().unwrap().failures, 1);
        st.lifecycle.begin_closing();
    }

    #[tokio::test]
    async fn removed_target_discards_older_successful_probe() {
        let (_dir, st) = fixture();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (ready, mut started) = watch::channel(false);
        let run = { let gate = gate.clone(); move |_, _, _| {
            let gate = gate.clone(); let ready = ready.clone();
            async move { ready.send(true).unwrap(); gate.acquire().await.unwrap().forget(); ProbeOutcome::Connected { latency: Some(7) } }
        }};
        let task = tokio::spawn(sample_with(st.clone(), "c1".into(), true, run));
        started.changed().await.unwrap();
        {
            let _guard = st.lifecycle.mutate("c1").await.unwrap();
            rusqlite::Connection::open(st.cfg.db_path()).unwrap().execute("UPDATE channels SET probe_url='' WHERE id='c1'", []).unwrap();
        }
        gate.add_permits(1);
        let value = task.await.unwrap().unwrap();
        assert_eq!(value["awaiting_target"], true);
        assert_eq!(value["status"], "running");
        assert_eq!(store::get_channel(&st.cfg.db_path(), "c1").unwrap().unwrap().status, "running");
    }

    #[tokio::test]
    async fn slow_probe_cannot_resurrect_stopped_channel() {
        let (_dir, st)=fixture();
        let (started_tx, mut started_rx)=watch::channel(false);
        let gate=Arc::new(tokio::sync::Semaphore::new(0));
        let run={let gate=gate.clone();move |_, _, _| {let gate=gate.clone(); let tx=started_tx.clone();async move {
            tx.send(true).unwrap(); gate.acquire().await.unwrap().forget();ProbeOutcome::Connected { latency: Some(1) }
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
    async fn stalled_channel_does_not_block_or_invalidate_another_channel() {
        let (_dir, st) = fixture();
        rusqlite::Connection::open(st.cfg.db_path()).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,probe_url,status) VALUES('c2','other','atrust','http://fixture.test','running')", []
        ).unwrap();
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (started_tx, mut started_rx) = watch::channel(false);
        let run = { let gate = gate.clone(); move |_, _, _| {
            let gate = gate.clone(); let tx = started_tx.clone();
            async move { tx.send(true).unwrap(); gate.acquire().await.unwrap().forget(); ProbeOutcome::Connected { latency: Some(1) } }
        }};
        let slow = tokio::spawn(sample_with(st.clone(), "c1".into(), true, run));
        started_rx.changed().await.unwrap();
        let healthy = tokio::time::timeout(Duration::from_millis(500),
            sample_with(st.clone(), "c2".into(), true, |_, _, _| async { ProbeOutcome::Connected { latency: Some(7) } })).await.unwrap().unwrap();
        {
            let _guard = st.lifecycle.mutate("c1").await.unwrap();
            store::set_status(&st.cfg.db_path(), "c1", "stopped").unwrap();
        }
        gate.add_permits(1);
        assert_eq!(slow.await.unwrap().unwrap()["status"], "stopped");
        let unchanged = sample_with(st.clone(), "c2".into(), false, |_, _, _| async { panic!("unrelated channel cache invalidated") }).await.unwrap();
        assert_eq!(healthy, unchanged);
        assert_eq!(store::get_channel(&st.cfg.db_path(), "c2").unwrap().unwrap().status, "logged_in");
    }

    #[tokio::test]
    async fn polling_uses_cache_but_manual_probe_is_fresh() {
        let (_dir,st)=fixture(); let count=Arc::new(AtomicUsize::new(0));
        let run={let count=count.clone();move |_,_,_| {let count=count.clone();async move {
            count.fetch_add(1,Ordering::SeqCst);ProbeOutcome::NotConnected
        }}};
        sample_with(st.clone(),"c1".into(),false,run.clone()).await.unwrap();
        sample_with(st.clone(),"c1".into(),false,run.clone()).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst),1);
        sample_with(st,"c1".into(),true,run).await.unwrap();
        assert_eq!(count.load(Ordering::SeqCst),2);
    }

    #[tokio::test]
    async fn dormant_runtime_does_not_probe_or_report_stored_login_as_connected() {
        let (_dir, mut st) = fixture();
        Arc::make_mut(&mut st.cfg).managed_vm = true;
        store::set_probe_result(&st.cfg.db_path(), "c1", "logged_in", Some(1)).unwrap();
        let value = sample_with(st.clone(), "c1".into(), true, |_, _, _| async { panic!("dormant runtime probed") }).await.unwrap();
        assert_eq!(value["connected"], Value::Null);
        assert_eq!(value["probe_failed"], true);
        assert_eq!(value["status"], "logged_in");
        assert_eq!(value["runtime_idle"], true);
        assert_eq!(value["stale"], true);
        assert_eq!(store::get_channel(&st.cfg.db_path(), "c1").unwrap().unwrap().status, "logged_in");
    }

    #[tokio::test]
    async fn expired_health_returns_stale_while_refreshing() {
        let (_dir,st)=fixture();
        let first=sample_with(st.clone(),"c1".into(),false,|_,_,_| async {ProbeOutcome::Connected { latency: Some(1) }}).await.unwrap();
        let slot=st.lifecycle.slot("c1");
        slot.probe.lock().await.cached.as_mut().unwrap().1=Instant::now();
        let gate=Arc::new(tokio::sync::Semaphore::new(0));
        let run={let gate=gate.clone();move |_,_,_| {let gate=gate.clone();async move {
            gate.acquire().await.unwrap().forget();ProbeOutcome::NotConnected
        }}};
        let stale=sample_with(st.clone(),"c1".into(),false,run).await.unwrap();
        assert_eq!(stale["stale"],true);
        assert_eq!(stale["checked_at"],first["checked_at"]);
        let mut pending=slot.probe.lock().await.in_flight.clone().unwrap();
        gate.add_permits(1);
        wait_probe(&mut pending).await.unwrap().1.unwrap();
        let refreshed=sample_with(st,"c1".into(),false,|_,_,_| async {panic!("cache missed")}).await.unwrap();
        assert_eq!(refreshed["stale"],false);
        assert_eq!(refreshed["connected"],false);
    }

    fn inner_rules(db: &Path) -> Vec<String> {
        let snap = crate::config_apply_store::snapshot(db, false).unwrap();
        manager::build_mihomo_config_with(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            &snap.channels, &snap.rules, &lost_channels(db))["rules"]
            .as_sequence().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
    }

    fn attempts(db: &Path) -> i64 {
        crate::config_apply_store::status(db, false).unwrap().attempt
    }

    /// 后台重建是 spawn 出去的;等它走到配置应用记录(控制口不可达,读回失败即返回)。
    async fn wait_rebuild(db: &Path, before: i64) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while attempts(db) <= before { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("threshold crossing did not trigger a rebuild");
    }

    async fn probe_once(st: &AppState, ok: bool) -> Value {
        probe_cid(st, "c1", ok).await
    }

    async fn probe_cid(st: &AppState, cid: &str, ok: bool) -> Value {
        sample_with_origin(st.clone(), cid.into(), true, true, move |_, _, _| async move {
            if ok { ProbeOutcome::Connected { latency: Some(9) } } else { ProbeOutcome::NotConnected }
        }).await.unwrap()
    }

    /// 后台重建落在「延后」(托管的运行环境在服务、没有容器引擎连接):算已处理,不留 resync,
    /// 也不连控制口。夹具默认的重建因控制口不可达总是失败，改用单工作者退避重试。
    async fn settling(mut st: AppState) -> AppState {
        Arc::make_mut(&mut st.cfg).managed_vm = true;
        st.lifecycle.runtime().ensure(|| async { Ok(()) }).await.unwrap();
        st
    }

    /// 等后台工作者记录第一次失败并保留待重建标记。
    async fn wait_resync(st: &AppState, cid: &str) {
        let _ = cid;
        tokio::time::timeout(Duration::from_secs(10), async {
            while st.lifecycle.rebuild.lock().unwrap().failures == 0 { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.expect("failed rebuild did not retain pending work");
    }

    #[tokio::test]
    async fn third_consecutive_failure_blocks_and_recovery_unblocks() {
        // R-06:前两次失败入口配置不变仍走通道,第 3 次后阻止;只在跨过阈值时热加载。
        let (_dir, st) = fixture();
        let st = settling(st).await;
        let db = st.cfg.db_path();
        store::add_rule(&db, "c1", "domain", "corp.example.test").unwrap();
        store::set_probe_result(&db, "c1", "logged_in", Some(5)).unwrap();
        let start = attempts(&db);
        for n in 1..=2 {
            let value = probe_once(&st, false).await;
            assert_eq!(value["blocked"], false, "failure {n} must not block");
            assert!(lost_channels(&db).is_empty());
            assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,ch-c1");
        }
        assert_eq!(store::get_channel(&db, "c1").unwrap().unwrap().status, "running");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), start, "no reload before the threshold");

        let value = probe_once(&st, false).await;
        assert_eq!(value["blocked"], true);
        assert_eq!(lost_channels(&db), HashSet::from(["c1".to_string()]));
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,REJECT");
        wait_rebuild(&db, start).await;

        let blocked_at = attempts(&db);
        assert_eq!(probe_once(&st, false).await["blocked"], true);
        let cached = sample_with(st.clone(), "c1".into(), false, |_, _, _| async { panic!("cache missed") }).await.unwrap();
        assert_eq!(cached["blocked"], true);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), blocked_at, "staying blocked does not reload again");

        let value = probe_once(&st, true).await;
        assert_eq!(value["blocked"], false);
        assert!(lost_channels(&db).is_empty());
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,ch-c1");
        wait_rebuild(&db, blocked_at).await;
    }

    #[tokio::test]
    async fn channel_operation_restarts_count_and_lifts_block() {
        let (_dir, st) = fixture();
        let st = settling(st).await;
        let db = st.cfg.db_path();
        store::add_rule(&db, "c1", "ip", "10.30.0.0/16").unwrap();
        let start = attempts(&db);
        for _ in 0..3 { probe_once(&st, false).await; }
        assert!(lost_channels(&db).contains("c1"));
        wait_rebuild(&db, start).await;

        let before = attempts(&db);
        drop(st.lifecycle.mutate("c1").await.unwrap());
        assert!(lost_channels(&db).is_empty());
        // 调用方(例如只改名字)未必重建:下一次探活结果补一次,入口不留阻止行。
        let value = probe_once(&st, false).await;
        assert_eq!(value["blocked"], false, "count restarts: this is failure 1, not 4");
        assert_eq!(inner_rules(&db)[0], "IP-CIDR,10.30.0.0/16,ch-c1,no-resolve");
        wait_rebuild(&db, before).await;
        let resynced = attempts(&db);
        assert_eq!(probe_once(&st, false).await["blocked"], false);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), resynced, "resync runs once");
        assert_eq!(probe_once(&st, false).await["blocked"], true);
        assert_eq!(inner_rules(&db)[0], "IP-CIDR,10.30.0.0/16,REJECT,no-resolve");
    }

    #[tokio::test]
    async fn errored_channel_reports_blocked_without_probe_and_stopped_does_not() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        store::set_status(&db, "c1", "error").unwrap();
        let value = sample_with(st.clone(), "c1".into(), true, |_, _, _| async { panic!("error channel probed") }).await.unwrap();
        assert_eq!(value["status"], "error");
        assert_eq!(value["blocked"], true);
        store::set_status(&db, "c1", "stopped").unwrap();
        let value = sample_with(st.clone(), "c1".into(), true, |_, _, _| async { panic!("stopped channel probed") }).await.unwrap();
        assert_eq!(value["blocked"], false);
    }

    #[test]
    fn sampler_only_picks_running_channels() {
        let channel = |id: &str, status: &str| store::ChannelPublic {
            id: id.into(), name: "n".into(), vpn_type: "easyconnect".into(), server: "".into(),
            ec_ver: None, login_method: "interactive".into(), username: "".into(),
            vnc_password: None, mac: None, novnc_port: None, probe_url: "".into(),
            status: status.into(), container_id: None, latency_ms: None, config: json!({}),
            routing_enabled: true,
        };
        let channels = ["running", "logged_in", "stopped", "error", "creating", ""].iter().enumerate()
            .map(|(i, status)| channel(&format!("c{i}"), status)).collect::<Vec<_>>();
        assert_eq!(sampled_channels(&channels), vec!["c0", "c1"]);
        assert!(sampled_channels(&channels[2..]).is_empty(), "all stopped: nothing sampled");
    }

    /// 没填内网验证地址的无头通道。
    fn add_untargeted(st: &AppState, cid: &str) {
        rusqlite::Connection::open(st.cfg.db_path()).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,probe_url,status) VALUES(?1,'untargeted','wireguard','','running')", [cid]
        ).unwrap();
    }

    #[tokio::test]
    async fn no_target_neither_probes_nor_counts() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        add_untargeted(&st, "c3");
        // 网段规则不算验证目标。
        store::add_rule(&db, "c3", "ip", "10.30.0.0/16").unwrap();
        let start = attempts(&db);
        for _ in 0..4 {
            let value = sample_with(st.clone(), "c3".into(), true, |_, _, _| async { panic!("probed without a target") }).await.unwrap();
            assert_eq!(value["awaiting_target"], true);
            assert_eq!(value["target"], Value::Null);
            assert_eq!(value["connected"], false);
            assert_eq!(value["status"], "running");
            assert_eq!(value["blocked"], false);
        }
        assert_eq!(st.lifecycle.slot("c3").probe.lock().await.failures, 0);
        assert!(lost_channels(&db).is_empty());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), start, "no reload without a target");
        assert_eq!(inner_rules(&db)[0], "IP-CIDR,10.30.0.0/16,ch-c3,no-resolve");
    }

    #[tokio::test]
    async fn handshake_target_counts_failures_only_after_first_success() {
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        add_untargeted(&st, "c3");
        store::add_rule(&db, "c3", "domain", "*.corp.example.test").unwrap();
        let handshake = |ok: bool| {
            let st = st.clone();
            async move {
                sample_with_origin(st, "c3".into(), true, true, move |_, _, target| async move {
                    assert_eq!(target, Target::Host("corp.example.test".into()));
                    if ok { ProbeOutcome::Connected { latency: Some(4) } } else { ProbeOutcome::NotConnected }
                }).await.unwrap()
            }
        };
        let start = attempts(&db);
        for _ in 0..3 {
            let value = handshake(false).await;
            assert_eq!(value["status"], "running");
            assert_eq!(value["awaiting_target"], false, "interactive channels must not land on the no-target screen");
            assert_eq!(value["target"], "corp.example.test");
            assert_eq!(value["blocked"], false);
        }
        assert!(lost_channels(&db).is_empty(), "never-succeeded handshake target must not block");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), start);
        assert!(!st.lifecycle.verified_target("c3", &Target::Host("corp.example.test".into())).await);

        assert_eq!(handshake(true).await["status"], "logged_in");
        assert!(st.lifecycle.verified_target("c3", &Target::Host("corp.example.test".into())).await);
        for n in 1..=2 { assert_eq!(handshake(false).await["blocked"], false, "failure {n} after success"); }
        assert_eq!(handshake(false).await["blocked"], true);
        assert!(lost_channels(&db).contains("c3"));
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,REJECT");
        wait_rebuild(&db, start).await;
    }

    #[tokio::test]
    async fn failed_background_rebuild_backs_off_until_recovery() {
        // 夹具的控制口不可达,重建总是失败。
        let (_dir, st) = fixture();
        let db = st.cfg.db_path();
        store::add_rule(&db, "c1", "ip", "10.30.0.0/16").unwrap();
        let start = attempts(&db);
        for _ in 0..3 { probe_once(&st, false).await; }
        assert!(lost_channels(&db).contains("c1"));
        wait_rebuild(&db, start).await;
        wait_resync(&st, "c1").await;
        // 第 4 次失败不跨阈值；页面/采样结果不能跳过退避。
        let failed_at = attempts(&db);
        assert_eq!(probe_once(&st, false).await["blocked"], true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(attempts(&db), failed_at, "page/probe result must not bypass backoff");
        retry_rebuild_after_recovery(&st);
        wait_rebuild(&db, failed_at).await;
        // 拿不到维护活动(退出中)时不重建，库级待重建标记保留。
        st.lifecycle.runtime().quiesce().await;
        let slot = st.lifecycle.slot("c1");
        let before = attempts(&db);
        {
            let mut probe = slot.probe.lock().await;
            probe.resync = false;
            spawn_rebuild(&st, &slot, &mut probe);
            assert!(!probe.resync);
            assert!(st.lifecycle.rebuild.lock().unwrap().pending);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), before);
    }

    #[tokio::test]
    async fn block_lifts_when_target_is_gone_or_never_verified() {
        let (_dir, st) = fixture();
        let st = settling(st).await;
        let db = st.cfg.db_path();
        let block = |cid: &'static str| {
            let st = st.clone(); let db = db.clone();
            async move {
                assert_eq!(probe_cid(&st, cid, true).await["status"], "logged_in");
                let start = attempts(&db);
                for _ in 0..3 { probe_cid(&st, cid, false).await; }
                assert!(lost_channels(&db).contains(cid));
                wait_rebuild(&db, start).await;
                attempts(&db)
            }
        };

        // 作为验证目标的域名规则被删掉,只剩网段:没有目标。
        add_untargeted(&st, "c3");
        let domain = store::add_rule(&db, "c3", "domain", "app.corp.example.test").unwrap();
        store::add_rule(&db, "c3", "ip", "10.20.0.0/16").unwrap();
        let blocked_at = block("c3").await;
        store::del_rule(&db, "c3", domain).unwrap();
        let value = sample_with(st.clone(), "c3".into(), true, |_, _, _| async { panic!("probed without a target") }).await.unwrap();
        assert_eq!(value["awaiting_target"], true);
        assert_eq!(value["blocked"], false);
        assert!(lost_channels(&db).is_empty());
        assert_eq!(inner_rules(&db)[0], "IP-CIDR,10.20.0.0/16,ch-c3,no-resolve");
        wait_rebuild(&db, blocked_at).await;
        let lifted_at = attempts(&db);
        sample_with(st.clone(), "c3".into(), true, |_, _, _| async { panic!("probed without a target") }).await.unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(attempts(&db), lifted_at, "the lift rebuilds once");

        // 删掉第一条域名规则,目标换成一个从没成功过的名字。
        add_untargeted(&st, "c4");
        let first = store::add_rule(&db, "c4", "domain", "app.corp.example.test").unwrap();
        store::add_rule(&db, "c4", "domain", "corp.example.test").unwrap();
        store::add_rule(&db, "c4", "ip", "10.40.0.0/16").unwrap();
        let blocked_at = block("c4").await;
        store::del_rule(&db, "c4", first).unwrap();
        let fail_new_target = || sample_with(st.clone(), "c4".into(), true, |_, _, target| async move {
            assert_eq!(target, Target::Host("corp.example.test".into()));
            ProbeOutcome::NotConnected
        });
        assert_eq!(fail_new_target().await.unwrap()["blocked"], false);
        assert!(!lost_channels(&db).contains("c4"));
        assert_eq!(inner_rules(&db)[0], "DOMAIN-SUFFIX,corp.example.test,ch-c4");
        wait_rebuild(&db, blocked_at).await;
        // 这个目标没成功过,之后的失败照旧不计数、不阻止。
        for _ in 0..3 { assert_eq!(fail_new_target().await.unwrap()["blocked"], false); }
        assert!(!lost_channels(&db).contains("c4"));
        assert_eq!(st.lifecycle.slot("c4").probe.lock().await.failures, 0);
    }

    #[test]
    fn latency_queue_keeps_only_the_last_hour() {
        let mut points = VecDeque::new();
        let t0 = 1_800_000_000_000;
        push_latency(&mut points, t0, Some(20));
        push_latency(&mut points, t0 + 1_000, None);
        push_latency(&mut points, t0 + LATENCY_WINDOW_MS, Some(30));
        assert_eq!(points.len(), 3, "exactly one hour old is kept");
        push_latency(&mut points, t0 + LATENCY_WINDOW_MS + 1, Some(40));
        assert_eq!(Vec::from(points.clone()), vec![(t0 + 1_000, None), (t0 + LATENCY_WINDOW_MS, Some(30)), (t0 + LATENCY_WINDOW_MS + 1, Some(40))]);
        trim_latency(&mut points, t0 + 3 * LATENCY_WINDOW_MS);
        assert!(points.is_empty());
    }

    #[tokio::test]
    async fn committed_probes_append_latency_and_stale_ones_do_not() {
        let (_dir, st) = fixture();
        assert!(st.lifecycle.latency_points("c1").is_empty(), "restart starts empty");
        probe_once(&st, true).await;
        probe_once(&st, false).await;
        let points = st.lifecycle.latency_points("c1");
        assert_eq!(points.iter().map(|p| p.1).collect::<Vec<_>>(), vec![Some(9), None]);
        assert!(points[0].0 <= points[1].0);
        // 代次变了的结果不提交,也不进曲线。
        let (started_tx, mut started_rx) = watch::channel(false);
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let run = { let gate = gate.clone(); move |_, _, _| { let gate = gate.clone(); let tx = started_tx.clone(); async move {
            tx.send(true).unwrap(); gate.acquire().await.unwrap().forget(); ProbeOutcome::Connected { latency: Some(1) }
        }}};
        let task = tokio::spawn(sample_with(st.clone(), "c1".into(), true, run));
        started_rx.changed().await.unwrap();
        { let _guard = st.lifecycle.mutate("c1").await.unwrap(); store::set_status(&st.cfg.db_path(), "c1", "stopped").unwrap(); }
        gate.add_permits(1);
        task.await.unwrap().unwrap();
        assert_eq!(st.lifecycle.latency_points("c1").len(), 2);
        // 没有验证目标:不探活,也不记点。
        add_untargeted(&st, "c3");
        sample_with(st.clone(), "c3".into(), true, |_, _, _| async { panic!("probed without a target") }).await.unwrap();
        assert!(st.lifecycle.latency_points("c3").is_empty());
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
