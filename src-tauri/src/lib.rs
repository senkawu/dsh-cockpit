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
mod deep_link;
mod host_ext;
mod inject;
mod preset;
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
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};
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

/// 强制 dsh Web UI 使用中文：dsh 按 `navigator.language` 决定界面语言（无则回退英文）。
/// 必须在页面脚本执行前（PageLoadEvent::Started）注入，否则 dsh 客户端已按英文初始化。
const FORCE_ZH_LOCALE_JS: &str = r#"
  (function(){
    try {
      Object.defineProperty(navigator, 'language', { get: () => 'zh-CN' });
      Object.defineProperty(navigator, 'languages', { get: () => ['zh-CN', 'zh', 'en'] });
    } catch (e) {}
  })();
"#;

/// 屏蔽 DevTools 快捷键（**不**拦截右键菜单——Inspect 由 devtools(false) 原生屏蔽，
/// WebView2 右键菜单里的 Inspect 项随之消失；拦截 contextmenu 会把复制/粘贴等右键功能一起干掉）。
const BLOCK_DEVTOOLS_KEYS_JS: &str = r#"
  (function(){
    document.addEventListener('keydown', function(e){
      // mac: Cmd+Opt+I/J/C；win: F12 / Ctrl+Shift+I/J/C
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
        .devtools(false) // 禁用开发者工具（原生屏蔽 Inspect，保留右键复制粘贴）
        .on_navigation(move |url| navigation_allowed(&app_for_nav, url))
        .on_new_window(move |url, _features| {
            let target = url.to_string();
            std::thread::spawn(move || crate::tray::open_external(&target));
            NewWindowResponse::Deny
        })
        .on_page_load(|win, payload| {
            use tauri::webview::PageLoadEvent;
            let safe = win
                .app_handle()
                .state::<DshManager>()
                .safe_mode
                .load(Ordering::Relaxed);
            let ev = payload.event();
            match ev {
                // 页面脚本执行前注入语言，让 dsh UI 使用中文
                PageLoadEvent::Started => {
                    let _ = win.eval(FORCE_ZH_LOCALE_JS);
                }
                PageLoadEvent::Finished => {
                    let _ = win.eval(BLOCK_DEVTOOLS_KEYS_JS);
                }
            }
            // P4 注入清单（版本化；safe-mode 关闭）
            inject::run_injections(&win, &ev, safe);
        })
        .build()?;

    // P4 2.10：窗口大小/位置记忆（保存由插件自动完成，这里恢复主窗口状态；
    // 排除 VISIBLE——可见性由代码控制，避免恢复成隐藏或把面板/日志窗口带出来）
    use tauri_plugin_window_state::{StateFlags, WindowExt};
    let _ = win.restore_state(
        StateFlags::all() - StateFlags::VISIBLE,
    );

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

/// 日志查看器窗口（隐藏创建，按需显示；关闭 → 隐藏，不退出）
fn create_logs_window(app: &tauri::AppHandle) -> Result<WebviewWindow, Box<dyn std::error::Error>> {
    let win = WebviewWindowBuilder::new(app, "logs", WebviewUrl::App("log-view.html".into()))
        .title("DSH-Cockpit · 日志查看器")
        .inner_size(760.0, 560.0)
        .center()
        .visible(false)
        .devtools(false)
        .on_navigation(|url| {
            // 只允许自身页面
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

/// 显示并聚焦日志查看器窗口
pub fn show_logs_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("logs") {
        let _ = win.show();
        let _ = win.set_focus();
    }
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
    // P4 2.5.3-3：自动检查发现更新 → 系统通知提醒（手动检查走弹窗，不重复通知）
    if !manual {
        host_ext::notify_update_available(app, &latest);
    }
    // 三按钮：立即更新 / 跳过此版本 / 稍后（自动与手动检查共用）
    // P4 2.6：命中兼容矩阵 → 标题加 ⚠️ 并在文案里附原因
    let app2 = app.clone();
    let manager2 = manager.clone();
    let current = manager.installed_version().unwrap_or_default();
    let incompat = crate::dsh::incompatible_reason(&latest);
    let title = match &incompat {
        Some(_) => "⚠️ DSH 内核更新（兼容性警告）",
        None => "DSH 内核更新",
    };
    let body = match &incompat {
        Some(reason) => format!(
            "检测到 DSH 内核新版本 {latest}（当前 {current}）。\n\n\
             ⚠️ 此版本可能包含破坏性变更：{reason}\n\
             建议先查看更新说明再决定是否升级。"
        ),
        None => format!("检测到 DSH 内核新版本 {latest}（当前 {current}）。"),
    };
    app.dialog()
        .message(body)
        .title(title)
        .kind(if incompat.is_some() {
            tauri_plugin_dialog::MessageDialogKind::Warning
        } else {
            tauri_plugin_dialog::MessageDialogKind::Info
        })
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

/// 首启配置互通向导：检测到系统 ~/.dsh 且隔离 home 为空时询问用户
///  ① 导入配置到隔离环境（复制凭据 + profile 补丁层，原目录不动，推荐）
///  ② 保持隔离（默认 dsh-home）
///  ③ 直接使用系统 ~/.dsh（命令行老用户，提示风险）
async fn run_setup_wizard(app: &tauri::AppHandle, manager: &DshManager) {
    use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogResult};
    info!("检测到系统 ~/.dsh 且隔离 home 为空，进入配置互通向导");
    let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
    let app2 = app.clone();
    app.dialog()
        .message(
            "检测到你已有系统级 DSH 配置（~/.dsh）。\n\n\
             「导入配置」：把系统凭据与插件开关复制进隔离环境（推荐，原目录不动）；\n\
             「保持隔离」：继续使用全新隔离环境（插件将自动安装，凭据可稍后同步）；\n\
             「使用系统目录」：直接用 ~/.dsh 作为 DSH_HOME（适用于不想迁移的命令行老用户，\n\
             注意：客户端不会修改系统文件，但与命令行实例并存时需自行注意并发）。",
        )
        .title("配置互通")
        .buttons(MessageDialogButtons::YesNoCancelCustom(
            "导入配置".into(),
            "保持隔离".into(),
            "使用系统目录".into(),
        ))
        .show_with_result(move |result| {
            let choice = match &result {
                MessageDialogResult::Yes => 0,
                MessageDialogResult::No => 1,
                _ => 2,
            };
            let _ = tx.send(choice);
            let _ = app2;
        });
    match rx.await.unwrap_or(1) {
        0 => {
            // ① 导入：复制凭据 + profile 补丁层（原目录不动）
            let sys_home = std::env::var("DSH_HOME").unwrap_or_else(|_| {
                std::env::var("HOME").map(|h| format!("{h}/.dsh")).unwrap_or_default()
            });
            let sys_home = std::path::PathBuf::from(sys_home);
            let cred = sys_home.join(".credentials.yaml");
            let _ = manager.sync_credentials_from_system(); // 单向复制缺失键
            if cred.is_file() && !manager.active_home().join(".credentials.yaml").exists() {
                let _ = std::fs::copy(&cred, manager.active_home().join(".credentials.yaml"));
            }
            // profile 补丁层（插件开关等用户定制，小文件）
            let sys_patch = sys_home.join("profiles").join("web").join("cordis.patch.yml");
            if sys_patch.is_file() {
                let dest = manager
                    .active_home()
                    .join("profiles")
                    .join("web")
                    .join("cordis.patch.yml");
                if let Some(p) = dest.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                let _ = std::fs::copy(&sys_patch, &dest);
            }
            manager.emit_status(app, "info", "已导入系统配置到隔离环境", None);
        }
        2 => {
            // ③ 系统模式
            if let Err(e) = manager.set_home_mode("system") {
                warn!("切换系统模式失败: {e}");
            } else {
                manager.emit_status(app, "info", "已切换到系统 ~/.dsh 模式", None);
            }
        }
        _ => {
            // ② 保持隔离
            manager.emit_status(app, "info", "保持隔离环境", None);
        }
    }
    let _ = manager.mark_setup_done();
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

    // 0) 解析运行时 Node（P3：系统 ≥24 优先 → 缓存 → 按需下载 + sha256 校验 + 进度事件）
    manager.emit_status(&app, "preparing", "正在准备 Node 运行时…", None);
    let app_prog = app.clone();
    match crate::process_mgr::ensure_runtime_node(&app, &move |line| {
        let _ = app_prog.emit("dsh-update-progress", line);
    })
    .await
    {
        Ok(node) => info!("Node 运行时就绪：{}", node.display()),
        Err(e) => {
            manager.emit_status(&app, "error", &format!("Node 运行时准备失败：{e}"), None);
            return;
        }
    }

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

    // 2.5) 配置互通：首启向导（系统 ~/.dsh 存在且隔离 home 为空 → 三选一）
    if !manager.safe_mode.load(Ordering::Relaxed)
        && !manager.setup_done.load(Ordering::Relaxed)
        && manager.system_home_exists()
        && manager.isolated_home_empty()
    {
        run_setup_wizard(&app, &manager).await;
    }
    // 2.6) 凭据单向同步（safe-mode 停用；系统模式无需同步）
    if !manager.safe_mode.load(Ordering::Relaxed)
        && manager.sync_credentials.load(Ordering::Relaxed)
        && !manager.use_system_home.load(Ordering::Relaxed)
    {
        let report = manager.sync_credentials_from_system();
        if !report.copied.is_empty() || !report.conflicted.is_empty() {
            manager.emit_status(
                &app,
                "info",
                &format!(
                    "凭据同步完成：新增 {} 项，冲突跳过 {} 项",
                    report.copied.len(),
                    report.conflicted.len()
                ),
                None,
            );
        }
    }

    // 3) 启动 dsh web 服务并加载 UI
    if let Err(e) = manager.start(&app).await {
        manager.emit_status(&app, "error", &format!("dsh 服务启动失败：{e}"), None);
        return;
    }

    // 4) 后台更新检查（尽快可用，再异步检查；尊重开关与安全模式）
    spawn_update_checker(&app, &manager);
}

/// 以带参数的形态重启自身（安全模式开关；崩溃弹窗「以安全模式重启」也走这里）
pub(crate) fn relaunch_with_flag(app: &tauri::AppHandle, flag: &str) {
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
    let logs_viewer = MenuItem::with_id(app, "open-logs-viewer", "日志查看器", true, None::<&str>)?;
    let panel = MenuItem::with_id(app, "panel", "控制面板", true, None::<&str>)?;
    let quit = PredefinedMenuItem::quit(app, Some("退出"))?;

    // 标准「编辑」菜单：macOS 的 Cmd+C/V/X/Z/A 依赖菜单角色路由，缺失则快捷键失灵
    let sep = PredefinedMenuItem::separator(app)?;
    let edit_menu = Submenu::with_items(
        app,
        "编辑",
        true,
        &[
            &PredefinedMenuItem::undo(app, Some("撤销"))?,
            &PredefinedMenuItem::redo(app, Some("重做"))?,
            &sep,
            &PredefinedMenuItem::cut(app, Some("剪切"))?,
            &PredefinedMenuItem::copy(app, Some("复制"))?,
            &PredefinedMenuItem::paste(app, Some("粘贴"))?,
            &sep,
            &PredefinedMenuItem::select_all(app, Some("全选"))?,
        ],
    )?;

    let app_menu = Submenu::with_items(app, "DSH-Cockpit", true, &[&about, &quit])?;
    let dsh_menu = Submenu::with_items(app, "DSH", true, &[&check, &restart, &safe, &logs, &logs_viewer, &panel])?;
    let menu = Menu::with_items(app, &[&app_menu, &edit_menu, &dsh_menu])?;
    app.set_menu(menu)?;

    app.on_menu_event(|app, event| match event.id().as_ref() {
        "about" => {
            use tauri_plugin_dialog::DialogExt;
            let m = app.state::<DshManager>().inner();
            let detail = format!(
                "外壳版本：{}\nDSH 内核：{}\n端口：{}\n\n隔离环境：{}\nDSH_HOME：{}\n\n安全模式：{}",
                app.package_info().version,
                m.installed_version().unwrap_or_else(|| "未安装".into()),
                m.active_port.load(std::sync::atomic::Ordering::Relaxed).max(m.port),
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
        "open-logs-viewer" => show_logs_window(app),
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

    // P2c 深链/文件关联（三种来源统一入口）：
    // 1) 启动时命令行参数（Windows/Linux 文件关联双击 / 深链单参数）
    for arg in std::env::args().skip(1) {
        if !arg.starts_with("--") {
            deep_link::handle_incoming(&handle, &arg);
        }
    }
    // （运行期深链：macOS 走 RunEvent::Opened；Windows/Linux 由
    //   single-instance 回调把 argv 转发到 handle_incoming，见下方插件注册）

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

    // 托盘（显示窗口 / 控制面板 / 重启 dsh / 安全模式重启 / 完全退出）
    // P4 2.10：tooltip 动态显示 dsh 版本与端口（就绪后更新）
    let tray_tooltip = format!(
        "DSH-Cockpit{}",
        manager
            .installed_version()
            .map(|v| format!(" · dsh {v}"))
            .unwrap_or_default()
    );
    let show_item = MenuItem::with_id(&handle, "show", "显示窗口", true, None::<&str>)?;
    let panel_item = MenuItem::with_id(&handle, "panel", "控制面板", true, None::<&str>)?;
    let restart_item = MenuItem::with_id(&handle, "restart", "重启 dsh 服务", true, None::<&str>)?;
    let safe_restart_item = MenuItem::with_id(
        &handle,
        "safe-restart",
        "以安全模式重启",
        true,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(&handle, "quit", "完全退出", true, None::<&str>)?;
    let menu = Menu::with_items(
        &handle,
        &[
            &show_item,
            &panel_item,
            &restart_item,
            &safe_restart_item,
            &quit_item,
        ],
    )?;

    let _tray = TrayIconBuilder::with_id("main-tray")
        .icon(handle.default_window_icon().unwrap().clone())
        .tooltip(tray_tooltip)
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
            // P4 2.8：托盘直接「以安全模式重启」（带 --safe-mode 重启自身）
            "safe-restart" => relaunch_with_flag(app, "--safe-mode"),
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
    create_logs_window(&handle)?;

    // 后台引导
    tauri::async_runtime::spawn(bootstrap(handle.clone()));

    // P4 宿主扩展：全局快捷键（Cmd+Shift+P 打开控制面板）
    host_ext::register_shortcuts(&handle);

    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_global_shortcut::Builder::new().build())
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(
            // P4 2.10 窗口状态记忆：只记忆大小/位置/最大化，**不恢复可见性**——
            // 否则面板/日志窗口上次被打开过，下次启动会自动弹出（bug）。
            tauri_plugin_window_state::Builder::default()
                .with_state_flags(
                    tauri_plugin_window_state::StateFlags::all()
                        - tauri_plugin_window_state::StateFlags::VISIBLE,
                )
                .build(),
        )        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(
            tauri_plugin_log::Builder::new()
                .level(log::LevelFilter::Info)
                .max_file_size(5 * 1024 * 1024) // 5MB 轮转，防止日志无限增长
                .targets([
                    Target::new(TargetKind::Stdout),
                    Target::new(TargetKind::LogDir {
                        file_name: Some("dsh-cockpit".into()),
                    }),
                ])
                .build(),
        )
        // 单实例锁：重复启动时聚焦已有主窗口；深链/文件参数转发给运行中的实例
        .plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            tray::show_main_window(app);
            for arg in argv.iter().skip(1) {
                if !arg.starts_with("--") {
                    deep_link::handle_incoming(app, arg);
                }
            }
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
            commands::get_logs,
            commands::collect_diagnostics,
            commands::open_log_viewer,
            commands::open_log_dir,
            commands::open_in_finder,
            commands::list_presets,
            commands::export_preset,
            commands::import_preset,
            commands::sync_credentials_now,
            commands::export_backup,
            commands::set_home_mode,
            commands::get_autostart,
            commands::set_autostart,
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
            // macOS/iOS：系统通过深链或文件关联唤起（URL 或 file:// 路径）。
            // 与 Reopen 同理，该变体仅在部分平台存在，用属性 cfg 包裹。
            #[cfg(any(target_os = "macos", target_os = "ios", target_os = "android"))]
            tauri::RunEvent::Opened { urls } => {
                for url in urls {
                    deep_link::handle_incoming(handle, url.as_str());
                }
            }            tauri::RunEvent::Exit => {
                if let Some(m) = handle.try_state::<DshManager>() {
                    let _ = m.inner().kill_managed_now();
                }
            }
            _ => {}
        }
    });
}
