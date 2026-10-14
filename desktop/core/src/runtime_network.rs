//! New runtime bridge: reserve .2 for DNS, allocate channels from the upper /25.
use anyhow::{anyhow, ensure, Result};
use bollard::{Docker, models::{Ipam, IpamConfig, Network}, network::{CreateNetworkOptions, ListNetworksOptions}};
use crate::config::Config;

const OWNER: &str = "com.tongdao.network-layout";
const VERSION: &str = "dns-v1";

fn layout(third: u8) -> IpamConfig {
    IpamConfig {
        subnet: Some(format!("172.30.{third}.0/24")),
        ip_range: Some(format!("172.30.{third}.128/25")),
        gateway: Some(format!("172.30.{third}.1")),
        ..Default::default()
    }
}

fn reserved_dns(network: &Network) -> Result<String> {
    ensure!(network.driver.as_deref() == Some("bridge") &&
        network.labels.as_ref().and_then(|l| l.get(OWNER)).map(String::as_str) == Some(VERSION),
        "现有运行环境网络不是本版本管理的固定 DNS 网络，请保留现有通道并选择新的运行环境网络");
    let configs = network.ipam.as_ref().and_then(|i| i.config.as_ref())
        .ok_or_else(|| anyhow!("运行环境网络缺少地址配置"))?;
    ensure!(configs.len() == 1, "运行环境网络地址配置已改变");
    let config = &configs[0];
    for third in 0..=255 {
        if config == &layout(third) { return Ok(format!("172.30.{third}.2")); }
    }
    Err(anyhow!("运行环境网络地址池已改变，不能保证 DNS 地址稳定"))
}

fn choose_layout(networks: &[Network]) -> Result<IpamConfig> {
    let occupied: Vec<ipnet::Ipv4Net> = networks.iter().filter_map(|n| n.ipam.as_ref())
        .filter_map(|i| i.config.as_ref()).flatten().filter_map(|i| i.subnet.as_ref())
        .filter_map(|s| s.parse().ok()).collect();
    // Prefer the high end, avoiding the common default Docker pools first.
    for third in (0..=255).rev() {
        let candidate = layout(third);
        let subnet: ipnet::Ipv4Net = candidate.subnet.as_ref().unwrap().parse()?;
        if !occupied.iter().any(|n| n.contains(&subnet.network()) || subnet.contains(&n.network())) {
            return Ok(candidate);
        }
    }
    Err(anyhow!("没有可用的独立运行环境网段"))
}

/// Never recreate an existing network: doing so would disconnect its channels.
pub async fn ensure(docker: &Docker, cfg: &Config) -> Result<Option<String>> {
    if cfg.identity().colima_home_rel.is_none() {
        crate::docker::create_bridge_network(docker, &cfg.vpn_net).await?;
        return Ok(None);
    }
    let networks = docker.list_networks(None::<ListNetworksOptions<String>>).await?;
    let matching: Vec<_> = networks.iter().filter(|n| n.name.as_deref() == Some(&cfg.vpn_net)).collect();
    ensure!(matching.len() <= 1, "存在多个同名运行环境网络");
    if let Some(network) = matching.first() { return reserved_dns(network).map(Some); }
    let config = choose_layout(&networks)?;
    docker.create_network(CreateNetworkOptions {
        name: cfg.vpn_net.clone(), driver: "bridge".to_string(), check_duplicate: true,
        ipam: Ipam { config: Some(vec![config]), ..Default::default() },
        labels: [(OWNER.to_string(), VERSION.to_string())].into_iter().collect(),
        ..Default::default()
    }).await?;
    let network = crate::docker::network_inspect_with_timeout(docker, &cfg.vpn_net,
        std::time::Duration::from_secs(10)).await?;
    reserved_dns(&network).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn network(config: IpamConfig) -> Network {
        Network { driver: Some("bridge".into()), labels: Some([(OWNER.into(), VERSION.into())].into_iter().collect()),
            ipam: Some(Ipam { config: Some(vec![config]), ..Default::default() }), ..Default::default() }
    }
    #[test]
    fn excludes_overlapping_pools_and_reserves_dns_outside_dynamic_range() {
        let used = network(IpamConfig { subnet: Some("172.30.254.0/23".into()), ..Default::default() });
        let chosen = choose_layout(&[used]).unwrap();
        assert_eq!(chosen, layout(253));
        let dns = reserved_dns(&network(chosen.clone())).unwrap();
        assert_eq!(dns, "172.30.253.2");
        assert!(!chosen.ip_range.unwrap().parse::<ipnet::Ipv4Net>().unwrap().contains(&dns.parse::<std::net::Ipv4Addr>().unwrap()));
        assert!(choose_layout(&[network(IpamConfig { subnet: Some("172.30.0.0/16".into()), ..Default::default() })]).is_err());
    }
    #[test]
    fn rejects_foreign_or_modified_network_without_migrating_it() {
        let mut n = network(layout(42));
        assert_eq!(reserved_dns(&n).unwrap(), "172.30.42.2");
        n.labels = None; assert!(reserved_dns(&n).is_err());
        let mut n = network(layout(42));
        n.ipam.as_mut().unwrap().config.as_mut().unwrap()[0].ip_range = None;
        assert!(reserved_dns(&n).is_err());
    }
}
