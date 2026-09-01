//! 前端可调用的 IPC 命令（控制面板 UI ↔ Rust 后端）

use std::path::PathBuf;
use serde::Serialize;
use tauri::{AppHandle, Emitter, State};

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

/// 读取外壳日志文件尾部（日志查看器历史；dsh 子进程实时日志走 dsh-log-line 事件）
#[tauri::command]
pub fn get_logs(manager: State<'_, DshManager>, limit: Option<usize>) -> Vec<String> {
    let path = manager.inner().log_dir.join("dsh-cockpit.log");
    let limit = limit.unwrap_or(800).min(5000);
    match std::fs::read_to_string(&path) {
        Ok(raw) => {
            let lines: Vec<String> = raw.lines().map(|s| s.to_string()).collect();
            let start = lines.len().saturating_sub(limit);
            lines[start..].to_vec()
        }
        Err(_) => Vec::new(),
    }
}

/// 导出诊断包：日志 + 版本/平台信息 → zip，返回压缩包路径（供前端展示/打开）
#[tauri::command]
pub fn collect_diagnostics(manager: State<'_, DshManager>, app: AppHandle) -> Result<String, String> {
    use std::io::Write;
    let m = manager.inner();
    let out_dir = m.data_dir.join("diagnostics");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let zip_path = out_dir.join(format!("dsh-cockpit-diagnostics-{secs}.zip"));

    let file = std::fs::File::create(&zip_path).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    let mut info = String::new();
    info.push_str(&format!("app_version: {}\n", app.package_info().version));
    info.push_str(&format!(
        "platform: {}-{}\n",
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    info.push_str(&format!(
        "dsh_version: {}\n",
        m.installed_version().unwrap_or_else(|| "未安装".into())
    ));
    info.push_str(&format!(
        "port: {}\n",
        m.active_port.load(std::sync::atomic::Ordering::Relaxed).max(m.port)
    ));
    info.push_str(&format!("registry: {}\n", m.registry));
    info.push_str(&format!(
        "node_runtime: {}\n",
        crate::process_mgr::runtime_node_cached()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未解析（尚未启动或解析失败）".into())
    ));
    info.push_str(&format!("env_dir: {}\n", m.env_dir.display()));
    info.push_str(&format!("home_dir: {}\n", m.active_home().display()));
    info.push_str(&format!("log_dir: {}\n", m.log_dir.display()));
    zw.start_file("info.txt", options).map_err(|e| e.to_string())?;
    zw.write_all(info.as_bytes()).map_err(|e| e.to_string())?;

    if let Ok(raw) = std::fs::read_to_string(m.log_dir.join("dsh-cockpit.log")) {
        zw.start_file("dsh-cockpit.log", options).map_err(|e| e.to_string())?;
        zw.write_all(raw.as_bytes()).map_err(|e| e.to_string())?;
    }
    zw.finish().map_err(|e| e.to_string())?;
    Ok(zip_path.to_string_lossy().into_owned())
}

/// 打开日志查看器窗口
#[tauri::command]
pub fn open_log_viewer(app: AppHandle) -> Result<(), String> {
    crate::show_logs_window(&app);
    Ok(())
}

/// 打开日志目录（系统文件管理器）
#[tauri::command]
pub fn open_log_dir(manager: State<'_, DshManager>) -> Result<(), String> {
    crate::tray::open_external(&manager.inner().log_dir.to_string_lossy());
    Ok(())
}

/// 在系统文件管理器中显示指定路径（诊断包等）
#[tauri::command]
pub fn open_in_finder(path: String) -> Result<(), String> {
    crate::tray::open_external(&path);
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent 预设（.dshpreset）导入导出
// ---------------------------------------------------------------------------

/// 列出自定义预设
#[tauri::command]
pub fn list_presets(manager: State<'_, DshManager>) -> Vec<crate::preset::PresetInfo> {
    crate::preset::list_presets(manager.inner())
}

/// 导出预设：原生保存对话框 → 打包 .dshpreset，返回导出路径
#[tauri::command]
pub async fn export_preset(
    app: AppHandle,
    manager: State<'_, DshManager>,
    id: String,
) -> Result<String, String> {
    use tauri_plugin_dialog::{DialogExt, FilePath};
    let m = manager.inner().clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    app.dialog()
        .file()
        .set_file_name(format!("{id}.dshpreset"))
        .add_filter("DSH 预设", &["dshpreset"])
        .save_file(move |path| {
            let _ = tx.send(match path {
                Some(FilePath::Path(p)) => Some(p.into()),
                _ => None,
            });
        });
    let dest = rx.await.ok().flatten().ok_or("已取消导出")?;
    crate::preset::export_preset(&crate::preset::presets_root(&m), &id, &dest)
}

/// 导入预设：原生选择对话框 → 校验并安装，返回新预设 id
#[tauri::command]
pub async fn import_preset(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<String, String> {
    use tauri_plugin_dialog::{DialogExt, FilePath};
    let m = manager.inner().clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    app.dialog()
        .file()
        .add_filter("DSH 预设", &["dshpreset"])
        .pick_file(move |path| {
            let _ = tx.send(match path {
                Some(FilePath::Path(p)) => Some(p.into()),
                _ => None,
            });
        });
    let src = rx.await.ok().flatten().ok_or("已取消导入")?;
    crate::preset::import_preset(&crate::preset::presets_root(&m), &src)
}

// ---------------------------------------------------------------------------
// 配置互通（home 模式 / 凭据同步 / 备份导出）
// ---------------------------------------------------------------------------

/// 手动触发凭据单向同步，返回报告
#[tauri::command]
pub fn sync_credentials_now(manager: State<'_, DshManager>) -> crate::dsh::CredentialSyncReport {
    manager.inner().sync_credentials_from_system()
}

/// 一键导出备份（隔离 dsh-home + 凭据 + config.json → zip），原生保存对话框
#[tauri::command]
pub async fn export_backup(
    app: AppHandle,
    manager: State<'_, DshManager>,
) -> Result<String, String> {
    use std::path::PathBuf;
    use tauri_plugin_dialog::{DialogExt, FilePath};
    let m = manager.inner().clone();
    let (tx, rx) = tokio::sync::oneshot::channel::<Option<PathBuf>>();
    app.dialog()
        .file()
        .set_file_name("dsh-cockpit-backup.zip")
        .add_filter("ZIP 备份", &["zip"])
        .save_file(move |path| {
            let _ = tx.send(match path {
                Some(FilePath::Path(p)) => Some(p.into()),
                _ => None,
            });
        });
    let dest = rx.await.ok().flatten().ok_or("已取消导出")?;
    m.export_backup(&dest)
}

/// 切换 home 模式（isolated / system），并返回是否需重启服务生效
#[tauri::command]
pub fn set_home_mode(manager: State<'_, DshManager>, mode: String) -> Result<(), String> {
    manager.inner().set_home_mode(&mode)
}

// ---------------------------------------------------------------------------
// P4 2.10 开机自启（tauri-plugin-autostart）
// ---------------------------------------------------------------------------

/// 查询开机自启状态
#[tauri::command]
pub fn get_autostart(app: AppHandle) -> bool {
    use tauri_plugin_autostart::ManagerExt;
    app.autolaunch().is_enabled().unwrap_or(false)
}

/// 设置开机自启
#[tauri::command]
pub fn set_autostart(app: AppHandle, enabled: bool) -> Result<(), String> {
    use tauri_plugin_autostart::ManagerExt;
    let mgr = app.autolaunch();
    if enabled {
        mgr.enable().map_err(|e| format!("开启开机自启失败: {e}"))
    } else {
        mgr.disable().map_err(|e| format!("关闭开机自启失败: {e}"))
    }
}
