use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub ui_port: u16,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub mihomo_ctrl_url: String,
    pub mihomo_secret: String,
    pub mihomo_host_port: String,
    pub mihomo_ctrl_port: Option<String>,
    pub vpn_net: String,
}

impl Config {
    /// 真实加载:env 非空才算设置(空串当未设)。
    pub fn load() -> Self {
        Self::from_getter(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
    }

    /// 可注入 getter,便于 hermetic 测试。
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Self {
        Config {
            ui_port: get("UI_PORT").and_then(|s| s.parse().ok()).unwrap_or(8787),
            data_dir: get("DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.data"))),
            static_dir: get("STATIC_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../app/static"))),
            mihomo_ctrl_url: get("MIHOMO_CTRL_URL").unwrap_or_else(|| "http://127.0.0.1:9090".into()),
            mihomo_secret: get("MIHOMO_SECRET").unwrap_or_default(),
            mihomo_host_port: get("MIHOMO_HOST_PORT").unwrap_or_default(),
            mihomo_ctrl_port: get("MIHOMO_CTRL_PORT"),
            vpn_net: get("VPN_NET").unwrap_or_else(|| "vpnmgr_vpnnet".into()),
        }
    }

    pub fn db_path(&self) -> PathBuf { self.data_dir.join("vpnmgr.db") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn defaults_when_env_absent() {
        let cfg = Config::from_getter(|_| None);
        assert_eq!(cfg.ui_port, 8787);
        assert_eq!(cfg.mihomo_ctrl_url, "http://127.0.0.1:9090");
        assert_eq!(cfg.mihomo_host_port, "");
        assert_eq!(cfg.mihomo_ctrl_port, None);
        assert_eq!(cfg.vpn_net, "vpnmgr_vpnnet");
    }

    #[test]
    fn reads_overrides() {
        let m: HashMap<&str, &str> = [
            ("UI_PORT", "9001"),
            ("MIHOMO_HOST_PORT", "7899"),
            ("MIHOMO_CTRL_PORT", "9090"),
            ("DATA_DIR", "/tmp/vpnmgr-test"),
        ].into_iter().collect();
        let cfg = Config::from_getter(|k| m.get(k).map(|s| s.to_string()));
        assert_eq!(cfg.ui_port, 9001);
        assert_eq!(cfg.mihomo_host_port, "7899");
        assert_eq!(cfg.mihomo_ctrl_port, Some("9090".into()));
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/vpnmgr-test"));
    }
}
