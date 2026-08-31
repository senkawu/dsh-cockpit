//! dsh 子进程生命周期管理：spawn（经 tauri-plugin-shell sidecar）、
//! 端口探测、优雅停止（先 SIGTERM 再兜底强杀，且只动自己管理的进程）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use log::warn;
use tauri::async_runtime::Receiver;
use tauri::{AppHandle, Manager};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// 我们管理的 dsh 子进程
pub struct DshProcess {
    pub child: CommandChild,
    pub pid: u32,
    pub managed: bool, // true=本程序 spawn；false=附加到外部已有实例（不做管理）
    pub port: u16,
}

/// 当前平台的 Rust target triple（与 tauri externalBin sidecar 命名一致）
pub fn target_triple() -> &'static str {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("aarch64", "macos") => "aarch64-apple-darwin",
        ("x86_64", "macos") => "x86_64-apple-darwin",
        ("x86_64", "windows") => "x86_64-pc-windows-msvc",
        ("aarch64", "windows") => "aarch64-pc-windows-msvc",
        ("x86_64", "linux") => "x86_64-unknown-linux-gnu",
        ("aarch64", "linux") => "aarch64-unknown-linux-gnu",
        (a, o) => {
            log::warn!("未识别的平台 {a}-{o}，sidecar 可能缺失");
            "unknown"
        }
    }
}

/// sidecar node 可执行文件的绝对路径。
/// 开发模式文件名为 `node-<triple>`；**打包后 tauri 会去掉 triple 后缀**（裸名 `node`），
/// 两种都要兼容，否则首次安装 dsh 时 ensure_node_launcher 找不到文件而失败。
pub fn sidecar_node_path() -> Result<PathBuf, String> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .ok_or("无法定位可执行目录")?;
    let triple = target_triple();
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let candidates = [
        exe_dir.join(format!("node-{triple}{suffix}")), // 开发/调试
        exe_dir.join(format!("node{suffix}")),          // 打包后（tauri 剥离 triple）
    ];
    for c in &candidates {
        if c.exists() {
            return Ok(c.clone());
        }
    }
    Err(format!(
        "sidecar node 不存在（已尝试: {}）",
        candidates
            .iter()
            .map(|c| c.display().to_string())
            .collect::<Vec<_>>()
            .join(" / ")
    ))
}

/// 在 <app_data>/bin 下创建名为 `node` 的启动器（unix 符号链接 / Windows .cmd），
/// 指向 sidecar node。pnpm 的 postinstall 生命周期脚本（koffi/node-pty 等）按
/// PATH 找 `node`，而 sidecar 文件名带 triple 后缀，必须提供这个裸名入口。
pub fn ensure_node_launcher(app: &AppHandle) -> Result<PathBuf, String> {
    let bin_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("bin");
    std::fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
    let node = sidecar_node_path()?;
    let link = bin_dir.join(if cfg!(windows) { "node.cmd" } else { "node" });
    if !link.exists() {
        if cfg!(windows) {
            std::fs::write(&link, format!("@echo off\r\n\"{}\" %*\r\n", node.display()))
                .map_err(|e| e.to_string())?;
        } else {
            std::os::unix::fs::symlink(&node, &link).map_err(|e| e.to_string())?;
        }
    }
    Ok(bin_dir)
}

/// 为子进程构造环境：在传入 env 基础上注入 PATH（<app_data>/bin node 启动器 +
/// 系统目录 + 继承值），并确保 `node` 启动器存在。
/// 所有 spawn 路径（dsh 启动 / npm / pnpm / 插件安装）都必须走这里，
/// 否则 Finder/托盘启动（PATH 极简）时 pnpm 的 postinstall 脚本报
/// `sh: node: command not found`，koffi/node-pty 等原生模块装不上。
pub fn build_child_env(
    app: &AppHandle,
    envs: &HashMap<String, String>,
) -> Result<HashMap<String, String>, String> {
    let mut envs = envs.clone();
    let launcher_dir = ensure_node_launcher(app)?;
    let sep = if cfg!(windows) { ";" } else { ":" };
    let sys_dirs = if cfg!(windows) {
        r"%SystemRoot%\System32;%SystemRoot%"
    } else {
        "/usr/local/bin:/usr/bin:/bin"
    };
    let inherited = envs
        .get("PATH")
        .cloned()
        .or_else(|| std::env::var("PATH").ok())
        .unwrap_or_default();
    let path_val = if inherited.is_empty() {
        format!("{}{sep}{sys_dirs}", launcher_dir.display())
    } else {
        format!("{}{sep}{sys_dirs}{sep}{inherited}", launcher_dir.display())
    };
    envs.insert("PATH".into(), path_val);
    Ok(envs)
}

/// 用 sidecar node 启动任意命令（dsh bin.js / npm / pnpm 等）
pub async fn spawn_node(
    app: &AppHandle,
    args: &[String],
    envs: &HashMap<String, String>,
) -> Result<(Receiver<CommandEvent>, CommandChild), String> {
    let envs = build_child_env(app, envs)?;
    let mut all = vec!["--no-warnings".to_string()];
    all.extend_from_slice(args);
    let cmd = app
        .shell()
        .sidecar("node")
        .map_err(|e| format!("解析 sidecar node 失败: {e}"))?
        .args(all)
        .envs(envs);
    let (rx, child) = cmd.spawn().map_err(|e| format!("spawn node 失败: {e}"))?;
    Ok((rx, child))
}

/// 等待 dsh 就绪行 `dsh web: http://127.0.0.1:<port>`（stdout 可能分包，需累积匹配）
pub async fn wait_for_url(rx: &mut Receiver<CommandEvent>, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut buf = String::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return None;
        }
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(CommandEvent::Stdout(bytes))) | Ok(Some(CommandEvent::Stderr(bytes))) => {
                buf.push_str(&String::from_utf8_lossy(&bytes));
                if let Some(m) = url_regex().captures(&buf) {
                    return Some(m[1].to_string());
                }
            }
            Ok(Some(CommandEvent::Terminated(p))) => {
                warn!("dsh 在输出就绪行前退出 code={:?}", p.code);
                return None;
            }
            Ok(Some(CommandEvent::Error(e))) => {
                warn!("dsh 子进程事件错误: {e}");
            }
            Ok(None) => return None,
            Err(_) => return None, // 超时
            _ => {}
        }
    }
}

fn url_regex() -> regex::Regex {
    regex::Regex::new(r"dsh web:\s+(https?://\S+)").unwrap()
}

/// 端口是否已被占用（只测 TCP 连通，不做 HTTP 校验）
pub async fn is_port_open(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
}

/// 等待端口可访问（HTTP GET / 返回 200 即视为就绪）
pub async fn wait_port_ready(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if http_probe(port).await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// 简单 HTTP 探测：GET /，读到响应头以 HTTP/1.x 200 开头
async fn http_probe(port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(_) => return false,
    };
    let req = format!("GET / HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    match tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buf)).await {
        Ok(Ok(n)) if n > 0 => {
            let head = String::from_utf8_lossy(&buf[..n]);
            head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
        }
        _ => false,
    }
}

/// 优雅停止：先 SIGTERM（unix 用 kill 命令，win 用 node process.kill），
/// 等 3 秒后仍未退出再强杀（插件 kill 在 unix 上是 SIGKILL）。
/// 注意：CommandChild::kill(self) 会消费掉 child，因此这里接收所有权。
pub async fn graceful_stop(_app: &AppHandle, proc: DshProcess) -> Result<(), String> {
    let pid = proc.pid;
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status();
    }
    #[cfg(windows)]
    {
        let _ = spawn_node(
            app,
            &["-e".into(), format!("process.kill({pid}, 'SIGTERM')")],
            &HashMap::new(),
        )
        .await;
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    let _ = proc.child.kill();
    Ok(())
}
