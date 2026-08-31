//! 前端可调用的 IPC 命令（控制面板 UI ↔ Rust 后端）

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::dsh::DshManager;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub installed: Option<String>,
    pub latest: Option<String>,
    pub has_update: bool,
}

/// 控制面板状态：版本信息、运行状态、路径、安全模式等
#[tauri::command]
pub fn get_status(app: AppHandle, manager: State<'_, DshManager>) -> crate::dsh::StatusPayload {
    let mut payload = manager.inner().status_payload();
    payload.app_version = app.package_info().version.to_string();
    payload
}

/// 检查 DSH 内核更新（npm 源，不弹窗，由前端展示；manual=true 忽略“跳过此版本”）
#[tauri::command]
pub async fn check_dsh_update(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<UpdateInfo, String> {
    let m = manager.inner().clone();
    drop(manager);
    let installed = m.installed_version();
    let latest = m.latest_version(&app).await?;
    let has_update = m.has_update(&latest, true); // 手动检查忽略跳过
    Ok(UpdateInfo {
        installed,
        latest: Some(latest),
        has_update,
    })
}

/// 记录“跳过此版本”（自动检查不再提示；手动检查忽略）
#[tauri::command]
pub fn set_skipped_version(
    manager: State<'_, DshManager>,
    version: Option<String>,
) -> Result<(), String> {
    manager.inner().set_skipped_version(version)
}

/// 退出安全模式并重启（不带 --safe-mode 参数重新拉起自己）
#[tauri::command]
pub fn exit_safe_mode(app: AppHandle) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| a != "--safe-mode")
        .collect();
    std::process::Command::new(&exe)
        .args(&args)
        .spawn()
        .map_err(|e| format!("重启失败: {e}"))?;
    app.exit(0);
    Ok(())
}

/// 执行 DSH 内核更新（停止 → 备份 → npm install → 冒烟 → 失败回退）
#[tauri::command]
pub async fn apply_dsh_update(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<String, String> {
    let m = manager.inner().clone();
    drop(manager);
    m.update(&app).await
}

/// 重启 dsh 服务
#[tauri::command]
pub async fn restart_dsh(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<(), String> {
    let m = manager.inner().clone();
    drop(manager);
    m.stop(&app).await?;
    m.start(&app).await
}

/// 停止 dsh 服务（保留外壳运行）
#[tauri::command]
pub async fn stop_dsh(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<(), String> {
    let m = manager.inner().clone();
    drop(manager);
    m.stop(&app).await
}

/// 完全退出（先优雅停止子进程）
#[tauri::command]
pub async fn quit_app(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<(), String> {
    let m = manager.inner().clone();
    drop(manager);
    m.stop(&app).await?;
    app.exit(0);
    Ok(())
}

/// 打开控制面板窗口
#[tauri::command]
pub fn open_panel(app: AppHandle) -> Result<(), String> {
    crate::tray::show_panel(&app);
    Ok(())
}

/// 设置「启动时自动检查 DSH 更新」开关（写入 config.json 持久化）
#[tauri::command]
pub fn set_auto_check_update(
    app: AppHandle,
    manager: State<'_, DshManager>,
    enabled: bool,
) -> Result<(), String> {
    let m = manager.inner().clone();
    let data_dir = m.data_dir.clone();
    let mut settings = crate::settings::Settings::load(&data_dir).unwrap_or_default();
    settings.auto_check_update = enabled;
    settings
        .save(&data_dir)
        .map_err(|e| format!("保存设置失败: {e}"))?;
    m.auto_check_update.store(enabled, std::sync::atomic::Ordering::Relaxed);
    let _ = app.emit("settings-changed", enabled);
    Ok(())
}

/// 最近一次 dsh 状态（状态页加载时回放，防止启动早期事件丢失）
#[tauri::command]
pub fn get_last_status(manager: State<'_, DshManager>) -> Option<serde_json::Value> {
    manager.inner().last_status()
}

/// 插件列表（市场 / 鲸鱼挂件 / 用量面板）
#[tauri::command]
pub fn get_plugins(manager: State<'_, DshManager>) -> Vec<crate::dsh::PluginInfo> {
    manager.inner().plugins_info()
}

/// 插件开关：写 profile 补丁层（disabled: true|false），HMR 约 1 秒生效
#[tauri::command]
pub fn set_plugin_enabled(
    manager: State<'_, DshManager>,
    id: String,
    enabled: bool,
) -> Result<(), String> {
    let m = manager.inner().clone();
    m.set_plugin_enabled(&id, enabled)
}

/// 检查 Tauri 外壳自身更新（tauri-plugin-updater；需先配置更新源与签名公钥）
#[tauri::command]
pub async fn check_shell_update(app: AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;
    let updater = app
        .updater()
        .map_err(|e| format!("updater 未配置（需要 endpoints 与签名公钥）：{e}"))?;
    match updater.check().await.map_err(|e| format!("检查外壳更新失败：{e}"))? {
        Some(update) => {
            update
                .download_and_install(|_chunk, _total| {}, || {})
                .await
                .map_err(|e| format!("安装外壳更新失败：{e}"))?;
            Ok("外壳已更新，重启后生效".into())
        }
        None => Ok("外壳已是最新版本".into()),
    }
}
