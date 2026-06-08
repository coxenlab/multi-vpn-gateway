#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! vpnmgr 桌面壳(Tauri v2)。
//!
//! 进程内(in-process)起 `vpnmgr-core` 的 axum,WKWebView 加载 `http://127.0.0.1:UI/`——
//! 即现有 6 屏 UI(同源伺服,前端零改)。命门 #4:axum 与 webview 全程只碰 127.0.0.1。
//! 关窗 = 隐藏到托盘(后台 core/容器续跑),退出走托盘菜单。

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use vpnmgr_core::{app, config::Config};

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            let cfg = Config::load();

            // 同步等引导完成:store::init + 连 docker(可选)+ 绑 127.0.0.1:ui_port。
            // 绑定先于建窗 → webview 立即可连,不会 connection-refused。
            let (listener, state) = tauri::async_runtime::block_on(app::bootstrap(cfg))
                .map_err(|e| format!("core bootstrap failed: {e}"))?;
            let port = listener.local_addr()?.port();

            // 后台跑 axum(Tauri 的 tokio runtime 上),直到进程退出。
            tauri::async_runtime::spawn(async move {
                if let Err(e) = app::serve(listener, state).await {
                    eprintln!("axum serve exited: {e}");
                }
            });

            // 运行时建主窗,加载进程内 axum 伺服的现有 6 屏 UI。
            let url: url::Url = format!("http://127.0.0.1:{port}/")
                .parse()
                .expect("loopback url is valid");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url))
                .title("VPN 管理网关")
                .inner_size(1240.0, 820.0)
                .build()?;

            // 系统托盘:显示窗口 / 退出。
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
