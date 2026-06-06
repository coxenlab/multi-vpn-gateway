//! Runtime 家族:把 AdapterSpec 合成 bollard 容器配置。对照 app/adapters.py。纯函数。
use anyhow::{anyhow, Result};
use bollard::container::Config;
use bollard::models::{DeviceMapping, HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
use std::collections::HashMap;
use crate::registry::AdapterSpec;

/// build_run_kwargs 的产物:容器名 + bollard 配置(Phase 4 manager 拿去 create_container)。
#[derive(Debug, Clone)]
pub struct ContainerPlan {
    pub name: String,
    pub config: Config<String>,
}

const DEFAULT_VERSION: &str = "7.6.3";

/// 占位替换:{mac}/{vnc_password}/{version}(对照 _ctx + str.format)。
fn apply_ctx(s: &str, mac: &str, vnc_pwd: &str, version: &str) -> String {
    s.replace("{mac}", mac)
        .replace("{vnc_password}", vnc_pwd)
        .replace("{version}", version)
}

/// "src:dst:perm" → DeviceMapping。
fn parse_device(s: &str) -> DeviceMapping {
    let mut it = s.splitn(3, ':');
    let host = it.next().unwrap_or("").to_string();
    let cont = it.next().map(String::from).unwrap_or_else(|| host.clone());
    let perm = it.next().unwrap_or("rwm").to_string();
    DeviceMapping {
        path_on_host: Some(host),
        path_in_container: Some(cont),
        cgroup_permissions: Some(perm),
    }
}

/// 共有 HostConfig:caps/devices/sysctls/network/restart(unless-stopped)/device_cgroup_rules。
fn base_host_config(spec: &AdapterSpec, vpn_net: &str) -> HostConfig {
    HostConfig {
        cap_add: Some(spec.caps.clone()),
        devices: Some(spec.devices.iter().map(|d| parse_device(d)).collect()),
        sysctls: if spec.sysctls.is_empty() {
            None
        } else {
            Some(spec.sysctls.clone().into_iter().collect())
        },
        network_mode: Some(vpn_net.to_string()),
        restart_policy: Some(RestartPolicy {
            name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
            maximum_retry_count: None,
        }),
        device_cgroup_rules: if spec.device_cgroup_rules.is_empty() {
            None
        } else {
            Some(spec.device_cgroup_rules.clone())
        },
        ..Default::default()
    }
}

/// noVNC 8080 → 127.0.0.1 随机高位(命门 #4)。host_port="" 让 Docker 自选。
fn novnc_port_bindings() -> HashMap<String, Option<Vec<PortBinding>>> {
    let mut m = HashMap::new();
    m.insert(
        "8080/tcp".to_string(),
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()),
            host_port: Some(String::new()),
        }]),
    );
    m
}

fn exposed_8080() -> HashMap<String, HashMap<(), ()>> {
    let mut m = HashMap::new();
    m.insert("8080/tcp".to_string(), HashMap::new());
    m
}

fn env_vec(map: &indexmap::IndexMap<String, String>) -> Vec<String> {
    map.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// 对照 _build_hagb:EC/aTrust。镜像/env 模板化;空 ec_ver 省略 EC_VER;noVNC 8080 绑 127.0.0.1;卷 /root。
fn build_hagb(id: &str, mac: &str, ec_ver: Option<&str>, spec: &AdapterSpec, vnc_pwd: &str, vpn_net: &str) -> ContainerPlan {
    let has_ver = ec_ver.map(|s| !s.is_empty()).unwrap_or(false);
    let version = if has_ver { ec_ver.unwrap() } else { DEFAULT_VERSION };
    let mut env: indexmap::IndexMap<String, String> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), apply_ctx(v, mac, vnc_pwd, version)))
        .collect();
    if env.contains_key("EC_VER") && !has_ver {
        env.shift_remove("EC_VER");
    }
    let mut host = base_host_config(spec, vpn_net);
    host.port_bindings = Some(novnc_port_bindings());
    host.binds = Some(vec![format!("vpndata-{id}:/root")]);
    let config = Config {
        image: Some(apply_ctx(&spec.image, mac, vnc_pwd, version)),
        hostname: Some(id.to_string()),
        env: Some(env_vec(&env)),
        exposed_ports: Some(exposed_8080()),
        host_config: Some(host),
        ..Default::default()
    };
    ContainerPlan { name: format!("vpn-{id}"), config }
}

/// 按 runtime 分派(对照 build_run_kwargs)。oss/byo 在后续 task 接上。
pub fn build_run_kwargs(
    id: &str,
    mac: &str,
    ec_ver: Option<&str>,
    spec: &AdapterSpec,
    vnc_pwd: &str,
    vpn_net: &str,
) -> Result<ContainerPlan> {
    match spec.runtime.as_str() {
        "hagb" => Ok(build_hagb(id, mac, ec_ver, spec, vnc_pwd, vpn_net)),
        other => Err(anyhow!("unsupported runtime: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry;

    fn hc(p: &ContainerPlan) -> &HostConfig {
        p.config.host_config.as_ref().unwrap()
    }

    #[test]
    fn hagb_easyconnect_shape() {
        let spec = registry::get("easyconnect").unwrap();
        let p = build_run_kwargs("abc", "02:aa:bb:cc:dd:ee", Some("7.6.7"), &spec, "vncpw", "vpnnet").unwrap();
        assert_eq!(p.name, "vpn-abc");
        assert_eq!(p.config.image.as_deref(), Some("hagb/docker-easyconnect:7.6.7"));
        assert_eq!(p.config.hostname.as_deref(), Some("abc"));
        let h = hc(&p);
        assert_eq!(h.cap_add, Some(vec!["NET_ADMIN".to_string()]));
        assert_eq!(h.devices.as_ref().unwrap()[0].path_in_container.as_deref(), Some("/dev/net/tun"));
        let pb = h.port_bindings.as_ref().unwrap().get("8080/tcp").unwrap().as_ref().unwrap();
        assert_eq!(pb[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(pb[0].host_port.as_deref(), Some(""));
        assert_eq!(h.binds, Some(vec!["vpndata-abc:/root".to_string()]));
        assert_eq!(h.network_mode.as_deref(), Some("vpnnet"));
        assert_eq!(h.restart_policy.as_ref().unwrap().name, Some(RestartPolicyNameEnum::UNLESS_STOPPED));
        let env = p.config.env.as_ref().unwrap();
        assert!(env.contains(&"PASSWORD=vncpw".to_string()));
        assert!(env.contains(&"FAKE_HWADDR=02:aa:bb:cc:dd:ee".to_string()));
        assert!(env.contains(&"EC_VER=7.6.7".to_string()));
        assert!(h.sysctls.is_none());
    }

    #[test]
    fn hagb_atrust_sysctl_and_no_ec_ver() {
        let spec = registry::get("atrust").unwrap();
        let p = build_run_kwargs("xy", "02:00:00:00:00:01", None, &spec, "vp", "net").unwrap();
        assert_eq!(p.config.image.as_deref(), Some("hagb/docker-atrust:latest"));
        let h = hc(&p);
        assert_eq!(h.sysctls.as_ref().unwrap().get("net.ipv4.conf.default.route_localnet").map(String::as_str), Some("1"));
        let env = p.config.env.as_ref().unwrap();
        assert!(env.iter().all(|e| !e.starts_with("EC_VER=")));
    }
}
