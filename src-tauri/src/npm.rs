//! pnpm 执行封装：全部经运行时 node（系统优先 / 缓存 / 按需下载）运行资源目录里的纯 JS CLI。
//! 用户机器上不需要预装 node/npm，且与系统环境完全隔离（P3 不再打包 node sidecar）。
//!
//! - dsh 的隔离环境安装/更新用 **pnpm**（并行下载 + 内容寻址 store，比 npm 快数倍；
//!   且 dsh 官方插件机制本身就用 pnpm）。
//! - 版本查询（pnpm view）等短命令同样走 pnpm CLI。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tauri::{AppHandle, Manager};
use tauri_plugin_shell::process::CommandEvent;
use tauri_plugin_shell::ShellExt;

use crate::process_mgr::ensure_runtime_node;

/// 资源目录里的 pnpm.cjs（scripts/fetch-node.mjs 从 npm 镜像拉取，纯 JS）
pub fn pnpm_cli_path(app: &AppHandle) -> Result<PathBuf, String> {
    resource_path(app, &["pnpm", "bin", "pnpm.cjs"])
}

fn resource_path(app: &AppHandle, parts: &[&str]) -> Result<PathBuf, String> {
    let res = app.path().resource_dir().map_err(|e| e.to_string())?;
    let mut p = res;
    for part in parts {
        p = p.join(part);
    }
    Ok(p)
}

/// 构造 `node <script> <args...>` 命令（shell 插件的 Command 是 builder，方法消费 self）。
/// P3：node 用运行时 node（ensure_runtime_node），不再依赖 sidecar。
/// 环境统一经 process_mgr::build_child_env 注入 PATH（node 启动器），
/// 保证 pnpm 的 postinstall 生命周期脚本能找到 node。
async fn build_script_command(
    app: &AppHandle,
    script: &Path,
    args: &[String],
    envs: &HashMap<String, String>,
) -> Result<tauri_plugin_shell::process::Command, String> {
    let node = ensure_runtime_node(app, &|_| {}).await?;
    let envs = crate::process_mgr::build_child_env(app, envs)?;
    let mut all = vec![script.to_string_lossy().into_owned()];
    all.extend_from_slice(args);
    let cmd = app.shell().command(&node).args(all).envs(envs);
    Ok(cmd)
}

/// 一次性执行并收集输出（用于 `pnpm view` 这类短命令）
pub async fn run_pnpm_view(
    app: &AppHandle,
    args: &[String],
    envs: &HashMap<String, String>,
) -> Result<(i32, String, String), String> {
    let script = pnpm_cli_path(app)?;
    let cmd = build_script_command(app, &script, args, envs).await?;
    let out = cmd
        .output()
        .await
        .map_err(|e| format!("pnpm 执行失败: {e}"))?;
    let code = out.status.code().unwrap_or(-1);
    Ok((
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// 递归统计目录大小（字节）
pub fn dir_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(cur) = stack.pop() {
        if let Ok(rd) = std::fs::read_dir(&cur) {
            for entry in rd.flatten() {
                let p = entry.path();
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        stack.push(p);
                    } else if let Ok(md) = entry.metadata() {
                        total += md.len();
                    }
                }
            }
        }
    }
    total
}

/// 流式执行 pnpm（隔离环境安装/更新 dsh）。
/// - 逐行回调输出（兼容 \r 与 \n）；
/// - 每 2 秒回调 progress_dir 目录增长量，供前端展示真实进度；
/// - CommandChild 被丢弃 → stdin 关闭，避免 Windows 下子进程挂起。
pub async fn stream_pnpm<F: FnMut(String) + Send, G: FnMut(u64) + Send>(
    app: &AppHandle,
    args: &[String],
    envs: &HashMap<String, String>,
    progress_dirs: &[PathBuf],
    mut on_line: F,
    mut on_progress: G,
) -> Result<i32, String> {
    let script = pnpm_cli_path(app)?;
    let cmd = build_script_command(app, &script, args, envs).await?;
    let (mut rx, _child) = cmd.spawn().map_err(|e| format!("pnpm 启动失败: {e}"))?;
    drop(_child); // 关闭 stdin（Windows 需要，避免挂起）
    let mut code = -1;
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(2));
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Some(CommandEvent::Stdout(bytes)) | Some(CommandEvent::Stderr(bytes)) => {
                    let text = String::from_utf8_lossy(&bytes).into_owned();
                    for seg in text.split(['\n', '\r']) {
                        let line = seg.trim().to_string();
                        if !line.is_empty() {
                            on_line(line);
                        }
                    }
                }
                Some(CommandEvent::Terminated(p)) => {
                    code = p.code.unwrap_or(-1);
                    break;
                }
                Some(CommandEvent::Error(e)) => {
                    log::warn!("pnpm 事件错误: {e}");
                }
                Some(_) => {}
                None => break,
            },
            _ = interval.tick() => {
                let total: u64 = progress_dirs.iter().map(|d| dir_size(d)).sum();
                on_progress(total);
            }
        }
    }
    Ok(code)
}
