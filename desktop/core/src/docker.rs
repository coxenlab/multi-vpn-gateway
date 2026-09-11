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

/// 原地重启容器(保留配置)。看门狗据此重启 mihomo，再由 app 按新容器 IP 重建 SSH 转发。
/// 仅 mihomo 这类可重启基础设施使用；EC/aTrust/oss 走重建。
pub async fn restart(docker: &Docker, name: &str) -> Result<()> {
    docker
        .restart_container(name, None::<RestartContainerOptions>)
        .await
        .map_err(|e| anyhow!("restart {name}: {e}"))
}

/// 容器在 docker 网络里的 IP(多网络时取第一个有 IP 的)。缺失/未运行 → None。
/// app 自持的 SSH 转发据此直连容器(mihomo#1 不再 publish 端口,见 [`crate::tunnel`])。
pub async fn container_ip(docker: &Docker, name: &str) -> Option<String> {
    docker
        .inspect_container(name, None)
        .await
        .ok()?
        .network_settings?
        .networks?
        .into_values()
        .find_map(|n| n.ip_address.filter(|ip| !ip.is_empty()))
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

pub async fn container_ip_on_net(docker: &Docker, name: &str, net: &str) -> Option<String> {
    let info = docker.inspect_container(name, None).await.ok()?;
    let nets = info.network_settings?.networks?;
    let ip = nets.get(net)?.ip_address.clone()?;
    if ip.is_empty() { None } else { Some(ip) }
}

/// 某 docker 网络的首个 IPv4 子网(如 `172.18.0.0/16`);网络不存在或无 IPAM 配置 → None。
pub async fn network_subnet(docker: &Docker, net: &str) -> Option<String> {
    let info = docker.inspect_network(net, None::<InspectNetworkOptions<String>>).await.ok()?;
    let cfgs = info.ipam?.config?;
    cfgs.into_iter()
        .filter_map(|c| c.subnet)
        .find(|s| s.contains('.'))
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
    let locked = crate::image_sources::for_image(repo, tag, host_arch)?;
    let full_src = match &locked {
        Some(identity) => format!("{src}@{}", identity.manifest),
        None => format!("{src}:{tag}"),
    };
    let opts = match &locked {
        Some(_) => CreateImageOptions { from_image: full_src.clone(), platform, ..Default::default() },
        None => CreateImageOptions { from_image: src.clone(), tag: tag.to_string(), platform, ..Default::default() },
    };
    let mut stream = docker.create_image(Some(opts), None, None);
    let mut progress = PullProgress::default();
    while let Some(item) = stream.next().await {
        let info = item.map_err(|e| anyhow!("pull {src}:{tag}: {e}"))?;
        if let Some(error) = image_stream_error(&info) {
            return Err(anyhow!("pull {src}:{tag}: {error}"));
        }
        on_progress(progress.observe(&info));
    }
    let arch = match docker.inspect_image(&full_src).await {
        Ok(info) => {
            if let Some(identity) = &locked {
                identity.verify(info.id.as_deref())?;
                anyhow::ensure!(info.architecture.as_deref() == Some(host_arch), "mihomo 镜像架构与锁定来源不同");
            }
            match info.architecture.filter(|arch| !arch.is_empty()) {
                Some(arch) => arch,
                None => {
                    let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
                    return Err(anyhow!("inspect {full_src}: architecture 缺失"));
                }
            }
        },
        Err(e) => {
            if locked.is_none() {
                let _ = docker.remove_image(&full_src, Some(RemoveImageOptions { force: true, ..Default::default() }), None).await;
            }
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
    // Keep the immutable source reference for provenance; deleting a digest is
    // not the same operation as removing a temporary mirror tag.
    if locked.is_some() { return Ok(PullOutcome::Tagged(arch)); }
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

/// 常驻的低权限探针容器。只复用本应用在同一网络上创建的实例,不发布宿主端口。
pub const PROBE_CONTAINER: &str = "vpncore-probe";
type ProbeContainerCache = std::sync::Arc<tokio::sync::Mutex<Option<String>>>;
static PROBE_CONTAINERS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, ProbeContainerCache>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

pub async fn run_probe_capture(docker: &Docker, cfg: &crate::config::Config, image: &str, cmd: Vec<&str>) -> Result<String> {
    let key = format!("{}:{}", cfg.docker_socket().display(), cfg.vpn_net);
    let cache = PROBE_CONTAINERS.lock().map_err(|_| anyhow!("probe cache unavailable"))?
        .entry(key).or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(None))).clone();
    let id = {
        let mut cached = cache.lock().await;
        if let Some(id) = &*cached { id.clone() } else {
            let id = match docker.inspect_container(PROBE_CONTAINER, None).await {
                Ok(info) => {
                    let owned = info.config.as_ref().and_then(|c| c.labels.as_ref())
                        .and_then(|m| m.get("com.vpnmgr.role")).map(String::as_str) == Some("probe");
                    let same_net = info.host_config.as_ref().and_then(|h| h.network_mode.as_deref()) == Some(&cfg.vpn_net);
                    anyhow::ensure!(owned && same_net, "探针容器名称已被其他实例占用");
                    let id = info.id.ok_or_else(|| anyhow!("探针容器缺少 ID"))?;
                    if info.state.and_then(|s| s.running) != Some(true) {
                        docker.start_container(&id, None::<StartContainerOptions<String>>).await?;
                    }
                    id
                }
                Err(e) if is_not_found(&e) => {
                    let config = bollard::container::Config {
                        image: Some(image.to_string()),
                        entrypoint: Some(vec!["sleep".into(), "infinity".into()]),
                        user: Some("65534".into()),
                        labels: Some(std::collections::HashMap::from([("com.vpnmgr.role".into(), "probe".into())])),
                        host_config: Some(bollard::models::HostConfig {
                            network_mode: Some(cfg.vpn_net.clone()),
                            readonly_rootfs: Some(true),
                            cap_drop: Some(vec!["ALL".into()]),
                            security_opt: Some(vec!["no-new-privileges:true".into()]),
                            ..Default::default()
                        }),
                        ..Default::default()
                    };
                    let id = docker.create_container(Some(CreateContainerOptions {name: PROBE_CONTAINER, platform: None}), config).await?.id;
                    docker.start_container(&id, None::<StartContainerOptions<String>>).await?;
                    id
                }
                Err(e) => return Err(e.into()),
            };
            *cached = Some(id.clone());
            id
        }
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), exec_capture(docker, &id, cmd))
        .await.unwrap_or_else(|_| Err(anyhow!("probe execution timed out")));
    if result.is_err() { *cache.lock().await = None; }
    result
}

/// 仅清理本次探针。API 层保有维护许可，HTTP 取消后仍执行到清理结束。
pub async fn run_tun_probe(docker: &Docker, image: &str) -> Result<bool> {
    run_tun_probe_with_budget(docker, image, std::time::Duration::from_secs(10)).await
}

async fn run_tun_probe_with_budget(docker: &Docker, image: &str, budget: std::time::Duration) -> Result<bool> {
    use bollard::container::{Config, WaitContainerOptions};
    use bollard::models::{DeviceMapping, HostConfig};
    let docker = docker.clone().with_timeout(std::time::Duration::from_secs(5));
    let token = format!("{:032x}", rand::random::<u128>());
    let name = format!("vpncore-tun-probe-{token}");
    let config = Config {
        image: Some(image.to_string()),
        entrypoint: Some(vec!["/bin/sh".into(), "-c".into(), "test -c /dev/net/tun".into()]),
        labels: Some(HashMap::from([
            ("com.vpnmgr.role".into(), "tun-probe".into()),
            ("com.vpnmgr.operation".into(), token.clone()),
        ])),
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
    let mut created_id = None;
    let outcome = tokio::time::timeout(budget, async {
        let id = docker.create_container(Some(CreateContainerOptions { name: &name, platform: None }), config).await?.id;
        anyhow::ensure!(!id.is_empty(), "探针创建结果缺少 ID");
        created_id = Some(id.clone());
        docker.start_container(&id, None::<StartContainerOptions<String>>).await?;
        let mut wait = docker.wait_container(&id, None::<WaitContainerOptions<String>>);
        match wait.next().await {
            Some(Ok(response)) => {
                anyhow::ensure!(response.error.and_then(|e| e.message).is_none_or(|m| m.is_empty()), "探针等待接口返回错误");
                Ok(response.status_code == 0)
            }
            Some(Err(bollard::errors::Error::DockerContainerWaitError { error, code })) if error.is_empty() && code > 0 => Ok(false),
            Some(Err(error)) => Err(error.into()),
            None => Err(anyhow!("探针未返回退出状态")),
        }
    }).await.unwrap_or_else(|_| Err(anyhow!("探针检测超时")));

    // 创建响应丢失时只读回归属，不重放 create，也不删未核对的同名容器。
    let cleanup = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        let id = match created_id {
            Some(id) => id,
            None => match docker.inspect_container(&name, None).await {
                Ok(info) => {
                    let labels = info.config.and_then(|c| c.labels).unwrap_or_default();
                    anyhow::ensure!(labels.get("com.vpnmgr.role").map(String::as_str) == Some("tun-probe")
                        && labels.get("com.vpnmgr.operation") == Some(&token), "探针归属未确认，未清理同名容器");
                    info.id.filter(|id| !id.is_empty()).ok_or_else(|| anyhow!("探针清理缺少 ID"))?
                }
                Err(e) if is_not_found(&e) => return Ok(()),
                Err(e) => return Err(e.into()),
            },
        };
        let removed = docker.remove_container(&id, Some(RemoveContainerOptions { force: true, v: true, ..Default::default() })).await;
        container_action_result("remove probe", &id, removed, true)
    }).await.unwrap_or_else(|_| Err(anyhow!("清理超时")));
    if let Err(error) = cleanup {
        return Err(anyhow!("{}；探针清理未确认: {error}", outcome.err().map(|e| e.to_string()).unwrap_or_else(|| "检测已结束".into())));
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tun_probe_errors_do_not_pass_or_delete_unowned_containers() {
        use axum::{body::Body, http::{Method, Request, StatusCode}, response::IntoResponse, Router};
        use std::sync::{Arc, Mutex};
        use serde_json::{json, Value};
        for mode in ["wait_error", "nonzero", "empty", "embedded_error", "timeout", "start_error", "cleanup_error", "ok", "lost_create", "foreign"] {
            let calls = Arc::new(Mutex::new(Vec::<(Method, String, Value)>::new()));
            let observed = calls.clone();
            let app = Router::new().fallback(move |request: Request<Body>| {
                let calls = observed.clone();
                async move {
                    let (parts, body) = request.into_parts();
                    let bytes = axum::body::to_bytes(body, 1024 * 1024).await.unwrap();
                    let value = serde_json::from_slice::<Value>(&bytes).unwrap_or(Value::Null);
                    let path = parts.uri.path().to_string();
                    calls.lock().unwrap().push((parts.method.clone(), parts.uri.to_string(), value));
                    if path.ends_with("/containers/create") {
                        return if mode == "lost_create" || mode == "foreign" {
                            (StatusCode::INTERNAL_SERVER_ERROR, axum::Json(json!({"message":"create result unavailable"}))).into_response()
                        } else { axum::Json(json!({"Id":"probe-owned-id","Warnings":[]})).into_response() };
                    }
                    if path.ends_with("/json") {
                        let create = calls.lock().unwrap().iter().find(|(_, p, _)| p.contains("/containers/create")).unwrap().2.clone();
                        let labels = if mode == "foreign" { json!({"com.vpnmgr.role":"other"}) } else { create["Labels"].clone() };
                        return axum::Json(json!({"Id":"probe-owned-id","Config":{"Labels":labels}})).into_response();
                    }
                    if parts.method == Method::DELETE {
                        return if mode == "cleanup_error" { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::NO_CONTENT }.into_response();
                    }
                    if path.ends_with("/start") {
                        return if mode == "start_error" { StatusCode::INTERNAL_SERVER_ERROR } else { StatusCode::NO_CONTENT }.into_response();
                    }
                    if path.ends_with("/wait") {
                        if mode == "timeout" { tokio::time::sleep(std::time::Duration::from_secs(1)).await; }
                        return match mode {
                            "wait_error" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                            "empty" => "".into_response(),
                            "nonzero" => axum::Json(json!({"StatusCode":1})).into_response(),
                            "embedded_error" => axum::Json(json!({"StatusCode":0,"Error":{"Message":"wait failed"}})).into_response(),
                            _ => axum::Json(json!({"StatusCode":0})).into_response(),
                        };
                    }
                    StatusCode::NOT_FOUND.into_response()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
            let docker = Docker::connect_with_http(&format!("http://{address}"), 2, bollard::API_DEFAULT_VERSION).unwrap();
            let result = if mode == "timeout" {
                tokio::time::timeout(std::time::Duration::from_secs(2),
                    run_tun_probe_with_budget(&docker, "sha256:fixture-image", std::time::Duration::from_millis(100))).await.unwrap()
            } else { run_tun_probe(&docker, "sha256:fixture-image").await };
            server.abort(); let _ = server.await;
            match mode {
                "ok" => assert!(result.unwrap()),
                "nonzero" => assert!(!result.unwrap()),
                _ => assert!(result.is_err(), "{mode} must not pass"),
            }
            let calls = calls.lock().unwrap();
            assert_eq!(calls[0].0, Method::POST, "do not delete before creating an owned probe");
            let create = &calls[0].2;
            assert_eq!(create["Image"], "sha256:fixture-image");
            assert_eq!(create["HostConfig"]["NetworkMode"], "none");
            assert_eq!(create["Labels"]["com.vpnmgr.role"], "tun-probe");
            let url = reqwest::Url::parse(&format!("http://fixture{}", calls[0].1)).unwrap();
            let name = url.query_pairs().find(|(key, _)| key == "name").unwrap().1;
            assert!(name.starts_with("vpncore-tun-probe-") && name.len() > 40);
            let deletes: Vec<_> = calls.iter().filter(|(method, _, _)| *method == Method::DELETE).collect();
            assert_eq!(deletes.len(), usize::from(mode != "foreign"));
            assert!(deletes.iter().all(|(_, path, _)| path.contains("/containers/probe-owned-id?")));
            assert!(deletes.iter().all(|(_, path, _)| path.contains("v=true")));
        }
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
        d.create_container(Some(CreateContainerOptions { name: name.to_string(), platform: None }), plan.config).await.unwrap();
        d.start_container(name, None::<StartContainerOptions<String>>).await.unwrap();
        exec_inject_stdin(&d, name, vec!["sh", "-c", "cat > /tmp/secret"], b"s3cr3t").await.unwrap();
        let out = exec_capture(&d, name, vec!["cat", "/tmp/secret"]).await.unwrap();
        assert_eq!(out.trim(), "s3cr3t");
        put_file(&d, name, "/tmp", "hello.txt", b"hi").await.unwrap();
        let out = exec_capture(&d, name, vec!["cat", "/tmp/hello.txt"]).await.unwrap();
        assert_eq!(out.trim(), "hi");
        rm_force(&d, name).await.unwrap();
    }
}
