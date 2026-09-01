//! 客户端本地设置（config.json，应用数据目录下）

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// 启动时自动检查 DSH 内核更新（默认开）
    pub auto_check_update: bool,
    /// npm registry 覆盖（默认官方源；国内可设 https://registry.npmmirror.com）
    pub registry: Option<String>,
    /// dsh web 端口覆盖（默认 0 = 系统动态分配）
    pub port: Option<u16>,
    /// 用户选择“跳过此版本”的 dsh 版本（自动检查时不再提示；手动检查忽略）
    pub skipped_version: Option<String>,
    /// home 模式："isolated"（默认，隔离 dsh-home）/ "system"（直接用 ~/.dsh）
    pub home_mode: Option<String>,
    /// 首启向导是否已完成（导入配置 / 保持隔离 / 使用系统目录）
    pub setup_done: bool,
    /// 启动时把系统 ~/.dsh 凭据单向复制进隔离凭据（默认开；safe-mode 停用）
    pub sync_credentials: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            auto_check_update: true,
            registry: None,
            port: None,
            skipped_version: None,
            home_mode: None,
            setup_done: false,
            sync_credentials: true,
        }
    }
}

impl Settings {
    pub fn load(data_dir: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let path = data_dir.join("config.json");
        if path.exists() {
            let raw = std::fs::read_to_string(&path)?;
            Ok(serde_json::from_str(&raw).unwrap_or_default())
        } else {
            Ok(Self::default())
        }
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
        let path = data_dir.join("config.json");
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
}
