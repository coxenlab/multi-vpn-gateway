//! Docker 编排 + mihomo 热加载 + SOCKS5 探活。对照 app/manager.py。
use std::collections::HashSet;
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

/// 内层入口的一条生效规则:已过滤停用、已出口再校验、已按首个命中顺序排好。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrderedRule {
    pub rule_id: i64,
    pub kind: String,
    pub pattern: String,
    pub channel_id: String,
    pub target: String,
}

/// 命门 #2:mihomo 是**首个命中**(非最长前缀),故规则须按范围从小到大产出。
/// 域名桶按标签数**降序**(`corp.example.com` 先于 `example.com`,否则跨通道时宽后缀盖住子域);
/// IP 桶按前缀长度**降序**(/32 先于 /24)。两桶都是稳定排序,同级保持插入顺序;
/// 域名桶整体在前,与 IP 分组互不干扰(域名匹配域名连接、IP-CIDR 匹配 IP 连接,二者正交)。
///
/// 目标:通道出错(`status == "error"`)、在掉线集合 `lost` 里，或者通用自装通道还没有
/// 用明确的验证地址通过实际探活时写 `REJECT`,在入口处阻止,
/// 不落到后面更宽的规则或直连;否则 `ch-{id}`。排序键不看目标,阻止的窄规则仍排在宽规则前。
/// 手动停止的通道规则已由 [`crate::store::effective_rules`] 折叠为停用,这里不再出现。
pub fn ordered_rules(channels: &[ChannelPublic], rules: &[Rule], lost: &HashSet<String>) -> Vec<OrderedRule> {
    let blocked: HashSet<&str> = channels.iter()
        .filter(|c| channel_blocked(c, lost))
        .map(|c| c.id.as_str())
        .collect();
    let mut out = sorted_rules(rules, true);
    for r in &mut out {
        if blocked.contains(r.channel_id.as_str()) || lost.contains(&r.channel_id) {
            r.target = "REJECT".into();
        }
    }
    out
}

/// 入口配置、探活返回和规则测试共用的阻止判据。通用自装客户端的 SOCKS 跟随
/// 容器默认路由，未连 VPN 时也可以通过公网，所以必须有非空验证地址且本次实测成功才能放行。
/// 对 custom 的明确 NotConnected 会立即回到 `running` 并阻止；探针自身故障不会改状态。
pub fn channel_blocked(ch: &ChannelPublic, lost: &HashSet<String>) -> bool {
    ch.status == "error"
        || lost.contains(&ch.id)
        || (ch.vpn_type == "custom" && ch.status != "stopped"
            && (ch.probe_url.is_empty() || ch.status != "logged_in"))
}

/// 过滤停用、出口再校验并分桶排序,目标一律 `ch-{id}`。`sort_domains` 为假时域名桶保持
/// 插入顺序(改动前的排法,只给 [`rule_diff`] 对照用)。
fn sorted_rules(rules: &[Rule], sort_domains: bool) -> Vec<OrderedRule> {
    let mut domain_out: Vec<(usize, OrderedRule)> = Vec::new();
    let mut ip_out: Vec<(u8, OrderedRule)> = Vec::new();
    for r in rules {
        if r.enabled == 0 {
            continue;
        }
        let Some((kind, pattern)) = crate::webutil::normalize_stored_rule(&r.kind, &r.pattern) else {
            continue;
        };
        let item = OrderedRule {
            rule_id: r.id,
            kind,
            pattern,
            channel_id: r.channel_id.clone(),
            target: format!("ch-{}", r.channel_id),
        };
        if item.kind == "ip" {
            ip_out.push((cidr_prefix_len(&item.pattern), item));
        } else {
            domain_out.push((item.pattern.split('.').count(), item));
        }
    }
    if sort_domains {
        domain_out.sort_by_key(|(n, _)| std::cmp::Reverse(*n)); // 稳定降序:标签越多(越具体)越靠前
    }
    ip_out.sort_by_key(|(plen, _)| std::cmp::Reverse(*plen)); // 稳定降序:前缀越长(越具体)越靠前
    domain_out.into_iter().map(|(_, r)| r)
        .chain(ip_out.into_iter().map(|(_, r)| r))
        .collect()
}

/// 命门 #2 的不对称:IP-CIDR 带 no-resolve,DOMAIN-SUFFIX 不带(阻止行同样如此)。
fn rule_line(r: &OrderedRule) -> String {
    if r.kind == "ip" {
        format!("IP-CIDR,{},{},no-resolve", r.pattern, r.target)
    } else {
        format!("DOMAIN-SUFFIX,{},{}", r.pattern, r.target)
    }
}

/// 命门 #2/#7:纯函数生成 mihomo 配置(读-改-写:保留 base 其它键)。掉线集合为空。
pub fn build_mihomo_config(base: Yaml, channels: &[ChannelPublic], rules: &[Rule]) -> Yaml {
    build_mihomo_config_with(base, channels, rules, &HashSet::new())
}

/// 同 [`build_mihomo_config`],另传掉线集合(`lifecycle::lost_channels`),其中通道的规则写成 `REJECT`。
pub fn build_mihomo_config_with(mut base: Yaml, channels: &[ChannelPublic], rules: &[Rule], lost: &HashSet<String>) -> Yaml {
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
    let mut out: Vec<String> = ordered_rules(channels, rules, lost).iter().map(rule_line).collect();
    out.push("IP-CIDR,198.18.0.0/15,REJECT,no-resolve".to_string());
    out.push("MATCH,DIRECT".to_string());

    if let Yaml::Mapping(m) = &mut base {
        m.insert(Yaml::String("proxies".into()), serde_yaml::to_value(&proxies).unwrap());
        m.insert(Yaml::String("proxy-groups".into()), Yaml::Sequence(vec![]));
        m.insert(Yaml::String("rules".into()), serde_yaml::to_value(&out).unwrap());
    }
    base
}

/// G-03 差异清单:导出文档里的通道一律当已连接(`error_names` 里的当出错),分别按改动前
/// (出错通道规则折叠、域名按插入顺序)和现在(出错通道 `REJECT`、域名按标签数)拼内层规则,
/// 逐条列出位置变化和改为阻止的行;都没有时为空。只读文档,不碰数据库和网络。
/// 导出不带规则的全局 id,插入顺序按文档里通道、规则的先后近似。
pub fn rule_diff(export_doc: &serde_json::Value, error_names: &[String]) -> Vec<String> {
    let mut names = std::collections::HashMap::new();
    let mut errored = HashSet::new();
    let mut rules = Vec::new();
    for (i, entry) in export_doc["channels"].as_array().into_iter().flatten().enumerate() {
        let id = format!("c{i}");
        let name = entry["name"].as_str().filter(|s| !s.is_empty()).map(String::from)
            .unwrap_or_else(|| format!("#{}", i + 1));
        if error_names.contains(&name) {
            errored.insert(id.clone());
        }
        names.insert(id.clone(), name);
        // 通道分流关着时两边都折叠。
        if entry["routing_enabled"] == false || entry["routing_enabled"] == 0 {
            continue;
        }
        for r in entry["rules"].as_array().into_iter().flatten() {
            let enabled = r["enabled"].as_i64().or_else(|| r["enabled"].as_bool().map(i64::from)).unwrap_or(1);
            rules.push(Rule {
                id: rules.len() as i64 + 1, channel_id: id.clone(),
                kind: r["kind"].as_str().unwrap_or_default().into(),
                pattern: r["pattern"].as_str().unwrap_or_default().into(),
                enabled, note: String::new(), locked: 0,
            });
        }
    }
    let before_rules: Vec<Rule> = rules.iter().filter(|r| !errored.contains(&r.channel_id)).cloned().collect();
    let before = sorted_rules(&before_rules, false);
    let after = ordered_rules(&[], &rules, &errored);
    let text = |r: &OrderedRule| format!("{},{}（{}）",
        if r.kind == "ip" { "IP-CIDR" } else { "DOMAIN-SUFFIX" }, r.pattern, names[&r.channel_id]);
    let mut out = Vec::new();
    // 只在两边都有的规则之间比先后,改为阻止的行插进来造成的整体下移不算位置变化。
    let common: Vec<(usize, &OrderedRule)> = after.iter().enumerate()
        .filter(|(_, r)| r.target != "REJECT").collect();
    for (k, (j, r)) in common.iter().enumerate() {
        let i = before.iter().position(|b| b.rule_id == r.rule_id).unwrap_or(k);
        if i != k {
            out.push(format!("位置变化：{}第 {} 条 → 第 {} 条", text(r), i + 1, j + 1));
        }
    }
    for (j, r) in after.iter().enumerate().filter(|(_, r)| r.target == "REJECT") {
        out.push(format!("改为阻止：{}第 {} 条", text(r), j + 1));
    }
    out
}

/// mihomo 宿主侧工作副本路径:env MIHOMO_CONFIG_PATH,默认 /cfg/config.yaml(对照 manager.py CFG)。
/// compose:即共享挂载本身;host-VM:`<data_dir>/config.yaml`(由 infra::ensure_params 注入),
/// 投递进容器靠 put_archive,见 [`rebuild`] 与 [`crate::infra::ensure_mihomo`]。
pub fn mihomo_config_path() -> String {
    std::env::var("MIHOMO_CONFIG_PATH").unwrap_or_else(|_| "/cfg/config.yaml".into())
}

pub(crate) fn atomic_write_0600(path: &std::path::Path, content: &[u8]) -> Result<()> {
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

/// 同进程排队；config_apply 另持跨进程文件锁。元信息/同内容不重复热加载。
static REBUILD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static GUARD_REQUESTS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, crate::vm::EgressPolicy>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

fn guard_changed(previous: &mut Option<crate::vm::EgressPolicy>, desired: &crate::vm::EgressPolicy) -> bool {
    if previous.as_ref() == Some(desired) { return false; }
    *previous = Some(desired.clone());
    true
}

fn schedule_egress_guard(cfg: &Config, docker: Option<&bollard::Docker>, db: &std::path::Path) {
    if !cfg.managed_vm || !cfg.host_integrations_allowed() || cfg.identity().tun_config_frozen { return; }
    let Some(docker) = docker else { return };
    let desired = match (crate::store::all_rules(db), crate::store::list_channels(db)) {
        (Ok(rules), Ok(channels)) => crate::vm::egress_policy(&rules, &channels),
        _ => { crate::ev!(warn, "vm", "egress_snapshot_failed", "规则已保存，出站守卫等待下次核对", {}); return; }
    };
    let key = format!("{}:{}", cfg.vm_profile, db.display());
    let Ok(mut requests) = GUARD_REQUESTS.lock() else { return };
    let mut previous = requests.get(&key).cloned();
    if !guard_changed(&mut previous, &desired) { return; }
    requests.insert(key, desired);
    drop(requests);
    let (cfg, docker, db) = (cfg.clone(), docker.clone(), db.to_owned());
    tokio::spawn(async move {
        if let Err(e) = crate::health::apply_egress_guard(&cfg, &docker, &db, true).await {
            crate::ev!(warn, "vm", "egress_guard_failed", "规则已保存，出站守卫等待下次核对", { "error": e.to_string() });
        }
    });
}

/// 热加载候选内容、读回托管规则/代理，再持久化启动文件；失败返回明确的部分完成状态。
pub async fn rebuild(cfg: &Config, docker: Option<&bollard::Docker>, db: &std::path::Path) -> String {
    let _guard = REBUILD_LOCK.lock().await;
    let status = match crate::config_apply::apply(cfg, docker, db).await {
        Ok(value) => value,
        Err(_) => "配置同步失败，已保存的设置仍待确认，请重试".into(),
    };
    // 层3 TUN 入口路由对账(best-effort):**detach** 到后台跑,不把 helper IPC 往返
    // (2s+8s 超时)串进每个 rebuild 调用点(规则增删改/建删通道/boot)的响应延迟。
    // 未启用时 tun_sync 立即早退;并发多次 ensure 各带全量 desired,helper 侧串行、末次收敛。
    if !cfg.managed_vm || docker.is_some() {
        let cfg_bg = cfg.clone();
        tokio::spawn(async move { crate::entry::tun_sync(&cfg_bg).await });
    }
    schedule_egress_guard(cfg, docker, db);
    status
}

// ── Task 3: probe(命门 #1)+ 日志折叠 ───────────────────────────────────────

/// 探活 sidecar 镜像:带 curl + ca-certificates 的自建 oss 镜像(spec §9 首启即内置/docker-load)。
/// 任意通道类型(含 EC/aTrust)都用它当 socks5h 客户端,与被探的 vpn 容器类型无关。
pub(crate) const PROBE_IMAGE: &str = "vpnmgr/oss-vpn:latest";

/// 命门 #1:唯一登录成功判据。**host-VM 模型**:Rust core 在宿主、够不着 VM 内网 172.x,
/// 故探活搬进 VM 内常驻探针(`vpnmgr_vpnnet` 上),`curl --socks5-hostname vpn-{id}:1080`
/// (socks5h 远程解析)打 probe_url。语义不变:容忍自签证书(`-k`)、6s 超时、status<500 才算通。
/// docker 不可用 / 空 probe_url / 起不来 →(false, None)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Connected { latency: Option<i64> },
    NotConnected,
    ProbeFailed { reason: String },
}

impl ProbeOutcome {
    pub fn failed(reason: impl Into<String>) -> Self { Self::ProbeFailed { reason: reason.into() } }
}

pub async fn probe(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic) -> ProbeOutcome {
    let first = probe_once(docker, cfg, ch).await;
    if first == ProbeOutcome::NotConnected && recover_hagb_dns(docker, ch).await {
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
    // A probe may outlive a concurrent replacement. The canonical name can now
    // point at a new instance, whose Dante must not be restarted by old work.
    let Some(container_id) = ch.container_id.as_deref().filter(|id| !id.is_empty()) else { return false };
    let result = tokio::time::timeout(std::time::Duration::from_secs(18), crate::docker::exec_capture(
        docker, container_id, vec!["timeout", "18", "bash", "-c", DNS_RECOVERY_SCRIPT, "vpnmgr-dns-recover", host, &port, url.scheme(), &tun],
    )).await;
    let recovered = matches!(result, Ok(Ok(ref s)) if s.lines().any(|l| l == "VPNMGR_DNS_RECOVERED"));
    if recovered {
        crate::ev!(info, "manager", "dns_cache_recovered", "通道域名代理已恢复,重新检测内网连通", { "cid": ch.id });
    } else if matches!(&result, Err(_) | Ok(Err(_))) || matches!(&result, Ok(Ok(s)) if s.contains("VPNMGR_DNS_START_FAILED") || s.contains("VPNMGR_DNS_NOT_READY") || s.contains("VPNMGR_DNS_OLD_LISTENER_BUSY")) {
        crate::ev!(warn, "manager", "dns_cache_recovery_failed", "通道域名代理恢复未完成,稍后可重试", { "cid": ch.id });
    }
    recovered
}

async fn probe_once(docker: Option<&bollard::Docker>, cfg: &Config, ch: &ChannelPublic) -> ProbeOutcome {
    if ch.probe_url.is_empty() {
        return ProbeOutcome::failed("没有验证地址");
    }
    let docker = match docker {
        Some(d) => d,
        None => return ProbeOutcome::failed("容器管理连接不可用"),
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
        Err(_) => ProbeOutcome::failed("探针执行失败"),
    }
}

/// 只有格式完整的 curl 结果才是连通结论；exec 错误文本不等于通道不通。
pub fn parse_probe_output(out: &str) -> ProbeOutcome {
    let mut it = out.split_whitespace();
    let code = it.next().and_then(|s| s.parse::<u32>().ok());
    let secs = it.next().and_then(|s| s.parse::<f64>().ok());
    match (code, secs, it.next()) {
        (Some(code), Some(secs), None) if (code == 0 || (100..=599).contains(&code)) && secs.is_finite() && secs >= 0.0 => {
            if code == 0 || code >= 500 { ProbeOutcome::NotConnected }
            else { ProbeOutcome::Connected { latency: Some((secs * 1000.0) as i64) } }
        }
        _ => ProbeOutcome::failed("探针未返回有效结果"),
    }
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

// ── 通道计划、初始化与启停 ─────────────────────────────────────────────

/// 普通创建与候选替换共用参数；这里只准备镜像和配置，不动既有容器。
pub async fn channel_plan(state: &crate::AppState, docker: &bollard::Docker, ch: &ChannelPublic, vnc_pwd: &str) -> Result<crate::adapters::ContainerPlan> {
    channel_plan_with_image(state, docker, ch, vnc_pwd, None).await
}

/// full-init 容器的宿主前置条件。正式 App（lite/with-vm）都走 managed VM；非托管
/// 只留给明确预装过同名策略的开发验证，避免悄悄用 `unconfined` 运行。
pub async fn ensure_full_init_host(cfg: &Config, spec: &crate::registry::AdapterSpec) -> Result<()> {
    if !spec.systemd { return Ok(()); }
    if cfg.managed_vm {
        return crate::vm::ensure_generic_apparmor(&cfg.vm_profile).await;
    }
    anyhow::ensure!(
        std::env::var("VPNMGR_GENERIC_APPARMOR_PRELOADED").as_deref() == Ok("1"),
        "非托管开发模式不会安装 AppArmor 策略；请先预装 tongdao-generic，并设置 VPNMGR_GENERIC_APPARMOR_PRELOADED=1"
    );
    Ok(())
}

pub async fn channel_plan_with_image(state: &crate::AppState, docker: &bollard::Docker, ch: &ChannelPublic, vnc_pwd: &str, image: Option<&str>) -> Result<crate::adapters::ContainerPlan> {
    let cfg = &state.cfg;
    let spec = crate::registry::get(&ch.vpn_type)?;
    ensure_full_init_host(cfg, &spec).await?;
    let mac = ch.mac.clone().unwrap_or_default();
    let mut plan = crate::adapters::build_run_kwargs(
        &ch.id, &mac, ch.ec_ver.as_deref(), &spec, vnc_pwd, &cfg.vpn_net,
    )?;
    crate::replacement_docker::use_volume(&mut plan, &crate::replacement_store::data_volume(&cfg.db_path(), &ch.id)?)?;
    if let Some(image) = image { plan.config.image = Some(image.into()); }

    // 对照 docker-py `containers.run()` 的 ImageNotFound 自动拉:镜像不在 VM 时走镜像源拉取。
    // bollard 的 create_container 不会自动拉,缺镜像会硬失败(EC 某 tag / aTrust 未预载即出错)。
    if let Some(image) = plan.config.image.as_deref() {
        ensure_image_mirrored(docker, cfg, image).await?;
    }

    let dns = if spec.runtime == "oss" {
        let actual = crate::docker::container_ip_on_net(docker, crate::infra::MIHOMO_CONTAINER, &cfg.vpn_net).await;
        if cfg.identity().colima_home_rel.is_some() {
            let expected = crate::runtime_network::ensure(docker, cfg).await?;
            anyhow::ensure!(actual.is_some() && actual == expected,
                "分流组件 DNS 尚未就绪，请先恢复运行环境");
        }
        actual.map(|ip| vec![ip])
    } else {
        None
    };

    if let Some(dns) = dns { plan.config.host_config.as_mut().expect("adapter host config").dns = Some(dns); }
    Ok(plan)
}

pub async fn initialize_gui(docker: &bollard::Docker, ch: &ChannelPublic) {
    let Ok(spec) = crate::registry::get(&ch.vpn_type) else { return; };
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

/// 原地 start —— 仅 byo(命门:hagb/oss 走 replacement)。
pub async fn start(docker: &bollard::Docker, cid: &str, expected: &str) -> Result<()> {
    let info = docker.inspect_container(expected, None).await?;
    anyhow::ensure!(info.id.as_deref() == Some(expected) && info.name.as_deref() == Some(&format!("/vpn-{cid}")), "原容器身份已变化，不能原地启动");
    let _ = crate::docker::start(docker, expected).await;
    anyhow::ensure!(docker.inspect_container(expected, None).await?.state.and_then(|s| s.running) == Some(true), "原容器启动未确认");
    Ok(())
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

    #[tokio::test]
    async fn dns_recovery_cannot_touch_replacement_with_reused_name() {
        use axum::{http::StatusCode, Router};
        use std::sync::{Arc, Mutex};
        let requested = Arc::new(Mutex::new(Vec::new()));
        let seen = requested.clone();
        let server = Router::new().fallback(move |request: axum::extract::Request| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(request.uri().path().to_owned());
                // The captured old container has stopped. Do not fall back to
                // its canonical name, which now belongs to the replacement.
                StatusCode::CONFLICT
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, server).await.unwrap() });
        let docker = bollard::Docker::connect_with_http(&address, 5, bollard::API_DEFAULT_VERSION).unwrap();
        let mut channel = ch("c1"); channel.vpn_type = "atrust".into();
        channel.probe_url = "https://fixture.test/".into(); channel.container_id = Some("old-container".into());
        assert!(!recover_hagb_dns(Some(&docker), &channel).await);
        let paths = requested.lock().unwrap().clone();
        assert_eq!(paths.len(), 1);
        assert!(paths[0].ends_with("/containers/old-container/exec"), "stale recovery targeted replacement: {paths:?}");
        channel.container_id = None;
        assert!(!recover_hagb_dns(Some(&docker), &channel).await);
        assert_eq!(requested.lock().unwrap().len(), 1);
        task.abort();
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
        assert_eq!(texts.len(), 3, "干净域名 + 占位段拒绝 + MATCH,DIRECT 兜底");
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
        assert_eq!(text, vec!["DOMAIN-SUFFIX,äbc.中国,ch-a", "IP-CIDR,198.18.0.0/15,REJECT,no-resolve", "MATCH,DIRECT"]);
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
    async fn config_delivery_failure_is_not_success() {
        // 配置投递失败不能被当成成功；独立假 socket，不读取日常配置或控制器。
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("not-a-docker.sock");
        std::fs::write(&sock, b"").unwrap();
        let bad = bollard::Docker::connect_with_unix(sock.to_str().unwrap(), 1, bollard::API_DEFAULT_VERSION).unwrap();
        assert!(crate::config_apply::persist_container(&bad, "mihomo", b"fixture").await.is_err());
    }

    #[test]
    fn mihomo_config_ip_rules_most_specific_first() {
        // 命门 #2:首个命中语义下,更具体的前缀必须排在宽网段之前,否则跨通道 /24 盖住 /32。
        // 插入顺序故意宽段在前。
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

    /// 按 mihomo 首个命中语义,返回某域名命中的 DOMAIN-SUFFIX 行的目标。
    fn first_domain_hit(texts: &[String], host: &str) -> Option<String> {
        texts.iter().find_map(|t| {
            let mut parts = t.split(',');
            if parts.next()? != "DOMAIN-SUFFIX" {
                return None;
            }
            let suffix = parts.next()?;
            let target = parts.next()?;
            (host == suffix || host.ends_with(&format!(".{suffix}"))).then(|| target.to_string())
        })
    }

    #[test]
    fn mihomo_config_domain_rules_most_specific_first() {
        // R-01:先加 example.com(甲),再加 corp.example.com(乙);子域必须排在前面才会走乙。
        let rules = vec![
            rule("a", "domain", "example.com", 1),
            rule("b", "domain", "corp.example.com", 1),
        ];
        let cfg = build_mihomo_config(
            serde_yaml::Value::Mapping(serde_yaml::Mapping::new()), &[ch("a"), ch("b")], &rules);
        let texts: Vec<String> = cfg["rules"].as_sequence().unwrap().iter()
            .map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(texts, vec![
            "DOMAIN-SUFFIX,corp.example.com,ch-b",
            "DOMAIN-SUFFIX,example.com,ch-a",
            "IP-CIDR,198.18.0.0/15,REJECT,no-resolve",
            "MATCH,DIRECT",
        ]);
        assert_eq!(first_domain_hit(&texts, "a.corp.example.com").as_deref(), Some("ch-b"));
        assert_eq!(first_domain_hit(&texts, "corp.example.com").as_deref(), Some("ch-b"));
        assert_eq!(first_domain_hit(&texts, "www.example.com").as_deref(), Some("ch-a"));
    }

    #[test]
    fn ordered_rules_same_level_keeps_insertion_order() {
        // R-03:同级保持插入顺序;通配前缀和大写、结尾点按规范化后的标签数计。
        let mut rules = vec![
            rule("a", "domain", "b.example.test", 1),
            rule("b", "ip", "10.1.0.0/16", 1),
            rule("b", "domain", "example.test", 1),
            rule("c", "domain", "*.corp.example", 1),
            rule("a", "domain", "a.example.test", 1),
            rule("c", "domain", "Deep.B.Example.Test.", 1),
            rule("b", "domain", "off.example.test", 0),
            rule("c", "ip", "10.1.2.0/24", 1),
        ];
        for (i, r) in rules.iter_mut().enumerate() {
            r.id = (i + 1) as i64;
        }
        let none = HashSet::new();
        let got: Vec<(i64, String, String, String)> = ordered_rules(&[], &rules, &none).into_iter()
            .map(|r| (r.rule_id, r.kind, r.pattern, r.target))
            .collect();
        let want = |id: i64, kind: &str, pattern: &str, target: &str| {
            (id, kind.to_string(), pattern.to_string(), target.to_string())
        };
        assert_eq!(got, vec![
            want(6, "domain", "deep.b.example.test", "ch-c"),
            want(1, "domain", "b.example.test", "ch-a"),
            want(5, "domain", "a.example.test", "ch-a"),
            want(3, "domain", "example.test", "ch-b"),
            want(4, "domain", "corp.example", "ch-c"),
            want(8, "ip", "10.1.2.0/24", "ch-c"),
            want(2, "ip", "10.1.0.0/16", "ch-b"),
        ]);
        let first = ordered_rules(&[], &rules, &none).remove(0);
        assert_eq!(first.channel_id, "c");
    }

    /// 按 mihomo 首个命中语义,返回某 IPv4 地址命中的 IP-CIDR 行的目标。
    fn first_ip_hit(texts: &[String], addr: &str) -> Option<String> {
        let addr: std::net::IpAddr = addr.parse().unwrap();
        texts.iter().find_map(|t| {
            let mut parts = t.split(',');
            if parts.next()? != "IP-CIDR" {
                return None;
            }
            let net: ipnet::IpNet = parts.next()?.parse().ok()?;
            let target = parts.next()?;
            net.contains(&addr).then(|| target.to_string())
        })
    }

    #[test]
    fn errored_or_lost_channel_rules_rejected_in_place() {
        // R-05:/16 绑出错的通道、/8 绑正常通道;/16 仍排在前面,命中即阻止,不落到 /8 或直连。
        let mut broken = ch("broken");
        broken.status = "error".into();
        let rules = vec![
            rule("ok", "ip", "10.0.0.0/8", 1),
            rule("broken", "ip", "10.30.0.0/16", 1),
            rule("broken", "domain", "corp.example.test", 1),
            rule("ok", "domain", "example.test", 1),
        ];
        let lines = |channels: &[ChannelPublic], lost: &HashSet<String>| -> Vec<String> {
            build_mihomo_config_with(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()), channels, &rules, lost)["rules"]
                .as_sequence().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
        };
        let errored = lines(&[ch("ok"), broken], &HashSet::new());
        assert_eq!(errored, vec![
            "DOMAIN-SUFFIX,corp.example.test,REJECT",
            "DOMAIN-SUFFIX,example.test,ch-ok",
            "IP-CIDR,10.30.0.0/16,REJECT,no-resolve",
            "IP-CIDR,10.0.0.0/8,ch-ok,no-resolve",
            "IP-CIDR,198.18.0.0/15,REJECT,no-resolve",
            "MATCH,DIRECT",
        ]);
        assert_eq!(first_ip_hit(&errored, "10.30.2.4").as_deref(), Some("REJECT"));
        assert_eq!(first_ip_hit(&errored, "10.40.2.4").as_deref(), Some("ch-ok"));
        assert_eq!(first_domain_hit(&errored, "a.corp.example.test").as_deref(), Some("REJECT"));
        assert_eq!(first_domain_hit(&errored, "www.example.test").as_deref(), Some("ch-ok"));
        // 运行态但在掉线集合里,写法相同;集合为空时照常走通道。
        let lost: HashSet<String> = ["broken".to_string()].into();
        assert_eq!(lines(&[ch("ok"), ch("broken")], &lost), errored);
        let normal = lines(&[ch("ok"), ch("broken")], &HashSet::new());
        assert_eq!(first_ip_hit(&normal, "10.30.2.4").as_deref(), Some("ch-broken"));
        assert_eq!(build_mihomo_config(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            &[ch("ok"), ch("broken")], &rules)["rules"].as_sequence().unwrap().len(), normal.len());
    }

    #[test]
    fn custom_rules_stay_rejected_until_explicit_target_is_verified() {
        let rules = vec![rule("byo", "domain", "intra.example.test", 1)];
        let lines = |ch: ChannelPublic| -> Vec<String> {
            build_mihomo_config(Yaml::Null, &[ch], &rules)["rules"].as_sequence().unwrap()
                .iter().map(|v| v.as_str().unwrap().to_string()).collect()
        };
        let mut custom = ch("byo");
        custom.vpn_type = "custom".into();
        custom.status = "running".into();
        custom.probe_url = "https://intra.example.test/health".into();
        assert_eq!(lines(custom.clone())[0], "DOMAIN-SUFFIX,intra.example.test,REJECT");

        custom.status = "logged_in".into();
        assert_eq!(lines(custom.clone())[0], "DOMAIN-SUFFIX,intra.example.test,ch-byo");

        custom.probe_url.clear();
        assert_eq!(lines(custom)[0], "DOMAIN-SUFFIX,intra.example.test,REJECT");

        let mut legacy = ch("byo");
        legacy.status = "running".into();
        assert_eq!(lines(legacy)[0], "DOMAIN-SUFFIX,intra.example.test,ch-byo",
            "other adapter status semantics must stay unchanged");
    }

    fn export_doc(channels: serde_json::Value) -> serde_json::Value {
        json!({"kind": "vpnmgr-export", "version": 1, "channels": channels})
    }

    #[test]
    fn rule_diff_lists_domain_moves_and_rejects_only() {
        let rules = |items: &[(&str, &str)]| -> serde_json::Value {
            items.iter().map(|(kind, pattern)| json!({"kind": kind, "pattern": pattern, "enabled": 1, "note": "", "locked": 0}))
                .collect()
        };
        // 两者都没有:同级域名、IP 桶本来就排过,输出为空。
        let plain = export_doc(json!([
            {"name": "甲", "routing_enabled": true, "rules": rules(&[("domain", "a.example.test"), ("ip", "10.1.0.0/16")])},
            {"name": "乙", "routing_enabled": true, "rules": rules(&[("domain", "b.example.test"), ("ip", "10.1.2.0/24")])},
        ]));
        assert!(rule_diff(&plain, &[]).is_empty());
        assert!(rule_diff(&json!({}), &[]).is_empty());

        // 域名顺序变化:先加的宽后缀被挪到子域之后。
        let moved = export_doc(json!([
            {"name": "甲", "routing_enabled": true, "rules": rules(&[("domain", "example.com")])},
            {"name": "乙", "routing_enabled": true, "rules": rules(&[("domain", "corp.example.com"), ("ip", "10.2.0.0/16")])},
        ]));
        assert_eq!(rule_diff(&moved, &[]), vec![
            "位置变化：DOMAIN-SUFFIX,corp.example.com（乙）第 2 条 → 第 1 条",
            "位置变化：DOMAIN-SUFFIX,example.com（甲）第 1 条 → 第 2 条",
        ]);

        // 出错通道变成 REJECT:其余规则相对顺序不变,不报位置变化;停用规则和分流关着的通道不出现。
        let errored = export_doc(json!([
            {"name": "甲", "routing_enabled": true, "rules": rules(&[("domain", "a.example.test"), ("ip", "10.1.0.0/16")])},
            {"name": "丙", "routing_enabled": true, "rules": [
                {"kind": "ip", "pattern": "10.1.3.0/24", "enabled": 1},
                {"kind": "domain", "pattern": "off.example.test", "enabled": 0}]},
            {"name": "丁", "routing_enabled": false, "rules": rules(&[("domain", "d.example.test")])},
        ]));
        assert_eq!(rule_diff(&errored, &["丙".to_string(), "丁".to_string()]), vec![
            "改为阻止：IP-CIDR,10.1.3.0/24（丙）第 2 条",
        ]);
        assert!(rule_diff(&errored, &[]).is_empty());
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
        assert_eq!(rules_seq.len(), 2);
    }

    #[test]
    fn egress_policy_changes_trigger_once_and_ignore_input_order() {
        let mut channel = ch("a");
        channel.server = "203.0.113.7".into();
        channel.status = "stopped".into();
        let mut rules = vec![rule("a", "ip", "192.0.2.5/24", 1), rule("a", "ip", "10.0.0.0/8", 0)];
        let desired = crate::vm::egress_policy(&rules, &[channel.clone()]);
        assert_eq!(desired.bound, vec!["192.0.2.0/24", "198.18.0.0/15"]);
        assert_eq!(desired.gateways, vec!["203.0.113.7/32"]);
        let mut previous = None;
        assert!(guard_changed(&mut previous, &desired));
        rules.reverse();
        assert!(!guard_changed(&mut previous, &crate::vm::egress_policy(&rules, &[channel.clone()])));
        rules[0].enabled = 1;
        assert!(guard_changed(&mut previous, &crate::vm::egress_policy(&rules, &[channel.clone()])));
        channel.server = "vpn.example.test".into();
        let no_gateway = crate::vm::egress_policy(&rules, &[channel]);
        assert!(no_gateway.gateways.is_empty());
        assert!(guard_changed(&mut previous, &no_gateway));
        assert!(guard_changed(&mut previous, &crate::vm::egress_policy(&[], &[])));
    }

    #[test]
    fn unbound_fake_addresses_are_rejected_before_direct() {
        let cfg = build_mihomo_config(Yaml::Null, &[], &[]);
        let lines: Vec<String> = cfg["rules"].as_sequence().unwrap().iter().map(|v| v.as_str().unwrap().into()).collect();
        assert_eq!(lines, ["IP-CIDR,198.18.0.0/15,REJECT,no-resolve", "MATCH,DIRECT"]);
        assert_eq!(first_ip_hit(&lines, "198.19.128.1").as_deref(), Some("REJECT"));
    }

    // ── Task 3: probe + log dedup ──────────────────────────────────────────
    #[tokio::test]
    async fn probe_empty_url_is_unavailable() {
        let c = ch("a"); // probe_url 空 → 不碰 docker，不能做连通判定
        let cfg = Config::from_getter(|_| None);
        assert!(matches!(probe(None, &cfg, &c).await, ProbeOutcome::ProbeFailed { .. }));
    }

    #[test]
    fn probe_output_parsing() {
        // 命门 #1 语义:<500 算通 + 时延;000(连不上)→ false/None;4xx 也算通(内网门户常 401/403)
        assert_eq!(parse_probe_output("200 0.234"), ProbeOutcome::Connected { latency: Some(234) });
        assert_eq!(parse_probe_output("302 1.5"), ProbeOutcome::Connected { latency: Some(1500) });
        assert_eq!(parse_probe_output("403 0.1"), ProbeOutcome::Connected { latency: Some(100) });
        assert_eq!(parse_probe_output("500 0.2"), ProbeOutcome::NotConnected);
        assert_eq!(parse_probe_output("000 0.000"), ProbeOutcome::NotConnected);
        assert!(matches!(parse_probe_output(""), ProbeOutcome::ProbeFailed { .. }));
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
        cfg.insert("server".into(), serde_json::json!("vpn.example.com:10443"));
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
        assert!(run.contains("openfortivpn 'vpn.example.com:10443'"), "host:port 整串走 CLI 位置参数(openfortivpn 自拆端口):\n{run}");
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
