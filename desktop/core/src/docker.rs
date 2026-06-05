use anyhow::{anyhow, Result};
use bollard::Docker;
use chrono::{DateTime, Utc};

/// 解析 VM 的 docker.sock(对照 spike::spike_socket):DOCKER_HOST 去 unix:// 前缀,否则 colima 默认 profile。
pub fn docker_socket() -> String {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        return h.trim_start_matches("unix://").to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.colima/default/docker.sock")
}

/// 连接 VM 内 Docker Engine(spike 已证)。等价今天 docker.from_env(),只是 socket 路径不同。
pub async fn connect() -> Result<Docker> {
    let sock = docker_socket();
    let docker = Docker::connect_with_socket(&sock, 120, bollard::API_DEFAULT_VERSION)
        .map_err(|e| anyhow!("connect {sock}: {e}"))?;
    docker.ping().await.map_err(|e| anyhow!("ping {sock}: {e}"))?;
    Ok(docker)
}

/// 对照 manager.uptime:容器 vpn-{cid} 运行时长的人类可读串;停止/缺失/任何错误 → None。
pub async fn uptime(docker: Option<&Docker>, cid: &str) -> Option<String> {
    let docker = docker?;
    let name = format!("vpn-{cid}");
    let info = docker.inspect_container(&name, None).await.ok()?;
    let state = info.state?;
    if !state.running.unwrap_or(false) {
        return None;
    }
    let started = state.started_at?;
    let started: DateTime<Utc> = DateTime::parse_from_rfc3339(&started).ok()?.with_timezone(&Utc);
    let secs = (Utc::now() - started).num_seconds();
    if secs < 0 {
        return None;
    }
    Some(fmt_uptime(secs))
}

/// 对照 manager.uptime 的格式化分支(中文单位)。
pub fn fmt_uptime(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}秒")
    } else if secs < 3600 {
        format!("{}分钟", secs / 60)
    } else if secs < 86400 {
        format!("{}小时{}分", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}天{}小时", secs / 86400, (secs % 86400) / 3600)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_uptime_units() {
        assert_eq!(fmt_uptime(5), "5秒");
        assert_eq!(fmt_uptime(90), "1分钟");
        assert_eq!(fmt_uptime(3661), "1小时1分");
        assert_eq!(fmt_uptime(90061), "1天1小时");
    }

    #[tokio::test]
    async fn uptime_none_without_docker() {
        assert_eq!(uptime(None, "deadbeef").await, None);
    }

    #[tokio::test]
    #[ignore]
    async fn uptime_none_for_missing_container() {
        let d = connect().await.unwrap();
        assert_eq!(uptime(Some(&d), "no-such-cid").await, None);
    }
}
