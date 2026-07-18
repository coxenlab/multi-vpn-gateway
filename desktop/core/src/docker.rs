use std::collections::HashMap;

use anyhow::{anyhow, Result};
use bollard::container::{CreateContainerOptions, LogsOptions, RemoveContainerOptions, RestartContainerOptions, StartContainerOptions, StopContainerOptions, UploadToContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::{CreateImageOptions, ImportImageOptions, RemoveImageOptions, TagImageOptions};
use bollard::models::CreateImageInfo;
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
    connect_at(&docker_socket()).await
}

/// 连接指定 unix sock(备援隧道 sock 用;`connect` 的参数化变体,构造后立即 ping 验证)。
pub async fn connect_at(sock: &str) -> Result<Docker> {
    let docker = Docker::connect_with_socket(sock, 120, bollard::API_DEFAULT_VERSION)
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePullProgress {
    pub detail: String,
    pub percent: Option<u8>,
}

#[derive(Default)]
struct PullProgress {
    layers: HashMap<String, (i64, i64)>,
}

impl PullProgress {
    fn observe(&mut self, info: &CreateImageInfo) -> ImagePullProgress {
        if let (Some(id), Some(progress)) = (&info.id, &info.progress_detail) {
            if let (Some(current), Some(total)) = (progress.current, progress.total) {
                if total > 0 {
                    self.layers.insert(id.clone(), (current.clamp(0, total), total));
                }
            } else if info.status.as_deref().is_some_and(|s| s.contains("complete") || s.contains("Already exists")) {
                if let Some((current, total)) = self.layers.get_mut(id) {
                    *current = *total;
                }
            }
        }
        let (current, total) = self.layers.values().fold((0_i128, 0_i128), |acc, item| {
            (acc.0 + i128::from(item.0), acc.1 + i128::from(item.1))
        });
        let percent = (total > 0).then(|| ((current * 100 / total).clamp(0, 100)) as u8);

        let mut detail = info.status.clone().unwrap_or_else(|| "正在拉取镜像".to_string());
        if let Some(id) = info.id.as_deref().filter(|s| !s.is_empty()) {
            detail.push_str(&format!(" · {id}"));
        }
        if let Some(percent) = percent {
            detail.push_str(&format!(" · {percent}%"));
        } else if let Some(raw) = info.progress.as_deref().filter(|s| !s.is_empty()) {
            detail.push_str(&format!(" · {raw}"));
        }
        ImagePullProgress { detail, percent }
    }
}

fn image_stream_error(info: &CreateImageInfo) -> Option<String> {
    info.error
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(String::from)
        .or_else(|| {
            info.error_detail
                .as_ref()
                .and_then(|e| e.message.as_deref())
                .filter(|s| !s.is_empty())
                .map(String::from)
        })
}

pub async fn ensure_image_with_progress<F>(docker: &Docker, image: &str, mut on_progress: F) -> Result<()>
where
    F: FnMut(ImagePullProgress) + Send,
{
    let opts = CreateImageOptions { from_image: image, ..Default::default() };
    let mut stream = docker.create_image(Some(opts), None, None);
    let mut progress = PullProgress::default();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| anyhow!("pull {image}: {e}"))?;
        if let Some(error) = image_stream_error(&info) {
            return Err(anyhow!("pull {image}: {error}"));
        }
        on_progress(progress.observe(&info));
    }
    Ok(())
}

pub async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
    ensure_image_with_progress(docker, image, |_| {}).await
}

/// docker-load 一个本地镜像 tarball(= `docker load`,POST /images/load)。打包内置镜像首启落进 VM 用。
/// tar 可为 gzip(daemon 自识别)。幂等:镜像已在则跳过返回 false;真载入返回 true。
pub async fn load_image_if_absent(docker: &Docker, image: &str, tar_path: &std::path::Path) -> Result<bool> {
    if image_present(docker, image).await == Some(true) {
        return Ok(false);
    }
    let bytes = tokio::fs::read(tar_path)
        .await
        .map_err(|e| anyhow!("读镜像 tarball {}: {e}", tar_path.display()))?;
    let mut stream = docker.import_image(ImportImageOptions { quiet: true }, bytes.into(), None);
    while let Some(item) = stream.next().await {
        item.map_err(|e| anyhow!("docker load {image}: {e}"))?;
    }
    Ok(true)
}

fn container_action_result(
    action: &str,
    name: &str,
    result: std::result::Result<(), bollard::errors::Error>,
    not_found_ok: bool,
) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(e) if not_found_ok && is_not_found(&e) => Ok(()),
        Err(e) => Err(anyhow!("{action} {name}: {e}")),
    }
}

pub async fn rm_force(docker: &Docker, name: &str) -> Result<()> {
    // 404(容器不存在)= 幂等成功；其余错误带上下文上抛（对照 restart()），别再吞掉让上层误判已删。
    let result = docker
        .remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() }))
        .await;
    container_action_result("remove", name, result, true)
}

pub async fn stop(docker: &Docker, name: &str) -> Result<()> {
    // 404 → 幂等成功；已停止(304)被 bollard 视作成功；其余错误上抛（对照 Python NotFound→pass）。
    let result = docker.stop_container(name, None::<StopContainerOptions>).await;
    container_action_result("stop", name, result, true)
}

pub async fn start(docker: &Docker, name: &str) -> Result<()> {
    // 404 is an error for start: a missing container was not started. Only stop/remove are idempotent.
    let result = docker.start_container(name, None::<StartContainerOptions<String>>).await;
    container_action_result("start", name, result, false)
}

/// 原地重启容器(保留端口映射/配置)。看门狗据此重启 mihomo,让其分流口"重新出现"→
/// 逼 lima 在当前活的 SSH 主连接上重建宿主端口转发(命门:仅 mihomo 这类可重启的基础设施用,
/// EC/aTrust/oss 见 [[hagb-oss-no-inplace-restart]] 走重建)。
pub async fn restart(docker: &Docker, name: &str) -> Result<()> {
    docker
        .restart_container(name, None::<RestartContainerOptions>)
        .await
        .map_err(|e| anyhow!("restart {name}: {e}"))
}

/// 容器是否在运行(缺失/任何错误 → false)。
pub async fn is_running(docker: &Docker, name: &str) -> bool {
    docker
        .inspect_container(name, None)
        .await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false)
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

/// 从 mirror 拉取 repo:tag(指定 platform)→ 校验 arch → retag 回原名 → 删 mirror 标,并上报 layer 进度。
pub async fn pull_retag_with_progress<F>(
    docker: &Docker,
    mirror: &str,
    repo: &str,
    tag: &str,
    host_arch: &str,
    mut on_progress: F,
) -> Result<PullOutcome>
where
    F: FnMut(ImagePullProgress) + Send,
{
    let src = format!("{mirror}/{repo}");
    let platform = format!("linux/{host_arch}");
    let opts = CreateImageOptions { from_image: src.clone(), tag: tag.to_string(), platform, ..Default::default() };
    let mut stream = docker.create_image(Some(opts), None, None);
    let mut progress = PullProgress::default();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| anyhow!("pull {src}:{tag}: {e}"))?;
        if let Some(error) = image_stream_error(&info) {
            return Err(anyhow!("pull {src}:{tag}: {error}"));
        }
        on_progress(progress.observe(&info));
    }
    let full_src = format!("{src}:{tag}");
    let arch = match docker.inspect_image(&full_src).await {
        Ok(info) => match info.architecture.filter(|arch| !arch.is_empty()) {
            Some(arch) => arch,
            None => {
                let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
                return Err(anyhow!("inspect {full_src}: architecture 缺失"));
            }
        },
        Err(e) => {
            let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
            return Err(anyhow!("inspect {full_src}: {e}"));
        }
    };
    if arch != host_arch {
        docker
            .remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None)
            .await
            .map_err(|e| anyhow!("{full_src} 架构为 {arch}(非 {host_arch}),且清理临时 tag 失败: {e}"))?;
        return Ok(PullOutcome::ArchMismatch(arch));
    }
    docker
        .tag_image(&full_src, Some(TagImageOptions { repo: repo.to_string(), tag: tag.to_string() }))
        .await
        .map_err(|e| anyhow!("tag {repo}:{tag}: {e}"))?;
    if let Err(e) = docker
        .remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None)
        .await
    {
        on_progress(ImagePullProgress {
            detail: format!("镜像已就绪，但清理临时 tag {full_src} 失败: {e}"),
            percent: Some(100),
        });
    }
    Ok(PullOutcome::Tagged(arch))
}

/// 无回调兼容入口。
pub async fn pull_retag(docker: &Docker, mirror: &str, repo: &str, tag: &str, host_arch: &str) -> Result<PullOutcome> {
    pull_retag_with_progress(docker, mirror, repo, tag, host_arch, |_| {}).await
}

struct OneshotCleanup {
    docker: Docker,
    id: Option<String>,
}

impl OneshotCleanup {
    fn new(docker: &Docker, id: String) -> Self {
        Self { docker: docker.clone(), id: Some(id) }
    }

    async fn cleanup(&mut self) -> Result<()> {
        if let Some(id) = self.id.as_deref() {
            rm_force(&self.docker, id).await?;
            self.id = None;
        }
        Ok(())
    }
}

impl Drop for OneshotCleanup {
    fn drop(&mut self) {
        let Some(id) = self.id.take() else { return };
        let docker = self.docker.clone();
        // 请求被取消时仍安排强删；普通成功/失败路径会先 await cleanup() 并清空 id。
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = rm_force(&docker, &id).await {
                    eprintln!("[docker] cancelled oneshot cleanup {id}: {e}");
                }
            });
        }
    }
}

/// 在指定网络上跑一次性容器并捕获 stdout(host-VM probe 用:宿主够不着 VM 内网 172.x,
/// 故探活搬进 VM 内一次性容器,经 socks5h://vpn-{id}:1080 打 probe_url)。create→start→wait→读 stdout→rm。
/// 调用方传唯一 name；后续一律用 create 返回的容器 ID，执行 10s 未收敛或任意错误也强删。
pub async fn run_oneshot_capture(docker: &Docker, name: &str, image: &str, cmd: Vec<&str>, network: &str) -> Result<String> {
    use bollard::container::{Config, WaitContainerOptions};
    use bollard::models::HostConfig;
    let config = Config {
        image: Some(image.to_string()),
        // 用 entrypoint 覆盖直接跑命令:oss 镜像自带 entrypoint(要 VPN_PROTOCOL),
        // 当 cmd 传会被 entrypoint 截走而非执行 curl,故整体覆盖 entrypoint。
        entrypoint: Some(cmd.into_iter().map(String::from).collect()),
        host_config: Some(HostConfig {
            network_mode: Some(network.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    };
    let created = docker
        .create_container(Some(CreateContainerOptions { name, platform: None }), config)
        .await
        .map_err(|e| anyhow!("create {name}: {e}"))?;
    let id = created.id;
    let mut cleanup = OneshotCleanup::new(docker, id.clone());
    let execution = match tokio::time::timeout(std::time::Duration::from_secs(10), async {
        docker.start_container(&id, None::<StartContainerOptions<String>>).await
            .map_err(|e| anyhow!("start {name}: {e}"))?;
        let mut wait = docker.wait_container(&id, None::<WaitContainerOptions<String>>);
        while let Some(item) = wait.next().await {
            match item {
                Ok(_) => {}
                // 容器非零退出(如 curl 半途超时/连接重置)不是错误:curl -w 仍把
                // http_code 写进 stdout,通没通交给 parse_probe_output 判(命门 #1)。
                Err(bollard::errors::Error::DockerContainerWaitError { .. }) => {}
                Err(e) => return Err(anyhow!("wait {name}: {e}")),
            }
        }
        // 只取 stdout(curl -w 写 stdout;-s 静默 stderr)
        let mut stream = docker.logs(&id, Some(LogsOptions::<String> {
            stdout: true, stderr: false, ..Default::default()
        }));
        let mut buf = String::new();
        while let Some(item) = stream.next().await {
            let out = item.map_err(|e| anyhow!("logs {name}: {e}"))?;
            buf.push_str(&String::from_utf8_lossy(out.into_bytes().as_ref()));
        }
        Ok::<String, anyhow::Error>(buf)
    }).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("oneshot {name} timed out after 10s")),
    };

    // Docker API 默认超时较长；删除自身不能反向拖住探活轮询。
    // 清理失败/超时只记日志、不影响执行结果(成功探活绝不因 rm 失败翻成离线);
    // 失败/超时时 guard 保留 id，Drop 会在后台再尝试强删。
    if let Err(rm) = match tokio::time::timeout(
        std::time::Duration::from_secs(2), cleanup.cleanup()).await {
        Ok(result) => result,
        Err(_) => Err(anyhow!("remove timed out after 2s; retrying in background")),
    } {
        eprintln!("[docker] oneshot cleanup {name}: {rm}");
    }
    execution
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
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};

    async fn fake_create() -> impl axum::response::IntoResponse {
        (axum::http::StatusCode::CREATED,
         axum::Json(serde_json::json!({ "Id": "probe-id", "Warnings": [] })))
    }

    async fn fake_start_failure() -> impl axum::response::IntoResponse {
        (axum::http::StatusCode::INTERNAL_SERVER_ERROR,
         axum::Json(serde_json::json!({ "message": "start failed" })))
    }

    async fn fake_remove(
        axum::extract::State(removes): axum::extract::State<Arc<AtomicUsize>>,
    ) -> axum::http::StatusCode {
        removes.fetch_add(1, Ordering::SeqCst);
        axum::http::StatusCode::NO_CONTENT
    }

    async fn fake_slow_remove(
        axum::extract::State(removes): axum::extract::State<Arc<AtomicUsize>>,
    ) -> axum::http::StatusCode {
        removes.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        axum::http::StatusCode::NO_CONTENT
    }

    async fn fake_start_ok() -> axum::http::StatusCode {
        axum::http::StatusCode::NO_CONTENT
    }

    async fn fake_wait_nonzero() -> impl axum::response::IntoResponse {
        (axum::http::StatusCode::OK,
         axum::Json(serde_json::json!({ "StatusCode": 7 })))
    }

    async fn fake_logs_empty() -> axum::http::StatusCode {
        axum::http::StatusCode::OK
    }

    #[test]
    fn not_found_classifies_404() {
        let e404 = bollard::errors::Error::DockerResponseServerError { status_code: 404, message: "no such image".into() };
        let e500 = bollard::errors::Error::DockerResponseServerError { status_code: 500, message: "boom".into() };
        assert!(is_not_found(&e404));
        assert!(!is_not_found(&e500));
    }

    #[test]
    fn start_404_is_error_but_stop_and_remove_are_idempotent() {
        let missing = || bollard::errors::Error::DockerResponseServerError {
            status_code: 404,
            message: "no such container".into(),
        };
        assert!(container_action_result("start", "vpn-x", Err(missing()), false).is_err());
        assert!(container_action_result("stop", "vpn-x", Err(missing()), true).is_ok());
        assert!(container_action_result("remove", "vpn-x", Err(missing()), true).is_ok());
    }


    #[test]
    fn fmt_uptime_units() {
        assert_eq!(fmt_uptime(5), "5秒");
        assert_eq!(fmt_uptime(90), "1分钟");
        assert_eq!(fmt_uptime(3661), "1小时1分");
        assert_eq!(fmt_uptime(90061), "1天1小时");
    }

    #[test]
    fn pull_progress_aggregates_layer_bytes() {
        let mut progress = PullProgress::default();
        let first = progress.observe(&CreateImageInfo {
            id: Some("layer-a".into()),
            status: Some("Downloading".into()),
            progress_detail: Some(bollard::models::ProgressDetail { current: Some(50), total: Some(100) }),
            ..Default::default()
        });
        assert_eq!(first.percent, Some(50));
        let second = progress.observe(&CreateImageInfo {
            id: Some("layer-b".into()),
            status: Some("Downloading".into()),
            progress_detail: Some(bollard::models::ProgressDetail { current: Some(25), total: Some(100) }),
            ..Default::default()
        });
        assert_eq!(second.percent, Some(37));
        assert!(second.detail.contains("37%"));
    }

    #[test]
    fn daemon_error_in_successful_stream_item_is_not_ignored() {
        let info = CreateImageInfo {
            error_detail: Some(bollard::models::ErrorDetail {
                code: Some(500),
                message: Some("registry unavailable".into()),
            }),
            ..Default::default()
        };
        assert_eq!(image_stream_error(&info).as_deref(), Some("registry unavailable"));
    }

    #[tokio::test]
    async fn uptime_none_without_docker() {
        assert_eq!(uptime(None, "deadbeef").await, None);
    }

    #[tokio::test]
    async fn oneshot_start_failure_still_removes_created_container() {
        use axum::routing::{delete, post};
        let removes = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/containers/create", post(fake_create))
            .route("/containers/probe-id/start", post(fake_start_failure))
            .route("/containers/probe-id", delete(fake_remove))
            .with_state(removes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let docker = Docker::connect_with_http(
            &format!("http://{addr}"), 5, bollard::API_DEFAULT_VERSION).unwrap();

        let err = run_oneshot_capture(&docker, "probe-test", "probe-image", vec!["true"], "testnet")
            .await.unwrap_err();
        assert!(err.to_string().contains("start"), "{err:#}");
        assert_eq!(removes.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn oneshot_cleanup_does_not_inherit_long_docker_timeout() {
        use axum::routing::{delete, post};
        let removes = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/containers/create", post(fake_create))
            .route("/containers/probe-id/start", post(fake_start_failure))
            .route("/containers/probe-id", delete(fake_slow_remove))
            .with_state(removes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let docker = Docker::connect_with_http(
            &format!("http://{addr}"), 120, bollard::API_DEFAULT_VERSION).unwrap();

        let err = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_oneshot_capture(&docker, "probe-test", "probe-image", vec!["true"], "testnet"),
        ).await.expect("cleanup must respect its own timeout").unwrap_err();
        assert!(err.to_string().contains("start"), "{err:#}");
        assert!(removes.load(Ordering::SeqCst) >= 1);
        server.abort();
    }

    /// 命门 #1 语义:容器非零退出(curl 连不通)≠执行错误 —— 仍读 stdout,
    /// 通没通交给 parse_probe_output;清理失败也不得覆盖执行结果。
    #[tokio::test]
    async fn oneshot_nonzero_exit_still_reads_stdout() {
        use axum::routing::{delete, get, post};
        let removes = Arc::new(AtomicUsize::new(0));
        let app = axum::Router::new()
            .route("/containers/create", post(fake_create))
            .route("/containers/probe-id/start", post(fake_start_ok))
            .route("/containers/probe-id/wait", post(fake_wait_nonzero))
            .route("/containers/probe-id/logs", get(fake_logs_empty))
            .route("/containers/probe-id", delete(fake_remove))
            .with_state(removes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let docker = Docker::connect_with_http(
            &format!("http://{addr}"), 5, bollard::API_DEFAULT_VERSION).unwrap();

        let out = run_oneshot_capture(&docker, "probe-test", "probe-image", vec!["true"], "testnet")
            .await.expect("non-zero container exit must not become an error");
        assert_eq!(out, "");
        assert_eq!(removes.load(Ordering::SeqCst), 1);
        server.abort();
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
