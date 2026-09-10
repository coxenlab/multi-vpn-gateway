//! 通道替换。入口必须由调用方持有通道操作锁；内部 payload 不返回 API 或日志。
use std::{collections::HashMap, time::Duration};
use anyhow::{anyhow, ensure, Result};
use bollard::{Docker, container::{RenameContainerOptions, UpdateContainerOptions, StartContainerOptions, StopContainerOptions, ListContainersOptions}, models::{ContainerInspectResponse, RestartPolicy, RestartPolicyNameEnum}};
use serde_json::{json, Map, Value};
use crate::{AppState, store::{self, ChannelPublic}, manager, registry, replacement_store::{self as journal, Record, AppliedRuntime}, replacement_docker::{self as resources, Owner}};

fn owner(record: &Record, restore: bool) -> Owner {
    Owner { channel: record.channel_id.clone(), operation: format!("{}{}", record.operation_id, if restore { "-restore" } else { "" }) }
}
fn text<'a>(payload: &'a Value, field: &str) -> Result<&'a str> { payload[field].as_str().ok_or_else(|| anyhow!("替换记录缺少 {field}")) }
fn object<'a>(payload: &'a Value, field: &str) -> Result<&'a Map<String, Value>> { payload[field].as_object().ok_or_else(|| anyhow!("替换记录缺少 {field}")) }
fn old_policy(record: &Record) -> Result<RestartPolicy> { Ok(serde_json::from_value(record.payload["old_policy"].clone())?) }
fn policy(name: RestartPolicyNameEnum) -> RestartPolicy { RestartPolicy { name: Some(name), maximum_retry_count: Some(0) } }
fn id(info: &ContainerInspectResponse) -> Result<&str> { info.id.as_deref().ok_or_else(|| anyhow!("容器缺少 ID")) }
fn running(info: &ContainerInspectResponse) -> Option<bool> { info.state.as_ref().and_then(|s| s.running) }

async fn get(docker: &Docker, name: &str) -> Result<Option<ContainerInspectResponse>> {
    match docker.inspect_container(name, None).await {
        Ok(info) => Ok(Some(info)), Err(e) if crate::docker::is_not_found(&e) => Ok(None), Err(e) => Err(e.into()),
    }
}
async fn identity(docker: &Docker, expected: &str) -> Result<ContainerInspectResponse> {
    let info = docker.inspect_container(expected, None).await?;
    ensure!(info.id.as_deref() == Some(expected), "容器 ID 与替换记录不匹配");
    Ok(info)
}
async fn rename(docker: &Docker, id: &str, expected: &str, target: &str) -> Result<()> {
    let info = identity(docker, id).await?;
    if info.name.as_deref() == Some(&format!("/{target}")) { return Ok(()); }
    ensure!(info.name.as_deref() == Some(&format!("/{expected}")), "容器名称已由外部改变");
    let _ = docker.rename_container(id, RenameContainerOptions { name: target }).await;
    ensure!(identity(docker, id).await?.name.as_deref() == Some(&format!("/{target}")), "容器更名未确认");
    Ok(())
}
async fn set_policy(docker: &Docker, id: &str, desired: RestartPolicy) -> Result<()> {
    let _ = docker.update_container(id, UpdateContainerOptions::<String> { restart_policy: Some(desired.clone()), ..Default::default() }).await;
    let actual = identity(docker, id).await?.host_config.and_then(|h| h.restart_policy).ok_or_else(|| anyhow!("容器重启策略缺失"))?;
    ensure!(actual.name == desired.name && actual.maximum_retry_count.unwrap_or(0) == desired.maximum_retry_count.unwrap_or(0), "容器重启策略未确认");
    Ok(())
}
async fn set_running(docker: &Docker, id: &str, desired: bool) -> Result<()> {
    if running(&identity(docker, id).await?) == Some(desired) { return Ok(()); }
    if desired { let _ = docker.start_container(id, None::<StartContainerOptions<String>>).await; }
    else { let _ = docker.stop_container(id, Some(StopContainerOptions { t: 10 })).await; }
    ensure!(running(&identity(docker, id).await?) == Some(desired), "容器启停结果未确认");
    Ok(())
}
async fn remove(docker: &Docker, info: &ContainerInspectResponse) -> Result<()> {
    let id = id(info)?;
    let _ = crate::docker::rm_force(docker, id).await;
    ensure!(get(docker, id).await?.is_none(), "容器清理未确认");
    Ok(())
}
async fn owned(docker: &Docker, owner: &Owner, canonical: bool) -> Result<Option<ContainerInspectResponse>> {
    let mut names = vec![owner.candidate_name()];
    if canonical { names.push(format!("vpn-{}", owner.channel)); }
    for name in names {
        if let Some(info) = get(docker, &name).await? {
            if owner.owns(info.config.as_ref().and_then(|c| c.labels.as_ref()), "candidate") { return Ok(Some(info)); }
            ensure!(name != owner.candidate_name(), "候选容器归属不匹配");
        }
    }
    Ok(None)
}
async fn remove_volume(docker: &Docker, name: &str, owner: Option<&Owner>) -> Result<()> {
    let volume = match docker.inspect_volume(name).await {
        Ok(v) => v, Err(e) if crate::docker::is_not_found(&e) => return Ok(()), Err(e) => return Err(e.into()),
    };
    if let Some(owner) = owner { ensure!(owner.owns(Some(&volume.labels), "candidate"), "候选数据卷归属不匹配"); }
    let used = docker.list_containers(Some(ListContainersOptions { all: true, filters: HashMap::from([("volume".to_string(), vec![name.to_string()])]), ..Default::default() })).await?;
    ensure!(used.is_empty(), "待清理数据卷仍被使用");
    let _ = docker.remove_volume(name, None).await;
    ensure!(matches!(docker.inspect_volume(name).await, Err(e) if crate::docker::is_not_found(&e)), "数据卷清理未确认");
    Ok(())
}
fn advance(st: &AppState, key: &str, record: &Record, phase: &str, updates: Value) -> Result<Record> {
    let mut payload = record.payload.clone();
    payload.as_object_mut().ok_or_else(|| anyhow!("替换记录无效"))?.extend(updates.as_object().ok_or_else(|| anyhow!("替换记录更新无效"))?.clone());
    journal::advance(&st.cfg.db_path(), key, record, phase, &payload)?;
    journal::get(&st.cfg.db_path(), key, &record.channel_id)?.ok_or_else(|| anyhow!("替换记录丢失"))
}
fn changed(ch: &ChannelPublic, fields: &Map<String, Value>) -> ChannelPublic {
    let mut ch = ch.clone();
    for (key, value) in fields {
        let value = store::clean_field(key, value.as_str().unwrap_or_default());
        match key.as_str() {
            "name" => ch.name = value, "server" => ch.server = value, "username" => ch.username = value,
            "ec_ver" => ch.ec_ver = Some(value), "probe_url" => ch.probe_url = value, _ => {}
        }
    }
    if let Some(value) = fields.get("routing_enabled").and_then(Value::as_bool) { ch.routing_enabled = value; }
    ch
}
async fn initialize(docker: &Docker, ch: &ChannelPublic, config: &Map<String, Value>) -> Result<()> {
    let spec = registry::get(&ch.vpn_type)?;
    manager::initialize_gui(docker, ch).await;
    if spec.runtime == "oss" { manager::oss_connect(docker, &ch.id, spec.protocol.as_deref().unwrap_or_default(), config).await?; }
    Ok(())
}
async fn applied_runtime(docker: &Docker, id: &str, volume: &str, connected: bool, latency: Option<i64>) -> Result<AppliedRuntime> {
    let info = identity(docker, id).await?;
    let is_running = running(&info) == Some(true);
    Ok(AppliedRuntime { container_id: id.into(), data_volume: volume.into(), novnc_port: None,
        status: if is_running && connected { "logged_in" } else if is_running { "running" } else { "stopped" }.into(),
        latency_ms: if connected { latency } else { None } })
}
fn validate_source(info: &ContainerInspectResponse, cid: &str, volume: &str) -> Result<()> {
    ensure!(info.name.as_deref() == Some(&format!("/vpn-{cid}")), "旧容器名称与通道不匹配");
    ensure!(info.mounts.as_ref().is_some_and(|m| m.iter().any(|m| m.name.as_deref() == Some(volume))), "旧数据卷与容器不匹配");
    Ok(())
}

/// 调用前已拿通道变更锁。BYO 的安装在可写层，不能走此重建路径。
pub async fn replace(st: &AppState, ch: &ChannelPublic, fields: &Map<String, Value>, force_start: bool) -> Result<()> {
    let docker = st.docker().ok_or_else(|| anyhow!("docker unavailable"))?;
    let db = st.cfg.db_path(); let key = store::master_key(&st.cfg.data_dir)?;
    ensure!(journal::get(&db, &key, &ch.id)?.is_none(), "上一次修改尚未验证，请先完成登录或恢复上一次设置");
    let spec = registry::get(&ch.vpn_type)?;
    ensure!(spec.runtime != "byo", "自装客户端不能通过重建修改连接参数");
    let old_id = ch.container_id.as_deref().ok_or_else(|| anyhow!("通道没有旧容器"))?;
    let old = identity(&docker, old_id).await?;
    let volume = journal::data_volume(&db, &ch.id)?;
    validate_source(&old, &ch.id, &volume)?;
    let desired = changed(ch, fields);
    let mut plan = manager::channel_plan(st, &docker, &desired, ch.vnc_password.as_deref().unwrap_or_default()).await?;
    let selected = docker.inspect_image(plan.config.image.as_deref().ok_or_else(|| anyhow!("候选镜像缺失"))?).await?;
    plan.config.image = Some(selected.id.ok_or_else(|| anyhow!("候选镜像缺少固定 ID"))?);
    let helper_image = docker.inspect_image("vpnmgr/oss-vpn:latest").await?.id.ok_or_else(|| anyhow!("复制镜像缺少固定 ID"))?;
    let old_config = store::get_config(&db, &key, &ch.id)?;
    let mut config = old_config.clone();
    for field in ["server", "username", "password"] {
        if let Some(value) = fields.get(field).and_then(Value::as_str) {
            config.insert(field.into(), json!(if field == "password" { value.to_string() } else { store::clean_field(field, value) }));
        }
    }
    let snapshot = serde_json::to_value(ch)?;
    let mut old_fields = Map::new();
    for field in fields.keys() {
        let value = if field == "password" { json!(store::get_password(&db, &key, &ch.id)?) }
        else { snapshot.get(field).filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!("")) };
        old_fields.insert(field.clone(), value);
    }
    let secrets: Vec<String> = spec.inputs.iter().filter(|i| i.secret).map(|i| i.key.clone()).collect();
    let operation: String = (0..16).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    let owner = Owner { channel: ch.id.clone(), operation: operation.clone() };
    let payload = json!({"fields":fields,"old_fields":old_fields,"old_config":old_config,"config":config,"secrets":secrets,
        "old_id":old_id,"old_volume":volume,"old_image":old.image.as_deref().ok_or_else(|| anyhow!("旧镜像 ID 缺失"))?,
        "old_running":running(&old)==Some(true),"old_policy":old.host_config.as_ref().and_then(|h| h.restart_policy.clone()).unwrap_or_else(|| policy(RestartPolicyNameEnum::NO)),
        "start":force_start || ch.status != "stopped","config_applied":false});
    journal::begin(&db, &key, &ch.id, &operation, &payload)?;
    let outcome = async {
        let record = journal::get(&db, &key, &ch.id)?.ok_or_else(|| anyhow!("替换记录丢失"))?;
        resources::ensure_volume(&docker, &owner).await?;
        plan.name = owner.candidate_name();
        plan.config.host_config.as_mut().ok_or_else(|| anyhow!("候选缺少运行配置"))?.restart_policy = Some(policy(RestartPolicyNameEnum::NO));
        resources::use_volume(&mut plan, &owner.volume_name())?;
        let candidate = resources::create_candidate(&docker, &plan, &owner).await?;
        let record = advance(st, &key, &record, "prepared", json!({"new_id":candidate}))?;
        let record = advance(st, &key, &record, "switching", json!({}))?;
        crate::novnc::drop_for(st, &ch.id).await;
        set_policy(&docker, old_id, policy(RestartPolicyNameEnum::NO)).await?;
        set_running(&docker, old_id, false).await?;
        resources::copy_volume(&docker, &owner, old_id, &volume, &helper_image).await?;
        let canonical = format!("vpn-{}", ch.id);
        rename(&docker, old_id, &canonical, &format!("{canonical}-previous-{operation}")).await?;
        rename(&docker, &candidate, &owner.candidate_name(), &canonical).await?;
        let record = advance(st, &key, &record, "validating", json!({}))?;
        let (mut connected, mut latency) = (false, None);
        if payload["start"] == true {
            set_running(&docker, &candidate, true).await?;
            initialize(&docker, &desired, &config).await?;
            if spec.runtime == "oss" {
                for attempt in 0..3 {
                    (connected, latency) = manager::probe(Some(&docker), &st.cfg, &desired).await;
                    if connected { break; }
                    if attempt < 2 { tokio::time::sleep(Duration::from_secs(2)).await; }
                }
                ensure!(connected, "新设置未通过原验证地址的 SOCKS 探活");
            }
        }
        let runtime = applied_runtime(&docker, &candidate, &owner.volume_name(), connected, latency).await?;
        ensure!(payload["start"] != true || runtime.status != "stopped", "候选容器在初始化时退出");
        journal::apply(&db, &key, &record, fields, &secrets, &runtime, !connected)?;
        Ok::<_, anyhow::Error>((candidate, !connected))
    }.await;
    match outcome {
        Ok((candidate, waiting)) => {
            if let Err(error) = set_policy(&docker, &candidate, policy(RestartPolicyNameEnum::UNLESS_STOPPED)).await {
                crate::ev!(warn,"replacement","cleanup_pending","通道已应用，重启策略待核对",{"cid":ch.id,"error":error.to_string()});
            }
            if !waiting {
                if let Err(error) = cleanup(st, &ch.id).await { crate::ev!(warn,"replacement","cleanup_pending","通道已应用，旧资源清理待重试",{"cid":ch.id,"error":error.to_string()}); }
            }
            Ok(())
        }
        Err(failure) => {
            // 提交响应不明先读回，不能把实际已提交的新配置再回滚一次。
            if journal::get(&db, &key, &ch.id)?.is_some_and(|r| matches!(r.phase.as_str(), "awaiting_login" | "committed")) { return Ok(()); }
            if let Err(rollback) = recover(st, &ch.id).await { return Err(anyhow!("替换失败，旧资源已保留但恢复尚未完成，请使用恢复上一次设置: {rollback}")); }
            Err(anyhow!("替换失败，已恢复上一次设置: {failure}"))
        }
    }
}

pub async fn recover(st: &AppState, cid: &str) -> Result<()> {
    let docker = st.docker().ok_or_else(|| anyhow!("docker unavailable"))?;
    let db = st.cfg.db_path(); let key = store::master_key(&st.cfg.data_dir)?;
    let Some(mut record) = journal::get(&db, &key, cid)? else { return Ok(()); };
    if matches!(record.phase.as_str(), "committed" | "rolled_back") { return cleanup(st, cid).await; }
    if record.phase != "rolling_back" {
        let applied = record.phase == "awaiting_login";
        record = advance(st, &key, &record, "rolling_back", json!({"config_applied":applied}))?;
    }
    let p = &record.payload; let owner = owner(&record, false); let canonical = format!("vpn-{cid}");
    if let Some(helper) = get(&docker, &owner.copy_name()).await? {
        ensure!(owner.owns(helper.config.as_ref().and_then(|c| c.labels.as_ref()), "copy"), "复制容器归属不匹配");
        remove(&docker, &helper).await?;
    }
    crate::novnc::drop_for(st, cid).await;
    if let Some(candidate) = owned(&docker, &owner, true).await? {
        set_policy(&docker, id(&candidate)?, policy(RestartPolicyNameEnum::NO)).await?;
        set_running(&docker, id(&candidate)?, false).await?;
        if candidate.name.as_deref() == Some(&format!("/{canonical}")) { rename(&docker, id(&candidate)?, &canonical, &owner.candidate_name()).await?; }
    }
    let old_id = text(p,"old_id")?; let old = identity(&docker, old_id).await?;
    let backup = format!("{canonical}-previous-{}", record.operation_id);
    ensure!(old.name.as_deref() == Some(&format!("/{canonical}")) || old.name.as_deref() == Some(&format!("/{backup}")), "旧容器名称已变化，拒绝自动恢复");
    let old_ch = changed(&store::get_channel(&db,cid)?.ok_or_else(|| anyhow!("通道不存在"))?, object(p,"old_fields")?);
    let restored_id = if running(&old) == Some(true) && old.name.as_deref() == Some(&format!("/{canonical}")) {
        old_id.to_string()
    } else if p["old_running"] == false {
        set_running(&docker, old_id, false).await?;
        rename(&docker, old_id, &backup, &canonical).await?;
        old_id.to_string()
    } else {
        set_policy(&docker, old_id, policy(RestartPolicyNameEnum::NO)).await?;
        set_running(&docker, old_id, false).await?;
        rename(&docker, old_id, &canonical, &backup).await?;
        let recovery_owner = self::owner(&record, true);
        if let Some(partial) = owned(&docker, &recovery_owner, true).await? { remove(&docker, &partial).await?; }
        let mut plan = manager::channel_plan_with_image(st, &docker, &old_ch, old_ch.vnc_password.as_deref().unwrap_or_default(), Some(text(p,"old_image")?)).await?;
        plan.name = recovery_owner.candidate_name();
        plan.config.host_config.as_mut().ok_or_else(|| anyhow!("恢复计划缺少运行配置"))?.restart_policy = Some(policy(RestartPolicyNameEnum::NO));
        resources::use_volume(&mut plan, text(p,"old_volume")?)?;
        let restored_id = resources::create_candidate(&docker, &plan, &recovery_owner).await?;
        rename(&docker, &restored_id, &recovery_owner.candidate_name(), &canonical).await?;
        set_running(&docker, &restored_id, true).await?;
        initialize(&docker, &old_ch, object(p,"old_config")?).await?;
        restored_id
    };
    let runtime = applied_runtime(&docker, &restored_id, text(p,"old_volume")?, false, None).await?;
    let secrets: Vec<String> = serde_json::from_value(p["secrets"].clone())?;
    journal::restore(&db, Some(&key), &record, if p["config_applied"] == true { Some(object(p,"old_fields")?) } else { None }, &secrets, &runtime)?;
    set_policy(&docker, &restored_id, old_policy(&record)?).await?;
    cleanup(st,cid).await
}

pub async fn cleanup(st: &AppState, cid: &str) -> Result<()> {
    let docker = st.docker().ok_or_else(|| anyhow!("docker unavailable"))?;
    let db = st.cfg.db_path(); let key = store::master_key(&st.cfg.data_dir)?;
    let Some(record) = journal::get(&db,&key,cid)? else { return Ok(()); };
    if !matches!(record.phase.as_str(), "committed" | "rolled_back") { return Ok(()); }
    let ch = store::get_channel(&db,cid)?.ok_or_else(|| anyhow!("通道不存在"))?;
    let current = identity(&docker, ch.container_id.as_deref().ok_or_else(|| anyhow!("当前容器 ID 缺失"))?).await?;
    let p = &record.payload; let owner = owner(&record,false);
    let selected = journal::data_volume(&db,cid)?;
    validate_source(&current,cid,&selected)?;
    set_policy(&docker, id(&current)?, if record.phase == "committed" { policy(RestartPolicyNameEnum::UNLESS_STOPPED) } else { old_policy(&record)? }).await?;
    if let Some(old) = get(&docker,text(p,"old_id")?).await? {
        if old.id != current.id {
            ensure!(old.name.as_deref() == Some(&format!("/vpn-{cid}-previous-{}",record.operation_id)), "旧容器名称已变化");
            ensure!(running(&old) == Some(false), "旧容器仍在运行");
            remove(&docker,&old).await?;
        }
    }
    for candidate_owner in [&owner, &self::owner(&record,true)] {
        if let Some(extra) = owned(&docker,candidate_owner,false).await? {
            if extra.id != current.id { remove(&docker,&extra).await?; }
        }
    }
    if record.phase == "committed" { remove_volume(&docker,text(p,"old_volume")?,None).await?; }
    else { remove_volume(&docker,&owner.volume_name(),Some(&owner)).await?; }
    journal::finish(&db,cid,&record.operation_id)
}

/// 仅从同代次真实成功探活调用；不在探活提交锁内执行 Docker 清理。
pub fn confirmed(st: &AppState, cid: &str) -> Result<()> {
    let db=st.cfg.db_path(); let key=store::master_key(&st.cfg.data_dir)?;
    if let Some(record)=journal::get(&db,&key,cid)? {
        if record.phase=="awaiting_login" { journal::confirm(&db,&record)?; }
    }
    Ok(())
}

pub async fn recover_all(st: &AppState) {
    let records = (|| { let key=store::master_key(&st.cfg.data_dir)?; journal::list(&st.cfg.db_path(),&key) })();
    let records = match records { Ok(r)=>r, Err(error)=> { crate::ev!(error,"replacement","recovery_failed","替换进度无法读取",{"error":error.to_string()}); return; } };
    for record in records {
        let Ok(_guard)=st.lifecycle.mutate(&record.channel_id).await else { return; };
        if record.phase=="awaiting_login" { continue; }
        if let Err(error)=recover(st,&record.channel_id).await {
            crate::ev!(warn,"replacement","recovery_pending","替换操作恢复未完成，保留记录与数据",{"cid":record.channel_id,"error":error.to_string()});
        }
    }
}
