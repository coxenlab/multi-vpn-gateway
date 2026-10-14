//! 应用身份表:旧应用(VPN 管理网关)与通道管家并存安装时,各自独占的名字、路径、网段与端口。
//! 身份由运行环境档名推出(`tongdao` 或 `tongdao-*` = 通道管家,其余一律旧身份),不另设环境变量。
//! 两行一次写全,后续改动只读不改;助手侧(desktop/helper)的同名取值必须与这里一致。

use std::ops::RangeInclusive;

#[derive(Debug)]
pub struct Identity {
    pub name: &'static str,
    /// 正式运行环境档名(开发模式禁用;`host_integrations_allowed` 只认它)。
    pub prod_profile: &'static str,
    /// 未设 `VPN_NET` 时的容器网络。
    pub default_vpn_net: &'static str,
    /// 未设 `MIHOMO_CTRL_URL` 时的控制口(非测试构建;见 `config::default_ctrl_url`)。
    pub default_ctrl_url: &'static str,
    /// `infra.json` 首次生成、UI 端口重选时的取值范围。
    pub port_range: RangeInclusive<u16>,
    /// 运行环境根目录(相对 HOME);`None` = colima 默认的 `.colima`。
    pub colima_home_rel: Option<&'static str>,
    pub bundle_id: &'static str,
    pub helper_label: &'static str,
    pub helper_sock: &'static str,
    pub helper_dir: &'static str,
    pub helper_plist: &'static str,
    pub tun_device: &'static str,
    /// 引擎(mihomo#2)配置是否逐字节冻结:旧身份为 true——配置文本变一个字节助手就重建网卡,
    /// 旧应用用本分支重建时输出必须和改动前一样;通道管家为 false,引擎配置可以修(例如关掉 ICMP 转发)。
    pub tun_config_frozen: bool,
    pub fake_ip_range: &'static str,
    pub nat_return_v4: &'static str,
    /// 占位段整段路由(方案 5.13 起);旧身份没有。
    pub fake_ip_route_v4: Option<&'static str>,
    /// `/etc/resolver/` 下解析文件名前缀(方案 5.13 起);旧身份没有。
    pub resolver_prefix: Option<&'static str>,
    /// 互斥接管检测要看的对方网卡(只读检测旧应用);旧身份不检测。
    pub watch_other_tun: Option<&'static str>,
    /// Clash 接入片段里的节点名和规则集名。
    pub clash_proxy_name: &'static str,
    pub clash_provider_name: &'static str,
}

/// 旧应用(VPN 管理网关):取值与改动前的写死常量逐字相同。
pub static VPNMGR: Identity = Identity {
    name: "vpnmgr",
    prod_profile: "vpnmgr",
    default_vpn_net: "vpnmgr_vpnnet",
    default_ctrl_url: "http://127.0.0.1:9090",
    port_range: RangeInclusive::new(20000, 59999),
    colima_home_rel: None,
    bundle_id: "com.vpnmgr.desktop",
    helper_label: "com.vpnmgr.helper",
    helper_sock: "/var/run/vpnmgr-helper.sock",
    helper_dir: "/Library/PrivilegedHelperTools/vpnmgr",
    helper_plist: "/Library/LaunchDaemons/com.vpnmgr.helper.plist",
    tun_device: "utun225",
    tun_config_frozen: true,
    fake_ip_range: "198.19.0.1/16",
    nat_return_v4: "198.19.0.0/30",
    fake_ip_route_v4: None,
    resolver_prefix: None,
    watch_other_tun: None,
    clash_proxy_name: "vpn-router",
    clash_provider_name: "vpn-rules",
};

/// 通道管家。控制口默认值不可达:非托管模式下绝不打到本机 9090(常是用户自己的 Clash)。
pub static TONGDAO: Identity = Identity {
    name: "tongdao",
    prod_profile: "tongdao",
    default_vpn_net: "tongdao_vpnnet",
    default_ctrl_url: "http://127.0.0.1:1",
    port_range: RangeInclusive::new(10000, 19999),
    colima_home_rel: Some(".tongdao/colima"),
    bundle_id: "com.tongdao.desktop",
    helper_label: "com.tongdao.helper",
    helper_sock: "/var/run/tongdao-helper.sock",
    helper_dir: "/Library/PrivilegedHelperTools/tongdao",
    helper_plist: "/Library/LaunchDaemons/com.tongdao.helper.plist",
    tun_device: "utun226",
    tun_config_frozen: false,
    fake_ip_range: "198.19.128.1/17",
    nat_return_v4: "198.19.128.0/30",
    fake_ip_route_v4: Some("198.19.128.0/17"),
    resolver_prefix: Some("tongdao."),
    watch_other_tun: Some("utun225"),
    clash_proxy_name: "tongdao-router",
    clash_provider_name: "tongdao-rules",
};

pub fn for_profile(profile: &str) -> &'static Identity {
    if profile == "tongdao" || profile.starts_with("tongdao-") { &TONGDAO } else { &VPNMGR }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐字段展开(解构不带 `..`:新增字段不列进来就编译不过)。
    fn fields(id: &Identity) -> Vec<(&'static str, String)> {
        let Identity {
            name, prod_profile, default_vpn_net, default_ctrl_url, port_range, colima_home_rel,
            bundle_id, helper_label, helper_sock, helper_dir, helper_plist, tun_device, tun_config_frozen,
            fake_ip_range, nat_return_v4, fake_ip_route_v4, resolver_prefix, watch_other_tun,
            clash_proxy_name, clash_provider_name,
        } = id;
        vec![
            ("name", name.to_string()),
            ("prod_profile", prod_profile.to_string()),
            ("default_vpn_net", default_vpn_net.to_string()),
            ("default_ctrl_url", default_ctrl_url.to_string()),
            ("port_range", format!("{port_range:?}")),
            ("colima_home_rel", format!("{colima_home_rel:?}")),
            ("bundle_id", bundle_id.to_string()),
            ("helper_label", helper_label.to_string()),
            ("helper_sock", helper_sock.to_string()),
            ("helper_dir", helper_dir.to_string()),
            ("helper_plist", helper_plist.to_string()),
            ("tun_device", tun_device.to_string()),
            ("tun_config_frozen", tun_config_frozen.to_string()),
            ("fake_ip_range", fake_ip_range.to_string()),
            ("nat_return_v4", nat_return_v4.to_string()),
            ("fake_ip_route_v4", format!("{fake_ip_route_v4:?}")),
            ("resolver_prefix", format!("{resolver_prefix:?}")),
            ("watch_other_tun", format!("{watch_other_tun:?}")),
            ("clash_proxy_name", clash_proxy_name.to_string()),
            ("clash_provider_name", clash_provider_name.to_string()),
        ]
    }

    #[test]
    fn profile_maps_to_identity() {
        for p in ["tongdao", "tongdao-dev", "tongdao-verify-x"] {
            assert!(std::ptr::eq(for_profile(p), &TONGDAO), "{p}");
        }
        for p in ["vpnmgr", "vpnmgr-test", "default", "tongdao_dev", "tongdaox", "xtongdao-dev", ""] {
            assert!(std::ptr::eq(for_profile(p), &VPNMGR), "{p}");
        }
    }

    #[test]
    fn tongdao_row_carries_no_legacy_value() {
        for (field, value) in fields(&TONGDAO) {
            // 它的职责就是只读检测旧应用的网卡。
            if field == "watch_other_tun" { continue; }
            for bad in ["vpnmgr", "utun225", "198.19.0.", "/.colima"] {
                assert!(!value.contains(bad), "{field}={value} 含 {bad}");
            }
        }
    }

    #[test]
    fn rows_differ_in_every_field() {
        for ((field, old), (_, new)) in fields(&VPNMGR).into_iter().zip(fields(&TONGDAO)) {
            assert_ne!(old, new, "{field}");
        }
    }

    #[test]
    fn legacy_row_matches_previous_constants() {
        assert_eq!(VPNMGR.prod_profile, "vpnmgr");
        assert_eq!(VPNMGR.default_vpn_net, "vpnmgr_vpnnet");
        assert_eq!(VPNMGR.default_ctrl_url, "http://127.0.0.1:9090");
        assert_eq!(VPNMGR.port_range, 20000..=59999);
        assert_eq!(VPNMGR.colima_home_rel, None);
        assert_eq!(VPNMGR.helper_label, "com.vpnmgr.helper");
        assert_eq!(VPNMGR.helper_sock, "/var/run/vpnmgr-helper.sock");
        assert_eq!(VPNMGR.helper_dir, "/Library/PrivilegedHelperTools/vpnmgr");
        assert_eq!(VPNMGR.helper_plist, "/Library/LaunchDaemons/com.vpnmgr.helper.plist");
        assert_eq!(VPNMGR.tun_device, "utun225");
        assert!(VPNMGR.tun_config_frozen, "旧应用的引擎配置逐字节冻结");
        assert_eq!(VPNMGR.fake_ip_range, "198.19.0.1/16");
        assert_eq!(VPNMGR.nat_return_v4, "198.19.0.0/30");
        assert_eq!((VPNMGR.clash_proxy_name, VPNMGR.clash_provider_name), ("vpn-router", "vpn-rules"));
    }
}
