//! Installer bytes stay in anonymous temporary files, never in the database or a full memory buffer.
use axum::extract::Multipart;
use axum::http::StatusCode;
use std::{io::{Seek, SeekFrom}, path::Path, sync::{Arc, Mutex}, time::Duration};
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
    pub async fn deliver(self, docker: &bollard::Docker, container: &str, directory: &Path) -> anyhow::Result<()> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::{Body, to_bytes}, http::Request, response::IntoResponse, routing::post, Router};
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
}
