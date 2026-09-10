//! 替换专用资源：候选容器先只创建，数据复制在 VM 内完成，旧卷只读。
use std::{collections::HashMap, time::Duration};
use anyhow::{anyhow, ensure, Result};
use bollard::{container::{Config, CreateContainerOptions, ListContainersOptions, WaitContainerOptions}, models::HostConfig, volume::CreateVolumeOptions, Docker};
use futures_util::StreamExt;
use crate::adapters::ContainerPlan;

const CHANNEL: &str = "io.vpnmgr.channel";
const OPERATION: &str = "io.vpnmgr.replacement";
const ROLE: &str = "io.vpnmgr.role";

#[derive(Clone)]
pub struct Owner { pub channel: String, pub operation: String }

impl Owner {
    pub fn validate(&self) -> Result<()> {
        ensure!([&self.channel, &self.operation].iter().all(|s| !s.is_empty() && s.len() <= 64 && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')), "无效的替换资源标识");
        Ok(())
    }
    pub fn candidate_name(&self) -> String { format!("vpn-{}-next-{}", self.channel, self.operation) }
    pub fn volume_name(&self) -> String { format!("vpndata-{}-next-{}", self.channel, self.operation) }
    pub fn copy_name(&self) -> String { format!("vpn-copy-{}-{}", self.channel, self.operation) }
    fn labels(&self, role: &str) -> HashMap<String, String> {
        HashMap::from([(CHANNEL.into(), self.channel.clone()), (OPERATION.into(), self.operation.clone()), (ROLE.into(), role.into())])
    }
    pub(crate) fn owns(&self, labels: Option<&HashMap<String, String>>, role: &str) -> bool {
        labels.is_some_and(|labels| self.labels(role).iter().all(|(k, v)| labels.get(k) == Some(v)))
    }
}

fn volume_name_valid(name: &str) -> bool {
    name.starts_with("vpndata-") && name.len() <= 180 && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

/// 只接受命名卷，不能把数据库内容变成宿主 bind 路径。
pub fn use_volume(plan: &mut ContainerPlan, volume: &str) -> Result<()> {
    ensure!(volume_name_valid(volume), "无效的通道数据卷名");
    let binds = plan.config.host_config.as_mut().and_then(|h| h.binds.as_mut()).ok_or_else(|| anyhow!("通道未声明数据卷"))?;
    ensure!(binds.len() == 1, "通道数据卷布局不受支持");
    let (_, target) = binds[0].split_once(':').ok_or_else(|| anyhow!("无效的数据卷绑定"))?;
    ensure!(matches!(target, "/root" | "/config"), "通道数据卷目标不受支持");
    binds[0] = format!("{volume}:{target}");
    Ok(())
}

pub async fn ensure_volume(docker: &Docker, owner: &Owner) -> Result<String> {
    owner.validate()?;
    let name = owner.volume_name();
    match docker.inspect_volume(&name).await {
        Ok(volume) => {
            ensure!(owner.owns(Some(&volume.labels), "candidate"), "候选数据卷已被其他操作占用");
            return Ok(name);
        }
        Err(e) if crate::docker::is_not_found(&e) => {}
        Err(e) => return Err(e.into()),
    }
    let created = docker.create_volume(CreateVolumeOptions {
        name: name.clone(), driver: "local".into(), labels: owner.labels("candidate"), ..Default::default()
    }).await;
    // 创建响应丢失也先读回，不能改名盲目再创建一份。
    let actual = docker.inspect_volume(&name).await;
    match actual {
        Ok(volume) if owner.owns(Some(&volume.labels), "candidate") => Ok(name),
        Ok(_) => Err(anyhow!("候选数据卷归属不匹配")),
        Err(error) => Err(anyhow!("候选数据卷创建未确认: {}; readback: {error}", created.err().map(|e| e.to_string()).unwrap_or_default())),
    }
}

pub async fn create_candidate(docker: &Docker, plan: &ContainerPlan, owner: &Owner) -> Result<String> {
    owner.validate()?;
    ensure!(plan.name == owner.candidate_name(), "候选容器名称与操作不匹配");
    match docker.inspect_container(&plan.name, None).await {
        Ok(info) => {
            ensure!(owner.owns(info.config.as_ref().and_then(|c| c.labels.as_ref()), "candidate"), "候选容器被其他操作占用");
            ensure!(info.state.as_ref().and_then(|s| s.status) == Some(bollard::models::ContainerStateStatusEnum::CREATED), "候选容器已启动，需先核对进度");
            return info.id.ok_or_else(|| anyhow!("候选容器缺少 ID"));
        }
        Err(e) if crate::docker::is_not_found(&e) => {}
        Err(e) => return Err(e.into()),
    }
    let mut config = plan.config.clone();
    config.labels.get_or_insert_with(HashMap::new).extend(owner.labels("candidate"));
    let created = docker.create_container(Some(CreateContainerOptions { name: plan.name.clone(), platform: None }), config).await;
    let info = docker.inspect_container(&plan.name, None).await;
    match info {
        Ok(info) if owner.owns(info.config.as_ref().and_then(|c| c.labels.as_ref()), "candidate") => {
            ensure!(info.state.as_ref().and_then(|s| s.status) == Some(bollard::models::ContainerStateStatusEnum::CREATED), "候选容器未保持初始创建状态");
            info.id.ok_or_else(|| anyhow!("候选容器缺少 ID"))
        }
        Ok(_) => Err(anyhow!("候选容器归属不匹配")),
        Err(error) => Err(anyhow!("候选容器创建未确认: {}; readback: {error}", created.err().map(|e| e.to_string()).unwrap_or_default())),
    }
}

const COPY_SCRIPT: &str = "set -euo pipefail; find /target -mindepth 1 -delete; tar --numeric-owner --xattrs --acls -C /source -cpf - . | tar --numeric-owner --xattrs --acls -C /target -xpf -";

/// 调用前旧容器必须已停止；目标卷只属于本次候选，tar 数据不经过宿主内存或日志。
pub async fn copy_volume(docker: &Docker, owner: &Owner, old_container: &str, source: &str, image: &str) -> Result<()> {
    owner.validate()?;
    let target = owner.volume_name();
    ensure!(volume_name_valid(source) && source != target, "源卷与候选卷无效或相同");
    ensure!(image.strip_prefix("sha256:").is_some_and(|digest| digest.len() == 64 && digest.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))), "复制工具必须固定到已加载镜像 ID");
    let old = docker.inspect_container(old_container, None).await?;
    ensure!(old.id.as_deref() == Some(old_container) && old.name.as_deref() == Some(&format!("/vpn-{}", owner.channel)), "旧容器身份与通道不匹配");
    ensure!(old.state.as_ref().and_then(|s| s.running) == Some(false), "复制数据前必须停止旧通道");
    ensure!(old.mounts.as_ref().is_some_and(|m| m.iter().any(|m| m.name.as_deref() == Some(source))), "源卷不属于旧容器");
    let volume = docker.inspect_volume(&target).await?;
    ensure!(owner.owns(Some(&volume.labels), "candidate"), "候选数据卷归属不匹配");
    for name in [source, target.as_str()] {
        let containers = docker.list_containers(Some(ListContainersOptions {
            all: true, filters: HashMap::from([("volume".to_string(), vec![name.to_string()])]), ..Default::default()
        })).await?;
        for container in containers {
            let id = container.id.ok_or_else(|| anyhow!("卷使用者缺少 ID"))?;
            let info = docker.inspect_container(&id, None).await?;
            ensure!(info.state.as_ref().and_then(|s| s.running) == Some(false), "数据卷仍有运行中的使用者");
            ensure!(info.id == old.id || owner.owns(info.config.as_ref().and_then(|c| c.labels.as_ref()), "candidate"), "数据卷被其他容器使用");
        }
    }
    let copy_name = owner.copy_name();
    // 遗留复制进程必须由恢复流程先核对；这里不抢占或清空正在复制的卷。
    match docker.inspect_container(&copy_name, None).await {
        Err(e) if crate::docker::is_not_found(&e) => {}
        Err(e) => return Err(e.into()),
        Ok(_) => return Err(anyhow!("存在待核对的复制容器")),
    }
    let config = Config {
        image: Some(image.to_string()), labels: Some(owner.labels("copy")),
        entrypoint: Some(vec!["bash".into(), "-c".into(), COPY_SCRIPT.into()]),
        host_config: Some(HostConfig {
            network_mode: Some("none".into()), readonly_rootfs: Some(true),
            binds: Some(vec![format!("{source}:/source:ro"), format!("{target}:/target")]),
            cap_drop: Some(vec!["ALL".into()]),
            cap_add: Some(["CHOWN", "DAC_OVERRIDE", "FOWNER", "FSETID", "SETFCAP", "MKNOD"].map(String::from).to_vec()),
            security_opt: Some(vec!["no-new-privileges:true".into()]), ..Default::default()
        }), ..Default::default()
    };
    let created = docker.create_container(Some(CreateContainerOptions { name: copy_name.clone(), platform: None }), config).await;
    let info = docker.inspect_container(&copy_name, None).await?;
    ensure!(owner.owns(info.config.as_ref().and_then(|c| c.labels.as_ref()), "copy"), "复制容器归属不匹配");
    let id = info.id.ok_or_else(|| anyhow!("复制容器缺少 ID"))?;
    // 不因丢失 create 响应重复创建；已有对象通过上面的身份读回确认。
    let _ = created;
    let run = async {
        let _started = docker.start_container(&id, None::<bollard::container::StartContainerOptions<String>>).await;
        let mut wait = docker.wait_container(&id, None::<WaitContainerOptions<String>>);
        let _waited = wait.next().await;
        let state = docker.inspect_container(&id, None).await?.state.ok_or_else(|| anyhow!("复制进程状态缺失"))?;
        ensure!(state.status == Some(bollard::models::ContainerStateStatusEnum::EXITED) && state.exit_code == Some(0), "数据卷复制未完成");
        Ok::<_, anyhow::Error>(())
    };
    let outcome = tokio::time::timeout(Duration::from_secs(180), run).await;
    let cleanup = crate::docker::rm_force(docker, &id).await;
    // 删除响应不明时核对本 ID，不重放删除。
    let removed = matches!(docker.inspect_container(&id, None).await, Err(e) if crate::docker::is_not_found(&e));
    ensure!(removed, "复制容器清理未确认: {}", cleanup.err().map(|e| e.to_string()).unwrap_or_default());
    outcome.map_err(|_| anyhow!("数据卷复制超时"))??;
    ensure!(docker.inspect_container(old_container, None).await?.state.and_then(|s| s.running) == Some(false), "复制期间旧通道被重新启动，候选数据不可提交");
    Ok(())
}
