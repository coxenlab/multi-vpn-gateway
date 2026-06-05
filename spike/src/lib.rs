use anyhow::{anyhow, Result};
use bollard::Docker;
use bollard::container::{Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions};
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bollard::image::CreateImageOptions;
use futures_util::StreamExt;
use tokio::io::AsyncWriteExt;

/// 解析 VM 的 docker.sock 路径:优先 DOCKER_HOST(去掉 unix:// 前缀),
/// 否则回退 colima 默认 profile 的 sock。
pub fn spike_socket() -> String {
    if let Ok(h) = std::env::var("DOCKER_HOST") {
        return h.trim_start_matches("unix://").to_string();
    }
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.colima/default/docker.sock")
}

/// 连接 VM 内 Docker Engine。等价于今天的 docker.from_env(),只是 socket 路径不同。
pub async fn connect() -> Result<Docker> {
    let sock = spike_socket();
    let docker = Docker::connect_with_socket(&sock, 120, bollard::API_DEFAULT_VERSION)
        .map_err(|e| anyhow!("connect {sock}: {e}"))?;
    docker.ping().await.map_err(|e| anyhow!("ping {sock}: {e}"))?;
    Ok(docker)
}

/// 确保镜像在 VM 内存在(集成测试用 alpine 这种小镜像)。
pub async fn ensure_image(docker: &Docker, image: &str) -> Result<()> {
    let opts = CreateImageOptions { from_image: image, ..Default::default() };
    let mut stream = docker.create_image(Some(opts), None, None);
    while let Some(item) = stream.next().await {
        item.map_err(|e| anyhow!("pull {image}: {e}"))?;
    }
    Ok(())
}

/// 用与 Python cryptography.Fernet 相同的 key 解密其密文。
/// 用 decrypt()(不带 TTL),保证旧 token 不被时间拒绝。
pub fn decrypt_fernet(key: &str, token: &str) -> Result<Vec<u8>> {
    let f = fernet::Fernet::new(key).ok_or_else(|| anyhow!("invalid fernet key"))?;
    f.decrypt(token).map_err(|e| anyhow!("fernet decrypt failed: {e:?}"))
}

/// 占位:证明工程能编译能测。后续任务会替换/扩充本文件。
pub fn spike_ready() -> bool {
    true
}

/// 起一个常驻容器(detached,auto_remove=false 便于多次 exec)。
pub async fn run_detached(docker: &Docker, name: &str, image: &str, cmd: Vec<&str>) -> Result<()> {
    let cfg = Config {
        image: Some(image.to_string()),
        cmd: Some(cmd.into_iter().map(String::from).collect()),
        ..Default::default()
    };
    docker
        .create_container(Some(CreateContainerOptions { name, platform: None }), cfg)
        .await
        .map_err(|e| anyhow!("create {name}: {e}"))?;
    docker
        .start_container(name, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow!("start {name}: {e}"))?;
    Ok(())
}

pub async fn rm_force(docker: &Docker, name: &str) -> Result<()> {
    docker
        .remove_container(name, Some(RemoveContainerOptions { force: true, ..Default::default() }))
        .await
        .map_err(|e| anyhow!("rm {name}: {e}"))?;
    Ok(())
}

/// 跑一次 exec 并收集 stdout/stderr 文本。
pub async fn exec_capture(docker: &Docker, name: &str, cmd: Vec<&str>) -> Result<String> {
    let exec = docker
        .create_exec(name, CreateExecOptions {
            cmd: Some(cmd.into_iter().map(String::from).collect()),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        })
        .await?;
    let mut out = String::new();
    if let StartExecResults::Attached { mut output, .. } =
        docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, ..Default::default() })).await?
    {
        while let Some(item) = output.next().await {
            out.push_str(&String::from_utf8_lossy(item?.into_bytes().as_ref()));
        }
    }
    Ok(out)
}

/// 核心:exec 时 attach_stdin,把 data 写进容器 stdin 并 shutdown 发 EOF。
/// 对应今天 Python 的 exec_run(stdin=True, socket=True)+sendall+shutdown(SHUT_WR)。
pub async fn exec_inject_stdin(docker: &Docker, name: &str, cmd: Vec<&str>, data: &[u8]) -> Result<String> {
    let exec = docker
        .create_exec(name, CreateExecOptions {
            cmd: Some(cmd.into_iter().map(String::from).collect()),
            attach_stdin: Some(true),
            attach_stdout: Some(true),
            attach_stderr: Some(true),
            ..Default::default()
        })
        .await?;
    let mut out = String::new();
    match docker.start_exec(&exec.id, Some(StartExecOptions { detach: false, ..Default::default() })).await? {
        StartExecResults::Attached { mut output, mut input } => {
            input.write_all(data).await.map_err(|e| anyhow!("write stdin: {e}"))?;
            input.shutdown().await.map_err(|e| anyhow!("shutdown stdin (EOF): {e}"))?; // EOF
            while let Some(item) = output.next().await {
                out.push_str(&String::from_utf8_lossy(item?.into_bytes().as_ref()));
            }
        }
        StartExecResults::Detached => return Err(anyhow!("exec detached unexpectedly")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoke_builds_and_runs() {
        assert!(spike_ready());
    }
}
