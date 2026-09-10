//! app 自持的 SSH 端口转发:宿主分流口/控制口的**唯一**伺服者(取代 lima 自动转发)。
//!
//! 为什么不用 lima(2026-08-04 实测坐实):lima hostagent 的 gRPC 转发通道会僵死并
//! **永不重建**——宿主日志 `could not open tunnel ... rpc error: code = Canceled desc =
//! grpc: the client connection is closing`。此时端口 listener 仍在、TCP 照常 accept,
//! 只是数据永远送不进去(旧判据据此误报健康,见 [`crate::health::proxy_serves`])。
//! 重启 VM 内 guestagent 无效(死的是宿主侧客户端),只有重启整个 VM 能恢复,
//! 而那会停掉全部 VPN 容器、逼用户重新登录每条通道。
//!
//! 触发条件是 Mac 睡眠冻结 VM 后的通道重建,叠加 guestagent 的 socket 泄漏
//! (实测死时 177k 句柄 / 2.5GB);合盖走人的用法下一两天就会撞一次。
//!
//! 故 mihomo#1 **不再 publish 任何端口**(不进 lima 视野),本模块用独立 SSH 连接把
//! 宿主 `127.0.0.1:<分流口|控制口>` 直接转到 VM 内 mihomo 容器 IP。SSH 走的是 VM 的
//! sshd,与 lima 的 gRPC 转发无关,不受该缺陷影响;睡醒后隧道若断,重建是秒级、
//! 不动任何容器、不需要重新登录。
//!
//! 命门 #4:宿主侧只绑 `127.0.0.1`,永不 `0.0.0.0`。
//! 取舍(用户 2026-08-04 知情选择):转发由 app 进程持有(前台 `-N`,非 `-f` 守护化),
//! app 退出即断——换来「坏了能秒修、不用重登通道」。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;

use crate::AppState;

/// 隧道子进程句柄(AppState 持有)。None = 尚未拉起 / 已回收。
pub type Handle = Arc<Mutex<Option<Child>>>;

pub fn handle() -> Handle {
    Arc::new(Mutex::new(None))
}

/// 最近一次隧道进程的 stderr 尾部(每次 spawn 清空,后台任务续写)。
/// exit 255 时 ssh 的真实死因只在 stderr 里(`Address already in use` vs 认证失败 vs
/// connection refused),不采集就只能靠猜——2026-08-26 复盘的盲点 #3。
static STDERR_TAIL: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
const STDERR_TAIL_LINES: usize = 8;

/// 供事件日志引用的 stderr 尾部(单行拼接)。
pub fn stderr_tail() -> String {
    STDERR_TAIL
        .lock()
        .map(|v| v.join(" | "))
        .unwrap_or_default()
}

fn stderr_reset() {
    if let Ok(mut v) = STDERR_TAIL.lock() {
        v.clear();
    }
}

fn stderr_push(line: String) {
    if let Ok(mut v) = STDERR_TAIL.lock() {
        if v.len() >= STDERR_TAIL_LINES {
            v.remove(0);
        }
        v.push(line);
    }
}

/// 不消费退出状态的存活检查(给看门狗的证据采集用):`(pid, alive)`。
/// 与 [`take_exited`] 不冲突:tokio 的 `try_wait` 对已结束子进程返回缓存状态。
pub async fn status(state: &AppState) -> (Option<u32>, bool) {
    let mut guard = state.tunnel.lock().await;
    match guard.as_mut() {
        None => (None, false),
        Some(child) => {
            let alive = matches!(child.try_wait(), Ok(None));
            (child.id(), alive)
        }
    }
}

/// 一条转发:宿主端口 → VM 内可达地址(`<容器IP>:<容器端口>`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fwd {
    pub host_port: u16,
    pub guest: String,
}

/// 纯:ssh 参数。独立连接(不搭 hostagent 的 ControlMaster 主连接,后者与被绕开的
/// 转发共享故障域);`ExitOnForwardFailure` 让绑定失败立刻反映为退出;前台 `-N` 保持
/// 子进程可被本模块 kill/探活。ServerAlive 容忍 ~2min:VM 高负载(容器重建)下别误杀。
pub fn forward_args(ssh_config: &str, profile: &str, fwds: &[Fwd]) -> Vec<String> {
    let mut a: Vec<String> = [
        "-F", ssh_config,
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=10",
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=8",
        "-N",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for f in fwds {
        a.push("-L".into());
        a.push(format!("127.0.0.1:{}:{}", f.host_port, f.guest)); // 命门 #4
    }
    a.push(format!("lima-colima-{profile}"));
    a
}

/// 要转发的目标:mihomo 容器 IP + 容器内固定端口。容器 IP 每次重建都可能变,
/// 故每次拉隧道前实时 inspect,不缓存。
pub async fn fwds(state: &AppState) -> Result<Vec<Fwd>> {
    let docker = state
        .docker()
        .ok_or_else(|| anyhow!("docker 不可用,取不到 mihomo 容器 IP"))?;
    let ip = crate::docker::container_ip(&docker, crate::infra::MIHOMO_CONTAINER)
        .await
        .ok_or_else(|| anyhow!("mihomo 容器无 IP(未运行?)"))?;
    let mut v = Vec::new();
    if let Ok(p) = state.cfg.mihomo_host_port.parse::<u16>() {
        if p != 0 {
            v.push(Fwd { host_port: p, guest: format!("{ip}:{}", crate::infra::MIHOMO_PROXY_PORT) });
        }
    }
    if let Some(p) = state.cfg.mihomo_ctrl_port.as_ref().and_then(|s| s.parse::<u16>().ok()) {
        if p != 0 {
            v.push(Fwd { host_port: p, guest: format!("{ip}:{}", crate::infra::MIHOMO_CTRL_PORT_IN) });
        }
    }
    if v.is_empty() {
        return Err(anyhow!("分流口/控制口未配置(ensure_params 应已注入)"));
    }
    Ok(v)
}

/// 幂等:端到端已通 → no-op;否则杀旧进程、拉新隧道、等分流口真正过数据。
///
/// 判据用 [`crate::health::proxy_serves`](真握手)而非「进程还在」——ssh 进程活着但
/// 转发失效是存在的(VM 侧目标变了/容器换 IP),那正是要重建的场景。
pub async fn ensure(state: &AppState) -> Result<()> {
    // 绑不上宿主端口通常是「上一秒还占着口的人正在退场」:升级首启时 lima 要等
    // guestagent 报完端口移除才松手,旧隧道进程退出也有毫秒级延迟。多试几次即可,
    // 不必把用户丢给 40s 后的下一拍看门狗。
    let mut last = None;
    for attempt in 0..4 {
        match try_ensure(state).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if attempt < 3 {
                    crate::ev!(warn, "tunnel", "tunnel_retry", "SSH 转发拉起失败,2 秒后重试", { "attempt": attempt + 1, "error": e.to_string() });
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                last = Some(e);
            }
        }
    }
    let error = last.unwrap_or_else(|| anyhow!("SSH 转发未能建立"));
    crate::ev!(error, "tunnel", "tunnel_failed", "SSH 转发未能建立", { "error": error.to_string() });
    Err(error)
}

async fn try_ensure(state: &AppState) -> Result<()> {
    if crate::health::proxy_serves(&state.cfg.mihomo_host_port).await {
        crate::ev!(debug, "tunnel", "tunnel_ensure_skip", "SSH 转发已可用,无需重建", {});
        return Ok(());
    }
    let fwds = fwds(state).await?;
    let cfg = crate::vm::ssh_config_path(&state.cfg.vm_profile);
    if !cfg.exists() {
        return Err(anyhow!("ssh.config 不存在:{}(VM 未初始化?)", cfg.display()));
    }
    kill(state).await; // 先回收旧进程,否则新 ssh 绑不上同一个宿主端口
    let mut cmd = Command::new("ssh");
    let forward_summary = fwds.iter()
        .map(|f| format!("127.0.0.1:{}->{}", f.host_port, f.guest))
        .collect::<Vec<_>>().join(",");
    let started = std::time::Instant::now();
    cmd.args(forward_args(&cfg.display().to_string(), &state.cfg.vm_profile, &fwds))
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| anyhow!("拉起 SSH 转发失败: {e}"))?;
    stderr_reset();
    if let Some(stderr) = child.stderr.take() {
        tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    stderr_push(line);
                }
            }
        });
    }
    *state.tunnel.lock().await = Some(child);
    crate::ev!(info, "tunnel", "tunnel_spawn", "SSH 转发进程已拉起", { "forwards": forward_summary });

    // 等端到端真通(ssh 建连 + 转发就绪,通常 <1s;VM 忙时给到 15s)。
    for _ in 0..30 {
        if crate::health::proxy_serves(&state.cfg.mihomo_host_port).await {
            crate::ev!(info, "tunnel", "tunnel_ready", "SSH 转发已就绪", { "duration_ms": started.elapsed().as_millis() as u64 });
            return Ok(());
        }
        // 进程当场退出 → 立刻报错,不空等满 15s;死因看 stderr(端口被占/认证失败/refused)。
        if let Some(st) = take_exited(state).await? {
            // 给 stderr 读取任务一点时间把最后几行冲进缓冲。
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stderr = stderr_tail();
            crate::ev!(error, "tunnel", "tunnel_exited", "SSH 转发进程在就绪前退出", { "exit_code": st.code(), "stderr": stderr });
            return Err(anyhow!("SSH 转发进程退出(exit {:?}):{}", st.code(), if stderr.is_empty() { "无 stderr 输出".to_string() } else { stderr }));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(anyhow!("SSH 转发已拉起但分流口仍不过数据(VM 内 mihomo 异常?)"))
}

/// 每拍检查已就绪隧道是否意外退出；退出时顺带清掉失效句柄。
pub async fn take_exited(state: &AppState) -> Result<Option<std::process::ExitStatus>> {
    let mut guard = state.tunnel.lock().await;
    let status = match guard.as_mut() {
        Some(child) => child.try_wait()?,
        None => None,
    };
    if status.is_some() { guard.take(); }
    Ok(status)
}

/// 回收隧道子进程(重建前 / app 退出前)。已退出或从未拉起都安全。
pub async fn kill(state: &AppState) {
    if let Some(mut child) = state.tunnel.lock().await.take() {
        let _ = child.kill().await;
    }
}

pub(crate) async fn stop_child_confirmed(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_some() { return Ok(()); }
    let result = child.kill().await;
    anyhow::ensure!(child.try_wait()?.is_some(), "转发进程停止尚未确认: {:?}", result);
    Ok(())
}

/// 空闲释放需要确认退出；失败时保留句柄供后续核查。
pub async fn kill_confirmed(state: &AppState) -> Result<()> {
    let mut guard = state.tunnel.lock().await;
    if let Some(child) = guard.as_mut() { stop_child_confirmed(child).await?; }
    *guard = None;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn idle_stop_confirms_child_exit_and_is_idempotent() {
        let mut child = Command::new("sleep").arg("30").kill_on_drop(true).spawn().unwrap();
        stop_child_confirmed(&mut child).await.unwrap();
        assert!(child.try_wait().unwrap().is_some());
        stop_child_confirmed(&mut child).await.unwrap();
    }

    #[test]
    fn forward_args_binds_loopback_only() {
        let fwds = vec![
            Fwd { host_port: 37473, guest: "172.18.0.2:7899".into() },
            Fwd { host_port: 48020, guest: "172.18.0.2:9090".into() },
        ];
        let a = forward_args("/tmp/ssh.config", "vpnmgr", &fwds);
        assert!(a.contains(&"-L".to_string()));
        assert!(a.contains(&"127.0.0.1:37473:172.18.0.2:7899".to_string()), "命门 #4:只绑 127.0.0.1");
        assert!(a.contains(&"127.0.0.1:48020:172.18.0.2:9090".to_string()));
        assert!(!a.iter().any(|s| s.contains("0.0.0.0")), "命门 #4:永不绑 0.0.0.0");
        assert_eq!(a.last().unwrap(), "lima-colima-vpnmgr");
    }

    #[test]
    fn forward_args_stays_foreground() {
        let a = forward_args("/tmp/ssh.config", "p", &[Fwd { host_port: 1, guest: "x:2".into() }]);
        assert!(a.contains(&"-N".to_string()));
        assert!(!a.contains(&"-fN".to_string()), "前台运行:子进程要能被 kill/探活");
        assert!(!a.contains(&"-f".to_string()));
    }
}
