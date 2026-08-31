//! 托盘与窗口显示辅助

use tauri::{AppHandle, Manager};

/// 显示并聚焦主窗口（窗口隐藏→显示；已销毁→重建）。
/// 单实例二次启动、托盘点击、Dock 图标点击都会走到这里。
pub fn show_main_window(app: &AppHandle) {
    crate::show_or_recreate_main(app);
}

/// 显示并聚焦控制面板窗口
pub fn show_panel(app: &AppHandle) {
    if let Some(panel) = app.get_webview_window("panel") {
        let _ = panel.show();
        let _ = panel.set_focus();
    }
}

/// 在系统浏览器中打开外部链接（跨平台，脱离 shell 环境依赖）
pub fn open_external(url: &str) {
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("/usr/bin/open").arg(url).spawn()
    } else if cfg!(windows) {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if let Err(e) = result {
        log::warn!("打开外部链接失败 {url}: {e}");
    }
}
