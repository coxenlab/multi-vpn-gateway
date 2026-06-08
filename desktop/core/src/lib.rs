pub mod config;
pub mod store;
pub mod docker;
pub mod vm;
pub mod manager;
pub mod mihomo;
pub mod server;
pub mod app;
pub mod routes;
pub mod registry;
pub mod adapters;
pub mod webutil;
pub mod api;
pub mod dockerhub;
pub mod preflight;

use std::sync::Arc;

/// 全局共享状态。bollard::Docker 与 reqwest::Client 内部是 Arc,clone 廉价。
#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<config::Config>,
    pub docker: Option<bollard::Docker>,
    pub mihomo: mihomo::Controller,
}
