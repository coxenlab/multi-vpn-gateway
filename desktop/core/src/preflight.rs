//! Docker 环境/容器体检 + 镜像类修复。对照 app/preflight.py。
//! 检查函数永不抛:内部错误转 warn/fail 的 CheckResult。
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use bollard::Docker;
use crate::{registry, dockerhub};

/// 自建镜像本地构建上下文(镜像名前缀 → 仓库内目录)。对照 _BUILD_CONTEXT。
pub const BUILD_CONTEXT: &[(&str, &str)] = &[
    ("vpnmgr/oss-vpn", "images/oss"),
    ("vpnmgr/byo-desktop", "images/byo"),
];

/// P1 硬编码镜像源(对照 DEFAULT_MIRRORS)。
pub const DEFAULT_MIRRORS: &[&str] = &["docker.1ms.run", "hub.rat.dev"];

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
        image = image.replace("{version}", version.unwrap_or("7.6.3"));
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
    InfraImage { image: "metacubex/mihomo:latest", kind: "pull", title: "mihomo 分流底座", build_context: None, arch: &["amd64", "arm64"] },
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

pub async fn check_dev_net_tun(docker: &Docker, image: &str, image_ok: bool) -> CheckResult {
    if !image_ok {
        return result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "skip", "镜像就绪后检测", None);
    }
    match crate::docker::run_tun_probe(docker, image).await {
        Ok(true) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "pass", "", None),
        Ok(false) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn", "容器内未见 /dev/net/tun,VPN 隧道可能起不来", None),
        Err(e) => result("dev_net_tun", "运行条件", "/dev/net/tun 可用", "warn", &format!("无法判定(尽力而为):{e}"), None),
    }
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
        Some(img) => check_dev_net_tun(d, img, image_ok).await,
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
fn tasks() -> &'static Mutex<HashMap<String, Value>> {
    static T: OnceLock<Mutex<HashMap<String, Value>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn get_task(tid: &str) -> Option<Value> {
    tasks().lock().unwrap().get(tid).cloned()
}

fn set_task(tid: &str, v: Value) {
    tasks().lock().unwrap().insert(tid.to_string(), v);
}

/// 对照 start_pull:后台任务遍历 mirror 拉取(pull_retag),更新任务表。返回 task_id(8 hex)。
pub fn start_pull(docker: Docker, image: &str, host_arch: &str, mirrors: Vec<String>) -> String {
    let tid: String = (0..4).map(|_| format!("{:02x}", rand::random::<u8>())).collect();
    set_task(&tid, json!({ "status": "running", "progress": "准备拉取…", "log_tail": [], "error": Value::Null }));
    let (image, host_arch, tid2) = (image.to_string(), host_arch.to_string(), tid.clone());
    let mirrors = if mirrors.is_empty() {
        DEFAULT_MIRRORS.iter().map(|s| s.to_string()).collect()
    } else {
        mirrors
    };
    tokio::spawn(async move {
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
            set_task(&tid2, json!({ "status": "running", "progress": format!("从 {m} 拉取 {repo}:{tag}…"), "log_tail": log, "error": Value::Null }));
            match crate::docker::pull_retag(&docker, m, &repo, &tag, &host_arch).await {
                Ok(arch) => {
                    set_task(&tid2, json!({ "status": "done", "progress": format!("完成:{repo}:{tag}({arch})"), "log_tail": log, "error": Value::Null }));
                    return;
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
    tid
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(get_task("nope").is_none());
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
