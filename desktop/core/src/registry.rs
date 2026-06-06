//! 适配器注册表:编译期嵌入 adapters.yaml,提供 get / list_adapters / host_arch。对照 app/registry.py。
use anyhow::{anyhow, Result};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const MANIFEST_YAML: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/adapters.yaml"));

#[derive(Deserialize, Serialize, Clone, Debug)]
pub struct InputField {
    pub key: String,
    pub label: String,
    #[serde(rename = "type")]
    pub field_type: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub secret: bool,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AdapterSpec {
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub desc: Option<String>,
    pub runtime: String,
    pub image: String,
    #[serde(default)]
    pub versioned: bool,
    #[serde(default)]
    pub version_repo: Option<String>,
    #[serde(default)]
    pub fallback_versions: Vec<String>,
    #[serde(default)]
    pub arch: Vec<String>,
    #[serde(default)]
    pub login_modes: Vec<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub inputs: Vec<InputField>,
    #[serde(default)]
    pub env: IndexMap<String, String>,
    #[serde(default)]
    pub caps: Vec<String>,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub sysctls: HashMap<String, String>,
    #[serde(default)]
    pub device_cgroup_rules: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AdapterSummary {
    pub key: String,
    pub label: String,
    pub desc: String,
    pub runtime: String,
    pub versioned: bool,
    pub arch: Vec<String>,
    pub login_modes: Vec<String>,
    pub inputs: Vec<InputField>,
}

#[derive(Deserialize)]
struct Manifest {
    adapters: IndexMap<String, AdapterSpec>,
}

fn manifest() -> Result<IndexMap<String, AdapterSpec>> {
    let m: Manifest = serde_yaml::from_str(MANIFEST_YAML).map_err(|e| anyhow!("parse adapters.yaml: {e}"))?;
    Ok(m.adapters)
}

/// 对照 get:返回 spec 克隆;未知 key → Err。
pub fn get(key: &str) -> Result<AdapterSpec> {
    manifest()?.swap_remove(key).ok_or_else(|| anyhow!("unknown adapter: {key}"))
}

/// 对照 list_adapters:保序输出 UI 字段。
pub fn list_adapters() -> Result<Vec<AdapterSummary>> {
    let m = manifest()?;
    Ok(m.into_iter()
        .map(|(key, spec)| AdapterSummary {
            label: spec.label.clone().unwrap_or_else(|| key.clone()),
            desc: spec.desc.clone().unwrap_or_default(),
            runtime: spec.runtime.clone(),
            versioned: spec.versioned,
            arch: spec.arch.clone(),
            login_modes: spec.login_modes.clone(),
            inputs: spec.inputs.clone(),
            key,
        })
        .collect())
}

/// 对照 host_arch。
pub fn host_arch() -> String {
    match std::env::consts::ARCH {
        "x86_64" | "amd64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        _ => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_is_ordered() {
        let list = list_adapters().unwrap();
        assert_eq!(list[0].key, "easyconnect");
        assert_eq!(list.len(), 11);
        let ec = &list[0];
        assert_eq!(ec.runtime, "hagb");
        assert!(ec.versioned);
        assert!(ec.login_modes.contains(&"headless".to_string()));
        assert_eq!(ec.inputs.len(), 3);
        assert!(ec.inputs.iter().any(|i| i.key == "password" && i.secret));
    }

    #[test]
    fn get_known_and_unknown() {
        let oss = get("anyconnect").unwrap();
        assert_eq!(oss.runtime, "oss");
        assert_eq!(oss.protocol.as_deref(), Some("anyconnect"));
        assert_eq!(oss.image, "vpnmgr/oss-vpn:latest");
        assert_eq!(oss.caps, vec!["NET_ADMIN"]);
        assert_eq!(oss.sysctls.get("net.ipv4.ip_forward").map(String::as_str), Some("1"));

        let atrust = get("atrust").unwrap();
        assert!(!atrust.versioned);
        assert_eq!(atrust.sysctls.get("net.ipv4.conf.default.route_localnet").map(String::as_str), Some("1"));

        let forti = get("openfortivpn").unwrap();
        assert_eq!(forti.device_cgroup_rules, vec!["c 108:* rwm"]);
        assert!(forti.caps.contains(&"MKNOD".to_string()));

        assert!(get("nope").is_err());
    }

    #[test]
    fn host_arch_known() {
        let a = host_arch();
        assert!(a == "amd64" || a == "arm64" || a == "unknown");
    }
}
