//! 桌面按需启动入口。UI 服务与 VM 独立，所有显式连接意图共享一轮底座初始化。
use anyhow::{anyhow, Result};
use crate::{docker, infra, manager, vm, AppState};
use crate::runtime_lifecycle::Phase;

pub fn serving(state: &AppState) -> bool {
    !state.cfg.managed_vm || matches!(state.lifecycle.runtime().snapshot().phase, Phase::Ready | Phase::Waiting)
}

/// 必须在获取通道操作锁之前调用；启动恢复会逐通道获取同一把锁。
pub async fn ensure(state: &AppState) -> Result<()> {
    if !state.cfg.managed_vm { return Ok(()); }
    let _activity = state.lifecycle.runtime().activity().await.map_err(anyhow::Error::msg)?;
    if serving(state) && vm::status(&state.cfg.vm_profile).await != vm::VmStatus::Running {
        state.lifecycle.runtime().unavailable("运行环境不可用，正在重新连接");
    }
    let owned = state.clone();
    state.lifecycle.runtime().ensure(move || async move {
        start(&owned).await.map_err(|error| error.to_string())
    }).await.map_err(anyhow::Error::msg)
}

pub async fn start_route(axum::extract::State(state): axum::extract::State<AppState>) -> axum::response::Response {
    use axum::response::IntoResponse;
    match ensure(&state).await {
        Ok(()) => axum::Json(serde_json::json!({"ok":true})).into_response(),
        Err(error) => crate::api::err_detail(axum::http::StatusCode::SERVICE_UNAVAILABLE, &error.to_string()),
    }
}

async fn start(state: &AppState) -> Result<()> {
    let cfg = &state.cfg;
    cfg.validate()?;
    let progress = |detail: &str| state.lifecycle.runtime().progress(detail);
    progress("检查运行环境…");
    let mut rosetta = vm::rosetta_available().await;
    if vm::host_needs_rosetta() && !rosetta {
        progress("缺少 Rosetta 2，等待你的选择…");
        if vm::prompt_rosetta_install().await? {
            progress("正在安装 Rosetta 2，请在系统窗口中授权…");
            rosetta = vm::install_rosetta().await?;
        }
        if !rosetta {
            crate::ev!(warn, "runtime", "rosetta_skipped", "已跳过 Rosetta 2，x86 镜像暂不可用", {});
        }
    }
    if vm::status(&cfg.vm_profile).await != vm::VmStatus::Running {
        progress("正在启动运行环境，首次准备可能需要下载组件…");
        vm::start_with_progress(&cfg.vm_profile, rosetta, |detail| {
            crate::ev!(debug, "runtime", "vm_start_progress", "运行环境启动进度", {"detail":detail});
        }).await?;
    }
    progress("等待容器引擎…");
    // 已运行 VM 的管理链路失败不能据此重启全部通道；保留现场供显式重试/诊断。
    vm::wait_docker_ready(&cfg.vm_profile, 40).await?;
    let connection = docker::connect_at(&cfg.docker_socket().display().to_string()).await?;
    state.set_docker(Some(connection.clone()));
    docker::create_bridge_network(&connection, &cfg.vpn_net).await?;
    if !crate::health::ensure_egress_guard(state.clone(), true).await {
        return Err(anyhow!("基础网络防护尚未就绪，请重试"));
    }
    if let Some(images) = cfg.bundled_images_dir.as_ref().filter(|path| path.is_dir()) {
        progress("检查内置 VPN 镜像…");
        if let Err(error) = infra::ensure_bundled_images(&connection, images).await {
            crate::ev!(warn, "runtime", "bundled_images_failed", "内置镜像载入未完成，可在环境检查中重试", {"error":error.to_string()});
        }
    }
    progress("检查分流组件…");
    infra::ensure_mihomo_image_with_progress(&connection, cfg, |detail| {
        crate::ev!(debug, "runtime", "image_progress", "分流组件准备进度", {"detail":detail});
    }).await?;
    infra::ensure_mihomo(&connection, cfg).await?;
    progress("建立本地分流入口…");
    crate::tunnel::ensure(state).await?;
    infra::wait_mihomo_ctrl(cfg).await?;
    progress("恢复操作进度并同步规则…");
    crate::replacement::recover_all(state).await;
    let outcome = manager::rebuild(cfg, Some(&connection), &cfg.db_path()).await;
    if !outcome.parse::<u16>().ok().is_some_and(|status| (200..300).contains(&status)) {
        crate::ev!(warn, "runtime", "rules_pending", "运行环境已启动，规则仍待同步", {"error":outcome});
    }
    progress("运行环境已就绪");
    crate::ev!(info, "runtime", "runtime_ready", "按需运行环境已就绪", {"profile":cfg.vm_profile});
    Ok(())
}
