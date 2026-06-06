use anyhow::{anyhow, Result};
use bollard::container::{CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions, UploadToContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use bollard::Docker;
use chrono::{DateTime, Utc};
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;
use crate::adapters::ContainerPlan;

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

pub async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
    let opts = CreateImageOptions { from_image: image, ..Default::default() };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(|e| anyhow!("pull {image}: {e}"))?;
    }
    Ok(())
}

pub async fn rm_force(docker: &Docker, name: &str) -> Result<()> {
    let _ = docker
        .remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() }))
        .await;
    Ok(())
}

pub async fn stop(docker: &Docker, name: &str) -> Result<()> {
    let _ = docker.stop_container(name, None::<StopContainerOptions>).await;
    Ok(())
}

pub async fn start(docker: &Docker, name: &str) -> Result<()> {
    let _ = docker.start_container(name, None::<StartContainerOptions<String>>).await;
    Ok(())
}

/// 由 ContainerPlan 创建并启动;dns 非空注入 HostConfig.dns(oss)。幂等(先力删同名)。
pub async fn create_from_plan(docker: &Docker, plan: &ContainerPlan, dns: Option<Vec<String>>) -> Result<String> {
    let _ = rm_force(docker, &plan.name).await;
    let mut config = plan.config.clone();
    if let Some(servers) = dns {
        let hc = config.host_config.get_or_insert_with(Default::default);
        hc.dns = Some(servers);
    }
    let res = docker
        .create_container(Some(CreateContainerOptions { name: plan.name.clone(), platform: None }), config)
        .await
        .map_err(|e| anyhow!("create {}: {e}", plan.name))?;
    docker
        .start_container(&plan.name, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow!("start {}: {e}", plan.name))?;
    Ok(res.id)
}

pub async fn novnc_port(docker: &Docker, cid: &str) -> Option<i64> {
    let name = format!("vpn-{cid}");
    let info = docker.inspect_container(&name, None).await.ok()?;
    let ports = info.network_settings?.ports?;
    let binding = ports.get("8080/tcp")?.as_ref()?.first()?;
    binding.host_port.as_ref()?.parse::<i64>().ok()
}

pub async fn container_ip_on_net(docker: &Docker, name: &str, net: &str) -> Option<String> {
    let info = docker.inspect_container(name, None).await.ok()?;
    let nets = info.network_settings?.networks?;
    let ip = nets.get(net)?.ip_address.clone()?;
    if ip.is_empty() { None } else { Some(ip) }
}

pub async fn exec_capture(docker: &Docker, name: &str, cmd: Vec<&str>) -> Result<String> {
    let exec = docker
        .create_exec(name, CreateExecOptions {
            cmd: Some(cmd.into_iter().map(String::from).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        })
        .await?;
    let mut out = String::new();
    if let StartExecResults::Attached { mut output, .. } =
        docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, ..Default::default() })).await?
    {
        while let Some(item) = output.next().await {
            out.push_str(&String::from_utf8_lossy(item?.into_bytes().as_ref()));
        }
    }
    Ok(out)
}

/// 命门 #5:exec attach_stdin,写 data 后 shutdown 发 EOF。
pub async fn exec_inject_stdin(docker: &Docker, name: &str, cmd: Vec<&str>, data: &[u8]) -> Result<String> {
    let exec = docker
        .create_exec(name, CreateExecOptions {
            cmd: Some(cmd.into_iter().map(String::from).collect()),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        })
        .await?;
    let mut out = String::new();
    match docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, ..Default::default() })).await? {
        StartExecResults::Attached { mut output, mut input } => {
            input.write_all(data).await.map_err(|e| anyhow!("write stdin: {e}"))?;
            input.shutdown().await.map_err(|e| anyhow!("shutdown stdin (EOF): {e}"))?;
            while let Some(item) = output.next().await {
                out.push_str(&String::from_utf8_lossy(item?.into_bytes().as_ref()));
            }
        }
        StartExecResults::Detached => return Err(anyhow!("exec detached unexpectedly")),
    }
    Ok(out)
}

/// 命门 #5(byo):内存 tar(mode 0600)→ upload_to_container。
pub async fn put_file(docker: &Docker, name: &str, dst_dir: &str, filename: &str, data: &[u8]) -> Result<()> {
    let mut ar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o600);
    header.set_cksum();
    ar.append_data(&mut header, filename, data)?;
    let tar_bytes = ar.into_inner()?;
    docker
        .upload_to_container(name, Some(UploadToContainerOptions { path: dst_dir, ..Default::default() }), bytes::Bytes::from(tar_bytes))
        .await
        .map_err(|e| anyhow!("upload_to_container {name}:{dst_dir}: {e}"))?;
    Ok(())
}

pub async fn raw_logs(docker: &Docker, name: &str, tail: i64) -> Result<Vec<String>> {
    let mut stream = docker.logs(name, Some(LogsOptions::<String> {
        stdout: true, stderr: true, tail: tail.to_string(), ..Default::default()
    }));
    let mut buf = String::new();
    while let Some(item) = stream.next().await {
        buf.push_str(&String::from_utf8_lossy(item?.into_bytes().as_ref()));
    }
    Ok(buf.lines().map(String::from).collect())
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

    #[tokio::test]
    #[ignore] // needs colima
    async fn exec_and_put_roundtrip_on_alpine() {
        let d = connect().await.unwrap();
        ensure_image(&d, "alpine:latest").await.unwrap();
        let name = "vpncore-it-exec";
        let _ = rm_force(&d, name).await;
        let plan = crate::adapters::ContainerPlan {
            name: name.into(),
            config: bollard::container::Config {
                image: Some("alpine:latest".into()),
                cmd: Some(vec!["sleep".into(), "60".into()]),
                ..Default::default()
            },
        };
        create_from_plan(&d, &plan, None).await.unwrap();
        exec_inject_stdin(&d, name, vec!["sh", "-c", "cat > /tmp/secret"], b"s3cr3t").await.unwrap();
        let out = exec_capture(&d, name, vec!["cat", "/tmp/secret"]).await.unwrap();
        assert_eq!(out.trim(), "s3cr3t");
        put_file(&d, name, "/tmp", "hello.txt", b"hi").await.unwrap();
        let out = exec_capture(&d, name, vec!["cat", "/tmp/hello.txt"]).await.unwrap();
        assert_eq!(out.trim(), "hi");
        rm_force(&d, name).await.unwrap();
    }
}
