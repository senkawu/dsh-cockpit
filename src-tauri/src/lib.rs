//! dsh-cockpit (Tauri 2) — DSH-Cockpit 桌面外壳
//!
//! 架构：外壳与 DSH 内核完全解耦。
//!   - Tauri 安装包只含：外壳二进制 + node/npm/pnpm sidecar 资源。
//!   - @deepseek-ai/dsh 内核安装在系统应用数据目录的隔离 pnpm 环境
//!     （<app_data>/dsh-env），DSH_HOME 隔离在 <app_data>/dsh-home。
//!   - DSH 内核从 npm 源拉取 latest，更新无需重新发布 Tauri 安装包；
//!     外壳自身用 tauri-plugin-updater 独立升级。
//!
//! 融合自 dataelement/dsh-desktop 的优点：
//!   - 用户登录 shell 环境注入（GUI 启动 PATH 精简，dsh/bash 工具需要用户 PATH）
//!   - 更新策略：启动延迟+jitter 检查、6h 定时复查、“跳过此版本”持久化
//!   - 安全模式（--safe-mode）：仅官方核心 bundle，跳过插件/更新，面板可退出
//!   - 主窗口销毁自动恢复（5s 冷却、最多 3 次）
//!   - 导航守卫：窗口只允许加载应用页面与 127.0.0.1，外部链接转系统浏览器
//!   - 插件补丁层写入安全化（备份 + 校验回滚）

mod commands;
mod dsh;
mod npm;
mod process_mgr;
mod settings;
mod tray;

use std::sync::atomic::Ordering;
use std::sync::Mutex;

use dsh::DshManager;
use log::{error, info, warn};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::NewWindowResponse;
use tauri::{Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};
use tauri_plugin_log::{Target, TargetKind};

/// 更新检查策略（参考 dataelement/dsh-desktop 的 update-policy）
const UPDATE_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);
const UPDATE_STARTUP_DELAY_MS: u64 = 10_000;
const UPDATE_STARTUP_JITTER_MS: u64 = 10_000;
/// 主窗口恢复策略（参考 main-window-recovery：有界重载，防抖）
const WINDOW_RECOVERY_COOLDOWN_MS: u64 = 5_000;
const WINDOW_RECOVERY_MAX_RELOADS: usize = 3;

/// 主窗口恢复状态（Destroyed 事件后按冷却/次数重建）
#[derive(Default)]
struct WindowRecovery {
    reload_count: usize,
    last_reload_at: u64,
}

// ---------------------------------------------------------------------------
// 窗口创建（程序化建窗：可挂导航守卫 / 新窗口策略 / 关闭到托盘 / 销毁恢复）
// ---------------------------------------------------------------------------

/// 导航守卫：只允许应用页面（tauri:/data:）与本机 dsh 服务；其余转系统浏览器
fn navigation_allowed(app: &tauri::AppHandle, url: &tauri::Url) -> bool {
    let scheme = url.scheme();
    if scheme == "tauri" || scheme == "data" || scheme == "about" || scheme == "file" {
        return true; // 客户端自带页面
    }
    if matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1")) {
        return true; // 本地 dsh web
    }
    // 外部链接：交给系统浏览器，窗口内阻止
    let target = url.to_string();
    let app = app.clone();
    std::thread::spawn(move || {
        let _ = app; // 保留句柄语义（未来用 opener 插件）
        crate::tray::open_external(&target);
    });
    false
}

/// 屏蔽右键菜单与 Inspect：每次页面加载完成都注入拦截脚本
/// （覆盖状态页/控制面板/以及导航过去的 dsh 页面）。
const BLOCK_CONTEXT_MENU_JS: &str = r#"
  (function(){
    document.addEventListener('contextmenu', function(e){ e.preventDefault(); return false; }, true);
    document.addEventListener('keydown', function(e){
      // 屏蔽 DevTools 快捷键（mac: Cmd+Opt+I/J/C；win: F12 / Ctrl+Shift+I/J/C）
      if ((e.metaKey || e.ctrlKey) && e.shiftKey && ['I','J','C'].includes(String(e.key).toUpperCase())) { e.preventDefault(); return false; }
      if (e.key === 'F12') { e.preventDefault(); return false; }
    }, true);
  })();
"#;

fn create_main_window(app: &tauri::AppHandle) -> Result<WebviewWindow, Box<dyn std::error::Error>> {
    let app_for_nav = app.clone();
    let win = WebviewWindowBuilder::new(app, "main", WebviewUrl::App("status.html".into()))
        .title("DSH-Cockpit")
        .inner_size(1440.0, 900.0)
        .min_inner_size(940.0, 600.0)
        .center()
        .visible(true)
        .devtools(false) // 禁用开发者工具（配合 JS 拦截，彻底屏蔽 Inspect）
        .on_navigation(move |url| navigation_allowed(&app_for_nav, url))
        .on_new_window(move |url, _features| {
            let target = url.to_string();
            std::thread::spawn(move || crate::tray::open_external(&target));
            NewWindowResponse::Deny
        })
        .on_page_load(|win, payload| {
            use tauri::webview::PageLoadEvent;
            if matches!(payload.event(), PageLoadEvent::Finished) {
                let _ = win.eval(BLOCK_CONTEXT_MENU_JS);
            }
        })
        .build()?;

    // 关闭 → 隐藏到托盘（不退出）
    {
        let win2 = win.clone();
        win.on_window_event(move |event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = win2.hide();
            }
        });
    }

    // 销毁（崩溃/异常关闭）→ 按冷却与次数上限自动重建
    {
        let app2 = app.clone();
        win.on_window_event(move |event| {
            if let WindowEvent::Destroyed = event {
                let now = now_millis();
                let recovery_state = app2.state::<Mutex<WindowRecovery>>();
                let mut st = recovery_state.lock().unwrap();
                if st.reload_count >= WINDOW_RECOVERY_MAX_RELOADS {
                    warn!("主窗口恢复次数已达上限，停止自动重建");
                    return;
                }
                if st.last_reload_at != 0 && now - st.last_reload_at < WINDOW_RECOVERY_COOLDOWN_MS {
                    return;
                }
                st.reload_count += 1;
                st.last_reload_at = now;
                drop(st);
                let app3 = app2.clone();
                tauri::async_runtime::spawn(async move {
                    // 延迟一点再重建，避免销毁尚未完成
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    if let Err(e) = create_main_window(&app3) {
                        error!("重建主窗口失败: {e}");
                        return;
                    }
                    // 若 dsh 已就绪，直接导航过去
                    if let Some(m) = app3.try_state::<DshManager>() {
                        let m = m.inner().clone();
                        m.navigate_main(&app3);
                    }
                });
            }
        });
    }
    Ok(win)
}

/// 显示（或按需重建）主窗口：托盘「显示窗口」、Dock 图标点击、单实例二次启动都走这里。
/// 窗口只是被隐藏 → show/focus；已被销毁 → 按恢复策略重建。
pub fn show_or_recreate_main(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
    } else if let Err(e) = create_main_window(app) {
        error!("重建主窗口失败: {e}");
    }
}

fn create_panel_window(app: &tauri::AppHandle) -> Result<WebviewWindow, Box<dyn std::error::Error>> {
    let win = WebviewWindowBuilder::new(app, "panel", WebviewUrl::App("index.html".into()))
        .title("DSH-Cockpit · 控制面板")
        .inner_size(440.0, 620.0)
        .resizable(false)
        .center()
        .visible(false)
        .devtools(false) // 控制面板同样禁用开发者工具
        .on_navigation(|url| {
            // 面板只允许自身页面
            url.scheme() == "tauri"
        })
        .build()?;
    let win2 = win.clone();
    win.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            api.prevent_close();
            let _ = win2.hide();
        }
    });
    Ok(win)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 更新检查（自动 + 定时，弹窗询问）
// ---------------------------------------------------------------------------

/// 伪随机 jitter（避免多实例同时打 registry）
fn jitter_ms() -> u64 {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    n % UPDATE_STARTUP_JITTER_MS
}

/// 检查并提示（manual=true 时忽略“跳过此版本”，且给出可见弹窗反馈）
async fn check_and_prompt(app: &tauri::AppHandle, manager: &DshManager, manual: bool) {
    let latest = match manager.latest_version(app).await {
        Ok(v) => v,
        Err(e) => {
            if manual {
                use tauri_plugin_dialog::DialogExt;
                app.dialog()
                    .message(format!("无法查询 npm 最新版本：{e}\n（请检查网络/registry）"))
                    .title("检查 DSH 更新")
                    .kind(tauri_plugin_dialog::MessageDialogKind::Error)
                    .show_with_result(|_| {});
            }
            warn!("检查 DSH 更新失败: {e}");
            return;
        }
    };
    if !manager.has_update(&latest, manual) {
        info!("DSH 内核已是最新（{latest}）");
        if manual {
            use tauri_plugin_dialog::DialogExt;
            app.dialog()
                .message(format!("DSH 内核已是最新版本（{latest}）"))
                .title("检查 DSH 更新")
                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                .show_with_result(|_| {});
        }
        return;
    }
    info!("检测到 dsh 新版本: {latest}");
    manager.emit_status(app, "update-available", &format!("发现 dsh 新版本 {latest}"), Some(&latest));
    // 三按钮：立即更新 / 跳过此版本 / 稍后（自动与手动检查共用）
    let app2 = app.clone();
    let manager2 = manager.clone();
    let current = manager.installed_version().unwrap_or_default();
    app.dialog()
        .message(format!("检测到 DSH 内核新版本 {latest}（当前 {current}）。"))
        .title("DSH 内核更新")
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            "立即更新".into(),
            "跳过此版本".into(),
            "稍后".into(),
        ))
        .show_with_result(move |result| {
            let want_update = match &result {
                MessageDialogResult::Yes => true,
                MessageDialogResult::Custom(s) => s == "立即更新",
                _ => false,
            };
            if want_update {
                let app3 = app2.clone();
                let manager3 = manager2.clone();
                tauri::async_runtime::spawn(async move {
                    match manager3.update(&app3).await {
                        Ok(v) => {
                            info!("DSH 内核已更新至 {v}");
                            let _ = manager3.start(&app3).await;
                        }
                        Err(e) => {
                            error!("DSH 内核更新失败，已回退: {e}");
                            manager3.emit_status(
                                &app3,
                                "error",
                                &format!("更新失败，已回退到上一个版本：{e}"),
                                None,
                            );
                            let _ = manager3.start(&app3).await;
                        }
                    }
                });
            } else if matches!(&result, MessageDialogResult::Custom(s) if s == "跳过此版本") {
                let _ = manager2.set_skipped_version(Some(latest.clone()));
                info!("已跳过 DSH 版本 {latest}（手动检查仍会提示）");
            } else {
                // 稍后：本次不处理，6h 后定时任务再问
                info!("用户选择稍后更新（{latest}）");
            }
        });
}

/// 后台更新检查循环：启动延迟 + jitter 后首次，之后每 6h 一次
fn spawn_update_checker(app: &tauri::AppHandle, manager: &DshManager) {
    let app = app.clone();
    let manager = manager.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(
            UPDATE_STARTUP_DELAY_MS + jitter_ms(),
        ))
        .await;
        loop {
            if manager.auto_check_update.load(Ordering::Relaxed)
                && !manager.safe_mode.load(Ordering::Relaxed)
            {
                check_and_prompt(&app, &manager, false).await;
            }
            tokio::time::sleep(UPDATE_CHECK_INTERVAL).await;
        }
    });
}

// ---------------------------------------------------------------------------
// 启动引导
// ---------------------------------------------------------------------------

async fn bootstrap(app: tauri::AppHandle) {
    let manager = {
        let state = app.state::<DshManager>();
        state.inner().clone()
    };

    if manager.safe_mode.load(Ordering::Relaxed) {
        info!("安全模式：仅官方核心 bundle，跳过插件安装与更新检查");
        manager.emit_status(
            &app,
            "safe-mode",
            "安全模式：已停用第三方插件（使用隔离 profile），可随时在面板退出",
            None,
        );
    } else {
        // 1) 首次运行：安装 dsh 内核
        if manager.installed_version().is_none() {
            manager.emit_status(&app, "installing", "首次运行，正在安装 dsh 内核（隔离 pnpm 环境）…", None);
            match manager.ensure_installed(&app).await {
                Ok(v) => info!("dsh 内核安装完成: {v}"),
                Err(e) => {
                    manager.emit_status(&app, "error", &format!("dsh 内核安装失败：{e}"), None);
                    return;
                }
            }
        }
        // 2) 安装内置插件（市场 + 鲸鱼挂件），失败不阻塞
        manager.install_plugins(&app).await;
    }

    // 3) 启动 dsh web 服务并加载 UI
    if let Err(e) = manager.start(&app).await {
        manager.emit_status(&app, "error", &format!("dsh 服务启动失败：{e}"), None);
        return;
    }

    // 4) 后台更新检查（尽快可用，再异步检查；尊重开关与安全模式）
    spawn_update_checker(&app, &manager);
}

/// 以带参数的形态重启自身（安全模式开关）
fn relaunch_with_flag(app: &tauri::AppHandle, flag: &str) {
    let exe = std::env::current_exe().ok();
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    if !args.iter().any(|a| a == flag) {
        args.push(flag.to_string());
    }
    if let Some(exe) = exe {
        let _ = std::process::Command::new(exe).args(&args).spawn();
    }
    app.exit(0);
}

/// 原生应用菜单：把借鉴自 dataelement/dsh-desktop 的能力做成可见入口
/// （关于 / 检查更新 / 重启服务 / 安全模式重启 / 打开日志目录 / 控制面板）
fn build_app_menu(app: &tauri::AppHandle) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};

    let about = MenuItem::with_id(app, "about", "关于 DSH-Cockpit", true, None::<&str>)?;
    let check = MenuItem::with_id(app, "check-update", "检查 DSH 更新", true, None::<&str>)?;
    let restart = MenuItem::with_id(app, "restart-dsh", "重启 dsh 服务", true, None::<&str>)?;
    let safe = MenuItem::with_id(app, "safe-restart", "以安全模式重启", true, None::<&str>)?;
    let logs = MenuItem::with_id(app, "open-logs", "打开日志目录", true, None::<&str>)?;
    let panel = MenuItem::with_id(app, "panel", "控制面板", true, None::<&str>)?;
    let quit = PredefinedMenuItem::quit(app, Some("退出"))?;

    let app_menu = Submenu::with_items(app, "DSH-Cockpit", true, &[&about, &quit])?;
    let dsh_menu = Submenu::with_items(app, "DSH", true, &[&check, &restart, &safe, &logs, &panel])?;
    let menu = Menu::with_items(app, &[&app_menu, &dsh_menu])?;
    app.set_menu(menu)?;

    app.on_menu_event(|app, event| match event.id().as_ref() {
        "about" => {
            use tauri_plugin_dialog::DialogExt;
            let m = app.state::<DshManager>().inner();
            let detail = format!(
                "外壳版本：{}\nDSH 内核：{}\n端口：{}\n\n隔离环境：{}\nDSH_HOME：{}\n\n安全模式：{}",
                app.package_info().version,
                m.installed_version().unwrap_or_else(|| "未安装".into()),
                m.port,
                m.env_dir.display(),
                m.active_home().display(),
                if m.safe_mode.load(Ordering::Relaxed) { "开" } else { "关" },
            );
            app.dialog()
                .message(format!(
                    "DSH-Cockpit — DeepSeek Harness 桌面外壳\n外壳与 dsh 内核解耦，dsh 随 npm 更新\n\n{detail}"
                ))
                .title("关于 DSH-Cockpit")
                .kind(tauri_plugin_dialog::MessageDialogKind::Info)
                .show_with_result(|_| {});
        }
        "check-update" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let m = app.state::<DshManager>().inner().clone();
                check_and_prompt(&app, &m, true).await;
            });
        }
        "restart-dsh" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let m = app.state::<DshManager>().inner().clone();
                let _ = m.stop(&app).await;
                if let Err(e) = m.start(&app).await {
                    error!("重启 dsh 失败: {e}");
                }
            });
        }
        "safe-restart" => relaunch_with_flag(app, "--safe-mode"),
        "open-logs" => {
            let m = app.state::<DshManager>().inner();
            crate::tray::open_external(&m.log_dir.to_string_lossy());
        }
        "panel" => tray::show_panel(app),
        _ => {}
    });
    Ok(())
}

fn setup(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let handle = app.handle().clone();

    // 解析 --safe-mode 启动参数
    let safe_mode = std::env::args().any(|a| a == "--safe-mode")
        || std::env::var("DSH_SAFE_MODE").map(|v| v == "1").unwrap_or(false);

    // 初始化隔离环境目录 + 管理对象
    let manager = DshManager::new(&handle)?;
    manager.safe_mode.store(safe_mode, Ordering::Relaxed);
    handle.manage(manager.clone());
    info!(
        "隔离 pnpm 环境: {}（安全模式: {safe_mode}）",
        manager.env_dir.display()
    );
    info!("DSH_HOME: {}", manager.active_home().display());

    // 原生菜单（关于 / 检查更新 / 重启 / 安全模式重启 / 日志 / 控制面板）
    build_app_menu(&handle)?;

    // 托盘（显示窗口 / 控制面板 / 重启 dsh / 完全退出）
    let show_item = MenuItem::with_id(&handle, "show", "显示窗口", true, None::<&str>)?;
    let panel_item = MenuItem::with_id(&handle, "panel", "控制面板", true, None::<&str>)?;
    let restart_item = MenuItem::with_id(&handle, "restart", "重启 dsh 服务", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(&handle, "quit", "完全退出", true, None::<&str>)?;
    let menu = Menu::with_items(&handle, &[&show_item, &panel_item, &restart_item, &quit_item])?;

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(handle.default_window_icon().unwrap().clone())
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => tray::show_main_window(app),
            "panel" => tray::show_panel(app),
            "restart" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let m = app.state::<DshManager>().inner().clone();
                    let _ = m.stop(&app).await;
                    if let Err(e) = m.start(&app).await {
                        error!("重启 dsh 失败: {e}");
                    }
                });
            }
            "quit" => {
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let m = app.state::<DshManager>().inner().clone();
                    let _ = m.stop(&app).await;
                    app.exit(0);
                });
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                tray::show_main_window(tray.app_handle());
            }
        })
        .build(&handle)?;

    // 程序化建窗（导航守卫 / 新窗口拦截 / 关闭到托盘 / 销毁恢复）
    handle.manage(Mutex::new(WindowRecovery::default()));
    create_main_window(&handle)?;
    create_panel_window(&handle)?;

    // 后台引导
    tauri::async_runtime::spawn(bootstrap(handle.clone()));

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::LogDir {
                        file_name: Some("dsh-cockpit".into()),
                    }),
                ])
                .build(),
        )
        // 单实例锁：重复启动时聚焦已有主窗口
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_main_window(app);
        }))
        .setup(setup)
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_last_status,
            commands::check_dsh_update,
            commands::apply_dsh_update,
            commands::restart_dsh,
            commands::stop_dsh,
            commands::quit_app,
            commands::open_panel,
            commands::check_shell_update,
            commands::get_plugins,
            commands::set_plugin_enabled,
            commands::set_skipped_version,
            commands::exit_safe_mode,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application");

    app.run(|handle, event| {
        match event {
            // macOS：点击 Dock 图标重新激活（窗口被隐藏/销毁后点 Dock 应能重新打开）。
            // 注意：Reopen 是 macOS 专属事件，Windows/Linux 的 RunEvent 没有该变体，
            // 必须用属性 cfg 包裹，否则交叉编译失败。
            #[cfg(target_os = "macos")]
            tauri::RunEvent::Reopen { .. } => show_or_recreate_main(handle),
            tauri::RunEvent::Exit => {
                if let Some(m) = handle.try_state::<DshManager>() {
                    let _ = m.inner().kill_managed_now();
                }
            }
            _ => {}
        }
    });
}
