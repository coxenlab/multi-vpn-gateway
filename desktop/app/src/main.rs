#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! vpnmgr 桌面壳(Tauri v2)。
//!
//! 启动:先建主窗显内置 loading 页 → 后台起自带 colima VM(专属 `vpnmgr` profile,不碰用户
//! `default`)→ 连 VM 内 Docker、进程内起 axum → 把主窗导航到 `http://127.0.0.1:UI/` 的真实 6 屏 UI。
//! 命门 #4:axum 与 webview 全程只碰 127.0.0.1。关窗 = 隐藏到托盘(后台 core/VM 续跑),退出走托盘菜单。

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use vpnmgr_core::{app, config::Config, infra, manager, vm};

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

    let data_dir = Config::load().data_dir;
    infra::ensure_params(&data_dir)?;
    Ok(())
}

/// 后台启动序列:起自带 VM → 连 docker → 建 bridge + mihomo#1 分流 → 起 axum → 导航真 UI。
async fn boot(handle: &tauri::AppHandle) -> Result<(), BootFailure> {
    let reporter = BootReporter::new(handle);

    let mut runtime_log = StepLog::default();
    reporter.update(
        BootStep::Runtime,
        BootStatus::Active,
        "检查随应用提供的运行组件…",
    );
    prepare_runtime(handle).map_err(|e| BootFailure::new(BootStep::Runtime, e, &runtime_log))?;
    runtime_log.push("运行组件与基础参数已就绪");
    reporter.update(BootStep::Runtime, BootStatus::Done, "运行组件已就绪");

    let mut vm_log = StepLog::default();
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

    let mut docker_log = StepLog::default();
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
    let docker = state
        .docker()
        .ok_or_else(|| BootFailure::new(BootStep::Docker, "容器引擎连接未建立", &docker_log))?;
    reporter.update(BootStep::Docker, BootStatus::Done, "容器引擎已就绪");

    let mut bundled_log = StepLog::default();
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

    let mut image_log = StepLog::default();
    let mut image_last_ui = None;
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

    let mut service_log = StepLog::default();
    reporter.update(
        BootStep::Service,
        BootStatus::Active,
        "启动分流服务并载入规则…",
    );
    infra::ensure_mihomo(&docker, &state.cfg)
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

    let port = listener
        .local_addr()
        .map_err(|e| BootFailure::new(BootStep::Service, e, &service_log))?
        .port();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = app::serve(listener, state).await {
            eprintln!("axum serve exited: {e}");
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
            eprintln!("启动失败: {}", failure.message);
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
