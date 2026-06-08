//! VM 内基础设施:`vpnmgr_vpnnet` bridge + mihomo#1 分流路由(对照旧 docker-compose 的
//! `mihomo` 服务)。嵌入路径下无 compose driver,故由 Rust core 用 bollard 直接 create+start
//! (设计 §5 改造C)。
//!
//! host-VM 模型下没有 `./mihomo:/cfg` 这种宿主共享挂载:Rust core 跑在宿主,mihomo 跑在 VM 容器里。
//! 故配置经 **named volume `vpnmgr_mihomo_cfg` + put_archive** 投递(绕开 colima 挂载读写/路径限制),
//! 不再依赖共享文件。端口/密钥首启随机生成、持久化到 `infra.json`(对照 gen_env.py),经 env 注入 Config。
//!
//! 命门 #4:7899(分流)/9090(控制)只映射到宿主 127.0.0.1 高位端口(与 compose 一致)。
//! 命门 #7:容器名 `mihomo`(docker 内嵌 DNS 别名),oss 容器据此当解析器、rebuild 据此投递配置。

use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use bollard::container::{Config as ContainerConfig, CreateContainerOptions, StartContainerOptions};
use bollard::models::{HostConfig, PortBinding, RestartPolicy, RestartPolicyNameEnum};
use bollard::Docker;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::docker;
use crate::mihomo::Controller;

/// mihomo#1 容器名(docker 内嵌 DNS 别名;命门 #7)。
pub const MIHOMO_CONTAINER: &str = "mihomo";
/// 配置投递目录(容器内,named volume 挂载点)。
pub const MIHOMO_CFG_DIR: &str = "/cfg";
/// 配置文件名。
pub const MIHOMO_CFG_FILE: &str = "config.yaml";
/// reload 用的容器内绝对路径(`PUT /configs` 的 `path`)。
pub const MIHOMO_CFG_PATH: &str = "/cfg/config.yaml";
/// 配置持久卷名。
pub const MIHOMO_VOLUME: &str = "vpnmgr_mihomo_cfg";
/// mihomo 分流底座镜像(对照 docker-compose 的 mihomo 服务)。
pub const MIHOMO_IMAGE: &str = "metacubex/mihomo:latest";

/// 编译期嵌入的 mihomo 基础配置模板(含 `__SECRET__` 占位、DNS/sniffer)。
/// 嵌入而非运行时读盘 → 打包后二进制自带,无外部文件依赖(7d 友好)。
const MIHOMO_TEMPLATE: &str = include_str!("../../../mihomo/config.template.yaml");

/// 首启生成、持久化的基础设施参数(对照 gen_env.py 的 MIHOMO_PORT/CTRL_PORT/SECRET)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfraParams {
    /// 分流端口映射到宿主的高位端口(用户 Clash 接这个)。
    pub mihomo_host_port: u16,
    /// 控制台端口映射到宿主的高位端口(Rust core 经此打 mihomo#1 控制 API)。
    pub mihomo_ctrl_port: u16,
    /// external-controller bearer 密钥。
    pub secret: String,
}

/// 把模板里的 `__SECRET__` 换成实际密钥(对照 start.sh 的 sed 渲染)。
pub fn render_base_config(secret: &str) -> String {
    MIHOMO_TEMPLATE.replace("__SECRET__", secret)
}

/// 16 字节随机 → 32 位 hex(对照 secrets.token_hex(16))。
fn random_secret() -> String {
    let bytes: [u8; 16] = rand::random();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 在 20000-60000 取一个当前空闲、未占用过的高位端口(对照 gen_env.free_high_port)。
fn free_high_port(used: &mut Vec<u16>) -> Result<u16> {
    use rand::Rng;
    let mut rng = rand::thread_rng();
    for _ in 0..500 {
        let p = rng.gen_range(20000..60000);
        if used.contains(&p) {
            continue;
        }
        if std::net::TcpListener::bind(("127.0.0.1", p)).is_ok() {
            used.push(p);
            return Ok(p);
        }
    }
    Err(anyhow!("找不到空闲高位端口"))
}

/// 确保 `infra.json` 存在(首启生成端口/密钥并持久化),把参数经 env 注入,供 [`Config::load`] 读取。
///
/// 对照 `wait_docker_ready` 注入 `DOCKER_HOST` 的模式:env set-if-unset(显式覆盖优先,
/// 否则用持久化值)。同时设 `MIHOMO_CONFIG_PATH` 为宿主侧工作副本(`<data_dir>/config.yaml`),
/// rebuild 读它当 base、写回它,再 put_archive 投递进容器。
pub fn ensure_params(data_dir: &Path) -> Result<InfraParams> {
    std::fs::create_dir_all(data_dir)?;
    let pf = data_dir.join("infra.json");
    let params: InfraParams = if pf.exists() {
        serde_json::from_str(&std::fs::read_to_string(&pf)?)
            .map_err(|e| anyhow!("解析 {}: {e}", pf.display()))?
    } else {
        let mut used = Vec::new();
        let p = InfraParams {
            mihomo_host_port: free_high_port(&mut used)?,
            mihomo_ctrl_port: free_high_port(&mut used)?,
            secret: random_secret(),
        };
        write_0600(&pf, &serde_json::to_string_pretty(&p)?)?;
        p
    };

    set_env_if_unset("MIHOMO_HOST_PORT", &params.mihomo_host_port.to_string());
    set_env_if_unset("MIHOMO_CTRL_PORT", &params.mihomo_ctrl_port.to_string());
    set_env_if_unset("MIHOMO_SECRET", &params.secret);
    set_env_if_unset("MIHOMO_CTRL_URL", &format!("http://127.0.0.1:{}", params.mihomo_ctrl_port));
    set_env_if_unset(
        "MIHOMO_CONFIG_PATH",
        &data_dir.join(MIHOMO_CFG_FILE).to_string_lossy(),
    );
    Ok(params)
}

fn set_env_if_unset(k: &str, v: &str) {
    if std::env::var(k).ok().filter(|s| !s.is_empty()).is_none() {
        std::env::set_var(k, v);
    }
}

fn write_0600(path: &Path, content: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    f.write_all(content.as_bytes())?;
    Ok(())
}

/// mihomo#1 容器是否在运行。
async fn container_running(docker: &Docker, name: &str) -> bool {
    docker
        .inspect_container(name, None)
        .await
        .ok()
        .and_then(|i| i.state)
        .and_then(|s| s.running)
        .unwrap_or(false)
}

/// 组 mihomo#1 的 bollard 容器配置:`-d /cfg`、卷 `/cfg`、7899/9090 → 127.0.0.1 高位、vpn_net、unless-stopped。
fn mihomo_container_config(cfg: &Config, host_port: &str, ctrl_port: &str) -> ContainerConfig<String> {
    let pb = |hp: &str| -> Option<Vec<PortBinding>> {
        Some(vec![PortBinding {
            host_ip: Some("127.0.0.1".to_string()), // 命门 #4
            host_port: Some(hp.to_string()),
        }])
    };
    let mut port_bindings = HashMap::new();
    port_bindings.insert("7899/tcp".to_string(), pb(host_port));
    port_bindings.insert("9090/tcp".to_string(), pb(ctrl_port));

    let mut exposed = HashMap::new();
    exposed.insert("7899/tcp".to_string(), HashMap::new());
    exposed.insert("9090/tcp".to_string(), HashMap::new());

    ContainerConfig {
        image: Some(MIHOMO_IMAGE.to_string()),
        cmd: Some(vec!["-d".to_string(), MIHOMO_CFG_DIR.to_string()]),
        exposed_ports: Some(exposed),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{MIHOMO_VOLUME}:{MIHOMO_CFG_DIR}")]),
            port_bindings: Some(port_bindings),
            network_mode: Some(cfg.vpn_net.clone()),
            restart_policy: Some(RestartPolicy {
                name: Some(RestartPolicyNameEnum::UNLESS_STOPPED),
                maximum_retry_count: None,
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// 确保 bridge + mihomo#1 就绪(幂等):建网络 → 确保镜像 → 已在跑则跳过,否则 create→投递基础配置→start→等控制 API。
///
/// 配置投递:先 create(不 start)使卷挂载就绪 → put_archive 基础配置到 `/cfg/config.yaml` →
/// start。mihomo 启动即读到带 external-controller/secret 的配置,控制 API 立刻在对的端口/密钥上起来。
/// 启动后由调用方紧随 `manager::rebuild` 把 DB 里的通道/规则并入(命门 #2/#3)。
pub async fn ensure_mihomo(docker: &Docker, cfg: &Config) -> Result<()> {
    docker::create_bridge_network(docker, &cfg.vpn_net).await?;

    if docker::image_present(docker, MIHOMO_IMAGE).await != Some(true) {
        docker::ensure_image(docker, MIHOMO_IMAGE).await?;
    }

    if container_running(docker, MIHOMO_CONTAINER).await {
        return Ok(()); // 已在跑,别打扰(保活既有连接;rebuild 仍会刷新规则)
    }
    docker::rm_force(docker, MIHOMO_CONTAINER).await?; // 清理停止态残留

    let host_port = cfg.mihomo_host_port.clone();
    let ctrl_port = cfg
        .mihomo_ctrl_port
        .clone()
        .ok_or_else(|| anyhow!("MIHOMO_CTRL_PORT 未设置(ensure_params 应已注入)"))?;
    if host_port.is_empty() {
        return Err(anyhow!("MIHOMO_HOST_PORT 未设置(ensure_params 应已注入)"));
    }

    // 宿主侧工作副本(= start.sh 渲染 ./mihomo/config.yaml 的等价):缺失才种入基础模板,
    // 不覆盖既有累积配置(含通道)。rebuild 读它当 base、并入通道/规则、写回它(命门 #2)。
    let host_cfg = crate::manager::mihomo_config_path();
    let host_path = std::path::Path::new(&host_cfg);
    if !host_path.exists() {
        if let Some(parent) = host_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(host_path, render_base_config(&cfg.mihomo_secret))
            .map_err(|e| anyhow!("种入 mihomo 基础配置 {host_cfg}: {e}"))?;
    }
    // 投递宿主工作副本(基础或累积)进容器,mihomo 启动即读到完整 DNS/sniffer/控制端口/密钥。
    let delivered = std::fs::read_to_string(host_path)
        .unwrap_or_else(|_| render_base_config(&cfg.mihomo_secret));

    let config = mihomo_container_config(cfg, &host_port, &ctrl_port);
    docker
        .create_container(
            Some(CreateContainerOptions { name: MIHOMO_CONTAINER.to_string(), platform: None }),
            config,
        )
        .await
        .map_err(|e| anyhow!("create {MIHOMO_CONTAINER}: {e}"))?;

    docker::put_file(docker, MIHOMO_CONTAINER, MIHOMO_CFG_DIR, MIHOMO_CFG_FILE, delivered.as_bytes())
        .await
        .map_err(|e| anyhow!("投递 mihomo 基础配置: {e}"))?;

    docker
        .start_container(MIHOMO_CONTAINER, None::<StartContainerOptions<String>>)
        .await
        .map_err(|e| anyhow!("start {MIHOMO_CONTAINER}: {e}"))?;

    // 等控制 API 起来(首启容器冷启动可能秒级)。
    let ctrl = Controller::new(cfg.mihomo_ctrl_url.clone(), cfg.mihomo_secret.clone());
    for _ in 0..30 {
        if ctrl.alive().await {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    Err(anyhow!("mihomo#1 控制 API 未在预期内就绪({})", cfg.mihomo_ctrl_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_replaces_secret_placeholder() {
        let out = render_base_config("deadbeef");
        assert!(out.contains("secret: \"deadbeef\""), "占位被替换");
        assert!(!out.contains("__SECRET__"), "无残留占位");
        assert!(out.contains("listen: 0.0.0.0:53"), "保留 DNS listen(oss 解析器)");
        assert!(out.contains("external-controller: 0.0.0.0:9090"), "保留控制端口");
    }

    #[test]
    fn random_secret_is_32_hex() {
        let s = random_secret();
        assert_eq!(s.len(), 32);
        assert!(s.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn free_ports_are_distinct() {
        let mut used = Vec::new();
        let a = free_high_port(&mut used).unwrap();
        let b = free_high_port(&mut used).unwrap();
        assert_ne!(a, b);
        assert!((20000..60000).contains(&a));
        assert!((20000..60000).contains(&b));
    }

    #[test]
    fn ensure_params_persists_and_reuses() {
        let dir = tempfile::tempdir().unwrap();
        // 清掉可能影响 set_if_unset 的 env
        for k in ["MIHOMO_HOST_PORT", "MIHOMO_CTRL_PORT", "MIHOMO_SECRET", "MIHOMO_CTRL_URL", "MIHOMO_CONFIG_PATH"] {
            std::env::remove_var(k);
        }
        let p1 = ensure_params(dir.path()).unwrap();
        assert!(dir.path().join("infra.json").exists());
        // 再次调用读回同一份(端口/密钥稳定,对照 gen_env 重启不变)
        let p2 = ensure_params(dir.path()).unwrap();
        assert_eq!(p1.mihomo_host_port, p2.mihomo_host_port);
        assert_eq!(p1.mihomo_ctrl_port, p2.mihomo_ctrl_port);
        assert_eq!(p1.secret, p2.secret);
        // env 已注入
        assert_eq!(std::env::var("MIHOMO_SECRET").unwrap(), p1.secret);
        assert_eq!(
            std::env::var("MIHOMO_CTRL_URL").unwrap(),
            format!("http://127.0.0.1:{}", p1.mihomo_ctrl_port)
        );
    }

    /// 真机 e2e(需 colima `vpnmgr` 在跑 + DOCKER_HOST 指向它):建网络 + 起 mihomo#1 + 控制 API 通 + rebuild。
    #[tokio::test]
    #[ignore] // needs colima vpnmgr VM + network pull
    async fn ensure_mihomo_brings_up_real_router() {
        let dir = tempfile::tempdir().unwrap();
        for k in ["MIHOMO_HOST_PORT", "MIHOMO_CTRL_PORT", "MIHOMO_SECRET", "MIHOMO_CTRL_URL", "MIHOMO_CONFIG_PATH"] {
            std::env::remove_var(k);
        }
        std::env::set_var("DATA_DIR", dir.path());
        let params = ensure_params(dir.path()).unwrap();
        let cfg = Config::load();
        crate::store::init(&cfg.db_path()).unwrap();
        let d = docker::connect().await.expect("colima vpnmgr docker");

        // 干净起：清掉任何残留
        let _ = docker::rm_force(&d, MIHOMO_CONTAINER).await;

        ensure_mihomo(&d, &cfg).await.expect("ensure_mihomo");
        assert!(container_running(&d, MIHOMO_CONTAINER).await, "mihomo 在跑");

        // 控制 API 在持久化的高位端口 + 密钥上通
        let ctrl = Controller::new(cfg.mihomo_ctrl_url.clone(), cfg.mihomo_secret.clone());
        assert!(ctrl.alive().await, "控制 API alive @ {}", cfg.mihomo_ctrl_url);

        // 端口只绑 127.0.0.1(命门 #4)
        let info = d.inspect_container(MIHOMO_CONTAINER, None).await.unwrap();
        let ports = info.network_settings.unwrap().ports.unwrap();
        for cport in ["7899/tcp", "9090/tcp"] {
            let b = ports.get(cport).unwrap().as_ref().unwrap();
            assert_eq!(b[0].host_ip.as_deref(), Some("127.0.0.1"), "{cport} 只绑 127.0.0.1");
        }
        assert_eq!(
            ports.get("9090/tcp").unwrap().as_ref().unwrap()[0].host_port.as_deref(),
            Some(params.mihomo_ctrl_port.to_string().as_str())
        );

        // rebuild 能并入(空 DB → 至少 MATCH,DIRECT;mihomo PUT /configs 成功返回 204 No Content)
        let code = crate::manager::rebuild(&cfg, Some(&d), &cfg.db_path()).await;
        assert_eq!(code, "204", "rebuild 经容器内 /cfg/config.yaml 重载成功(mihomo 返回 204)");

        // 关键:rebuild 后控制 API 仍在(base 保留了 external-controller/secret,未被并入 stripped 掉)
        assert!(ctrl.alive().await, "rebuild 后控制 API 仍 alive(base 保留)");
        // 宿主工作副本保留 DNS base(命门:oss 经 mihomo DoH 解析内网域名)
        let host_yaml = std::fs::read_to_string(cfg.db_path().parent().unwrap().join("config.yaml")).unwrap();
        assert!(host_yaml.contains("dns:") && host_yaml.contains("0.0.0.0:53"), "rebuild 后 DNS base 仍在");
        assert!(host_yaml.contains("MATCH,DIRECT"), "并入了规则");

        // 幂等:再 ensure 一次不报错、不重建
        ensure_mihomo(&d, &cfg).await.expect("ensure_mihomo idempotent");

        // 清理
        let _ = docker::rm_force(&d, MIHOMO_CONTAINER).await;
        let _ = d.remove_volume(MIHOMO_VOLUME, None).await;
        std::env::remove_var("DATA_DIR");
    }

    #[test]
    fn container_config_binds_127_only_and_volume() {
        let cfg = Config::from_getter(|_| None);
        let c = mihomo_container_config(&cfg, "21000", "29090");
        let h = c.host_config.as_ref().unwrap();
        assert_eq!(h.binds, Some(vec!["vpnmgr_mihomo_cfg:/cfg".to_string()]));
        assert_eq!(h.network_mode.as_deref(), Some("vpnmgr_vpnnet"));
        let pb = h.port_bindings.as_ref().unwrap();
        for (port, hp) in [("7899/tcp", "21000"), ("9090/tcp", "29090")] {
            let b = &pb.get(port).unwrap().as_ref().unwrap()[0];
            assert_eq!(b.host_ip.as_deref(), Some("127.0.0.1"), "命门 #4");
            assert_eq!(b.host_port.as_deref(), Some(hp));
        }
        assert_eq!(c.cmd, Some(vec!["-d".to_string(), "/cfg".to_string()]));
    }
}
