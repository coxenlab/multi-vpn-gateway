use anyhow::{anyhow, Result};
use bollard::container::{CreateContainerOptions, LogsOptions, RemoveContainerOptions, StartContainerOptions, StopContainerOptions, UploadToContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::{CreateImageOptions, RemoveImageOptions, TagImageOptions};
use bollard::network::{CreateNetworkOptions, InspectNetworkOptions};
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

/// fire-and-forget exec(对照 Python `c.exec_run(..., detach=True)`)。
/// 用于不会自行退出的前台进程(openfortivpn --persistent)与 ensure_novnc_bridge 的等待脚本:
/// exec_capture 会阻塞在 output 流上直到进程退出 —— 这类进程永不退出会挂死,故必须 detach。
pub async fn exec_detach(docker: &Docker, name: &str, cmd: Vec<&str>) -> Result<()> {
    let exec = docker
        .create_exec(name, CreateExecOptions {
            cmd: Some(cmd.into_iter().map(String::from).collect()),
            ..Default::default()
        })
        .await?;
    docker
        .start_exec(&exec.id, Some(StartExecOptions { detach: true, ..Default::default() }))
        .await?;
    Ok(())
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

/// byo 安装器上传:内存 tar → upload_to_container。对照 Python put_file。
/// mode 0o755:安装器须可执行(用户在 noVNC 桌面里直接跑)。命门 #5 由「不进 argv」满足,与文件 mode 无关。
pub async fn put_file(docker: &Docker, name: &str, dst_dir: &str, filename: &str, data: &[u8]) -> Result<()> {
    let mut ar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o755);
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

// ── Phase 6:infra 低层助手(对照 preflight.py / dockerhub.py 的 docker 调用) ──

/// 404 判定(对照 docker.errors.NotFound / ImageNotFound)。
pub fn is_not_found(e: &bollard::errors::Error) -> bool {
    matches!(e, bollard::errors::Error::DockerResponseServerError { status_code: 404, .. })
}

/// daemon 可达(对照 dc.ping())。
pub async fn ping(docker: &Docker) -> Result<()> {
    docker.ping().await.map(|_| ()).map_err(|e| anyhow!("{e}"))
}

/// Docker 版本串(对照 dc.version()["Version"])。
pub async fn docker_version(docker: &Docker) -> Result<String> {
    let v = docker.version().await?;
    Ok(v.version.unwrap_or_else(|| "?".into()))
}

/// 镜像层占用 GB(对照 dc.df()["LayersSize"]/1024^3)。
pub async fn layers_size_gb(docker: &Docker) -> Result<f64> {
    let df = docker.df().await?;
    Ok(df.layers_size.unwrap_or(0) as f64 / 1024f64.powi(3))
}

/// 本机是否已有该镜像:Some(true)/Some(false)=404/None=其它错误(对照 _image_present)。
pub async fn image_present(docker: &Docker, image: &str) -> Option<bool> {
    match docker.inspect_image(image).await {
        Ok(_) => Some(true),
        Err(e) if is_not_found(&e) => Some(false),
        Err(_) => None,
    }
}

/// 本地镜像架构(对照 img.attrs["Architecture"]);缺失/错误 → None。
pub async fn image_arch(docker: &Docker, image: &str) -> Option<String> {
    docker.inspect_image(image).await.ok().and_then(|i| i.architecture).filter(|s| !s.is_empty())
}

/// docker 网络是否存在。
pub async fn network_exists(docker: &Docker, name: &str) -> bool {
    docker.inspect_network(name, None::<InspectNetworkOptions<String>>).await.is_ok()
}

/// 创建 bridge 网络(幂等:已存在则跳过)。
pub async fn create_bridge_network(docker: &Docker, name: &str) -> Result<()> {
    if network_exists(docker, name).await {
        return Ok(());
    }
    docker
        .create_network(CreateNetworkOptions { name: name.to_string(), driver: "bridge".to_string(), ..Default::default() })
        .await
        .map(|_| ())
        .map_err(|e| anyhow!("create_network {name}: {e}"))
}

/// pull_retag 的结果(对照 _pull_worker:成功 vs arch 不匹配弃用 vs 真失败 Err)。
pub enum PullOutcome {
    Tagged(String),       // 成功 retag,携带实际 arch
    ArchMismatch(String), // 拉到非目标 arch,已弃用(删除),携带实际 arch
}

/// 从 mirror 拉取 repo:tag(指定 platform)→ 校验 arch → retag 回原名 → 删 mirror 标。
/// 对照 _pull_worker 单镜像源那一轮:成功 Tagged、arch 不匹配 ArchMismatch、真失败 Err。
pub async fn pull_retag(docker: &Docker, mirror: &str, repo: &str, tag: &str, host_arch: &str) -> Result<PullOutcome> {
    let src = format!("{mirror}/{repo}");
    let platform = format!("linux/{host_arch}");
    let opts = CreateImageOptions { from_image: src.clone(), tag: tag.to_string(), platform, ..Default::default() };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(|e| anyhow!("pull {src}:{tag}: {e}"))?;
    }
    let full_src = format!("{src}:{tag}");
    let arch = image_arch(docker, &full_src).await.unwrap_or_default();
    if !arch.is_empty() && arch != host_arch {
        let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
        return Ok(PullOutcome::ArchMismatch(arch));
    }
    docker
        .tag_image(&full_src, Some(TagImageOptions { repo: repo.to_string(), tag: tag.to_string() }))
        .await
        .map_err(|e| anyhow!("tag {repo}:{tag}: {e}"))?;
    let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
    Ok(PullOutcome::Tagged(if arch.is_empty() { host_arch.to_string() } else { arch }))
}

/// 一次性容器测 /dev/net/tun(对照 check_dev_net_tun)。Ok(true)=exit0;Ok(false)=非0;Err=起不来。
pub async fn run_tun_probe(docker: &Docker, image: &str) -> Result<bool> {
    use bollard::container::{Config, WaitContainerOptions};
    use bollard::models::{DeviceMapping, HostConfig};
    let name = "vpncore-tun-probe";
    let _ = rm_force(docker, name).await;
    let config = Config {
        image: Some(image.to_string()),
        entrypoint: Some(vec!["/bin/sh".into(), "-c".into(), "test -c /dev/net/tun".into()]),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()),
            devices: Some(vec![DeviceMapping {
                path_on_host: Some("/dev/net/tun".into()),
                path_in_container: Some("/dev/net/tun".into()),
                cgroup_permissions: Some("rwm".into()),
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    docker.create_container(Some(CreateContainerOptions { name, platform: None }), config).await?;
    docker.start_container(name, None::<StartContainerOptions<String>>).await?;
    let mut wait = docker.wait_container(name, None::<WaitContainerOptions<String>>);
    let mut code = 0i64;
    while let Some(item) = wait.next().await {
        if let Ok(r) = item {
            code = r.status_code;
        }
    }
    let _ = rm_force(docker, name).await;
    Ok(code == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_classifies_404() {
        let e404 = bollard::errors::Error::DockerResponseServerError { status_code: 404, message: "no such image".into() };
        let e500 = bollard::errors::Error::DockerResponseServerError { status_code: 500, message: "boom".into() };
        assert!(is_not_found(&e404));
        assert!(!is_not_found(&e500));
    }


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
