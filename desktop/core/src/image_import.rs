//! Private staged image import. Preview never starts Docker; confirmation loads once and reads back IDs.
use crate::{
    image_archive::{self, Preview},
    AppState,
};
use anyhow::{anyhow, ensure, Result};
use axum::{
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde_json::json;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

type JobRef = Arc<Mutex<Job>>;
type JobKey = (PathBuf, String);
static JOBS: LazyLock<Mutex<HashMap<JobKey, JobRef>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static UPLOAD: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
static LOAD: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
const TTL: Duration = Duration::from_secs(30 * 60);
struct Job {
    file: Option<std::fs::File>,
    preview: Preview,
    status: &'static str,
    progress: String,
    error: Option<String>,
    created: Instant,
}
fn reply(id: &str, job: &Job) -> Response {
    Json(json!({"id":id,"preview":job.preview,"status":job.status,"progress":job.progress,"error":job.error})).into_response()
}
fn lookup(state: &AppState, id: &str) -> Option<JobRef> {
    JOBS.lock()
        .unwrap()
        .get(&(state.cfg.data_dir.clone(), id.to_string()))
        .cloned()
}
fn prune(jobs: &mut HashMap<JobKey, JobRef>) {
    jobs.retain(|_, job| {
        let job = job.lock().unwrap();
        job.status == "loading" || job.created.elapsed() < TTL
    });
}
pub async fn preview(State(state): State<AppState>, request: Request) -> Response {
    match prepare(&state, request).await {
        Ok((id, job)) => reply(&id, &job.lock().unwrap()),
        Err(error) => crate::api::err_detail(StatusCode::BAD_REQUEST, &error.to_string()),
    }
}
async fn prepare(state: &AppState, request: Request) -> Result<(String, JobRef)> {
    let _permit = UPLOAD
        .try_acquire()
        .map_err(|_| anyhow!("另一个镜像包正在校验，请稍后再试"))?;
    {
        let mut jobs = JOBS.lock().unwrap();
        prune(&mut jobs);
        ensure!(
            jobs.keys()
                .filter(|(path, _)| *path == state.cfg.data_dir)
                .count()
                < 4,
            "请先关闭已有镜像预览再选择新文件"
        );
    }
    if let Some(size) = request
        .headers()
        .get("content-length")
        .and_then(|s| s.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
    {
        ensure!(
            size > 0 && size <= image_archive::MAX_ARCHIVE,
            "镜像归档为空或超过 12 GiB"
        );
    }
    // Anonymous private file: kernel cleanup also covers crashes; no archive pathname or restart residue.
    let mut file = tokio::fs::File::from_std(tempfile::tempfile_in(&state.cfg.data_dir)?);
    let mut stream = request.into_body().into_data_stream();
    let mut size = 0u64;
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(60), stream.next())
        .await
        .map_err(|_| anyhow!("镜像文件传输长时间无进展"))?
    {
        let chunk = chunk?;
        size += chunk.len() as u64;
        ensure!(size <= image_archive::MAX_ARCHIVE, "镜像归档超过 12 GiB");
        file.write_all(&chunk).await?;
    }
    file.sync_all().await?;
    let mut file = file.into_std().await;
    let destination = tempfile::tempfile_in(&state.cfg.data_dir)?;
    let archive = tokio::task::spawn_blocking(move || -> Result<_> {
        image_archive::prepare(&mut file, destination, &crate::registry::host_arch())
    })
    .await??;
    let id: String = (0..16)
        .map(|_| format!("{:02x}", rand::random::<u8>()))
        .collect();
    let job = Arc::new(Mutex::new(Job {
        file: Some(archive.0),
        preview: archive.1,
        status: "preview",
        progress: "校验完成，等待确认导入".into(),
        error: None,
        created: Instant::now(),
    }));
    JOBS.lock()
        .unwrap()
        .insert((state.cfg.data_dir.clone(), id.clone()), job.clone());
    tokio::spawn(async {
        tokio::time::sleep(TTL).await;
        prune(&mut JOBS.lock().unwrap());
    });
    Ok((id, job))
}
pub async fn status(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(job) = lookup(&state, &id) else {
        return crate::api::err404("镜像预览已失效，请重新选择文件");
    };
    let response = reply(&id, &job.lock().unwrap());
    response
}
pub async fn discard(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let mut jobs = JOBS.lock().unwrap();
    let key = (state.cfg.data_dir.clone(), id);
    if jobs
        .get(&key)
        .is_some_and(|job| job.lock().unwrap().status == "loading")
    {
        return crate::api::err_detail(StatusCode::CONFLICT, "镜像正在导入，请等待结果后再关闭");
    }
    jobs.remove(&key);
    Json(json!({"ok":true})).into_response()
}
pub async fn confirm(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let Some(job) = lookup(&state, &id) else {
        return crate::api::err404("镜像预览已失效，请重新选择文件");
    };
    {
        let job = job.lock().unwrap();
        // A lost response can be recovered by querying the same ticket; never load it twice.
        if job.status != "preview" {
            return reply(&id, &job);
        }
    }
    if !crate::runtime::serving(&state) {
        return crate::api::err_detail(
            StatusCode::CONFLICT,
            "请先在设置中连接运行环境，再确认导入",
        );
    }
    let Some(docker) = state.docker() else {
        return crate::api::err503("运行环境暂不可用，镜像预览已保留");
    };
    let Ok(permit) = LOAD.try_acquire() else {
        return crate::api::err_detail(StatusCode::CONFLICT, "已有镜像正在导入，请稍后再试");
    };
    let activity = match state.lifecycle.runtime().activity().await {
        Ok(guard) => guard,
        Err(error) => return crate::api::err503(&error),
    };
    let (file, preview) = {
        let mut job = job.lock().unwrap();
        if job.status != "preview" {
            return reply(&id, &job);
        }
        let Some(file) = job.file.take() else {
            return crate::api::err404("镜像预览已失效");
        };
        job.status = "loading";
        job.progress = "正在核对运行环境中的镜像…".into();
        (file, job.preview.clone())
    };
    let captured = job.clone();
    tokio::spawn(async move {
        let _permit = permit;
        let _activity = activity;
        let result = tokio::time::timeout(
            Duration::from_secs(1200),
            load(&docker, file, &preview, captured.clone()),
        )
        .await;
        let mut job = captured.lock().unwrap();
        match result {
            Ok(Ok(already)) => {
                job.status = "done";
                job.progress = if already {
                    "相同镜像已存在，无需重复导入"
                } else {
                    "镜像已导入并核对，可用于创建通道"
                }
                .into();
                crate::events::audit(
                    "image_import",
                    "镜像模板导入已确认",
                    json!({"images":preview.images.iter().map(|i|&i.tags).collect::<Vec<_>>(),"sha256":preview.sha256,"already_present":already,"result":"success"}),
                );
            }
            result => {
                job.status = "error";
                job.error = Some(match result {
                    Ok(Err(error)) => error.to_string(),
                    _ => "导入超时，结果尚未确认；请刷新镜像清单后再决定是否重新导入".into(),
                });
                job.progress = "导入未确认".into();
            }
        }
    });
    (
        StatusCode::ACCEPTED,
        Json(json!({"id":id,"status":"loading"})),
    )
        .into_response()
}
async fn matches(docker: &bollard::Docker, preview: &Preview) -> Result<bool> {
    for image in &preview.images {
        for tag in &image.tags {
            match docker.inspect_image(tag).await {
                Ok(info) => {
                    if info.id.as_deref() != Some(&image.id)
                        || info.architecture.as_deref() != Some(&image.architecture)
                        || info.os.as_deref() != Some("linux")
                    {
                        return Ok(false);
                    }
                }
                Err(bollard::errors::Error::DockerResponseServerError {
                    status_code: 404, ..
                }) => return Ok(false),
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(true)
}
async fn load(
    docker: &bollard::Docker,
    file: std::fs::File,
    preview: &Preview,
    job: JobRef,
) -> Result<bool> {
    if matches(docker, preview).await? {
        return Ok(true);
    }
    let mut file = tokio::fs::File::from_std(file);
    file.seek(std::io::SeekFrom::Start(0)).await?;
    let read_error = Arc::new(Mutex::new(None::<String>));
    let failures = read_error.clone();
    let total = file.metadata().await?.len();
    let stream = futures_util::stream::unfold(
        (file, 0u64, job, failures),
        move |(mut file, sent, job, failures)| async move {
            let mut buffer = vec![0; 65536];
            match file.read(&mut buffer).await {
                Ok(0) => None,
                Ok(n) => {
                    buffer.truncate(n);
                    let sent = sent + n as u64;
                    job.lock().unwrap().progress =
                        format!("正在传入镜像：{} / {} MiB", sent / 1048576, total / 1048576);
                    Some((bytes::Bytes::from(buffer), (file, sent, job, failures)))
                }
                Err(_) => {
                    *failures.lock().unwrap() = Some("读取暂存镜像失败".into());
                    None
                }
            }
        },
    );
    let mut responses = docker.import_image_stream(
        bollard::image::ImportImageOptions { quiet: true },
        stream,
        None,
    );
    let mut response_error = None;
    while let Some(response) = responses.next().await {
        if let Err(error) = response {
            response_error = Some(error);
            break;
        }
    }
    // Load success is never inferred from progress text or a 2xx alone.
    ensure!(
        read_error.lock().unwrap().is_none(),
        "镜像读取中断，导入结果未确认"
    );
    let confirmed = matches(docker, preview)
        .await
        .context("导入后无法核对镜像，结果未确认")?;
    ensure!(
        confirmed,
        "镜像标签、架构或摘要读回不一致，导入未确认{}",
        if response_error.is_some() {
            "（运行环境未返回完整成功应答）"
        } else {
            ""
        }
    );
    Ok(false)
}
use anyhow::Context;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    async fn state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = crate::config::Config::from_getter(|_| None);
        cfg.data_dir = dir.path().into();
        cfg.ui_port = 0;
        cfg.managed_vm = true;
        cfg.dev_mode = true;
        cfg.vm_profile = "vpnmgr-image-qa".into();
        let (_, state) = crate::app::bootstrap(cfg).await.unwrap();
        (dir, state)
    }
    async fn call(
        app: axum::Router,
        method: &str,
        path: &str,
        body: Vec<u8>,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1048576)
                .await
                .unwrap(),
        )
        .unwrap();
        (status, value)
    }
    #[tokio::test]
    async fn preview_is_offline_scoped_cancellable_and_leaves_no_named_archive() {
        let _serial = SERIAL.lock().await;
        let (dir, state) = state().await;
        let app = crate::server::build_router(state.clone());
        let before = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        let bytes = crate::image_archive::tests::fixture(
            &crate::registry::host_arch(),
            "vpnmgr/oss-vpn:latest",
            false,
        );
        let (status, preview) = call(app.clone(), "POST", "/api/images/imports", bytes).await;
        assert_eq!(status, StatusCode::OK, "{preview}");
        let id = preview["id"].as_str().unwrap();
        let (status, _) = call(
            app.clone(),
            "POST",
            &format!("/api/images/imports/{id}/confirm"),
            vec![],
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(state.lifecycle.runtime().snapshot().start_attempt, 0);
        let (_, other) = self::state().await;
        assert!(lookup(&other, id).is_none());
        let after = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|p| p.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(before, after);
        assert_eq!(
            call(
                app.clone(),
                "DELETE",
                &format!("/api/images/imports/{id}"),
                vec![]
            )
            .await
            .0,
            StatusCode::OK
        );
        assert_eq!(
            call(
                app.clone(),
                "GET",
                &format!("/api/images/imports/{id}"),
                vec![]
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(app, "POST", "/api/images/imports", b"installer".to_vec())
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(state.lifecycle.runtime().snapshot().start_attempt, 0);
    }
    #[tokio::test]
    async fn import_reads_back_ids_and_does_not_repeat_after_lost_ack() {
        let _serial = SERIAL.lock().await;
        for mode in [
            "success",
            "already",
            "lost_ack",
            "failed",
            "wrong_id",
            "read_error",
        ] {
            let (_dir, mut state) = state().await;
            Arc::make_mut(&mut state.cfg).managed_vm = false;
            let bytes = crate::image_archive::tests::fixture(
                &crate::registry::host_arch(),
                "vpnmgr/oss-vpn:latest",
                false,
            );
            let app = crate::server::build_router(state.clone());
            let (status, preview) =
                call(app.clone(), "POST", "/api/images/imports", bytes.clone()).await;
            assert_eq!(status, StatusCode::OK, "{preview}");
            let id = preview["id"].as_str().unwrap();
            let image_id = preview["preview"]["images"][0]["id"]
                .as_str()
                .unwrap()
                .to_owned();
            let observed = Arc::new(Mutex::new((mode == "already", 0u32)));
            let capture = observed.clone();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            state.set_docker(Some(
                bollard::Docker::connect_with_http(
                    &format!("http://{}", listener.local_addr().unwrap()),
                    2,
                    bollard::API_DEFAULT_VERSION,
                )
                .unwrap(),
            ));
            let docker_api=axum::Router::new().fallback(move |request:Request<Body>| {
                let capture=capture.clone();let bytes=bytes.clone();let image_id=image_id.clone();
                async move {
                    if request.method()=="POST" {
                        assert!(request.uri().path().ends_with("/images/load"));
                        let received=axum::body::to_bytes(request.into_body(),1048576).await.unwrap();
                        let mut expected=std::io::Cursor::new(&bytes);let mut actual=std::io::Cursor::new(received);
                        let expected=tar::Archive::new(&mut expected).entries().unwrap().map(|e|e.unwrap().path().unwrap().into_owned()).collect::<std::collections::BTreeSet<_>>();
                        let actual=tar::Archive::new(&mut actual).entries().unwrap().map(|e|e.unwrap().path().unwrap().into_owned()).collect::<std::collections::BTreeSet<_>>();assert_eq!(actual,expected);
                        let mut s=capture.lock().unwrap();s.1+=1;s.0=true;
                        if mode=="lost_ack" {return (StatusCode::INTERNAL_SERVER_ERROR,Json(json!({"message":"response lost"}))).into_response();}
                        if mode=="failed" {return Json(json!({"error":"load failed"})).into_response();}
                        return Json(json!({"stream":"Loaded image"})).into_response();
                    }
                    assert!(request.uri().path().ends_with("/json"));
                    let s=capture.lock().unwrap();
                    if mode=="read_error" && s.0 {return (StatusCode::SERVICE_UNAVAILABLE,Json(json!({"message":"unavailable"}))).into_response();}
                    if !s.0 || mode=="failed" {return (StatusCode::NOT_FOUND,Json(json!({"message":"missing"}))).into_response();}
                    Json(json!({"Id":if mode=="wrong_id" {"sha256:wrong"} else {&image_id},"Architecture":crate::registry::host_arch(),"Os":"linux"})).into_response()
                }
            });
            let server = tokio::spawn(async move {
                axum::serve(listener, docker_api).await.unwrap();
            });
            let path = format!("/api/images/imports/{id}");
            assert_eq!(
                call(app.clone(), "POST", &format!("{path}/confirm"), vec![])
                    .await
                    .0,
                StatusCode::ACCEPTED
            );
            // Repeat confirmation of the same ticket while loading or after completion is read-only.
            let second = call(app.clone(), "POST", &format!("{path}/confirm"), vec![]).await;
            assert!(second.0.is_success());
            let result = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let (_, value) = call(app.clone(), "GET", &path, vec![]).await;
                    if value["status"] != "loading" {
                        break value;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                result["status"],
                if matches!(mode, "success" | "already" | "lost_ack") {
                    "done"
                } else {
                    "error"
                },
                "{mode}: {result}"
            );
            assert_eq!(
                observed.lock().unwrap().1,
                if mode == "already" { 0 } else { 1 },
                "{mode}"
            );
            assert_eq!(state.lifecycle.runtime().snapshot().start_attempt, 0);
            assert_eq!(call(app, "DELETE", &path, vec![]).await.0, StatusCode::OK);
            server.abort();
        }
    }
}
