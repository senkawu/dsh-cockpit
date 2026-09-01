//! 深链与文件关联（P2c）：
//! - `dsh-cockpit://import?path=...` 深链 → 导入 .dshpreset 向导
//! - `.dshpreset` 文件关联双击 → 同一向导
//! 统一入口 `handle_incoming`（可被 macOS RunEvent::Opened、deep-link 插件回调、
//! Windows/Linux 命令行参数三种来源调用）。

use std::path::PathBuf;

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::{DialogExt, MessageDialogButtons, MessageDialogKind};

/// 处理一条进入应用的资源（深链 URL 或本地文件路径）。
/// - 深链：`dsh-cockpit://import?path=<urlencoded>`（path 为 file:// 或绝对路径）
/// - 文件：`.dshpreset` 绝对路径（文件关联双击）
/// 其他内容忽略。导入结果通过 `preset-import-result` 事件 + 确认框反馈。
pub fn handle_incoming(app: &AppHandle, raw: &str) {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return;
    }

    // 1) 深链 URL
    if let Ok(url) = url::Url::parse(trimmed) {
        if url.scheme() == "dsh-cockpit" {
            let query = url.query().unwrap_or("");
            let mut pairs = url::form_urlencoded::parse(query.as_bytes());
            let file_arg = pairs.find(|(k, _)| k == "path").map(|(_, v)| v.into_owned());
            match (url.host_str(), file_arg) {
                (Some("import"), Some(path_str)) => {
                    handle_import_arg(app, &path_str);
                    return;
                }
                (Some("import"), None) => {
                    // 缺 path 参数 → 打开面板让用户手动选
                    crate::tray::show_panel(app);
                    return;
                }
                _ => {
                    log::warn!("未知深链: {trimmed}");
                    return;
                }
            }
        }
        // file:// URL（macOS 文件关联）
        if url.scheme() == "file" {
            if let Ok(p) = url.to_file_path() {
                if is_preset_file(&p) {
                    import_preset_file(app, &p.to_string_lossy());
                    return;
                }
            }
        }
        return;
    }

    // 2) 裸路径（Windows/Linux 文件关联双击把文件路径作为唯一参数传入）
    let p = PathBuf::from(trimmed);
    if is_preset_file(&p) {
        import_preset_file(app, &p.to_string_lossy());
    }
}

/// 处理深链 path 参数：可能是 file:// URL 或绝对路径，统一转成文件路径后导入。
fn handle_import_arg(app: &AppHandle, arg: &str) {
    match extract_preset_path(arg) {
        Some(p) => import_preset_file(app, &p.to_string_lossy()),
        None => log::warn!("深链 path 无效: {arg}"),
    }
}

/// 从深链 path 参数提取 .dshpreset 文件路径（纯函数，可单测）：
/// - `file:///abs/x.dshpreset` → `/abs/x.dshpreset`
/// - `/abs/x.dshpreset` → 原样
/// 非预设文件返回 None。
fn extract_preset_path(arg: &str) -> Option<PathBuf> {
    // 1) file:// URL → 路径
    if let Ok(u) = url::Url::parse(arg) {
        if u.scheme() == "file" {
            if let Ok(p) = u.to_file_path() {
                return is_preset_path(&p).then_some(p);
            }
            return None;
        }
    }
    // 2) 绝对路径（不存在时仅按扩展名放行，交由导入校验给出准确错误）
    let p = PathBuf::from(arg);
    is_preset_path(&p).then_some(p)
}

/// 是否 .dshpreset 扩展名（不要求文件存在，用于纯解析）
fn is_preset_path(p: &PathBuf) -> bool {
    p.extension()
        .map(|e| e.eq_ignore_ascii_case("dshpreset"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_file_url() {
        let p = extract_preset_path("file:///tmp/test.dshpreset").unwrap();
        assert!(p.ends_with("test.dshpreset"));
        assert!(p.is_absolute());
    }

    #[test]
    fn extracts_plain_path() {
        let p = extract_preset_path("/tmp/x.dshpreset").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/x.dshpreset"));
    }

    #[test]
    fn rejects_non_preset() {
        assert!(extract_preset_path("file:///tmp/readme.txt").is_none());
        assert!(extract_preset_path("dsh-cockpit://other").is_none());
        assert!(extract_preset_path("").is_none());
    }
}

fn is_preset_file(p: &PathBuf) -> bool {
    p.extension()
        .map(|e| e.eq_ignore_ascii_case("dshpreset"))
        .unwrap_or(false)
        && p.is_file()
}

/// 导入向导：确认 → 校验安装 → 结果反馈（弹窗 + 事件，供面板刷新）。
fn import_preset_file(app: &AppHandle, path_str: &str) {
    let app2 = app.clone();
    let path_owned = path_str.to_string();
    let file_name = PathBuf::from(path_str)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| path_str.to_string());

    app.dialog()
        .message(format!("将导入 Agent 预设：\n{file_name}\n\n是否继续？"))
        .title("导入 .dshpreset")
        .kind(MessageDialogKind::Info)
        .buttons(MessageDialogButtons::OkCancelCustom(
            "导入".into(),
            "取消".into(),
        ))
        .show(move |confirmed| {
            if !confirmed {
                return;
            }
            // 校验（解压/检查是阻塞 IO）+ 结果弹窗都在后台线程做，
            // 避免卡 UI；事件供控制面板刷新预设列表。
            let app_work = app2.clone();
            std::thread::spawn(move || {
                let m = app_work
                    .state::<crate::dsh::DshManager>()
                    .inner()
                    .clone();
                let root = crate::preset::presets_root(&m);
                match crate::preset::import_preset(&root, &std::path::Path::new(&path_owned)) {
                    Ok(id) => {
                        let _ = app_work.emit("preset-import-result", format!("ok:{id}"));
                        let _ = app_work.emit("presets-changed", ());
                        let _ = app_work
                            .dialog()
                            .message(format!("预设导入成功：{id}"))
                            .title("导入完成")
                            .blocking_show();
                    }
                    Err(e) => {
                        let _ = app_work.emit("preset-import-result", format!("err:{e}"));
                        let _ = app_work
                            .dialog()
                            .message(format!("预设导入失败：{e}"))
                            .title("导入失败")
                            .blocking_show();
                    }
                }
            });
        });
}
