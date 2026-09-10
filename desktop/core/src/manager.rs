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

/// 取 CIDR 前缀长度(`10.20.30.40/32` → 32,`fd00::/8` → 8)。裸 IP(无 `/`,理论上
/// `_classify` 已补 /32、/128)保守视为最具体(128),确保它排在任何网段之前。
fn cidr_prefix_len(pattern: &str) -> u8 {
    pattern
        .rsplit('/')
        .next()
        .and_then(|s| s.parse::<u8>().ok())
        .unwrap_or(128)
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
    // 命门 #2:mihomo 是**首个命中**(非最长前缀),故 IP-CIDR 须按前缀长度**降序**产出
    // ——更具体的绑定(/32)排在宽网段(/24)之前,否则跨通道撞同一私网段时宽段会盖住具体主机
    // 绑定(见「客户A .73 被客户B /24 盖住」)。同前缀长度保持插入顺序(stable sort);域名保持原序,
    // 与 IP 分组互不干扰(域名匹配域名连接、IP-CIDR 匹配 IP 连接,二者正交)。
    let mut domain_out: Vec<String> = Vec::new();
    let mut ip_out: Vec<(u8, String)> = Vec::new();
    for r in rules {
        if r.enabled == 0 {
            continue;
        }
        let Some((kind, pattern)) = crate::webutil::normalize_stored_rule(&r.kind, &r.pattern) else {
            continue;
        };
        if kind == "ip" {
            ip_out.push((
                cidr_prefix_len(&pattern),
                format!("IP-CIDR,{pattern},ch-{},no-resolve", r.channel_id),
            ));
        } else {
            domain_out.push(format!("DOMAIN-SUFFIX,{pattern},ch-{}", r.channel_id));
        }
    }
    ip_out.sort_by_key(|(plen, _)| std::cmp::Reverse(*plen)); // 稳定降序:前缀越长(越具体)越靠前
    let mut out: Vec<String> = domain_out;
    out.extend(ip_out.into_iter().map(|(_, text)| text));
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

fn atomic_write_0600(path: &std::path::Path, content: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let parent = path.parent().filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("config");
    let mut opened = None;
    for _ in 0..16 {
        let tmp = parent.join(format!(".{name}.{:016x}.tmp", rand::random::<u64>()));
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp) {
            Ok(file) => {
                opened = Some((tmp, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let (tmp, mut file) = opened.ok_or_else(|| anyhow!("could not create unique config temp file"))?;
    let result = (|| -> Result<()> {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(content)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// 串行化并发 rebuild：api.rs 有 8 处调用点，加上 rebuild 末尾 spawn 的 tun_sync 会并发触发；
/// 两个 rebuild 交错「读 base → 写盘」会互相冲掉对方并入的非托管键(dns/listeners)、或让读者看到
/// 半截配置。全程持锁把「读-改-原子写-put_file-PUT reload」串成一个原子序列。
static REBUILD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 命门 #3:写 CFG + PUT /configs?force=true(不重启 mihomo、不断连)。返回状态码串或错误串。
///
/// host-VM 模型下 mihomo 跑在 VM 容器里、宿主无 `/cfg` 共享挂载,故:读宿主工作副本
/// (`mihomo_config_path()`)当 base、并入通道/规则、写回工作副本,再经 `put_archive` 把成品
/// 投递进容器 `/cfg/config.yaml`(`docker` 为 Some 时),最后 PUT 让 mihomo 从容器内绝对路径重载。
/// `docker` 为 Some 时 put_file 是配置进容器的**唯一通道**,失败即判整体重载失败(返回 "put_file failed: ..."、
/// reload_ok 判 false,端点不误报绑定成功);`docker` 为 None(bind-mount / 无 docker 测试)时跳过投递、直接 PUT。
pub async fn rebuild(cfg: &Config, docker: Option<&bollard::Docker>, db: &std::path::Path) -> String {
    let _guard = REBUILD_LOCK.lock().await; // 全程持锁,读-改-写-重载串行(见 REBUILD_LOCK 说明)
    let inner = async {
        let channels = crate::store::list_channels(db)?;
        let rules = crate::store::effective_rules(db)?;
        let cfg_path = mihomo_config_path();
        // base 读取:NotFound = 首启合法(空 Mapping);其余 IO 错误 / YAML 解析错误 → 不写盘、
        // 直接上抛(配置文件保持原样)。否则一次读失败就把 dns/listeners 等非托管键整份冲掉,
        // oss 容器依赖 mihomo DoH+listen:53,冲掉即容器断解析。
        let base: Yaml = match std::fs::read_to_string(&cfg_path) {
            Ok(s) => serde_yaml::from_str(&s).map_err(|e| anyhow!("config parse error: {e}"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Yaml::Mapping(serde_yaml::Mapping::new())
            }
            Err(e) => return Err(anyhow!("config read error: {e}")),
        };
        let merged = build_mihomo_config(base, &channels, &rules);
        let yaml = serde_yaml::to_string(&merged)?;
        // 唯一同目录 0600 临时文件,flush+fsync 后原子 rename；读者不见半截或宽权限配置。
        atomic_write_0600(std::path::Path::new(&cfg_path), yaml.as_bytes())?;
        if let Some(d) = docker {
            // host-VM 模型:put_file 经 put_archive 把成品投递进容器,是配置进容器的**唯一通道**。
            // 失败即整体重载失败——不再发 PUT(否则 mihomo reload 的是容器里的旧配置)、不返回裸 2xx,
            // 让 reload_ok 判 false、端点不误报绑定成功(此前 let _ 吞掉后仍并入 "204 (put_file failed:...)",
            // reload_ok 只看首个 2xx token 就误判成功)。docker 为 None(bind-mount/无 docker 测试)不投递。
            if let Err(e) = crate::docker::put_file(
                d,
                crate::infra::MIHOMO_CONTAINER,
                crate::infra::MIHOMO_CFG_DIR,
                crate::infra::MIHOMO_CFG_FILE,
                yaml.as_bytes(),
            )
            .await
            {
                crate::ev!(error, "manager", "put_file_failed", "mihomo 配置投递失败",
                    { "container": crate::infra::MIHOMO_CONTAINER, "error": e.to_string() });
                return Ok::<String, anyhow::Error>(format!("put_file failed: {e}"));
            }
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
        Ok::<String, anyhow::Error>(resp.status().as_u16().to_string())
    };
    let status = match inner.await {
        Ok(s) => s,
        Err(e) => format!("{e}"),
    };
    // 层3 TUN 入口路由对账(best-effort):**detach** 到后台跑,不把 helper IPC 往返
    // (2s+8s 超时)串进每个 rebuild 调用点(规则增删改/建删通道/boot)的响应延迟。
    // 未启用时 tun_sync 立即早退;并发多次 ensure 各带全量 desired,helper 侧串行、末次收敛。
    let cfg_bg = cfg.clone();
    tokio::spawn(async move { crate::entry::tun_sync(&cfg_bg).await });
    status
}

// ── Task 3: probe(命门 #1)+ 日志折叠 ───────────────────────────────────────

/// 探活 sidecar 镜像:带 curl + ca-certificates 的自建 oss 镜像(spec §9 首启即内置/docker-load)。
/// 任意通道类型(含 EC/aTrust)都用它当 socks5h 客户端,与被探的 vpn 容器类型无关。
const PROBE_IMAGE: &str = "vpnmgr/oss-vpn:latest";

/// 命门 #1:唯一登录成功判据。**host-VM 模型**:Rust core 在宿主、够不着 VM 内网 172.x,
/// 故探活搬进 VM 内常驻探针(`vpnmgr_vpnnet` 上),`curl --socks5-hostname vpn-{id}:1080`
/// (socks5h 远程解析)打 probe_url。语义不变:容忍自签证书(`-k`)、6s 超时、status<500 才算通。
/// docker 不可用 / 空 probe_url / 起不来 →(false, None)。
pub async fn probe(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic) -> (bool, Option<i64>) {
    let first = probe_once(docker, cfg, ch).await;
    if !first.0 && recover_hagb_dns(docker, ch).await {
        // 修复标记不是登录判据:重走原 probe_url 的 SOCKS5 远端解析。
        return probe_once(docker, cfg, ch).await;
    }
    first
}

const DNS_RECOVERY_SCRIPT: &str = include_str!("../../../app/hagb_dns_recover.sh");

async fn recover_hagb_dns(docker: Option<&bollard::Docker>, ch: &ChannelPublic) -> bool {
    let Some(docker) = docker else { return false };
    let Ok(spec) = crate::registry::get(&ch.vpn_type) else { return false };
    let Some(tun) = spec.dns_recovery_tun else { return false };
    let Ok(url) = reqwest::Url::parse(&ch.probe_url) else { return false };
    let Some(host) = url.host_str() else { return false };
    if host.parse::<std::net::IpAddr>().is_ok() { return false; }
    let Some(port) = url.port_or_known_default() else { return false };
    let port = port.to_string();
    let name = format!("vpn-{}", ch.id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(18), crate::docker::exec_capture(
        docker, &name, vec!["timeout", "18", "bash", "-c", DNS_RECOVERY_SCRIPT, "vpnmgr-dns-recover", host, &port, url.scheme(), &tun],
    )).await;
    let recovered = matches!(result, Ok(Ok(ref s)) if s.lines().any(|l| l == "VPNMGR_DNS_RECOVERED"));
    if recovered {
        crate::ev!(info, "manager", "dns_cache_recovered", "通道域名代理已恢复,重新检测内网连通", { "cid": ch.id });
    } else if matches!(&result, Err(_) | Ok(Err(_))) || matches!(&result, Ok(Ok(s)) if s.contains("VPNMGR_DNS_START_FAILED") || s.contains("VPNMGR_DNS_NOT_READY") || s.contains("VPNMGR_DNS_OLD_LISTENER_BUSY")) {
        crate::ev!(warn, "manager", "dns_cache_recovery_failed", "通道域名代理恢复未完成,稍后可重试", { "cid": ch.id });
    }
    recovered
}

async fn probe_once(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic) -> (bool, Option<i64>) {
    if ch.probe_url.is_empty() {
        return (false, None);
    }
    let docker = match docker {
        Some(d) => d,
        None => return (false, None),
    };
    let proxy = format!("vpn-{}:1080", ch.id);
    let cmd = vec![
        "curl", "-s", "-o", "/dev/null",
        "-w", "%{http_code} %{time_total}",
        "--socks5-hostname", proxy.as_str(),
        "-k", "--max-time", "6",
        ch.probe_url.as_str(),
    ];
    match crate::docker::run_probe_capture(docker, cfg, PROBE_IMAGE, cmd).await {
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

/// 起容器，noVNC 端口按需在 login 接口建立。
/// (凭据注入由调用方紧随其后经 oss_connect 做,见命门 #5)。返回 (container_id, novnc_port)。
///
/// 宿主架构差异:Python 用 `socket.gethostbyname("mihomo")` 设 oss 容器 DNS;Rust core
/// 在宿主、不在 docker 网,改 inspect mihomo 在 vpn-net 上的 IP(best-effort,失败则不设 DNS,
/// 对齐 Python 吞 OSError)。
pub async fn create_channel(
    state: &crate::AppState,
    docker: &bollard::Docker,
    ch: &ChannelPublic,
    vnc_pwd: &str,
) -> Result<(String, Option<i64>)> {
    let cfg = &state.cfg;
    let spec = crate::registry::get(&ch.vpn_type)?;
    let mac = ch.mac.clone().unwrap_or_default();
    let plan = crate::adapters::build_run_kwargs(
        &ch.id, &mac, ch.ec_ver.as_deref(), &spec, vnc_pwd, &cfg.vpn_net,
    )?;

    // 对照 docker-py `containers.run()` 的 ImageNotFound 自动拉:镜像不在 VM 时走镜像源拉取。
    // bollard 的 create_container 不会自动拉,缺镜像会硬失败(EC 某 tag / aTrust 未预载即出错)。
    if let Some(image) = plan.config.image.as_deref() {
        ensure_image_mirrored(docker, cfg, image).await?;
    }

    let dns = if spec.runtime == "oss" {
        crate::docker::container_ip_on_net(docker, crate::infra::MIHOMO_CONTAINER, &cfg.vpn_net)
            .await
            .map(|ip| vec![ip])
    } else {
        None
    };

    crate::novnc::drop_for(state, &ch.id).await;
    let id = crate::docker::create_from_plan(docker, &plan, dns).await?;

    if spec.runtime == "hagb" {
        // aTrust 的 DNS 对内网域名优先回 IPv6 假地址(fdff:5341:4e47:464f::/64),而其
        // Linux 客户端的 IPv6 代理通路会 RST 一切连接(v4 假地址 198.18.x 通路正常),
        // 致 dante 按域名出站必败、按 IP 正常。glibc 侧强制 IPv4 优先即修;best-effort,
        // 失败不阻断建通道(EC 无此症状,同写无害)。
        let name = format!("vpn-{}", ch.id);
        if let Err(e) = crate::docker::exec_capture(
            docker,
            &name,
            vec!["sh", "-c", "echo 'precedence ::ffff:0:0/96 100' > /etc/gai.conf"],
        )
        .await
        {
            crate::ev!(warn, "manager", "gai_conf_failed", "写入容器 gai.conf(IPv4 优先)失败", { "channel": ch.id.clone(), "error": e.to_string() });
        }
    }
    // 显示入口失败不再把已启动的 VPN 容器判为创建失败；oss 凭据仍由调用方注入。
    Ok((id, None))
}

/// 对照 docker-py `containers.run()` 的自动拉:镜像已在 VM → 直接返回;否则走镜像源
/// `pull_retag`(非裸 docker.io —— 后者 CDN-EOF 常断;aTrust 须架构正确)拉取并重打回原 tag。
/// 镜像源取库内 enabled 的(无则用内置默认)。所有源失败 → Err(让 create 落 error、信息可读)。
/// 注:同步阻塞直至拉完(大镜像数分钟,一次性);需进度可改走 preflight::start_pull 后台任务。
pub async fn ensure_image_mirrored(docker: &bollard::Docker, cfg: &Config, image: &str) -> Result<()> {
    if crate::docker::image_present(docker, image).await == Some(true) {
        return Ok(());
    }
    let (repo, tag) = match image.split_once(':') {
        Some((r, t)) if !t.is_empty() => (r.to_string(), t.to_string()),
        _ => (image.trim_end_matches(':').to_string(), "latest".to_string()),
    };
    let host_arch = crate::registry::host_arch();
    let mut mirrors: Vec<String> = crate::store::list_mirrors(&cfg.db_path())
        .unwrap_or_default()
        .into_iter()
        .filter(|m| m.enabled != 0)
        .map(|m| m.host)
        .collect();
    if mirrors.is_empty() {
        mirrors = crate::preflight::DEFAULT_MIRRORS.iter().map(|s| s.to_string()).collect();
    }
    let mut errs: Vec<String> = Vec::new();
    for m in &mirrors {
        if !crate::preflight::mirror_reachable(m).await {
            errs.push(format!("{m} 不可达"));
            continue;
        }
        match crate::docker::pull_retag(docker, m, &repo, &tag, &host_arch).await {
            Ok(crate::docker::PullOutcome::Tagged(_)) => return Ok(()),
            Ok(crate::docker::PullOutcome::ArchMismatch(a)) => {
                errs.push(format!("{m} 拉到 {a}(非 {host_arch}),弃用"))
            }
            Err(e) => errs.push(format!("{m}: {e}")),
        }
    }
    Err(anyhow!("拉取镜像 {image} 失败(所有镜像源):{}", errs.join("; ")))
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
    forti_digest: Option<&str>,
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
            // 对照 manager.py:host:port 走 CLI 位置参数(openfortivpn CLI 自行拆端口),
            // conf 只放 password(+ trusted-cert),命门 #5 密码不进 argv。
            // 旧实现把 host:port 塞进 conf 的 `host =` 指令 → openfortivpn 把整串("ip:port")
            // 当主机名解析,getaddrinfo 直接失败(实测 "Name or service not known")。
            let host = server.split("://").last().unwrap_or(&server).to_string();
            let mut conf = format!("password = {pwd}\n");
            if let Some(d) = forti_digest.filter(|d| !d.is_empty()) {
                // 自签网关 TOFU pin(对照 _forti_cert_digest);拿不到则不写,等同不 pin。
                conf.push_str(&format!("trusted-cert = {d}\n"));
            }
            let run = format!(
                "openfortivpn {} -u {} -c /config/forti.conf --persistent=20 >/tmp/connect.log 2>&1",
                sh(&host), sh(&user)
            );
            Ok(vec![
                OssAction::WriteFile { path: "/config/forti.conf".into(), content: conf },
                OssAction::Exec { cmd: vec!["sh".into(), "-c".into(), run] },
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
    // openfortivpn:连接前先抓网关证书指纹做 trusted-cert(对照 _forti_cert_digest);
    // 自签 FortiGate 否则会拒连。拿不到(网关不可达等)→ None,不写 trusted-cert。
    let forti_digest = if protocol == "openfortivpn" {
        let server = cfg_str(config, "server").trim().to_string();
        let host = server.split("://").last().unwrap_or(&server).to_string();
        forti_cert_digest(docker, &name, &host).await
    } else {
        None
    };
    for action in oss_plan(protocol, config, forti_digest.as_deref())? {
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

/// 对照 _forti_cert_digest:openssl 单次 TLS 握手取网关证书 sha256(DER)指纹,作
/// openfortivpn 的 trusted-cert(TOFU,等同 FortiClient「信任此证书」)。只做握手 —— 不认证、
/// 不建隧道、不占 FortiGate 会话。拿不到(网关不可达/无 openssl 等)→ None。指纹非机密。
async fn forti_cert_digest(docker: &bollard::Docker, name: &str, host: &str) -> Option<String> {
    let sni = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    let cmd = format!(
        "openssl s_client -connect {} -servername {} </dev/null 2>/dev/null \
         | openssl x509 -outform DER 2>/dev/null | sha256sum",
        sh(host), sh(sni)
    );
    let out = crate::docker::exec_capture(docker, name, vec!["sh", "-c", &cmd]).await.ok()?;
    extract_hex64(&out)
}

/// 从 sha256sum 输出里抽出恰 64 位的十六进制串(对照 Python `\b([0-9a-fA-F]{64})\b`,无 regex 依赖)。
/// 只认「极大长度恰为 64」的 hex 串,避免误取更长 hex 的子串。
pub fn extract_hex64(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_hexdigit() {
            let start = i;
            while i < b.len() && b[i].is_ascii_hexdigit() {
                i += 1;
            }
            if i - start == 64 {
                return Some(s[start..i].to_string());
            }
        } else {
            i += 1;
        }
    }
    None
}

/// arm64 noVNC 自愈:root 起 websockify 8082→5901(best-effort,对照 ensure_novnc_bridge)。
/// detach:脚本含至多 9s 的 5901 等待循环,不应阻塞调用方(对照 Python detach=True)。
pub async fn ensure_novnc_bridge(docker: &bollard::Docker, cid: &str) {
    let name = format!("vpn-{cid}");
    let script = "ss -tln 2>/dev/null | grep -q :8082 && exit 0; \
                  for i in $(seq 1 30); do ss -tln 2>/dev/null | grep -q :5901 && break; sleep 0.3; done; \
                  websockify --daemon 127.0.0.1:8082 127.0.0.1:5901 >/tmp/novnc-bridge.log 2>&1";
    // best-effort:登录页有自身重试 UX,不阻塞调用方;仅告警,不上抛。
    if let Err(e) = crate::docker::exec_detach(docker, &name, vec!["sh", "-c", script]).await {
        crate::ev!(warn, "manager", "novnc_bridge_failed", "noVNC 桥拉起失败,登录页将自重试",
            { "cid": cid, "error": e.to_string() });
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
            routing_enabled: true,
        }
    }
    fn rule(cid: &str, kind: &str, pat: &str, enabled: i64) -> Rule {
        Rule { id: 0, channel_id: cid.into(), kind: kind.into(), pattern: pat.into(), enabled,
            note: String::new(), locked: 0 }
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
    fn mihomo_config_skips_malicious_pattern() {
        // 命门②出口再校验:落盘 mihomo 规则前,含 DANGER 字符的存量脏 pattern 整条跳过(不成注入行)。
        let base = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let chans = vec![ch("a")];
        let rules = vec![
            rule("a", "domain", "victim.example,DIRECT", 1), // 逗号注入(第三字段变策略)
            rule("a", "ip", "10.0.0.0/8\nMATCH,DIRECT", 1),  // 换行注入
            rule("a", "domain", "corp.example.com", 1),       // 干净
        ];
        let cfg = build_mihomo_config(base, &chans, &rules);
        let texts: Vec<String> = cfg.get("rules").unwrap().as_sequence().unwrap()
            .iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert!(!texts.iter().any(|t| t.contains("victim.example")), "逗号脏条被跳过");
        assert!(!texts.iter().any(|t| t.contains("10.0.0.0/8")), "换行脏条被跳过");
        assert!(texts.iter().any(|t| t == "DOMAIN-SUFFIX,corp.example.com,ch-a"), "干净条仍在");
        assert_eq!(texts.last().unwrap(), "MATCH,DIRECT");
        assert_eq!(texts.len(), 2, "只剩 1 条干净域名 + MATCH,DIRECT 兜底");
    }

    #[test]
    fn mihomo_config_skips_invalid_stored_semantics_and_normalizes_unicode() {
        let rules = vec![
            rule("a", "domain", "ÄBC.中国", 1),
            rule("a", "ip", "10.0.0.0/99", 1),
            rule("a", "domain", "bad..example", 1),
            rule("a", "unknown", "unknown.example", 1),
        ];
        let cfg = build_mihomo_config(
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()), &[ch("a")], &rules);
        let text: Vec<&str> = cfg["rules"].as_sequence().unwrap().iter()
            .map(|v| v.as_str().unwrap()).collect();
        assert_eq!(text, vec!["DOMAIN-SUFFIX,äbc.中国,ch-a", "MATCH,DIRECT"]);
    }

    #[test]
    fn atomic_config_write_replaces_with_mode_0600() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.yaml");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        atomic_write_0600(&path, b"new").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }

    #[tokio::test]
    async fn rebuild_put_file_failure_is_not_success() {
        // D 故障注入:docker 指向不存在的 socket → put_file 必失败(host-VM 下投递是配置进容器唯一通道)。
        // rebuild 应返回 "put_file failed: ..."(无裸 2xx),即使控制器 PUT 本会 204 也不返回成功语义
        //(此前吞掉后并入 "204 (put_file failed:...)" 被 reload_ok 首 token 误判成功)。
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        std::env::set_var("MIHOMO_CONFIG_PATH", dir.path().join("m.yaml"));
        let mut cfg = Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into();
        cfg.vm_profile = "vpnmgr-test".into();
        cfg.dev_mode = true;
        assert!(!cfg.host_integrations_allowed());
        // 造一个"存在但非 docker"的 sock 文件:connect_with_unix 只查路径存在性(构造通过),
        // 而 put_file 首个 API 调用连它即失败(非真 socket)→ 稳定触发 put_file 失败路径。
        let sock = dir.path().join("not-a-docker.sock");
        std::fs::write(&sock, b"").unwrap();
        let bad = bollard::Docker::connect_with_unix(
            sock.to_str().unwrap(), 5, bollard::API_DEFAULT_VERSION,
        )
        .unwrap();
        let status = rebuild(&cfg, Some(&bad), &db).await;
        assert!(status.starts_with("put_file failed:"), "put_file 失败应返回明确失败串,实际: {status}");
        // 复刻 api::reload_ok 的完整字符串解析:失败串不是裸 2xx 状态码 → 不判成功。
        assert!(status.parse::<u16>().is_err(), "失败串非裸状态码,reload_ok 判 false");
    }

    #[test]
    fn mihomo_config_ip_rules_most_specific_first() {
        // 命门 #2:首个命中语义下,更具体的前缀必须排在宽网段之前,否则跨通道 /24 盖住 /32
        //(复现「客户A .73/32 被客户B 10.20.30.0/24 盖住」)。插入顺序故意宽段在前。
        let base = serde_yaml::Value::Mapping(serde_yaml::Mapping::new());
        let chans = vec![ch("wide"), ch("host")];
        let rules = vec![
            rule("wide", "ip", "10.20.30.0/24", 1),
            rule("host", "ip", "10.20.30.40/32", 1),
            rule("wide", "ip", "10.0.0.0/8", 1),
            rule("host", "domain", "corp.example.com", 1),
        ];
        let cfg = build_mihomo_config(base, &chans, &rules);
        let rules_seq = cfg.get("rules").unwrap().as_sequence().unwrap();
        let texts: Vec<String> = rules_seq.iter().map(|v| v.as_str().unwrap().to_string()).collect();
        let pos = |needle: &str| texts.iter().position(|t| t.contains(needle)).unwrap();
        // /32 先于 /24 先于 /8:更具体的绑定优先命中
        assert!(pos("10.20.30.40/32") < pos("10.20.30.0/24"));
        assert!(pos("10.20.30.0/24") < pos("10.0.0.0/8"));
        assert_eq!(texts.last().unwrap(), "MATCH,DIRECT");
    }

    #[test]
    fn golden_mihomo_rules_match_fixture() {
        // 双栈 golden 契约(tests/fixtures/golden_rules.json,Python test_golden.py 同源消费):
        // 钉死 mihomo rules 的顺序/格式。key→固定 id 映射,expected 内 {ch0}/{ch1} 占位符按同映射
        // 替换;比对解析后的列表逐项相等(不比字节,PyYAML 与 serde_yaml 格式本就不同)。
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/golden_rules.json")).unwrap();
        // key→固定 id 映射(与 expected 内 {ch0}/{ch1} 占位符同源)。
        let id_of = |key: &str| -> &'static str {
            match key {
                "ch0" => "aaa111",
                "ch1" => "bbb222",
                other => panic!("golden fixture 出现未映射的 key: {other}"),
            }
        };
        let channels: Vec<ChannelPublic> = fixture["channels"]
            .as_array().unwrap().iter()
            .map(|c| ch(id_of(c["key"].as_str().unwrap())))
            .collect();
        let rules: Vec<Rule> = fixture["rules_insertion_order"]
            .as_array().unwrap().iter().enumerate()
            .map(|(i, r)| Rule {
                id: (i + 1) as i64, // ORDER BY id → 插入序
                channel_id: id_of(r["channel"].as_str().unwrap()).to_string(),
                kind: r["kind"].as_str().unwrap().to_string(),
                pattern: r["pattern"].as_str().unwrap().to_string(),
                enabled: r["enabled"].as_bool().unwrap() as i64,
                note: String::new(),
                locked: 0,
            })
            .collect();
        let cfg = build_mihomo_config(
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()), &channels, &rules);
        let got: Vec<String> = cfg.get("rules").unwrap().as_sequence().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string()).collect();
        let expected: Vec<String> = fixture["expected_mihomo_rules"]
            .as_array().unwrap().iter()
            .map(|v| v.as_str().unwrap().replace("{ch0}", "aaa111").replace("{ch1}", "bbb222"))
            .collect();
        assert_eq!(got, expected);
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
        let actions = oss_plan("anyconnect", &cfg, None).unwrap();
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
    fn oss_plan_openfortivpn_host_port_not_in_conf_host_directive() {
        // 回归:旧实现把 "ip:port" 塞进 conf 的 `host =` → openfortivpn getaddrinfo 整串失败。
        // 现在:host:port 走 CLI 位置参数;conf 只放 password(+ trusted-cert),不含 host=/username=。
        let mut cfg = serde_json::Map::new();
        cfg.insert("server".into(), serde_json::json!("203.0.113.10:10443"));
        cfg.insert("username".into(), serde_json::json!("alice"));
        cfg.insert("password".into(), serde_json::json!("p@ss w0rd"));
        let actions = oss_plan("openfortivpn", &cfg, Some("ABCDEF0123")).unwrap();

        let conf = actions.iter().find_map(|a| match a {
            OssAction::WriteFile { path, content } if path == "/config/forti.conf" => Some(content.clone()),
            _ => None,
        }).expect("forti.conf WriteFile");
        assert!(!conf.contains("host ="), "host 不进 conf(走 CLI):\n{conf}");
        assert!(!conf.contains("username ="), "username 不进 conf(走 CLI -u)");
        assert!(conf.contains("password = p@ss w0rd"), "password 进 conf(不进 argv,命门 #5)");
        assert!(conf.contains("trusted-cert = ABCDEF0123"), "有指纹时写 trusted-cert");

        let run = actions.iter().find_map(|a| match a {
            OssAction::Exec { cmd } => Some(cmd.join(" ")),
            _ => None,
        }).expect("openfortivpn Exec");
        assert!(run.contains("openfortivpn '203.0.113.10:10443'"), "host:port 整串走 CLI 位置参数(openfortivpn 自拆端口):\n{run}");
        assert!(run.contains("-u 'alice'"), "username 走 CLI -u");
        assert!(!run.contains("p@ss w0rd"), "命门 #5:密码绝不在 argv");
    }

    #[test]
    fn oss_plan_openfortivpn_no_digest_omits_trusted_cert() {
        let mut cfg = serde_json::Map::new();
        cfg.insert("server".into(), serde_json::json!("https://fw.example.com:443"));
        cfg.insert("username".into(), serde_json::json!("u"));
        cfg.insert("password".into(), serde_json::json!("pw"));
        let actions = oss_plan("openfortivpn", &cfg, None).unwrap();
        let conf = actions.iter().find_map(|a| match a {
            OssAction::WriteFile { content, .. } => Some(content.clone()), _ => None,
        }).unwrap();
        assert!(!conf.contains("trusted-cert"), "无指纹则不写 trusted-cert");
        // scheme 被剥离:host 走 CLI 用 fw.example.com:443
        let run = actions.iter().find_map(|a| match a {
            OssAction::Exec { cmd } => Some(cmd.join(" ")), _ => None,
        }).unwrap();
        assert!(run.contains("openfortivpn 'fw.example.com:443'"), "scheme 剥离后整串走 CLI:\n{run}");
    }

    #[test]
    fn extract_hex64_picks_sha256sum_line() {
        // sha256sum 输出形如 "<64hex>  -\n"
        let h = "a".repeat(64);
        assert_eq!(extract_hex64(&format!("{h}  -\n")), Some(h.clone()));
        assert_eq!(extract_hex64("not hex here"), None);
        assert_eq!(extract_hex64(&"a".repeat(63)), None, "63 位不取");
        assert_eq!(extract_hex64(&"a".repeat(65)), None, "65 位整串不取(非恰 64)");
    }

    #[test]
    fn oss_plan_openvpn_config_file_via_write_not_argv() {
        let mut cfg = serde_json::Map::new();
        cfg.insert("config_file".into(), serde_json::json!("client\nremote vpn 1194\n<secret-key>"));
        let actions = oss_plan("openvpn", &cfg, None).unwrap();
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
