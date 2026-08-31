//! DSH 内核管理器：隔离 npm 环境中的 dsh 安装 / 更新 / 回退 / 冒烟 / 启动。
//!
//! 目录布局（应用数据目录 <app_data> 下，macOS: ~/Library/Application Support/…，
//! Windows: %APPDATA%\…）：
//!   dsh-env/      隔离 npm 环境（package.json + node_modules，dsh 安装在此）
//!   dsh-home/     隔离的 DSH_HOME（profile / 凭据 / 会话，与用户 ~/.dsh 互不干扰）
//!   npm-cache/    客户端自己的 npm 缓存
//!   logs/         运行日志（tauri-plugin-log 写入）
//!
//! 更新策略（关键安全逻辑）：
//!   停止子进程 → 备份 dsh-env 与 dsh-home（.bak）→ npm install @latest
//!   → 冒烟测试（临时端口启动 + HTTP 探测）→ 成功则正式启动
//!   → 任一环节失败则自动回退 .bak，避免“变砖”。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_shell::process::CommandEvent;

use crate::npm;
use crate::process_mgr::{self, DshProcess};
use crate::settings::Settings;

pub const DSH_PACKAGE: &str = "@deepseek-ai/dsh";

/// 内置插件：插件市场 / 小鲸鱼余额挂件 / 用量统计面板。
/// 三者均已发布到 npm（registry 默认走 npmmirror，国内无需访问 GitHub）。
pub const PLUGIN_MARKET: &str = "dsh-market"; // bundle 行 id
pub const PLUGIN_MARKET_PKG: &str = "dshmarket"; // npm 包名
pub const PLUGIN_WHALE: &str = "dsh-whale-widget"; // bundle 行 id == npm 包名
pub const PLUGIN_WHALE_SRC: &str = "dsh-whale-widget"; // npm 包名（0.2.10 起已发布到 npm）
pub const PLUGIN_USAGE: &str = "usage-stats"; // bundle 行 id
pub const PLUGIN_USAGE_PKG: &str = "dsh-usage-statistics-panel"; // npm 包名

/// npm 12 的 allowScripts 白名单：dsh 依赖树里需要跑安装脚本的原生包。
/// 裸包名 = 按名字允许任意版本（防 npm 12 默认拦截导致 koffi.node 缺失）。
const DSH_ALLOWED_SCRIPTS: &[&str] = &[
    "koffi",                                  // FFI，dsh-fs-local / dsh-subprocess-local 依赖
    "node-pty",                               // 终端
    "protobufjs",                             // @google/genai 依赖
    "@google/genai",
    "@deepseek-ai/dsh-subprocess-local",      // 生成 spawn helper
    "esbuild",
    "unrs-resolver",
    "@parcel/watcher",
    "sharp",
    "@tailwindcss/oxide",
];

#[derive(Clone)]
pub struct DshManager {
    pub data_dir: PathBuf,      // 应用数据目录
    pub env_dir: PathBuf,       // 隔离 npm 环境（dsh 安装目录）
    pub home_dir: PathBuf,      // 隔离的 DSH_HOME
    pub cache_dir: PathBuf,     // npm 缓存
    pub store_dir: PathBuf,     // pnpm 内容寻址 store（安装/更新加速）
    pub log_dir: PathBuf,       // 运行日志目录
    pub registry: String,       // npm registry（默认官方源；可配置 npmmirror 等）
    pub tag: String,            // dsh 版本 tag，默认 latest
    pub port: u16,              // dsh web 端口，默认 3080
    pub auto_check_update: Arc<AtomicBool>, // 启动时自动检查更新开关（控制面板可改）
    pub safe_mode: Arc<AtomicBool>,         // 安全模式：仅官方核心 bundle，跳过插件/更新
    pub skipped_version: Arc<Mutex<Option<String>>>, // 用户跳过的 dsh 版本（自动检查不再提示）
    pub last_status: Arc<Mutex<Option<serde_json::Value>>>, // 最近一次状态（页面加载后可回放，防事件早于监听丢失）
    pub process: Arc<Mutex<Option<DshProcess>>>, // 我们管理的 dsh 子进程
}

/// 返回给前端的状态载荷
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct StatusPayload {
    pub installed: Option<String>,
    pub running: bool,
    pub port: u16,
    pub env_dir: String,
    pub home_dir: String,
    pub registry: String,
    pub auto_check_update: bool,
    pub safe_mode: bool,
    pub skipped_version: Option<String>,
    pub log_dir: String,
    pub app_version: String,
}

impl DshManager {
    /// 由应用数据目录构造管理器（目录不存在则创建）
    pub fn new(app: &AppHandle) -> Result<Self, Box<dyn std::error::Error>> {
        let data_dir = app.path().app_data_dir()?;

        // 迁移旧版（更名前 com.deepseek.dsh.desktop）数据：插件/会话/凭据/缓存一次性搬过来，
        // 否则改 identifier 后旧目录被"遗弃"，用户会发现插件不见了。
        migrate_legacy_data(&data_dir);

        let env_dir = data_dir.join("dsh-env");
        let home_dir = data_dir.join("dsh-home");
        let cache_dir = data_dir.join("npm-cache");
        let store_dir = data_dir.join("pnpm-store");
        let log_dir = data_dir.join("logs");
        for d in [&env_dir, &home_dir, &cache_dir, &store_dir, &log_dir] {
            std::fs::create_dir_all(d)?;
        }

        let settings = Settings::load(&data_dir)?;
        // 环境变量可覆盖配置（便于测试/多实例）。默认 npmmirror——国内主场景，
        // 官方源在境内访问极慢（曾实测单请求 14~28s）；可用 DSH_REGISTRY 改回官方。
        let registry = std::env::var("DSH_REGISTRY")
            .ok()
            .or(settings.registry)
            .unwrap_or_else(|| "https://registry.npmmirror.com".into());
        let port = std::env::var("DSH_PORT")
            .ok()
            .and_then(|p| p.parse().ok())
            .or(settings.port)
            .unwrap_or(3080);
        let tag = std::env::var("DSH_TAG").unwrap_or_else(|_| "latest".into());

        Ok(Self {
            data_dir,
            env_dir,
            home_dir,
            cache_dir,
            store_dir,
            log_dir,
            registry,
            tag,
            port,
            auto_check_update: Arc::new(AtomicBool::new(settings.auto_check_update)),
            safe_mode: Arc::new(AtomicBool::new(false)),
            skipped_version: Arc::new(Mutex::new(settings.skipped_version)),
            last_status: Arc::new(Mutex::new(None)),
            process: Arc::new(Mutex::new(None)),
        })
    }

    /// dsh 已安装版本（读隔离环境的 package.json）
    pub fn installed_version(&self) -> Option<String> {
        let pkg = self.env_dir.join("node_modules").join(DSH_PACKAGE).join("package.json");
        let raw = std::fs::read_to_string(pkg).ok()?;
        let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
        json.get("version")?.as_str().map(|s| s.to_string())
    }

    /// dsh 可执行入口（bin.js）
    pub fn dsh_bin(&self) -> Option<PathBuf> {
        let pkg = self.env_dir.join("node_modules").join(DSH_PACKAGE).join("package.json");
        let raw = std::fs::read_to_string(pkg).ok()?;
        let json: serde_json::Value = serde_json::from_str(&raw).ok()?;
        let bin = json.get("bin")?;
        let rel = if let Some(s) = bin.as_str() {
            s.to_string()
        } else {
            bin.get("dsh")?.as_str()?.to_string()
        };
        Some(self.env_dir.join("node_modules").join(DSH_PACKAGE).join(rel))
    }

    /// npm 侧配置环境（隔离缓存 + registry + pnpm store）
    pub fn npm_envs(&self) -> std::collections::HashMap<String, String> {
        let mut env = std::collections::HashMap::new();
        env.insert("npm_config_cache".into(), self.cache_dir.to_string_lossy().into_owned());
        env.insert("npm_config_registry".into(), self.registry.clone());
        env.insert("npm_config_store_dir".into(), self.store_dir.to_string_lossy().into_owned());
        env.insert("npm_config_update_notifier".into(), "false".into());
        env.insert("npm_config_fund".into(), "false".into());
        env.insert("npm_config_audit".into(), "false".into());
        env.insert("NO_COLOR".into(), "1".into());
        env
    }

    /// 预写隔离环境的配置文件：
    /// - package.json（dsh 依赖 + npm12 的 allowScripts 白名单）
    /// - .npmrc（registry / pnpm store-dir / node-linker=hoisted）
    /// - pnpm-workspace.yaml（pnpm11 的 allowBuilds 构建脚本白名单，原生包必需）
    pub fn write_manifest(&self) {
        std::fs::create_dir_all(&self.env_dir).ok();
        let allow = DSH_ALLOWED_SCRIPTS
            .iter()
            .map(|s| (s.to_string(), serde_json::Value::Bool(true)))
            .collect::<serde_json::Map<String, serde_json::Value>>();
        let manifest = serde_json::json!({
            "name": "dsh-isolated-env",
            "private": true,
            "dependencies": { DSH_PACKAGE: self.tag },
            "allowScripts": allow,
        });
        std::fs::write(
            self.env_dir.join("package.json"),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .ok();

        // pnpm 配置（node-linker=hoisted 与 dsh profile 布局一致）。
        // 注意：store-dir 不放 .npmrc——应用数据目录含空格（Application Support），
        // 会破坏 npmrc 解析导致整份配置被忽略；store 改由 CLI --config 传入（见 install_args）。
        let npmrc = format!("registry={}\nnode-linker=hoisted\nfetch-retries=3\n", self.registry);
        std::fs::write(self.env_dir.join(".npmrc"), npmrc).ok();

        // pnpm11 构建脚本白名单（防止 koffi 等原生包脚本被默认拦截）
        // nodeLinker: hoisted 与 dsh profile 布局一致（pnpm11 只认 workspace 里的 camelCase）
        let allow_builds = DSH_ALLOWED_SCRIPTS
            .iter()
            .map(|s| format!("  {}: true", serde_json::to_string(s).unwrap()))
            .collect::<Vec<_>>()
            .join("\n");
        let workspace = format!(
            "packages: []\nnodeLinker: hoisted\nallowBuilds:\n{allow_builds}\nonlyBuiltDependencies:\n{}\n",
            DSH_ALLOWED_SCRIPTS
                .iter()
                .map(|s| format!("  - {}", serde_json::to_string(s).unwrap()))
                .collect::<Vec<_>>()
                .join("\n")
        );
        std::fs::write(self.env_dir.join("pnpm-workspace.yaml"), workspace).ok();
    }

    /// 查询 npm 官方源上的最新版本
    pub async fn latest_version(&self, app: &AppHandle) -> Result<String, String> {
        let args = vec!["view".into(), DSH_PACKAGE.to_string(), "version".into()];
        let (code, stdout, stderr) = npm::run_npm(app, &args, &self.npm_envs()).await?;
        if code != 0 {
            return Err(format!("npm view 失败(code={code}): {}", stderr.trim()));
        }
        // `npm view xxx version` 可能输出多行（多版本时取最后一行）
        Ok(stdout.trim().lines().last().unwrap_or("").trim().to_string())
    }

    /// 构造 pnpm 安装参数：`-C <env> install`（在隔离环境目录执行，读取其 .npmrc / pnpm-workspace.yaml）。
    /// registry 与 store-dir 用 CLI `--config` 显式传入——比 .npmrc/env 更可靠，
    /// 且 store 路径含空格也不会影响解析。用 pnpm 而非 npm：实测同一依赖树
    /// npm 8 分钟装不完、pnpm 6 秒装完（并行下载 + store）。
    fn install_args(&self) -> Vec<String> {
        vec![
            "-C".into(),
            self.env_dir.to_string_lossy().into_owned(),
            "install".into(),
            format!("--config.registry={}", self.registry),
            format!("--config.store-dir={}", self.store_dir.to_string_lossy()),
        ]
    }

    /// 下载进度监控目录：pnpm store（下载写入处）+ 隔离环境（reify 写入处）
    fn progress_dirs(&self) -> Vec<PathBuf> {
        vec![self.store_dir.clone(), self.env_dir.clone()]
    }

    /// 是否有可用更新（semver 比较，带预发布支持）。
    /// `manual=true`（用户在面板手动检查）时忽略“跳过此版本”，
    /// 让用户可以主动收回跳过的版本。
    pub fn has_update(&self, latest: &str, manual: bool) -> bool {
        let installed = match self.installed_version() {
            Some(v) => v,
            None => return true,
        };
        let newer = match (
            semver::Version::parse(&installed),
            semver::Version::parse(latest),
        ) {
            (Ok(a), Ok(b)) => b > a,
            _ => latest != installed, // 解析失败时按字符串不等处理
        };
        if !newer {
            return false;
        }
        if !manual {
            if let Some(skipped) = self.skipped_version.lock().unwrap().as_deref() {
                if skipped == latest {
                    return false; // 用户跳过了这个版本，自动检查不再打扰
                }
            }
        }
        true
    }

    /// 持久化“跳过此版本”（写 config.json）
    pub fn set_skipped_version(&self, version: Option<String>) -> Result<(), String> {
        let data_dir = self.data_dir.clone();
        let mut settings = crate::settings::Settings::load(&data_dir).unwrap_or_default();
        settings.skipped_version = version.clone();
        settings.save(&data_dir).map_err(|e| format!("保存跳过版本失败: {e}"))?;
        *self.skipped_version.lock().unwrap() = version;
        Ok(())
    }

    /// 首次安装 dsh（只装 latest，不做版本对比）
    pub async fn ensure_installed(&self, app: &AppHandle) -> Result<String, String> {
        self.write_manifest();
        self.emit_status(app, "installing", &format!("正在安装 {}@{}（隔离 npm 环境）…", DSH_PACKAGE, self.tag), None);
        let app_line = app.clone();
        let app_prog = app.clone();
        let progress_dirs = self.progress_dirs();
        let mut last_mb = 0u64;
        let start = std::time::Instant::now();
        let code = npm::stream_pnpm(
            app,
            &self.install_args(),
            &self.npm_envs(),
            &progress_dirs,
            move |line| {
                info!("pnpm: {line}");
                let _ = app_line.emit("dsh-update-progress", line);
            },
            move |bytes| {
                // 真实下载进度（pnpm 非 TTY 不输出进度条，用 store+env 目录增长量替代）；
                // 缓存不变时（元数据/解压阶段）改报耗时，避免界面“卡死”观感
                let mb = bytes / 1024 / 1024;
                let secs = start.elapsed().as_secs();
                if mb != last_mb {
                    last_mb = mb;
                    let _ = app_prog.emit(
                        "dsh-update-progress",
                        format!("下载依赖中… 已下载 {mb} MB（用时 {secs}s）"),
                    );
                } else {
                    let _ = app_prog.emit(
                        "dsh-update-progress",
                        format!("处理依赖中… 已用 {secs}s（缓存 {mb} MB）"),
                    );
                }
            },
        )
        .await?;
        if code != 0 {
            return Err(format!("pnpm install 失败（code={code}）"));
        }
        self.installed_version().ok_or_else(|| "安装后未找到 dsh 版本信息".into())
    }

    /// 更新 dsh 内核：停止 → 备份 → install @latest → 冒烟 → 失败回退
    pub async fn update(&self, app: &AppHandle) -> Result<String, String> {
        // 1) 优雅停止正在运行的 dsh 子进程（只动我们自己管理的）
        self.stop(app).await?;

        // 2) 更新前备份：隔离 npm 环境 + DSH_HOME（profile）
        self.emit_status(app, "updating", "更新前备份当前环境与配置…", None);
        backup_dir(&self.env_dir);
        backup_dir(&self.home_dir);

        // 3) 安装 latest
        self.write_manifest();
        self.emit_status(app, "updating", "正在更新 dsh 内核…", None);
        let app_line = app.clone();
        let app_prog = app.clone();
        let progress_dirs = self.progress_dirs();
        let mut last_mb = 0u64;
        let start = std::time::Instant::now();
        let code = npm::stream_pnpm(
            app,
            &self.install_args(),
            &self.npm_envs(),
            &progress_dirs,
            move |line| {
                info!("pnpm: {line}");
                let _ = app_line.emit("dsh-update-progress", line);
            },
            move |bytes| {
                let mb = bytes / 1024 / 1024;
                let secs = start.elapsed().as_secs();
                if mb != last_mb {
                    last_mb = mb;
                    let _ = app_prog.emit(
                        "dsh-update-progress",
                        format!("下载依赖中… 已下载 {mb} MB（用时 {secs}s）"),
                    );
                } else {
                    let _ = app_prog.emit(
                        "dsh-update-progress",
                        format!("处理依赖中… 已用 {secs}s（缓存 {mb} MB）"),
                    );
                }
            },
        )
        .await?;
        if code != 0 {
            let msg = format!("npm install 失败（code={code}）");
            self.rollback(app);
            return Err(msg);
        }
        let new_ver = match self.installed_version() {
            Some(v) => v,
            None => {
                self.rollback(app);
                return Err("安装后无法读取版本信息，已回退".into());
            }
        };

        // 4) 冒烟测试：临时端口启动 + HTTP 探测，确保新版本真的能跑
        self.emit_status(app, "updating", &format!("新版本 {new_ver} 安装完成，执行冒烟测试…"), Some(&new_ver));
        if let Err(e) = self.smoke_test(app).await {
            self.rollback(app);
            return Err(format!("冒烟测试失败（{e}），已回退到上一个版本"));
        }
        info!("冒烟测试通过: {new_ver}");
        Ok(new_ver)
    }

    /// 冒烟测试：临时端口（0 = 系统分配）启动 dsh web，解析 stdout 的 URL 并探测
    async fn smoke_test(&self, app: &AppHandle) -> Result<(), String> {
        let bin = self.dsh_bin().ok_or("dsh 入口缺失")?;
        let (mut rx, child) = process_mgr::spawn_node(
            app,
            &[
                bin.to_string_lossy().into_owned(),
                "web".into(),
                "--no-open".into(),
                "--port".into(),
                "0".into(),
            ],
            &self.dsh_envs(),
        )
        .await?;
        let pid = child.pid();

        // 解析 "dsh web: http://127.0.0.1:<port>" 就绪行
        let url = process_mgr::wait_for_url(&mut rx, std::time::Duration::from_secs(60)).await;
        let _ = child.kill();
        match url {
            Some(u) => {
                let port = u
                    .split(':')
                    .last()
                    .and_then(|s| s.parse::<u16>().ok())
                    .ok_or("无法解析就绪 URL")?;
                // 等端口真正可访问
                if process_mgr::wait_port_ready(port, std::time::Duration::from_secs(20)).await {
                    Ok(())
                } else {
                    Err(format!("端口 {port} 探测超时（pid={pid}）"))
                }
            }
            None => Err(format!("冒烟启动超时（pid={pid}）")),
        }
    }

    /// 回退：恢复 .bak 备份（更新失败时调用，避免程序变砖）
    pub fn rollback(&self, app: &AppHandle) {
        info!("执行回退…");
        restore_dir(&self.env_dir);
        restore_dir(&self.home_dir);
        self.emit_status(app, "error", "已回退到上一个可用版本", None);
    }

    /// 启动 dsh web 服务（端口占用 → 附加到已有实例，不管理、不杀掉外部进程）
    pub async fn start(&self, app: &AppHandle) -> Result<(), String> {
        // 端口已被占用：可能是用户外部手动启动的 dsh（或其它服务）。
        // 按需求“不要杀掉用户外部手动启动的 dsh 进程”，这里只附加、不做进程管理。
        if process_mgr::is_port_open(self.port).await {
            warn!("端口 {} 已被占用，附加到已有服务（不做进程管理）", self.port);
            self.emit_status(app, "ready", &format!("已连接到 127.0.0.1:{}", self.port), None);
            self.navigate_main(app);
            return Ok(());
        }

        let bin = self.dsh_bin().ok_or("dsh 入口缺失，请先安装/更新 dsh")?;
        self.emit_status(app, "starting", &format!("正在启动 dsh web (127.0.0.1:{})…", self.port), None);
        let (mut rx, child) = process_mgr::spawn_node(
            app,
            &[
                bin.to_string_lossy().into_owned(),
                "web".into(),
                "--no-open".into(),
                "--port".into(),
                self.port.to_string(),
            ],
            &self.dsh_envs(),
        )
        .await?;
        let pid = child.pid();
        let dsh_proc = DshProcess {
            child,
            pid,
            managed: true,
            port: self.port,
        };
        *self.process.lock().unwrap() = Some(dsh_proc);

        // 崩溃监听：轮询事件直到进程终止（前几个事件通常是 stdout 启动日志）
        let app2 = app.clone();
        let manager2 = self.clone();
        tauri::async_runtime::spawn(async move {
            loop {
                match rx.recv().await {
                    Some(CommandEvent::Terminated(payload)) => {
                        let code = payload.code.unwrap_or(-1);
                        error!("dsh 子进程退出 code={code}");
                        *manager2.process.lock().unwrap() = None;
                        if code != 0 {
                            manager2.emit_status(&app2, "crashed", &format!("dsh 服务已停止（退出码 {code}）"), None);
                        }
                        break;
                    }
                    Some(_) => continue, // stdout/stderr 等，继续等待
                    None => break,
                }
            }
        });

        // 等待端口就绪（最多 90s）
        if process_mgr::wait_port_ready(self.port, std::time::Duration::from_secs(90)).await {
            info!("dsh web 就绪: http://127.0.0.1:{}", self.port);
            self.emit_status(app, "ready", &format!("http://127.0.0.1:{}", self.port), None);
            self.navigate_main(app);
            Ok(())
        } else {
            error!("dsh 启动超时，清理子进程");
            self.stop(app).await?;
            Err(format!("dsh 服务启动超时（127.0.0.1:{}）", self.port))
        }
    }

    /// 停止我们自己管理的 dsh 子进程（先 SIGTERM 优雅退出，超时再强杀）
    pub async fn stop(&self, app: &AppHandle) -> Result<(), String> {
        // 用块语句立即释放 MutexGuard（if-let 的临时值会活到分支结束，跨 await 会非 Send）
        let proc = { self.process.lock().unwrap().take() };
        if let Some(proc) = proc {
            process_mgr::graceful_stop(app, proc).await?;
            self.emit_status(app, "stopped", "dsh 服务已停止", None);
        }
        Ok(())
    }

    /// 退出兜底：同步尽力清理（app.run 的 Exit 事件里调用，不能 await）
    pub fn kill_managed_now(&self) {
        if let Some(proc) = self.process.lock().unwrap().take() {
            let _ = proc.child.kill();
        }
    }

    /// 实际使用的 DSH_HOME：安全模式下切到全新隔离目录（仅官方核心 bundle，恢复用）
    pub fn active_home(&self) -> PathBuf {
        if self.safe_mode.load(Ordering::Relaxed) {
            self.data_dir.join("dsh-home-safe")
        } else {
            self.home_dir.clone()
        }
    }

    /// dsh 子进程环境变量（DSH_HOME 隔离到应用数据目录；
    /// 额外注入用户登录 shell 的完整环境——GUI 启动时 PATH 精简，
    /// dsh 及其 bash 工具需要 Homebrew/mise 等路径，参考 dataelement/dsh-desktop 的做法）
    fn dsh_envs(&self) -> std::collections::HashMap<String, String> {
        // 顺序很重要：先用户 shell 环境，再客户端覆盖项（npm 配置 / DSH_HOME 优先级最高）
        let mut env: std::collections::HashMap<String, String> = resolve_shell_env().clone();
        for (k, v) in self.npm_envs() {
            env.insert(k, v);
        }
        env.insert("DSH_HOME".into(), self.active_home().to_string_lossy().into_owned());
        env
    }

    /// 主窗口导航到 dsh web UI
    pub fn navigate_main(&self, app: &AppHandle) {
        if let Some(win) = app.get_webview_window("main") {
            let url = format!("http://127.0.0.1:{}", self.port);
            if let Err(e) = win.navigate(url.parse().unwrap()) {
                error!("导航失败: {e}");
            }
        }
    }

    /// 向前端广播 dsh 状态（并缓存最近一条，供页面加载后回放）
    pub fn emit_status(&self, app: &AppHandle, state: &str, message: &str, version: Option<&str>) {
        let payload = serde_json::json!({
            "state": state,
            "message": message,
            "version": version,
            "port": self.port,
        });
        *self.last_status.lock().unwrap() = Some(payload.clone());
        let _ = app.emit("dsh-status", payload);
    }

    /// 最近一次状态（状态页加载时先拉一次，避免启动早期事件早于页面监听而丢失）
    pub fn last_status(&self) -> Option<serde_json::Value> {
        self.last_status.lock().unwrap().clone()
    }

    /// 控制面板展示用的状态
    pub fn status_payload(&self) -> StatusPayload {
        StatusPayload {
            installed: self.installed_version(),
            running: self.process.lock().unwrap().is_some(),
            port: self.port,
            env_dir: self.env_dir.to_string_lossy().into_owned(),
            home_dir: self.active_home().to_string_lossy().into_owned(),
            registry: self.registry.clone(),
            auto_check_update: self.auto_check_update.load(Ordering::Relaxed),
            safe_mode: self.safe_mode.load(Ordering::Relaxed),
            skipped_version: self.skipped_version.lock().unwrap().clone(),
            log_dir: self.log_dir.to_string_lossy().into_owned(),
            app_version: String::new(), // 由 commands::get_status 用 package_info 填充
        }
    }
}

// ---------------------------------------------------------------------------
// 插件管理：dsh-market 插件市场 + 小鲸鱼余额挂件
// ---------------------------------------------------------------------------

/// 插件信息（控制面板展示/开关）
#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub enabled: bool,
}

impl DshManager {
    /// 隔离 profile 目录：<dsh-home>/profiles/web（安全模式下用隔离安全 home）
    pub fn profile_dir(&self) -> PathBuf {
        self.active_home().join("profiles").join("web")
    }

    /// 用户补丁层文件（HMR 监听的官方开关机制）
    pub fn patch_path(&self) -> PathBuf {
        self.profile_dir().join("cordis.patch.yml")
    }

    /// 插件是否已安装（profile package.json dependencies 里有对应包）
    fn plugin_installed(&self, pkg: &str) -> bool {
        let raw = match std::fs::read_to_string(self.profile_dir().join("package.json")) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let json: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return false,
        };
        json.pointer("/dependencies").and_then(|d| d.get(pkg)).is_some()
    }

    /// 插件启用状态：补丁层存在 `- id: X` + `disabled: true` 行 → 禁用；否则启用
    pub fn plugin_enabled(&self, id: &str) -> bool {
        let raw = match std::fs::read_to_string(self.patch_path()) {
            Ok(r) => r,
            Err(_) => return true,
        };
        let lines: Vec<&str> = raw.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim();
            if let Some(rid) = t.strip_prefix("- id: ") {
                if rid.trim() == id {
                    let next = lines.get(i + 1).map(|s| s.trim()).unwrap_or("");
                    return !next.starts_with("disabled: true");
                }
            }
        }
        true // 无补丁行 = 启用
    }

    /// 设置插件开关：往 cordis.patch.yml 写/删 `- id: X\n  disabled: true|false` 行。
    /// 这是 dsh 官方补丁层机制（与 dsh-plugin-hub / dsh-market 同款），
    /// profile 文件 watcher（HMR）约 1 秒内重组合 loader，无需重启。
    ///
    /// 安全化（参考 dataelement/dsh-desktop 的 patch-layer 原则）：
    /// 写入前备份，写入后用 serde_yaml 校验仍是合法列表，非法则回滚，绝不写坏文件。
    pub fn set_plugin_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        use std::io::Write;
        let path = self.patch_path();
        let raw = std::fs::read_to_string(&path).unwrap_or_default();

        // 解析现有补丁为 YAML 值列表（非合法列表视为空，但绝不把坏文件写得更糟）
        let mut rows: Vec<serde_yaml::Value> = match serde_yaml::from_str(&raw) {
            Ok(serde_yaml::Value::Sequence(seq)) => seq,
            Ok(serde_yaml::Value::Null) => Vec::new(),
            _ => {
                return Err(format!(
                    "补丁层不是合法 YAML 列表（已跳过写入，文件未改动）: {}",
                    path.display()
                ));
            }
        };

        // 去掉目标 id 的既有行（`- id: X` 的 map）
        rows.retain(|row| {
            !(row.get("id").and_then(|v| v.as_str()) == Some(id))
        });

        // 需要禁用 → 追加 `- id: X` + `disabled: true`
        if !enabled {
            rows.push(serde_yaml::from_str::<serde_yaml::Value>(&format!(
                "- id: {id}\n  disabled: true\n"
            ))
            .map_err(|e| format!("构造补丁行失败: {e}"))?);
        }

        // 备份 + 写新内容 + 校验回滚
        let backup = format!("{}.bak", path.display());
        std::fs::copy(&path, &backup).ok();
        let new_text = if rows.is_empty() {
            "[]\n".to_string()
        } else {
            let mut out = Vec::new();
            serde_yaml::to_writer(&mut out, &rows).map_err(|e| format!("序列化补丁失败: {e}"))?;
            let mut s = String::from_utf8_lossy(&out).into_owned();
            if !s.ends_with('\n') {
                s.push('\n');
            }
            s
        };
        if let Err(e) = std::fs::File::create(&path).and_then(|mut f| f.write_all(new_text.as_bytes())) {
            return Err(format!("写入补丁层失败: {e}"));
        }
        // 写回后自检：必须仍是合法列表
        if serde_yaml::from_str::<serde_yaml::Value>(&new_text)
            .map(|v| !v.is_sequence())
            .unwrap_or(true)
        {
            std::fs::copy(&backup, &path).ok(); // 回滚
            return Err("补丁写入后校验失败，已回滚".into());
        }
        log::info!("插件开关: {id} → {}", if enabled { "启用" } else { "禁用" });
        Ok(())
    }

    /// 确保 pnpm 可执行 shim（dsh plugin 命令按 PATH 找 pnpm）。
    /// shim 指向 bundled node + bundled pnpm.cjs，用户机器无需预装 pnpm。
    fn ensure_pnpm_shim(&self, app: &AppHandle) -> Result<PathBuf, String> {
        let bin_dir = self.data_dir.join("bin");
        std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
        let node = crate::process_mgr::sidecar_node_path()?;
        let pnpm_cjs = crate::npm::pnpm_cli_path(app)?;
        let shim = bin_dir.join(if cfg!(windows) { "pnpm.cmd" } else { "pnpm" });
        let content = if cfg!(windows) {
            format!("@echo off\r\n\"{}\" \"{}\" %*\r\n", node.display(), pnpm_cjs.display())
        } else {
            format!("#!/bin/sh\nexec \"{}\" \"{}\" \"$@\"\n", node.display(), pnpm_cjs.display())
        };
        if !shim.exists() || std::fs::read_to_string(&shim).ok() != Some(content.clone()) {
            std::fs::write(&shim, content).map_err(|e| e.to_string())?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755));
            }
        }
        Ok(bin_dir)
    }

    /// 执行 `dsh plugin …`（在隔离 DSH_HOME 下，PATH 前置 pnpm shim）
    async fn dsh_plugin(&self, app: &AppHandle, args: &[String]) -> Result<i32, String> {
        // git 傻瓜式处理：github: 源插件需要系统 git；缺失时先弹窗引导（内置插件全走 npm，不涉及）
        if args.iter().any(|a| a.starts_with("github:")) && !git_available() {
            use tauri_plugin_dialog::{DialogExt, MessageDialogButtons};
            let app2 = app.clone();
            app.dialog()
                .message(
                    "安装 GitHub 源插件需要 Git（内置插件已全部走 npm，不需要）。\n\
                     是否打开 Git 官方下载页？安装后重试即可。",
                )
                .title("需要 Git")
                .buttons(MessageDialogButtons::OkCustom("打开下载页".into()))
                .show(move |_| {
                    crate::tray::open_external("https://git-scm.com/download");
                    let _ = app2;
                });
            return Err("缺少 Git（已提示用户）".into());
        }

        let bin = self.dsh_bin().ok_or("dsh 未安装")?;
        let shim_dir = self.ensure_pnpm_shim(app)?;
        let mut envs = self.dsh_envs();
        // PATH 显式构造：shim + 系统目录 + 继承值。
        // GUI 应用（尤其 Finder/托盘启动）PATH 很精简，pnpm 解析 github: 源需要 git(/usr/bin)。
        let sep = if cfg!(windows) { ";" } else { ":" };
        let sys_dirs = if cfg!(windows) {
            r"%SystemRoot%\System32;%SystemRoot%"
        } else {
            "/usr/local/bin:/usr/bin:/bin"
        };
        let inherited = envs.get("PATH").cloned().unwrap_or_default();
        let path_val = if inherited.is_empty() {
            format!("{}{sep}{sys_dirs}", shim_dir.display())
        } else {
            format!("{}{sep}{sys_dirs}{sep}{inherited}", shim_dir.display())
        };
        envs.insert("PATH".into(), path_val);

        // 国内 GitHub 兜底：若设置了 DSH_GIT_MIRROR（如 https://ghfast.top/），
        // 用 git 的 env 级 insteadOf 把 github.com 透明重写到镜像（不动用户全局 git 配置）。
        // 内置插件已全部走 npm（npmmirror），此兜底供未来 github: 源插件或用户自装使用。
        if let Ok(mirror) = std::env::var("DSH_GIT_MIRROR") {
            if !mirror.is_empty() {
                envs.insert("GIT_CONFIG_COUNT".into(), "1".into());
                envs.insert(
                    "GIT_CONFIG_KEY_0".into(),
                    format!("url.{mirror}https://github.com/.insteadOf"),
                );
                envs.insert("GIT_CONFIG_VALUE_0".into(), "https://github.com/".into());
            }
        }
        let mut args_all = vec![
            bin.to_string_lossy().into_owned(),
            "plugin".into(),
            "--profile".into(),
            "web".into(),
        ];
        // 显式传入 registry 与 store-dir（保证国内镜像生效，且路径含空格也安全）
        args_all.push(format!("--config.registry={}", self.registry));
        args_all.push(format!("--config.store-dir={}", self.store_dir.to_string_lossy()));
        args_all.extend_from_slice(args);
        let (mut rx, child) = crate::process_mgr::spawn_node(app, &args_all, &envs).await?;
        let _ = child; // 插件安装命令跑完即结束
        // 等待退出码（stdout/stderr 转发给前端）
        loop {
            match rx.recv().await {
                Some(tauri_plugin_shell::process::CommandEvent::Stdout(b))
                | Some(tauri_plugin_shell::process::CommandEvent::Stderr(b)) => {
                    let text = String::from_utf8_lossy(&b).into_owned();
                    for seg in text.split(['\n', '\r']) {
                        let line = seg.trim().to_string();
                        if !line.is_empty() {
                            log::info!("dsh plugin: {line}");
                            let _ = app.emit("dsh-update-progress", line);
                        }
                    }
                }
                Some(tauri_plugin_shell::process::CommandEvent::Terminated(p)) => {
                    return Ok(p.code.unwrap_or(-1));
                }
                Some(tauri_plugin_shell::process::CommandEvent::Error(e)) => {
                    log::warn!("dsh plugin 事件错误: {e}");
                }
                Some(_) => {}
                None => return Ok(-1),
            }
        }
    }

    /// 安装内置插件（插件市场 + 小鲸鱼挂件 + 用量统计面板）。
    /// 全部走 npm registry（默认 npmmirror，国内无需访问 GitHub）；幂等：已装则跳过。
    /// 失败不阻塞（插件是可选增强，dsh 仍可正常启动）。
    pub async fn install_plugins(&self, app: &AppHandle) {
        let jobs: &[(&str, &str, &str)] = &[
            (PLUGIN_MARKET_PKG, PLUGIN_MARKET_PKG, "插件市场"),
            (PLUGIN_WHALE_SRC, PLUGIN_WHALE, "小鲸鱼余额挂件"),
            (PLUGIN_USAGE_PKG, PLUGIN_USAGE_PKG, "用量统计面板"),
        ];
        for (pkg, dep_name, label) in jobs {
            if self.plugin_installed(dep_name) {
                continue;
            }
            self.emit_status(app, "installing", &format!("正在安装插件：{label}…"), None);
            match self.dsh_plugin(app, &["add".into(), (*pkg).into()]).await {
                Ok(0) => log::info!("{label}安装完成"),
                Ok(c) => log::warn!("{label}安装退出码 {c}"),
                Err(e) => log::warn!("{label}安装失败（不阻塞启动）: {e}"),
            }
        }
    }

    /// 控制面板插件列表
    pub fn plugins_info(&self) -> Vec<PluginInfo> {
        vec![
            PluginInfo {
                id: PLUGIN_MARKET.into(),
                name: "DSH 插件市场".into(),
                installed: self.plugin_installed(PLUGIN_MARKET_PKG),
                enabled: self.plugin_enabled(PLUGIN_MARKET),
            },
            PluginInfo {
                id: PLUGIN_WHALE.into(),
                name: "小鲸鱼余额挂件".into(),
                installed: self.plugin_installed(PLUGIN_WHALE),
                enabled: self.plugin_enabled(PLUGIN_WHALE),
            },
            PluginInfo {
                id: PLUGIN_USAGE.into(),
                name: "用量统计面板".into(),
                installed: self.plugin_installed(PLUGIN_USAGE_PKG),
                enabled: self.plugin_enabled(PLUGIN_USAGE),
            },
        ]
    }
}

/// 系统 git 是否可用（github: 源插件安装需要）
fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 解析用户登录 shell 的完整环境（GUI 启动 PATH 精简，dsh 子进程需要用户 PATH）。
/// 移植自 dataelement/dsh-desktop 的 resolveShellEnvironment：
/// - macOS/Linux：`$SHELL -l -i -c env`（同时加载 .zprofile 与 .zshrc，拿到
///   Homebrew/mise/~/.local/bin 等路径）；
/// - Windows：带 $PROFILE 的 PowerShell 导出全量环境变量。
/// 失败/超时返回空 Map（保持继承环境，不阻断）。结果进程生命周期内只解析一次。
pub fn resolve_shell_env() -> &'static std::collections::HashMap<String, String> {
    static CACHE: std::sync::OnceLock<std::collections::HashMap<String, String>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| {
        let mut captured = std::collections::HashMap::new();
        if cfg!(windows) {
            // PowerShell：加载用户 profile 后导出全量环境（简单版，含编码修正）
            let script = "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8; . $PROFILE 2>$null; Get-ChildItem Env: | ForEach-Object { \"$($_.Name)=$($_.Value)\" }";
            if let Some(out) = run_capture("powershell", &["-NoLogo", "-NonInteractive", "-OutputFormat", "Text", "-Command", script]) {
                merge_env_output(&mut captured, &out);
            }
        } else {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
            if let Some(out) = run_capture(&shell, &["-l", "-i", "-c", "env"]) {
                merge_env_output(&mut captured, &out);
            }
        }
        captured
    })
}

/// 带超时地运行一条命令并捕获 stdout（登录 shell 可能被 profile 拖慢，10s 超时）。
fn run_capture(program: &str, args: &[&str]) -> Option<String> {
    let child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = child
            .wait_with_output()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        let _ = tx.send(out);
    });
    match rx.recv_timeout(std::time::Duration::from_secs(10)) {
        Ok(out) => Some(out),
        Err(_) => {
            log::warn!("shell 环境解析超时（{program}），使用继承环境");
            None
        }
    }
}

/// 解析 `NAME=VALUE` 输出并合并；跳过无 '=' 或名字非法的行。
fn merge_env_output(target: &mut std::collections::HashMap<String, String>, output: &str) {
    for line in output.lines() {
        let Some(eq) = line.find('=') else { continue };
        let (name, value) = (&line[..eq], &line[eq + 1..]);
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            continue;
        }
        // 丢弃解码损坏的值（U+FFFD 说明 shell 输出编码与假设不符）
        if value.contains('\u{fffd}') {
            continue;
        }
        target.insert(name.to_string(), value.to_string());
    }
}

/// 迁移旧版应用数据（更名前的 identifier：com.deepseek.dsh.desktop）。
/// 仅当新数据目录不存在/为空、且旧目录存在时执行，把插件/会话/凭据/缓存整体搬过来。
fn migrate_legacy_data(new_dir: &Path) {
    if new_dir.exists() && std::fs::read_dir(new_dir).map(|mut d| d.next().is_some()).unwrap_or(true) {
        return; // 新目录已有数据，不迁移
    }
    let legacy = match new_dir.parent() {
        Some(p) => p.join("com.deepseek.dsh.desktop"),
        None => return,
    };
    if !legacy.exists() {
        return;
    }
    log::info!("检测到旧版数据目录，正在迁移: {:?} → {:?}", legacy, new_dir);
    let _ = std::fs::create_dir_all(new_dir);
    for entry in std::fs::read_dir(&legacy).ok().into_iter().flatten().flatten() {
        let from = entry.path();
        let to = new_dir.join(entry.file_name());
        if from.is_dir() {
            if let Err(e) = copy_dir_recursive(&from, &to) {
                log::warn!("迁移目录失败 {:?}: {e}", from);
            }
        } else if let Err(e) = std::fs::copy(&from, &to) {
            log::warn!("迁移文件失败 {:?}: {e}", from);
        }
    }
    log::info!("旧版数据迁移完成（插件/会话/凭据已保留）");
}

/// 递归复制目录（备份/迁移用）
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else if file_type.is_symlink() {
            let target = std::fs::read_link(&from)?;
            #[cfg(unix)]
            std::os::unix::fs::symlink(target, &to)?;
            #[cfg(windows)]
            std::os::windows::fs::symlink_file(target, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// 备份目录：删除旧 .bak 后整目录复制为 .bak
fn backup_dir(dir: &Path) {
    let bak = PathBuf::from(format!("{}.bak", dir.display()));
    let _ = std::fs::remove_dir_all(&bak);
    if dir.exists() {
        if let Err(e) = copy_dir_recursive(dir, &bak) {
            warn!("备份 {dir:?} 失败: {e}");
        } else {
            info!("已备份 {dir:?} → {bak:?}");
        }
    }
}

/// 回退：用 .bak 覆盖当前目录
fn restore_dir(dir: &Path) {
    let bak = PathBuf::from(format!("{}.bak", dir.display()));
    if bak.exists() {
        let _ = std::fs::remove_dir_all(dir);
        if std::fs::rename(&bak, dir).is_ok() {
            info!("已从 {bak:?} 回退 {dir:?}");
        }
    }
}
