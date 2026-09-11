//! Docker 环境/容器体检 + 镜像类修复。对照 app/preflight.py。
//! 检查函数永不抛:内部错误转 warn/fail 的 CheckResult。
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use bollard::Docker;
use crate::{registry, dockerhub};

/// 自建镜像本地构建上下文(镜像名前缀 → 仓库内目录)。对照 _BUILD_CONTEXT。
pub const BUILD_CONTEXT: &[(&str, &str)] = &[
    ("vpnmgr/oss-vpn", "images/oss"),
    ("vpnmgr/byo-desktop", "images/byo"),
    ("vpnmgr/hillstone-desktop", "images/hillstone"),
];

/// 内置国内 Docker Hub 加速源(优先级序,对照 preflight.py DEFAULT_MIRRORS)。
/// ⚠️ 收录标准:实测能取到真实 manifest(如 metacubex/mihomo),不是 /v2/ 探针 <500 就算——
/// 红队实测 xuanyuan(manifest 403)/dockerproxy.net(404)/aityp(非 registry)全是僵尸源,已剔除。
pub const DEFAULT_MIRRORS: &[&str] = &[
    "docker.1ms.run",
    "docker.m.daocloud.io",
    "hub.rat.dev",
];

pub(crate) fn build_context_of(repo: &str) -> Option<&'static str> {
    BUILD_CONTEXT.iter().find(|(k, _)| *k == repo).map(|(_, v)| *v)
}

/// 对照 is_buildable:vpnmgr/* 自建。
pub fn is_buildable(image: &str) -> bool {
    let repo = image.split(':').next().unwrap_or("");
    build_context_of(repo).is_some()
}

pub struct SplitImage {
    pub repo: String,
    pub tag: Option<String>,
    pub versioned: bool,
    pub image_field: String,
    pub display: String,
}

/// 对照 _split_image。
pub fn split_image(full: &str) -> SplitImage {
    if full.contains("{version}") {
        let repo = full.split(':').next().unwrap_or("").to_string();
        return SplitImage { repo: repo.clone(), tag: None, versioned: true, image_field: repo, display: full.to_string() };
    }
    let (repo, tag) = match full.split_once(':') {
        Some((r, t)) => (r.to_string(), if t.is_empty() { "latest".to_string() } else { t.to_string() }),
        None => (full.to_string(), "latest".to_string()),
    };
    SplitImage { repo, tag: Some(tag), versioned: false, image_field: full.to_string(), display: full.to_string() }
}

/// 对照 resolve_image:替换 {version}(默认 7.6.3)。未知类型 → Err。
pub fn resolve_image(vpn_type: &str, version: Option<&str>) -> anyhow::Result<String> {
    let spec = registry::get(vpn_type)?;
    let mut image = spec.image.clone();
    if image.contains("{version}") {
        // 对照 Python `version or "7.6.3"`:空串也算 falsy → 回退默认
        image = image.replace("{version}", version.filter(|v| !v.is_empty()).unwrap_or("7.6.3"));
    }
    Ok(image)
}

/// 对照 known_repos:所有适配器 + infra(pull)声明的 repo。
pub fn known_repos() -> HashSet<String> {
    let mut repos = HashSet::new();
    if let Ok(list) = registry::list_adapters() {
        for a in list {
            if let Ok(spec) = registry::get(&a.key) {
                let repo = spec.image.split(':').next().unwrap_or("").replace("{version}", "");
                let repo = repo.trim_end_matches(':').to_string();
                if !repo.is_empty() {
                    repos.insert(repo);
                }
            }
        }
    }
    for inf in INFRA_IMAGES {
        if inf.kind == "pull" {
            repos.insert(inf.image.split(':').next().unwrap_or("").to_string());
        }
    }
    repos
}

/// CheckResult(对照 _result)。
#[derive(Serialize, Clone)]
pub struct CheckResult {
    pub id: String,
    pub layer: String,
    pub title: String,
    pub status: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<Value>,
}

pub fn result(id: &str, layer: &str, title: &str, status: &str, detail: &str, fix: Option<Value>) -> CheckResult {
    CheckResult { id: id.into(), layer: layer.into(), title: title.into(), status: status.into(), detail: detail.into(), fix }
}

fn severity(s: &str) -> u8 {
    match s {
        "warn" => 1,
        "fail" => 2,
        _ => 0, // pass/skip
    }
}

/// 对照 _aggregate 的 overall。
pub fn aggregate_overall(checks: &[CheckResult]) -> String {
    let mut overall = "pass";
    for c in checks {
        if severity(&c.status) > severity(overall) {
            overall = match c.status.as_str() {
                "warn" => "warn",
                "fail" => "fail",
                _ => overall,
            };
        }
    }
    overall.to_string()
}

/// infra 镜像(对照 INFRA_IMAGES)。
pub struct InfraImage {
    pub image: &'static str,
    pub kind: &'static str,
    pub title: &'static str,
    pub build_context: Option<&'static str>,
    pub arch: &'static [&'static str],
}

pub const INFRA_IMAGES: &[InfraImage] = &[
    InfraImage { image: crate::image_sources::MIHOMO_IMAGE, kind: "pull", title: "mihomo 分流底座", build_context: None, arch: &["amd64", "arm64"] },
    InfraImage { image: "app", kind: "compose", title: "管理后端(FastAPI)", build_context: Some("app"), arch: &[] },
];

// ── 检查函数 + run_checks(对照 preflight.py;永不抛) ──────────────────────────

pub async fn check_docker_daemon(docker: &Docker) -> CheckResult {
    match crate::docker::ping(docker).await {
        Ok(_) => result("docker_daemon", "引擎", "Docker 守护进程可达", "pass", "", None),
        Err(e) => result("docker_daemon", "引擎", "Docker 守护进程可达", "fail",
            &format!("无法连接 Docker:{e}"),
            Some(json!({"kind":"tutorial","action":"install_docker","label":"查看安装/启动 Docker 教程"}))),
    }
}

pub async fn check_image_present(docker: &Docker, image: &str) -> CheckResult {
    match crate::docker::image_present(docker, image).await {
        Some(true) => result("image_present", "镜像", "目标镜像本地就绪", "pass", image, None),
        Some(false) => {
            if is_buildable(image) {
                let ctx = build_context_of(image.split(':').next().unwrap_or("")).unwrap_or("");
                result("image_present", "镜像", "目标镜像本地就绪", "fail",
                    &format!("自建镜像未构建。请在仓库根执行:docker build -t {image} {ctx}"),
                    Some(json!({"kind":"none"})))
            } else {
                result("image_present", "镜像", "目标镜像本地就绪", "fail",
                    &format!("本地缺少镜像 {image},起容器会失败(自动拉取可能因 Docker Hub 网络不通而失败)"),
                    Some(json!({"kind":"auto","action":"pull_image","label":"走国内镜像源拉取","params":{"image":image}})))
            }
        }
        None => result("image_present", "镜像", "目标镜像本地就绪", "warn", "检查出错", None),
    }
}

pub async fn check_image_arch_match(docker: &Docker, image: &str, host_arch: &str) -> CheckResult {
    match crate::docker::image_present(docker, image).await {
        Some(false) => return result("image_arch_match", "镜像", "镜像架构匹配宿主", "skip", "镜像就绪后再检测架构", None),
        None => return result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn", "检查出错", None),
        Some(true) => {}
    }
    let arch = crate::docker::image_arch(docker, image).await.unwrap_or_default();
    if arch.is_empty() {
        return result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn",
            "无法判定本地镜像架构(多架构存储下可能为空),起容器后留意是否走模拟", None);
    }
    if arch == host_arch {
        return result("image_arch_match", "镜像", "镜像架构匹配宿主", "pass", &format!("{arch} 原生"), None);
    }
    if is_buildable(image) {
        return result("image_arch_match", "镜像", "镜像架构匹配宿主", "warn",
            &format!("自建镜像架构 {arch} ≠ 宿主 {host_arch},建议本地重建"), None);
    }
    result("image_arch_match", "镜像", "镜像架构匹配宿主", "fail",
        &format!("本地镜像是 {arch},宿主是 {host_arch} → 会走模拟(如 aTrust 核心会崩)"),
        Some(json!({"kind":"auto","action":"pull_image","label":format!("拉取 {host_arch} 版并重打标签"),"params":{"image":image,"arch":host_arch}})))
}

pub async fn check_vpn_network(docker: &Docker, vpn_net: &str) -> CheckResult {
    if crate::docker::network_exists(docker, vpn_net).await {
        result("vpn_network", "运行条件", "VPN docker 网络存在", "pass", vpn_net, None)
    } else {
        result("vpn_network", "运行条件", "VPN docker 网络存在", "fail",
            &format!("docker 网络 {vpn_net} 不存在,容器无法接入"),
            Some(json!({"kind":"auto","action":"create_network","label":"创建该网络","params":{"name":vpn_net}})))
    }
}

#[derive(Default)]
struct TunChecks {
    // 极小的一次性检测串行化，缓存只记录已完成且已尝试清理的结果。
    samples: tokio::sync::Mutex<HashMap<(String, String), (Instant, CheckResult)>>,
}

impl TunChecks {
    async fn sample(&self, key: (String, String), fresh: bool, probe: impl std::future::Future<Output = CheckResult>) -> CheckResult {
        let requested = Instant::now();
        let mut samples = self.samples.lock().await;
        samples.retain(|_, (at, check)| at.elapsed() < Duration::from_secs(if check.status == "pass" { 60 } else { 5 }));
        if let Some((at, check)) = samples.get(&key) {
            // 手动完整检查重测；并发请求可共享请求之后完成的这一轮。
            if !fresh || *at >= requested {
                let mut cached = check.clone();
                cached.detail = format!("{}{}复用 {} 秒前的检测结果", cached.detail,
                    if cached.detail.is_empty() { "" } else { "；" }, at.elapsed().as_secs());
                return cached;
            }
        }
        let check = probe.await;
        if samples.len() >= 32 {
            if let Some(oldest) = samples.iter().min_by_key(|(_, (at, _))| *at).map(|(key, _)| key.clone()) {
                samples.remove(&oldest);
            }
        }
        samples.insert(key, (Instant::now(), check.clone()));
        check
    }
}

pub async fn check_dev_net_tun(docker: &Docker, image: &str, image_ok: bool, fresh: bool) -> CheckResult {
    if !image_ok {
        return result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip", "镜像就绪后检测", None);
    }
    // 以实际引擎和不可变镜像 ID 隔离结果；标签切换或 profile 切换不会借用旧结果。
    let identity: anyhow::Result<_> = tokio::time::timeout(Duration::from_secs(5), async {
        let (engine, image) = tokio::try_join!(docker.info(), docker.inspect_image(image))?;
        let engine = engine.id.filter(|id| !id.is_empty()).ok_or_else(|| anyhow::anyhow!("引擎缺少 ID"))?;
        let image = image.id.filter(|id| !id.is_empty()).ok_or_else(|| anyhow::anyhow!("镜像缺少 ID"))?;
        Ok((engine, image))
    }).await.unwrap_or_else(|_| Err(anyhow::anyhow!("读取检测环境超时")));
    let key = match identity {
        Ok(key) => key,
        Err(error) => return result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn", &format!("无法判定: {error}"), None),
    };
    static CHECKS: OnceLock<TunChecks> = OnceLock::new();
    CHECKS.get_or_init(TunChecks::default).sample(key.clone(), fresh, async {
        match crate::docker::run_tun_probe(docker, &key.1).await {
            Ok(true) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "pass", "", None),
            Ok(false) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn", "探针未通过，VPN 隧道可能起不来", None),
            Err(e) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn", &format!("无法判定: {e}"), None),
        }
    }).await
}

pub async fn check_disk_space(docker: &Docker) -> CheckResult {
    match crate::docker::layers_size_gb(docker).await {
        Ok(gb) => result("disk_space", "运行条件", "磁盘空间", "pass",
            &format!("Docker 镜像层已占用约 {gb:.1} GB;每个 VPN 镜像 1.5–5GB,注意留足空间"), None),
        Err(e) => result("disk_space", "运行条件", "磁盘空间", "skip", &format!("无法读取:{e}"), None),
    }
}

pub async fn check_docker_version(docker: &Docker) -> CheckResult {
    match crate::docker::docker_version(docker).await {
        Ok(v) => result("docker_version", "引擎", "Docker 版本", "pass", &format!("Docker {v}"), None),
        Err(e) => result("docker_version", "引擎", "Docker 版本", "warn", &format!("读取失败:{e}"), None),
    }
}

pub async fn check_mirror_reachable(mirrors: &[String]) -> CheckResult {
    for h in mirrors {
        if mirror_reachable(h).await {
            return result("mirror_reachable", "镜像", "国内镜像源可达", "pass", &format!("{h} 可达"), None);
        }
    }
    result("mirror_reachable", "镜像", "国内镜像源可达", "warn",
        "配置的镜像源都不可达,自动拉取可能失败",
        Some(json!({"kind":"tutorial","action":"switch_registry_mirror","label":"查看切换 Docker 国内源教程"})))
}

pub fn check_mihomo(alive: bool) -> CheckResult {
    if alive {
        result("mihomo_health", "分流底座", "mihomo 分流实例", "pass", "running", None)
    } else {
        result("mihomo_health", "分流底座", "mihomo 分流实例", "warn", "mihomo 未运行,通道起来了也不会分流", None)
    }
}

/// 对照 _mirror_reachable:GET https://{host}/v2/ status<500。async(避免 tokio 内 blocking)。
pub async fn mirror_reachable(host: &str) -> bool {
    reqwest::Client::new()
        .get(format!("https://{host}/v2/"))
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .map(|r| r.status().as_u16() < 500)
        .unwrap_or(false)
}

/// 对照 run_checks。docker None → daemon fail + 其余 skip。
#[allow(clippy::too_many_arguments)]
pub async fn run_checks(
    docker: Option<&Docker>,
    vpn_type: Option<&str>,
    version: Option<&str>,
    host_arch: &str,
    vpn_net: &str,
    scope: &str,
    mirrors: &[String],
    mihomo_alive: Option<bool>,
) -> Value {
    let image = vpn_type.and_then(|t| resolve_image(t, version).ok());
    let mut checks: Vec<CheckResult> = Vec::new();

    let daemon = match docker {
        Some(d) => check_docker_daemon(d).await,
        None => result("docker_daemon", "引擎", "Docker 守护进程可达", "fail", "无法连接 Docker:daemon 不可用", None),
    };
    let daemon_fail = daemon.status == "fail";
    checks.push(daemon);
    if daemon_fail {
        for (cid, title) in [
            ("image_present", "目标镜像本地就绪"),
            ("image_arch_match", "镜像架构匹配宿主"),
            ("vpn_network", "VPN docker 网络存在"),
            ("dev_net_tun", "/dev/net/tun 可用"),
            ("disk_space", "磁盘空间"),
        ] {
            checks.push(result(cid, "—", title, "skip", "Docker 不可达,跳过", None));
        }
        return aggregate(checks, host_arch, image);
    }
    let d = docker.unwrap();

    let image_ok = if let Some(img) = &image {
        let present = check_image_present(d, img).await;
        let ok = present.status == "pass";
        checks.push(present);
        checks.push(check_image_arch_match(d, img, host_arch).await);
        ok
    } else {
        checks.push(result("image_present", "镜像", "目标镜像本地就绪", "skip", "未指定通道类型", None));
        checks.push(result("image_arch_match", "镜像", "镜像架构匹配宿主", "skip", "未指定通道类型", None));
        false
    };

    checks.push(check_vpn_network(d, vpn_net).await);
    checks.push(match &image {
        Some(img) => check_dev_net_tun(d, img, image_ok, scope == "full").await,
        None => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip", "未指定通道类型", None),
    });
    checks.push(check_disk_space(d).await);
    if scope == "full" {
        checks.push(check_docker_version(d).await);
        checks.push(result("host_arch", "引擎", "宿主架构", "pass", host_arch, None));
        checks.push(check_mirror_reachable(mirrors).await);
        checks.push(check_mihomo(mihomo_alive.unwrap_or(false)));
    }
    aggregate(checks, host_arch, image)
}

fn aggregate(checks: Vec<CheckResult>, host_arch: &str, image: Option<String>) -> Value {
    let overall = aggregate_overall(&checks);
    json!({ "host_arch": host_arch, "target_image": image, "overall": overall, "checks": checks })
}

// ── image_inventory + 后台拉镜像 worker(对照 image_inventory / start_pull) ────

/// 对照 image_inventory。docker None → present 不查(保持 None)。
pub async fn image_inventory(docker: Option<&Docker>, host_arch: &str, mirrors: &[String]) -> Value {
    let mut order: Vec<String> = Vec::new();
    let mut entries: HashMap<String, Value> = HashMap::new();

    if let Ok(list) = registry::list_adapters() {
        for a in &list {
            let Ok(spec) = registry::get(&a.key) else { continue };
            let s = split_image(&spec.image);
            let key = if s.versioned { s.repo.clone() } else { s.image_field.clone() };
            let entry = entries.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                json!({
                    "image": s.image_field, "display": s.display, "repo": s.repo,
                    "tag": s.tag, "kind": if is_buildable(&s.image_field) { "build" } else { "pull" },
                    "role": "vpn", "title": a.label, "used_by": [],
                    "arch": [], "versioned": s.versioned,
                    "build_context": build_context_of(&s.repo),
                    "versions": [], "present": Value::Null,
                    "_fallback": spec.fallback_versions,
                })
            });
            entry["used_by"].as_array_mut().unwrap().push(json!(a.label));
            let arr = entry["arch"].as_array_mut().unwrap();
            for ar in &spec.arch {
                if !arr.iter().any(|x| x == ar) {
                    arr.push(json!(ar));
                }
            }
        }
    }
    for inf in INFRA_IMAGES {
        let s = split_image(inf.image);
        let key = s.image_field.clone();
        order.push(key.clone());
        entries.insert(key, json!({
            "image": s.image_field, "display": s.display, "repo": s.repo, "tag": s.tag,
            "kind": inf.kind, "role": "infra", "title": inf.title, "used_by": [],
            "arch": inf.arch, "versioned": false,
            "build_context": inf.build_context,
            "versions": [], "present": Value::Null, "_fallback": [],
        }));
    }

    let mut images = Vec::new();
    for key in &order {
        let mut e = entries.remove(key).unwrap();
        let fb: Vec<String> = e["_fallback"].as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        e.as_object_mut().unwrap().remove("_fallback");
        let versioned = e["versioned"].as_bool().unwrap_or(false);
        let kind = e["kind"].as_str().unwrap_or("").to_string();
        if versioned {
            let repo = e["repo"].as_str().unwrap_or("").to_string();
            e["versions"] = json!(dockerhub::versions(&repo, host_arch, &fb).await);
        } else if kind != "compose" {
            if kind == "pull" {
                let tag = e["tag"].clone();
                let arch = e["arch"].clone();
                e["versions"] = json!([{ "tag": tag, "arch": arch, "usable_here": true }]);
            }
            if let Some(d) = docker {
                let img = e["image"].as_str().unwrap_or("").to_string();
                e["present"] = json!(crate::docker::image_present(d, &img).await);
            }
        }
        images.push(e);
    }
    json!({ "host_arch": host_arch, "mirrors": mirrors, "images": images })
}

// ── 后台拉镜像任务表(对照 _TASKS) ──
struct PullTask {
    key: (String, String),
    state: Value,
    finished: Option<Instant>,
}

#[derive(Default)]
struct PullTasks { entries: HashMap<String, PullTask> }

impl PullTasks {
    fn prune(&mut self, now: Instant) {
        let mut finished: Vec<_> = self.entries.iter().filter_map(|(id, e)| e.finished.map(|when| (when, id.clone()))).collect();
        finished.sort();
        let excess = finished.len().saturating_sub(32);
        for (index, (when, id)) in finished.into_iter().enumerate() {
            if now.duration_since(when) >= Duration::from_secs(3600) || index < excess {
                self.entries.remove(&id);
            }
        }
    }

    fn reserve(&mut self, image: &str, arch: &str, now: Instant) -> Result<(String, bool), &'static str> {
        self.prune(now);
        let (repo, tag) = image.split_once(':').unwrap_or((image, "latest"));
        let key = (format!("{repo}:{}", if tag.is_empty() { "latest" } else { tag }), arch.to_string());
        let active: Vec<_> = self.entries.iter().filter(|(_, e)| e.finished.is_none()).collect();
        if let Some((tid, _)) = active.iter().find(|(_, e)| e.key == key) {
            return Ok(((*tid).clone(), false));
        }
        if active.len() >= 2 { return Err("已有 2 个镜像正在下载，请等待其中一个完成后重试"); }
        let tid = format!("{:032x}", rand::random::<u128>());
        self.entries.insert(tid.clone(), PullTask { key, finished: None,
            state: json!({"status":"running", "progress":"准备拉取…", "log_tail":[], "error":null}) });
        Ok((tid, true))
    }

    fn finish(&mut self, tid: &str, now: Instant) {
        if let Some(entry) = self.entries.get_mut(tid) {
            if entry.state["status"] == "running" {
                entry.state["status"] = json!("error");
                entry.state["error"] = json!("下载任务意外结束，请核对镜像清单后重试");
            }
            entry.finished = Some(now);
        }
        self.prune(now);
    }
}

fn tasks() -> &'static Mutex<PullTasks> {
    static T: OnceLock<Mutex<PullTasks>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(PullTasks::default()))
}

pub fn get_task(tid: &str) -> Option<Value> {
    let mut tasks = tasks().lock().unwrap();
    tasks.prune(Instant::now());
    tasks.entries.get(tid).map(|e| e.state.clone())
}

fn set_task(tid: &str, v: Value) {
    if let Some(entry) = tasks().lock().unwrap().entries.get_mut(tid) { entry.state = v; }
}

// Covers early return, panic and cancellation. Active work is never expired by a reader.
struct PullCompletion(String);
impl Drop for PullCompletion {
    fn drop(&mut self) { tasks().lock().unwrap().finish(&self.0, Instant::now()); }
}

/// Share in-flight work, cap workers at two, retain at most 32 finished records for one hour.
pub fn start_pull(docker: Docker, image: &str, host_arch: &str, mirrors: Vec<String>, activity: crate::runtime_lifecycle::Activity) -> Result<String, &'static str> {
    let (tid, created) = tasks().lock().unwrap().reserve(image, host_arch, Instant::now())?;
    if !created { return Ok(tid); }
    let (image, host_arch, tid2) = (image.to_string(), host_arch.to_string(), tid.clone());
    let mirrors = if mirrors.is_empty() {
        DEFAULT_MIRRORS.iter().map(|s| s.to_string()).collect()
    } else {
        mirrors
    };
    let completion = PullCompletion(tid2.clone());
    tokio::spawn(async move {
        let _activity = activity;
        let _completion = completion;
        let (repo, tag) = match image.split_once(':') {
            Some((r, t)) => (r.to_string(), if t.is_empty() { "latest".into() } else { t.to_string() }),
            None => (image.clone(), "latest".to_string()),
        };
        let mut log: Vec<String> = Vec::new();
        for m in &mirrors {
            set_task(&tid2, json!({ "status": "running", "progress": format!("探测镜像源 {m}…"), "log_tail": log, "error": Value::Null }));
            if !mirror_reachable(m).await {
                log.push(format!("{m} 不可达,跳过"));
                log = log.split_off(log.len().saturating_sub(20));
                continue;
            }
            set_task(&tid2, json!({ "status": "running", "progress": format!("从 {m} 拉取 {repo}:{tag}(linux/{host_arch})…"), "log_tail": log, "error": Value::Null }));
            match crate::docker::pull_retag(&docker, m, &repo, &tag, &host_arch).await {
                Ok(crate::docker::PullOutcome::Tagged(arch)) => {
                    set_task(&tid2, json!({ "status": "done", "progress": format!("完成:{repo}:{tag}({arch})"), "log_tail": log, "error": Value::Null }));
                    return;
                }
                Ok(crate::docker::PullOutcome::ArchMismatch(arch)) => {
                    log.push(format!("{m} 拉到 {arch}(非 {host_arch}),弃用"));
                    log = log.split_off(log.len().saturating_sub(20));
                }
                Err(e) => {
                    log.push(format!("{m} 失败:{e}"));
                    log = log.split_off(log.len().saturating_sub(20));
                }
            }
        }
        set_task(&tid2, json!({ "status": "error", "progress": "", "log_tail": log,
            "error": "所有镜像源均失败,建议配置 Docker daemon 国内源后重试(见教程)" }));
    });
    Ok(tid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tun_cache_shares_overlap_rechecks_fresh_and_expires_results() {
        let cache = TunChecks::default();
        let key = ("daemon".into(), "image".into());
        let check = |status| result("dev_net_tun", "运行条件", "TUN", status, "", None);
        let (release, wait) = tokio::sync::oneshot::channel();
        let first = cache.sample(key.clone(), true, async { wait.await.unwrap(); check("pass") });
        let second = cache.sample(key.clone(), true, async { panic!("overlapping request created another probe") });
        tokio::pin!(first, second);
        assert!(futures_util::poll!(&mut first).is_pending());
        assert!(futures_util::poll!(&mut second).is_pending());
        release.send(()).unwrap();
        let (a, b) = tokio::join!(first, second);
        assert_eq!(a.status, "pass");
        assert!(b.detail.contains("复用"));
        assert_eq!(cache.sample(key.clone(), false, async { panic!("cached result not reused") }).await.status, "pass");
        assert_eq!(cache.sample(key.clone(), true, async { check("warn") }).await.status, "warn");
        cache.samples.lock().await.get_mut(&key).unwrap().0 = Instant::now() - Duration::from_secs(5);
        assert_eq!(cache.sample(key.clone(), false, async { check("pass") }).await.status, "pass");
        cache.samples.lock().await.get_mut(&key).unwrap().0 = Instant::now() - Duration::from_secs(60);
        assert_eq!(cache.sample(key.clone(), false, async { check("warn") }).await.status, "warn");
        for index in 0..40 {
            cache.sample((format!("daemon-{index}"), "image".into()), false, async { check("pass") }).await;
        }
        assert_eq!(cache.samples.lock().await.len(), 32);
    }

    #[tokio::test]
    async fn tun_check_pins_image_and_separates_daemon_and_image_changes() {
        use axum::{body::Body, http::{Request, StatusCode}, response::IntoResponse, Router};
        use std::sync::Arc;
        let observed = Arc::new(Mutex::new((format!("daemon-{:032x}", rand::random::<u128>()), "sha256:image-a".to_string(), Vec::new())));
        let fixture = observed.clone();
        let app = Router::new().fallback(move |request: Request<Body>| {
            let fixture = fixture.clone();
            async move {
                let path = request.uri().path().to_string();
                if path.ends_with("/info") {
                    return axum::Json(json!({"ID":fixture.lock().unwrap().0})).into_response();
                }
                if path.contains("/images/") {
                    return axum::Json(json!({"Id":fixture.lock().unwrap().1})).into_response();
                }
                if path.ends_with("/containers/create") {
                    let body = axum::body::to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
                    let body: Value = serde_json::from_slice(&body).unwrap();
                    fixture.lock().unwrap().2.push(body["Image"].as_str().unwrap().to_string());
                    return axum::Json(json!({"Id":"owned-probe","Warnings":[]})).into_response();
                }
                if path.ends_with("/wait") { return axum::Json(json!({"StatusCode":0})).into_response(); }
                StatusCode::NO_CONTENT.into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let docker = Docker::connect_with_http(&format!("http://{address}"), 2, bollard::API_DEFAULT_VERSION).unwrap();
        assert_eq!(check_dev_net_tun(&docker, "fixture:tag", true, false).await.status, "pass");
        assert!(check_dev_net_tun(&docker, "fixture:tag", true, false).await.detail.contains("复用"));
        assert!(!check_dev_net_tun(&docker, "fixture:tag", true, true).await.detail.contains("复用"));
        observed.lock().unwrap().1 = "sha256:image-b".into();
        assert!(!check_dev_net_tun(&docker, "fixture:tag", true, false).await.detail.contains("复用"));
        observed.lock().unwrap().0.push_str("-different");
        assert!(!check_dev_net_tun(&docker, "fixture:tag", true, false).await.detail.contains("复用"));
        server.abort(); let _ = server.await;
        assert_eq!(observed.lock().unwrap().2, ["sha256:image-a", "sha256:image-a", "sha256:image-b", "sha256:image-b"]);
    }

    #[tokio::test]
    async fn inventory_dedups_oss_and_has_infra() {
        let inv = image_inventory(None, "arm64", &["docker.1ms.run".into()]).await;
        assert_eq!(inv["host_arch"], "arm64");
        let imgs = inv["images"].as_array().unwrap();
        let oss = imgs.iter().filter(|e| e["repo"] == "vpnmgr/oss-vpn").count();
        assert_eq!(oss, 1, "oss 去重成 1 条");
        assert!(imgs.iter().any(|e| e["repo"] == "metacubex/mihomo"), "infra mihomo");
        assert!(imgs.iter().any(|e| e["role"] == "infra" && e["kind"] == "compose"), "app compose 条");
        let mihomo = imgs.iter().find(|e| e["repo"] == "metacubex/mihomo").unwrap();
        assert!(mihomo["present"].is_null());
    }

    #[test]
    fn pull_task_lifecycle() {
        let mut tasks = PullTasks::default();
        let now = Instant::now();
        let (first, created) = tasks.reserve("repo", "arm64", now).unwrap();
        assert!(created);
        assert_eq!(tasks.reserve("repo:latest", "arm64", now).unwrap(), (first.clone(), false));
        let (second, _) = tasks.reserve("repo:other", "arm64", now).unwrap();
        let later = now + Duration::from_secs(7200);
        assert!(tasks.reserve("repo", "amd64", later).is_err());
        assert_eq!(tasks.reserve("repo", "arm64", later).unwrap(), (first.clone(), false));
        tasks.entries.get_mut(&first).unwrap().state["status"] = json!("done");
        // A published result alone must not release an active worker's slot.
        assert!(tasks.reserve("repo", "amd64", later).is_err());
        tasks.finish(&first, later);
        tasks.prune(later + Duration::from_secs(3599));
        assert!(tasks.entries.contains_key(&first));
        tasks.prune(later + Duration::from_secs(3600));
        assert!(!tasks.entries.contains_key(&first));
        assert!(tasks.entries.contains_key(&second));
        assert!(tasks.reserve("repo", "amd64", later + Duration::from_secs(3600)).is_ok());
    }

    #[test]
    fn pull_tasks_bound_history_and_concurrent_requests() {
        let tasks = std::sync::Arc::new(Mutex::new(PullTasks::default()));
        let requests: Vec<_> = (0..16).map(|_| {
            let tasks = tasks.clone();
            std::thread::spawn(move || tasks.lock().unwrap().reserve("repo", "arm64", Instant::now()).unwrap())
        }).collect();
        let results: Vec<_> = requests.into_iter().map(|r| r.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|(_, created)| *created).count(), 1);
        assert!(results.iter().all(|(id, _)| id == &results[0].0));
        let mut tasks = tasks.lock().unwrap();
        for i in 0..40 {
            let now = Instant::now();
            let (id, _) = tasks.reserve(&format!("history:{i}"), "arm64", now).unwrap();
            tasks.finish(&id, now);
            assert_eq!(tasks.entries[&id].state["status"], "error");
        }
        assert_eq!(tasks.entries.len(), 33); // 32 completed plus the live worker.
    }

    #[tokio::test]
    async fn abandoned_pull_releases_activity_and_publishes_failure() {
        let coordinator = std::sync::Arc::new(crate::runtime_lifecycle::Coordinator::default());
        let activity = coordinator.activity().await.unwrap();
        let (id, _) = tasks().lock().unwrap().reserve("aborted-fixture", "arm64", Instant::now()).unwrap();
        let completion = PullCompletion(id.clone());
        let handle = tokio::spawn(async move {
            let _activity = activity;
            let _completion = completion;
            std::future::pending::<()>().await;
        });
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        let result = get_task(&id).unwrap();
        assert_eq!(result["status"], "error");
        tokio::time::timeout(Duration::from_secs(1), coordinator.quiesce()).await.unwrap();
        assert!(tasks().lock().unwrap().entries[&id].finished.is_some());
    }

    #[tokio::test]
    async fn run_checks_no_docker_daemon_fails_rest_skip() {
        let out = run_checks(None, Some("easyconnect"), Some("7.6.3"), "arm64", "vpnmgr_vpnnet", "preflight", &[], None).await;
        assert_eq!(out["overall"], "fail");
        let checks = out["checks"].as_array().unwrap();
        assert_eq!(checks[0]["id"], "docker_daemon");
        assert_eq!(checks[0]["status"], "fail");
        assert!(checks.iter().skip(1).all(|c| c["status"] == "skip"));
        assert_eq!(out["target_image"].as_str().unwrap(), "hagb/docker-easyconnect:7.6.3");
    }

    #[test]
    fn mihomo_check_reflects_alive() {
        assert_eq!(check_mihomo(true).status, "pass");
        assert_eq!(check_mihomo(false).status, "warn");
    }

    #[test]
    fn buildable_only_vpnmgr() {
        assert!(is_buildable("vpnmgr/oss-vpn:latest"));
        assert!(is_buildable("vpnmgr/byo-desktop:latest"));
        assert!(is_buildable("vpnmgr/hillstone-desktop:latest"));
        assert!(!is_buildable("hagb/docker-easyconnect:7.6.3"));
    }

    #[test]
    fn split_versioned_vs_fixed() {
        let s = split_image("hagb/docker-easyconnect:{version}");
        assert_eq!(s.repo, "hagb/docker-easyconnect");
        assert_eq!(s.tag, None);
        assert!(s.versioned);
        assert_eq!(s.image_field, "hagb/docker-easyconnect");
        let s2 = split_image("metacubex/mihomo:latest");
        assert_eq!(s2.repo, "metacubex/mihomo");
        assert_eq!(s2.tag.as_deref(), Some("latest"));
        assert!(!s2.versioned);
        assert_eq!(s2.image_field, "metacubex/mihomo:latest");
        let s3 = split_image("vpnmgr/oss-vpn");
        assert_eq!(s3.tag.as_deref(), Some("latest"));
    }

    #[test]
    fn resolve_image_substitutes_version() {
        let img = resolve_image("easyconnect", Some("7.6.3")).unwrap();
        assert!(img.contains("7.6.3"));
        assert!(resolve_image("nonexistent-type", None).is_err());
        // 对照 Python `version or "7.6.3"`:None 与空串都回退默认
        assert!(resolve_image("easyconnect", None).unwrap().contains("7.6.3"));
        assert!(resolve_image("easyconnect", Some("")).unwrap().contains("7.6.3"));
    }

    #[test]
    fn known_repos_includes_infra_and_adapters() {
        let repos = known_repos();
        assert!(repos.contains("metacubex/mihomo"), "infra mihomo");
        assert!(repos.iter().any(|r| r.contains("easyconnect")), "EC 适配器");
    }

    #[test]
    fn aggregate_picks_worst_severity() {
        let checks = vec![
            result("a", "x", "t", "pass", "", None),
            result("b", "x", "t", "warn", "", None),
            result("c", "x", "t", "fail", "", None),
        ];
        assert_eq!(aggregate_overall(&checks), "fail");
        let checks2 = vec![result("a", "x", "t", "pass", "", None), result("b", "x", "t", "skip", "", None)];
        assert_eq!(aggregate_overall(&checks2), "pass");
    }
}
