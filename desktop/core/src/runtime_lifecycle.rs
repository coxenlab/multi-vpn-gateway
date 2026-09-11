//! 按需底座的并发协调：共享启动、可取消的空闲等待、释放与新操作互斥、退出收口。
//! 不直接操作 VM。调用者持有 Activity 覆盖完整资源操作，再用 ensure 启动底座。
//! idle 的可释放判断还必须检查通道意图、登录租约及待恢复记录，不能只看活动计数。
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::watch;

type Outcome = Result<(), String>;
type Flight = watch::Receiver<Option<Outcome>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase { Dormant, Starting, Ready, Waiting, Releasing, Failed, Closing }

#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    pub phase: Phase,
    pub active_tasks: usize,
    pub start_attempt: u64,
    pub release_in_seconds: Option<u64>,
    pub error: Option<String>,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_age_seconds: Option<u64>,
}

struct State {
    phase: Phase,
    closing: bool,
    active: usize,
    revision: u64,
    attempt: u64,
    deadline: Option<Instant>,
    error: Option<String>,
    detail: String,
    detail_changed: Instant,
    flight: Option<Flight>,
}

pub struct Coordinator {
    state: Mutex<State>,
    changed: watch::Sender<u64>,
}

impl Default for Coordinator {
    fn default() -> Self {
        Self {
            state: Mutex::new(State { phase: Phase::Dormant, closing: false, active: 0,
                revision: 0, attempt: 0, deadline: None, error: None, detail: String::new(), detail_changed: Instant::now(), flight: None }),
            changed: watch::channel(0).0,
        }
    }
}

/// Activity 不随 HTTP future 自动延长；脱离请求的任务必须把它移入任务或在任务内取得。
pub struct Activity { coordinator: Arc<Coordinator> }

impl Drop for Activity {
    fn drop(&mut self) {
        let mut state = self.coordinator.state.lock().unwrap();
        state.active -= 1;
        self.coordinator.publish(&mut state);
    }
}

impl Coordinator {
    fn publish(&self, state: &mut State) {
        state.revision = state.revision.wrapping_add(1);
        self.changed.send_replace(state.revision);
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> Snapshot {
        let state = self.state.lock().unwrap();
        Snapshot {
            phase: if state.closing { Phase::Closing } else { state.phase },
            active_tasks: state.active, start_attempt: state.attempt,
            release_in_seconds: state.deadline.map(|at| at.saturating_duration_since(Instant::now()).as_secs()),
            error: state.error.clone(),
            detail: state.detail.clone(),
            progress_age_seconds: (state.phase == Phase::Starting && !state.closing)
                .then(|| now.saturating_duration_since(state.detail_changed).as_secs()),
        }
    }

    pub fn progress(&self, detail: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        let detail = detail.into();
        if state.detail == detail { return; }
        state.detail = detail;
        state.detail_changed = Instant::now();
        self.publish(&mut state);
    }

    /// 观测到底座不可用时，下一次显式连接可以重新初始化；不自行启动。
    pub fn unavailable(&self, error: impl Into<String>) {
        let mut state = self.state.lock().unwrap();
        if !state.closing && matches!(state.phase, Phase::Ready | Phase::Waiting) {
            state.phase = Phase::Failed;
            state.deadline = None;
            state.error = Some(error.into());
            self.publish(&mut state);
        }
    }

    /// 新操作取消等待；已经开始释放则等释放结束再交付许可，避免半途启动/操作 VM。
    pub async fn activity(self: &Arc<Self>) -> Result<Activity, String> {
        let mut changed = self.changed.subscribe();
        loop {
            {
                let mut state = self.state.lock().unwrap();
                if state.closing { return Err("应用正在退出".into()); }
                if state.phase != Phase::Releasing {
                    state.active += 1;
                    state.deadline = None;
                    if state.phase == Phase::Waiting { state.phase = Phase::Ready; }
                    self.publish(&mut state);
                    return Ok(Activity { coordinator: self.clone() });
                }
            }
            changed.changed().await.map_err(|_| "底座状态已关闭".to_string())?;
        }
    }

    /// 后台检查只占用资源，不续期用户的空闲等待；释放/退出开始后跳过本轮。
    pub fn maintenance(self: &Arc<Self>) -> Option<Activity> {
        let mut state = self.state.lock().unwrap();
        if state.closing || state.phase == Phase::Releasing { return None; }
        state.active += 1;
        self.publish(&mut state);
        Some(Activity { coordinator: self.clone() })
    }

    /// 同一轮启动的调用者共享成功或失败；请求取消、启动函数 panic 都不会卡死状态。
    /// 失败后只有下一次显式 ensure 才重试，不在等待者内部隐式循环启动。
    pub async fn ensure<F, Fut>(self: &Arc<Self>, start: F) -> Outcome
    where F: FnOnce() -> Fut + Send + 'static, Fut: Future<Output = Outcome> + Send + 'static {
        let _activity = self.activity().await?;
        let mut start = Some(start);
        loop {
            let (mut flight, was_release) = {
                let mut state = self.state.lock().unwrap();
                if state.closing { return Err("应用正在退出".into()); }
                match state.phase {
                    Phase::Ready | Phase::Waiting => return Ok(()),
                    Phase::Starting => (state.flight.clone().unwrap(), false),
                    Phase::Releasing => (state.flight.clone().unwrap(), true),
                    _ => {
                        state.phase = Phase::Starting;
                        state.attempt += 1;
                        state.error = None;
                        state.detail = "正在准备连接…".into();
                        state.detail_changed = Instant::now();
                        let (sender, receiver) = watch::channel(None);
                        state.flight = Some(receiver.clone());
                        self.publish(&mut state);
                        self.launch(sender, start.take().unwrap(), Phase::Ready);
                        (receiver, false)
                    }
                }
            };
            let result = wait_flight(&mut flight).await;
            if !was_release { return result; }
        }
    }

    fn launch<F, Fut>(self: &Arc<Self>, sender: watch::Sender<Option<Outcome>>, run: F, success: Phase)
    where F: FnOnce() -> Fut + Send + 'static, Fut: Future<Output = Outcome> + Send + 'static {
        let coordinator = self.clone();
        tokio::spawn(async move {
            let result = match tokio::spawn(async move { run().await }).await {
                Ok(result) => result,
                Err(error) => Err(format!("底座任务异常退出: {error}")),
            };
            let mut state = coordinator.state.lock().unwrap();
            state.phase = if result.is_ok() { success } else { Phase::Failed };
            state.error = result.as_ref().err().cloned();
            state.flight = None;
            state.deadline = None;
            // 发结果后才允许新一轮 ensure；旧等待者总是读取自己的 flight。
            sender.send_replace(Some(result));
            coordinator.publish(&mut state);
        });
    }

    /// 每轮重新核对业务是否空闲；核对期间有操作进入/退出则丢弃本轮结果。
    /// 返回 true 仅表示本调用启动的释放已完成，false 表示尚未达到释放条件。
    pub async fn release_if_idle<C, CF, R, RF>(self: &Arc<Self>, grace: Duration, can_release: C, release: R) -> Result<bool, String>
    where C: FnOnce() -> CF, CF: Future<Output = Result<bool, String>>,
          R: FnOnce() -> RF + Send + 'static, RF: Future<Output = Outcome> + Send + 'static {
        self.release_at(Instant::now(), grace, can_release, release).await
    }

    async fn release_at<C, CF, R, RF>(self: &Arc<Self>, now: Instant, grace: Duration, can_release: C, release: R) -> Result<bool, String>
    where C: FnOnce() -> CF, CF: Future<Output = Result<bool, String>>,
          R: FnOnce() -> RF + Send + 'static, RF: Future<Output = Outcome> + Send + 'static {
        let revision = {
            let state = self.state.lock().unwrap();
            if state.closing || state.active != 0 || !matches!(state.phase, Phase::Ready | Phase::Waiting) { return Ok(false); }
            state.revision
        };
        let eligible = can_release().await;
        let mut flight = {
            let mut state = self.state.lock().unwrap();
            if state.closing || state.active != 0 || state.revision != revision { return Ok(false); }
            if eligible.as_ref().ok() != Some(&true) {
                state.phase = Phase::Ready;
                state.deadline = None;
                state.error = eligible.as_ref().err().cloned();
                self.publish(&mut state);
                return eligible.map(|_| false);
            }
            state.error = None;
            let deadline = *state.deadline.get_or_insert(now + grace);
            if now < deadline {
                state.phase = Phase::Waiting;
                self.publish(&mut state);
                return Ok(false);
            }
            state.phase = Phase::Releasing;
            state.deadline = None;
            let (sender, receiver) = watch::channel(None);
            state.flight = Some(receiver.clone());
            self.publish(&mut state);
            self.launch(sender, release, Phase::Dormant);
            receiver
        };
        wait_flight(&mut flight).await.map(|_| true)
    }

    /// 永久拒绝新操作，等已接受的资源任务和启动/释放完成；总预算由退出流程控制。
    pub async fn quiesce(&self) {
        let mut changed = self.changed.subscribe();
        {
            let mut state = self.state.lock().unwrap();
            state.closing = true;
            state.deadline = None;
            self.publish(&mut state);
        }
        loop {
            {
                let state = self.state.lock().unwrap();
                if state.active == 0 && state.flight.is_none() { return; }
            }
            if changed.changed().await.is_err() { return; }
        }
    }
}

async fn wait_flight(receiver: &mut Flight) -> Outcome {
    loop {
        if let Some(result) = receiver.borrow().clone() { return result; }
        receiver.changed().await.map_err(|_| "底座任务未返回结果".to_string())?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Semaphore;

    fn coordinator() -> Arc<Coordinator> { Arc::new(Coordinator::default()) }
    async fn ready(c: &Arc<Coordinator>) { c.ensure(|| async { Ok(()) }).await.unwrap(); }
    async fn settle() { for _ in 0..20 { tokio::task::yield_now().await; } }

    #[tokio::test]
    async fn progress_age_observes_changes_and_resets_on_explicit_retry() {
        let c = coordinator();
        assert_eq!(c.snapshot().progress_age_seconds, None);
        let (signal, started) = tokio::sync::oneshot::channel();
        let gate = Arc::new(Semaphore::new(0));
        let task = tokio::spawn({ let c = c.clone(); let gate = gate.clone(); async move {
            c.ensure(move || async move {
                signal.send(()).unwrap(); gate.acquire().await.unwrap().forget(); Err("fixture failure".into())
            }).await
        }});
        started.await.unwrap();
        c.progress("正在下载运行环境文件…");
        let changed = c.state.lock().unwrap().detail_changed;
        c.progress("正在下载运行环境文件…");
        assert_eq!(c.state.lock().unwrap().detail_changed, changed);
        let quiet = c.snapshot_at(changed + Duration::from_secs(90));
        assert_eq!(quiet.phase, Phase::Starting); assert_eq!(quiet.progress_age_seconds, Some(90));
        assert_eq!(quiet.error, None); assert_eq!(quiet.start_attempt, 1);
        gate.add_permits(1); assert!(task.await.unwrap().is_err());
        assert_eq!(c.snapshot().progress_age_seconds, None);
        let retry = c.clone();
        c.ensure(move || async move {
            let snapshot = retry.snapshot();
            assert_eq!(snapshot.detail, "正在准备连接…");
            assert_eq!(snapshot.error, None); assert_eq!(snapshot.progress_age_seconds, Some(0));
            Ok(())
        }).await.unwrap();
        assert_eq!(c.snapshot().start_attempt, 2); assert_eq!(c.snapshot().progress_age_seconds, None);
    }

    #[tokio::test]
    async fn concurrent_boot_shares_result_even_when_first_request_is_cancelled() {
        let c = coordinator(); let gate = Arc::new(Semaphore::new(0)); let count = Arc::new(AtomicUsize::new(0));
        let mut calls = Vec::new();
        for _ in 0..8 {
            let (c, gate, count) = (c.clone(), gate.clone(), count.clone());
            calls.push(tokio::spawn(async move { c.ensure(move || async move {
                count.fetch_add(1, Ordering::SeqCst); gate.acquire().await.unwrap().forget(); Ok(())
            }).await }));
        }
        settle().await;
        assert_eq!(count.load(Ordering::SeqCst), 1);
        calls.remove(0).abort(); gate.add_permits(1);
        for call in calls { call.await.unwrap().unwrap(); }
        assert_eq!(c.snapshot().phase, Phase::Ready);
        assert_eq!(c.snapshot().active_tasks, 0);
        assert_eq!(c.snapshot().start_attempt, 1);
    }

    #[tokio::test]
    async fn failed_boot_is_shared_and_next_explicit_request_can_retry() {
        let c = coordinator(); let gate = Arc::new(Semaphore::new(0)); let mut calls = Vec::new();
        for _ in 0..8 {
            let (c, gate) = (c.clone(), gate.clone());
            calls.push(tokio::spawn(async move { c.ensure(move || async move {
                gate.acquire().await.unwrap().forget(); Err("fixture failure".into())
            }).await }));
        }
        settle().await; gate.add_permits(1);
        for call in calls { assert_eq!(call.await.unwrap(), Err("fixture failure".into())); }
        assert_eq!(c.snapshot().start_attempt, 1);
        ready(&c).await;
        assert_eq!(c.snapshot().start_attempt, 2);
    }

    #[tokio::test]
    async fn boot_panic_is_a_failure_and_does_not_block_quit() {
        let c = coordinator();
        assert!(c.ensure(|| async { panic!("fixture panic") }).await.unwrap_err().contains("异常退出"));
        assert_eq!(c.snapshot().phase, Phase::Failed);
        c.quiesce().await;
        assert_eq!(c.snapshot().phase, Phase::Closing);
    }

    #[tokio::test]
    async fn lost_runtime_waits_for_explicit_request_before_starting_again() {
        let c = coordinator(); ready(&c).await;
        c.unavailable("VM stopped");
        assert_eq!(c.snapshot().phase, Phase::Failed);
        assert_eq!(c.snapshot().start_attempt, 1);
        ready(&c).await;
        assert_eq!(c.snapshot().start_attempt, 2);
    }

    #[tokio::test]
    async fn live_work_blocks_release_and_new_intent_restarts_the_entire_grace_period() {
        let c = coordinator(); ready(&c).await;
        let now = Instant::now(); let grace = Duration::from_secs(60);
        let activity = c.activity().await.unwrap();
        assert!(!c.release_at(now, grace, || async { panic!("must not inspect while busy") }, || async { Ok(()) }).await.unwrap());
        drop(activity);
        assert!(!c.release_at(now, grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        assert_eq!(c.snapshot().phase, Phase::Waiting);
        drop(c.activity().await.unwrap());
        assert!(!c.release_at(now + grace, grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        assert!(!c.release_at(now + grace + Duration::from_secs(59), grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        assert!(c.release_at(now + grace * 2, grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        assert_eq!(c.snapshot().phase, Phase::Dormant);
    }

    #[tokio::test]
    async fn changed_intent_invalidates_slow_idle_check_and_check_failure_preserves_runtime() {
        let c = coordinator(); ready(&c).await;
        let other = c.clone();
        assert!(!c.release_if_idle(Duration::ZERO, move || async move {
            drop(other.activity().await.unwrap()); Ok(true)
        }, || async { panic!("stale idle result") }).await.unwrap());
        assert!(c.release_if_idle(Duration::ZERO, || async { Err("store unavailable".into()) }, || async { panic!("unconfirmed idle") }).await.is_err());
        assert_eq!(c.snapshot().phase, Phase::Ready);
        assert_eq!(c.snapshot().error.as_deref(), Some("store unavailable"));
    }

    #[tokio::test]
    async fn release_survives_request_cancel_and_new_start_waits_for_release() {
        let c = coordinator(); ready(&c).await;
        let gate = Arc::new(Semaphore::new(0));
        let release = { let (c, gate) = (c.clone(), gate.clone()); tokio::spawn(async move {
            c.release_if_idle(Duration::ZERO, || async { Ok(true) }, move || async move {
                gate.acquire().await.unwrap().forget(); Ok(())
            }).await
        }) };
        settle().await;
        assert_eq!(c.snapshot().phase, Phase::Releasing);
        release.abort();
        let start = { let c=c.clone(); tokio::spawn(async move { c.ensure(|| async { Ok(()) }).await }) };
        settle().await; assert!(!start.is_finished());
        gate.add_permits(1); start.await.unwrap().unwrap();
        assert_eq!(c.snapshot().phase, Phase::Ready);
        assert_eq!(c.snapshot().start_attempt, 2);
    }

    #[tokio::test]
    async fn quit_waits_for_accepted_work_and_boot_but_rejects_new_work() {
        let c = coordinator(); let activity = c.activity().await.unwrap();
        let gate = Arc::new(Semaphore::new(0));
        let boot = { let (c, gate) = (c.clone(), gate.clone()); tokio::spawn(async move {
            c.ensure(move || async move { gate.acquire().await.unwrap().forget(); Ok(()) }).await
        }) };
        settle().await; boot.abort();
        let quit = { let c=c.clone(); tokio::spawn(async move { c.quiesce().await }) };
        settle().await;
        assert!(!quit.is_finished()); assert!(c.activity().await.is_err());
        drop(activity); settle().await; assert!(!quit.is_finished());
        gate.add_permits(1); quit.await.unwrap();
        assert_eq!(c.snapshot().phase, Phase::Closing);
        assert!(c.ensure(|| async { panic!("quit must not reboot") }).await.is_err());
    }

    #[tokio::test]
    async fn maintenance_blocks_release_without_resetting_deadline() {
        let c = coordinator(); ready(&c).await;
        let now = Instant::now(); let grace = Duration::from_secs(60);
        assert!(!c.release_at(now, grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        let task = c.maintenance().unwrap();
        assert_eq!(c.snapshot().phase, Phase::Waiting);
        assert!(!c.release_at(now + grace, grace, || async { panic!("live maintenance") }, || async { Ok(()) }).await.unwrap());
        drop(task);
        assert!(c.release_at(now + grace, grace, || async { Ok(true) }, || async { Ok(()) }).await.unwrap());
        assert_eq!(c.snapshot().phase, Phase::Dormant);
    }

    #[tokio::test]
    async fn unconfirmed_release_is_failed_and_does_not_automatically_retry() {
        let c = coordinator(); ready(&c).await;
        assert!(c.release_if_idle(Duration::ZERO, || async { Ok(true) }, || async { Err("readback unavailable".into()) }).await.is_err());
        assert_eq!(c.snapshot().phase, Phase::Failed);
        assert!(!c.release_if_idle(Duration::ZERO, || async { panic!("no implicit retry") }, || async { Ok(()) }).await.unwrap());
        c.ensure(|| async { Ok(()) }).await.unwrap();
        assert_eq!(c.snapshot().phase, Phase::Ready);
    }
}
