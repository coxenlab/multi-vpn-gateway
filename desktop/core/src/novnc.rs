//! 每通道独立的 app 自持 noVNC SSH 转发。
//!
//! 容器 8080 不 publish 到宿主，避免再次落入 lima gRPC 自动转发的僵死故障域；本模块
//! 用前台 `ssh -N -L 127.0.0.1:<port>:<container-ip>:8080` 持有宿主入口。每条通道
//! 独立子进程，起停不触碰全局 mihomo 转发，也不参与 `logged_in` 判定（命门 #1）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::AppState;

#[derive(Default)]
pub struct Forward {
    child: Option<Child>,
    viewers: HashMap<String, Instant>,
    legacy: bool,
}

impl Forward {
    fn watched(&mut self, now: Instant) -> bool {
        self.viewers.retain(|_, expires| *expires > now);
        self.legacy || !self.viewers.is_empty()
    }
}

#[derive(Default)]
pub struct Pool {
    forwards: Mutex<HashMap<String, Forward>>,
    operations: std::sync::Mutex<HashMap<String, Arc<Mutex<()>>>>,
}

impl Pool {
    async fn operation(&self, cid: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = self.operations.lock().unwrap().entry(cid.into()).or_default().clone();
        lock.lock_owned().await
    }
}

pub type Handle = Arc<Pool>;
pub const VIEWER_TTL_SECONDS: u64 = 60;

pub fn handle() -> Handle {
    Arc::new(Pool::default())
}

static WATCHDOG_LOCK: Mutex<()> = Mutex::const_new(());

/// 让内核挑一个当前空闲的 loopback 高位端口，随即释放；SSH 的
/// `ExitOnForwardFailure=yes` 与四次重试兜住释放后的 TOCTOU 窗口。
pub fn alloc_host_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    Ok(listener.local_addr()?.port())
}

fn port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("127.0.0.1", port)).is_ok()
}

async fn serves(port: u16) -> bool {
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
    {
        Ok(client) => client,
        Err(_) => return false,
    };
    client
        .get(format!("http://127.0.0.1:{port}/"))
        .send()
        .await
        .map(|response| response.status().is_success())
        .unwrap_or(false)
}

/// 调用方先持有通道 lifecycle.access；旧客户端无 viewer 时保留原有常驻行为。
pub async fn acquire(state: &AppState, cid: &str, viewer: Option<&str>) -> Result<i64> {
    let _guard = state.novnc.operation(cid).await;
    if let Some(viewer) = viewer {
        anyhow::ensure!(valid_viewer(viewer), "无效的登录视图标识");
        let mut forwards = state.novnc.forwards.lock().await;
        if let Some(forward) = forwards.get_mut(cid) {
            forward.watched(Instant::now());
            anyhow::ensure!(forward.viewers.contains_key(viewer) || forward.viewers.len() < 64,
                "同时打开的登录视图过多");
        }
    }
    let port = ensure_bounded(state, cid).await?;
    let mut forwards = state.novnc.forwards.lock().await;
    let forward = forwards.entry(cid.into()).or_default();
    if let Some(viewer) = viewer {
        forward.viewers.insert(viewer.into(), Instant::now() + Duration::from_secs(VIEWER_TTL_SECONDS));
    } else {
        forward.legacy = true;
    }
    Ok(port)
}

pub fn valid_viewer(viewer: &str) -> bool {
    (32..=64).contains(&viewer.len()) && viewer.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-')
}

pub async fn renew(state: &AppState, cid: &str, viewer: &str) -> bool {
    let _guard = state.novnc.operation(cid).await;
    let mut forwards = state.novnc.forwards.lock().await;
    let Some(forward) = forwards.get_mut(cid) else { return false; };
    let now = Instant::now();
    forward.watched(now);
    let Some(expires) = forward.viewers.get_mut(viewer) else { return false; };
    *expires = now + Duration::from_secs(VIEWER_TTL_SECONDS);
    true
}

pub async fn release(state: &AppState, cid: &str, viewer: &str) {
    let _guard = state.novnc.operation(cid).await;
    let watched = {
        let mut forwards = state.novnc.forwards.lock().await;
        let Some(forward) = forwards.get_mut(cid) else { return; };
        forward.viewers.remove(viewer);
        forward.watched(Instant::now())
    };
    if !watched { drop_for_unlocked(state, cid).await; }
}

async fn ensure_unlocked(state: &AppState, cid: &str) -> Result<i64> {
    let db = state.cfg.db_path();
    let ch = crate::store::get_channel(&db, cid)?
        .ok_or_else(|| anyhow!("通道不存在:{cid}"))?;
    if ch.login_method == "headless" {
        return Err(anyhow!("无头通道没有 noVNC:{cid}"));
    }
    anyhow::ensure!(matches!(ch.status.as_str(), "running" | "logged_in"), "通道尚未运行:{cid}");
    let owned = state.novnc.forwards.lock().await.get(cid).is_some_and(|f| f.child.is_some());
    let alive = owned && take_exited(state, cid).await?.is_none();
    if let Some(port) = ch.novnc_port.and_then(|value| u16::try_from(value).ok()) {
        if alive && port != 0 && serves(port).await {
            return Ok(i64::from(port));
        }
    }
    let docker = state.docker().ok_or_else(|| anyhow!("docker 不可用,取不到通道容器 IP"))?;
    crate::manager::ensure_novnc_bridge(&docker, cid).await;

    let mut last = None;
    for attempt in 0..4 {
        match try_ensure(state, cid).await {
            Ok(port) => return Ok(i64::from(port)),
            Err(error) => {
                crate::ev!(warn, "novnc", "forward_failed", "noVNC SSH 转发拉起失败", {
                    "cid": cid, "attempt": attempt + 1, "error": error.to_string()
                });
                if attempt < 3 {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                last = Some(error);
            }
        }
    }
    let error = last.unwrap_or_else(|| anyhow!("noVNC SSH 转发未能建立"));
    Err(error)
}

async fn ensure_bounded(state: &AppState, cid: &str) -> Result<i64> {
    match tokio::time::timeout(Duration::from_secs(20), ensure_unlocked(state, cid)).await {
        Ok(result) => result,
        Err(_) => {
            drop_child(state, cid).await;
            Err(anyhow!("登录入口尚未就绪，请稍后重新打开"))
        }
    }
}

async fn try_ensure(state: &AppState, cid: &str) -> Result<u16> {
    let docker = state
        .docker()
        .ok_or_else(|| anyhow!("docker 不可用,取不到通道容器 IP"))?;
    let container = format!("vpn-{cid}");
    let ip = crate::docker::container_ip(&docker, &container)
        .await
        .ok_or_else(|| anyhow!("{container} 无 IP(未运行?)"))?;
    let db = state.cfg.db_path();
    let old_port = crate::store::get_channel(&db, cid)?
        .and_then(|ch| ch.novnc_port)
        .and_then(|value| u16::try_from(value).ok())
        .filter(|port| *port != 0);

    drop_child(state, cid).await;
    let port = old_port.filter(|port| port_available(*port)).unwrap_or(alloc_host_port()?);
    let ssh_config = crate::vm::ssh_config_path(&state.cfg.vm_profile);
    if !ssh_config.exists() {
        return Err(anyhow!("ssh.config 不存在:{}(VM 未初始化?)", ssh_config.display()));
    }
    let forward = crate::tunnel::Fwd { host_port: port, guest: format!("{ip}:8080") };
    let args = crate::tunnel::forward_args(
        &ssh_config.display().to_string(),
        &state.cfg.vm_profile,
        std::slice::from_ref(&forward),
    );
    let started = std::time::Instant::now();
    let mut cmd = Command::new("ssh");
    cmd.args(args)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let child = cmd.spawn().map_err(|error| anyhow!("拉起 noVNC SSH 转发失败:{error}"))?;
    state.novnc.forwards.lock().await.entry(cid.to_string()).or_default().child = Some(child);
    crate::ev!(info, "novnc", "forward_spawn", "noVNC SSH 转发进程已拉起", {
        "cid": cid, "port": port, "target": forward.guest.as_str()
    });

    for _ in 0..30 {
        if serves(port).await {
            if let Err(error) = crate::store::set_novnc_port(&db, cid, i64::from(port)) {
                drop_child(state, cid).await;
                return Err(anyhow!("noVNC 端口落库失败:{error}"));
            }
            crate::ev!(info, "novnc", "forward_ready", "noVNC SSH 转发已就绪", {
                "cid": cid, "port": port,
                "duration_ms": started.elapsed().as_millis() as u64
            });
            return Ok(port);
        }
        if let Some(status) = take_exited(state, cid).await? {
            crate::ev!(error, "novnc", "forward_exited", "noVNC SSH 转发在就绪前退出", {
                "cid": cid, "port": port, "exit_code": status.code()
            });
            return Err(anyhow!("noVNC SSH 转发进程退出(exit {:?})", status.code()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    drop_child(state, cid).await;
    Err(anyhow!("noVNC SSH 转发已拉起但 HTTP 仍未就绪"))
}

pub async fn take_exited(
    state: &AppState,
    cid: &str,
) -> Result<Option<std::process::ExitStatus>> {
    let mut children = state.novnc.forwards.lock().await;
    let status = match children.get_mut(cid).and_then(|f| f.child.as_mut()) {
        Some(child) => child.try_wait()?,
        None => None,
    };
    if status.is_some() {
        if let Some(forward) = children.get_mut(cid) { forward.child = None; }
    }
    Ok(status)
}

/// 通道 stop/remove 时回收对应子进程；DB 端口保留，下一次启动优先复用。
pub async fn drop_for(state: &AppState, cid: &str) {
    let _guard = state.novnc.operation(cid).await;
    drop_for_unlocked(state, cid).await;
}

async fn drop_for_unlocked(state: &AppState, cid: &str) {
    let child = state.novnc.forwards.lock().await.remove(cid).and_then(|f| f.child);
    if let Some(mut child) = child { let _ = child.kill().await; }
}

async fn drop_child(state: &AppState, cid: &str) {
    let child = state.novnc.forwards.lock().await.get_mut(cid).and_then(|f| f.child.take());
    if let Some(mut child) = child {
        let _ = child.kill().await;
    }
}

/// 每拍回收无人观看的入口；只对仍有观看者的通道做局部恢复。
pub async fn watchdog_tick(state: AppState, ensure_due: bool) {
    let Ok(_guard) = WATCHDOG_LOCK.try_lock() else { return; };
    let ids: Vec<_> = state.novnc.forwards.lock().await.keys().cloned().collect();
    for cid in ids {
        let Ok(_operation) = state.lifecycle.access(&cid).await else { continue; };
        let _ensure = state.novnc.operation(&cid).await;
        let watched = state.novnc.forwards.lock().await.get_mut(&cid).is_some_and(|f| f.watched(Instant::now()));
        if !watched { drop_for_unlocked(&state, &cid).await; continue; }
        let channel = match crate::store::get_channel(&state.cfg.db_path(), &cid) {
            Ok(Some(channel)) => channel,
            Ok(None) => { drop_for_unlocked(&state, &cid).await; continue; }
            Err(_) => continue,
        };
        if channel.login_method == "headless"
            || !matches!(channel.status.as_str(), "running" | "logged_in")
        {
            drop_for_unlocked(&state, &cid).await;
            continue;
        }
        let exited = match take_exited(&state, &channel.id).await {
            Ok(status) => status,
            Err(error) => {
                crate::ev!(warn, "novnc", "forward_failed", "noVNC SSH 转发状态检查失败", {
                    "cid": channel.id.as_str(), "error": error.to_string()
                });
                continue;
            }
        };
        if let Some(status) = exited {
            crate::ev!(error, "novnc", "forward_exited", "noVNC SSH 转发进程非预期退出", {
                "cid": channel.id.as_str(), "exit_code": status.code()
            });
        }
        if (ensure_due || exited.is_some()) && state.self_heal_enabled() {
            if let Err(error) = ensure_bounded(&state, &channel.id).await {
                crate::ev!(error, "novnc", "forward_failed", "noVNC SSH 转发自愈失败", {
                    "cid": channel.id.as_str(), "error": error.to_string()
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn alloc_host_port_returns_rebindable_loopback_port() {
        let port = alloc_host_port().unwrap();
        assert_ne!(port, 0);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        assert_eq!(listener.local_addr().unwrap().ip().to_string(), "127.0.0.1");
    }

    #[tokio::test]
    async fn drop_for_serializes_with_ensure_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState {
            lifecycle: Default::default(),
            cfg: Arc::new(crate::config::Config {
                vm_profile: "vpnmgr-test".into(),
                dev_mode: true,
                managed_vm: false,
                bundled_images_dir: None,
                ui_port: 0,
                data_dir: dir.path().to_path_buf(),
                static_dir: dir.path().to_path_buf(),
                mihomo_ctrl_url: "http://127.0.0.1:1".into(),
                mihomo_secret: String::new(),
                mihomo_host_port: "1".into(),
                mihomo_ctrl_port: None,
                vpn_net: "vpnnet".into(),
            }),
            docker: Arc::new(std::sync::RwLock::new(None)),
            mihomo: crate::mihomo::Controller::new("http://127.0.0.1:1".into(), String::new()),
            health: crate::health::shared(),
            tunnel: crate::tunnel::handle(),
            novnc: handle(),
            self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };
        let guard = state.novnc.operation("c1").await;
        let mut task = tokio::spawn({
            let state = state.clone();
            async move { drop_for(&state, "c1").await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut task).await.is_err(),
            "drop_for must wait for an in-flight ensure"
        );
        drop(guard);
        tokio::time::timeout(Duration::from_secs(1), task).await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn owned_forward_leases_isolate_viewers_and_expire_without_healing() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 512];
                let _ = socket.read(&mut request).await;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.unwrap();
            }
        });

        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            vm_profile: "vpnmgr-test".into(),
            dev_mode: true,
            managed_vm: false,
            bundled_images_dir: None,
            ui_port: 0,
            data_dir: dir.path().to_path_buf(),
            static_dir: dir.path().to_path_buf(),
            mihomo_ctrl_url: "http://127.0.0.1:1".into(),
            mihomo_secret: String::new(),
            mihomo_host_port: "1".into(),
            mihomo_ctrl_port: None,
            vpn_net: "vpnnet".into(),
        };
        crate::store::init(&cfg.db_path()).unwrap();
        let key = crate::store::master_key(dir.path()).unwrap();
        let channel = crate::store::NewChannel {
            id: "c1".into(), name: "n".into(), vpn_type: "easyconnect".into(),
            server: String::new(), ec_ver: String::new(), login_method: "interactive".into(),
            username: String::new(), password: String::new(), vnc_password: String::new(),
            mac: String::new(), probe_url: String::new(), status: "running".into(),
            routing_enabled: true,
        };
        crate::store::add_channel(&cfg.db_path(), &key, &channel, &serde_json::Map::new(), &[]).unwrap();
        crate::store::set_novnc_port(&cfg.db_path(), "c1", i64::from(port)).unwrap();
        let state = AppState {
            lifecycle: Default::default(),
            cfg: Arc::new(cfg),
            docker: Arc::new(std::sync::RwLock::new(None)),
            mihomo: crate::mihomo::Controller::new("http://127.0.0.1:1".into(), String::new()),
            health: crate::health::shared(),
            tunnel: crate::tunnel::handle(),
            novnc: handle(),
            self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };

        let first = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let second = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        assert!(acquire(&state, "c1", Some(first)).await.is_err(), "unowned listener is not our SSH forward");
        assert!(state.novnc.forwards.lock().await.is_empty());
        watchdog_tick(state.clone(), true).await;
        assert!(state.novnc.forwards.lock().await.is_empty(), "running channel alone must not start noVNC");
        let child = Command::new("sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        let pid = child.id().unwrap();
        state.novnc.forwards.lock().await.insert("c1".into(), Forward { child: Some(child), ..Default::default() });
        assert_eq!(acquire(&state, "c1", Some(first)).await.unwrap(), i64::from(port));
        assert_eq!(acquire(&state, "c1", Some(second)).await.unwrap(), i64::from(port));
        assert_eq!(state.novnc.forwards.lock().await["c1"].child.as_ref().unwrap().id(), Some(pid));
        release(&state, "other", first).await;
        release(&state, "c1", first).await;
        assert!(!renew(&state, "c1", first).await);
        assert!(renew(&state, "c1", second).await);
        assert_eq!(state.novnc.forwards.lock().await["c1"].child.as_ref().unwrap().id(), Some(pid));
        release(&state, "c1", second).await;
        assert!(state.novnc.forwards.lock().await.is_empty());
        assert!(!Command::new("kill").args(["-0", &pid.to_string()]).stderr(std::process::Stdio::null()).status().await.unwrap().success());

        let child = Command::new("sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        state.novnc.forwards.lock().await.insert("c1".into(), Forward {
            child: Some(child), viewers: HashMap::from([(first.into(), Instant::now())]), legacy: false,
        });
        state.set_self_heal_enabled(false);
        assert!(!renew(&state, "c1", first).await, "expired viewer cannot silently revive");
        watchdog_tick(state.clone(), false).await;
        assert!(state.novnc.forwards.lock().await.is_empty(), "expiry cleanup is independent of self-heal");

        state.novnc.forwards.lock().await.insert("c1".into(), Forward { legacy: true, ..Default::default() });
        watchdog_tick(state.clone(), false).await;
        assert!(state.novnc.forwards.lock().await.contains_key("c1"), "old clients retain their legacy lifetime");
        drop_for(&state, "c1").await;
        server.abort();
    }
}
