//! main.py 内联纯逻辑的 Rust 端:规则分类 / Clash 文本 / PAC。对照 app/main.py。

/// 对照 _classify:('ip', cidr) / ('domain', host) / None。
pub fn classify(token: &str) -> Option<(String, String)> {
    if token.is_empty() {
        return None;
    }
    let addr = token.split('/').next().unwrap_or("");
    // 是否走 IP 分支:对齐 Python ipaddress.ip_address 的接受集(含 scoped IPv6 X%zone)。
    // scoped IPv6 仍走 IP 分支,再由 norm_ip 因含 '%' 拒掉 → None(命门②:zone id 能藏逗号是注入
    // 向量、且非 mihomo 合法 CIDR);绝不落域名分支——否则 norm_domain 的剥端口会把 "2001:db8::1%eth0"
    // 砍成 "2001" 误当域名。scoped IPv4 / 空 zone / 非 IP 串在 Python 里 ip_address 抛错,故落域名分支。
    if looks_like_ip(addr) {
        return norm_ip(token).map(|ip| ("ip".into(), ip));
    }
    // 域名:规范化(剥 scheme/path/userinfo/port + bare + 小写)并过 DANGER 校验(命门②),对照 _norm_domain。
    let host = norm_domain(token)?;
    Some(("domain".into(), host))
}

/// addr 是否为 IP 字面量(对齐 Python ipaddress.ip_address 接受集):普通 v4/v6,或 scoped IPv6
///(X%zone,X 为合法 v6、zone 非空)。std::net::IpAddr 不认 zone id,故 scoped 单独判;scoped 仅
/// IPv6 合法(Python 对 scoped IPv4 抛 ValueError,实测同 3.9+ 行为)。
fn looks_like_ip(addr: &str) -> bool {
    if let Some((base, zone)) = addr.split_once('%') {
        !zone.is_empty() && base.parse::<std::net::Ipv6Addr>().is_ok()
    } else {
        addr.parse::<std::net::IpAddr>().is_ok()
    }
}

/// 对照 _norm_ip:IP/CIDR token 规范化 + 校验(命门②收紧)。裸 IP 补 /32(v4)/128(v6);
/// 带掩码经 ipnet 非严格解析(天然拒 dotted mask 如 /255.255.255.0、拒越界前缀如 /40)。
/// addr 含 '%'(zone/scope id)→ None(注入向量 + 非 mihomo 合法 CIDR)。非法返回 None。
/// 只存解析规范结果(裸 IP 补掩码 / CIDR 原样),绝不把含 '%' 或逗号的原 token 存回。
pub fn norm_ip(token: &str) -> Option<String> {
    if token.is_empty() || !rule_pattern_safe(token) {
        return None;
    }
    let addr = token.split('/').next().unwrap_or("");
    if addr.contains('%') {
        return None;
    }
    if addr.parse::<std::net::IpAddr>().is_err() {
        return None;
    }
    if token.contains('/') {
        if token.parse::<ipnet::IpNet>().is_err() {
            return None;
        }
        return Some(token.to_string());
    }
    // 裸 IP 补掩码(对照 Python `":" in addr` 判族)。
    Some(format!("{token}{}", if addr.contains(':') { "/128" } else { "/32" }))
}

/// 对照 _norm_domain:域名 token 规范化 + 校验。剥 scheme/path/userinfo/port → bare 去 +./*. 通配前缀
/// → 去首尾点 → Unicode 小写;非空 / 无 ".." / 长度<=253 / DANGER 字符集统一由 valid_domain 裁定(命门②)。
/// 存规范化后裸域名,**Unicode 原样保留**(放行中文内网域名,不转 punycode),与 Python _norm_domain 逐字对齐;非法返回 None。
pub fn norm_domain(token: &str) -> Option<String> {
    if token.is_empty() || !rule_pattern_safe(token) {
        return None;
    }
    // 剥 scheme / path / userinfo / port,只留主机名(否则 "https://oa.x.com/" 整串被当域名,
    // 生成的 DOMAIN-SUFFIX 永不命中)。
    let after_scheme = token.splitn(2, "://").last().unwrap_or("");
    let no_path = after_scheme.split('/').next().unwrap_or("");
    let no_user = no_path.rsplit('@').next().unwrap_or("");
    let no_port = no_user.split(':').next().unwrap_or("");
    // Unicode lowercase 与 Python str.lower 对齐；中文等其它 Unicode 原样保留、不转 punycode。
    let host = bare(no_port).trim_matches('.').to_lowercase();
    if valid_domain(&host) { Some(host) } else { None }
}

/// 对照 _bare:剥 `+.` / `*.` 前缀。
pub fn bare(p: &str) -> String {
    for pre in ["+.", "*."] {
        if let Some(rest) = p.strip_prefix(pre) {
            return rest.to_string();
        }
    }
    p.to_string()
}

/// 命门②共享原语:危险字符集 DANGER = 逗号 / 任意 Unicode 空白 / 双引号 / 单引号 /
/// 反斜杠 / U+0000..U+001F。
/// 双栈契约:与 Python 同名判定逐字等价。域名校验 / 出口再校验 / 镜像源 host 校验共用同一集合。
fn is_danger_char(c: char) -> bool {
    c == ','
        || c == '"'
        || c == '\''
        || c == '\\'
        || c.is_whitespace()
        || (c as u32) <= 0x1f
}

/// 命门②出口再校验:pattern 命中任一 DANGER 字符 → false(该条不得写进任何规则输出面)。
/// 出口 YAML/JSON 编码只挡 YAML 结构注入,挡不住 mihomo/Clash classical 规则行内的逗号语法
///(`victim.example,DIRECT` 会让第三字段变策略);存量脏数据须在 rebuild/provider/pac 三面被拦。
pub fn rule_pattern_safe(p: &str) -> bool {
    !p.chars().any(is_danger_char)
}

/// 域名校验(命门②:堵逗号/换行/引号/空格/反斜杠等注入,别让脏 host 流进 mihomo/provider/pac 输出面)。
/// 双栈契约:与 Python _valid_domain 逐字等价——**放行 Unicode**(mihomo 支持中文 DOMAIN-SUFFIX,
/// 如 fp.内网;旧的 ASCII 白名单 [a-z0-9_.-] 误杀中文内网域名,是回归,已弃)。只拒 DANGER 字符,
/// 另须非空 / 无 ".." / 长度(按码点)1..=253。domain 经 norm_domain 规范化(bare+Unicode 小写)后存储。
pub fn valid_domain(host: &str) -> bool {
    !host.is_empty()
        && host.chars().count() <= 253
        && !host.contains("..")
        && !host.chars().any(is_danger_char)
}

/// Sole stored-rule contract used by every runtime output consumer.
pub fn normalize_stored_rule(kind: &str, pattern: &str) -> Option<(String, String)> {
    let normalized = match kind {
        "domain" => norm_domain(pattern),
        "ip" => norm_ip(pattern),
        _ => None,
    }?;
    Some((kind.to_string(), normalized))
}

use std::collections::HashMap;

/// plan_rules 的结果(对照 add_rules 的 added/rejected + 实际要落库的 (kind,pat))。
pub struct RulePlan {
    pub to_add: Vec<(String, String)>,
    pub added: HashMap<String, i64>,
    pub rejected: Vec<String>,
}

/// 对照 add_rules 的内联分类/去重逻辑(纯函数)。forced = Some("ip"|"domain") 或 None(自动 classify)。
/// existing = 已存在的 (kind,pattern);去重对全集(含本批已加)。
pub fn plan_rules(patterns: &[String], forced: Option<&str>, existing: &[(String, String)]) -> RulePlan {
    let mut seen: std::collections::HashSet<(String, String)> = existing.iter().cloned().collect();
    let mut to_add = Vec::new();
    let mut added: HashMap<String, i64> = HashMap::from([("domain".into(), 0), ("ip".into(), 0)]);
    let mut rejected = Vec::new();
    for tok in patterns {
        let (kind, pat) = match forced {
            Some("domain") => {
                // 强制 domain 绕过 classify,仍须过 norm_domain(命门②)并存规范化裸小写域名,
                // 与 Python add_rules 的 _norm_domain 一致(如 "Corp.COM"→"corp.com"、"+.x.com"→"x.com")。
                let pat = match norm_domain(tok) {
                    Some(h) => h,
                    None => {
                        rejected.push(tok.clone());
                        continue;
                    }
                };
                ("domain".to_string(), pat)
            }
            Some("ip") => match classify(tok) {
                Some((k, p)) if k == "ip" => (k, p),
                _ => {
                    rejected.push(tok.clone());
                    continue;
                }
            },
            _ => match classify(tok) {
                Some((k, p)) => (k, p),
                None => {
                    rejected.push(tok.clone());
                    continue;
                }
            },
        };
        if pat.is_empty() || seen.contains(&(kind.clone(), pat.clone())) {
            continue;
        }
        seen.insert((kind.clone(), pat.clone()));
        *added.entry(kind.clone()).or_insert(0) += 1;
        to_add.push((kind, pat));
    }
    RulePlan { to_add, added, rejected }
}

/// 对照 main.py login 的 url 构造(命门 #4:端口是容器实时映射的 host 端口)。
/// path 须带尾斜杠:镜像内 tinyproxy 把 /websockify 301→/websockify/,WS 握手不跟随 301。
pub fn login_url(port: i64, vnc_password: &str) -> String {
    format!(
        "http://127.0.0.1:{port}/vnc.html?path=websockify/&autoconnect=true&resize=remote&password={vnc_password}"
    )
}

use crate::store::Rule;

/// 对照 clash_provider:behavior classical 的 rule-provider payload。命门 #2:域名经 bare。
/// 命门②双层防线:(1) 出口再校验——含 DANGER 字符的脏 pattern 整条跳过(rule_pattern_safe);
/// (2) serde_yaml 序列化而非手拼字符串:漏网的 payload 每项仍是被正确引用的 YAML 标量,逃不出单个
/// 列表元素。插入序、domain/ip 交错原样,不排序。
pub fn clash_provider_text(rules: &[Rule]) -> String {
    #[derive(serde::Serialize)]
    struct Provider {
        payload: Vec<String>,
    }
    let payload: Vec<String> = rules.iter().filter(|r| r.enabled != 0).filter_map(|r| {
        let (kind, pattern) = normalize_stored_rule(&r.kind, &r.pattern)?;
        Some(if kind == "ip" {
            format!("IP-CIDR,{pattern}")
        } else {
            format!("DOMAIN-SUFFIX,{pattern}")
        })
    }).collect();
    serde_yaml::to_string(&Provider { payload }).unwrap_or_else(|_| "payload: []\n".to_string())
}

/// 对照 clash_snippet:节点 + provider 订阅 + 内联两种(二选一)。命门 #2:IP/域名都带 no-resolve(入口侧语义)。
pub fn clash_snippet_text(rules: &[Rule], mihomo_host_port: &str, ui_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let ui = if ui_port.is_empty() { "<UI端口>" } else { ui_port };
    let mut l = vec![
        "# ① 在你现有 Clash 的 proxies: 下加这个节点".to_string(),
        "proxies:".into(),
        "  - name: vpn-router".into(),
        "    type: socks5".into(),
        "    server: 127.0.0.1".into(),
        format!("    port: {port}"),
        "".into(),
        "# ② 方式甲(推荐):订阅一份规则,绑新域名/IP 自动同步,之后不再动 Clash".into(),
        "rule-providers:".into(),
        "  vpn-rules:".into(),
        "    type: http".into(),
        "    behavior: classical".into(),
        "    format: yaml".into(),
        format!("    url: http://127.0.0.1:{ui}/clash/vpn-rules.yaml"),
        "    interval: 60".into(),
        "    path: ./providers/vpn-rules.yaml".into(),
        "# rules: 顶部加一行引用(no-resolve 对清单内 IP-CIDR 生效)".into(),
        "  - RULE-SET,vpn-rules,vpn-router,no-resolve".into(),
        "".into(),
        "# ② 方式乙:直接内联(不想用 provider 时)".into(),
    ];
    // 保持文本模板,但每条 pattern 过校验、非法跳过(命门②):ip 须 parse 成功、domain 过白名单,
    // 别把脏串拼进给用户复制的片段。
    let enabled: Vec<(String, String)> = rules.iter().filter(|r| r.enabled != 0)
        .filter_map(|r| normalize_stored_rule(&r.kind, &r.pattern)).collect();
    if enabled.is_empty() {
        l.push("  # (还没绑定任何规则)".into());
    } else {
        for (kind, pattern) in enabled {
            if kind == "ip" {
                l.push(format!("  - IP-CIDR,{pattern},vpn-router,no-resolve"));
            } else {
                l.push(format!("  - DOMAIN-SUFFIX,{pattern},vpn-router,no-resolve"));
            }
        }
    }
    l.push("".into());
    l.push(format!("# ③ 无 Clash 时:把系统/浏览器代理指向 127.0.0.1:{port}"));
    l.push("#    本工具 mihomo 自身分流:命中→VPN 容器,其余→直连。".into());
    l.join("\n")
}

/// 对照 entry_pac:命中域名/v4 网段走入口 SOCKS5,其余 DIRECT(v6 网段略过,isInNet 仅 v4)。
pub fn pac_text(rules: &[Rule], mihomo_host_port: &str) -> String {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let proxy = format!("SOCKS5 127.0.0.1:{port}; SOCKS 127.0.0.1:{port}; DIRECT");
    let mut domains = Vec::new();
    let mut nets = Vec::new();
    // 命门②出口再校验:含 DANGER 字符的脏 pattern 整条跳过(serde_json 转义是第二层防线)。
    for r in rules.iter().filter(|r| r.enabled != 0) {
        let Some((kind, pattern)) = normalize_stored_rule(&r.kind, &r.pattern) else { continue };
        if kind == "domain" {
            domains.push(pattern);
        } else if let Ok(ipnet::IpNet::V4(n)) = pattern.parse::<ipnet::IpNet>() {
            nets.push((n.network().to_string(), n.netmask().to_string()));
        }
    }
    // domains/nets 数组用 serde_json 生成:引号/反斜杠等自动转义,脏 pattern 也破不了 JS 字符串(命门②)。
    let dom_js = serde_json::to_string(&domains).unwrap_or_else(|_| "[]".to_string());
    let nets_pairs: Vec<[String; 2]> = nets.iter().map(|(a, m)| [a.clone(), m.clone()]).collect();
    let net_js = serde_json::to_string(&nets_pairs).unwrap_or_else(|_| "[]".to_string());
    let mut s = String::new();
    s.push_str("// 本工具自动生成 · 命中客户域名/IP 走入口,其余 DIRECT\n");
    s.push_str(&format!("var PROXY = \"{proxy}\";\n"));
    s.push_str(&format!("var DOMAINS = {dom_js};\n"));
    s.push_str(&format!("var NETS = {net_js};\n"));
    s.push_str("function FindProxyForURL(url, host) {\n");
    s.push_str("  host = (host || '').toLowerCase();\n");
    s.push_str("  for (var i = 0; i < DOMAINS.length; i++) {\n");
    s.push_str("    var d = DOMAINS[i];\n");
    s.push_str("    if (host === d || host.slice(-(d.length + 1)) === '.' + d) return PROXY;\n");
    s.push_str("  }\n");
    s.push_str("  if (/^\\d+\\.\\d+\\.\\d+\\.\\d+$/.test(host)) {\n");
    s.push_str("    for (var j = 0; j < NETS.length; j++) {\n");
    s.push_str("      if (isInNet(host, NETS[j][0], NETS[j][1])) return PROXY;\n");
    s.push_str("    }\n");
    s.push_str("  }\n");
    s.push_str("  return 'DIRECT';\n");
    s.push_str("}\n");
    s
}

/// 对照 entry_setup_commands:各平台指向入口的开/关命令。
pub fn setup_commands(mihomo_host_port: &str, ui_port: &str) -> serde_json::Value {
    let port = if mihomo_host_port.is_empty() { "?" } else { mihomo_host_port };
    let pac_url = if ui_port.is_empty() {
        "/entry/proxy.pac".to_string()
    } else {
        format!("http://127.0.0.1:{ui_port}/entry/proxy.pac")
    };
    serde_json::json!({
        "port": port,
        "pac_url": pac_url,
        "macos": {
            "socks_on": format!("networksetup -setsocksfirewallproxy Wi-Fi 127.0.0.1 {port}"),
            "socks_off": "networksetup -setsocksfirewallproxystate Wi-Fi off",
            "pac_on": format!("networksetup -setautoproxyurl Wi-Fi {pac_url}"),
            "pac_off": "networksetup -setautoproxystate Wi-Fi off",
        },
        "windows": format!("设置 → 网络和 Internet → 代理 → 手动设置代理填 127.0.0.1:{port};或「使用安装脚本」填 PAC URL"),
        "env": {
            "socks": format!("export ALL_PROXY=socks5h://127.0.0.1:{port}"),
            "http": format!("export HTTPS_PROXY=http://127.0.0.1:{port} HTTP_PROXY=http://127.0.0.1:{port}"),
            "unset": "unset ALL_PROXY HTTPS_PROXY HTTP_PROXY",
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_bare_ipv4_gets_32() {
        assert_eq!(classify("10.1.2.3"), Some(("ip".into(), "10.1.2.3/32".into())));
    }
    #[test]
    fn classify_bare_ipv6_gets_128() {
        assert_eq!(classify("fd00::1"), Some(("ip".into(), "fd00::1/128".into())));
    }
    #[test]
    fn classify_cidr_passthrough_strict_false() {
        assert_eq!(classify("10.0.0.0/8"), Some(("ip".into(), "10.0.0.0/8".into())));
        assert_eq!(classify("10.1.2.3/8"), Some(("ip".into(), "10.1.2.3/8".into())));
        assert_eq!(classify("10.0.0.0/40"), None);
    }
    #[test]
    fn classify_domain_strips_scheme_path_port_userinfo() {
        assert_eq!(classify("https://oa.x.com/login"), Some(("domain".into(), "oa.x.com".into())));
        assert_eq!(classify("user@MAIL.X.com:443"), Some(("domain".into(), "mail.x.com".into())));
        assert_eq!(classify("  Corp.Example.Com.  "), None, "任何 Unicode 空白都拒绝,不先 trim");
        // bare:通配前缀 +./*. 经 norm_domain 剥掉(此前 classify 缺 bare 会误拒)
        assert_eq!(classify("*.y.com"), Some(("domain".into(), "y.com".into())));
        assert_eq!(classify("+.x.com"), Some(("domain".into(), "x.com".into())));
        assert_eq!(classify("https://oa.x.com/path"), Some(("domain".into(), "oa.x.com".into())));
    }
    #[test]
    fn classify_empty_is_none() {
        assert_eq!(classify(""), None);
        assert_eq!(classify("   "), None);
        assert_eq!(classify("https://"), None);
    }

    #[test]
    fn classify_scoped_ipv6_rejected() {
        // 命门②收紧:scoped IPv6(zone/scope id)能藏逗号(注入向量)且非 mihomo 合法 CIDR → 一律 None
        //(此前当 IP 存原串,现拒)。走 IP 分支后由 norm_ip 因含 '%' 拒掉;scoped IPv4/空 zone 落域名分支。
        assert_eq!(classify("fe80::1%eth0"), None);
        assert_eq!(classify("2001:db8::1%eth0"), None);
        assert_eq!(classify("2001:db8::1%eth0,MATCH"), None); // zone 里藏逗号,一样拒
    }

    #[test]
    fn classify_ip_normalization_matches_python() {
        // B 双栈契约:裸 v4 补 /32、CIDR 原样过;dotted mask(/255.255.255.0)由 ipnet 天然拒。
        assert_eq!(classify("10.0.0.5"), Some(("ip".into(), "10.0.0.5/32".into())));
        assert_eq!(classify("10.0.0.0/8"), Some(("ip".into(), "10.0.0.0/8".into())));
        assert_eq!(classify("10.0.0.0/255.255.255.0"), None, "dotted mask 拒");
    }
    #[test]
    fn bare_strips_plus_and_star() {
        assert_eq!(bare("+.x.com"), "x.com");
        assert_eq!(bare("*.x.com"), "x.com");
        assert_eq!(bare("x.com"), "x.com");
    }

    #[test]
    fn norm_domain_matches_python_semantics() {
        // 跨栈边界钉死:接受/存储集与 Python _norm_domain 逐字对齐(通配 bare、剥 scheme/path/port、小写)。
        assert_eq!(norm_domain("*.y.com").as_deref(), Some("y.com"));
        assert_eq!(norm_domain("+.x.com").as_deref(), Some("x.com"));
        assert_eq!(norm_domain("Corp.COM").as_deref(), Some("corp.com"));
        assert_eq!(norm_domain("https://oa.x.com:8443/p?x=1").as_deref(), Some("oa.x.com"));
        // A 回归修复:放行 Unicode 中文内网域名(旧 ASCII 白名单误杀,是回归);Unicode 原样、不转 punycode。
        assert_eq!(norm_domain("fp.内网").as_deref(), Some("fp.内网"));
        assert_eq!(norm_domain("_dmarc.x.com").as_deref(), Some("_dmarc.x.com"));
        // ASCII 部分小写、Unicode 字符不改写。
        assert_eq!(norm_domain("OA.内网.Corp").as_deref(), Some("oa.内网.corp"));
        assert_eq!(norm_domain("ÄBC.中国").as_deref(), Some("äbc.中国"), "Unicode lowercase");
        assert_eq!(norm_domain("example.ΟΣ").as_deref(), Some("example.ος"), "contextual lowercase parity");
        // 命门②:注入/脏串(逗号/空格/换行/引号/单引号/反斜杠/制表符)一律拒
        assert_eq!(norm_domain("evil.com,ch-x"), None);
        assert_eq!(norm_domain("a b.com"), None);
        assert_eq!(norm_domain("a\nb.com"), None);
        assert_eq!(norm_domain("a\"b.com"), None);
        assert_eq!(norm_domain("a'b.com"), None);
        assert_eq!(norm_domain("a\\b.com"), None);
        assert_eq!(norm_domain("a\tb.com"), None);
        assert_eq!(norm_domain("a\u{00a0}b.com"), None, "NBSP 也是 Unicode 空白");
        assert_eq!(norm_domain("a\u{2003}b.com"), None, "em-space 也是 Unicode 空白");
    }

    #[test]
    fn valid_domain_allows_unicode_rejects_danger() {
        // A:放行 Unicode(fp.内网)、下划线、正常多标签;拒 DANGER + 空 + 连续点。
        assert!(valid_domain("fp.内网"));
        assert!(valid_domain("_dmarc.x.com"));
        assert!(valid_domain("oa.x.com"));
        assert!(!valid_domain("a,b.com"));
        assert!(!valid_domain("a b.com"));
        assert!(!valid_domain("a\nb.com"));
        assert!(!valid_domain(""));
        assert!(!valid_domain("a..b"));
    }

    #[test]
    fn plan_rules_auto_classifies_and_dedups() {
        let existing = vec![("domain".to_string(), "a.com".to_string())];
        let out = plan_rules(
            &["a.com".into(), "https://b.com/x".into(), "10.0.0.0/8".into(), "10.0.0.0/40".into()],
            None,
            &existing,
        );
        assert!(out.to_add.contains(&("domain".into(), "b.com".into())));
        assert!(out.to_add.contains(&("ip".into(), "10.0.0.0/8".into())));
        assert!(!out.to_add.iter().any(|(_, p)| p == "a.com"), "已存在不重复");
        assert_eq!(out.added.get("ip"), Some(&1));
        assert_eq!(out.added.get("domain"), Some(&1));
        assert!(out.rejected.contains(&"10.0.0.0/40".to_string()), "非法 CIDR 拒绝");
    }

    #[test]
    fn plan_rules_forced_ip_rejects_non_ip() {
        let out = plan_rules(&["not-an-ip".into(), "10.0.0.1".into()], Some("ip"), &[]);
        assert!(out.rejected.contains(&"not-an-ip".to_string()));
        assert!(out.to_add.contains(&("ip".into(), "10.0.0.1/32".into())));
    }

    #[test]
    fn plan_rules_forced_domain_normalizes() {
        // forced domain 经 norm_domain 规范化后入库(bare + 小写),与 Python _norm_domain 一致。
        let out = plan_rules(&["Corp.COM".into(), "  spaced.com  ".into(), "+.x.com".into()], Some("domain"), &[]);
        assert!(out.to_add.contains(&("domain".into(), "corp.com".into())), "大小写规范化为小写");
        assert!(out.to_add.contains(&("domain".into(), "x.com".into())), "+. 通配前缀 bare 掉");
        assert!(out.rejected.contains(&"  spaced.com  ".to_string()));
    }

    #[test]
    fn login_url_has_websockify_path_and_password() {
        let u = login_url(45678, "deadbeef");
        assert!(u.contains("127.0.0.1:45678/vnc.html"));
        assert!(u.contains("path=websockify/"), "尾斜杠不可少(tinyproxy 301 不被 WS 跟随)");
        assert!(u.contains("autoconnect=true"));
        assert!(u.contains("password=deadbeef"));
    }

    fn rl(kind: &str, pat: &str, en: i64) -> crate::store::Rule {
        crate::store::Rule { id: 0, channel_id: "c".into(), kind: kind.into(), pattern: pat.into(), enabled: en }
    }

    #[test]
    fn clash_provider_classical_skips_disabled() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "+.x.com", 1), rl("domain", "off.com", 0)];
        let out = clash_provider_text(&rs);
        // serde_yaml 输出(块序列 0 缩进);解析后逐项比对,不比字节格式。
        assert!(out.starts_with("payload:"));
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let payload: Vec<String> = parsed.get("payload").unwrap().as_sequence().unwrap()
            .iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(payload.contains(&"IP-CIDR,10.0.0.0/8".to_string()));
        assert!(payload.contains(&"DOMAIN-SUFFIX,x.com".to_string())); // bare 去 +.
        assert!(!out.contains("off.com"));
    }

    #[test]
    fn golden_provider_payload_match_fixture() {
        // 双栈 golden 契约:provider payload 保持 ORDER BY id 插入序、domain/ip 交错原样、不做前缀排序。
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/golden_rules.json")).unwrap();
        let id_of = |key: &str| -> &'static str {
            match key {
                "ch0" => "aaa111",
                "ch1" => "bbb222",
                other => panic!("golden fixture 出现未映射的 key: {other}"),
            }
        };
        let rules: Vec<crate::store::Rule> = fixture["rules_insertion_order"]
            .as_array().unwrap().iter().enumerate()
            .map(|(i, r)| crate::store::Rule {
                id: (i + 1) as i64,
                channel_id: id_of(r["channel"].as_str().unwrap()).to_string(),
                kind: r["kind"].as_str().unwrap().to_string(),
                pattern: r["pattern"].as_str().unwrap().to_string(),
                enabled: r["enabled"].as_bool().unwrap() as i64,
            })
            .collect();
        let out = clash_provider_text(&rules);
        let parsed: serde_yaml::Value = serde_yaml::from_str(&out).unwrap();
        let payload: Vec<String> = parsed.get("payload").unwrap().as_sequence().unwrap()
            .iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let expected: Vec<String> = fixture["expected_provider_payload"]
            .as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(payload, expected);
    }

    #[test]
    fn classify_rejects_injection_chars() {
        // 命门②:逗号/换行/双引号/空格污染 host → 拒绝(不落库,输出面无从注入);正常域名仍过。
        assert_eq!(classify("evil,MATCH"), None);
        assert_eq!(classify("a\nb"), None);
        assert_eq!(classify("a\"b"), None);
        assert_eq!(classify("a b"), None);
        assert_eq!(classify("ok.example.com"), Some(("domain".into(), "ok.example.com".into())));
    }

    #[test]
    fn provider_and_pac_skip_malicious_pattern() {
        // 命门②出口再校验:存量脏规则(绕过 classify 直接构造)含 DANGER 字符 → 三输出面整条跳过,
        // 不再依赖编码转义"带毒保真"。断言脏条完全不出现、干净条仍在。
        let evil_ip = rl("ip", "10.0.0.0/8\nMATCH,DIRECT", 1); // 换行注入
        let evil_dom = rl("domain", "victim.example,DIRECT", 1); // 逗号注入(会让第三字段变策略)
        let clean = rl("ip", "10.1.0.0/16", 1);
        let rs = vec![evil_ip, evil_dom, clean];

        let prov = clash_provider_text(&rs);
        assert!(!prov.contains("MATCH,DIRECT"), "换行脏条被跳过");
        assert!(!prov.contains("victim.example"), "逗号脏条被跳过");
        assert!(prov.contains("IP-CIDR,10.1.0.0/16"), "干净条仍在");
        let parsed: serde_yaml::Value = serde_yaml::from_str(&prov).unwrap();
        assert_eq!(parsed["payload"].as_sequence().unwrap().len(), 1, "只剩 1 条干净规则");

        let pac = pac_text(&rs, "7899");
        assert!(!pac.contains("victim.example"), "PAC 也跳过逗号脏域名");
        assert!(!pac.contains("MATCH,DIRECT"));
    }

    #[test]
    fn every_text_output_skips_invalid_cidr_domain_and_unknown_kind() {
        let rs = vec![
            rl("domain", "ÄBC.中国", 1),
            rl("ip", "10.0.0.0/99", 1),
            rl("domain", "bad..example", 1),
            rl("unknown", "unknown.example", 1),
        ];
        let provider = clash_provider_text(&rs);
        let snippet = clash_snippet_text(&rs, "7899", "8787");
        let pac = pac_text(&rs, "7899");
        for output in [&provider, &snippet, &pac] {
            assert!(!output.contains("10.0.0.0/99"));
            assert!(!output.contains("bad..example"));
            assert!(!output.contains("unknown.example"));
        }
        assert!(provider.contains("äbc.中国"));
        assert!(snippet.contains("äbc.中国"));
        assert!(pac.contains("äbc.中国"));
    }

    #[test]
    fn clash_snippet_inline_has_no_resolve_and_node() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "*.y.com", 1)];
        let out = clash_snippet_text(&rs, "7899", "8787");
        assert!(out.contains("name: vpn-router"));
        assert!(out.contains("port: 7899"));
        assert!(out.contains("http://127.0.0.1:8787/clash/vpn-rules.yaml"));
        assert!(out.contains("IP-CIDR,10.0.0.0/8,vpn-router,no-resolve"));
        assert!(out.contains("DOMAIN-SUFFIX,y.com,vpn-router,no-resolve"));
    }

    #[test]
    fn pac_text_ipv4_netmask_and_domains() {
        let rs = vec![rl("ip", "10.0.0.0/8", 1), rl("domain", "x.com", 1), rl("ip", "fd00::/8", 1)];
        let out = pac_text(&rs, "7899");
        assert!(out.contains("SOCKS5 127.0.0.1:7899"));
        assert!(out.contains(r#"["10.0.0.0","255.0.0.0"]"#), "v4 netmask 由 ipnet 算");
        assert!(out.contains(r#""x.com""#));
        assert!(!out.contains("fd00"), "v6 网段 PAC 略过(isInNet 仅 v4)");
    }
}
