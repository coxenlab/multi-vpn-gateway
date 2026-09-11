//! Checked upstream image identity shared with the Web stack and release tooling.
use anyhow::{anyhow, ensure, Result};
use serde::Deserialize;
use std::collections::HashMap;

pub const MIHOMO_IMAGE: &str = "metacubex/mihomo:v1.19.27";

#[derive(Deserialize)]
struct Sources {
    schema: u32,
    image: String,
    repository: String,
    version: String,
    index: String,
    platforms: HashMap<String, Identity>,
}

#[derive(Deserialize)]
pub struct Identity {
    pub manifest: String,
    pub config: String,
    #[serde(skip)]
    pub index: String,
}

pub fn mihomo(arch: &str) -> Result<Identity> {
    let mut sources: Sources = serde_json::from_str(include_str!("../../../app/mihomo-source.json"))?;
    ensure!(sources.schema == 1 && sources.image == MIHOMO_IMAGE
        && format!("{}:{}", sources.repository, sources.version) == MIHOMO_IMAGE, "mihomo 来源清单与运行版本不一致");
    let mut identity = sources.platforms.remove(arch).ok_or_else(|| anyhow!("mihomo 不支持当前架构: {arch}"))?;
    identity.index = sources.index;
    Ok(identity)
}

pub fn for_image(repo: &str, tag: &str, arch: &str) -> Result<Option<Identity>> {
    if format!("{repo}:{tag}") == MIHOMO_IMAGE { Ok(Some(mihomo(arch)?)) } else { Ok(None) }
}

impl Identity {
    pub fn verify(&self, image_id: Option<&str>) -> Result<()> {
        // Classic image stores use the config digest; containerd stores use
        // the selected manifest or index digest. All are bound by the source lock.
        ensure!(image_id.is_some_and(|id| [self.config.as_str(), self.manifest.as_str(), self.index.as_str()].contains(&id)),
            "mihomo 镜像内容与锁定版本不同，未启用该镜像");
        Ok(())
    }
}

pub async fn installed_mihomo(docker: &bollard::Docker, arch: &str) -> Result<Option<String>> {
    let locked = mihomo(arch)?;
    for reference in [MIHOMO_IMAGE, &locked.config, &locked.manifest, &locked.index] {
        match docker.inspect_image(reference).await {
            Ok(info) if locked.verify(info.id.as_deref()).is_ok() && info.architecture.as_deref() == Some(arch) => return Ok(info.id),
            Ok(_) => {},
            Err(error) if crate::docker::is_not_found(&error) => {},
            Err(error) => return Err(error.into()),
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_lock_checks_actual_content_and_supported_platform() {
        let arm = mihomo("arm64").unwrap();
        let x86 = mihomo("amd64").unwrap();
        assert!(arm.verify(Some(&arm.config)).is_ok());
        assert!(arm.verify(Some(&arm.manifest)).is_ok());
        assert!(arm.verify(Some(&arm.index)).is_ok());
        assert!(arm.verify(Some(&x86.config)).is_err());
        assert!(arm.verify(None).is_err());
        assert!(mihomo("unknown").is_err());
        assert!(for_image("metacubex/mihomo", "v1.19.27", "arm64").unwrap().is_some());
        assert!(for_image("hagb/docker-easyconnect", "7.6.3", "arm64").unwrap().is_none());
    }

    #[tokio::test]
    async fn docker_protocol_pulls_digest_and_never_tags_wrong_content() {
        use axum::{body::Body, http::{Request, StatusCode}, response::IntoResponse, Router};
        use std::sync::{Arc, Mutex};
        let identity = mihomo("arm64").unwrap();
        for (id, arch, succeeds) in [
            (identity.config.clone(), "arm64", true),
            (identity.manifest.clone(), "arm64", true),
            (identity.index.clone(), "arm64", true),
            ("sha256:incorrect".to_string(), "arm64", false),
            (identity.config.clone(), "amd64", false),
        ] {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let observed = calls.clone();
            let app = Router::new().fallback(move |request: Request<Body>| {
                let calls = observed.clone();
                let id = id.clone();
                async move {
                    let url = reqwest::Url::parse(&format!("http://fixture{}", request.uri())).unwrap();
                    calls.lock().unwrap().push((request.method().clone(), url.clone()));
                    if url.path().ends_with("/images/create") {
                        return "{\"status\":\"Pull complete\"}\n".into_response();
                    }
                    if url.path().ends_with("/json") {
                        return axum::Json(serde_json::json!({"Id": id, "Architecture": arch})).into_response();
                    }
                    if url.path().ends_with("/tag") { return StatusCode::CREATED.into_response(); }
                    StatusCode::NOT_FOUND.into_response()
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let docker = bollard::Docker::connect_with_http(&format!("http://{address}"), 5, bollard::API_DEFAULT_VERSION).unwrap();
            let result = crate::docker::pull_retag(&docker, "mirror.invalid", "metacubex/mihomo", "v1.19.27", "arm64").await;
            let installed = installed_mihomo(&docker, "arm64").await.unwrap();
            server.abort(); let _ = server.await;
            assert_eq!(result.is_ok(), succeeds, "{:?}", result.err());
            assert_eq!(installed.is_some(), succeeds);
            let calls = calls.lock().unwrap();
            let pull = calls.iter().find(|(_, url)| url.path().ends_with("/images/create")).unwrap();
            let query: HashMap<_, _> = pull.1.query_pairs().collect();
            assert_eq!(query["fromImage"], format!("mirror.invalid/metacubex/mihomo@{}", identity.manifest));
            assert_eq!(query["platform"], "linux/arm64");
            assert_eq!(calls.iter().filter(|(_, url)| url.path().ends_with("/tag")).count(), usize::from(succeeds));
            assert!(calls.iter().all(|(method, _)| *method != axum::http::Method::DELETE));
        }
    }
}
