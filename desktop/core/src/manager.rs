//! Docker 编排 + mihomo 热加载 + SOCKS5 探活。对照 app/manager.py。
use serde::Serialize;
use serde_yaml::Value as Yaml;
use crate::config::Config;
use crate::store::{ChannelPublic, Rule};

#[derive(Serialize)]
struct ProxyEntry {
    name: String,
    #[serde(rename = "type")]
    typ: String,
    server: String,
    port: u16,
    udp: bool,
}

/// 命门 #2/#7:纯函数生成 mihomo 配置(读-改-写:保留 base 其它键)。
pub fn build_mihomo_config(mut base: Yaml, channels: &[ChannelPublic], rules: &[Rule]) -> Yaml {
    if !base.is_mapping() {
        base = Yaml::Mapping(serde_yaml::Mapping::new());
    }
    let proxies: Vec<ProxyEntry> = channels
        .iter()
        .map(|c| ProxyEntry {
            name: format!("ch-{}", c.id),
            typ: "socks5".into(),
            server: format!("vpn-{}", c.id),
            port: 1080,
            udp: true,
        })
        .collect();
    let mut out: Vec<String> = Vec::new();
    for r in rules {
        if r.enabled == 0 {
            continue;
        }
        if r.kind == "ip" {
            out.push(format!("IP-CIDR,{},ch-{},no-resolve", r.pattern, r.channel_id));
        } else if r.kind == "domain" {
            out.push(format!("DOMAIN-SUFFIX,{},ch-{}", r.pattern, r.channel_id));
        }
    }
    out.push("MATCH,DIRECT".to_string());

    if let Yaml::Mapping(m) = &mut base {
        m.insert(Yaml::String("proxies".into()), serde_yaml::to_value(&proxies).unwrap());
        m.insert(Yaml::String("proxy-groups".into()), Yaml::Sequence(vec![]));
        m.insert(Yaml::String("rules".into()), serde_yaml::to_value(&out).unwrap());
    }
    base
}

/// mihomo 配置文件路径:env MIHOMO_CONFIG_PATH,默认 /cfg/config.yaml(对照 manager.py CFG)。
fn mihomo_config_path() -> String {
    std::env::var("MIHOMO_CONFIG_PATH").unwrap_or_else(|_| "/cfg/config.yaml".into())
}

/// 命门 #3:写 CFG + PUT /configs?force=true(不重启 mihomo、不断连)。返回状态码串或错误串。
pub async fn rebuild(cfg: &Config, db: &std::path::Path) -> String {
    let inner = async {
        let channels = crate::store::list_channels(db)?;
        let rules = crate::store::all_rules(db)?;
        let cfg_path = mihomo_config_path();
        let base: Yaml = std::fs::read_to_string(&cfg_path)
            .ok()
            .and_then(|s| serde_yaml::from_str(&s).ok())
            .unwrap_or_else(|| Yaml::Mapping(serde_yaml::Mapping::new()));
        let merged = build_mihomo_config(base, &channels, &rules);
        let yaml = serde_yaml::to_string(&merged)?;
        std::fs::write(&cfg_path, yaml)?;
        let client = reqwest::Client::new();
        let resp = client
            .put(format!("{}/configs", cfg.mihomo_ctrl_url))
            .query(&[("force", "true")])
            .bearer_auth(&cfg.mihomo_secret)
            .json(&serde_json::json!({ "path": cfg_path }))
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await?;
        Ok::<u16, anyhow::Error>(resp.status().as_u16())
    };
    match inner.await {
        Ok(code) => code.to_string(),
        Err(e) => format!("{e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ChannelPublic, Rule};
    use serde_json::json;

    fn ch(id: &str) -> ChannelPublic {
        ChannelPublic {
            id: id.into(), name: "n".into(), vpn_type: "easyconnect".into(), server: "".into(),
            ec_ver: None, login_method: "interactive".into(), username: "".into(),
            vnc_password: None, mac: None, novnc_port: None, probe_url: "".into(),
            status: "logged_in".into(), container_id: None, latency_ms: None, config: json!({}),
        }
    }
    fn rule(cid: &str, kind: &str, pat: &str, enabled: i64) -> Rule {
        Rule { id: 0, channel_id: cid.into(), kind: kind.into(), pattern: pat.into(), enabled }
    }

    #[test]
    fn mihomo_config_dns_asymmetry_and_naming() {
        let base = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let chans = vec![ch("a")];
        let rules = vec![
            rule("a", "ip", "10.0.0.0/8", 1),
            rule("a", "domain", "corp.example.com", 1),
            rule("a", "domain", "disabled.com", 0),
        ];
        let cfg = build_mihomo_config(base, &chans, &rules);
        let s = serde_yaml::to_string(&cfg).unwrap();
        assert!(s.contains("ch-a"));
        assert!(s.contains("vpn-a"));
        let rules_seq = cfg.get("rules").unwrap().as_sequence().unwrap();
        let texts: Vec<String> = rules_seq.iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(texts.contains(&"IP-CIDR,10.0.0.0/8,ch-a,no-resolve".to_string()));
        assert!(texts.contains(&"DOMAIN-SUFFIX,corp.example.com,ch-a".to_string()));
        assert!(!texts.iter().any(|t| t.contains("disabled.com")));
        assert!(!texts.iter().any(|t| t.starts_with("DOMAIN-SUFFIX") && t.contains("no-resolve")));
        assert_eq!(texts.last().unwrap(), "MATCH,DIRECT");
    }

    #[test]
    fn mihomo_config_preserves_base_keys() {
        let base: serde_yaml::Value = serde_yaml::from_str("dns:\n  enable: true\nlisteners: []\n").unwrap();
        let cfg = build_mihomo_config(base, &[], &[]);
        assert!(cfg.get("dns").is_some());
        let rules_seq = cfg.get("rules").unwrap().as_sequence().unwrap();
        assert_eq!(rules_seq.len(), 1);
    }
}
