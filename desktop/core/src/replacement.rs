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

/// 调用前已拿通道变更锁。没有旧实例时仍使用候选和持久化记录。
pub async fn replace(st: &AppState, ch: &ChannelPublic, fields: &Map<String, Value>, force_start: bool) -> Result<()> {
    let docker = st.docker().ok_or_else(|| anyhow!("docker unavailable"))?;
    let db = st.cfg.db_path(); let key = store::master_key(&st.cfg.data_dir)?;
    let queued = journal::get(&db, &key, &ch.id)?;
    ensure!(queued.as_ref().is_none_or(|r| r.phase == "queued"), "上一次修改尚未验证，请先完成登录或恢复上一次设置");
    let mut merged = queued.as_ref().and_then(|r| r.payload["fields"].as_object()).cloned().unwrap_or_default();
    merged.extend(fields.clone());
    let fields = &merged;
    let spec = registry::get(&ch.vpn_type)?;
    ensure!(spec.runtime != "byo" || ch.container_id.is_none(), "自装客户端的安装在原容器中，不能通过重建恢复或修改连接参数");
    let old = match ch.container_id.as_deref() { Some(old_id) => get(&docker, old_id).await?, None => None };
    let volume = journal::data_volume(&db, &ch.id)?;
    if old.is_none() && ch.container_id.is_some() { store::set_status(&db, &ch.id, "error")?; }
    if let Some(old) = old.as_ref() {
        ensure!(old.id == ch.container_id, "旧容器 ID 与通道不匹配");
        validate_source(old, &ch.id, &volume)?;
    } else { ensure!(get(&docker, &format!("vpn-{}", ch.id)).await?.is_none(), "正式容器名已被外部占用，保留现有资源"); }
    let source_exists = match docker.inspect_volume(&volume).await {
        Ok(_) => true, Err(e) if crate::docker::is_not_found(&e) => false, Err(e) => return Err(e.into()),
    };
    ensure!(old.is_none() || source_exists, "旧数据卷不存在，保留原实例");
    let desired = changed(ch, fields);
    let mut plan = manager::channel_plan(st, &docker, &desired, ch.vnc_password.as_deref().unwrap_or_default()).await?;
    let selected = docker.inspect_image(plan.config.image.as_deref().ok_or_else(|| anyhow!("候选镜像缺失"))?).await?;
    plan.config.image = Some(selected.id.ok_or_else(|| anyhow!("候选镜像缺少固定 ID"))?);
    let helper_image = if source_exists { Some(docker.inspect_image("vpnmgr/oss-vpn:latest").await?.id.ok_or_else(|| anyhow!("复制镜像缺少固定 ID"))?) } else { None };
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
        "kind":if old.is_some(){"replacement"}else{"initial"},"source_exists":source_exists,
        "old_id":old.as_ref().and_then(|o|o.id.as_deref()),"old_volume":volume,"old_image":old.as_ref().and_then(|o|o.image.as_deref()),"new_image":plan.config.image,
        "old_running":old.as_ref().is_some_and(|o|running(o)==Some(true)),"old_policy":old.as_ref().and_then(|o|o.host_config.as_ref()).and_then(|h| h.restart_policy.clone()).unwrap_or_else(|| policy(RestartPolicyNameEnum::NO)),
        "start":force_start || ch.status != "stopped","config_applied":false});
    journal::begin_after_queue(&db, &key, &ch.id, &operation, &payload, queued.as_ref())?;
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
        let canonical = format!("vpn-{}", ch.id);
        if let Some(old) = old.as_ref() {
            let old_id = id(old)?;
            set_policy(&docker, old_id, policy(RestartPolicyNameEnum::NO)).await?;
            set_running(&docker, old_id, false).await?;
            resources::copy_volume(&docker, &owner, old_id, &volume, helper_image.as_deref().ok_or_else(||anyhow!("复制镜像缺失"))?).await?;
            rename(&docker, old_id, &canonical, &format!("{canonical}-previous-{operation}")).await?;
        } else {
            ensure!(get(&docker, &canonical).await?.is_none(), "正式容器名已被外部占用");
            if let Some(helper_image) = helper_image.as_deref() { resources::copy_unattached_volume(&docker, &owner, &volume, helper_image).await?; }
        }
        rename(&docker, &candidate, &owner.candidate_name(), &canonical).await?;
        let record = advance(st, &key, &record, "validating", json!({}))?;
        let (mut connected, mut latency) = (false, None);
        if payload["start"] == true {
            set_running(&docker, &candidate, true).await?;
            initialize(&docker, &desired, &config).await?;
            if spec.runtime == "oss" || desired.login_method == "headless" {
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
        let waiting = !connected && (old.is_some() || source_exists || !fields.is_empty());
        journal::apply(&db, &key, &record, fields, &secrets, &runtime, waiting)?;
        Ok::<_, anyhow::Error>((candidate, waiting))
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
    ensure!(record.phase != "deleting", "通道删除已开始，请重试删除");
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
    if p["kind"] == "initial" {
        ensure!(get(&docker, &canonical).await?.is_none(), "正式容器名已被外部占用，保留所有资源");
        if let Some(candidate) = owned(&docker, &owner, false).await? { remove(&docker, &candidate).await?; }
        let secrets: Vec<String> = serde_json::from_value(p["secrets"].clone())?;
        journal::restore_absent(&db, &key, &record, if p["config_applied"] == true { Some(object(p,"old_fields")?) } else { None }, &secrets, text(p,"old_volume")?, p["start"] == false)?;
        return cleanup(st, cid).await;
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
    if record.payload["kind"] == "initial" && record.phase == "rolled_back" {
        ensure!(ch.container_id.is_none(), "无旧实例恢复后出现未知运行实例");
        let owner = owner(&record, false);
        ensure!(get(&docker, &format!("vpn-{cid}")).await?.is_none(), "正式容器名已被外部占用");
        if let Some(extra) = owned(&docker, &owner, false).await? { remove(&docker, &extra).await?; }
        remove_volume(&docker, &owner.volume_name(), Some(&owner)).await?;
        return journal::finish(&db, cid, &record.operation_id);
    }
    let current = identity(&docker, ch.container_id.as_deref().ok_or_else(|| anyhow!("当前容器 ID 缺失"))?).await?;
    let p = &record.payload; let owner = owner(&record,false);
    let selected = journal::data_volume(&db,cid)?;
    validate_source(&current,cid,&selected)?;
    set_policy(&docker, id(&current)?, if record.phase == "committed" { policy(RestartPolicyNameEnum::UNLESS_STOPPED) } else { old_policy(&record)? }).await?;
    if let Some(old_id) = p["old_id"].as_str() {
      if let Some(old) = get(&docker,old_id).await? {
        if old.id != current.id {
            ensure!(old.name.as_deref() == Some(&format!("/vpn-{cid}-previous-{}",record.operation_id)), "旧容器名称已变化");
            ensure!(running(&old) == Some(false), "旧容器仍在运行");
            remove(&docker,&old).await?;
        }
      }
    }
    for candidate_owner in [&owner, &self::owner(&record,true)] {
        if let Some(extra) = owned(&docker,candidate_owner,false).await? {
            if extra.id != current.id { remove(&docker,&extra).await?; }
        }
    }
    if record.phase == "committed" {
        if p["source_exists"] != false { remove_volume(&docker,text(p,"old_volume")?,None).await?; }
    }
    else { remove_volume(&docker,&owner.volume_name(),Some(&owner)).await?; }
    journal::finish(&db,cid,&record.operation_id)
}

/// 仅从同代次真实成功探活调用；不在探活提交锁内执行 Docker 清理。
pub fn confirmed(st: &AppState, cid: &str) -> Result<()> {
    let db=st.cfg.db_path(); let key=store::master_key(&st.cfg.data_dir)?;
    if let Some(record)=journal::get(&db,&key,cid)? {
        if record.phase=="awaiting_login" {
            ensure!(store::get_channel(&db,cid)?.and_then(|ch|ch.container_id).as_deref()==Some(text(&record.payload,"new_id")?), "探活容器与候选代次不匹配");
            journal::confirm(&db,&record)?;
        }
    }
    Ok(())
}

pub async fn recover_all(st: &AppState) {
    let records = (|| { let key=store::master_key(&st.cfg.data_dir)?; journal::list(&st.cfg.db_path(),&key) })();
    let records = match records { Ok(r)=>r, Err(error)=> { crate::ev!(error,"replacement","recovery_failed","替换进度无法读取",{"error":error.to_string()}); return; } };
    let had_records = records.iter().any(|record| record.phase != "queued");
    // 保存不等于应用；等用户明确启动该通道。
    for record in records.into_iter().filter(|record| record.phase != "queued") {
        let Ok(_guard)=st.lifecycle.mutate(&record.channel_id).await else { return; };
        let result = match record.phase.as_str() {
            "awaiting_login" => reconcile_waiting(st,&record.channel_id).await,
            "deleting" => discard(st,&record.channel_id).await.map(|_|()),
            _ => recover(st,&record.channel_id).await,
        };
        if let Err(error)=result {
            crate::ev!(warn,"replacement","recovery_pending","替换操作恢复未完成，保留记录与数据",{"cid":record.channel_id,"error":error.to_string()});
        }
    }
    if had_records { let _ = manager::rebuild(&st.cfg, st.docker().as_ref(), &st.cfg.db_path()).await; }
}

pub async fn reconcile_waiting(st: &AppState, cid: &str) -> Result<()> {
    let docker=st.docker().ok_or_else(||anyhow!("docker unavailable"))?;
    let db=st.cfg.db_path();let key=store::master_key(&st.cfg.data_dir)?;
    let Some(record)=journal::get(&db,&key,cid)? else { return Ok(()); };
    if record.phase!="awaiting_login" {return Ok(());}
    let owner=owner(&record,false);
    let info=owned(&docker,&owner,true).await?.ok_or_else(||anyhow!("待登录候选不存在，请恢复上一次设置"))?;
    rename(&docker,id(&info)?,&owner.candidate_name(),&format!("vpn-{cid}")).await?;
    ensure!(info.mounts.as_ref().is_some_and(|m|m.iter().any(|m|m.name.as_deref()==Some(&owner.volume_name()))),"候选数据卷已变化");
    let record=advance(st,&key,&record,"awaiting_login",json!({"new_id":id(&info)?}))?;
    journal::resumed(&db,&record,&applied_runtime(&docker,id(&info)?,&owner.volume_name(),false,None).await?)?;
    if running(&info)==Some(true) {set_policy(&docker,id(&info)?,policy(RestartPolicyNameEnum::UNLESS_STOPPED)).await?;}
    Ok(())
}

/// 等待登录期间启动新配置；保留旧备份，不把启动解释为丢弃新设置。
pub async fn resume(st: &AppState,cid:&str)->Result<bool> {
    let docker=st.docker().ok_or_else(||anyhow!("docker unavailable"))?;
    let db=st.cfg.db_path();let key=store::master_key(&st.cfg.data_dir)?;
    let Some(record)=journal::get(&db,&key,cid)? else {return Ok(false);};
    if record.phase == "queued" {
        let ch=store::get_channel(&db,cid)?.ok_or_else(||anyhow!("通道不存在"))?;
        replace(st,&ch,&Map::new(),true).await?;
        return Ok(true);
    }
    if matches!(record.phase.as_str(),"committed"|"rolled_back") {cleanup(st,cid).await?;return Ok(false);}
    ensure!(record.phase=="awaiting_login","上次操作尚未恢复，请先恢复上一次设置");
    let owner=owner(&record,false);let ch=store::get_channel(&db,cid)?.ok_or_else(||anyhow!("通道不存在"))?;
    let mut current=owned(&docker,&owner,true).await?;
    if current.as_ref().is_some_and(|c|running(c)==Some(true)) {reconcile_waiting(st,cid).await?;return Ok(true);}
    let volume=docker.inspect_volume(&owner.volume_name()).await?;
    ensure!(owner.owns(Some(&volume.labels),"candidate"),"候选数据卷归属已变化");
    if current.as_ref().is_none_or(|c|c.state.as_ref().and_then(|s|s.status)!=Some(bollard::models::ContainerStateStatusEnum::CREATED)) {
        let image=current.as_ref().and_then(|c|c.image.as_deref()).or_else(||record.payload["new_image"].as_str()).ok_or_else(||anyhow!("待登录候选缺少固定镜像，请恢复上一次设置"))?;
        let mut plan=manager::channel_plan_with_image(st,&docker,&ch,ch.vnc_password.as_deref().unwrap_or_default(),Some(image)).await?;
        plan.name=owner.candidate_name();
        plan.config.host_config.as_mut().ok_or_else(||anyhow!("候选缺少运行配置"))?.restart_policy=Some(policy(RestartPolicyNameEnum::NO));
        resources::use_volume(&mut plan,&owner.volume_name())?;
        if let Some(info)=current.as_ref() {remove(&docker,info).await?;}
        let created=resources::create_candidate(&docker,&plan,&owner).await?;
        current=Some(identity(&docker,&created).await?);
    }
    let current=current.ok_or_else(||anyhow!("候选创建未确认"))?;
    let record=advance(st,&key,&record,"awaiting_login",json!({"new_id":id(&current)?}))?;
    rename(&docker,id(&current)?,&owner.candidate_name(),&format!("vpn-{cid}")).await?;
    set_running(&docker,id(&current)?,true).await?;
    initialize(&docker,&ch,object(&record.payload,"config")?).await?;
    let runtime=applied_runtime(&docker,id(&current)?,&owner.volume_name(),false,None).await?;
    ensure!(runtime.status!="stopped","候选启动失败，旧资源已保留，可恢复上一次设置");
    journal::resumed(&db,&record,&runtime)?;
    set_policy(&docker,id(&current)?,policy(RestartPolicyNameEnum::UNLESS_STOPPED)).await?;
    Ok(true)
}

/// 停止未完成操作时保留停用意图，不为恢复短暂登录旧客户端。
pub async fn before_stop(st:&AppState,cid:&str)->Result<()> {
    let db=st.cfg.db_path();let key=store::master_key(&st.cfg.data_dir)?;
    let Some(record)=journal::get(&db,&key,cid)? else {return Ok(());};
    if record.phase == "queued" { return Ok(()); }
    if record.phase=="awaiting_login" {return reconcile_waiting(st,cid).await;}
    ensure!(record.phase!="deleting","通道删除已开始，请重试删除");
    if matches!(record.phase.as_str(),"committed"|"rolled_back") {return cleanup(st,cid).await;}
    if record.payload["kind"] == "initial" {
        advance(st,&key,&record,&record.phase,json!({"start":false}))?;
        return recover(st,cid).await;
    }
    let docker=st.docker().ok_or_else(||anyhow!("docker unavailable"))?;
    let old_id=text(&record.payload,"old_id")?;
    let old=identity(&docker,old_id).await?;
    ensure!(old.name.as_deref()==Some(&format!("/vpn-{cid}")) || old.name.as_deref()==Some(&format!("/vpn-{cid}-previous-{}",record.operation_id)),"旧容器名称已变化");
    ensure!(old.mounts.as_ref().is_some_and(|m|m.iter().any(|m|m.name.as_deref()==record.payload["old_volume"].as_str())),"旧数据卷已变化");
    advance(st,&key,&record,&record.phase,json!({"old_running":false,"start":false}))?;
    set_policy(&docker,old_id,policy(RestartPolicyNameEnum::NO)).await?;
    set_running(&docker,old_id,false).await?;
    recover(st,cid).await
}

/// 用户删除通道时确认关联实例全部移除；命名卷与既有删除策略一致，保留。
pub async fn discard(st:&AppState,cid:&str)->Result<bool> {
    let docker=st.docker().ok_or_else(||anyhow!("docker unavailable"))?;
    let db=st.cfg.db_path();let key=store::master_key(&st.cfg.data_dir)?;
    let Some(mut record)=journal::get(&db,&key,cid)? else {return Ok(false);};
    if record.phase!="deleting" {
        journal::request_delete(&db,&record)?;
        record=journal::get(&db,&key,cid)?.ok_or_else(||anyhow!("删除记录丢失"))?;
    }
    let owner=owner(&record,false);let canonical=format!("vpn-{cid}");let mut containers=HashMap::new();
    if let Some(old_id)=record.payload["old_id"].as_str() {
      if let Some(old)=get(&docker,old_id).await? {
        ensure!(old.name.as_deref()==Some(&format!("/{canonical}")) || old.name.as_deref()==Some(&format!("/{canonical}-previous-{}",record.operation_id)),"旧容器名称已变化");
        ensure!(old.mounts.as_ref().is_some_and(|m|m.iter().any(|m|m.name.as_deref()==record.payload["old_volume"].as_str())),"旧数据卷已变化");
        containers.insert(id(&old)?.to_string(),old);
      }
    }
    for candidate_owner in [&owner,&self::owner(&record,true)] {
        if let Some(info)=owned(&docker,candidate_owner,true).await? {containers.insert(id(&info)?.to_string(),info);}
    }
    if let Some(current)=get(&docker,&canonical).await? {ensure!(containers.contains_key(id(&current)?),"正式容器名已被外部占用");}
    if let Some(helper)=get(&docker,&owner.copy_name()).await? {
        ensure!(owner.owns(helper.config.as_ref().and_then(|c|c.labels.as_ref()),"copy"),"复制容器归属不匹配");
        remove(&docker,&helper).await?;
    }
    crate::novnc::drop_for(st,cid).await;
    for info in containers.values() {
        set_policy(&docker,id(info)?,policy(RestartPolicyNameEnum::NO)).await?;
        remove(&docker,info).await?;
    }
    journal::deleted(&db,&record)?;
    Ok(true)
}
