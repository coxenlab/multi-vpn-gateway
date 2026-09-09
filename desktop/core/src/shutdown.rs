//! 退出清理:按层级把运行面收干净——关 TUN → 关系统代理 → 停全部通道容器 → 停 VM。
//!
//! 语义与「关容器即关分流」(store::effective_rules)同一条层级公理:上层关闭,
//! 下层不得残留半死状态。app 退出后分流口 SSH 转发(kill_on_drop)必死,若不清理:
//! TUN 路由把命中网段变黑洞、外层 Clash 的 vpn-router 节点悬空、容器空耗资源。
//! 全程 best-effort:任何一步失败只记事件、不阻塞退出(用户要退出,不能锁死在清理上)。

use std::time::Duration;

use futures_util::future::join_all;

use crate::{config::Config, entry, manager, store, vm, AppState};

/// 单个容器 stop 的等待上限(docker stop 本身有 SIGTERM→SIGKILL 宽限)。
const CONTAINER_STOP_TIMEOUT: Duration = Duration::from_secs(20);
/// VM 停止等待上限。
const VM_STOP_TIMEOUT: Duration = Duration::from_secs(40);

/// 停全部运行中的通道容器(并发),状态落库 stopped——DB 与实际一致,
/// 下次启动通道按「已停止」呈现,由用户按需逐个启动(hagb/oss 启动 = 重建)。
async fn stop_all_channels(state: &AppState, cfg: &Config) {
    let db = cfg.db_path();
    let channels = match store::list_channels(&db) {
        Ok(list) => list,
        Err(e) => {
            crate::ev!(warn, "shutdown", "list_channels_failed", "退出清理:读取通道列表失败",
                { "error": e.to_string() });
            return;
        }
    };
    let Some(docker) = state.docker() else {
        crate::ev!(warn, "shutdown", "docker_unavailable", "退出清理:docker 不可达,跳过停容器",
            { "skipped": channels.len() as i64 });
        return;
    };
    let running: Vec<_> = channels.into_iter().filter(|c| c.status != "stopped").collect();
    if running.is_empty() {
        return;
    }
    let tasks = running.iter().map(|ch| {
        let docker = docker.clone();
        let db = db.clone();
        let cid = ch.id.clone();
        async move {
            match tokio::time::timeout(CONTAINER_STOP_TIMEOUT, manager::stop(&docker, &cid)).await {
                Ok(Ok(())) => {
                    let _ = store::set_status(&db, &cid, "stopped");
                    true
                }
                Ok(Err(e)) => {
                    crate::ev!(warn, "shutdown", "container_stop_failed", "退出清理:容器停止失败",
                        { "cid": cid.as_str(), "error": e.to_string() });
                    false
                }
                Err(_) => {
                    crate::ev!(warn, "shutdown", "container_stop_timeout", "退出清理:容器停止超时",
                        { "cid": cid.as_str() });
                    false
                }
            }
        }
    });
    let stopped = join_all(tasks).await.into_iter().filter(|ok| *ok).count();
    crate::ev!(info, "shutdown", "containers_stopped", "退出清理:通道容器已停止",
        { "stopped": stopped as i64, "total": running.len() as i64 });
}

/// 关闭本工具启用的系统自动代理。遍历所有网络服务(不只当前默认那一个——用户换过网
/// 时旧服务上也可能残留指向本工具 PAC 的配置),只动确实指向本工具 PAC 的服务。
async fn park_system_proxy(cfg: &Config) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let parked = entry::system_proxy_park_all(&cfg.ui_port.to_string()).await;
    if parked > 0 {
        crate::ev!(info, "shutdown", "system_proxy_parked",
            "退出清理:系统自动代理已关闭(退出后 PAC 不可达)", { "services": parked as i64 });
    }
}

/// 常规退出(托盘/菜单 Quit → ExitRequested 拦截)的总预算。
pub const NORMAL_BUDGET: Duration = Duration::from_secs(90);
/// terminate: 兜底路径(注销/关机等绕过 ExitRequested 的退出)的收紧预算——
/// 系统给 willTerminate 的时间有限,超了会被直接 SIGKILL。
pub const FALLBACK_BUDGET: Duration = Duration::from_secs(25);

/// 层级清理总入口(供壳的退出拦截调用)。`state` 为 None 表示 boot 未完成
/// (还没建出 AppState)。`stop_vm=false` 用于 boot 仍在进行时的退出——
/// 此刻 colima start 可能还在跑,再并发 colima stop 会把 lima 搞进半创建态
/// (红队 M7),宁可让 VM 留着由下次启动接管。带总预算兜底,超时放行退出。
pub async fn shutdown_all(state: Option<&AppState>, cfg: &Config, stop_vm: bool, budget: Duration) {
    if tokio::time::timeout(budget, run(state, cfg, stop_vm)).await.is_err() {
        crate::ev!(warn, "shutdown", "shutdown_timeout", "退出清理总超时,放行退出", { "ok": false });
    }
}

async fn run(state: Option<&AppState>, cfg: &Config, stop_vm: bool) {
    crate::ev!(info, "shutdown", "shutdown_begin", "退出清理开始",
        { "boot_ready": state.is_some(), "stop_vm": stop_vm });
    // ① 入口层:TUN 路由回收(保留启用标记)+ 系统代理还原——先掐入口,避免
    //    清理期间还有新流量进到即将拆掉的链路上。
    entry::tun_park(cfg).await;
    park_system_proxy(cfg).await;
    // ② 容器层:停全部通道容器,状态落库(关容器即关分流:effective_rules 折叠)。
    if let Some(st) = state {
        stop_all_channels(st, cfg).await;
    }
    // ③ 平台层:停 VM(mihomo#1 与 docker daemon 随之关闭)。boot 进行中则跳过(M7)。
    if !stop_vm {
        crate::ev!(warn, "shutdown", "vm_stop_skipped", "退出清理:boot 进行中,跳过停 VM(避免与 colima start 并发)", { "ok": true });
        crate::ev!(info, "shutdown", "shutdown_done", "退出清理完成", { "ok": true });
        return;
    }
    match tokio::time::timeout(VM_STOP_TIMEOUT, vm::stop(vm::PROFILE)).await {
        Ok(Ok(())) => {
            crate::ev!(info, "shutdown", "vm_stopped", "退出清理:VM 已停止", { "ok": true });
        }
        Ok(Err(e)) => {
            crate::ev!(warn, "shutdown", "vm_stop_failed", "退出清理:VM 停止失败",
                { "error": e.to_string() });
        }
        Err(_) => {
            crate::ev!(warn, "shutdown", "vm_stop_timeout", "退出清理:VM 停止超时", { "ok": false });
        }
    }
    crate::ev!(info, "shutdown", "shutdown_done", "退出清理完成", { "ok": true });
}
