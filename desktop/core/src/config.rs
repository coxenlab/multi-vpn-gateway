use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    pub vm_profile: String,
    pub dev_mode: bool,
    /// 桌面壳开启按需 VM；独立 core/测试默认只连接已有 Docker。
    pub managed_vm: bool,
    pub bundled_images_dir: Option<PathBuf>,
    pub ui_port: u16,
    pub data_dir: PathBuf,
    pub static_dir: PathBuf,
    pub mihomo_ctrl_url: String,
    pub mihomo_secret: String,
    pub mihomo_host_port: String,
    pub mihomo_ctrl_port: Option<String>,
    pub vpn_net: String,
}

/// 编译期开发默认数据目录(desktop/core/.data)。⚠️ 只在开发机成立:打包分发到别人机器
/// 上这是个不存在且不可写的绝对路径(os error 13),壳须在启动最早期把 DATA_DIR 改写到
/// 用户可写目录(见 desktop/app main.rs resolve_data_dir)。
pub fn dev_default_data_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/.data"))
}

impl Config {
    /// 真实加载:env 非空才算设置(空串当未设)。
    pub fn load() -> Self {
        Self::from_getter(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))
    }

    /// 可注入 getter,便于 hermetic 测试。
    pub fn from_getter(get: impl Fn(&str) -> Option<String>) -> Self {
        Config {
            vm_profile: get("VPNMGR_VM_PROFILE").unwrap_or_else(|| "vpnmgr".into()),
            dev_mode: get("VPNMGR_DEV_MODE").as_deref() == Some("1"),
            managed_vm: get("VPNMGR_MANAGED_VM").as_deref() == Some("1"),
            bundled_images_dir: get("VPNMGR_BUNDLED_IMAGES_DIR").map(PathBuf::from),
            ui_port: get("UI_PORT").and_then(|s| s.parse().ok()).unwrap_or(8787),
            data_dir: get("DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(dev_default_data_dir),
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

    pub fn host_integrations_allowed(&self) -> bool {
        !self.dev_mode && self.vm_profile == "vpnmgr"
    }

    pub fn docker_socket(&self) -> PathBuf {
        crate::vm::socket_path(&self.vm_profile)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(!self.data_dir.join(crate::upgrade::PENDING).try_exists()?, "这是尚未启用的升级副本，请先完成或恢复离线配置切换");
        crate::upgrade_switch::validate_boot(&self.data_dir)?;
        anyhow::ensure!(!self.vm_profile.is_empty() && self.vm_profile.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'), "无效 VM profile");
        anyhow::ensure!(!self.managed_vm || self.vm_profile != "default", "桌面底座不能管理 default profile");
        if self.dev_mode {
            anyhow::ensure!(self.vm_profile != "vpnmgr" && self.vm_profile != "default",
                "开发模式必须使用独立 VM profile");
            anyhow::ensure!(self.data_dir != dev_default_data_dir(),
                "开发模式必须显式指定独立 DATA_DIR");
        }
        Ok(())
    }
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
        assert_eq!(cfg.vm_profile, "vpnmgr");
        assert!(cfg.host_integrations_allowed());
    }

    #[test]
    fn reads_overrides() {
        let m: HashMap<&str, &str> = [
            ("UI_PORT", "9001"),
            ("MIHOMO_HOST_PORT", "7899"),
            ("MIHOMO_CTRL_PORT", "9090"),
            ("DATA_DIR", "/tmp/vpnmgr-test"),
            ("VPNMGR_VM_PROFILE", "vpnmgr-dev"),
            ("VPNMGR_DEV_MODE", "1"),
        ].into_iter().collect();
        let cfg = Config::from_getter(|k| m.get(k).map(|s| s.to_string()));
        assert_eq!(cfg.ui_port, 9001);
        assert_eq!(cfg.mihomo_host_port, "7899");
        assert_eq!(cfg.mihomo_ctrl_port, Some("9090".into()));
        assert_eq!(cfg.data_dir, PathBuf::from("/tmp/vpnmgr-test"));
        assert_eq!(cfg.vm_profile, "vpnmgr-dev");
        assert!(!cfg.host_integrations_allowed());
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn rejects_development_using_daily_vm_or_data() {
        let mut cfg = Config::from_getter(|_| None);
        cfg.dev_mode = true;
        assert!(cfg.validate().is_err());
        cfg.vm_profile = "vpnmgr-dev".into();
        assert!(cfg.validate().is_err());
        cfg.data_dir = PathBuf::from("/tmp/vpnmgr-isolated");
        assert!(cfg.validate().is_ok());
        cfg.vm_profile = "../../vpnmgr".into();
        assert!(cfg.validate().is_err());
    }
}
