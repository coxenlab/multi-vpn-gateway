//! 低频、只读的 usernet 拨号诊断。只查询当前 profile 配置关联的 PID，绝不扫描全机连接。
//! SYN_SENT 是候选证据；共享 NAT 下无法据此反查 VPN 容器，不参与自动重启或登录判定。
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{anyhow, ensure, Result};
use serde::Serialize;
use tokio::io::AsyncReadExt;

use crate::runtime_info::RuntimeInfo;

#[derive(Debug, Clone, Serialize)]
pub struct Destination {
    pub address: String,
    pub count: usize,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    pub checked_at: String,
    pub status: String,
    pub reason: Option<String>,
    pub network: Option<String>,
    pub pid: Option<u32>,
    pub multiple_interfaces: bool,
    pub runtime: Option<RuntimeInfo>,
    pub runtime_error: Option<String>,
    pub syn_sent: Option<usize>,
    pub at_default_limit: Option<bool>,
    pub destinations: Vec<Destination>,
    pub destinations_omitted: usize,
    pub duration_ms: u64,
    pub attribution: String,
}

#[derive(Default)]
pub struct Sampler {
    last_attempt: Option<Instant>,
    runtime: Option<(u32, PathBuf, u64, std::time::SystemTime, RuntimeInfo)>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn network_for(config: &str, networks: &str) -> Result<(String, bool)> {
    let vm: serde_yaml::Value = serde_yaml::from_str(config).map_err(|_| anyhow!("invalid_vm_config"))?;
    let definitions: serde_yaml::Value = serde_yaml::from_str(networks).map_err(|_| anyhow!("invalid_network_config"))?;
    let interfaces = vm["networks"].as_sequence().ok_or_else(|| anyhow!("no_named_network"))?;
    let names: Vec<_> = interfaces.iter().filter_map(|i| i["lima"].as_str())
        .filter(|name| definitions["networks"][*name]["mode"].as_str() == Some("user-v2"))
        .collect();
    ensure!(names.len() == 1 && valid_name(names[0]), "usernet_mapping_ambiguous");
    Ok((names[0].into(), interfaces.len() > 1))
}

fn process_matches(command: &str, pid_file: &Path) -> bool {
    let Some((exe, args)) = command.trim().split_once(" usernet ") else { return false; };
    if Path::new(exe).file_name().and_then(|s| s.to_str()) != Some("limactl") { return false; }
    let expected = format!("-p {}", pid_file.display());
    args.strip_prefix(&expected).is_some_and(|tail| tail.is_empty() || tail.starts_with(' '))
        || args.contains(&format!(" {expected} "))
        || args.ends_with(&format!(" {expected}"))
}

async fn small_file(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    tokio::fs::File::open(path).await.map_err(|_| anyhow!("state_file_unavailable"))?
        .take(256 * 1024 + 1).read_to_end(&mut bytes).await?;
    ensure!(bytes.len() <= 256 * 1024, "state_file_too_large");
    String::from_utf8(bytes).map_err(|_| anyhow!("invalid_state_file"))
}

async fn command(program: &str, args: &[&str]) -> Result<(bool, String)> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut child = cmd.spawn().map_err(|_| anyhow!("diagnostic_command_unavailable"))?;
        let mut data = Vec::new();
        let mut errors = Vec::new();
        let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("missing_stdout"))?.take(64 * 1024 + 1);
        let mut stderr = child.stderr.take().ok_or_else(|| anyhow!("missing_stderr"))?.take(1025);
        tokio::try_join!(
            async {
                stdout.read_to_end(&mut data).await?;
                ensure!(data.len() <= 64 * 1024, "diagnostic_output_limit");
                Ok::<_, anyhow::Error>(())
            },
            async {
                stderr.read_to_end(&mut errors).await?;
                ensure!(errors.is_empty(), "diagnostic_command_reported_error");
                Ok::<_, anyhow::Error>(())
            }
        )?;
        let status = child.wait().await?;
        ensure!(status.success() || status.code() == Some(1), "diagnostic_command_failed");
        Ok((status.success(), String::from_utf8_lossy(&data).into_owned()))
    }).await.map_err(|_| anyhow!("diagnostic_timeout"))?
}

fn connections(text: &str) -> (usize, Vec<Destination>, usize) {
    let mut counts = BTreeMap::<String, usize>::new();
    let (mut address, mut syn_sent) = (None, false);
    let mut flush = |address: &mut Option<String>, syn_sent: &mut bool| {
        if *syn_sent {
            *counts.entry(address.take().unwrap_or_else(|| "目标待定位".into())).or_default() += 1;
        }
        *address = None; *syn_sent = false;
    };
    for line in text.lines() {
        if line.starts_with('f') { flush(&mut address, &mut syn_sent); }
        else if let Some(name) = line.strip_prefix('n') {
            address = name.split_once("->").and_then(|(_, remote)| remote.parse::<std::net::SocketAddr>().ok())
                .map(|a| a.to_string());
        } else if line == "TST=SYN_SENT" { syn_sent = true; }
    }
    flush(&mut address, &mut syn_sent);
    let total = counts.values().sum();
    let mut destinations: Vec<_> = counts.into_iter().map(|(address, count)| Destination { address, count }).collect();
    destinations.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.address.cmp(&b.address)));
    let omitted = destinations.len().saturating_sub(16);
    destinations.truncate(16);
    (total, destinations, omitted)
}

impl Sampler {
    /// 节流同时覆盖睡醒/连续故障；API 只读缓存，不触发采样。
    pub async fn sample_if_due(&mut self, profile: &str) -> Option<Snapshot> {
        if self.last_attempt.is_some_and(|t| t.elapsed() < Duration::from_secs(60)) { return None; }
        self.last_attempt = Some(Instant::now());
        let started = Instant::now();
        let mut result = Snapshot {
            checked_at: chrono::Utc::now().to_rfc3339(), status: "unavailable".into(),
            attribution: "共享网络进程；来源待定位，不能据此确定 VPN 通道或默认出口".into(),
            ..Default::default()
        };
        let home = std::env::var("HOME").unwrap_or_default();
        match tokio::time::timeout(Duration::from_secs(10), self.collect(Path::new(&home), profile, &mut result)).await {
            Ok(Ok(())) => result.status = "observed".into(),
            Ok(Err(e)) => result.reason = Some(e.to_string()),
            Err(_) => result.reason = Some("sample_timeout".into()),
        }
        result.duration_ms = started.elapsed().as_millis() as u64;
        Some(result)
    }

    async fn collect(&mut self, home: &Path, profile: &str, result: &mut Snapshot) -> Result<()> {
        ensure!(valid_name(profile), "invalid_profile");
        let lima = home.join(".colima/_lima");
        let cfg = small_file(&lima.join(format!("colima-{profile}/lima.yaml"))).await?;
        let networks = small_file(&lima.join("_config/networks.yaml")).await?;
        let (network, multiple_interfaces) = network_for(&cfg, &networks)?;
        let pid_file = lima.join("_networks").join(&network).join(format!("usernet_{network}.pid"));
        result.network = Some(network); result.multiple_interfaces = multiple_interfaces;
        let pid = small_file(&pid_file).await?.trim().parse::<u32>().map_err(|_| anyhow!("invalid_usernet_pid"))?;
        ensure!(pid > 1, "invalid_usernet_pid");
        let pid_s = pid.to_string();
        let (_, args) = command("/bin/ps", &["-p", &pid_s, "-o", "command="]).await?;
        ensure!(process_matches(&args, &pid_file), "usernet_pid_mismatch");
        result.pid = Some(pid);
        match self.runtime_info(pid).await {
            Ok(info) => result.runtime = Some(info),
            Err(e) => result.runtime_error = Some(e.to_string()),
        }
        let (ok, sockets) = command("/usr/sbin/lsof", &["-nP", "-a", "-p", &pid_s, "-iTCP", "-sTCP:SYN_SENT", "-FfnT"]).await?;
        // lsof 没有匹配项时 exit 1、stdout 为空；其他失败不伪装成 0。
        ensure!(ok || sockets.is_empty(), "socket_query_failed");
        let (_, after) = command("/bin/ps", &["-p", &pid_s, "-o", "command="]).await?;
        ensure!(process_matches(&after, &pid_file), "usernet_process_changed");
        let (total, destinations, omitted) = connections(&sockets);
        result.syn_sent = Some(total); result.destinations = destinations; result.destinations_omitted = omitted;
        result.at_default_limit = result.runtime.as_ref().and_then(|r| r.default_dial_limit).map(|limit| total >= limit);
        Ok(())
    }

    async fn runtime_info(&mut self, pid: u32) -> Result<RuntimeInfo> {
        use std::os::unix::fs::MetadataExt;
        let pid_s = pid.to_string();
        let (_, exe) = command("/bin/ps", &["-p", &pid_s, "-o", "comm="]).await?;
        let path = tokio::fs::canonicalize(exe.trim()).await.map_err(|_| anyhow!("runtime_path_unavailable"))?;
        let meta = tokio::fs::metadata(&path).await?;
        let modified = meta.modified()?;
        if let Some((old_pid, old_path, inode, mtime, info)) = &self.runtime {
            if *old_pid == pid && *old_path == path && *inode == meta.ino() && *mtime == modified { return Ok(info.clone()); }
        }
        let (_, mapped) = command("/usr/sbin/lsof", &["-a", "-p", &pid_s, "-d", "txt", "-Ffin"]).await?;
        let mut inode = None;
        let matches = mapped.lines().any(|line| {
            if line.starts_with('f') { inode = None; }
            if let Some(value) = line.strip_prefix('i') { inode = value.parse::<u64>().ok(); }
            line.strip_prefix('n').is_some_and(|name| Path::new(name) == path && inode == Some(meta.ino()))
        });
        ensure!(matches, "runtime_file_changed_or_unverified");
        let read_path = path.clone();
        let info = tokio::task::spawn_blocking(move || crate::runtime_info::read(&read_path)).await??;
        self.runtime = Some((pid, path, meta.ino(), modified, info.clone()));
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_profile_network_and_rejects_ambiguous_or_unrelated_pid() {
        let definitions = "networks:\n  user-v2:\n    mode: user-v2\n";
        assert_eq!(network_for("networks:\n- lima: user-v2\n", definitions).unwrap(), ("user-v2".into(), false));
        assert!(network_for("networks:\n- vzNAT: true\n", definitions).is_err());
        assert!(network_for("networks:\n- lima: user-v2\n- lima: user-v2\n", definitions).is_err());
        let pid_file = Path::new("/test space/usernet.pid");
        assert!(process_matches("/app space/limactl usernet -p /test space/usernet.pid --listen /x", pid_file));
        assert!(!process_matches("/bin/limactl hostagent -p /test space/usernet.pid", pid_file));
        assert!(!process_matches("/bin/limactl usernet -p /other/usernet.pid", pid_file));
    }

    #[test]
    fn counts_only_syn_sent_and_groups_remote_ipv4_and_ipv6() {
        let data = "p12\nf10\nn127.0.0.1:3000->10.0.0.1:443\nTST=SYN_SENT\nf11\nn127.0.0.1:3001->10.0.0.1:443\nTST=SYN_SENT\nf12\nn[::1]:3000->[2001:db8::1]:443\nTST=SYN_SENT\nf13\nn127.0.0.1:3002->10.0.0.2:443\nTST=ESTABLISHED\n";
        let (count, targets, omitted) = connections(data);
        assert_eq!(count, 3); assert_eq!(targets[0].address, "10.0.0.1:443");
        assert_eq!(targets[0].count, 2); assert_eq!(targets[1].address, "[2001:db8::1]:443"); assert_eq!(omitted, 0);
        assert_eq!(connections("").0, 0);
    }

    #[tokio::test]
    async fn output_limit_and_command_error_are_not_reported_as_zero_connections() {
        assert!(command("/usr/bin/yes", &[]).await.unwrap_err().to_string().contains("output_limit"));
        assert!(command("/bin/sh", &["-c", "printf denied >&2; exit 1"]).await.is_err());
        let (ok, text) = command("/bin/sh", &["-c", "exit 1"]).await.unwrap();
        assert!(!ok && text.is_empty());
        let mut sampler = Sampler { last_attempt: Some(Instant::now()), ..Default::default() };
        assert!(sampler.sample_if_due("not-a-real-profile").await.is_none());
    }
}
