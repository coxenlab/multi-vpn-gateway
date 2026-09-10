#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! vpnmgr 桌面壳(Tauri v2)。
//!
//! 启动:先建主窗显内置 loading 页 → 后台起自带 colima VM(专属 `vpnmgr` profile,不碰用户
//! `default`)→ 连 VM 内 Docker、进程内起 axum → 把主窗导航到 `http://127.0.0.1:UI/` 的真实 6 屏 UI。
//! 命门 #4:axum 与 webview 全程只碰 127.0.0.1。关窗 = 隐藏到托盘(后台 core/VM 续跑),退出走托盘菜单。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem, Submenu},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use vpnmgr_core::{app, config::Config, infra, manager, vm};

/// 退出清理用:boot 建出 AppState 后存一份克隆(内部全 Arc,克隆廉价)。
static SHUTDOWN_STATE: OnceLock<vpnmgr_core::AppState> = OnceLock::new();
/// 退出清理已完成:下一次 ExitRequested 直接放行。
static CLEANUP_DONE: AtomicBool = AtomicBool::new(false);
/// 退出清理进行中(防重复触发,如托盘「退出」连点)。
static CLEANUP_STARTED: AtomicBool = AtomicBool::new(false);
/// 清理起始时刻(epoch 秒)。红队 M6:清理任务若 panic,CLEANUP_DONE 永远不落,
/// app 会退不掉——超过逃生阈值后放行退出,别把用户逼去强杀(强杀恰好跳过清理)。
static CLEANUP_STARTED_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// 逃生阈值:NORMAL_BUDGET(90s)+ 余量。
const CLEANUP_ESCAPE_SECS: u64 = 120;

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 清理期间的用户可见反馈(红队 M8:最长 90s 零提示会逼用户强杀):
/// 亮出主窗 + 覆盖一层不可关的关闭提示。best-effort,失败无害。
fn show_shutdown_overlay(handle: &tauri::AppHandle) {
    if let Some(w) = handle.get_webview_window("main") {
        let _ = w.show();
        let _ = w.eval(
            "(() => { if (document.getElementById('vpnmgr-shutdown-overlay')) return; \
             const d = document.createElement('div'); d.id = 'vpnmgr-shutdown-overlay'; \
             d.style.cssText = 'position:fixed;inset:0;z-index:99999;background:rgba(250,249,245,.96);display:flex;flex-direction:column;align-items:center;justify-content:center;gap:12px;font-family:system-ui;color:#3d3929;'; \
             d.innerHTML = '<div style=\"font-size:18px;font-weight:600;\">正在退出…</div>\
               <div style=\"font-size:13px;color:#83827d;\">按层级关闭:TUN 路由 → 系统代理 → VPN 容器 → 虚拟机(最多约 90 秒)</div>'; \
             document.body.appendChild(d); })()",
        );
    }
}

/// 是否仍在 boot(colima start 可能在跑)。红队 M7:此时再并发 colima stop 会把
/// lima 搞进半创建态 → 清理跳过停 VM。
fn boot_in_progress(handle: &tauri::AppHandle) -> bool {
    handle
        .try_state::<BootState>()
        .map(|s| s.running.lock().map(|g| *g).unwrap_or(false))
        .unwrap_or(false)
}

#[derive(Clone, Copy)]
enum BootStep {
    Runtime,
    Vm,
    Docker,
    Bundled,
    Mihomo,
    Service,
}

impl BootStep {
    fn id(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Vm => "vm",
            Self::Docker => "docker",
            Self::Bundled => "bundled",
            Self::Mihomo => "mihomo",
            Self::Service => "service",
        }
    }

    fn has_settings_help(self) -> bool {
        matches!(self, Self::Vm | Self::Docker | Self::Mihomo | Self::Service)
    }
}

fn boot_step_start(step: BootStep) -> Instant {
    vpnmgr_core::ev!(info, "boot", "boot_step_start", format!("启动步骤开始:{}", step.id()), { "step": step.id() });
    Instant::now()
}

fn boot_step_done(step: BootStep, started: Instant, result: &str) {
    vpnmgr_core::ev!(info, "boot", "boot_step_done", format!("启动步骤完成:{}", step.id()), {
        "step": step.id(), "duration_ms": started.elapsed().as_millis() as u64, "result": result
    });
}

#[derive(Clone, Copy)]
enum BootStatus {
    Active,
    Done,
    Warning,
}

impl BootStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Done => "done",
            Self::Warning => "warning",
        }
    }
}

fn js_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000c}' => out.push_str("\\f"),
            c if c <= '\u{001f}' || matches!(c, '\u{2028}' | '\u{2029}') => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn js_array(values: &[String]) -> String {
    format!(
        "[{}]",
        values
            .iter()
            .map(|v| js_quote(v))
            .collect::<Vec<_>>()
            .join(",")
    )
}

#[derive(Clone)]
struct BootReporter {
    handle: tauri::AppHandle,
}

impl BootReporter {
    fn new(handle: &tauri::AppHandle) -> Self {
        Self {
            handle: handle.clone(),
        }
    }

    fn eval(&self, js: &str) {
        if let Some(window) = self.handle.get_webview_window("main") {
            // setup 后 webview 可能早于页面脚本完成加载；短暂排队可避免毫秒级失败信息被静默丢掉。
            let guarded = format!(
                "(()=>{{let n=0;const apply=()=>{{if(window.boot){{{js}}}else if(n++<100){{setTimeout(apply,50);}}}};apply();}})();"
            );
            let _ = window.eval(&guarded);
        }
    }

    fn reset(&self) {
        self.eval("window.boot&&window.boot.reset();");
    }

    fn update(&self, step: BootStep, status: BootStatus, detail: &str) {
        self.eval(&format!(
            "window.boot&&window.boot.update({},{},{});",
            js_quote(step.id()),
            js_quote(status.as_str()),
            js_quote(detail),
        ));
    }

    fn fail(&self, failure: &BootFailure) {
        let actions = if failure.step.has_settings_help() {
            vec!["settings".to_string()]
        } else {
            Vec::new()
        };
        self.eval(&format!(
            "window.boot&&window.boot.fail({},{},{},{});",
            js_quote(failure.step.id()),
            js_quote(&failure.message),
            js_array(&failure.log_tail),
            js_array(&actions),
        ));
    }
}

#[derive(Default)]
struct StepLog {
    lines: VecDeque<String>,
}

impl StepLog {
    fn push(&mut self, line: impl Into<String>) {
        let line = line.into();
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if self.lines.len() == 200 {
            self.lines.pop_front();
        }
        self.lines.push_back(line.to_string());
    }

    fn tail(&self) -> Vec<String> {
        self.lines.iter().cloned().collect()
    }
}

struct BootFailure {
    step: BootStep,
    message: String,
    log_tail: Vec<String>,
}

impl BootFailure {
    fn new(step: BootStep, error: impl std::fmt::Display, log: &StepLog) -> Self {
        Self {
            step,
            message: error.to_string(),
            log_tail: log.tail(),
        }
    }
}

fn forward_progress(
    reporter: &BootReporter,
    step: BootStep,
    log: &mut StepLog,
    last_ui: &mut Option<Instant>,
    detail: String,
) {
    log.push(detail.clone());
    if last_ui
        .map(|last| last.elapsed() >= Duration::from_millis(500))
        .unwrap_or(true)
    {
        reporter.update(step, BootStatus::Active, &detail);
        *last_ui = Some(Instant::now());
    }
}

/// 分发修复:DATA_DIR 未设时,core 的编译期默认(desktop/core/.data)在别人机器上是
/// 不存在且不可写的绝对路径 → 首启「准备运行时」Permission denied (os error 13)。
/// 开发机已有历史库则沿用旧路径(不迁移);否则落到 ~/Library/Application Support/<bundle-id>。
/// 必须在第一次 Config::load()(含 boot 里的 events::init)之前调用。
fn resolve_data_dir(handle: &tauri::AppHandle) {
    if std::env::var("DATA_DIR").ok().filter(|s| !s.is_empty()).is_some() {
        return; // 用户/环境显式指定,尊重
    }
    if vpnmgr_core::config::dev_default_data_dir().join("vpnmgr.db").exists() {
        return; // 开发机既有数据,保持编译期默认
    }
    match handle.path().app_data_dir() {
        Ok(dir) => std::env::set_var("DATA_DIR", &dir),
        // 回落编译期默认 = 在别人机器上正是要修的那个 os error 13,必须留痕
        Err(e) => eprintln!("[boot] app_data_dir 解析失败({e}),回落编译期默认数据目录(打包机外将不可写)"),
    }
}

fn prepare_runtime(handle: &tauri::AppHandle) -> anyhow::Result<()> {
    let mut prefixes = Vec::new();
    if let Ok(resources) = handle.path().resource_dir() {
        let runtime_bin = resources.join("runtime").join("bin");
        if runtime_bin.join("colima").exists() {
            prefixes.push(runtime_bin.to_string_lossy().to_string());
        }
    }
    prefixes.extend([
        "/opt/homebrew/bin".to_string(),
        "/usr/local/bin".to_string(),
    ]);
    let current = std::env::var("PATH").unwrap_or_default();
    for part in current.split(':').filter(|part| !part.is_empty()) {
        if !prefixes.iter().any(|prefix| prefix == part) {
            prefixes.push(part.to_string());
        }
    }
    std::env::set_var("PATH", prefixes.join(":"));

    if let Ok(resources) = handle.path().resource_dir() {
        let static_dir = resources.join("static");
        if static_dir.join("index.html").exists() {
            std::env::set_var("STATIC_DIR", &static_dir);
        }
        let helper_dir = resources.join("runtime").join("helper");
        if helper_dir.join("vpnmgr-helper").exists() {
            std::env::set_var("HELPER_RES_DIR", &helper_dir);
        }
    }

    seed_vm_image_cache(handle);

    let data_dir = Config::load().data_dir;
    infra::ensure_params(&data_dir)?;
    Ok(())
}

/// 内置 VM 磁盘镜像预置:bundle 里带了 colima 的下载缓存文件(key = sha256(下载 URL),
/// 见 stage-vm-image.sh),种到 ~/Library/Caches/colima/caches/ 后,colima start 命中
/// 缓存即跳过 GitHub 下载 —— 修「首启在无法连 GitHub 的网络里 resolve redirect failed」。
/// best-effort:失败只打日志,colima 仍可走原下载路径。
fn seed_vm_image_cache(handle: &tauri::AppHandle) {
    let Ok(resources) = handle.path().resource_dir() else { return };
    let Ok(entries) = std::fs::read_dir(resources.join("vm-image")) else {
        // 红队 F5:漏跑 stage-vm-image.sh 的包若在此静默,首启会退回连 GitHub 下 317MB,
        // 失败现象指向第 02 步、没人能反推到打包漏了一步——必须留痕。
        eprintln!("[boot] bundle 缺 vm-image 资源(打包漏跑 stage-vm-image.sh?),VM 镜像将走在线下载");
        return;
    };
    let Ok(home) = handle.path().home_dir() else { return };
    let cache_dir = home.join("Library/Caches/colima/caches");
    for entry in entries.flatten() {
        let dst = cache_dir.join(entry.file_name());
        if dst.exists() {
            continue; // 已有缓存(或已种过),不重复拷 300MB
        }
        // 红队 M5:直写目标名时,317MB 拷到一半进程被 kill/断电会留下截断文件——
        // colima 校验 sha512 失败后只报错不重下,首启从此硬卡。改为 .part + 原子 rename。
        let tmp = cache_dir.join(format!("{}.part", entry.file_name().to_string_lossy()));
        let result = std::fs::create_dir_all(&cache_dir)
            .map_err(anyhow::Error::from)
            .and_then(|_| std::fs::copy(entry.path(), &tmp).map_err(Into::into))
            .and_then(|_| std::fs::rename(&tmp, &dst).map_err(Into::into));
        if let Err(e) = result {
            eprintln!("[boot] 预置 VM 镜像缓存失败(将回退在线下载): {e}");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

/// 后台启动序列:起自带 VM → 连 docker → 建 bridge + mihomo#1 分流 → 起 axum → 导航真 UI。
async fn boot(handle: &tauri::AppHandle) -> Result<(), BootFailure> {
    vpnmgr_core::events::init(&Config::load().data_dir);
    let reporter = BootReporter::new(handle);

    let mut runtime_log = StepLog::default();
    let runtime_started = boot_step_start(BootStep::Runtime);
    reporter.update(
        BootStep::Runtime,
        BootStatus::Active,
        "检查随应用提供的运行组件…",
    );
    prepare_runtime(handle).map_err(|e| BootFailure::new(BootStep::Runtime, e, &runtime_log))?;
    runtime_log.push("运行组件与基础参数已就绪");
    reporter.update(BootStep::Runtime, BootStatus::Done, "运行组件已就绪");
    boot_step_done(BootStep::Runtime, runtime_started, "done");

    let mut vm_log = StepLog::default();
    let vm_started = boot_step_start(BootStep::Vm);
    reporter.update(BootStep::Vm, BootStatus::Active, "检查虚拟机状态…");
    let mut rosetta_enabled = vm::rosetta_available().await;
    let mut rosetta_skipped = false;
    if vm::host_needs_rosetta() && !rosetta_enabled {
        reporter.update(
            BootStep::Vm,
            BootStatus::Active,
            "缺少 Rosetta 2，等待你的选择…",
        );
        let accepted = vm::prompt_rosetta_install()
            .await
            .map_err(|e| BootFailure::new(BootStep::Vm, e, &vm_log))?;
        if accepted {
            reporter.update(
                BootStep::Vm,
                BootStatus::Active,
                "正在安装 Rosetta 2，请在系统窗口中授权…",
            );
            rosetta_enabled = vm::install_rosetta()
                .await
                .map_err(|e| BootFailure::new(BootStep::Vm, e, &vm_log))?;
            rosetta_skipped = !rosetta_enabled;
        } else {
            rosetta_skipped = true;
        }
        if rosetta_skipped {
            vm_log.push("用户已跳过 Rosetta 2；启动 VM 时不传 --vz-rosetta");
            vpnmgr_core::ev!(warn, "boot", "rosetta_skipped", "用户已跳过 Rosetta 2", { "skipped": true });
        }
    }

    if vm::status(vm::PROFILE).await == vm::VmStatus::Running {
        vm_log.push("虚拟机已在运行");
    } else {
        reporter.update(
            BootStep::Vm,
            BootStatus::Active,
            "首次初始化会下载 Linux 虚拟机镜像…",
        );
        let mut last_ui = None;
        vm::start_with_progress(vm::PROFILE, rosetta_enabled, |detail| {
            forward_progress(&reporter, BootStep::Vm, &mut vm_log, &mut last_ui, detail);
        })
        .await
        .map_err(|e| BootFailure::new(BootStep::Vm, e, &vm_log))?;
    }
    if rosetta_skipped {
        reporter.update(
            BootStep::Vm,
            BootStatus::Warning,
            "虚拟机已就绪；已跳过 Rosetta 2，x86 镜像暂不可用",
        );
    } else {
        reporter.update(BootStep::Vm, BootStatus::Done, "虚拟机已就绪");
    }
    boot_step_done(BootStep::Vm, vm_started, if rosetta_skipped { "warning" } else { "done" });

    let mut docker_log = StepLog::default();
    let docker_started = boot_step_start(BootStep::Docker);
    reporter.update(BootStep::Docker, BootStatus::Active, "等待容器引擎响应…");
    if let Err(first_error) = vm::wait_docker_ready(vm::PROFILE, 40).await {
        docker_log.push(format!("首次等待失败: {first_error}"));
        reporter.update(
            BootStep::Docker,
            BootStatus::Active,
            "底座连接异常，正在自动重启虚拟机修复…",
        );
        vm::stop(vm::PROFILE)
            .await
            .map_err(|e| BootFailure::new(BootStep::Docker, e, &docker_log))?;
        let mut last_ui = None;
        vm::start_with_progress(vm::PROFILE, rosetta_enabled, |detail| {
            forward_progress(
                &reporter,
                BootStep::Docker,
                &mut docker_log,
                &mut last_ui,
                detail,
            );
        })
        .await
        .map_err(|e| BootFailure::new(BootStep::Docker, e, &docker_log))?;
        vm::wait_docker_ready(vm::PROFILE, 180)
            .await
            .map_err(|e| BootFailure::new(BootStep::Docker, e, &docker_log))?;
    }

    let cfg = Config::load();
    let (listener, state) = app::bootstrap(cfg)
        .await
        .map_err(|e| BootFailure::new(BootStep::Docker, e, &docker_log))?;
    let _ = SHUTDOWN_STATE.set(state.clone());   // 供退出清理停容器用
    let docker = state
        .docker()
        .ok_or_else(|| BootFailure::new(BootStep::Docker, "容器引擎连接未建立", &docker_log))?;
    // 网络与基础防护先于任何镜像加载/下载,已有容器的恢复不依赖镜像源可达。
    vpnmgr_core::docker::create_bridge_network(&docker, &state.cfg.vpn_net)
        .await
        .map_err(|e| BootFailure::new(BootStep::Docker, e, &docker_log))?;
    vpnmgr_core::health::ensure_egress_guard(state.clone(), true).await;
    reporter.update(BootStep::Docker, BootStatus::Done, "容器引擎已就绪");
    boot_step_done(BootStep::Docker, docker_started, "done");

    let mut bundled_log = StepLog::default();
    let bundled_started = boot_step_start(BootStep::Bundled);
    reporter.update(BootStep::Bundled, BootStatus::Active, "检查内置 VPN 镜像…");
    // 坏 tarball 重试修不好,标黄放行:oss 镜像只在建 oss 通道/探活时才用得上,
    // 缺了可去 Docker 诊断屏拉取/构建,不能把管理 UI 挡在 loading 页外。
    let mut bundled_warning = None;
    if let Ok(resources) = handle.path().resource_dir() {
        let images = resources.join("images");
        if images.exists() {
            bundled_log.push(format!("载入目录 {}", images.display()));
            if let Err(e) = infra::ensure_bundled_images(&docker, &images).await {
                bundled_log.push(format!("载入失败: {e}"));
                bundled_warning = Some(format!("内置镜像载入失败,可稍后在 Docker 诊断屏拉取: {e}"));
            }
        } else {
            bundled_log.push("开发模式未提供 bundled images，跳过");
        }
    }
    match &bundled_warning {
        Some(msg) => reporter.update(BootStep::Bundled, BootStatus::Warning, msg),
        None => reporter.update(BootStep::Bundled, BootStatus::Done, "内置 VPN 镜像已就绪"),
    }
    boot_step_done(BootStep::Bundled, bundled_started, if bundled_warning.is_some() { "warning" } else { "done" });

    let mut image_log = StepLog::default();
    let mut image_last_ui = None;
    let mihomo_started = boot_step_start(BootStep::Mihomo);
    reporter.update(BootStep::Mihomo, BootStatus::Active, "检查分流内核镜像…");
    infra::ensure_mihomo_image_with_progress(&docker, &state.cfg, |detail| {
        forward_progress(
            &reporter,
            BootStep::Mihomo,
            &mut image_log,
            &mut image_last_ui,
            detail,
        );
    })
    .await
    .map_err(|e| BootFailure::new(BootStep::Mihomo, e, &image_log))?;
    reporter.update(BootStep::Mihomo, BootStatus::Done, "分流内核已就绪");
    boot_step_done(BootStep::Mihomo, mihomo_started, "done");

    let mut service_log = StepLog::default();
    let service_started = boot_step_start(BootStep::Service);
    reporter.update(
        BootStep::Service,
        BootStatus::Active,
        "启动分流服务并载入规则…",
    );
    infra::ensure_mihomo(&docker, &state.cfg)
        .await
        .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?;
    // 宿主分流口/控制口由 app 自持的 SSH 转发伺服(不经 lima,见 vpnmgr_core::tunnel)。
    // 必须早于 rebuild:后者要打控制口下发规则。失败不锁死启动——看门狗会持续重试,
    // 横幅如实报「分流链路中断」,总比把用户卡在 loading 页强。
    if let Err(e) = vpnmgr_core::tunnel::ensure(&state).await {
        service_log.push(format!("分流口转发未就绪: {e}"));
        eprintln!("[boot] 分流口 SSH 转发未就绪(看门狗将重试): {e}");
    }
    // 控制 API 就绪等待放在转发之后(素机首启顺序 bug:宿主 ctrl 口靠上面的转发才通)。
    infra::wait_mihomo_ctrl(&state.cfg)
        .await
        .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?;
    let rebuild_status = manager::rebuild(&state.cfg, Some(&docker), &state.cfg.db_path()).await;
    service_log.push(format!("规则载入结果: {rebuild_status}"));
    // rebuild 失败多为配置/数据问题(如 config parse error),重试修不好;删坏通道、
    // 看门狗横幅修复都在 UI 里,标黄放行而非把用户锁在 loading 页(原 best-effort 语义)。
    let rules_ok = rebuild_status
        .parse::<u16>()
        .ok()
        .is_some_and(|status| (200..300).contains(&status));
    if rules_ok {
        vpnmgr_core::ev!(info, "boot", "rules_reload", "启动规则载入完成", { "status": rebuild_status.as_str() });
    } else {
        vpnmgr_core::ev!(warn, "boot", "rules_reload", "启动规则载入未完成", { "status": "failed", "error": rebuild_status.as_str() });
    }

    let port = listener
        .local_addr()
        .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?
        .port();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = app::serve(listener, state).await {
            vpnmgr_core::ev!(error, "boot", "service_exited", "本地服务异常退出", { "error": e.to_string() });
        }
    });
    if rules_ok {
        reporter.update(
            BootStep::Service,
            BootStatus::Done,
            "服务已启动，正在打开管理界面…",
        );
    } else {
        reporter.update(
            BootStep::Service,
            BootStatus::Warning,
            &format!("服务已启动,但规则载入未完成({rebuild_status}),可在管理界面修复"),
        );
    }
    let url: tauri::Url = format!("http://127.0.0.1:{port}/")
        .parse()
        .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?;
    if let Some(window) = handle.get_webview_window("main") {
        window
            .navigate(url)
            .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?;
    }
    boot_step_done(BootStep::Service, service_started, if rules_ok { "done" } else { "warning" });
    Ok(())
}

#[derive(Clone, Default)]
struct BootState {
    running: Arc<Mutex<bool>>,
}

fn start_boot(handle: tauri::AppHandle, running: Arc<Mutex<bool>>) -> Result<(), String> {
    {
        let mut guard = running.lock().map_err(|_| "启动状态锁已损坏".to_string())?;
        if *guard {
            return Err("启动流程正在运行".to_string());
        }
        *guard = true;
    }

    let reporter = BootReporter::new(&handle);
    reporter.reset();
    tauri::async_runtime::spawn(async move {
        if let Err(failure) = boot(&handle).await {
            let log_tail = failure.log_tail.iter().rev().take(5).rev().cloned().collect::<Vec<_>>().join(" | ");
            vpnmgr_core::ev!(error, "boot", "boot_step_failed", format!("启动步骤失败:{}", failure.step.id()), {
                "step": failure.step.id(), "error": failure.message.as_str(), "log_tail": log_tail
            });
            BootReporter::new(&handle).fail(&failure);
        }
        if let Ok(mut guard) = running.lock() {
            *guard = false;
        }
    });
    Ok(())
}

#[tauri::command]
fn boot_retry(app: tauri::AppHandle, state: tauri::State<'_, BootState>) -> Result<(), String> {
    start_boot(app, state.running.clone())
}

#[tauri::command]
fn boot_open_settings() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("x-apple.systempreferences:com.apple.settings.PrivacySecurity")
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("打开系统设置失败: {e}"))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err("此操作仅适用于 macOS".to_string())
    }
}

fn main() {
    tauri::Builder::default()
        .manage(BootState::default())
        .invoke_handler(tauri::generate_handler![boot_retry, boot_open_settings])
        .setup(|app| {
            resolve_data_dir(app.handle());   // 须先于一切 Config::load()

            // 红队 H1:默认 macOS 应用菜单的 Quit 直发 AppKit `terminate:`,tao 没有
            // applicationShouldTerminate 拦截 → 完全绕过 ExitRequested 与层级清理。
            // 换成自定义菜单:Quit(⌘Q)走 app.exit(0) → ExitRequested → 清理。
            // Edit 子菜单必须保留,否则 webview 里 ⌘C/⌘V 失效。
            let app_sub = Submenu::with_items(app, "VPN 管理网关", true, &[
                &PredefinedMenuItem::hide(app, None)?,
                &PredefinedMenuItem::hide_others(app, None)?,
                &PredefinedMenuItem::show_all(app, None)?,
                &PredefinedMenuItem::separator(app)?,
                &MenuItem::with_id(app, "menu-quit", "退出 VPN 管理网关", true, Some("CmdOrCtrl+Q"))?,
            ])?;
            let edit_sub = Submenu::with_items(app, "编辑", true, &[
                &PredefinedMenuItem::undo(app, None)?,
                &PredefinedMenuItem::redo(app, None)?,
                &PredefinedMenuItem::separator(app)?,
                &PredefinedMenuItem::cut(app, None)?,
                &PredefinedMenuItem::copy(app, None)?,
                &PredefinedMenuItem::paste(app, None)?,
                &PredefinedMenuItem::select_all(app, None)?,
            ])?;
            app.set_menu(Menu::with_items(app, &[&app_sub, &edit_sub])?)?;
            app.on_menu_event(|app, event| {
                if event.id.as_ref() == "menu-quit" {
                    app.exit(0);
                }
            });

            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("VPN 管理网关")
                .inner_size(1240.0, 820.0)
                .build()?;

            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            let running = app.state::<BootState>().running.clone();
            if let Err(e) = start_boot(app.handle().clone(), running) {
                eprintln!("无法启动引导流程: {e}");
            }
            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        // 退出层级清理:托盘「退出」与自定义菜单 ⌘Q 都经 app.exit(0) 汇到 ExitRequested——
        // 先拦下,按 TUN → 系统代理 → 容器 → VM 收干净再真正退出。
        // RunEvent::Exit 是 terminate: 路径(注销/关机等绕过 ExitRequested)的同步兜底。
        .run(|handle, event| match event {
            tauri::RunEvent::ExitRequested { api, .. } => {
                if CLEANUP_DONE.load(Ordering::SeqCst) {
                    return; // 清理已完成:放行退出
                }
                // M6 逃生阈值:清理任务 panic/卡死时不落 CLEANUP_DONE → 超时放行,
                // 别把用户逼去强杀(强杀恰好跳过全部清理)。
                let started_at = CLEANUP_STARTED_AT.load(Ordering::SeqCst);
                if started_at > 0 && now_epoch_secs().saturating_sub(started_at) > CLEANUP_ESCAPE_SECS {
                    eprintln!("[shutdown] 清理超过逃生阈值仍未完成,放行退出");
                    return;
                }
                api.prevent_exit();
                if CLEANUP_STARTED.swap(true, Ordering::SeqCst) {
                    return; // 清理进行中:忽略重复退出请求
                }
                CLEANUP_STARTED_AT.store(now_epoch_secs(), Ordering::SeqCst);
                show_shutdown_overlay(handle); // M8:清理期给用户可见反馈
                let stop_vm = !boot_in_progress(handle); // M7:boot 中不与 colima start 并发停 VM
                let handle = handle.clone();
                tauri::async_runtime::spawn(async move {
                    let cfg = Config::load();
                    vpnmgr_core::shutdown::shutdown_all(
                        SHUTDOWN_STATE.get(), &cfg, stop_vm,
                        vpnmgr_core::shutdown::NORMAL_BUDGET,
                    ).await;
                    CLEANUP_DONE.store(true, Ordering::SeqCst);
                    handle.exit(0);
                });
            }
            // H1 兜底:AppKit terminate:(注销/关机、任何未被自定义菜单收编的退出手势)
            // 不发 ExitRequested,只在进程将死前走到这里——同步做一轮收紧预算的清理。
            tauri::RunEvent::Exit if !CLEANUP_DONE.swap(true, Ordering::SeqCst) => {
                let stop_vm = !boot_in_progress(handle);
                let cfg = Config::load();
                tauri::async_runtime::block_on(vpnmgr_core::shutdown::shutdown_all(
                    SHUTDOWN_STATE.get(), &cfg, stop_vm,
                    vpnmgr_core::shutdown::FALLBACK_BUDGET,
                ));
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn js_quote_escapes_untrusted_progress_text() {
        assert_eq!(js_quote("a\"\\\n\u{2028}"), "\"a\\\"\\\\\\n\\u2028\"");
        assert_eq!(
            js_array(&["一".into(), "b\nc".into()]),
            "[\"一\",\"b\\nc\"]"
        );
    }

    #[test]
    fn step_log_is_a_200_line_ring() {
        let mut log = StepLog::default();
        for i in 0..205 {
            log.push(format!("line-{i}"));
        }
        let tail = log.tail();
        assert_eq!(tail.len(), 200);
        assert_eq!(tail.first().map(String::as_str), Some("line-5"));
        assert_eq!(tail.last().map(String::as_str), Some("line-204"));
    }
}
