//! 退出清理:按层级把运行面收干净——关 TUN → 关系统代理 → 停全部通道容器 → 停 VM。
//!
//! 语义与「关容器即关分流」(store::effective_rules)同一条层级公理:上层关闭,
//! 下层不得残留半死状态。app 退出后分流口 SSH 转发(kill_on_drop)必死,若不清理:
//! TUN 路由把命中网段变黑洞、外层 Clash 的 vpn-router 节点悬空、容器空耗资源。
//! 全程 best-effort:任何一步失败只记事件、不阻塞退出(用户要退出,不能锁死在清理上)。

use std::time::Duration;

use futures_util::future::join_all;

use crate::{app_settings, config::Config, entry, manager, stop_intents, store, vm, AppState};

/// 单个容器 stop 的等待上限(docker stop 本身有 SIGTERM→SIGKILL 宽限)。
const CONTAINER_STOP_TIMEOUT: Duration = Duration::from_secs(20);

/// 停全部运行中的通道容器(并发),状态落库 stopped——DB 与实际一致,
/// 下次启动通道按「已停止」呈现,由用户按需逐个启动(hagb/oss 启动 = 重建)。
pub(crate) async fn stop_all_channels(state: &AppState, cfg: &Config) -> bool {
    let db = cfg.db_path();
    let channels = match store::list_channels(&db) {
        Ok(list) => list,
        Err(e) => {
            crate::ev!(warn, "shutdown", "list_channels_failed", "退出清理:读取通道列表失败",
                { "error": e.to_string() });
            return false;
        }
    };
    let Some(docker) = state.docker() else {
        crate::ev!(warn, "shutdown", "docker_unavailable", "退出清理:docker 不可达,跳过停容器",
            { "skipped": channels.len() as i64 });
        return false;
    };
    let running: Vec<_> = channels.into_iter().filter(|c| c.status != "stopped").collect();
    if running.is_empty() {
        return true;
    }
    let tasks = running.iter().map(|ch| {
        let docker = docker.clone();
        let db = db.clone();
        let cid = ch.id.clone();
        async move {
            match tokio::time::timeout(CONTAINER_STOP_TIMEOUT, manager::stop(&docker, &cid)).await {
                Ok(Ok(())) => {
                    let _ = store::set_status(&db, &cid, "stopped");
                    true
                }
                Ok(Err(e)) => {
                    crate::ev!(warn, "shutdown", "container_stop_failed", "退出清理:容器停止失败",
                        { "cid": cid.as_str(), "error": e.to_string() });
                    false
                }
                Err(_) => {
                    crate::ev!(warn, "shutdown", "container_stop_timeout", "退出清理:容器停止超时",
                        { "cid": cid.as_str() });
                    false
                }
            }
        }
    });
    let stopped = join_all(tasks).await.into_iter().filter(|ok| *ok).count();
    crate::ev!(info, "shutdown", "containers_stopped", "退出清理:通道容器已停止",
        { "stopped": stopped as i64, "total": running.len() as i64 });
    stopped == running.len()
}

/// 纯函数:退出时在用的通道——状态为 running/logged_in、且没有停止意图。
pub fn in_use_ids(channels: &[store::ChannelPublic], stopping: impl Fn(&str) -> bool) -> Vec<String> {
    channels.iter()
        .filter(|c| matches!(c.status.as_str(), "running" | "logged_in") && !stopping(&c.id))
        .map(|c| c.id.clone())
        .collect()
}

/// 记下退出时在用的通道(方案 5.9「打开后自动连接」)。总是记录,用不用由开关决定(界面启动时做)。
/// 停止意图读不出时按「有」处理,不自动启动。旧身份没有这个开关,不写 `app-settings.json`(旧应用的行为不变)。
async fn remember_in_use(cfg: &Config) {
    if !std::ptr::eq(cfg.identity(), &crate::identity::TONGDAO) {
        return;
    }
    let db = cfg.db_path();
    let channels = match store::list_channels(&db) {
        Ok(list) => list,
        Err(e) => {
            crate::ev!(warn, "shutdown", "resume_record_failed", "退出清理:记录在用通道失败",
                { "error": e.to_string() });
            return;
        }
    };
    let ids = in_use_ids(&channels, |cid| stop_intents::pending(&db, cid).unwrap_or(true));
    let count = ids.len() as i64;
    match app_settings::set_resume_channels(&cfg.data_dir, ids).await {
        Ok(()) => {
            crate::ev!(info, "shutdown", "resume_recorded", "退出清理:已记下在用通道", { "channels": count });
        }
        Err(e) => {
            crate::ev!(warn, "shutdown", "resume_record_failed", "退出清理:记录在用通道失败",
                { "error": e.to_string() });
        }
    }
}

/// 关闭本工具启用的系统自动代理。遍历所有网络服务(不只当前默认那一个——用户换过网
/// 时旧服务上也可能残留指向本工具 PAC 的配置),只动确实指向本工具 PAC 的服务。
async fn park_system_proxy(cfg: &Config) {
    if !cfg.host_integrations_allowed() || !cfg!(target_os = "macos") {
        return;
    }
    let parked = entry::system_proxy_park_all(&cfg.ui_port.to_string()).await;
    if parked > 0 {
        crate::ev!(info, "shutdown", "system_proxy_parked",
            "退出清理:系统自动代理已关闭(退出后 PAC 不可达)", { "services": parked as i64 });
    }
}

/// 常规退出(托盘/菜单 Quit → ExitRequested 拦截)的总预算。
pub const NORMAL_BUDGET: Duration = Duration::from_secs(90);
/// terminate: 兜底路径(注销/关机等绕过 ExitRequested 的退出)的收紧预算——
/// 系统给 willTerminate 的时间有限,超了会被直接 SIGKILL。
pub const FALLBACK_BUDGET: Duration = Duration::from_secs(25);

/// 层级清理总入口(供壳的退出拦截调用)。`state` 为 None 表示 boot 未完成
/// (还没建出 AppState)。`stop_vm=false` 用于 boot 仍在进行时的退出——
/// 此刻 colima start 可能还在跑,再并发 colima stop 会把 lima 搞进半创建态
/// (红队 M7),宁可让 VM 留着由下次启动接管。带总预算兜底,超时放行退出。
pub async fn shutdown_all(state: Option<&AppState>, cfg: &Config, stop_vm: bool, budget: Duration) {
    if tokio::time::timeout(budget, run(state, cfg, stop_vm, budget)).await.is_err() {
        crate::ev!(warn, "shutdown", "shutdown_timeout", "退出清理总超时,放行退出", { "ok": false });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step { Tun, Proxy, Remember, Quiesce, Containers, Vm }

/// 每层独立预算，入口或在途任务卡住也不能吞掉其余层的清理机会。
/// 注销预算按比例收紧；VM 可以使用前面步骤节省的时间，但仍受总预算限制。
async fn run_steps<F, Fut>(budget: Duration, mut perform: F) -> Vec<Step>
where F: FnMut(Step) -> Fut, Fut: std::future::Future<Output = ()> {
    let deadline = tokio::time::Instant::now() + budget;
    let mut timed_out = Vec::new();
    for (step, seconds) in [(Step::Tun, 10), (Step::Proxy, 10), (Step::Remember, 1),
        (Step::Quiesce, 20), (Step::Containers, 20), (Step::Vm, 60)] {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let cap = if step == Step::Vm { Duration::from_secs(seconds) }
            else { budget.mul_f64(seconds as f64 / NORMAL_BUDGET.as_secs_f64()) };
        if tokio::time::timeout(remaining.min(cap), perform(step)).await.is_err() {
            timed_out.push(step);
            crate::ev!(warn, "shutdown", "shutdown_step_timeout", "退出清理步骤超时，继续后续清理",
                { "step": format!("{step:?}") });
        }
    }
    timed_out
}

async fn run(state: Option<&AppState>, cfg: &Config, stop_vm: bool, budget: Duration) {
    crate::ev!(info, "shutdown", "shutdown_begin", "退出清理开始",
        { "boot_ready": state.is_some(), "stop_vm": stop_vm });
    // 同步封住新操作；不在收入口前等待正在拉镜像、启动或释放的任务。
    entry::begin_shutdown();
    if let Some(st) = state { st.lifecycle.begin_closing(); }
    let timed_out = run_steps(budget, |step| async move {
        match step {
            Step::Tun => entry::tun_park(cfg).await,
            Step::Proxy => park_system_proxy(cfg).await,
            Step::Remember => { if state.is_some() { remember_in_use(cfg).await; } }
            Step::Quiesce => { if let Some(st) = state { st.lifecycle.quiesce().await; } }
            Step::Containers => { if let Some(st) = state { stop_all_channels(st, cfg).await; } }
            Step::Vm => {
                if let Some(st) = state {
                    if let Err(error) = crate::health::stop_docker_transport(st).await {
                        crate::ev!(warn, "shutdown", "docker_tunnel_stop_failed", "备援隧道停止尚未确认", {"error":error.to_string()});
                    }
                }
                if !stop_vm || state.is_some_and(|st| st.lifecycle.runtime().start_in_flight()) {
                    crate::ev!(warn, "shutdown", "vm_stop_skipped", "退出清理:boot 进行中,跳过停 VM(避免与 colima start 并发)", { "ok": true });
                } else {
                    match vm::stop(&cfg.vm_profile).await {
                        Ok(()) => { crate::ev!(info, "shutdown", "vm_stopped", "退出清理:VM 已停止", { "ok": true }); }
                        Err(e) => { crate::ev!(warn, "shutdown", "vm_stop_failed", "退出清理:VM 停止失败", { "error": e.to_string() }); }
                    }
                }
            }
        }
    }).await;
    crate::ev!(info, "shutdown", "shutdown_done", "退出清理流程结束",
        { "timed_out_steps": timed_out.iter().map(|step| format!("{step:?}")).collect::<Vec<_>>() });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stuck_work_cannot_delay_entry_cleanup_or_skip_later_layers() {
        let runtime = std::sync::Arc::new(crate::runtime_lifecycle::Coordinator::default());
        let activity = runtime.activity().await.unwrap();
        runtime.begin_closing();
        assert!(runtime.activity().await.is_err());
        let calls = std::sync::Mutex::new(Vec::new());
        let started = std::time::Instant::now();
        let timeouts = run_steps(Duration::from_millis(450), |step| {
            let (runtime, calls) = (&runtime, &calls);
            async move {
                calls.lock().unwrap().push(step);
                if step == Step::Quiesce { runtime.quiesce().await; }
            }
        }).await;
        assert_eq!(timeouts, [Step::Quiesce]);
        assert_eq!(*calls.lock().unwrap(), [Step::Tun, Step::Proxy, Step::Remember,
            Step::Quiesce, Step::Containers, Step::Vm]);
        assert!(started.elapsed() < Duration::from_secs(2));
        drop(activity);
        runtime.quiesce().await;
    }

    #[tokio::test]
    async fn entry_lock_timeout_still_allows_proxy_and_resource_cleanup() {
        let calls = std::sync::Mutex::new(Vec::new());
        let timeouts = run_steps(Duration::from_millis(450), |step| {
            let calls = &calls;
            async move {
                calls.lock().unwrap().push(step);
                if step == Step::Tun { std::future::pending::<()>().await; }
            }
        }).await;
        assert_eq!(timeouts, [Step::Tun]);
        assert_eq!(calls.lock().unwrap().last(), Some(&Step::Vm));
    }

    fn cfg(data_dir: &std::path::Path) -> Config {
        Config {
            vm_profile: "tongdao-test".into(),
            dev_mode: true,
            managed_vm: false,
            bundled_images_dir: None,
            ui_port: 18787,
            data_dir: data_dir.to_path_buf(),
            static_dir: data_dir.to_path_buf(),
            mihomo_ctrl_url: "http://127.0.0.1:1".into(),
            mihomo_secret: "".into(),
            mihomo_host_port: "17899".into(),
            mihomo_ctrl_port: None,
            vpn_net: "tongdao_test_vpnnet".into(),
        }
    }

    #[tokio::test]
    async fn remembers_running_and_logged_in_channels_without_stop_intent() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = cfg(dir.path());
        let db = cfg.db_path();
        store::init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (id, status) in [("a-run", "running"), ("b-in", "logged_in"), ("c-stop", "stopped"),
            ("d-err", "error"), ("e-new", "creating"), ("f-leaving", "logged_in")] {
            conn.execute("INSERT INTO channels(id,name,vpn_type,status) VALUES(?1,?1,'wireguard',?2)", [id, status]).unwrap();
        }
        conn.execute("INSERT INTO channel_stop_intents VALUES('f-leaving','pending')", []).unwrap();
        let path = app_settings::file_path(dir.path());
        app_settings::save(&path, &app_settings::AppSettings {
            auto_connect: true, resume_channels: vec!["old".into()], ..Default::default()
        }).unwrap();

        remember_in_use(&cfg).await;
        let saved = app_settings::load(&path);
        let mut ids = saved.resume_channels.clone();
        ids.sort();
        assert_eq!(ids, vec!["a-run".to_string(), "b-in".to_string()]);
        // 其余设置不变。
        assert!(saved.auto_connect && saved.keep_running_on_close && !saved.launch_at_login);

        // 没有在用的通道时也记录(清空上一次的名单)。
        conn.execute("UPDATE channels SET status='stopped'", []).unwrap();
        remember_in_use(&cfg).await;
        assert!(app_settings::load(&path).resume_channels.is_empty());
    }

    #[tokio::test]
    async fn legacy_identity_exit_writes_no_app_settings() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config { vm_profile: "vpnmgr-test".into(), vpn_net: "vpnmgr_test_vpnnet".into(), ..cfg(dir.path()) };
        assert!(std::ptr::eq(cfg.identity(), &crate::identity::VPNMGR));
        let db = cfg.db_path();
        store::init(&db).unwrap();
        let conn = rusqlite::Connection::open(&db).unwrap();
        for (id, status) in [("a-run", "running"), ("b-in", "logged_in")] {
            conn.execute("INSERT INTO channels(id,name,vpn_type,status) VALUES(?1,?1,'wireguard',?2)", [id, status]).unwrap();
        }
        remember_in_use(&cfg).await;
        assert!(!app_settings::file_path(dir.path()).exists());
    }
}
