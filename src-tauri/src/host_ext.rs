//! P4 宿主扩展层（2.5.3）：
//! - 系统通知（就绪 / 崩溃 / 更新可用 / 导入结果）
//! - 全局快捷键 Cmd+Shift+P 打开控制面板
//! - IPC 桥 `window.__dshCockpit`（白名单命令，Rust 侧守卫）

use tauri::AppHandle;

// ---------------------------------------------------------------------------
// 系统通知（tauri-plugin-notification）
// ---------------------------------------------------------------------------

/// 发系统通知（低危操作，失败仅记日志，不阻塞主流程）。
pub fn notify(app: &AppHandle, title: &str, body: &str) {
    use tauri_plugin_notification::NotificationExt;
    let app2 = app.clone();
    let title = title.to_string();
    let body = body.to_string();
    std::thread::spawn(move || {
        let _ = app2.notification().builder().title(title).body(body).show();
    });
}

/// dsh 服务就绪通知（仅首次就绪发一次，避免每次重启打扰）
pub fn notify_ready(app: &AppHandle, port: u16) {
    notify(app, "DSH-Cockpit", &format!("DSH 内核已就绪，服务运行于 127.0.0.1:{port}"));
}

/// 崩溃提示
pub fn notify_crash(app: &AppHandle, detail: &str) {
    notify(app, "DSH 服务异常", detail);
}

/// 内核更新可用（自动检查发现新版本时）
pub fn notify_update_available(app: &AppHandle, version: &str) {
    notify(
        app,
        "DSH 内核更新可用",
        &format!("最新版本 {version}，可在控制面板一键升级"),
    );
}

// ---------------------------------------------------------------------------
// 全局快捷键（tauri-plugin-global-shortcut）
// ---------------------------------------------------------------------------

/// 注册宿主快捷键：Cmd+Shift+P（mac）/ Ctrl+Shift+P（win/linux）→ 打开控制面板。
/// 注册失败不阻塞启动（可能被其他应用占用）。
pub fn register_shortcuts(app: &AppHandle) {
    use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};
    let shortcut = Shortcut::new(Some(Modifiers::SHIFT | Modifiers::SUPER), Code::KeyP);
    let app2 = app.clone();
    let result = app.global_shortcut().on_shortcut(shortcut, move |app_handle, _sc, event| {
        if event.state() == ShortcutState::Pressed {
            crate::tray::show_panel(app_handle);
        }
    });
    match result {
        Ok(_) => log::info!("全局快捷键已注册: Cmd+Shift+P"),
        Err(e) => log::warn!("全局快捷键注册失败: {e}"),
    }
    let _ = &app2;
}

// ---------------------------------------------------------------------------
// IPC 桥 window.__dshCockpit（2.5.3-4）
// ---------------------------------------------------------------------------

/// 注入到 dsh Web UI 的桥接脚本：暴露 `window.__dshCockpit` 白名单命令。
/// 只允许调用宿主提供的只读/本地操作（开日志目录、导入预设、重启服务、读版本）。
/// 实现：前端用 Tauri 全局 API（withGlobalTauri）直接 invoke 宿主命令——
/// 但为隔离前端与宿主约定，这里注入一个轻量封装，前端 dsh 页面（非本壳页面）
/// 无需关心实现细节；守卫仍在 commands.rs（仅 loopback 主窗口可调）。
pub const DSH_COCKPIT_BRIDGE_JS: &str = r#"
  (function(){
    if (window.__dshCockpit) return;
    var bridge = {
      // 打开系统日志目录
      openLogDir: function(){ try { return window.__TAURI__.core.invoke('open_log_dir'); } catch(e){} },
      // 打开 .dshpreset 导入对话框
      importPreset: function(){ try { return window.__TAURI__.core.invoke('import_preset'); } catch(e){} },
      // 重启 dsh 服务
      restartService: function(){ try { return window.__TAURI__.core.invoke('restart_dsh'); } catch(e){} },
      // 读取版本信息
      versions: function(){ try { return window.__TAURI__.core.invoke('get_status'); } catch(e){} }
    };
    Object.defineProperty(window, '__dshCockpit', { value: bridge, configurable: false, writable: false });
  })();
"#;
