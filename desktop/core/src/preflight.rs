//! Docker 环境/容器体检 + 镜像类修复。对照 app/preflight.py。
//! 检查函数永不抛:内部错误转 warn/fail 的 CheckResult。
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use crate::registry;

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

#[cfg(test)]
mod tests {
    use super::*;

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
