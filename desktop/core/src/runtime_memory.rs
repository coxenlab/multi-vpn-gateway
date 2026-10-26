//! 来宾本次启动的 OOM 计数与实际内存。探测失败不推断内存不足，也不启动 VM。
use serde::Serialize;
use std::{collections::HashMap, sync::LazyLock, time::{Duration, Instant}};
use tokio::{process::Command, sync::Mutex};

#[derive(Clone, Debug, Serialize)]
pub struct MemoryPressure { pub oom_kills: u64, pub memory_gib: u64 }
static CACHE: LazyLock<Mutex<HashMap<std::path::PathBuf, (Instant, MemoryPressure)>>> = LazyLock::new(Default::default);

pub fn counter(text: &str, name: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        (parts.next()? == name).then(|| parts.next()?.parse().ok()).flatten()
    })
}

pub fn parse(text: &str) -> Option<MemoryPressure> {
    let kib = counter(text, "MemTotal:")?;
    Some(MemoryPressure { oom_kills: counter(text, "oom_kill")?, memory_gib: (kib + 524288) / 1048576 })
}

pub async fn invalidate(profile: &str) { CACHE.lock().await.remove(&crate::vm::ssh_config_path(profile)); }

pub async fn sample(state: &crate::AppState) -> Option<MemoryPressure> {
    if !state.cfg.managed_vm || !crate::runtime::serving(state)
        || !std::ptr::eq(state.cfg.identity(), &crate::identity::TONGDAO) { return None; }
    let path = crate::vm::ssh_config_path(&state.cfg.vm_profile);
    let mut cache = CACHE.lock().await;
    if let Some((_, value)) = cache.get(&path).filter(|(at, _)| at.elapsed() < Duration::from_secs(10)) {
        return Some(value.clone());
    }
    let mut args = crate::vm::probe_args(path.to_str()?, &state.cfg.vm_profile);
    *args.last_mut()? = "cat /proc/meminfo /proc/vmstat".into();
    let output = tokio::time::timeout(Duration::from_secs(3), Command::new("ssh").args(args).kill_on_drop(true).output()).await.ok()?.ok()?;
    if !output.status.success() { return None; }
    let value = parse(&String::from_utf8_lossy(&output.stdout))?;
    cache.insert(path, (Instant::now(), value.clone()));
    Some(value)
}

pub async fn host_memory_gib() -> Option<u64> {
    let output = tokio::time::timeout(Duration::from_secs(1), Command::new("/usr/sbin/sysctl").args(["-n", "hw.memsize"]).kill_on_drop(true).output()).await.ok()?.ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse::<u64>().ok().map(|bytes| bytes / 1073741824)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn actual_memory_and_boot_oom_only() {
        let value = parse("MemTotal: 1966080 kB\noom_kill 3\n").unwrap();
        assert_eq!(value.memory_gib, 2); assert_eq!(value.oom_kills, 3);
        assert!(parse("MemTotal: 1966080 kB").is_none());
        assert_eq!(counter("oom 9\noom_kill 2\noom_group_kill 1", "oom_kill"), Some(2));
    }
}
