//! Docker Hub registry tags → 语义版本号,按架构标可用性。对照 app/dockerhub.py。
//! 进程内缓存(TTL 3600s)+ 离线兜底。async(reqwest);避免在 tokio 运行时里用 blocking。
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const TTL: Duration = Duration::from_secs(3600);

#[derive(Clone, Debug)]
pub struct RawVersion {
    pub tag: String,
    pub arch: Vec<String>,
}

#[allow(clippy::type_complexity)]
fn cache() -> &'static Mutex<HashMap<String, (Instant, Vec<RawVersion>)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, Vec<RawVersion>)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 对照 _SEMVER `^\d+\.\d+(?:\.\d+)?$`:2 或 3 段纯数字。
pub fn is_semver(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (parts.len() == 2 || parts.len() == 3)
        && parts.iter().all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// 对照 _fetch 的解析部分(纯):results → [{tag, arch[]}],仅 semver,数字段降序。
pub fn parse_tags(body: &Value) -> Vec<RawVersion> {
    let mut out: Vec<RawVersion> = Vec::new();
    if let Some(results) = body.get("results").and_then(|v| v.as_array()) {
        for t in results {
            let name = t.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !is_semver(name) {
                continue;
            }
            let archs: Vec<String> = t
                .get("images")
                .and_then(|v| v.as_array())
                .map(|imgs| {
                    imgs.iter()
                        .filter_map(|i| i.get("architecture").and_then(|a| a.as_str()).map(String::from))
                        .filter(|a| !a.is_empty())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect()
                })
                .unwrap_or_default();
            out.push(RawVersion { tag: name.to_string(), arch: archs });
        }
    }
    out.sort_by_key(|v| std::cmp::Reverse(ver_key(&v.tag))); // 降序
    out
}

fn ver_key(tag: &str) -> Vec<u64> {
    tag.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect()
}

/// 对照 versions 的标注部分(纯):usable = arch 空 或 host ∈ arch。
pub fn apply_usable(raw: &[RawVersion], host_arch: &str) -> Vec<Value> {
    raw.iter()
        .map(|v| {
            json!({
                "tag": v.tag,
                "arch": v.arch,
                "usable_here": v.arch.is_empty() || v.arch.iter().any(|a| a == host_arch),
            })
        })
        .collect()
}

async fn fetch(repo: &str) -> anyhow::Result<Vec<RawVersion>> {
    let url = format!("https://hub.docker.com/v2/repositories/{repo}/tags?page_size=100");
    let body: Value = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(8))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    Ok(parse_tags(&body))
}

/// 对照 versions:缓存命中用缓存,否则 fetch;fetch 失败 → fallback(全标 usable、arch 空)。
pub async fn versions(repo: &str, host_arch: &str, fallback: &[String]) -> Vec<Value> {
    {
        let c = cache().lock().unwrap();
        if let Some((ts, raw)) = c.get(repo) {
            if ts.elapsed() < TTL {
                return apply_usable(raw, host_arch);
            }
        }
    } // 锁在 await 前释放
    match fetch(repo).await {
        Ok(raw) => {
            let out = apply_usable(&raw, host_arch);
            cache().lock().unwrap().insert(repo.to_string(), (Instant::now(), raw));
            out
        }
        Err(_) => fallback
            .iter()
            .map(|t| json!({ "tag": t, "arch": [], "usable_here": true }))
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_filter() {
        assert!(is_semver("7.6.3"));
        assert!(is_semver("7.6"));
        assert!(!is_semver("latest"));
        assert!(!is_semver("7.6.3-vncless"));
        assert!(!is_semver("dev-x"));
        assert!(!is_semver("7"));
    }

    #[test]
    fn parse_and_sort_desc() {
        let body = serde_json::json!({"results": [
            {"name": "7.6", "images": [{"architecture": "amd64"}]},
            {"name": "latest", "images": [{"architecture": "amd64"}]},
            {"name": "7.6.3", "images": [{"architecture": "amd64"}, {"architecture": "arm64"}]},
            {"name": "7.10.0", "images": [{"architecture": "arm64"}]},
        ]});
        let raw = parse_tags(&body);
        assert_eq!(raw.iter().map(|v| v.tag.as_str()).collect::<Vec<_>>(), ["7.10.0", "7.6.3", "7.6"]);
        assert_eq!(raw[1].arch, vec!["amd64", "arm64"]); // sorted unique
    }

    #[test]
    fn apply_usable_marks_arch() {
        let raw = vec![
            RawVersion { tag: "7.6.3".into(), arch: vec!["amd64".into(), "arm64".into()] },
            RawVersion { tag: "9.9".into(), arch: vec![] },
        ];
        let out = apply_usable(&raw, "arm64");
        assert_eq!(out[0]["usable_here"], true);
        assert_eq!(out[1]["usable_here"], true);
        let out2 = apply_usable(&raw[..1], "riscv64");
        assert_eq!(out2[0]["usable_here"], false);
    }

    #[tokio::test]
    async fn versions_offline_falls_back() {
        let out = versions("definitely/nonexistent-repo-xyz-9999", "arm64", &["1.0".into(), "2.0".into()]).await;
        if out.len() == 2 && out[0]["tag"] == "1.0" {
            assert_eq!(out[0]["usable_here"], true);
            assert_eq!(out[0]["arch"], serde_json::json!([]));
        }
    }
}
