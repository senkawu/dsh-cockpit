//! dsh 子进程生命周期管理：spawn（经 tauri-plugin-shell sidecar）、
//! 端口探测、优雅停止（先 SIGTERM 再兜底强杀，且只动自己管理的进程）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use log::warn;
use tauri::async_runtime::Receiver;
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_shell::process::{CommandChild, CommandEvent};
use tauri_plugin_shell::ShellExt;

/// 我们管理的 dsh 子进程
pub struct DshProcess {
    pub child: CommandChild,
    pub pid: u32,
    pub managed: bool, // true=本程序 spawn；false=附加到外部已有实例（不做管理）
    pub port: u16,
}

/// Node 运行时版本与镜像（P3 按需下载；镜像默认 npmmirror，可用 DSH_NODE_MIRROR 覆盖）
pub const NODE_VERSION: &str = "v26.5.0";
pub const NODE_MIRROR_BASE: &str = "https://cdn.npmmirror.com/binaries/node";
pub const NODE_OFFICIAL_BASE: &str = "https://nodejs.org/dist";
/// 系统 Node 最低可接受主版本（低于则用内置/下载运行时）
pub const NODE_MIN_SYSTEM_MAJOR: u32 = 24;

/// 运行时 Node 解析结果缓存（进程生命周期内只解析一次）
static RUNTIME_NODE: std::sync::OnceLock<Result<PathBuf, String>> = std::sync::OnceLock::new();

/// 目标平台发行包目录名（Node 官方命名：darwin-arm64 / win-x64 / linux-x64 …）。
/// 注意：Rust 的 ARCH 是 x86_64/aarch64，必须映射成 Node 的 x64/arm64，
/// 否则下载 URL 404（此前误用 darwin-aarch64，CDN 永远返回 404）。
fn dist_platform_arch() -> (String, String) {
    let platform = std::env::var("DSH_TARGET_PLATFORM").unwrap_or_else(|_| {
        match std::env::consts::OS {
            "macos" => "darwin".to_string(),
            "windows" => "win".to_string(),
            other => other.to_string(),
        }
    });
    let arch = std::env::var("DSH_TARGET_ARCH").unwrap_or_else(|_| {
        match std::env::consts::ARCH {
            "x86_64" => "x64".to_string(),
            "aarch64" => "arm64".to_string(),
            other => other.to_string(),
        }
    });
    (platform, arch)
}

/// 发行包文件名与解压目录名
fn dist_archive_names() -> (String, String) {
    let (platform, arch) = dist_platform_arch();
    let dir = format!("node-{NODE_VERSION}-{platform}-{arch}");
    let file = if platform == "win" {
        format!("{dir}.zip")
    } else {
        format!("{dir}.tar.gz")
    };
    (file, dir)
}

/// 已解析的运行时 node 路径（若已解析）
pub fn runtime_node_cached() -> Option<PathBuf> {
    RUNTIME_NODE.get().and_then(|r| r.as_ref().ok().cloned())
}

/// 校验并解析运行时 node：
///   1) DSH_USE_SYSTEM_NODE=1 → 强制系统 node（<24 报错）
///   2) 系统 node ≥24 → 直接用（系统优先，设计 2.4-B）
///   3) <app_data>/node-runtime 已有缓存 → 复用
///   4) 否则从镜像下载（sha256 校验 + 进度事件），失败回退官方源
pub async fn ensure_runtime_node(
    app: &tauri::AppHandle,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<PathBuf, String> {
    if let Some(cached) = RUNTIME_NODE.get() {
        return cached.clone();
    }

    let node = resolve_runtime_node(app, on_progress).await;
    let _ = RUNTIME_NODE.set(node.clone());
    node
}

async fn resolve_runtime_node(
    app: &tauri::AppHandle,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<PathBuf, String> {
    let force_system = std::env::var("DSH_USE_SYSTEM_NODE").as_deref() == Ok("1");
    let force_download = std::env::var("DSH_FORCE_NODE_DOWNLOAD").as_deref() == Ok("1");

    // 1) 系统 node ≥24 优先（可 DSH_FORCE_NODE_DOWNLOAD=1 强制走下载，用于调试/兜底）
    if !force_download {
        if let Some(sys) = find_system_node() {
            if system_node_major(&sys) >= NODE_MIN_SYSTEM_MAJOR {
                log::info!("使用系统 Node（>=24）：{}", sys.display());
                return Ok(sys);
            }
            if force_system {
                return Err(format!(
                    "DSH_USE_SYSTEM_NODE=1 但系统 node 版本过低（{}）",
                    sys.display()
                ));
            }
        }
    }

    // 2) 已有缓存
    let runtime_dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("node-runtime");
    let (file, dir) = dist_archive_names();
    let cached_node = runtime_dir.join(&dir).join(if cfg!(windows) { "node.exe" } else { "bin/node" });
    if cached_node.is_file() {
        log::info!("使用缓存的 Node 运行时：{}", cached_node.display());
        return Ok(cached_node);
    }

    // 3) 下载（镜像优先，失败回退官方源）——下载/校验/解压都是阻塞 IO，放进 spawn_blocking
    std::fs::create_dir_all(&runtime_dir).map_err(|e| e.to_string())?;
    let archive_path = runtime_dir.join(&file);
    let task_runtime_dir = runtime_dir.clone();
    let task_archive = archive_path.clone();
    let task_cached = cached_node.clone();
    let task_file = file.clone();
    let app_clone = app.clone();
    let task_progress = move |line: String| {
        let _ = app_clone.emit("dsh-update-progress", line);
    };
    let dl = tauri::async_runtime::spawn_blocking(move || {
        let mut last_err = String::new();
        // 整体重试：镜像/官方都可能间歇 404（CDN 边缘节点同步延迟）或慢速超时，
        // 最多 5 轮、轮间退避 3s（刚发布的版本在部分边缘节点会短暂 404）
        for round in 1..=5 {
            if round > 1 {
                std::thread::sleep(std::time::Duration::from_secs(3));
                task_progress(format!("下载重试（第 {round}/5 轮）…"));
            }
            for base in [NODE_MIRROR_BASE, NODE_OFFICIAL_BASE] {
                let url = format!("{base}/{NODE_VERSION}/{task_file}");
                match download_node_archive(&url, &task_archive, &task_progress) {
                    Ok(()) => break,
                    Err(e) => {
                        last_err = format!("{base}: {e}");
                        log::warn!("Node 下载失败（{}），尝试下一镜像", last_err);
                    }
                }
            }
            if task_archive.is_file() {
                break;
            }
        }
        if !task_archive.is_file() {
            return Err(format!("Node 运行时下载失败：{last_err}"));
        }
        task_progress("Node 下载完成，解压中…".into());
        extract_archive(&task_archive, &task_runtime_dir).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(&task_archive);
        if !task_cached.is_file() {
            return Err(format!("解压后未找到 node 可执行文件：{}", task_cached.display()));
        }
        Ok::<(), String>(())
    });
    dl.await.map_err(|e| format!("Node 下载任务失败: {e}"))??;
    on_progress(format!("Node 运行时就绪：{NODE_VERSION}"));
    log::info!("Node 运行时就绪：{}", cached_node.display());
    Ok(cached_node)
}

/// 查找系统 node（PATH + 常见目录）
fn find_system_node() -> Option<PathBuf> {
    let names: &[&str] = if cfg!(windows) { &["node.exe", "node"] } else { &["node"] };
    for dir in ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"] {
        for name in names {
            let p = std::path::Path::new(dir).join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// 系统 node 主版本
fn system_node_major(node: &Path) -> u32 {
    std::process::Command::new(node)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|v| {
            v.trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|s| s.parse::<u32>().ok())
        })
        .unwrap_or(0)
}

/// 下载 Node 发行包（阻塞调用方线程；curl 断点续传 + sha256 校验 + 进度事件）。
/// 注意：调用方应处于 spawn_blocking 中。
fn download_node_archive(
    url: &str,
    dest: &Path,
    on_progress: &(dyn Fn(String) + Send + Sync),
) -> Result<(), String> {
    let (file, _) = dist_archive_names();
    // 期望 sha256：从发行目录的 SHASUMS256.txt 取（镜像优先，官方源兜底）
    let mut expected = None;
    for base in [NODE_MIRROR_BASE, NODE_OFFICIAL_BASE] {
        let checksum_url = format!("{base}/{NODE_VERSION}/SHASUMS256.txt");
        if let Some(h) = fetch_expected_sha256(&checksum_url, &file) {
            expected = Some(h);
            break;
        }
    }

    // 下载前清掉残留（上次失败可能留下 422 字节的错误页/不完整文件，
    // 否则 `-C -` 会基于垃圾文件续传，sha256 永远校验不过）
    let _ = std::fs::remove_file(dest);

    on_progress(format!("下载 Node 运行时 {NODE_VERSION}（镜像）…"));
    // --connect-timeout/--max-time 防止官方源在国内不可达时无限挂起；
    // --fail 让 HTTP 4xx/5xx 以非零退出码结束。
    // 注意：npmmirror CDN 实测对 `-C -`（断点续传）请求返回 404，
    // 因此先尝试续传，失败则删掉残留、不带 -C - 全量重下一次。
    let curl = |resume: bool, dest: &Path| {
        let mut args: Vec<String> = vec![
            "-sSL".into(),
            "--fail".into(),
            "--connect-timeout".into(),
            "10".into(),
            "--max-time".into(),
            "900".into(),
        ];
        if resume {
            args.push("-C".into());
            args.push("-".into());
        }
        args.push("-o".into());
        args.push(dest.to_string_lossy().into_owned());
        args.push(url.to_string());
        std::process::Command::new("curl")
            .args(&args)
            .output()
            .map_err(|e| format!("curl 执行失败: {e}"))
    };
    let mut output = curl(true, dest)?;
    if !output.status.success() {
        log::warn!(
            "Node 续传下载失败（code {:?}），全量重下: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        let _ = std::fs::remove_file(dest);
        output = curl(false, dest)?;
    }
    if !output.status.success() {
        let _ = std::fs::remove_file(dest); // 失败不留残渣
        return Err(format!(
            "curl 退出码 {:?}: {}",
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    // sha256 校验
    if let Some(want) = expected {
        let got = sha256_file(dest)?;
        if !got.eq_ignore_ascii_case(&want) {
            let _ = std::fs::remove_file(dest);
            return Err(format!("sha256 校验失败：期望 {want}，实际 {got}"));
        }
    } else {
        log::warn!("未取得期望 sha256（两个源均失败），跳过校验");
    }
    on_progress(format!(
        "已下载 {} 校验通过",
        dest.file_name().unwrap_or_default().to_string_lossy()
    ));
    Ok(())
}

/// 从 SHASUMS256.txt 取目标文件的期望哈希（同步；调用方应在 spawn_blocking 中）
fn fetch_expected_sha256(checksum_url: &str, file: &str) -> Option<String> {
    let out = std::process::Command::new("curl")
        .args(["-sSL", "--max-time", "30", checksum_url])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().find_map(|l| {
        let mut parts = l.split_whitespace();
        let hash = parts.next()?.to_string();
        if parts.any(|f| f == file) {
            Some(hash)
        } else {
            None
        }
    })
}

fn sha256_file(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// 解压 Node 发行包（zip 用 zip crate；tar.gz 用 tar+flate2）
fn extract_archive(archive: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| e.to_string())?;
    if archive.to_string_lossy().ends_with(".zip") {
        let file = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
        zip.extract(dest).map_err(|e| e.to_string())?;
    } else {
        let f = std::fs::File::open(archive).map_err(|e| e.to_string())?;
        let gz = flate2::read::GzDecoder::new(f);
        let mut tar = tar::Archive::new(gz);
        tar.unpack(dest).map_err(|e| e.to_string())?;
    }
    Ok(())
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

/// 运行时 node 可执行文件路径（P3：系统优先/缓存/按需下载后解析）。
/// 必须在调用过 ensure_runtime_node（或 spawn_node / build_script_command）之后使用。
pub fn sidecar_node_path() -> Result<PathBuf, String> {
    match runtime_node_cached() {
        Some(p) => Ok(p),
        None => Err("Node 运行时尚未解析（请先完成 ensure_runtime_node）".into()),
    }
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
        // 注意：必须用属性 cfg 而非 if cfg!(…)，否则两个分支都会编译，
        // Windows 上会因 std::os::unix 不存在而编译失败。
        #[cfg(windows)]
        {
            std::fs::write(&link, format!("@echo off\r\n\"{}\" %*\r\n", node.display()))
                .map_err(|e| e.to_string())?;
        }
        #[cfg(not(windows))]
        {
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

/// 用运行时 node 启动任意命令（dsh bin.js / npm / pnpm 等）。
/// P3：先确保运行时 node（系统优先 / 缓存 / 按需下载），再直接 spawn 该路径。
pub async fn spawn_node(
    app: &AppHandle,
    args: &[String],
    envs: &HashMap<String, String>,
) -> Result<(Receiver<CommandEvent>, CommandChild), String> {
    let node = ensure_runtime_node(app, &|_| {}).await?;
    let envs = build_child_env(app, envs)?;
    let mut all = vec!["--no-warnings".to_string()];
    all.extend_from_slice(args);
    let cmd = app.shell().command(&node).args(all).envs(envs);
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
            // 超时：把已收集的输出打进日志，便于诊断（此前直接丢弃导致查不到原因）
            log::error!("dsh 等待就绪超时，已收集输出：\n{}", buf.trim());
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
                // 退出前把已收集输出打出来（此前丢弃，诊断包看不到 dsh 报错）
                if !buf.trim().is_empty() {
                    log::error!(
                        "dsh 在输出就绪行前退出 code={:?}，已收集输出：\n{}",
                        p.code,
                        buf.trim()
                    );
                } else {
                    warn!("dsh 在输出就绪行前退出 code={:?}（无任何输出）", p.code);
                }
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

/// 端口是否已被占用（只测 TCP 连通，不做 HTTP 校验；800ms 超时防挂起）
pub async fn is_port_open(port: u16) -> bool {
    tokio::time::timeout(
        Duration::from_millis(800),
        tokio::net::TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map(|r| r.is_ok())
    .unwrap_or(false)
}

/// 从 dsh 就绪行 URL（`dsh web: http://127.0.0.1:<port>`）解析端口
pub fn port_from_url(url: &str) -> Option<u16> {
    url.split(':').last().and_then(|s| s.parse().ok())
}

/// 在 [start, end] 范围内找第一个空闲端口（动态端口分配用）
pub async fn find_free_port(start: u16, end: u16) -> Option<u16> {
    for port in start..=end {
        if !is_port_open(port).await {
            return Some(port);
        }
    }
    None
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
#[allow(unused_variables)]
pub async fn graceful_stop(app: &AppHandle, proc: DshProcess) -> Result<(), String> {
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
