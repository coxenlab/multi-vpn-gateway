//! Persisted SSH ownership survives core termination; never kill by port or name alone.
use std::path::{Path, PathBuf};
use std::time::Duration;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

#[derive(Serialize, Deserialize)]
struct Record {
    pid: u32,
    started: String,
    args: Vec<String>,
    ports: Vec<u16>,
}

pub fn novnc_path(data: &Path, cid: &str) -> PathBuf {
    let name: String = cid.as_bytes().iter().map(|b| format!("{b:02x}")).collect();
    data.join(format!("novnc-forward-{name}.pid"))
}

async fn output(program: &str, args: &[String]) -> Result<std::process::Output> {
    let mut command = Command::new(program);
    command.args(args).env("LC_ALL", "C").kill_on_drop(true);
    tokio::time::timeout(Duration::from_secs(2), command.output()).await
        .map_err(|_| anyhow!("读取转发进程信息超时"))?.map_err(Into::into)
}

async fn process(pid: u32, column: &str) -> Result<Option<String>> {
    let result = output("/bin/ps", &["-ww".into(), "-p".into(), pid.to_string(), "-o".into(), column.into()]).await?;
    if !result.status.success() { return Ok(None); }
    let text = String::from_utf8(result.stdout)?.trim().to_string();
    Ok((!text.is_empty()).then_some(text))
}

pub async fn record(path: &Path, pid: u32, args: &[String], ports: &[u16]) -> Result<()> {
    let started = process(pid, "lstart=").await?.ok_or_else(|| anyhow!("转发进程已退出，无法记录归属"))?;
    let record = Record { pid, started, args: args.to_vec(), ports: ports.to_vec() };
    crate::manager::atomic_write_0600(path, &serde_json::to_vec(&record)?)
}

fn scope_matches(record: &Record, profile: &str, ports: Option<&[u16]>) -> bool {
    if record.pid <= 1 || record.started.is_empty() || record.ports.is_empty()
        || record.ports.contains(&0) || ports.is_some_and(|ports| ports != record.ports) { return false; }
    let config = crate::vm::ssh_config_path(profile).display().to_string();
    let mut fwds = Vec::new();
    for pair in record.args.windows(2).filter(|pair| pair[0] == "-L") {
        let Some(value) = pair[1].strip_prefix("127.0.0.1:") else { return false; };
        let Some((port, guest)) = value.split_once(':') else { return false; };
        let Ok(host_port) = port.parse::<u16>() else { return false; };
        fwds.push(crate::tunnel::Fwd { host_port, guest: guest.into() });
    }
    fwds.iter().map(|f| f.host_port).collect::<Vec<_>>() == record.ports
        && crate::tunnel::forward_args(&config, profile, &fwds) == record.args
}

fn identity_matches(record: &Record, started: &str, command: &str) -> bool {
    record.started == started && ["ssh", "/usr/bin/ssh"].iter()
        .any(|binary| command == format!("{binary} {}", record.args.join(" ")))
}

/// Only recorded, same-instance SSH listeners can be reclaimed. A mismatch is left alone.
pub async fn reclaim(path: &Path, profile: &str, ports: Option<&[u16]>) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(12), reclaim_inner(path, profile, ports)).await
        .map_err(|_| anyhow!("残留转发回收超时，保留归属记录"))?
}

async fn reclaim_inner(path: &Path, profile: &str, ports: Option<&[u16]>) -> Result<()> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let record: Record = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(scope_matches(&record, profile, ports), "转发归属记录与本实例不匹配，未回收进程");
    let Some(started) = process(record.pid, "lstart=").await? else {
        std::fs::remove_file(path)?;
        return Ok(());
    };
    let command = process(record.pid, "command=").await?.unwrap_or_default();
    anyhow::ensure!(identity_matches(&record, &started, &command), "转发进程身份已变化，未回收进程");
    for port in &record.ports {
        let result = output("/usr/sbin/lsof", &["-nP".into(), "-a".into(), "-p".into(), record.pid.to_string(),
            format!("-iTCP:{port}"), "-sTCP:LISTEN".into(), "-Fp".into()]).await?;
        anyhow::ensure!(result.status.success() && String::from_utf8(result.stdout)?.lines()
            .any(|line| line == format!("p{}", record.pid)), "记录的转发端口并非该进程监听，未回收进程");
    }
    // Recheck identity immediately before the signal, after potentially slow socket inspection.
    let started = process(record.pid, "lstart=").await?.unwrap_or_default();
    let command = process(record.pid, "command=").await?.unwrap_or_default();
    anyhow::ensure!(identity_matches(&record, &started, &command), "转发进程身份已变化，未回收进程");
    let result = output("/bin/kill", &["-TERM".into(), record.pid.to_string()]).await?;
    anyhow::ensure!(result.status.success(), "残留转发停止命令未确认");
    for _ in 0..20 {
        if process(record.pid, "lstart=").await?.as_deref() != Some(&record.started) {
            std::fs::remove_file(path)?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(anyhow!("残留转发尚未退出，保留记录等待重试"))
}

pub async fn reclaim_startup(state: &crate::AppState) {
    let Ok(entries) = std::fs::read_dir(&state.cfg.data_dir) else { return; };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let novnc = name.strip_prefix("novnc-forward-").and_then(|value| value.strip_suffix(".pid"))
            .is_some_and(|value| !value.is_empty() && value.bytes().all(|b| b.is_ascii_hexdigit()));
        if name != "forward.pid" && !novnc { continue; }
        if name == "forward.pid" && state.tunnel.lock().await.is_some() { continue; }
        if let Err(error) = reclaim(&entry.path(), &state.cfg.vm_profile, None).await {
            crate::ev!(warn, "tunnel", "orphan_recovery_pending", "残留转发归属或停止尚未确认", {"record":name,"error":error.to_string()});
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ownership_requires_exact_instance_ports_start_time_and_command() {
        let profile = "tongdao-verify-owned";
        let config = crate::vm::ssh_config_path(profile).display().to_string();
        let args = crate::tunnel::forward_args(&config, profile,
            &[crate::tunnel::Fwd { host_port: 14567, guest: "172.18.0.2:7890".into() }]);
        let mut record = Record { pid: 12345, started: "Sat Oct 3 12:00:00 2026".into(), args, ports: vec![14567] };
        assert!(scope_matches(&record, profile, Some(&[14567])));
        assert!(!scope_matches(&record, "vpnmgr", Some(&[14567])));
        assert!(!scope_matches(&record, profile, Some(&[14568])));
        let command = format!("ssh {}", record.args.join(" "));
        assert!(identity_matches(&record, &record.started, &command));
        assert!(!identity_matches(&record, "different start", &command));
        assert!(!identity_matches(&record, &record.started, &format!("{command} extra")));
        assert!(!identity_matches(&record, &record.started, &command.replacen("ssh", "other", 1)));
        record.args.push("unrecorded-option".into());
        assert!(!scope_matches(&record, profile, None));
        assert_eq!(novnc_path(Path::new("/tmp/owned"), "../x").parent(), Some(Path::new("/tmp/owned")));
    }

    #[tokio::test]
    async fn unrelated_live_pid_is_never_signalled_or_forgotten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("forward.pid");
        let profile = "tongdao-verify-owned";
        let args = crate::tunnel::forward_args(&crate::vm::ssh_config_path(profile).display().to_string(), profile,
            &[crate::tunnel::Fwd { host_port: 14567, guest: "172.18.0.2:7890".into() }]);
        let mut child = Command::new("/bin/sleep").arg("60").kill_on_drop(true).spawn().unwrap();
        record(&path, child.id().unwrap(), &args, &[14567]).await.unwrap();
        assert!(reclaim(&path, profile, Some(&[14567])).await.is_err());
        assert!(child.try_wait().unwrap().is_none());
        assert!(path.exists());
        child.kill().await.unwrap();
        reclaim(&path, profile, Some(&[14567])).await.unwrap();
        assert!(!path.exists(), "a reaped process leaves no stale ownership record");
    }
}
