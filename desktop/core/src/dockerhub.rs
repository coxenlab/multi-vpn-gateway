//! Docker Hub registry tags → 语义版本号,按架构标可用性。对照 app/dockerhub.py。
//! 进程内缓存(TTL 3600s)+ 离线兜底。async(reqwest);避免在 tokio 运行时里用 blocking。
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
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
    let (namespace, repository) = repo.split_once('/').ok_or_else(|| anyhow::anyhow!("invalid repository"))?;
    let url = reqwest::Url::parse(&format!("https://hub.docker.com/v2/namespaces/{namespace}/repositories/{repository}/tags?page_size=100"))?;
    fetch_pages(url, Duration::from_secs(8)).await
}

async fn fetch_pages(first: reqwest::Url, budget: Duration) -> anyhow::Result<Vec<RawVersion>> {
    tokio::time::timeout(budget, fetch_pages_inner(first)).await?
}

async fn fetch_pages_inner(first: reqwest::Url) -> anyhow::Result<Vec<RawVersion>> {
    let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(8)).build()?;
    let (mut url, mut seen, mut tags, mut out) = (first.clone(), HashSet::new(), HashSet::new(), Vec::new());
    loop {
        anyhow::ensure!(url.origin() == first.origin() && url.path().trim_end_matches('/') == first.path().trim_end_matches('/')
            && url.username().is_empty() && url.password().is_none(), "unexpected Docker Hub pagination URL");
        anyhow::ensure!(seen.len() < 100 && seen.insert(url.to_string()), "Docker Hub pagination did not complete");
        let response = client.get(url.clone()).send().await?.error_for_status()?;
        if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
            let location = response.headers().get(reqwest::header::LOCATION)
                .ok_or_else(|| anyhow::anyhow!("invalid Docker Hub redirect"))?.to_str()?;
            url = url.join(location)?;
            continue; // Validate the redirect exactly like a next-page URL.
        }
        anyhow::ensure!(response.status() == reqwest::StatusCode::OK, "unexpected Docker Hub response");
        let body: Value = response.json().await?;
        anyhow::ensure!(body.get("results").is_some_and(Value::is_array), "invalid Docker Hub tag page");
        for tag in body["results"].as_array().unwrap() {
            if tag.get("name").and_then(Value::as_str).is_some_and(|name| is_semver(name) && !tags.contains(name)) {
                anyhow::ensure!(tag.get("images").is_none_or(|v| v.is_null() || v.is_array()), "invalid Docker Hub architecture list");
            }
        }
        for version in parse_tags(&body) {
            if tags.insert(version.tag.clone()) { out.push(version); }
        }
        match body.get("next") {
            None | Some(Value::Null) => break,
            Some(Value::String(next)) if next.is_empty() => break,
            Some(Value::String(next)) => url = url.join(next)?,
            _ => anyhow::bail!("invalid Docker Hub next page"),
        }
    }
    out.sort_by_key(|v| std::cmp::Reverse(ver_key(&v.tag)));
    Ok(out)
}

/// Only complete pagination replaces the cache; errors retain the last complete result.
pub async fn versions(repo: &str, host_arch: &str, fallback: &[String]) -> Vec<Value> {
    versions_with(repo, host_arch, fallback, fetch(repo)).await
}

async fn versions_with(repo: &str, host_arch: &str, fallback: &[String], fetch: impl std::future::Future<Output = anyhow::Result<Vec<RawVersion>>>) -> Vec<Value> {
    {
        let c = cache().lock().unwrap();
        if let Some((ts, raw)) = c.get(repo) {
            if ts.elapsed() < TTL {
                return apply_usable(raw, host_arch);
            }
        }
    } // 锁在 await 前释放
    match fetch.await {
        Ok(raw) => {
            let out = apply_usable(&raw, host_arch);
            cache().lock().unwrap().insert(repo.to_string(), (Instant::now(), raw));
            out
        }
        Err(_) => match cache().lock().unwrap().get(repo) {
            Some((_, raw)) => apply_usable(raw, host_arch),
            None => fallback.iter().map(|t| json!({ "tag": t, "arch": [], "usable_here": true })).collect(),
        },
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

    struct PageServer {
        url: reqwest::Url,
        calls: std::sync::Arc<Mutex<Vec<String>>>,
        task: tokio::task::JoinHandle<()>,
    }
    impl Drop for PageServer { fn drop(&mut self) { self.task.abort(); } }

    async fn page_server(pages: Vec<(u16, Value)>, delay: Duration) -> PageServer {
        let calls = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let observed = calls.clone();
        let pages = std::sync::Arc::new(pages);
        let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
            let observed = observed.clone(); let pages = pages.clone();
            async move {
                let index = { let mut calls = observed.lock().unwrap(); calls.push(uri.to_string()); calls.len() - 1 };
                tokio::time::sleep(delay).await;
                let (status, body) = pages.get(index).cloned().unwrap_or((500, json!({"error":"unexpected request"})));
                let mut headers = axum::http::HeaderMap::new();
                if let Some(location) = body.get("location").and_then(Value::as_str) {
                    headers.insert(axum::http::header::LOCATION, location.parse().unwrap());
                }
                (axum::http::StatusCode::from_u16(status).unwrap(), headers, axum::Json(body))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = reqwest::Url::parse(&format!("http://{}/tags?page_size=100", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        PageServer { url, calls, task }
    }

    #[tokio::test]
    async fn fetches_later_versions_and_deduplicates_before_caching() {
        let mut first: Vec<_> = (0..99).map(|i| json!({"name":format!("ci-{i}")})).collect();
        first.push(json!({"name":"7.6.3","images":[{"architecture":"amd64"}]}));
        let server = page_server(vec![
            (200, json!({"results":first,"next":"?page=2&page_size=100"})),
            (200, json!({"results":[
                {"name":"7.6.7","images":[{"architecture":"arm64"}]},
                {"name":"7.10.0","images":[{"architecture":"arm64"}]},
                {"name":"7.6.3","images":[{"architecture":"arm64"}]}
            ],"next":null})),
        ], Duration::ZERO).await;
        let result = versions_with("fixture-complete", "arm64", &[], fetch_pages(server.url.clone(), Duration::from_secs(1))).await;
        assert_eq!(result.iter().map(|v| v["tag"].as_str().unwrap()).collect::<Vec<_>>(), ["7.10.0", "7.6.7", "7.6.3"]);
        assert_eq!(result[2]["usable_here"], false);
        assert_eq!(server.calls.lock().unwrap().as_slice(), ["/tags?page_size=100", "/tags?page=2&page_size=100"]);
        assert_eq!(versions_with("fixture-complete", "arm64", &[], async { panic!("must use complete cache") }).await, result);
    }

    #[tokio::test]
    async fn follows_canonical_redirect_for_the_same_repository() {
        let server = page_server(vec![
            (301, json!({"location":"/tags/?page_size=100"})),
            (200, json!({"results":[{"name":"7.6.7","images":[{"architecture":"arm64"}]}],"next":null})),
        ], Duration::ZERO).await;
        let raw = fetch_pages(server.url.clone(), Duration::from_secs(1)).await.unwrap();
        assert_eq!(raw[0].tag, "7.6.7");
        assert_eq!(server.calls.lock().unwrap().as_slice(), ["/tags?page_size=100", "/tags/?page_size=100"]);
    }

    #[tokio::test]
    async fn failed_later_page_preserves_complete_stale_cache() {
        let repo = "fixture-stale";
        let original = Instant::now() - TTL;
        cache().lock().unwrap().insert(repo.into(), (original, vec![RawVersion { tag:"7.6.3".into(), arch:vec!["amd64".into()] }]));
        let server = page_server(vec![
            (200, json!({"results":[{"name":"9.9"}],"next":"?page=2"})),
            (503, json!({"error":"fixture failure"})),
        ], Duration::ZERO).await;
        let result = versions_with(repo, "arm64", &["1.0".into()], fetch_pages(server.url.clone(), Duration::from_secs(1))).await;
        assert_eq!(result, vec![json!({"tag":"7.6.3","arch":["amd64"],"usable_here":false})]);
        assert_eq!(cache().lock().unwrap()[repo].0, original);
        assert_eq!(server.calls.lock().unwrap().len(), 2);
        let out = versions_with("fixture-offline", "arm64", &["1.0".into()], async { anyhow::bail!("offline fixture") }).await;
        assert_eq!(out, vec![json!({"tag":"1.0","arch":[],"usable_here":true})]);
        assert!(!cache().lock().unwrap().contains_key("fixture-offline"));
    }

    #[tokio::test]
    async fn refuses_broken_or_cyclic_pagination_before_another_request() {
        for (status, body) in [
            (200, json!({})), (200, json!([])),
            (200, json!({"results":[],"next":42})),
            (200, json!({"results":[],"next":"?page_size=100"})),
            (200, json!({"results":[],"next":"https://example.invalid/tags?page=2"})),
            (200, json!({"results":[],"next":"/other/tags?page=2"})),
            (200, json!({"results":[{"name":"7.6.7","images":"invalid"}],"next":null})),
            (302, json!({"results":[],"next":null})),
            (302, json!({"location":"https://example.invalid/tags"})),
        ] {
            let server = page_server(vec![(status,body)], Duration::ZERO).await;
            assert!(fetch_pages(server.url.clone(), Duration::from_secs(1)).await.is_err());
            assert_eq!(server.calls.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn pagination_deadline_bounds_a_stalled_response() {
        let server = page_server(vec![(200,json!({"results":[],"next":null}))], Duration::from_secs(10)).await;
        let error = fetch_pages(server.url.clone(), Duration::from_millis(50)).await.unwrap_err();
        assert!(error.downcast_ref::<tokio::time::error::Elapsed>().is_some());
        assert_eq!(server.calls.lock().unwrap().len(), 1);
    }
}
