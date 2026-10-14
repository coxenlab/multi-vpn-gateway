//! Installer bytes stay in anonymous temporary files, never in the database or a full memory buffer.
use axum::extract::Multipart;
use axum::http::StatusCode;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::HashSet, io::{Seek, SeekFrom}, path::{Path, PathBuf}, sync::{Arc, Mutex, OnceLock}, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const MAX_BYTES: usize = 1024 * 1024 * 1024;
pub const BODY_LIMIT: usize = MAX_BYTES + 65536;
pub static SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
pub type Failure = (StatusCode, String);
fn bad(message: &str) -> Failure { (StatusCode::BAD_REQUEST, message.into()) }
fn disk(_: std::io::Error) -> Failure { (StatusCode::INSUFFICIENT_STORAGE, "无法暂存安装包，请检查可用磁盘空间".into()) }

pub fn valid_filename(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && name.len() <= 255
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | '"'))
}

pub struct Upload { pub filename: String, pub size: u64, file: std::fs::File }

const INSTALL_LOG_BYTES: usize = 64 * 1024;
static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn active() -> &'static Mutex<HashSet<String>> { ACTIVE.get_or_init(|| Mutex::new(HashSet::new())) }
pub fn mark_active(attempt_id: &str) { active().lock().unwrap().insert(attempt_id.to_string()); }
fn unmark_active(attempt_id: &str) { active().lock().unwrap().remove(attempt_id); }
fn is_active(attempt_id: &str) -> bool { active().lock().unwrap().contains(attempt_id) }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstallStatus {
    pub phase: String,
    pub attempt_id: String,
    pub filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exec_id: Option<String>,
}

fn status_path(directory: &Path, cid: &str) -> PathBuf {
    let key = format!("{:x}", Sha256::digest(cid.as_bytes()));
    directory.join("installer-status").join(format!("{key}.json"))
}

fn save_status(directory: &Path, cid: &str, status: &InstallStatus) -> anyhow::Result<()> {
    let path = status_path(directory, cid);
    let parent = path.parent().expect("installer status parent");
    std::fs::create_dir_all(parent)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut tmp, status)?;
    tmp.as_file_mut().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    Ok(())
}

pub fn load_status(directory: &Path, cid: &str) -> anyhow::Result<Option<InstallStatus>> {
    let path = status_path(directory, cid);
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A queued/running record without a task in this core process belongs to an interrupted
/// attempt. We deliberately do not infer success or replay it after restart.
pub fn public_status(directory: &Path, cid: &str) -> anyhow::Result<Option<InstallStatus>> {
    let Some(mut status) = load_status(directory, cid)? else { return Ok(None) };
    if matches!(status.phase.as_str(), "queued" | "running") && !is_active(&status.attempt_id) {
        status.phase = "interrupted".into();
        status.finished_at = Some(chrono::Utc::now().to_rfc3339());
        status.log = Some("安装结果尚未确认，可能仍在通道内运行；请在登录窗口确认当前状态，不会自动重装".into());
        save_status(directory, cid, &status)?;
    }
    Ok(Some(status))
}

/// Guard the uploaded pathname as well as package-manager execution. After a core restart,
/// the old attached exec may still be reading `/root/<package>`; do not overwrite it until
/// Docker confirms that exec has ended. Only an explicit Docker 404 proves it is gone.
pub async fn install_in_progress(
    docker: &bollard::Docker,
    directory: &Path,
    cid: &str,
) -> anyhow::Result<bool> {
    let Some(status) = load_status(directory, cid)? else { return Ok(false) };
    if is_active(&status.attempt_id) { return Ok(true); }
    let Some(exec_id) = status.exec_id else { return Ok(false) };
    match docker.inspect_exec(&exec_id).await {
        Ok(info) => info.running.ok_or_else(|| anyhow::anyhow!("Docker 没有返回上次安装任务的运行状态")),
        Err(bollard::errors::Error::DockerResponseServerError { status_code: 404, .. }) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("无法确认上次安装任务是否仍在运行：{e}")),
    }
}

fn tail_text(mut bytes: Vec<u8>) -> String {
    if bytes.len() > INSTALL_LOG_BYTES { bytes.drain(..bytes.len() - INSTALL_LOG_BYTES); }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn install_command(package_path: String) -> Vec<String> {
    vec!["/usr/local/bin/tongdao-install".into(), package_path]
}

fn phase_for_exit(code: i64) -> &'static str {
    match code { 0 => "succeeded", 20 => "manual", 21 => "launch_failed", _ => "failed" }
}

pub async fn receive(mp: &mut Multipart, directory: &Path, maximum: usize) -> Result<Upload, Failure> {
    let mut field = tokio::time::timeout(Duration::from_secs(60), mp.next_field()).await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "等待安装包超时".into()))?
        .map_err(|e| (e.status(), "安装包表单无效或超过 1 GiB".into()))?
        .ok_or_else(|| bad("请选择安装包文件"))?;
    if field.name() != Some("file") { return Err(bad("安装包字段须为 file")); }
    let filename = field.file_name().unwrap_or_default().to_string();
    if !valid_filename(&filename) { return Err(bad("安装包文件名无效，请重命名后上传")); }
    let mut file = tokio::fs::File::from_std(tempfile::tempfile_in(directory).map_err(disk)?);
    let mut size = 0u64;
    loop {
        let chunk = tokio::time::timeout(Duration::from_secs(60), field.chunk()).await
            .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "安装包上传超时".into()))?
            .map_err(|e| (e.status(), "安装包上传中断或超过 1 GiB".into()))?;
        let Some(chunk) = chunk else { break };
        size += chunk.len() as u64;
        if size > maximum as u64 { return Err((StatusCode::PAYLOAD_TOO_LARGE, "安装包超过 1 GiB".into())); }
        file.write_all(&chunk).await.map_err(disk)?;
    }
    if size == 0 { return Err(bad("安装包不能为空")); }
    drop(field);
    if tokio::time::timeout(Duration::from_secs(60), mp.next_field()).await
        .map_err(|_| (StatusCode::REQUEST_TIMEOUT, "安装包上传超时".into()))?
        .map_err(|e| (e.status(), "安装包表单不完整".into()))?.is_some() {
        return Err(bad("每次只能上传一个安装包"));
    }
    file.flush().await.map_err(disk)?;
    Ok(Upload { filename, size, file: file.into_std().await })
}

impl Upload {
    pub async fn deliver(self, docker: &bollard::Docker, container: &str, directory: &Path) -> anyhow::Result<String> {
        let installed_path = format!("/root/{}", self.filename);
        let destination = tempfile::tempfile_in(directory)?;
        let archive = tokio::task::spawn_blocking(move || -> anyhow::Result<std::fs::File> {
            let mut source = self.file;
            source.seek(SeekFrom::Start(0))?;
            let mut archive = tar::Builder::new(destination);
            let mut header = tar::Header::new_gnu();
            header.set_size(self.size); header.set_mode(0o755); header.set_cksum();
            archive.append_data(&mut header, &self.filename, &mut source)?;
            let mut file = archive.into_inner()?;
            file.seek(SeekFrom::Start(0))?;
            Ok(file)
        }).await??;
        let read_error = Arc::new(Mutex::new(false));
        let failure = read_error.clone();
        let stream = futures_util::stream::unfold((tokio::fs::File::from_std(archive), failure), |(mut file, failure)| async move {
            let mut buffer = vec![0; 65536];
            match file.read(&mut buffer).await {
                Ok(0) => None,
                Ok(n) => { buffer.truncate(n); Some((bytes::Bytes::from(buffer), (file, failure))) },
                Err(_) => { *failure.lock().unwrap() = true; None },
            }
        });
        let delivered = docker.upload_to_container_streaming(container,
            Some(bollard::container::UploadToContainerOptions { path: "/root", ..Default::default() }), stream).await;
        anyhow::ensure!(!*read_error.lock().unwrap(), "读取暂存安装包失败，投递结果未确认");
        delivered?;
        Ok(installed_path)
    }
}

/// Start one explicit installer attempt. The caller persists `queued` before spawning this
/// future, so a failed task can never be mistaken for a completed upload or replayed silently.
pub struct InstallRequest {
    pub container: String,
    pub package_path: String,
    pub directory: PathBuf,
    pub db_path: PathBuf,
    pub cid: String,
    pub expected_container_id: Option<String>,
}

enum InstallRunOutcome {
    Finished { code: i64, log: String },
    FailedBeforeStart(String),
    Uncertain { error: String, log: String },
}

fn uncertain(error: impl ToString, output: Vec<u8>) -> InstallRunOutcome {
    InstallRunOutcome::Uncertain { error: error.to_string(), log: tail_text(output) }
}

pub async fn run_install(docker: bollard::Docker, request: InstallRequest, mut status: InstallStatus) {
    let InstallRequest { container, package_path, directory, db_path, cid, expected_container_id } = request;
    let attempt_id = status.attempt_id.clone();
    let outcome = 'run: {
        let exec = match docker.create_exec(&container, CreateExecOptions {
            cmd: Some(install_command(package_path)),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        }).await {
            Ok(exec) => exec,
            Err(e) => break 'run InstallRunOutcome::FailedBeforeStart(e.to_string()),
        };
        status.phase = "running".into();
        status.started_at = Some(chrono::Utc::now().to_rfc3339());
        status.exec_id = Some(exec.id.clone());
        if let Err(e) = save_status(&directory, &cid, &status) {
            // create_exec alone does not execute the process, so this failure is safe to retry.
            break 'run InstallRunOutcome::FailedBeforeStart(e.to_string());
        }

        let mut output_bytes = Vec::new();
        match docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, ..Default::default() })).await {
            Ok(StartExecResults::Attached { mut output, .. }) => {
                while let Some(item) = output.next().await {
                    let bytes = match item {
                        Ok(item) => item.into_bytes(),
                        Err(e) => break 'run uncertain(e, output_bytes),
                    };
                    output_bytes.extend_from_slice(bytes.as_ref());
                    if output_bytes.len() > INSTALL_LOG_BYTES * 2 {
                        output_bytes.drain(..output_bytes.len() - INSTALL_LOG_BYTES);
                    }
                }
            }
            Ok(StartExecResults::Detached) => break 'run uncertain("安装命令意外转为后台执行", output_bytes),
            Err(e) => break 'run uncertain(e, output_bytes),
        }
        let inspected = match docker.inspect_exec(&exec.id).await {
            Ok(info) => info,
            Err(e) => break 'run uncertain(e, output_bytes),
        };
        if inspected.running != Some(false) {
            break 'run uncertain("安装命令的结束状态尚未确认", output_bytes);
        }
        let Some(code) = inspected.exit_code else {
            break 'run uncertain("安装命令退出码尚未确认", output_bytes);
        };
        InstallRunOutcome::Finished { code, log: tail_text(output_bytes) }
    };

    status.finished_at = Some(chrono::Utc::now().to_rfc3339());
    match outcome {
        InstallRunOutcome::Finished { code, log } => {
            status.exec_id = None;
            status.exit_code = Some(code);
            status.log = (!log.is_empty()).then_some(log);
            status.phase = phase_for_exit(code).into();
            if code != 0 && status.log.is_none() {
                status.log = Some(format!("安装命令退出码 {code}"));
            }
        }
        InstallRunOutcome::FailedBeforeStart(error) => {
            status.exec_id = None;
            status.phase = "failed".into();
            status.log = Some(error);
        }
        InstallRunOutcome::Uncertain { error, log } => {
            status.phase = "interrupted".into();
            status.exit_code = None;
            status.log = Some(if log.is_empty() { format!("安装结果尚未确认：{error}；不会自动重装") }
                else { format!("{log}\n安装结果尚未确认：{error}；不会自动重装") });
        }
    }
    // A stopped container produces a normal exec failure. A deleted/replaced channel is
    // different: never attach this attempt's result to the new instance.
    let still_same = crate::store::get_channel(&db_path, &cid).ok().flatten()
        .is_some_and(|ch| ch.container_id == expected_container_id);
    if !still_same {
        status.phase = "interrupted".into();
        status.exit_code = None;
        status.log = Some("安装期间通道实例已删除或替换，结果未应用".into());
    }
    let saved = save_status(&directory, &cid, &status);
    unmark_active(&attempt_id);
    if let Err(e) = saved {
        crate::events::audit_failed("channel_install", "安装结果状态保存失败", serde_json::json!({
            "target_kind":"channel", "target_id":cid, "error":e.to_string()
        }));
    } else if status.phase == "succeeded" {
        crate::events::audit("channel_install", "官方安装包安装完成", serde_json::json!({
            "target_kind":"channel", "target_id":cid, "attempt_id":status.attempt_id
        }));
    } else if status.phase == "manual" {
        crate::events::audit("channel_install", "安装包等待在登录窗口交互安装", serde_json::json!({
            "target_kind":"channel", "target_id":cid, "attempt_id":status.attempt_id
        }));
    } else if status.phase == "interrupted" {
        crate::events::audit_failed("channel_install", "官方安装包安装结果尚未确认", serde_json::json!({
            "target_kind":"channel", "target_id":cid, "attempt_id":status.attempt_id
        }));
    } else {
        crate::events::audit_failed("channel_install", "官方安装包安装失败", serde_json::json!({
            "target_kind":"channel", "target_id":cid, "attempt_id":status.attempt_id,
            "exit_code":status.exit_code
        }));
    }
}

pub fn queued(filename: String) -> InstallStatus {
    InstallStatus {
        phase: "queued".into(),
        attempt_id: format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>()),
        filename,
        started_at: None,
        finished_at: None,
        exit_code: None,
        log: None,
        exec_id: None,
    }
}

pub fn persist_queued(directory: &Path, cid: &str, status: &InstallStatus) -> anyhow::Result<()> {
    save_status(directory, cid, status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::{Body, to_bytes}, http::{Request, Response}, response::IntoResponse, routing::{any, post}, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn multipart_is_bounded_validated_and_anonymous() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let app = Router::new().route("/", post(move |mut mp: Multipart| {
            let root = root.clone();
            async move {
                match receive(&mut mp, &root, 1024).await {
                    Ok(upload) => { assert_eq!(upload.size, 1024); StatusCode::OK.into_response() },
                    Err((code, message)) => (code, message).into_response(),
                }
            }
        }));
        for (name, bytes, expected) in [("客户端.run", 1024, StatusCode::OK), ("x.run", 1025, StatusCode::PAYLOAD_TOO_LARGE),
            ("x.run", 0, StatusCode::BAD_REQUEST), ("../x", 1, StatusCode::BAD_REQUEST)] {
            let mut body = format!("--test\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\r\n").into_bytes();
            body.extend(vec![255; bytes]); body.extend(b"\r\n--test--\r\n");
            let response = app.clone().oneshot(Request::post("/").header("Content-Type", "multipart/form-data; boundary=test").body(Body::from(body)).unwrap()).await.unwrap();
            assert_eq!(response.status(), expected, "{}", String::from_utf8_lossy(&to_bytes(response.into_body(),4096).await.unwrap()));
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        }
        for name in ["", ".", "..", "/etc/x", "a\\b", "x\nrun", "x\"run"] { assert!(!valid_filename(name)); }
    }

    #[test]
    fn install_status_survives_and_stale_work_is_interrupted_without_replay() {
        let dir = tempfile::tempdir().unwrap();
        let mut status = queued("client.deb".into());
        status.exec_id = Some("old-exec".into());
        persist_queued(dir.path(), "channel/unsafe", &status).unwrap();
        assert_eq!(load_status(dir.path(), "channel/unsafe").unwrap().unwrap().phase, "queued");
        let interrupted = public_status(dir.path(), "channel/unsafe").unwrap().unwrap();
        assert_eq!(interrupted.phase, "interrupted");
        assert!(interrupted.log.unwrap().contains("不会自动重装"));
        assert_eq!(load_status(dir.path(), "channel/unsafe").unwrap().unwrap().exec_id.as_deref(), Some("old-exec"));
        assert_eq!(std::fs::read_dir(dir.path().join("installer-status")).unwrap().count(), 1);

        let active_status = queued("other.run".into());
        persist_queued(dir.path(), "active", &active_status).unwrap();
        mark_active(&active_status.attempt_id);
        assert_eq!(public_status(dir.path(), "active").unwrap().unwrap().phase, "queued");
        unmark_active(&active_status.attempt_id);
    }

    #[test]
    fn install_log_is_bounded_and_keeps_the_useful_tail() {
        let mut bytes = vec![b'x'; INSTALL_LOG_BYTES + 100];
        bytes.extend_from_slice(b"final error");
        let log = tail_text(bytes);
        assert!(log.len() <= INSTALL_LOG_BYTES);
        assert!(log.ends_with("final error"));
    }

    #[test]
    fn installer_uses_fixed_binary_and_literal_absolute_package_argument() {
        assert_eq!(install_command("/root/Vendor client.deb".into()), vec![
            "/usr/local/bin/tongdao-install", "/root/Vendor client.deb"
        ]);
        assert_eq!(phase_for_exit(0), "succeeded");
        assert_eq!(phase_for_exit(20), "manual");
        assert_eq!(phase_for_exit(21), "launch_failed");
        assert_eq!(phase_for_exit(1), "failed");
    }

    #[tokio::test]
    async fn old_exec_must_be_confirmed_stopped_before_package_can_be_overwritten() {
        let docker_api = Router::new().fallback(any(|request: Request<Body>| async move {
            let path = request.uri().path();
            let (status, body) = if path.ends_with("/exec/running/json") {
                (200, r#"{"ID":"running","Running":true}"#)
            } else if path.ends_with("/exec/done/json") {
                (200, r#"{"ID":"done","Running":false,"ExitCode":0}"#)
            } else if path.ends_with("/exec/missing/json") {
                (404, r#"{"message":"not found"}"#)
            } else if path.ends_with("/exec/unknown/json") {
                (200, r#"{"ID":"unknown"}"#)
            } else {
                (500, r#"{"message":"daemon unavailable"}"#)
            };
            Response::builder().status(status).header("content-type", "application/json")
                .body(Body::from(body)).unwrap()
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, docker_api).await.unwrap() });
        let docker = bollard::Docker::connect_with_http(
            &format!("http://{address}"), 5, bollard::API_DEFAULT_VERSION).unwrap();
        let dir = tempfile::tempdir().unwrap();
        for (cid, exec_id) in [("a", "running"), ("b", "done"), ("c", "missing"), ("d", "broken"), ("e", "unknown")] {
            let mut status = queued("client.deb".into());
            status.phase = "interrupted".into();
            status.exec_id = Some(exec_id.into());
            save_status(dir.path(), cid, &status).unwrap();
        }
        assert!(install_in_progress(&docker, dir.path(), "a").await.unwrap());
        assert!(!install_in_progress(&docker, dir.path(), "b").await.unwrap());
        assert!(!install_in_progress(&docker, dir.path(), "c").await.unwrap());
        assert!(install_in_progress(&docker, dir.path(), "d").await.is_err());
        assert!(install_in_progress(&docker, dir.path(), "e").await.is_err());
        server.abort(); let _ = server.await;
    }

    #[tokio::test]
    async fn inspect_failure_after_real_attached_stream_keeps_exec_id_and_unknown_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0u8; 4096];
                    loop {
                        let n = socket.read(&mut chunk).await.unwrap();
                        if n == 0 { return; }
                        request.extend_from_slice(&chunk[..n]);
                        if request.windows(4).any(|part| part == b"\r\n\r\n") { break; }
                    }
                    let head = String::from_utf8_lossy(&request);
                    if head.contains("/containers/owned/exec ") {
                        let body = br#"{"Id":"exec-stream"}"#;
                        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.write_all(body).await.unwrap();
                    } else if head.contains("/exec/exec-stream/start ") {
                        socket.write_all(b"HTTP/1.1 101 UPGRADED\r\nConnection: Upgrade\r\nUpgrade: tcp\r\nContent-Type: application/vnd.docker.raw-stream\r\n\r\n").await.unwrap();
                        let text = b"installer output before inspect\n";
                        let mut frame = vec![1, 0, 0, 0, 0, 0, 0, text.len() as u8];
                        frame.extend_from_slice(text);
                        socket.write_all(&frame).await.unwrap();
                    } else if head.contains("/exec/exec-stream/json ") {
                        let body = br#"{"message":"inspect unavailable"}"#;
                        let response = format!("HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.write_all(body).await.unwrap();
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("vpnmgr.db");
        crate::store::init(&db).unwrap();
        rusqlite::Connection::open(&db).unwrap().execute(
            "INSERT INTO channels(id,name,vpn_type,status,container_id) VALUES('stream','fixture','custom','running','owned')", []).unwrap();
        let status = queued("client.deb".into());
        persist_queued(dir.path(), "stream", &status).unwrap();
        mark_active(&status.attempt_id);
        let docker = bollard::Docker::connect_with_http(
            &format!("http://{address}"), 5, bollard::API_DEFAULT_VERSION).unwrap();
        run_install(docker, InstallRequest {
            container: "owned".into(), package_path: "/root/client.deb".into(),
            directory: dir.path().to_path_buf(), db_path: db, cid: "stream".into(),
            expected_container_id: Some("owned".into()),
        }, status).await;
        let result = load_status(dir.path(), "stream").unwrap().unwrap();
        assert_eq!(result.phase, "interrupted", "{result:?}");
        assert_eq!(result.exec_id.as_deref(), Some("exec-stream"));
        assert!(result.log.unwrap().contains("installer output before inspect"));
        server.abort(); let _ = server.await;
    }
}
