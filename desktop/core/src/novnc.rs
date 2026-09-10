//! 每通道独立的 app 自持 noVNC SSH 转发。
//!
//! 容器 8080 不 publish 到宿主，避免再次落入 lima gRPC 自动转发的僵死故障域；本模块
//! 用前台 `ssh -N -L 127.0.0.1:<port>:<container-ip>:8080` 持有宿主入口。每条通道
//! 独立子进程，起停不触碰全局 mihomo 转发，也不参与 `logged_in` 判定（命门 #1）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::AppState;

pub type Handle = Arc<Mutex<HashMap<String, Child>>>;

pub fn handle() -> Handle {
    Arc::new(Mutex::new(HashMap::new()))
}

static ENSURE_LOCK: Mutex<()> = Mutex::const_new(());
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

/// 幂等确保某通道 noVNC 入口可用，返回并持久化宿主端口。
pub async fn ensure(state: &AppState, cid: &str) -> Result<i64> {
    let _guard = ENSURE_LOCK.lock().await;
    let db = state.cfg.db_path();
    let ch = crate::store::get_channel(&db, cid)?
        .ok_or_else(|| anyhow!("通道不存在:{cid}"))?;
    if ch.login_method == "headless" {
        return Err(anyhow!("无头通道没有 noVNC:{cid}"));
    }
    if let Some(port) = ch.novnc_port.and_then(|value| u16::try_from(value).ok()) {
        if port != 0 && serves(port).await {
            return Ok(i64::from(port));
        }
    }

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

    drop_for_unlocked(state, cid).await;
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
    state.novnc.lock().await.insert(cid.to_string(), child);
    crate::ev!(info, "novnc", "forward_spawn", "noVNC SSH 转发进程已拉起", {
        "cid": cid, "port": port, "target": forward.guest.as_str()
    });

    for _ in 0..30 {
        if serves(port).await {
            if let Err(error) = crate::store::set_novnc_port(&db, cid, i64::from(port)) {
                drop_for_unlocked(state, cid).await;
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
    drop_for_unlocked(state, cid).await;
    Err(anyhow!("noVNC SSH 转发已拉起但 HTTP 仍未就绪"))
}

pub async fn take_exited(
    state: &AppState,
    cid: &str,
) -> Result<Option<std::process::ExitStatus>> {
    let mut children = state.novnc.lock().await;
    let status = match children.get_mut(cid) {
        Some(child) => child.try_wait()?,
        None => None,
    };
    if status.is_some() {
        children.remove(cid);
    }
    Ok(status)
}

/// 通道 stop/remove 时回收对应子进程；DB 端口保留，下一次启动优先复用。
pub async fn drop_for(state: &AppState, cid: &str) {
    let _guard = ENSURE_LOCK.lock().await;
    drop_for_unlocked(state, cid).await;
}

async fn drop_for_unlocked(state: &AppState, cid: &str) {
    let child = state.novnc.lock().await.remove(cid);
    if let Some(mut child) = child {
        let _ = child.kill().await;
    }
}

/// 看门狗每拍清理退出句柄；每三拍或睡眠唤醒时做端到端 ensure。
pub async fn watchdog_tick(state: AppState, ensure_due: bool) {
    let Ok(_guard) = WATCHDOG_LOCK.try_lock() else { return; };
    let channels = match crate::store::list_channels(&state.cfg.db_path()) {
        Ok(channels) => channels,
        Err(error) => {
            crate::ev!(warn, "novnc", "forward_failed", "noVNC 看门狗读取通道失败", {
                "error": error.to_string()
            });
            return;
        }
    };
    for channel in channels {
        if channel.login_method == "headless"
            || !matches!(channel.status.as_str(), "running" | "logged_in")
        {
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
            if let Err(error) = ensure(&state, &channel.id).await {
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
            cfg: Arc::new(crate::config::Config {
                vm_profile: "vpnmgr-test".into(),
                dev_mode: true,
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
        let guard = ENSURE_LOCK.lock().await;
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
    async fn ensure_is_idempotent_when_stored_endpoint_serves() {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 512];
            let _ = socket.read(&mut request).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await.unwrap();
        });

        let dir = tempfile::tempdir().unwrap();
        let cfg = crate::config::Config {
            vm_profile: "vpnmgr-test".into(),
            dev_mode: true,
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
            cfg: Arc::new(cfg),
            docker: Arc::new(std::sync::RwLock::new(None)),
            mihomo: crate::mihomo::Controller::new("http://127.0.0.1:1".into(), String::new()),
            health: crate::health::shared(),
            tunnel: crate::tunnel::handle(),
            novnc: handle(),
            self_heal_enabled: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        };

        assert_eq!(ensure(&state, "c1").await.unwrap(), i64::from(port));
        assert!(state.novnc.lock().await.is_empty(), "ready probe must not spawn a child");
    }
}
