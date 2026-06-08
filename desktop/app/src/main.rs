#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! vpnmgr 桌面壳(Tauri v2)。
//!
//! 启动:先建主窗显内置 loading 页 → 后台起自带 colima VM(专属 `vpnmgr` profile,不碰用户
//! `default`)→ 连 VM 内 Docker、进程内起 axum → 把主窗导航到 `http://127.0.0.1:UI/` 的真实 6 屏 UI。
//! 命门 #4:axum 与 webview 全程只碰 127.0.0.1。关窗 = 隐藏到托盘(后台 core/VM 续跑),退出走托盘菜单。

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use vpnmgr_core::{app, config::Config, infra, manager, vm};

/// 后台启动序列:起自带 VM → 连 docker → 建 bridge + mihomo#1 分流 → 起 axum → 把主窗从 loading 页导航到真 UI。
async fn boot(handle: &tauri::AppHandle) -> anyhow::Result<()> {
    // Finder 双击启动的 app 走 launchd 的 PATH(本机为空 → 默认 /usr/bin:/bin:/usr/sbin:/sbin),
    // 不含 Homebrew bin;而 colima/docker 装在 /opt/homebrew/bin(Apple Silicon)或 /usr/local/bin(Intel)。
    // 不补则 vm.rs 的 Command::new("colima") 双击启动时必然 not-found、boot 卡在 ensure_running。
    // 一次性把两个标准 Homebrew 位置前置进 PATH(后续 sidecar 内置 colima 后可去掉)。
    {
        let path = std::env::var("PATH").unwrap_or_default();
        if !path.split(':').any(|p| p == "/opt/homebrew/bin") {
            std::env::set_var("PATH", format!("/opt/homebrew/bin:/usr/local/bin:{path}"));
        }
    }

    vm::ensure_running(vm::PROFILE).await?;
    vm::wait_docker_ready(vm::PROFILE, 180).await?; // 设 DOCKER_HOST 指向专属 profile

    // 打包后:静态 6 屏 UI 随 bundle 落在 .app 的 Contents/Resources/static;经 env 指过去
    // (须在 Config::load 之前,Config 读 STATIC_DIR)。开发态(cargo run)该路径不存在 → 不设,
    // Config 回落编译期 baked 的 ../../app/static,dev/bundle 同一份代码两态自洽。
    if let Ok(res) = handle.path().resource_dir() {
        let static_dir = res.join("static");
        if static_dir.join("index.html").exists() {
            std::env::set_var("STATIC_DIR", &static_dir);
        }
    }

    // mihomo#1 端口/密钥首启生成并持久化,经 env 注入 Config(对照 gen_env.py;须在 Config::load 之前)。
    let data_dir = Config::load().data_dir; // data_dir 不依赖 MIHOMO_* env
    let _ = infra::ensure_params(&data_dir);

    let cfg = Config::load(); // DOCKER_HOST + MIHOMO_* 已就位 → bootstrap 连专属 VM
    let (listener, state) = app::bootstrap(cfg).await?;

    // 建 vpnmgr_vpnnet bridge + 起 mihomo#1 分流路由(设计 §5 改造C),再并入 DB 里的通道/规则。
    // best-effort:mihomo 起不来不挡管理 UI(env-check/preflight 会如实报状态)。
    if let Some(d) = state.docker.as_ref() {
        if let Err(e) = infra::ensure_mihomo(d, &state.cfg).await {
            eprintln!("mihomo#1 未就绪: {e}");
        } else {
            let _ = manager::rebuild(&state.cfg, Some(d), &state.cfg.db_path()).await;
        }
    }

    let port = listener.local_addr()?.port();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = app::serve(listener, state).await {
            eprintln!("axum serve exited: {e}");
        }
    });
    let url: tauri::Url = format!("http://127.0.0.1:{port}/").parse()?;
    if let Some(win) = handle.get_webview_window("main") {
        win.navigate(url)?; // 同窗:loading 页 → 真 UI(命门 #4:127.0.0.1)
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            // 1) 先建主窗,加载内置 loading 页(立即可见,不被 VM 冷启动阻塞)。
            WebviewWindowBuilder::new(app, "main", WebviewUrl::App("index.html".into()))
                .title("VPN 管理网关")
                .inner_size(1240.0, 820.0)
                .build()?;

            // 2) 系统托盘:显示窗口 / 退出。
            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // 3) 后台起自带 VM + axum,就绪后导航主窗到真 UI;失败则在 loading 页显错。
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                if let Err(e) = boot(&handle).await {
                    eprintln!("启动失败: {e}");
                    if let Some(w) = handle.get_webview_window("main") {
                        let msg = format!("启动失败:{e}");
                        let js = format!(
                            "var h=document.querySelector('h1');if(h)h.textContent='启动未完成';\
                             var s=document.querySelector('.spinner');if(s)s.style.display='none';\
                             var p=document.querySelector('.hint');if(p){{p.style.color='#dc2626';p.textContent={msg:?};}}"
                        );
                        let _ = w.eval(&js);
                    }
                }
            });

            Ok(())
        })
        // 关窗 = 隐藏到托盘,不退进程;退出走托盘菜单「退出」。
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
