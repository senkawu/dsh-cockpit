//! Agent 预设（`.dshpreset`）导入导出 —— 与 dataelement/dsh-desktop 生态互通。
//!
//! 格式契约：
//! ```text
//! .dshpreset = ZIP
//! ├── manifest.json        # {format:"dsh-preset", version:1, id, name, description, sourceDshVersion, exportedAt}
//! └── preset/
//!     ├── agent.cordis.yml
//!     ├── preset.yml                 # optional
//!     └── skills / plugins / 预设自有资产
//! ```
//!
//! 安全边界（导入两步式中的校验）：
//!   - 包体积 ≤ 100MB；
//!   - 条目名拒绝：绝对路径、`..` 遍历、反斜杠（Windows 兼容拒绝）、驱动盘符；
//!   - manifest 必须为 dsh-preset v1 且含合法 id/name；
//!   - 导出拒绝符号链接与垃圾文件（.DS_Store / Thumbs.db / desktop.ini）；
//!   - 安装为原子 move：先解压到临时目录校验，再整体移入 `.agent-presets/<id>`；
//!   - id 冲突时自动派生新 id（`<id>-2`、`<id>-3`…），绝不覆盖已有预设。

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use log::{info, warn};
use serde::{Deserialize, Serialize};

use crate::dsh::DshManager;

pub const PRESET_FORMAT: &str = "dsh-preset";
pub const PRESET_VERSION: u32 = 1;
pub const MAX_PRESET_SIZE: u64 = 100 * 1024 * 1024; // 100MB

/// 预设信息（面板展示 / 导出选择）
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PresetInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub path: String,
}

/// .dshpreset 内的 manifest.json
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PresetManifest {
    pub format: String,
    pub version: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub source_dsh_version: Option<String>,
    #[serde(default)]
    pub exported_at: String,
}

/// 自定义预设根目录（隔离 DSH_HOME 下）
pub fn presets_root(manager: &DshManager) -> PathBuf {
    manager.active_home().join(".agent-presets")
}

/// 列出自定义预设（读取 agent.cordis.yml 的 name/description 作为展示信息）
pub fn list_presets(manager: &DshManager) -> Vec<PresetInfo> {
    let root = presets_root(manager);
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(&root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let (name, description) = read_preset_meta(&path);
        out.push(PresetInfo {
            id: id.clone(),
            name: name.unwrap_or_else(|| id.clone()),
            description: description.unwrap_or_default(),
            path: path.to_string_lossy().into_owned(),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// 从 agent.cordis.yml 读取 name/description（失败返回 None）
fn read_preset_meta(dir: &Path) -> (Option<String>, Option<String>) {
    let file = dir.join("agent.cordis.yml");
    let raw = match std::fs::read_to_string(&file) {
        Ok(r) => r,
        Err(_) => return (None, None),
    };
    let value: serde_yaml::Value = match serde_yaml::from_str(&raw) {
        Ok(v) => v,
        Err(_) => return (None, None),
    };
    let name = value.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
    let description = value
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (name, description)
}

/// 导出预设为 .dshpreset（仅允许自定义预设；拒符号链接与垃圾文件）
pub fn export_preset(root: &Path, id: &str, dest_zip: &Path) -> Result<String, String> {
    let src = root.join(id);
    if !src.is_dir() {
        return Err(format!("预设 {id} 不存在或不是自定义预设（内置预设需先复制）"));
    }
    if let Some(parent) = dest_zip.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = std::fs::File::create(dest_zip).map_err(|e| e.to_string())?;
    let mut zw = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);

    // manifest.json
    let (name, description) = read_preset_meta(&src);
    let manifest = PresetManifest {
        format: PRESET_FORMAT.into(),
        version: PRESET_VERSION,
        id: id.to_string(),
        name: name.unwrap_or_else(|| id.to_string()),
        description: description.unwrap_or_default(),
        source_dsh_version: None,
        exported_at: now_rfc3339(),
    };
    zw.start_file("manifest.json", options).map_err(|e| e.to_string())?;
    zw.write_all(
        serde_json::to_string_pretty(&manifest)
            .map_err(|e| e.to_string())?
            .as_bytes(),
    )
    .map_err(|e| e.to_string())?;

    // preset/ 目录内容（递归）
    let mut count = 0usize;
    add_dir_to_zip(&mut zw, &src, &PathBuf::from("preset"), &options, &mut count)?;
    zw.finish().map_err(|e| e.to_string())?;
    info!("已导出预设 {id}（{count} 个文件）→ {}", dest_zip.display());
    Ok(dest_zip.to_string_lossy().into_owned())
}

/// 递归把目录加入 zip（rel 为 zip 内相对路径）；跳过符号链接与垃圾文件
fn add_dir_to_zip(
    zw: &mut zip::ZipWriter<std::fs::File>,
    dir: &Path,
    rel: &Path,
    options: &zip::write::SimpleFileOptions,
    count: &mut usize,
) -> Result<(), String> {
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())?.flatten() {
        let from = entry.path();
        let meta = std::fs::symlink_metadata(&from).map_err(|e| e.to_string())?;
        if meta.file_type().is_symlink() {
            warn!("跳过符号链接: {}", from.display());
            continue;
        }
        let name = entry.file_name();
        if is_junk_file(&name) {
            continue;
        }
        let rel_path = rel.join(&name);
        if meta.is_dir() {
            add_dir_to_zip(zw, &from, &rel_path, options, count)?;
        } else if meta.is_file() {
            let mut data = Vec::new();
            std::fs::File::open(&from)
                .and_then(|mut f| f.read_to_end(&mut data))
                .map_err(|e| e.to_string())?;
            zw.start_file(rel_path.to_string_lossy().replace('\\', "/"), *options)
                .map_err(|e| e.to_string())?;
            zw.write_all(&data).map_err(|e| e.to_string())?;
            *count += 1;
        }
    }
    Ok(())
}

fn is_junk_file(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_string_lossy().as_ref(),
        ".DS_Store" | "Thumbs.db" | "desktop.ini"
    )
}

/// 导入 .dshpreset：校验 → 解压到临时目录 → 原子移入。返回新预设 id。
pub fn import_preset(root: &Path, zip_path: &Path) -> Result<String, String> {
    // 1) 大小与打开校验
    let meta = std::fs::metadata(zip_path).map_err(|e| e.to_string())?;
    if meta.len() > MAX_PRESET_SIZE {
        return Err(format!("预设包超过体积上限（{}MB）", MAX_PRESET_SIZE / 1024 / 1024));
    }
    let file = std::fs::File::open(zip_path).map_err(|e| e.to_string())?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| format!("不是有效的 zip 包: {e}"))?;

    // 2) 解压到临时目录（先整体校验条目名，拒绝路径穿越）
    std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let tmp = root.join(format!(".import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).map_err(|e| e.to_string())?;

    let mut manifest: Option<PresetManifest> = None;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).map_err(|e| e.to_string())?;
        let raw_name = entry.name().to_string();
        // 安全校验：绝对路径 / .. 遍历 / 反斜杠 / 盘符
        if !safe_zip_name(&raw_name) {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(format!("预设包包含非法路径（已拒绝）：{raw_name}"));
        }
        let clean = raw_name.trim_start_matches("./");
        if clean == "manifest.json" {
            let mut buf = String::new();
            entry.read_to_string(&mut buf).map_err(|e| e.to_string())?;
            let m: PresetManifest = serde_json::from_str(&buf)
                .map_err(|e| format!("manifest.json 非法: {e}"))?;
            if m.format != PRESET_FORMAT || m.version != PRESET_VERSION || m.id.trim().is_empty() {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err("不支持的预设包（manifest 格式/版本不匹配）".into());
            }
            manifest = Some(m);
            continue;
        }
        let dest = tmp.join(clean);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = std::fs::File::create(&dest).map_err(|e| e.to_string())?;
        std::io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }

    let manifest = manifest.ok_or_else(|| {
        let _ = std::fs::remove_dir_all(&tmp);
        "预设包缺少 manifest.json".to_string()
    })?;

    // 3) 确认包内确实有 agent.cordis.yml（preset/ 下）
    let preset_dir = tmp.join("preset");
    if !preset_dir.join("agent.cordis.yml").is_file() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err("预设包缺少 preset/agent.cordis.yml".into());
    }
    // 剥离 preset/ 包装层：dsh 的 .agent-presets/<id>/ 布局要求 agent.cordis.yml 在预设根
    let tmp2 = tmp.join(".flat");
    std::fs::create_dir_all(&tmp2).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&preset_dir).map_err(|e| e.to_string())?.flatten() {
        let from = entry.path();
        let to = tmp2.join(entry.file_name());
        std::fs::rename(&from, &to).map_err(|e| e.to_string())?;
    }
    // 先把 tmp2 移出 tmp，再清理 tmp，最后把 tmp2 挪回 tmp 路径（顺序不能反）
    let flat = tmp.parent().unwrap_or(Path::new("/")).join(format!(".import-flat-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&flat);
    std::fs::rename(&tmp2, &flat).map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::rename(&flat, &tmp).map_err(|e| e.to_string())?;

    // 4) id 冲突 → 派生新 id；原子 move
    let mut final_id = manifest.id.clone();
    let mut n = 2;
    while root.join(&final_id).exists() {
        final_id = format!("{}-{}", manifest.id, n);
        n += 1;
    }
    let dest = root.join(&final_id);
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_dir_all(&tmp);
        format!("安装预设失败: {e}")
    })?;
    info!(
        "已导入预设 {}（来源 id: {}）",
        final_id, manifest.id
    );
    Ok(final_id)
}

/// zip 条目名安全校验：拒绝绝对路径 / `..` 遍历 / 反斜杠 / 盘符
fn safe_zip_name(name: &str) -> bool {
    if name.starts_with('/') || name.contains('\\') {
        return false;
    }
    if name.len() >= 2 && name.as_bytes()[1] == b':' {
        return false; // Windows 盘符 C:\
    }
    for seg in name.split('/') {
        if seg == ".." || seg.is_empty() {
            return false;
        }
    }
    true
}

/// 当前时间的 RFC3339 字符串（导出时间戳）
fn now_rfc3339() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 简化：epoch 秒 + UTC 标注（仅用于展示/排序）
    format!("{}Z", secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn make_preset(root: &Path, id: &str) {
        let dir = root.join(id);
        fs::create_dir_all(dir.join("skills")).unwrap();
        fs::write(
            dir.join("agent.cordis.yml"),
            format!("name: {id}-名称\ndescription: 测试预设\n"),
        )
        .unwrap();
        fs::write(dir.join("skills").join("demo.md"), "# 技能\n").unwrap();
        // 符号链接与垃圾文件应被剔除
        #[cfg(unix)]
        {
            let _ = std::os::unix::fs::symlink("/etc/hosts", dir.join("evil-link"));
        }
        fs::write(dir.join(".DS_Store"), "junk").unwrap();
    }

    #[test]
    fn export_import_roundtrip() {
        let tmp = std::env::temp_dir().join(format!("preset-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let root = tmp.join("presets");
        fs::create_dir_all(&root).unwrap();

        make_preset(&root, "demo");
        let zip = tmp.join("demo.dshpreset");
        export_preset(&root, "demo", &zip).unwrap();

        // zip 内容检查：manifest + preset/ 存在；无 evil-link / .DS_Store
        let f = fs::File::open(&zip).unwrap();
        let mut archive = zip::ZipArchive::new(f).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.contains(&"manifest.json".to_string()));
        assert!(names.contains(&"preset/agent.cordis.yml".to_string()));
        assert!(!names.iter().any(|n| n.contains("evil-link") || n.contains(".DS_Store")));
        assert!(names.iter().all(|n| safe_zip_name(n)));

        // 导入到新根
        let root2 = tmp.join("presets2");
        let id = import_preset(&root2, &zip).unwrap();
        assert_eq!(id, "demo");
        assert!(root2.join("demo").join("agent.cordis.yml").is_file());

        // 重复导入 → 派生新 id
        let id2 = import_preset(&root2, &zip).unwrap();
        assert_eq!(id2, "demo-2");
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn reject_unsafe_names() {
        assert!(!safe_zip_name("../evil"));
        assert!(!safe_zip_name("/abs/path"));
        assert!(!safe_zip_name("a\\b"));
        assert!(!safe_zip_name("C:/x"));
        assert!(!safe_zip_name("preset//x"));
        assert!(safe_zip_name("preset/agent.cordis.yml"));
        assert!(safe_zip_name("preset/skills/demo.md"));
    }
}
