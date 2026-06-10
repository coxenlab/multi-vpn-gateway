//! 自带 colima VM(app 专属 profile,绝不碰用户 `default`)的生命周期 + 健康。
//!
//! 用 colima CLI(`tokio::process`)。VM 内的 docker.sock 经 `DOCKER_HOST` 注入给
//! [`crate::docker::connect`](docker::docker_socket 读 `DOCKER_HOST` 去 `unix://` 前缀)。
//! 命门 #4 不受影响:sock 是 unix domain socket,UI/转发端口仍只绑 127.0.0.1。
//! 命门隔离:`--activate=false` 不改用户 active docker context;专属 profile 与 `default` 互不干扰。

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::process::Command;

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
/// (父进程在全部转发建立后才退出 0)。ServerAlive*:VM 停/断后隧道自杀,不留死进程占口。
pub fn tunnel_args(ssh_config: &str, profile: &str, ports: &[u16]) -> Vec<String> {
    let mut a: Vec<String> = [
        "-F", ssh_config,
        "-o", "ControlMaster=no",
        "-o", "ControlPath=none",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ConnectTimeout=10", // 半死 hostagent 可能 accept 后不发 banner,不设则 ssh 永挂
        "-o", "ServerAliveInterval=10",
        "-o", "ServerAliveCountMax=3",
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

/// 起专属 VM:vz + rosetta(EC amd64 经 Rosetta;aTrust 容器仍须原生 arm64 镜像)。
/// `--activate=false`:不改用户 active docker context(不干扰其 `default`/`docker` CLI)。
/// 阻塞至 VM + dockerd 就绪(colima start 自身等 dockerd);**首次**会下载 guest 镜像(分钟级)。幂等。
pub async fn start(profile: &str) -> Result<()> {
    if !colima_present().await {
        return Err(anyhow!("未找到 colima(请先安装;后续版本会随 app 内置打包)"));
    }
    let st = Command::new("colima")
        .args([
            "start",
            profile,
            "--vm-type",
            "vz",
            "--vz-rosetta",
            "--activate=false",
            "--cpu",
            "4",
            "--memory",
            "6",
            "--disk",
            "60",
        ])
        .status()
        .await
        .map_err(|e| anyhow!("colima start {profile}: {e}"))?;
    if !st.success() {
        return Err(anyhow!("colima start {profile} 失败(exit {:?})", st.code()));
    }
    Ok(())
}

/// 确保 VM 在跑(已 Running 则跳过 start)。
pub async fn ensure_running(profile: &str) -> Result<()> {
    if status(profile).await == VmStatus::Running {
        return Ok(());
    }
    start(profile).await
}

/// 设 `DOCKER_HOST` 指向该 profile,等 dockerd 可 ping(colima start 后通常已 ready,补一层重试)。
pub async fn wait_docker_ready(profile: &str, timeout_secs: u64) -> Result<()> {
    std::env::set_var("DOCKER_HOST", docker_host(profile));
    let mut waited = 0u64;
    loop {
        if socket_path(profile).exists() && crate::docker::connect().await.is_ok() {
            return Ok(());
        }
        if waited >= timeout_secs {
            return Err(anyhow!("等 {profile} 的 dockerd 就绪超时({timeout_secs}s)"));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        waited += 1;
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
    fn docker_host_uses_unix_scheme() {
        // docker_host 读 HOME;断言 scheme 前缀与 socket 后缀,不绑死 home。
        let h = docker_host("vpnmgr");
        assert!(h.starts_with("unix://"));
        assert!(h.ends_with("/.colima/vpnmgr/docker.sock"));
    }

    #[tokio::test]
    #[ignore] // needs colima installed
    async fn status_of_absent_profile_is_stopped() {
        assert_eq!(status("definitely-no-such-profile").await, VmStatus::Stopped);
    }
}
