pub mod config;
pub mod store;
pub mod docker;
pub mod vm;
pub mod infra;
pub mod manager;
pub mod mihomo;
pub mod server;
pub mod app;
pub mod routes;
pub mod registry;
pub mod adapters;
pub mod webutil;
pub mod entry;
pub mod api;
pub mod diag;
pub mod containers;
pub mod dockerhub;
pub mod preflight;
pub mod health;

use std::sync::Arc;

/// 全局共享状态。bollard::Docker 与 reqwest::Client 内部是 Arc,clone 廉价。
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<config::Config>,
    /// docker 连接,可热替换:传输层坏死时看门狗经备援隧道 sock 重建连接换入
    /// (盲区 #3 自愈的最后一环,见 health.rs 模块注释)。读走 [`AppState::docker`]。
    pub docker: Arc<std::sync::RwLock<Option<bollard::Docker>>>,
    pub mihomo: mihomo::Controller,
    /// 分流口健康快照(看门狗写、/api/system 读)。
    pub health: health::SharedHealth,
}

impl AppState {
    /// 当前 docker 连接快照(bollard 内部 Arc,clone 廉价);None = 未连上。
    pub fn docker(&self) -> Option<bollard::Docker> {
        self.docker.read().ok().and_then(|g| g.clone())
    }

    /// 热替换 docker 连接(看门狗自愈 / 原生 sock 复活重连用)。
    pub fn set_docker(&self, d: Option<bollard::Docker>) {
        if let Ok(mut g) = self.docker.write() {
            *g = d;
        }
    }
}
