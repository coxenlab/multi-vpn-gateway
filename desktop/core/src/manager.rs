//! Docker 编排 + mihomo 热加载 + SOCKS5 探活。对照 app/manager.py。
use anyhow::{anyhow, Result};
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
        } else {
            // 对照 manager.py:非 "ip" 一律 DOMAIN-SUFFIX(catch-all,非仅 "domain")。命门 #2 不对称不变。
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

/// mihomo 宿主侧工作副本路径:env MIHOMO_CONFIG_PATH,默认 /cfg/config.yaml(对照 manager.py CFG)。
/// compose:即共享挂载本身;host-VM:`<data_dir>/config.yaml`(由 infra::ensure_params 注入),
/// 投递进容器靠 put_archive,见 [`rebuild`] 与 [`crate::infra::ensure_mihomo`]。
pub fn mihomo_config_path() -> String {
    std::env::var("MIHOMO_CONFIG_PATH").unwrap_or_else(|_| "/cfg/config.yaml".into())
}

/// 命门 #3:写 CFG + PUT /configs?force=true(不重启 mihomo、不断连)。返回状态码串或错误串。
///
/// host-VM 模型下 mihomo 跑在 VM 容器里、宿主无 `/cfg` 共享挂载,故:读宿主工作副本
/// (`mihomo_config_path()`)当 base、并入通道/规则、写回工作副本,再经 `put_archive` 把成品
/// 投递进容器 `/cfg/config.yaml`(`docker` 为 Some 时),最后 PUT 让 mihomo 从容器内绝对路径重载。
/// compose 模型下宿主写的就是共享挂载本身,put_archive 失败被忽略(容器名不同),不影响重载。
pub async fn rebuild(cfg: &Config, docker: Option<&bollard::Docker>, db: &std::path::Path) -> String {
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
        std::fs::write(&cfg_path, &yaml)?; // 宿主工作副本(compose:即共享挂载)
        if let Some(d) = docker {
            // host-VM:投递进容器卷;best-effort(compose 下容器名不同会失败,但共享挂载已落地)
            let _ = crate::docker::put_file(
                d,
                crate::infra::MIHOMO_CONTAINER,
                crate::infra::MIHOMO_CFG_DIR,
                crate::infra::MIHOMO_CFG_FILE,
                yaml.as_bytes(),
            )
            .await;
        }
        let client = reqwest::Client::new();
        let resp = client
            .put(format!("{}/configs", cfg.mihomo_ctrl_url))
            .query(&[("force", "true")])
            .bearer_auth(&cfg.mihomo_secret)
            .json(&serde_json::json!({ "path": crate::infra::MIHOMO_CFG_PATH }))
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

// ── Task 3: probe(命门 #1)+ 日志折叠 ───────────────────────────────────────

/// 探活 sidecar 镜像:带 curl + ca-certificates 的自建 oss 镜像(spec §9 首启即内置/docker-load)。
/// 任意通道类型(含 EC/aTrust)都用它当 socks5h 客户端,与被探的 vpn 容器类型无关。
const PROBE_IMAGE: &str = "vpnmgr/oss-vpn:latest";

/// 命门 #1:唯一登录成功判据。**host-VM 模型**:Rust core 在宿主、够不着 VM 内网 172.x,
/// 故探活搬进 VM 内一次性容器(`vpnmgr_vpnnet` 上),`curl --socks5-hostname vpn-{id}:1080`
/// (socks5h 远程解析)打 probe_url。语义不变:容忍自签证书(`-k`)、6s 超时、status<500 才算通。
/// docker 不可用 / 空 probe_url / 起不来 →(false, None)。
pub async fn probe(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic) -> (bool, Option<i64>) {
    if ch.probe_url.is_empty() {
        return (false, None);
    }
    let docker = match docker {
        Some(d) => d,
        None => return (false, None),
    };
    let proxy = format!("vpn-{}:1080", ch.id);
    let name = format!("vpncore-probe-{}", ch.id);
    let cmd = vec![
        "curl", "-s", "-o", "/dev/null",
        "-w", "%{http_code} %{time_total}",
        "--socks5-hostname", proxy.as_str(),
        "-k", "--max-time", "6",
        ch.probe_url.as_str(),
    ];
    match crate::docker::run_oneshot_capture(docker, &name, PROBE_IMAGE, cmd, &cfg.vpn_net).await {
        Ok(out) => parse_probe_output(&out),
        Err(_) => (false, None),
    }
}

/// 解析 `curl -w '%{http_code} %{time_total}'` 输出 →(status<500 且非 000,时延 ms)。
/// curl 连不上 → http_code=000 →(false, None),保持命门 #1「探不通即未登录」。
pub fn parse_probe_output(out: &str) -> (bool, Option<i64>) {
    let mut it = out.split_whitespace();
    let code: u32 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let secs: f64 = it.next().and_then(|s| s.parse().ok()).unwrap_or(0.0);
    if code == 0 {
        return (false, None);
    }
    (code < 500, Some((secs * 1000.0) as i64))
}

/// 折叠相邻完全相同的行(对照 manager.logs):重复 n>1 次时,首行后插
/// "  ⋯ 上一行重复 {n-1} 次"(标记额外重复次数;manager.py 用总次数 n,本端取
/// 额外次数 n-1,语义更准 —— 行已显示一次)。
pub fn dedup_log_lines(lines: Vec<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let cur = &lines[i];
        let mut n = 1;
        while i + n < lines.len() && lines[i + n] == *cur {
            n += 1;
        }
        out.push(cur.clone());
        if n > 1 {
            out.push(format!("  ⋯ 上一行重复 {} 次", n - 1));
        }
        i += n;
    }
    out
}

/// 容器日志(tail)+ 折叠。错误 → 单行说明。对照 manager.logs。
pub async fn logs(docker: &bollard::Docker, cid: &str, tail: i64) -> Vec<String> {
    let name = format!("vpn-{cid}");
    match crate::docker::raw_logs(docker, &name, tail).await {
        Ok(lines) => dedup_log_lines(lines),
        Err(e) => vec![format!("<no logs: {e}>")],
    }
}

// ── Task 4: create_channel + stop/start/remove/novnc_port ─────────────────

/// 对照 create_channel:起容器并(hagb/byo)返回 novnc 端口;oss 返回 None
/// (凭据注入由调用方紧随其后经 oss_connect 做,见命门 #5)。返回 (container_id, novnc_port)。
///
/// 宿主架构差异:Python 用 `socket.gethostbyname("mihomo")` 设 oss 容器 DNS;Rust core
/// 在宿主、不在 docker 网,改 inspect mihomo 在 vpn-net 上的 IP(best-effort,失败则不设 DNS,
/// 对齐 Python 吞 OSError)。
pub async fn create_channel(
    docker: &bollard::Docker,
    cfg: &Config,
    ch: &ChannelPublic,
    vnc_pwd: &str,
) -> Result<(String, Option<i64>)> {
    let spec = crate::registry::get(&ch.vpn_type)?;
    let mac = ch.mac.clone().unwrap_or_default();
    let plan = crate::adapters::build_run_kwargs(
        &ch.id, &mac, ch.ec_ver.as_deref(), &spec, vnc_pwd, &cfg.vpn_net,
    )?;

    let dns = if spec.runtime == "oss" {
        crate::docker::container_ip_on_net(docker, crate::infra::MIHOMO_CONTAINER, &cfg.vpn_net)
            .await
            .map(|ip| vec![ip])
    } else {
        None
    };

    let id = crate::docker::create_from_plan(docker, &plan, dns).await?;

    if spec.runtime == "oss" {
        // 凭据注入(oss_connect)在调用方紧随其后做;这里只起容器。
        return Ok((id, None));
    }
    // hagb/byo:读 host 映射的 noVNC 端口。对照 Python create_channel 的严格 int(c.ports[...]):
    // 端口缺失(竞态/未映射)→ 硬失败(create 报错),不静默落 None;按需的 novnc_port() 仍宽松(对齐 Python 两个读法的切分)。
    let port = crate::docker::novnc_port(docker, &ch.id)
        .await
        .ok_or_else(|| anyhow!("no 8080/tcp HostPort for vpn-{}", ch.id))?;
    Ok((id, Some(port)))
}

/// docker stop(忽略不存在)。
pub async fn stop(docker: &bollard::Docker, cid: &str) -> Result<()> {
    crate::docker::stop(docker, &format!("vpn-{cid}")).await
}

/// 原地 start —— 仅 byo(命门:hagb/oss 走重建 create_channel)。
pub async fn start(docker: &bollard::Docker, cid: &str) -> Result<()> {
    crate::docker::start(docker, &format!("vpn-{cid}")).await
}

/// 删容器(忽略不存在)。
pub async fn remove(docker: &bollard::Docker, cid: &str) -> Result<()> {
    crate::docker::rm_force(docker, &format!("vpn-{cid}")).await
}

/// 读 noVNC 端口(转发 docker.rs)。
pub async fn novnc_port(docker: &bollard::Docker, cid: &str) -> Option<i64> {
    crate::docker::novnc_port(docker, cid).await
}

// ── Task 5: oss_connect + sh + 文件注入(命门 #5)+ ensure_novnc_bridge ────

/// oss 注入动作(命门 #5:secret 只经 stdin/文件,非 secret 经 sh 转义进 argv)。
#[derive(Debug, Clone, PartialEq)]
pub enum OssAction {
    Feed { cmd: Vec<String>, secret: String },   // exec_inject_stdin(密码经 stdin)
    WriteFile { path: String, content: String },  // umask 077; cat >(私钥/配置经文件)
    Exec { cmd: Vec<String> },                    // detach exec(fire-and-forget,对照 Python detach=True)
}

/// POSIX 单引号转义(对照 _sh):非密参数进 sh -c 用。
pub fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn cfg_str<'a>(c: &'a serde_json::Map<String, serde_json::Value>, k: &str) -> &'a str {
    c.get(k).and_then(|v| v.as_str()).unwrap_or("")
}

/// 对照 oss_connect 的命令构造(纯函数,便于测 argv/stdin 切分,命门 #5)。
/// config 是 store::get_config 解密后的明文。
pub fn oss_plan(
    protocol: &str,
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<OssAction>> {
    // server/username strip 首尾空白(对照 manager.py:尾随空格的用户名被网关拒登);密码不 strip(可能合法含空格)。
    let server = cfg_str(config, "server").trim().to_string();
    let user = cfg_str(config, "username").trim().to_string();
    let pwd = cfg_str(config, "password").to_string();
    match protocol {
        "anyconnect" | "gp" | "fortinet" | "nc" | "pulse" => {
            let cmd = format!(
                "openconnect --protocol={} --user={} --passwd-on-stdin --non-inter --background --script /usr/share/vpnc-scripts/vpnc-script {} >/tmp/connect.log 2>&1",
                protocol, sh(&user), sh(&server)
            );
            Ok(vec![OssAction::Feed { cmd: vec!["sh".into(), "-c".into(), cmd], secret: pwd }])
        }
        "openvpn" => {
            let ovpn = cfg_str(config, "config_file").to_string();
            let mut actions = vec![OssAction::WriteFile { path: "/config/client.ovpn".into(), content: ovpn }];
            let auth = if !user.is_empty() && !pwd.is_empty() {
                actions.push(OssAction::WriteFile { path: "/config/auth.txt".into(), content: format!("{user}\n{pwd}\n") });
                "--auth-user-pass /config/auth.txt "
            } else {
                ""
            };
            let cmd = format!("openvpn --config /config/client.ovpn {auth}--daemon >/tmp/connect.log 2>&1");
            actions.push(OssAction::Exec { cmd: vec!["sh".into(), "-c".into(), cmd] });
            Ok(actions)
        }
        "wireguard" => {
            let conf = cfg_str(config, "config_file").to_string();
            Ok(vec![
                OssAction::WriteFile { path: "/config/wg0.conf".into(), content: conf },
                OssAction::Exec { cmd: vec!["sh".into(), "-c".into(), "wg-quick up /config/wg0.conf >/tmp/connect.log 2>&1".into()] },
            ])
        }
        "openfortivpn" => {
            // 简化:host/user/password 全写进 /config/forti.conf(命门 #5:密码不进 argv)。
            // TODO 后续加 _forti_cert_digest 的 trusted-cert TOFU pin(自签网关否则可能拒连)。
            let host = server.split("://").last().unwrap_or(&server).to_string();
            let conf = format!("host = {host}\nusername = {user}\npassword = {pwd}\n");
            Ok(vec![
                OssAction::WriteFile { path: "/config/forti.conf".into(), content: conf },
                OssAction::Exec { cmd: vec!["sh".into(), "-c".into(), "openfortivpn -c /config/forti.conf --persistent=20 >/tmp/connect.log 2>&1".into()] },
            ])
        }
        other => Err(anyhow!("unknown oss protocol: {other}")),
    }
}

/// 执行 oss_plan 的动作(命门 #5:Feed/WriteFile 经 stdin/文件,Exec 走 detach)。
pub async fn oss_connect(
    docker: &bollard::Docker,
    cid: &str,
    protocol: &str,
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<()> {
    let name = format!("vpn-{cid}");
    for action in oss_plan(protocol, config)? {
        match action {
            OssAction::Feed { cmd, secret } => {
                let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
                crate::docker::exec_inject_stdin(docker, &name, argv, format!("{secret}\n").as_bytes()).await?;
            }
            OssAction::WriteFile { path, content } => {
                let script = format!("umask 077; cat > {}", sh(&path));
                // 对照 Python _feed_stdin:内容尾部补 \n(client.ovpn/wg0.conf/auth.txt/forti.conf 字节对齐)。
                let body = format!("{content}\n");
                crate::docker::exec_inject_stdin(docker, &name, vec!["sh", "-c", &script], body.as_bytes()).await?;
            }
            OssAction::Exec { cmd } => {
                // detach:openfortivpn --persistent 等前台进程不退出,exec_capture 会挂死(对照 Python detach=True)。
                let argv: Vec<&str> = cmd.iter().map(String::as_str).collect();
                crate::docker::exec_detach(docker, &name, argv).await?;
            }
        }
    }
    Ok(())
}

/// arm64 noVNC 自愈:root 起 websockify 8082→5901(best-effort,对照 ensure_novnc_bridge)。
/// detach:脚本含至多 9s 的 5901 等待循环,不应阻塞调用方(对照 Python detach=True)。
pub async fn ensure_novnc_bridge(docker: &bollard::Docker, cid: &str) {
    let name = format!("vpn-{cid}");
    let script = "ss -tln 2>/dev/null | grep -q :8082 && exit 0; \
                  for i in $(seq 1 30); do ss -tln 2>/dev/null | grep -q :5901 && break; sleep 0.3; done; \
                  websockify --daemon 127.0.0.1:8082 127.0.0.1:5901 >/tmp/novnc-bridge.log 2>&1";
    let _ = crate::docker::exec_detach(docker, &name, vec!["sh", "-c", script]).await;
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

    // ── Task 3: probe + log dedup ──────────────────────────────────────────
    #[tokio::test]
    async fn probe_empty_url_is_false() {
        let c = ch("a"); // probe_url 空 → 不碰 docker 直接 false
        let cfg = Config::from_getter(|_| None);
        let (ok, ms) = probe(None, &cfg, &c).await;
        assert!(!ok);
        assert!(ms.is_none());
    }

    #[test]
    fn probe_output_parsing() {
        // 命门 #1 语义:<500 算通 + 时延;000(连不上)→ false/None;4xx 也算通(内网门户常 401/403)
        assert_eq!(parse_probe_output("200 0.234"), (true, Some(234)));
        assert_eq!(parse_probe_output("302 1.5"), (true, Some(1500)));
        assert_eq!(parse_probe_output("403 0.1"), (true, Some(100)));
        assert_eq!(parse_probe_output("500 0.2"), (false, Some(200)));
        assert_eq!(parse_probe_output("000 0.000"), (false, None));
        assert_eq!(parse_probe_output(""), (false, None));
    }

    #[test]
    fn dedup_collapses_adjacent_repeats() {
        let lines = vec![
            "warn X".to_string(), "warn X".to_string(), "warn X".to_string(),
            "ok".to_string(), "warn X".to_string(),
        ];
        let out = dedup_log_lines(lines);
        // 第一行 + 折叠标记 + ok + 再次 warn X
        assert_eq!(out[0], "warn X");
        assert!(out[1].contains("上一行重复") && out[1].contains("2"), "3 次 → 标记重复 2 次");
        assert_eq!(out[2], "ok");
        assert_eq!(out[3], "warn X");
    }

    // ── Task 4: create_channel 签名占位 ────────────────────────────────────
    #[test]
    fn create_channel_signature_compiles() {
        // 纯编译/类型存在性占位(真实起容器在 ignore 测/手动验)
        let _ = create_channel;
        let _ = stop;
        let _ = start;
        let _ = remove;
        let _ = novnc_port;
    }

    // ── Task 5: oss_plan + sh(命门 #5:argv/stdin 切分) ────────────────────
    #[test]
    fn sh_escapes_single_quotes() {
        assert_eq!(sh("a'b"), "'a'\\''b'");
        assert_eq!(sh("plain"), "'plain'");
    }

    #[test]
    fn oss_plan_anyconnect_password_via_stdin_not_argv() {
        let mut cfg = serde_json::Map::new();
        cfg.insert("server".into(), serde_json::json!("vpn.corp.com"));
        cfg.insert("username".into(), serde_json::json!("alice"));
        cfg.insert("password".into(), serde_json::json!("p@ss w0rd"));
        let actions = oss_plan("anyconnect", &cfg).unwrap();
        match &actions[0] {
            OssAction::Feed { cmd, secret } => {
                let joined = cmd.join(" ");
                assert!(joined.contains("openconnect"));
                assert!(joined.contains("--protocol=anyconnect"));
                assert!(joined.contains("vpn.corp.com"), "server 在 argv(已转义)");
                assert!(joined.contains("alice"), "user 在 argv");
                assert!(!joined.contains("p@ss w0rd"), "命门 #5:密码绝不在 argv");
                assert_eq!(secret, "p@ss w0rd"); // 走 stdin
            }
            _ => panic!("expected Feed"),
        }
    }

    #[test]
    fn oss_plan_openvpn_config_file_via_write_not_argv() {
        let mut cfg = serde_json::Map::new();
        cfg.insert("config_file".into(), serde_json::json!("client\nremote vpn 1194\n<secret-key>"));
        let actions = oss_plan("openvpn", &cfg).unwrap();
        // 有 WriteFile 写 .ovpn(私钥经文件,不进 argv)
        assert!(actions.iter().any(|a| matches!(a, OssAction::WriteFile { path, .. } if path.contains(".ovpn") || path.contains("client"))));
        // 最终 Exec openvpn,argv 不含私钥内容
        assert!(actions.iter().any(|a| matches!(a, OssAction::Exec { cmd } if cmd.join(" ").contains("openvpn"))));
        for a in &actions {
            if let OssAction::Exec { cmd } = a {
                assert!(!cmd.join(" ").contains("secret-key"), "命门 #5:私钥不进 argv");
            }
        }
    }
}
