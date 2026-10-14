//! 自带 colima VM(app 专属 profile,绝不碰用户 `default`)的生命周期 + 健康。
//!
//! 用 colima CLI(`tokio::process`)。VM 内的 docker.sock 经 `DOCKER_HOST` 注入给
//! [`crate::docker::connect`](docker::docker_socket 读 `DOCKER_HOST` 去 `unix://` 前缀)。
//! 命门 #4 不受影响:sock 是 unix domain socket,UI/转发端口仍只绑 127.0.0.1。
//! 命门隔离:`--activate=false` 不改用户 active docker context;专属 profile 与 `default` 互不干扰。

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    Running,
    /// 含「profile 不存在」与「已停止」。
    Stopped,
}

/// 纯:运行环境根目录 = `<home>/<身份的 colima_home_rel>`;旧身份是 colima 默认的 `<home>/.colima`。
/// 通道管家放在 `~/.tongdao/colima`:与旧应用不共用 `_lima`(网络定义、出站进程),
/// 且不放 Application Support 下(路径太长,lima 的套接字路径有 104 字节上限)。
pub fn colima_home_in(home: impl AsRef<Path>, profile: &str) -> PathBuf {
    home.as_ref().join(crate::identity::for_profile(profile).colima_home_rel.unwrap_or(".colima"))
}

/// 运行环境根目录(读 `$HOME`)。
pub fn colima_home(profile: &str) -> PathBuf {
    colima_home_in(std::env::var("HOME").unwrap_or_default(), profile)
}

/// 纯:该 profile 的 `colima` 命令。通道管家身份设 `COLIMA_HOME`(= [`colima_home_in`]);
/// 旧身份不设,与改动前相同。
fn colima_command_in(home: &str, profile: &str) -> Command {
    colima_command_with_debug(home, profile, std::env::var("GODEBUG").ok().as_deref())
}

/// Only the new runtime's descendants bypass macOS supplemental resolver files.
/// gvisor-tap-vsock uses net.Resolver{PreferGo:false}; forcing the Go path avoids
/// re-reading our /etc/resolver fake-IP domains for a container's upstream query.
/// Keep other Go options (notably CPU compatibility workarounds) intact.
fn usernet_godebug(inherited: Option<&str>) -> String {
    let mut options: Vec<_> = inherited.unwrap_or("").split(',').map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("netdns=")).collect();
    options.push("netdns=go");
    options.join(",")
}

fn colima_command_with_debug(home: &str, profile: &str, inherited: Option<&str>) -> Command {
    let mut cmd = Command::new("colima");
    if crate::identity::for_profile(profile).colima_home_rel.is_some() {
        cmd.env("COLIMA_HOME", colima_home_in(home, profile));
        cmd.env("LIMA_HOME", colima_home_in(home, profile).join("_lima"));
        cmd.env("GODEBUG", usernet_godebug(inherited));
    }
    cmd
}

fn prepare_colima_home_in(home: &Path, profile: &str) -> Result<()> {
    let Some(relative) = crate::identity::for_profile(profile).colima_home_rel else { return Ok(()) };
    anyhow::ensure!(home.is_absolute() && home.is_dir(), "无法定位独立运行环境的主目录");
    let mut path = home.to_path_buf();
    for component in Path::new(relative).components() {
        anyhow::ensure!(matches!(component, std::path::Component::Normal(_)), "运行环境目录配置无效");
        path.push(component);
        match std::fs::create_dir(&path) {
            Ok(()) => {},
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {},
            Err(error) => return Err(error.into()),
        }
        anyhow::ensure!(std::fs::symlink_metadata(&path)?.file_type().is_dir(),
            "独立运行环境目录不可用或是符号链接: {}", path.display());
    }
    Ok(())
}

fn colima_command(profile: &str) -> Result<Command> {
    let home = std::env::var("HOME").unwrap_or_default();
    // Colima 0.10.3 ignores a nonexistent COLIMA_HOME and silently falls back
    // to ~/.colima. Establish our directory before *any* command can do that.
    prepare_colima_home_in(Path::new(&home), profile)?;
    Ok(colima_command_in(&home, profile))
}

/// 纯:docker.sock 路径 = `<运行环境根目录>/<profile>/docker.sock`。
pub fn socket_path_in(home: &str, profile: &str) -> PathBuf {
    colima_home_in(home, profile).join(profile).join("docker.sock")
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
    colima_home_in(home, profile)
        .join("_lima")
        .join(format!("colima-{profile}"))
        .join("ssh.config")
}

/// ssh.config 路径(读 `$HOME`)。
pub fn ssh_config_path(profile: &str) -> PathBuf {
    ssh_config_path_in(&std::env::var("HOME").unwrap_or_default(), profile)
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

const GENERIC_APPARMOR_PROFILE: &str =
    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/tongdao-generic.apparmor"));
const GENERIC_APPARMOR_INSTALL_SCRIPT: &str = r#"set -e
enabled=$(cat /sys/module/apparmor/parameters/enabled 2>/dev/null || true)
[ "$enabled" = Y ] || { echo 'AppArmor is unavailable' >&2; exit 41; }
command -v apparmor_parser >/dev/null 2>&1 || { echo 'apparmor_parser is unavailable' >&2; exit 42; }
tmp=$(mktemp /tmp/tongdao-generic-apparmor.XXXXXX)
dst=/etc/apparmor.d/.tongdao-generic.$$
trap 'rm -f "$tmp"; sudo -n rm -f "$dst"' EXIT
cat >"$tmp"
sudo -n install -D -m 0644 "$tmp" "$dst"
sudo -n mv -f "$dst" /etc/apparmor.d/tongdao-generic
sudo -n apparmor_parser -r /etc/apparmor.d/tongdao-generic
sudo -n grep -q '^tongdao-generic ' /sys/kernel/security/apparmor/profiles
sudo -n systemctl is-enabled --quiet apparmor.service || sudo -n systemctl enable apparmor.service
echo TONGDAO_GENERIC_APPARMOR_OK"#;

/// 为完整 init 的通用桌面容器装载最小 AppArmor 策略。
///
/// 策略只允许 systemd 启动所需的私有 cgroup2 remount；不存在 AppArmor、解析器、免密
/// VM sudo 或加载确认时都失败关闭，绝不退回 `unconfined`。调用方只在托管 VM 中准备
/// `systemd: true` 的 custom 通道时调用，普通 VPN 容器不依赖此能力。
pub async fn ensure_generic_apparmor(profile: &str) -> Result<()> {
    let cfg = ssh_config_path(profile);
    if !cfg.exists() {
        return Err(anyhow!("ssh config 不存在: {}", cfg.display()));
    }
    let mut child = Command::new("ssh")
        .kill_on_drop(true)
        .args([
            "-F", &cfg.display().to_string(),
            "-o", "ControlMaster=no",
            "-o", "ControlPath=none",
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=5",
            &format!("lima-colima-{profile}"),
            "--", GENERIC_APPARMOR_INSTALL_SCRIPT,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| anyhow!("启动 AppArmor 配置命令失败: {error}"))?;
    let mut stdin = child.stdin.take().ok_or_else(|| anyhow!("无法写入 AppArmor 配置"))?;
    stdin.write_all(GENERIC_APPARMOR_PROFILE.as_bytes()).await
        .map_err(|error| anyhow!("写入 AppArmor 配置失败: {error}"))?;
    stdin.shutdown().await.map_err(|error| anyhow!("结束 AppArmor 配置输入失败: {error}"))?;
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .map_err(|_| anyhow!("装载通用容器 AppArmor 配置超时(20s)"))?
        .map_err(|error| anyhow!("等待 AppArmor 配置命令失败: {error}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !stdout.lines().any(|line| line == "TONGDAO_GENERIC_APPARMOR_OK") {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!(
            "通用安装环境需要 AppArmor 限定策略，当前 VM 无法安全加载: {}",
            stderr.trim().lines().last().unwrap_or("未确认策略已生效")
        ));
    }
    Ok(())
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
        "-N",
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
pub async fn spawn_docker_sock_tunnel(profile: &str, local_sock: &std::path::Path) -> Result<tokio::process::Child> {
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
    .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true);
    cmd.spawn().map_err(|e| anyhow!("ssh 启动失败: {e}"))
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

/// 限时等待并回收自己启动的进程；超时不能解释为虚拟机已停止。
async fn command_status(mut command: Command, budget: Duration, label: &str) -> Result<std::process::ExitStatus> {
    // 外壳可能先关日志管道；避免 Go 子进程向已关闭的 stdout/stderr 写入而提前 SIGPIPE。
    let mut child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
        .kill_on_drop(true).spawn().map_err(|e| anyhow!("{label}: {e}"))?;
    match tokio::time::timeout(budget, child.wait()).await {
        Ok(result) => result.map_err(|e| anyhow!("{label}: {e}")),
        Err(_) => {
            let _ = child.kill().await;
            crate::ev!(warn, "vm", "command_timeout", "运行环境命令超时", { "command": label });
            Err(anyhow!("{label} 超时"))
        }
    }
}

/// exit 0 = Running，正常返回非零 = Stopped；命令失败或超时返回 Err。
pub async fn status(profile: &str) -> Result<VmStatus> {
    let mut cmd = colima_command(profile)?;
    cmd.args(["status", "--profile", profile]);
    let result = command_status(cmd, Duration::from_secs(15), "colima status").await?;
    Ok(if result.success() { VmStatus::Running } else { VmStatus::Stopped })
}

/// 用于释放确认：命令失败、缺少目标、未知状态都不能当作已停止。
pub async fn stopped_confirmed(profile: &str) -> Result<bool> {
    let output = tokio::time::timeout(Duration::from_secs(8),
        colima_command(profile)?.args(["list", "--json"]).kill_on_drop(true).output()).await??;
    anyhow::ensure!(output.status.success(), "无法读取虚拟机列表");
    stopped_from_listing(&String::from_utf8(output.stdout)?, profile)
}

fn stopped_from_listing(text: &str, profile: &str) -> Result<bool> {
    let mut found = None;
    for line in text.lines().filter(|line| !line.trim().is_empty()) {
        let item: serde_json::Value = serde_json::from_str(line)?;
        if item["name"] == profile {
            anyhow::ensure!(found.is_none(), "虚拟机列表存在重复目标");
            found = Some(match item["status"].as_str() {
                Some("Stopped") => true,
                Some("Running") => false,
                _ => anyhow::bail!("虚拟机状态待确认"),
            });
        }
    }
    found.ok_or_else(|| anyhow!("虚拟机列表缺少目标实例"))
}

fn start_args(profile: &str, enable_rosetta: bool) -> Vec<String> {
    let mut args = vec!["start".to_string(), profile.to_string(), "--vm-type".to_string(), "vz".to_string()];
    if enable_rosetta {
        args.push("--vz-rosetta".to_string());
    }
    args.extend([
        // 分流/控制/noVNC 已由 app 自持 loopback SSH 转发。禁用 Colima 将来宾通配
        // 监听复制到宿主通配地址的规则；Docker Unix socket、VM SSH、usernet 出站不受影响。
        // 当前 Lima 在 none 下仍保留来宾回环 TCP 的默认回环转发，不等于关闭全部监听。
        "--port-forwarder", "none", "--activate=false", "--cpu", "4", "--memory", "6", "--disk", "60",
    ].into_iter().map(String::from));
    if profile != "vpnmgr" {
        args.extend(["--mount", "none", "--ssh-config=false", "--template=false"].into_iter().map(String::from));
    }
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
pub async fn start_with_progress<F>(profile: &str, enable_rosetta: bool, on_progress: F) -> Result<()>
where
    F: FnMut(String) + Send,
{
    if !colima_present().await {
        return Err(anyhow!("未找到 colima(请先安装;后续版本会随 app 内置打包)"));
    }
    let mut command = colima_command(profile)?;
    command.args(start_args(profile, enable_rosetta));
    run_start_command(command, on_progress).await?;
    ensure_guest_dns(profile).await
}

// The bundled minimal image has networkd DNS configuration but no resolved
// service. Repair only its dangling stub link; working/custom resolvers survive.
const GUEST_DNS_REPAIR: &str = include_str!("guest_dns_repair.sh");

pub(crate) async fn ensure_guest_dns(profile: &str) -> Result<()> {
    if crate::identity::for_profile(profile).colima_home_rel.is_none() {
        return Ok(());
    }
    let mut command = colima_command(profile)?;
    let script = format!("sudo -n sh <<'TONGDAO_GUEST_DNS_SCRIPT'\n{GUEST_DNS_REPAIR}\nTONGDAO_GUEST_DNS_SCRIPT");
    command.args(["--profile", profile, "ssh", "--", "sh", "-c", &script]);
    let output = tokio::time::timeout(Duration::from_secs(20),
        command.kill_on_drop(true).output()).await
        .map_err(|_| anyhow!("运行环境 DNS 检查超时"))??;
    anyhow::ensure!(output.status.success(), "运行环境 DNS 初始化失败: {}",
        String::from_utf8_lossy(&output.stderr).trim());
    Ok(())
}

async fn run_start_command<F>(mut command: Command, mut on_progress: F) -> Result<()>
where F: FnMut(String) + Send {
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| anyhow!("运行环境启动命令不可用: {e}"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("无法捕获 colima stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("无法捕获 colima stderr"))?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let stdout_task = tokio::spawn(read_progress(stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_progress(stderr, tx.clone()));
    drop(tx);

    let mut wait = Box::pin(child.wait());
    let mut streams_open = true;
    let mut failure_hint = None;
    let st = loop {
        tokio::select! {
            result = &mut wait => {
                break result.map_err(|e| anyhow!("无法确认运行环境启动结果: {e}"))?;
            }
            line = rx.recv(), if streams_open => {
                match line {
                    Some(line) => {
                        failure_hint = crate::startup_feedback::failure_hint(&line).or(failure_hint);
                        on_progress(line);
                    },
                    None => streams_open = false,
                }
            }
        }
    };
    let _ = stdout_task.await;
    let _ = stderr_task.await;
    while let Ok(line) = rx.try_recv() {
        failure_hint = crate::startup_feedback::failure_hint(&line).or(failure_hint);
        on_progress(line);
    }
    if !st.success() {
        return Err(anyhow!("{}", failure_hint.unwrap_or("运行环境启动未完成。请查看运行诊断，核对原因后重试连接。")));
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

// ── 私网出站守卫(VM 层,命门 #8)─────────────────────────────────────────────
//
// VM 出站走 lima usernet(gvisor-tap-vsock v0.8.9):其 TCP forwarder 硬编码 maxInFlight=10,
// 宿主侧 `net.Dial` 无超时且在释放名额前同步阻塞;macOS `net.inet.tcp.keepinit`=75s。容器里
// 没被隧道 / 客户端代理接管的私网目标(aTrust 探测的内网备用线路、未登录通道的探活、绑定但
// 服务端未下发的网段)沿默认路由从 eth0 漏到 VM,每个占一个槽 75s;10 个占满,VM 内所有新建
// TCP 的 SYN 被静默丢弃,全部通道一起「时通时断」、登录报「服务器不可达」(2026-09-10 实测
// 在途 9 → 建连 3/3、10 → 0/3;上游 issue containers/gvisor-tap-vsock#676,修复 PR #698 已合并
// 未发布,lima 2.2.0 仍锁 v0.8.9)。
//
// 挂在 VM 的 DOCKER-USER 链(FORWARD 首跳)而不是各容器内:约束 VPN Docker 网段的 IPv4 转发,覆盖容器
// 自发重启(unless-stopped)后的流量;不覆盖 VM OUTPUT、IPv6 或其他网络,不依赖镜像里有 iptables /
// NET_ADMIN,也不会被 VPN 客户端的 iptables 操作冲掉。REJECT(TCP 回 RST)让容器立即失败而不是
// 漏出去挂 75s。豁免:docker 网段自身、VM 自己的网段、以及宿主当前直连的局域网网段(这些目标
// 需要保留合法互通,其中不可达目标仍可能占槽;换网 / 睡醒后更新豁免)。
// ⚠️ 别改成容器内 `ip route add unreachable`:本机发包先查路由再过 nat OUTPUT,会把 EC 用
// nat REDIRECT(→4440)接管的网段资源在查路由时就拒掉(实测)。Web 栈跑在 Docker Desktop
// 上没有 usernet 这个上限,不下发。

const EGRESS_CHAIN: &str = "VPNMGR_EGRESS";
const PRIVATE_NETS: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10"];

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EgressPolicy {
    pub bound: Vec<String>,
    pub gateways: Vec<String>,
    pub bridge: Option<String>,
}

/// 使用原始启用规则，不随通道停止、掉线或暂停分流而撤掉防回环保护。
pub fn egress_policy(rules: &[crate::store::Rule], channels: &[crate::store::ChannelPublic]) -> EgressPolicy {
    let mut bound = std::collections::BTreeSet::from(["198.18.0.0/15".to_owned()]);
    for r in rules.iter().filter(|r| r.enabled != 0 && r.kind == "ip") {
        if let Some((_, pattern)) = crate::webutil::normalize_stored_rule(&r.kind, &r.pattern) {
            if let Ok(net) = pattern.parse::<ipnet::Ipv4Net>() { bound.insert(net.trunc().to_string()); }
        }
    }
    let gateways: std::collections::BTreeSet<_> = channels.iter()
        .filter_map(|c| c.server.trim().parse::<std::net::Ipv4Addr>().ok())
        .map(|ip| format!("{ip}/32")).collect();
    EgressPolicy { bound: bound.into_iter().collect(), gateways: gateways.into_iter().collect(), bridge: None }
}

pub fn valid_bridge(name: &str) -> bool {
    !name.is_empty() && name.len() < 16 && !name.starts_with('-')
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
}

/// 合成在 VM 内执行的守卫脚本(纯函数,便于测)。`vpn_subnet` = docker VPN 网段;`exempt` =
/// 额外放行的目标网段(宿主直连局域网)。只在 COMMIT 时替换本链,失败保留旧规则。
pub fn egress_guard_script(vpn_subnet: &str, exempt: &[String]) -> String {
    egress_guard_script_with(vpn_subnet, exempt, &EgressPolicy::default())
}

pub fn egress_guard_script_with(vpn_subnet: &str, exempt: &[String], policy: &EgressPolicy) -> String {
    let mut sh = String::from("set -e\nIPT='sudo -n iptables -w 5'\n");
    // VM 侧锁覆盖 restore 与跳转检查,避免两个 app/SSH 调用交错插入重复跳转。
    sh.push_str("exec 9>/tmp/vpnmgr-egress.lock\nflock -w 10 9\n");
    // VM 自己的网段(usernet 网关 / VM DNS 所在)
    sh.push_str("VMSUB=$(ip -4 -o addr show eth0 | awk 'NR==1{print $4}')\n[ -n \"$VMSUB\" ]\n");
    sh.push_str(&format!("sudo -n iptables-restore --noflush -w 5 <<VPNMGR_RULES\n*filter\n:{c} - [0:0]\n", c = EGRESS_CHAIN));
    sh.push_str(&format!("-A {c} -d {s} -j RETURN\n", c = EGRESS_CHAIN, s = vpn_subnet));
    sh.push_str(&format!(
        "-A {c} -d $VMSUB -j RETURN\n",
        c = EGRESS_CHAIN
    ));
    for gateway in &policy.gateways {
        sh.push_str(&format!("-A {EGRESS_CHAIN} -d {gateway} -j RETURN\n"));
    }
    for n in &policy.bound {
        sh.push_str(&format!("-A {EGRESS_CHAIN} -d {n} -p tcp -j REJECT --reject-with tcp-reset\n"));
        sh.push_str(&format!("-A {EGRESS_CHAIN} -d {n} -j REJECT --reject-with icmp-net-unreachable\n"));
    }
    for e in exempt {
        sh.push_str(&format!("-A {c} -d {e} -j RETURN\n", c = EGRESS_CHAIN));
    }
    for n in PRIVATE_NETS {
        sh.push_str(&format!("-A {c} -d {n} -p tcp -j REJECT --reject-with tcp-reset\n", c = EGRESS_CHAIN));
        sh.push_str(&format!("-A {c} -d {n} -j REJECT --reject-with icmp-net-unreachable\n", c = EGRESS_CHAIN));
    }
    sh.push_str("COMMIT\nVPNMGR_RULES\n");
    if let Some(bridge) = &policy.bridge {
        for rule in [format!("-i {bridge} -s {vpn_subnet} -j {EGRESS_CHAIN}"),
            format!("-i {bridge} ! -s {vpn_subnet} -j DROP")] {
            sh.push_str(&format!("$IPT -C DOCKER-USER {rule} 2>/dev/null || $IPT -I DOCKER-USER 1 {rule}\n"));
        }
        // 先装好按接口的两条，再删旧版仅按源地址的跳转；重跑不会增加重复项。
        sh.push_str(&format!("while $IPT -C DOCKER-USER -s {vpn_subnet} -j {EGRESS_CHAIN} 2>/dev/null; do\n$IPT -D DOCKER-USER -s {vpn_subnet} -j {EGRESS_CHAIN}\ndone\necho VPNMGR_EGRESS_OK"));
    } else {
        sh.push_str(&format!(
        "$IPT -C DOCKER-USER -s {s} -j {c} 2>/dev/null || $IPT -I DOCKER-USER 1 -s {s} -j {c}; echo VPNMGR_EGRESS_OK",
        s = vpn_subnet, c = EGRESS_CHAIN
        ));
    }
    sh
}

/// 从 macOS `ifconfig` 输出解析宿主直连的 IPv4 网段(只取 en*/bridge* 之外的物理口 en*;跳过
/// lo/utun/vmnet/bridge/awdl/llw)。纯函数,便于测。
pub fn parse_host_lan_subnets(ifconfig: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur_if = String::new();
    for line in ifconfig.lines() {
        if !line.starts_with(' ') && !line.starts_with('\t') {
            cur_if = line.split(':').next().unwrap_or("").to_string();
            continue;
        }
        if !cur_if.starts_with("en") {
            continue;
        }
        let t: Vec<&str> = line.split_whitespace().collect();
        if t.len() >= 4 && t[0] == "inet" && t[2] == "netmask" {
            let (Ok(ip), Some(mask)) = (t[1].parse::<std::net::Ipv4Addr>(), t[3].strip_prefix("0x")) else { continue };
            let Ok(mask) = u32::from_str_radix(mask, 16) else { continue };
            if ip.is_loopback() || ip.is_link_local() || mask == 0 {
                continue;
            }
            let prefix = mask.count_ones();
            let net = std::net::Ipv4Addr::from(u32::from(ip) & mask);
            let cidr = format!("{net}/{prefix}");
            if !out.contains(&cidr) {
                out.push(cidr);
            }
        }
    }
    out
}

async fn host_lan_subnets() -> Vec<String> {
    let out = Command::new("ifconfig").output().await;
    match out {
        Ok(o) => parse_host_lan_subnets(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => Vec::new(),
    }
}

type GuardState = std::sync::Arc<tokio::sync::Mutex<String>>;
static EGRESS_GUARDS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, GuardState>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// 每次调用都实际下发守卫,缓存仅用于日志去重。按 profile 串行,VM 重建也不跳过。
/// `force` 只要求记录成功事件。经 ssh 进 VM 用 `sudo -n iptables`;
/// 成功判据是脚本末尾回显的标记(脚本 `set -e`,任一步失败即无标记)。
pub async fn ensure_egress_guard(profile: &str, vpn_subnet: &str, policy: &EgressPolicy, force: bool) -> Result<()> {
    let _: ipnet::Ipv4Net = vpn_subnet.parse().map_err(|_| anyhow!("无效 VPN IPv4 网段"))?;
    for net in policy.bound.iter().chain(&policy.gateways) {
        let _: ipnet::Ipv4Net = net.parse().map_err(|_| anyhow!("无效守卫目标网段"))?;
    }
    anyhow::ensure!(policy.bridge.as_deref().is_none_or(valid_bridge), "无效 VPN 网桥名");
    let guard = EGRESS_GUARDS.lock().map_err(|_| anyhow!("守卫状态锁不可用"))?
        .entry(profile.to_owned()).or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(String::new())))
        .clone();
    let mut last = guard.lock().await;
    let cfg = ssh_config_path(profile);
    if !cfg.exists() {
        return Err(anyhow!("ssh config 不存在: {}", cfg.display()));
    }
    let exempt = host_lan_subnets().await;
    let script = egress_guard_script_with(vpn_subnet, &exempt, policy);
    let out = tokio::time::timeout(
        Duration::from_secs(20),
        Command::new("ssh")
            .kill_on_drop(true)
            .args([
                "-F", &cfg.display().to_string(),
                "-o", "ControlMaster=no",
                "-o", "ControlPath=none",
                "-o", "BatchMode=yes",
                "-o", "ConnectTimeout=5",
                &format!("lima-colima-{profile}"),
                "--", &script,
            ])
            .output(),
    )
    .await
    .map_err(|_| anyhow!("ssh 超时(20s)"))?
    .map_err(|e| anyhow!("ssh: {e}"))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() || !stdout.lines().any(|s| s == "VPNMGR_EGRESS_OK") {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("守卫脚本未完成: {}", err.trim().lines().last().unwrap_or("(无输出)")));
    }
    if force || *last != script {
        crate::ev!(info, "vm", "egress_guard_applied", "VM 私网出站守卫已下发", {
            "vpn_subnet": vpn_subnet, "exempt_host_lan": exempt, "profile": profile
        });
    }
    *last = script;
    Ok(())
}

/// 确保 VM 在跑(已 Running 则跳过 start)。
pub async fn ensure_running(profile: &str) -> Result<()> {
    if status(profile).await? == VmStatus::Running {
        return ensure_guest_dns(profile).await;
    }
    start(profile).await
}

/// 停 VM。用于传输层坏死时的自动恢复(停→冷起,实测 2026-07-02 该路径 ~15s 满血)。
pub async fn stop(profile: &str) -> Result<()> {
    let mut command = colima_command(profile)?;
    command.args(["stop", profile]);
    let st = match command_status(command, Duration::from_secs(60), "colima stop").await {
        Ok(status) => status,
        Err(error) => {
            // 写操作结果不明只读回一次，不能重复 stop。
            if stopped_confirmed(profile).await.unwrap_or(false) { return Ok(()); }
            return Err(error);
        }
    };
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
    fn generic_apparmor_only_allows_scoped_cgroup_remount() {
        assert!(GENERIC_APPARMOR_PROFILE.contains("profile tongdao-generic"));
        assert!(GENERIC_APPARMOR_PROFILE.contains(
            "mount options=(rw,nosuid,nodev,noexec,remount,relatime) -> /sys/fs/cgroup/"
        ));
        assert!(GENERIC_APPARMOR_PROFILE.contains("mount options=(rw,rslave) -> /,"));
        assert!(GENERIC_APPARMOR_PROFILE.contains("mount options=(rw,rslave) -> /dev/,"));
        assert!(GENERIC_APPARMOR_PROFILE.contains(
            "mount fstype=tmpfs options=(rw,nosuid,nodev,noexec) tmpfs -> /dev/shm/,"
        ));
        assert!(GENERIC_APPARMOR_PROFILE.contains(
            "mount options=(rw,rbind) / -> /run/systemd/unit-root/,"
        ));
        assert!(!GENERIC_APPARMOR_PROFILE.contains("unconfined"));
        assert!(!GENERIC_APPARMOR_PROFILE.contains("mount -> /tmp"));
        assert!(!GENERIC_APPARMOR_PROFILE.contains("/** rw,"));
        assert!(GENERIC_APPARMOR_INSTALL_SCRIPT.contains(
            "install -D -m 0644 \"$tmp\" \"$dst\""
        ));
        assert!(GENERIC_APPARMOR_INSTALL_SCRIPT.contains(
            "mv -f \"$dst\" /etc/apparmor.d/tongdao-generic"
        ));
        assert!(GENERIC_APPARMOR_INSTALL_SCRIPT.contains(
            "apparmor_parser -r /etc/apparmor.d/tongdao-generic"
        ));
        assert!(GENERIC_APPARMOR_INSTALL_SCRIPT.contains("is-enabled --quiet apparmor.service"));
    }

    #[test]
    fn guest_dns_repairs_only_known_dangling_stub_and_preserves_custom_dns() {
        use std::io::Write;
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        std::fs::create_dir_all(root.path().join("run/systemd/network")).unwrap();
        let resolver = root.path().join("etc/resolv.conf");
        let network = root.path().join("run/systemd/network/10-netplan-eth0.network");
        let run = || {
            let script = GUEST_DNS_REPAIR.replacen("root=/", "root=$1", 1);
            let mut child = std::process::Command::new("sh")
                .args(["-s", "--"]).arg(root.path())
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null())
                .spawn().unwrap();
            child.stdin.take().unwrap().write_all(script.as_bytes()).unwrap();
            child.wait().unwrap().success()
        };
        std::os::unix::fs::symlink("../run/systemd/resolve/stub-resolv.conf", &resolver).unwrap();
        for invalid in ["DNS=999.1.2.3\n", "DNS=1.2.3.4\nDNS=2.3.4.5\n", "DNS=garbage\n", ""] {
            std::fs::write(&network, invalid).unwrap();
            assert!(!run());
            assert!(std::fs::symlink_metadata(&resolver).unwrap().file_type().is_symlink());
        }
        std::fs::write(&network, "[Network]\nDNS=192.168.5.2\n").unwrap();
        assert!(run());
        assert_eq!(std::fs::read_to_string(&resolver).unwrap(), "nameserver 192.168.5.2\n");
        assert!(std::fs::read_link(root.path().join("etc/resolv.conf.tongdao-original")).is_ok());
        std::fs::write(&resolver, "nameserver 10.20.30.40\n").unwrap();
        assert!(run());
        assert_eq!(std::fs::read_to_string(&resolver).unwrap(), "nameserver 10.20.30.40\n");
        std::fs::remove_file(&resolver).unwrap();
        std::os::unix::fs::symlink("custom-missing", &resolver).unwrap();
        assert!(run());
        assert_eq!(std::fs::read_link(&resolver).unwrap(), PathBuf::from("custom-missing"));
    }

    #[test]
    fn release_confirmation_requires_explicit_target_status() {
        assert!(stopped_from_listing(r#"{"name":"vpnmgr-dev","status":"Stopped"}"#, "vpnmgr-dev").unwrap());
        assert!(!stopped_from_listing(r#"{"name":"vpnmgr-dev","status":"Running"}"#, "vpnmgr-dev").unwrap());
        for listing in ["", "not json", r#"{"name":"vpnmgr","status":"Stopped"}"#,
            r#"{"name":"vpnmgr-dev","status":"Broken"}"#] {
            assert!(stopped_from_listing(listing, "vpnmgr-dev").is_err());
        }
    }

    #[test]
    fn socket_path_layout() {
        assert_eq!(
            socket_path_in("/Users/x", "vpnmgr"),
            PathBuf::from("/Users/x/.colima/vpnmgr/docker.sock")
        );
    }

    #[test]
    fn egress_guard_script_shape() {
        let sh = egress_guard_script("172.18.0.0/16", &["192.168.0.0/24".to_string()]);
        assert!(sh.starts_with("set -e\n"), "must abort on first failure");
        assert!(sh.contains("iptables-restore --noflush -w 5"));
        assert!(!sh.contains("$IPT -F"));
        assert!(sh.find("COMMIT").unwrap() < sh.find("-C DOCKER-USER").unwrap());
        assert!(sh.contains("-A VPNMGR_EGRESS -d 172.18.0.0/16 -j RETURN"));
        assert!(sh.contains("-A VPNMGR_EGRESS -d 192.168.0.0/24 -j RETURN"));
        for n in PRIVATE_NETS {
            assert!(sh.contains(&format!("-d {n} -p tcp -j REJECT --reject-with tcp-reset")), "{n}");
        }
        assert!(sh.contains("-C DOCKER-USER -s 172.18.0.0/16 -j VPNMGR_EGRESS 2>/dev/null || $IPT -I DOCKER-USER 1"));
        assert!(sh.ends_with("echo VPNMGR_EGRESS_OK"));
        // 豁免必须排在 REJECT 前面
        assert!(sh.find("192.168.0.0/24 -j RETURN").unwrap() < sh.find("-j REJECT").unwrap());
        // 打印出来供手工下发核对(cargo test -- --nocapture)
        println!("EGRESS_GUARD_SCRIPT={sh}");
    }

    #[test]
    fn guard_bound_targets_precede_lan_exemptions_and_check_source_interface() {
        let policy = EgressPolicy { bound: vec!["192.168.0.99/32".into(), "198.18.0.0/15".into()],
            gateways: vec!["192.168.0.1/32".into()], bridge: Some("br-123456abcdef".into()) };
        let sh = egress_guard_script_with("172.18.0.0/16", &["192.168.0.0/24".into()], &policy);
        let pos = |s: &str| sh.find(s).unwrap();
        assert!(pos("-d 172.18.0.0/16 -j RETURN") < pos("-d $VMSUB -j RETURN"));
        assert!(pos("-d $VMSUB -j RETURN") < pos("-d 192.168.0.1/32 -j RETURN"));
        assert!(pos("-d 192.168.0.1/32 -j RETURN") < pos("-d 192.168.0.99/32 -p tcp -j REJECT"));
        assert!(pos("-d 198.18.0.0/15 -j REJECT") < pos("-d 192.168.0.0/24 -j RETURN"));
        assert!(pos("-d 192.168.0.0/24 -j RETURN") < pos("-d 10.0.0.0/8 -p tcp -j REJECT"));
        assert!(sh.contains("-i br-123456abcdef -s 172.18.0.0/16 -j VPNMGR_EGRESS"));
        assert!(sh.contains("-i br-123456abcdef ! -s 172.18.0.0/16 -j DROP"));
        assert!(sh.contains("-D DOCKER-USER -s 172.18.0.0/16 -j VPNMGR_EGRESS"));
        let fallback = egress_guard_script_with("172.18.0.0/16", &[], &EgressPolicy { bridge: None, ..policy });
        assert!(!fallback.contains("-i br-") && !fallback.contains("-j DROP"));
        assert!(fallback.contains("-C DOCKER-USER -s 172.18.0.0/16 -j VPNMGR_EGRESS"));
        assert!(fallback.contains("-d 198.18.0.0/15 -j REJECT"));
        // 只解析脚本语法，不执行任何 sudo、iptables、接口或宿主命令。
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("guard.sh");
        for script in [sh, fallback] {
            std::fs::write(&path, script).unwrap();
            assert!(std::process::Command::new("/bin/bash").arg("-n").arg(&path).status().unwrap().success());
        }
    }

    #[test]
    fn docker_bridge_uses_explicit_name_or_valid_network_id() {
        let network = |value| serde_json::from_value::<bollard::models::Network>(value).unwrap();
        assert_eq!(crate::docker::network_bridge(&network(serde_json::json!({"Id":"123456abcdef9999"}))).as_deref(), Some("br-123456abcdef"));
        assert_eq!(crate::docker::network_bridge(&network(serde_json::json!({"Id":"123456abcdef9999", "Options":{"com.docker.network.bridge.name":"vpn-bridge"}}))).as_deref(), Some("vpn-bridge"));
        for value in [serde_json::json!({}), serde_json::json!({"Id":"bad"}),
            serde_json::json!({"Id":"zzzzzzzzzzzz"}), serde_json::json!({"Options":{"com.docker.network.bridge.name":"x;touch /tmp/oops"}})] {
            assert!(crate::docker::network_bridge(&network(value)).is_none());
        }
    }

    #[test]
    fn parse_host_lan_subnets_from_ifconfig() {
        let out = "lo0: flags=8049<UP,LOOPBACK> mtu 16384\n\tinet 127.0.0.1 netmask 0xff000000\n\
en0: flags=8863<UP,BROADCAST> mtu 1500\n\tinet 192.168.7.14 netmask 0xffffff00 broadcast 192.168.7.255\n\
en5: flags=8863<UP> mtu 1500\n\tinet 10.1.2.3 netmask 0xfffff000 broadcast 10.1.15.255\n\
utun4: flags=8051<UP,POINTOPOINT> mtu 1420\n\tinet 10.66.0.2 --> 10.66.0.2 netmask 0xffffffff\n\
bridge100: flags=8863<UP> mtu 1500\n\tinet 192.168.64.1 netmask 0xffffff00 broadcast 192.168.64.255\n\
en0: flags=8863<UP,BROADCAST> mtu 1500\n\tinet 169.254.5.5 netmask 0xffff0000\n";
        let got = parse_host_lan_subnets(out);
        assert_eq!(got, vec!["192.168.7.0/24".to_string(), "10.1.0.0/20".to_string()]);
    }

    #[test]
    fn ssh_config_path_layout() {
        assert_eq!(
            ssh_config_path_in("/Users/x", "vpnmgr"),
            PathBuf::from("/Users/x/.colima/_lima/colima-vpnmgr/ssh.config")
        );
    }

    #[test]
    fn tongdao_paths_live_under_own_root() {
        for profile in ["tongdao", "tongdao-dev", "tongdao-verify-x"] {
            for p in [
                colima_home_in("/Users/x", profile),
                socket_path_in("/Users/x", profile),
                ssh_config_path_in("/Users/x", profile),
            ] {
                assert!(p.starts_with("/Users/x/.tongdao/colima"), "{}", p.display());
                assert!(!p.to_string_lossy().contains("/.colima"), "{}", p.display());
            }
        }
        assert_eq!(socket_path_in("/Users/x", "tongdao"), PathBuf::from("/Users/x/.tongdao/colima/tongdao/docker.sock"));
        assert_eq!(
            ssh_config_path_in("/Users/x", "tongdao-dev"),
            PathBuf::from("/Users/x/.tongdao/colima/_lima/colima-tongdao-dev/ssh.config")
        );
    }

    #[test]
    fn legacy_paths_unchanged() {
        for profile in ["vpnmgr", "vpnmgr-dev", "default"] {
            assert_eq!(colima_home_in("/Users/x", profile), PathBuf::from("/Users/x/.colima"));
            assert_eq!(
                socket_path_in("/Users/x", profile),
                PathBuf::from(format!("/Users/x/.colima/{profile}/docker.sock"))
            );
            assert_eq!(
                ssh_config_path_in("/Users/x", profile),
                PathBuf::from(format!("/Users/x/.colima/_lima/colima-{profile}/ssh.config"))
            );
        }
    }

    fn envs(cmd: &Command) -> Vec<(String, Option<String>)> {
        cmd.as_std()
            .get_envs()
            .map(|(k, v)| (k.to_string_lossy().into_owned(), v.map(|v| v.to_string_lossy().into_owned())))
            .collect()
    }

    #[test]
    fn colima_home_env_only_for_tongdao() {
        // 只构造、不执行。
        for profile in ["tongdao", "tongdao-dev"] {
            let cmd = colima_command_with_debug("/Users/x", profile, None);
            assert_eq!(cmd.as_std().get_program(), "colima");
            assert_eq!(envs(&cmd), vec![("COLIMA_HOME".to_string(), Some("/Users/x/.tongdao/colima".to_string())),
                ("GODEBUG".to_string(), Some("netdns=go".to_string())),
                ("LIMA_HOME".to_string(), Some("/Users/x/.tongdao/colima/_lima".to_string()))]);
        }
        for profile in ["vpnmgr", "vpnmgr-dev"] {
            assert!(envs(&colima_command_in("/Users/x", profile)).is_empty(), "{profile}");
        }
        // 读 $HOME 的版本与 colima_home 一致。
        let home = colima_home("tongdao-dev").display().to_string();
        assert!(home.ends_with("/.tongdao/colima"));
        assert_eq!(envs(&colima_command("tongdao-dev").unwrap()), vec![("COLIMA_HOME".to_string(), Some(home.clone())),
            ("GODEBUG".to_string(), Some(usernet_godebug(std::env::var("GODEBUG").ok().as_deref()))),
            ("LIMA_HOME".to_string(), Some(format!("{home}/_lima")))]);
        assert!(envs(&colima_command("vpnmgr").unwrap()).is_empty());
    }

    #[test]
    fn runtime_dns_mode_is_scoped_and_preserves_unrelated_go_options() {
        assert_eq!(usernet_godebug(None), "netdns=go");
        assert_eq!(usernet_godebug(Some("cpu.avx2=off,netdns=cgo+2,madvdontneed=1,netdns=go+1")),
            "cpu.avx2=off,madvdontneed=1,netdns=go");
        for profile in ["tongdao", "tongdao-dev", "tongdao-verify-dns"] {
            let cmd = colima_command_with_debug("/Users/x", profile, Some("cpu.avx2=off,netdns=cgo"));
            assert!(envs(&cmd).contains(&("GODEBUG".into(), Some("cpu.avx2=off,netdns=go".into()))));
        }
        for profile in ["default", "vpnmgr", "vpnmgr-dev"] {
            assert!(envs(&colima_command_with_debug("/Users/x", profile, Some("netdns=cgo"))).is_empty());
        }
    }

    #[test]
    fn new_runtime_home_exists_before_cli_and_cannot_alias_legacy() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join(".colima"); std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("sentinel"), "unchanged").unwrap();
        prepare_colima_home_in(root.path(), "tongdao-verify-home").unwrap();
        let ours = colima_home_in(root.path(), "tongdao-verify-home");
        assert!(ours.is_dir());
        assert_eq!(std::fs::read_dir(&old).unwrap().count(), 1);
        std::fs::remove_dir(&ours).unwrap();
        std::os::unix::fs::symlink(&old, &ours).unwrap();
        assert!(prepare_colima_home_in(root.path(), "tongdao").is_err());
        assert_eq!(std::fs::read_to_string(old.join("sentinel")).unwrap(), "unchanged");
        let empty = tempfile::tempdir().unwrap();
        prepare_colima_home_in(empty.path(), "vpnmgr").unwrap();
        assert_eq!(std::fs::read_dir(empty.path()).unwrap().count(), 0);
        assert!(prepare_colima_home_in(Path::new(""), "tongdao").is_err());
    }


    #[test]
    fn socket_tunnel_args_shape() {
        let a = socket_tunnel_args("/tmp/ssh.config", "vpnmgr", "/tmp/dt.sock", "/var/run/docker.sock");
        assert!(a.contains(&"StreamLocalBindUnlink=yes".to_string()), "须能覆盖陈死本地 sock");
        assert!(a.contains(&"ControlMaster=no".to_string()), "独立连接,不依赖 hostagent 主连接");
        assert!(a.contains(&"/tmp/dt.sock:/var/run/docker.sock".to_string()));
        assert!(a.contains(&"-N".to_string()));
        assert!(!a.iter().any(|arg| arg.contains("-f")));
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

    #[tokio::test]
    async fn startup_output_is_drained_and_failure_has_actionable_feedback() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'Downloading fixture\\r43%% |====|\\n'; printf 'no space left on device: /private/fixture' >&2; exit 42"]);
        let mut lines = Vec::new();
        let error = run_start_command(command, |line| lines.push(line)).await.unwrap_err();
        assert!(lines.iter().any(|line| line == "43% |====|"));
        assert!(lines.iter().any(|line| line.contains("no space left")));
        assert!(error.to_string().contains("磁盘空间不足"));
        assert!(!error.to_string().contains("/private/fixture"));

        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'resolve redirect failed during earlier fallback\\n'; exit 0"]);
        // A recoverable diagnostic never overrides the real exit status.
        run_start_command(command, |_| {}).await.unwrap();
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'unknown failure' >&2; exit 42"]);
        assert!(run_start_command(command, |_| {}).await.unwrap_err().to_string().contains("运行诊断"));
    }

    #[tokio::test]
    async fn command_timeout_reaps_only_its_owned_process() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("pid");
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "echo $$ > \"$1\"; exec /bin/sleep 60", "fixture"]).arg(&pid_path);
        let error = command_status(command, Duration::from_secs(2), "fixture").await.unwrap_err();
        assert!(error.to_string().contains("超时"));
        let pid = std::fs::read_to_string(pid_path).unwrap();
        let alive = std::process::Command::new("/bin/kill").args(["-0", pid.trim()])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap().success();
        assert!(!alive, "owned child remains alive after timeout");
        let mut failed = Command::new("/bin/sh");
        failed.args(["-c", "exit 7"]);
        assert_eq!(command_status(failed, Duration::from_secs(2), "fixture").await.unwrap().code(), Some(7));
    }

    #[test]
    fn start_args_omit_rosetta_after_user_skips() {
        let with = start_args("vpnmgr", true);
        let without = start_args("vpnmgr", false);
        assert!(with.contains(&"--vz-rosetta".to_string()));
        assert!(!without.contains(&"--vz-rosetta".to_string()));
        assert!(without.windows(2).any(|w| w == ["--vm-type", "vz"]));
        assert!(without.contains(&"--activate=false".to_string()));
        for args in [with, without, start_args("vpnmgr-dev", false)] {
            assert!(args.windows(2).any(|w| w == ["--port-forwarder", "none"]));
        }
    }

    #[tokio::test]
    #[ignore] // needs colima installed
    async fn status_of_absent_profile_is_stopped() {
        assert_eq!(status("definitely-no-such-profile").await.unwrap(), VmStatus::Stopped);
    }
}
