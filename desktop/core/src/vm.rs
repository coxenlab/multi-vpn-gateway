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
