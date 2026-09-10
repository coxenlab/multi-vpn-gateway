//! 托管规则/代理读回与串行应用。确认不代表 DNS、TUN 或企业 VPN 健康。
use std::{collections::BTreeSet, path::Path, time::Duration};
use anyhow::{anyhow, bail, ensure, Result};
use serde_json::Value;
use serde_yaml::Value as Yaml;
use sha2::{Digest, Sha256};
use crate::{config::Config, config_apply_store as journal};

const PREFIX: &str = "__vpnmgr_cfg_";

/// 与 Python 的有类型/长度编码一致；浮点用 IEEE bytes，避免 JSON 指数格式差异。
pub fn fingerprint(value: &Yaml) -> Result<String> {
    fn encode(v: &Yaml, output: &mut Vec<u8>) -> Result<()> {
        match v {
            Yaml::Null => output.push(b'n'),
            Yaml::Bool(v) => output.push(if *v { b't' } else { b'f' }),
            Yaml::Number(v) => {
                if let Some(i) = v.as_i64() { output.extend(format!("i{i};").as_bytes()); }
                else if let Some(u) = v.as_u64() { output.extend(format!("i{u};").as_bytes()); }
                else {
                    let f = v.as_f64().filter(|f| f.is_finite()).ok_or_else(|| anyhow!("配置数字无效"))?;
                    output.push(b'd'); output.extend(f.to_be_bytes());
                }
            }
            Yaml::String(s) => { output.extend(format!("s{}:", s.len()).as_bytes()); output.extend(s.as_bytes()); }
            Yaml::Sequence(items) => {
                output.extend(format!("a{}:", items.len()).as_bytes());
                for item in items { encode(item, output)?; }
            }
            Yaml::Mapping(map) => {
                let mut keys = map.keys().map(|k| k.as_str().ok_or_else(|| anyhow!("配置键必须是文本"))).collect::<Result<Vec<_>>>()?;
                keys.sort_unstable();
                output.extend(format!("o{}:", keys.len()).as_bytes());
                for key in keys { encode(&Yaml::String(key.into()), output)?; encode(&map[Yaml::String(key.into())], output)?; }
            }
            _ => bail!("配置类型不支持"),
        }
        Ok(())
    }
    let mut bytes = b"vpnmgr-config-v1\0".to_vec();
    encode(value, &mut bytes)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

pub fn marker(ticket: &journal::Ticket) -> String { format!("{PREFIX}{}_{}", ticket.generation, ticket.digest) }

pub fn stamp(config: &Yaml, ticket: &journal::Ticket) -> Result<Yaml> {
    let mut value = config.clone();
    value["proxies"].as_sequence_mut().ok_or_else(|| anyhow!("代理配置无效"))?
        .push(serde_yaml::to_value(serde_json::json!({"name":marker(ticket),"type":"direct"}))?);
    Ok(value)
}

pub fn matches(config: &Yaml, ticket: &journal::Ticket, proxies: &Value, rules: &Value, general: &Value) -> bool {
    let Some(entries) = proxies["proxies"].as_object() else { return false };
    let Some(wanted_proxies) = config["proxies"].as_sequence() else { return false };
    let expected: BTreeSet<&str> = wanted_proxies.iter().filter_map(|p| p["name"].as_str()).collect();
    let actual: BTreeSet<&str> = entries.keys().filter(|s| s.starts_with("ch-")).map(String::as_str).collect();
    let marks: Vec<&String> = entries.keys().filter(|s| s.starts_with(PREFIX)).collect();
    let mark = marker(ticket);
    if actual != expected || marks != vec![&mark] || entries[&mark]["type"] != "Direct" { return false; }
    if expected.iter().any(|name| entries[*name]["type"] != "Socks5") { return false; }
    if general["mode"].as_str() != Some(config["mode"].as_str().unwrap_or("rule")) { return false; }
    let Some(wanted_rules) = config["rules"].as_sequence() else { return false };
    let Some(live_rules) = rules["rules"].as_array() else { return false };
    if live_rules.len() != wanted_rules.len() { return false; }
    for (wanted, live) in wanted_rules.iter().zip(live_rules) {
        let Some(text) = wanted.as_str() else { return false };
        let parts: Vec<&str> = text.split(',').collect();
        let (kind, payload, proxy) = match parts.as_slice() {
            ["MATCH", proxy] => ("Match", "", *proxy),
            ["DOMAIN-SUFFIX", payload, proxy] => ("DomainSuffix", *payload, *proxy),
            ["IP-CIDR", payload, proxy, "no-resolve"] => ("IPCIDR", *payload, *proxy),
            _ => return false,
        };
        if live["type"] != kind || live["payload"] != payload || live["proxy"] != proxy
            || live["extra"]["disabled"].as_bool().unwrap_or(false) { return false; }
    }
    true
}

pub async fn application_lock(db: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let path = db.parent().unwrap_or_else(|| Path::new(".")).join("mihomo-apply.lock");
    let file = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).mode(0o600).open(path)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if tokio::time::Instant::now() < deadline => tokio::time::sleep(Duration::from_millis(50)).await,
            _ => bail!("配置应用仍在处理中或锁不可用"),
        }
    }
}

async fn get(client: &reqwest::Client, cfg: &Config, path: &str) -> Result<Value> {
    Ok(client.get(format!("{}/{path}", cfg.mihomo_ctrl_url)).bearer_auth(&cfg.mihomo_secret)
        .timeout(Duration::from_secs(3)).send().await?.error_for_status()?.json().await?)
}

pub async fn readback(config: &Yaml, ticket: &journal::Ticket, cfg: &Config) -> Result<bool> {
    let client = reqwest::Client::new();
    let proxies = get(&client, cfg, "proxies").await?;
    let rules = get(&client, cfg, "rules").await?;
    let general = get(&client, cfg, "configs").await?;
    // 应用先更新 proxies 再更新 rules；第二次代理读取拒绝跨切换拼出的版本。
    Ok(matches(config, ticket, &proxies, &rules, &general)
        && matches(config, ticket, &get(&client, cfg, "proxies").await?, &rules, &general))
}

async fn file_matches(docker: &bollard::Docker, name: &str, path: &str, expected: &[u8]) -> Result<bool> {
    use futures_util::StreamExt;
    use std::io::Read;
    let mut stream = docker.download_from_container(name, Some(bollard::container::DownloadFromContainerOptions { path }));
    let mut bytes = Vec::new();
    while let Some(part) = stream.next().await {
        bytes.extend(part?);
        ensure!(bytes.len() <= expected.len() + 1024 * 1024, "启动配置归档超出预期");
    }
    for entry in tar::Archive::new(bytes.as_slice()).entries()? {
        let mut entry = entry?;
        if entry.path()?.file_name() == Path::new(path).file_name() && entry.header().entry_type().is_file() {
            let mut content = Vec::new(); entry.read_to_end(&mut content)?;
            return Ok(content == expected && entry.header().mode()? & 0o777 == 0o600);
        }
    }
    Ok(false)
}

/// 0600 暂存文件完整投递后原子替换启动文件；写响应不明先读回，不重复覆盖。
pub async fn persist_container(docker: &bollard::Docker, name: &str, content: &[u8]) -> Result<()> {
    let pending = "/cfg/.vpnmgr-config.pending";
    let mut archive = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(content.len() as u64); header.set_mode(0o600); header.set_cksum();
    archive.append_data(&mut header, ".vpnmgr-config.pending", content)?;
    let put = docker.upload_to_container(name, Some(bollard::container::UploadToContainerOptions { path: "/cfg", ..Default::default() }),
        bytes::Bytes::from(archive.into_inner()?)).await;
    if put.is_err() && file_matches(docker, name, "/cfg/config.yaml", content).await.unwrap_or(false) { return Ok(()); }
    ensure!(file_matches(docker, name, pending, content).await.unwrap_or(false), "启动配置暂存未确认");
    // exec_capture 不保证非零退出码报错，故以最终文件内容/权限读回作为确认。
    let _ = crate::docker::exec_capture(docker, name, vec!["/bin/sh", "-c", "mv -f /cfg/.vpnmgr-config.pending /cfg/config.yaml"]).await;
    ensure!(file_matches(docker, name, "/cfg/config.yaml", content).await.unwrap_or(false), "启动配置替换未确认");
    Ok(())
}

pub async fn apply(cfg: &Config, docker: Option<&bollard::Docker>, db: &Path) -> Result<String> {
    let _lock = application_lock(db).await?;
    let path = crate::manager::mihomo_config_path();
    let base: Yaml = match std::fs::read_to_string(&path) {
        Ok(text) => serde_yaml::from_str(&text).map_err(|_| anyhow!("启动配置无法解析"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Yaml::Mapping(Default::default()),
        Err(_) => bail!("启动配置无法读取"),
    };
    let off = crate::store::routing_off(&cfg.data_dir);
    let previous = journal::status(db, off)?;
    let mut prepared = None;
    for _ in 0..8 {
        let snapshot = journal::snapshot(db, off)?;
        let config = crate::manager::build_mihomo_config(base.clone(), &snapshot.channels, &snapshot.rules);
        let digest = fingerprint(&config)?;
        if let Ok(ticket) = journal::prepare(db, snapshot.revision, off, &digest) { prepared = Some((config, ticket)); break; }
    }
    let (config, ticket) = prepared.ok_or_else(|| anyhow!("配置持续变化，已保存但尚未同步，请重试"))?;
    let stamped = stamp(&config, &ticket)?;
    let yaml = serde_yaml::to_string(&stamped)?;
    let mut verified = readback(&config, &ticket, cfg).await.unwrap_or(false);
    let needs_flush = verified && (previous.desired_revision != ticket.revision || previous.last_error.is_some());
    let mut reload_failed = false;
    if !verified {
        let response = reqwest::Client::new().put(format!("{}/configs", cfg.mihomo_ctrl_url)).query(&[("force", "true")])
            .bearer_auth(&cfg.mihomo_secret).json(&serde_json::json!({"payload":yaml})).timeout(Duration::from_secs(10)).send().await;
        reload_failed = !response.as_ref().is_ok_and(|r| r.status().is_success());
        verified = match readback(&config, &ticket, cfg).await {
            Ok(value) => value,
            Err(_) => { journal::failed(db, &ticket, "readback_failed")?; return Ok("配置已保存，但无法确认规则应用，请重试".into()); }
        };
    }
    if !verified {
        journal::failed(db, &ticket, if reload_failed { "reload_failed" } else { "readback_mismatch" })?;
        return Ok("配置已保存，但运行规则未匹配，请重试".into());
    }
    journal::observed(db, &ticket)?;
    if reload_failed {
        journal::failed(db, &ticket, "reload_failed")?;
        return Ok("规则已读回，但重载响应未确认，请重试".into());
    }
    if needs_flush {
        let response = reqwest::Client::new().post(format!("{}/cache/dns/flush", cfg.mihomo_ctrl_url))
            .bearer_auth(&cfg.mihomo_secret).timeout(Duration::from_secs(3)).send().await;
        if !response.as_ref().is_ok_and(|r| r.status().is_success()) {
            journal::failed(db, &ticket, "dns_flush_failed")?;
            return Ok("规则已读回，但通道地址缓存刷新未确认，请重试".into());
        }
    }
    let needs_delivery = base != stamped || previous.last_error.is_some() || previous.applied_hash != ticket.digest;
    if base != stamped && crate::manager::atomic_write_0600(Path::new(&path), yaml.as_bytes()).is_err() {
        journal::failed(db, &ticket, "write_failed")?;
        return Ok("运行规则已应用，但启动配置保存失败，请重试".into());
    }
    if let Some(docker) = docker.filter(|_| needs_delivery) {
        if !matches!(tokio::time::timeout(Duration::from_secs(15), persist_container(docker, crate::infra::MIHOMO_CONTAINER, yaml.as_bytes())).await, Ok(Ok(()))) {
            journal::failed(db, &ticket, "delivery_failed")?;
            return Ok("put_file failed: 运行规则已应用，但启动配置投递未确认，请重试".into());
        }
    }
    if needs_delivery || needs_flush {
        match readback(&config, &ticket, cfg).await {
            Ok(true) => {},
            Ok(false) => {
                journal::failed(db, &ticket, "readback_mismatch")?;
                return Ok("启动配置已保存，但内核配置已变化，请重试".into());
            }
            Err(_) => {
                journal::failed(db, &ticket, "readback_failed")?;
                return Ok("启动配置已保存，运行规则仍待重新确认，请重试".into());
            }
        }
    }
    journal::confirmed(db, &ticket)?;
    if journal::status(db, crate::store::routing_off(&cfg.data_dir))?.pending {
        return Ok("已有更新配置保存，尚未全部同步，请重试".into());
    }
    Ok("204".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fingerprint_is_order_independent_and_matches_python_scalar_encoding() {
        let value: Yaml = serde_yaml::from_str("中文: [null, true, -2, 9223372036854775808, 1.0e-7, 'x:y']\nz: {}\n").unwrap();
        assert_eq!(fingerprint(&value).unwrap(), "4fe6b8c4681c29fec703a233f7b83f15fee96a4ca69842dc7fbb89b2392a3c56");
        let reordered: Yaml = serde_yaml::from_str("z: {}\n中文: [null, true, -2, 9223372036854775808, 1.0e-7, 'x:y']\n").unwrap();
        assert_eq!(fingerprint(&value).unwrap(), fingerprint(&reordered).unwrap());
        assert_ne!(fingerprint(&serde_yaml::from_str("[1]").unwrap()).unwrap(), fingerprint(&serde_yaml::from_str("[1.0]").unwrap()).unwrap());
        assert!(fingerprint(&serde_yaml::from_str(".nan").unwrap()).is_err());
        assert!(fingerprint(&serde_yaml::from_str("{1: bad-key}").unwrap()).is_err());
    }

    #[test]
    fn readback_requires_marker_rule_order_proxy_type_mode_and_enabled_rules() {
        let ticket = journal::Ticket {revision:0, routing_off:false, generation:1, attempt:1, digest:"a".repeat(64)};
        let config: Yaml = serde_yaml::from_str("mode: rule\nproxies: [{name: ch-a}]\nrules: ['DOMAIN-SUFFIX,example.test,ch-a', 'MATCH,DIRECT']").unwrap();
        let proxies = json!({"proxies":{"ch-a":{"type":"Socks5"},marker(&ticket):{"type":"Direct"}}});
        let rules = json!({"rules":[{"type":"DomainSuffix","payload":"example.test","proxy":"ch-a"},{"type":"Match","payload":"","proxy":"DIRECT"}]});
        let general = json!({"mode":"rule"});
        assert!(matches(&config, &ticket, &proxies, &rules, &general));
        let mut changed = proxies.clone(); changed["proxies"]["ch-a"]["type"] = json!("Direct");
        assert!(!matches(&config, &ticket, &changed, &rules, &general));
        let mut changed = rules.clone(); changed["rules"].as_array_mut().unwrap().reverse();
        assert!(!matches(&config, &ticket, &proxies, &changed, &general));
        let mut changed = rules.clone(); changed["rules"][0]["extra"] = json!({"disabled":true});
        assert!(!matches(&config, &ticket, &proxies, &changed, &general));
        assert!(!matches(&config, &ticket, &proxies, &rules, &json!({"mode":"direct"})));
        let mut other = ticket.clone(); other.generation += 1;
        assert!(!matches(&config, &other, &proxies, &rules, &general));
        assert!(!matches(&config, &ticket, &Value::Null, &Value::Null, &Value::Null));
    }

    #[tokio::test]
    async fn application_lock_released_when_owner_is_cancelled() {
        let directory = tempfile::tempdir().unwrap();
        let db = directory.path().join("vpnmgr.db");
        let path = db.clone();
        let (send, ready) = tokio::sync::oneshot::channel();
        let owner = tokio::spawn(async move {
            let _lock = application_lock(&path).await.unwrap();
            send.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(30), application_lock(&db)).await.is_err());
        owner.abort(); let _ = owner.await;
        assert!(tokio::time::timeout(Duration::from_secs(1), application_lock(&db)).await.unwrap().is_ok());
    }
}
