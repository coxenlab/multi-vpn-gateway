//! 自带 colima VM(app 专属 profile,绝不碰用户 `default`)的生命周期 + 健康。
//!
//! 用 colima CLI(`tokio::process`)。VM 内的 docker.sock 经 `DOCKER_HOST` 注入给
//! [`crate::docker::connect`](docker::docker_socket 读 `DOCKER_HOST` 去 `unix://` 前缀)。
//! 命门 #4 不受影响:sock 是 unix domain socket,UI/转发端口仍只绑 127.0.0.1。
//! 命门隔离:`--activate=false` 不改用户 active docker context;专属 profile 与 `default` 互不干扰。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

/// app 专属 profile 名(独立 VM,与用户 `default` 隔离)。
pub const PROFILE: &str = "vpnmgr";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    Running,
    /// 含「profile 不存在」与「已停止」。
    Stopped,
}

/// 纯:docker.sock 路径 = `<home>/.colima/<profile>/docker.sock`。
pub fn socket_path_in(home: &str, profile: &str) -> PathBuf {
    PathBuf::from(home).join(".colima").join(profile).join("docker.sock")
}

/// docker.sock 路径(读 `$HOME`)。对照 colima status 输出的 "docker socket"。
pub fn socket_path(profile: &str) -> PathBuf {
    socket_path_in(&std::env::var("HOME").unwrap_or_default(), profile)
}

/// `DOCKER_HOST` 值:`unix://<sock>`。docker::docker_socket 据此连到该 profile。
pub fn docker_host(profile: &str) -> String {
    format!("unix://{}", socket_path(profile).display())
}

/// 纯:lima 实例目录(colima 实例的 lima 名 = `colima-<profile>`)下的 ssh.config。
pub fn ssh_config_path_in(home: &str, profile: &str) -> PathBuf {
    PathBuf::from(home)
        .join(".colima")
        .join("_lima")
        .join(format!("colima-{profile}"))
        .join("ssh.config")
}

/// ssh.config 路径(读 `$HOME`)。
pub fn ssh_config_path(profile: &str) -> PathBuf {
    ssh_config_path_in(&std::env::var("HOME").unwrap_or_default(), profile)
}

/// 纯:备援隧道的 ssh 参数。与 colima 自身 docker.sock 隧道同款:独立连接(ControlMaster=no,
/// 不依赖 hostagent 的主连接)、`ExitOnForwardFailure` 让绑定失败反映在退出码、`-f` 守护化
/// (父进程在全部转发建立后才退出 0)。ServerAlive*:VM 停/断后隧道自杀,不留死进程占口;
/// 容忍 ~2min(实测 2026-07-02:容器重建时 VM 高负载,30s 容忍会误杀活隧道)。
pub fn tunnel_args(ssh_config: &str, profile: &str, ports: &[u16]) -> Vec<String> {
    let mut a: Vec<String> = [
        "-F", ssh_config,
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ConnectTimeout=10", // 半死 hostagent 可能 accept 后不发 banner,不设则 ssh 永挂
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=8",
        "-fN",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    for p in ports {
        a.push("-L".into());
        a.push(format!("127.0.0.1:{p}:127.0.0.1:{p}")); // 命门 #4:宿主侧只绑 127.0.0.1
    }
    a.push(format!("lima-colima-{profile}"));
    a
}

/// 纯:独立 SSH 探针的参数(鉴别诊断用,与备援隧道同款连接方式:不依赖 hostagent 主连接)。
/// `BatchMode` 防交互挂死;远端只跑 `true`,零副作用。
pub fn probe_args(ssh_config: &str, profile: &str) -> Vec<String> {
    [
        "-F", ssh_config,
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=8",
        &format!("lima-colima-{profile}"),
        "true",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// 独立 SSH 探针:docker.sock 不可达时鉴别「VM 真死」vs「传输层(mux/转发)死」。
/// 实测 2026-07-02(auto-memory `watchdog-transport-dead-misdiagnosed-vmdown`):杀 mux 后
/// docker ping 挂、但此探针可达且备援隧道能当场救活分流口——二者共享 mux 故障域,
/// 只看 docker ping 会把可自愈的传输层故障误诊成 vm_down。
pub async fn ssh_reachable(profile: &str) -> bool {
    let cfg = ssh_config_path(profile);
    if !cfg.exists() {
        return false;
    }
    let mut cmd = Command::new("ssh");
    cmd.args(probe_args(&cfg.display().to_string(), profile)).kill_on_drop(true);
    matches!(
        tokio::time::timeout(Duration::from_secs(12), cmd.status()).await,
        Ok(Ok(st)) if st.success()
    )
}

/// 纯:docker.sock 备援隧道参数(unix streamlocal 转发;`StreamLocalBindUnlink` 覆盖陈死本地 sock)。
pub fn socket_tunnel_args(ssh_config: &str, profile: &str, local_sock: &str, remote_sock: &str) -> Vec<String> {
    let mut a: Vec<String> = [
        "-F", ssh_config,
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "StreamLocalBindUnlink=yes",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ConnectTimeout=10",
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=8",
        "-fN",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    a.push("-L".into());
    a.push(format!("{local_sock}:{remote_sock}"));
    a.push(format!("lima-colima-{profile}"));
    a
}

/// docker.sock 备援隧道:传输层坏死时把 VM 内 `/var/run/docker.sock` 经独立 SSH 转到本地
/// sock 路径(lima 用户在 docker 组、免 sudo,实测 2026-07-02)。成功后 `docker::connect_at`
/// 重建连接换入 AppState → 下一拍 ping 通,状态从 TransportDead 收敛回 Healthy。
/// ⚠️ local_sock 须短路径(unix sun_path 上限 104 字节),放 data_dir 下。
pub async fn spawn_docker_sock_tunnel(profile: &str, local_sock: &std::path::Path) -> Result<()> {
    let cfg = ssh_config_path(profile);
    if !cfg.exists() {
        return Err(anyhow!("ssh.config 不存在:{}(VM 未初始化?)", cfg.display()));
    }
    let mut cmd = Command::new("ssh");
    cmd.args(socket_tunnel_args(
        &cfg.display().to_string(),
        profile,
        &local_sock.display().to_string(),
        "/var/run/docker.sock",
    ))
    .kill_on_drop(true);
    let st = tokio::time::timeout(Duration::from_secs(30), cmd.status())
        .await
        .map_err(|_| anyhow!("docker.sock 隧道建立超时(30s)"))?
        .map_err(|e| anyhow!("ssh 启动失败: {e}"))?;
    if !st.success() {
        return Err(anyhow!("docker.sock 隧道建立失败(exit {:?})", st.code()));
    }
    Ok(())
}

/// 备援端口隧道:hostagent 僵死(`docker restart` 重建不了 lima 转发)时,直接经 VM 的 sshd
/// 把宿主 `127.0.0.1:<port>` 转回 VM 内同端口(mihomo 的 docker 端口映射绑在 VM 的 127.0.0.1)。
/// `-f` 守护化,本函数等到转发建立(exit 0)或失败(非 0,如端口被占/ssh 不可达)即返回。
pub async fn spawn_port_tunnel(profile: &str, ports: &[u16]) -> Result<()> {
    if ports.is_empty() {
        return Err(anyhow!("无可转发端口(分流口/控制口未配置)"));
    }
    let cfg = ssh_config_path(profile);
    if !cfg.exists() {
        return Err(anyhow!("ssh.config 不存在:{}(VM 未初始化?)", cfg.display()));
    }
    // 外层 30s 兜底超时:调用方在看门狗单循环里裸 await,这里挂死=整个看门狗冻结。
    // kill_on_drop:超时丢弃 future 时把 ssh 父进程一并杀掉,不留半截进程。
    let mut cmd = Command::new("ssh");
    cmd.args(tunnel_args(&cfg.display().to_string(), profile, ports)).kill_on_drop(true);
    let st = tokio::time::timeout(Duration::from_secs(30), cmd.status())
        .await
        .map_err(|_| anyhow!("备援隧道建立超时(30s,VM ssh 无响应)"))?
        .map_err(|e| anyhow!("ssh 启动失败: {e}"))?;
    if !st.success() {
        return Err(anyhow!("备援隧道建立失败(exit {:?};端口被占或 VM ssh 不可达)", st.code()));
    }
    Ok(())
}

/// colima 是否在 PATH 上(后续版本将 sidecar 内置)。
pub async fn colima_present() -> bool {
    Command::new("colima")
        .arg("version")
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `colima status --profile p`:exit 0 = Running,否则 Stopped(含不存在)。
pub async fn status(profile: &str) -> VmStatus {
    let ok = Command::new("colima")
        .args(["status", "--profile", profile])
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        VmStatus::Running
    } else {
        VmStatus::Stopped
    }
}

fn start_args(profile: &str, enable_rosetta: bool) -> Vec<String> {
    let mut args = vec!["start".to_string(), profile.to_string(), "--vm-type".to_string(), "vz".to_string()];
    if enable_rosetta {
        args.push("--vz-rosetta".to_string());
    }
    args.extend([
        // gRPC 转发器(vz vsock 通道),ssh 退出数据面:载重 ssh 转发每隔几分钟必死,
        // 且 ssh -L 只转 TCP(vpn-router 的 udp:true 靠它才真生效)。
        "--port-forwarder", "grpc", "--activate=false", "--cpu", "4", "--memory", "6", "--disk", "60",
    ].into_iter().map(String::from));
    args
}

/// 把 colima/lima 用 `\r` 刷新的进度和普通 `\n` 日志统一切成非空行。
fn split_progress_chunk(pending: &mut Vec<u8>, chunk: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    for &byte in chunk {
        if byte == b'\r' || byte == b'\n' {
            if !pending.is_empty() {
                let line = String::from_utf8_lossy(pending).trim().to_string();
                pending.clear();
                if !line.is_empty() {
                    lines.push(line);
                }
            }
        } else {
            pending.push(byte);
        }
    }
    lines
}

async fn read_progress<R>(mut reader: R, tx: mpsc::UnboundedSender<String>)
where
    R: AsyncRead + Unpin,
{
    let mut chunk = [0_u8; 4096];
    let mut pending = Vec::new();
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                for line in split_progress_chunk(&mut pending, &chunk[..n]) {
                    let _ = tx.send(line);
                }
            }
            Err(_) => break,
        }
    }
    if !pending.is_empty() {
        let line = String::from_utf8_lossy(&pending).trim().to_string();
        if !line.is_empty() {
            let _ = tx.send(line);
        }
    }
}

/// 起专属 VM 并逐行上报 stdout/stderr。非 TTY 下 colima 的输出格式不稳定,故保留原文。
/// `enable_rosetta=false` 用于 Apple Silicon 用户明确跳过 Rosetta 时,避免 `--vz-rosetta` 硬失败。
pub async fn start_with_progress<F>(profile: &str, enable_rosetta: bool, mut on_progress: F) -> Result<()>
where
    F: FnMut(String) + Send,
{
    if !colima_present().await {
        return Err(anyhow!("未找到 colima(请先安装;后续版本会随 app 内置打包)"));
    }
    let mut child = Command::new("colima")
        .args(start_args(profile, enable_rosetta))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("colima start {profile}: {e}"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法捕获 colima stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("无法捕获 colima stderr"))?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_progress(stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_progress(stderr, tx.clone()));
    drop(tx);

    let mut wait = Box::pin(child.wait());
    let mut streams_open = true;
    let st = loop {
        tokio::select! {
            result = &mut wait => {
                break result.map_err(|e| anyhow!("colima start {profile}: {e}"))?;
            }
            line = rx.recv(), if streams_open => {
                match line {
                    Some(line) => on_progress(line),
                    None => streams_open = false,
                }
            }
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    while let Ok(line) = rx.try_recv() {
        on_progress(line);
    }
    if !st.success() {
        return Err(anyhow!("colima start {profile} 失败(exit {:?})", st.code()));
    }
    Ok(())
}

/// 保留原调用语义:默认启用 Rosetta。
pub async fn start(profile: &str) -> Result<()> {
    start_with_progress(profile, true, |_| {}).await
}

/// 当前构建是否运行在需要 Rosetta 的 Apple Silicon 架构上。
pub fn host_needs_rosetta() -> bool {
    cfg!(target_os = "macos") && matches!(std::env::consts::ARCH, "aarch64" | "arm64")
}

/// 运行 x86_64 真进程探测 Rosetta,不依赖易变的文件路径。
pub async fn rosetta_available() -> bool {
    if !host_needs_rosetta() {
        return true;
    }
    Command::new("/usr/bin/arch")
        .args(["-x86_64", "/usr/bin/true"])
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 原生确认框:解释用途后由用户选择安装或跳过。点「跳过」会以 AppleScript -128 返回。
pub async fn prompt_rosetta_install() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"display dialog "运行 x86 版 VPN 客户端需要 Rosetta 2。现在安装会由 macOS 请求管理员授权。" with title "需要 Rosetta 2" buttons {"跳过", "安装"} default button "安装" cancel button "跳过" with icon note"#;
        let out = Command::new("osascript")
            .args(["-e", script])
            .output()
            .await
            .map_err(|e| anyhow!("启动 Rosetta 确认框失败: {e}"))?;
        if out.status.success() {
            return Ok(String::from_utf8_lossy(&out.stdout).contains("安装"));
        }
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stderr.contains("-128") || stderr.contains("User canceled") {
            return Ok(false);
        }
        Err(anyhow!("Rosetta 确认框失败: {}", stderr.trim()))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(false)
    }
}

/// 用与 TUN helper 相同的 osascript 管理员授权模式安装 Rosetta。
/// 返回 false 表示用户在系统授权框取消。
pub async fn install_rosetta() -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let script = r#"do shell script "/usr/sbin/softwareupdate --install-rosetta --agree-to-license" with administrator privileges"#;
        let out = Command::new("osascript")
            .args(["-e", script])
            .output()
            .await
            .map_err(|e| anyhow!("启动 Rosetta 安装失败: {e}"))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("-128") || stderr.contains("User canceled") {
                return Ok(false);
            }
            return Err(anyhow!("Rosetta 安装失败: {}", stderr.trim()));
        }
        if !rosetta_available().await {
            return Err(anyhow!("Rosetta 安装命令已结束,但 x86_64 运行探测仍失败"));
        }
        Ok(true)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(true)
    }
}

/// 确保 VM 在跑(已 Running 则跳过 start)。
pub async fn ensure_running(profile: &str) -> Result<()> {
    if status(profile).await == VmStatus::Running {
        return Ok(());
    }
    start(profile).await
}

/// 停 VM。用于传输层坏死时的自动恢复(停→冷起,实测 2026-07-02 该路径 ~15s 满血)。
pub async fn stop(profile: &str) -> Result<()> {
    let st = Command::new("colima")
        .args(["stop", profile])
        .status()
        .await
        .map_err(|e| anyhow!("colima stop {profile}: {e}"))?;
    if !st.success() {
        return Err(anyhow!("colima stop {profile} 失败(exit {:?})", st.code()));
    }
    Ok(())
}

/// 设 `DOCKER_HOST` 指向该 profile,等 dockerd 可 ping(colima start 后通常已 ready,补一层重试)。
/// 墙钟计时 + 单次 connect 包 5s 超时:半死 sock(accept 后不回话)下 bollard 兜底超时
/// 是 120s,按迭代数计时会把 40s 门拖成小时级,boot 自愈永远打不响。
pub async fn wait_docker_ready(profile: &str, timeout_secs: u64) -> Result<()> {
    std::env::set_var("DOCKER_HOST", docker_host(profile));
    let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs);
    loop {
        if socket_path(profile).exists() {
            if let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_secs(5), crate::docker::connect()).await
            {
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err(anyhow!("等 {profile} 的 dockerd 就绪超时({timeout_secs}s)"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_layout() {
        assert_eq!(
            socket_path_in("/Users/x", "vpnmgr"),
            PathBuf::from("/Users/x/.colima/vpnmgr/docker.sock")
        );
    }

    #[test]
    fn ssh_config_path_layout() {
        assert_eq!(
            ssh_config_path_in("/Users/x", "vpnmgr"),
            PathBuf::from("/Users/x/.colima/_lima/colima-vpnmgr/ssh.config")
        );
    }

    #[test]
    fn tunnel_args_shape() {
        let a = tunnel_args("/tmp/ssh.config", "vpnmgr", &[37473, 48020]);
        assert_eq!(a[0], "-F");
        assert_eq!(a[1], "/tmp/ssh.config");
        assert!(a.contains(&"ControlMaster=no".to_string()), "独立连接,不依赖 hostagent 主连接");
        assert!(a.contains(&"ExitOnForwardFailure=yes".to_string()), "绑定失败须反映在退出码");
        assert!(a.contains(&"ConnectTimeout=10".to_string()), "半死 sshd 不能挂死调用方");
        assert!(a.contains(&"-fN".to_string()));
        // 命门 #4:宿主侧只绑 127.0.0.1
        assert!(a.contains(&"127.0.0.1:37473:127.0.0.1:37473".to_string()));
        assert!(a.contains(&"127.0.0.1:48020:127.0.0.1:48020".to_string()));
        assert_eq!(a.last().unwrap(), "lima-colima-vpnmgr");
    }

    #[test]
    fn socket_tunnel_args_shape() {
        let a = socket_tunnel_args("/tmp/ssh.config", "vpnmgr", "/tmp/dt.sock", "/var/run/docker.sock");
        assert!(a.contains(&"StreamLocalBindUnlink=yes".to_string()), "须能覆盖陈死本地 sock");
        assert!(a.contains(&"ControlMaster=no".to_string()), "独立连接,不依赖 hostagent 主连接");
        assert!(a.contains(&"/tmp/dt.sock:/var/run/docker.sock".to_string()));
        assert!(a.contains(&"-fN".to_string()));
        assert_eq!(a.last().unwrap(), "lima-colima-vpnmgr");
    }

    #[test]
    fn probe_args_shape() {
        let a = probe_args("/tmp/ssh.config", "vpnmgr");
        assert_eq!(a[0], "-F");
        assert!(a.contains(&"ControlMaster=no".to_string()), "独立连接,不依赖 hostagent 主连接");
        assert!(a.contains(&"BatchMode=yes".to_string()), "禁交互,不能挂死看门狗");
        assert!(a.contains(&"ConnectTimeout=8".to_string()));
        // 远端零副作用:只跑 true
        assert_eq!(a[a.len() - 2], "lima-colima-vpnmgr");
        assert_eq!(a.last().unwrap(), "true");
        // 探针不带端口转发
        assert!(!a.contains(&"-L".to_string()));
    }

    #[test]
    fn docker_host_uses_unix_scheme() {
        // docker_host 读 HOME;断言 scheme 前缀与 socket 后缀,不绑死 home。
        let h = docker_host("vpnmgr");
        assert!(h.starts_with("unix://"));
        assert!(h.ends_with("/.colima/vpnmgr/docker.sock"));
    }

    #[test]
    fn progress_chunks_split_cr_lf_and_preserve_partial_utf8() {
        let mut pending = Vec::new();
        let first = split_progress_chunk(&mut pending, "下载镜".as_bytes());
        assert!(first.is_empty());
        let second = split_progress_chunk(&mut pending, "像 43%\rprovision\n\rready".as_bytes());
        assert_eq!(second, vec!["下载镜像 43%", "provision"]);
        assert_eq!(String::from_utf8(pending).unwrap(), "ready");
    }

    #[test]
    fn start_args_omit_rosetta_after_user_skips() {
        let with = start_args("vpnmgr", true);
        let without = start_args("vpnmgr", false);
        assert!(with.contains(&"--vz-rosetta".to_string()));
        assert!(!without.contains(&"--vz-rosetta".to_string()));
        assert!(without.windows(2).any(|w| w == ["--vm-type", "vz"]));
        assert!(without.contains(&"--activate=false".to_string()));
    }

    #[tokio::test]
    #[ignore] // needs colima installed
    async fn status_of_absent_profile_is_stopped() {
        assert_eq!(status("definitely-no-such-profile").await, VmStatus::Stopped);
    }
}
