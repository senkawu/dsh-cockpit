<div align="center">

<img src="src-tauri/icons/128x128.png" width="96" alt="DSH-Cockpit logo" />

# DSH-Cockpit

**DeepSeek Harness 桌面伴侣 · 外壳与内核完全解耦的 Tauri 2 客户端**

一个轻量、自更新的桌面外壳：随 npm 安装最新版 DSH 内核，在原生窗口里承载 DeepSeek Harness 的完整 Web 界面——无需打开终端、无需手动装 Node、无需关心环境配置。

[![构建状态](https://github.com/senkawu/dsh-cockpit/actions/workflows/build.yml/badge.svg)](https://github.com/senkawu/dsh-cockpit/actions/workflows/build.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Tauri 2](https://img.shields.io/badge/Tauri-2.x-24c8db.svg)](https://tauri.app)
[![平台](https://img.shields.io/badge/平台-macOS%20·%20Windows%20·%20Debian%20·%20RedHat-4c8bf5.svg)](#-下载安装)
[![Node 按需加载](https://img.shields.io/badge/Node-按需下载%20v26-339933.svg)](docs/upgrade-design-v0.3.md)

</div>

---

## ✨ 特性一览

| | 能力 | 说明 |
| --- | --- | --- |
| 🔌 | **内核随 npm 更新** | DSH 永远是最新版：启动时自动检查、弹窗确认后原地升级，**更新内核无需重新安装客户端** |
| 🛡️ | **更新即回退** | 升级前自动备份，冒烟测试失败即刻回滚——不会"变砖" |
| 🌐 | **国内网络友好** | 内核与插件全部走 npmmirror 镜像；GitHub 源插件可一键挂镜像加速 |
| 🐋 | **三款内置插件** | 插件市场、小鲸鱼余额挂件、用量统计面板——开箱即用，面板一键开关 |
| 🖥️ | **原生桌面体验** | 托盘驻留、关闭最小化、单实例、Dock 重新激活、崩溃自恢复 |
| 🔒 | **安全边界** | 右键 Inspect 全面屏蔽；外部链接一律转系统浏览器；`--safe-mode` 故障恢复 |
| 🇨🇳 | **零依赖开箱** | 用户无需安装 Node / pnpm / Git——Node 运行时**按需加载**：系统 Node ≥24 优先，否则自动下载到应用数据目录（sha256 校验） |

## 🏗️ 架构：外壳与内核解耦

```
┌─ DSH-Cockpit（Tauri 2 外壳）───────────────────────────┐
│  · 安装包仅含：外壳 + pnpm 纯 JS 资源（~30MB，无 Node） │
│  · 不打包 @deepseek-ai/dsh（硬性约束）                   │
└──────────────────────┬─────────────────────────────────┘
                       │ spawn（tauri-plugin-shell）
                       ▼
┌─ 隔离运行时（应用数据目录）──────────────────────────────┐
│  node-runtime/ Node 按需加载（系统优先/缓存/下载+校验）  │
│  dsh-env/     隔离 pnpm 环境（DSH 内核安装于此）          │
│  dsh-home/    隔离 DSH_HOME（profile / 凭据 / 会话）     │
│  pnpm-store/  内容寻址 store（更新秒级）                 │
└──────────────────────┬─────────────────────────────────┘
```

> 运行日志不在应用数据目录内，由 `tauri-plugin-log` 写入**系统日志目录**（macOS `~/Library/Logs/com.dshcockpit.desktop/dsh-cockpit.log`、Windows `%LOCALAPPDATA%\com.dshcockpit.desktop\logs`），面板与菜单里的「日志目录」指向的就是该位置。
                       │ node dsh/bin.js web
                       ▼
          http://127.0.0.1:<port>  ← WebView 加载官方 Web UI
```

> **为什么这样设计？** 内核与外壳各自独立升级：DSH 有更新 → npm 一键升级，外壳不用重装；
> 外壳有更新 → tauri-plugin-updater 独立升级，两者互不阻塞。

## 🚀 下载安装

安装包由 [GitHub Actions](https://github.com/senkawu/dsh-cockpit/actions) 自动构建，在每次运行结果的 **Artifacts** 中下载：

| 平台 | 产物 | 架构 |
| --- | --- | --- |
| macOS | `.dmg` | Apple Silicon（arm64）/ Intel（x64） |
| Windows | `Setup.exe`（NSIS） | x64 / arm64 |
| Debian / Ubuntu | `.deb` | x64 / arm64 |
| RedHat / Fedora | `.rpm` | x64 / arm64 |

**首次启动**：客户端自动完成 ① 安装 DSH 内核（约 6~8 秒）→ ② 安装三款内置插件 → ③ 启动服务并加载界面，全程可视化进度，无需任何手动配置。

> macOS 首次打开如提示"无法验证开发者"，请到 **系统设置 → 隐私与安全性 → 仍要打开**（未签名应用属正常流程）。

## 🧩 内置插件

| 插件 | npm 包名 | 功能 |
| --- | --- | --- |
| [dsh-market](https://github.com/dsh-market/dsh-market) 插件市场 | `dshmarket` | 可视化浏览 / 一键安装社区插件 |
| [小鲸鱼余额挂件](https://github.com/MeteorNOX/DeepSeek-Balance-Whale-Widget) | `dsh-whale-widget` | 右下角常驻：余额、今日已用、每轮消耗 |
| [用量统计面板](https://github.com/HaoyueQin/dsh-usage-statistics-panel) | `dsh-usage-statistics-panel` | Token 用量趋势 / 活动热力图 / 分模型统计 |

- 控制面板「插件」区提供**热开关**（写入官方补丁层，HMR 约 1 秒生效，无需重启）；
- 全部走 npm（npmmirror）安装，**国内无需访问 GitHub**。

## ⚙️ 配置

| 环境变量 | 说明 | 默认 |
| --- | --- | --- |
| `DSH_REGISTRY` | npm registry（国内可设 npmmirror） | `https://registry.npmmirror.com` |
| `DSH_PORT` | dsh web 端口 | `3080` |
| `DSH_TAG` | DSH 内核版本 tag | `latest` |
| `DSH_GIT_MIRROR` | GitHub 源插件的 git 镜像前缀（如 `https://ghfast.top/`） | 直连 |
| `DSH_SAFE_MODE` | 设为 `1` 以安全模式启动 | 关 |
| `DSH_USE_SYSTEM_NODE` | 设为 `1` 强制使用系统 Node（需 ≥24） | 系统 Node 自动优先 |
| `DSH_FORCE_NODE_DOWNLOAD` | 设为 `1` 强制走下载（调试/兜底） | 关 |
| `DSH_NODE_MIRROR` | Node 下载镜像（国内自动 npmmirror） | `https://cdn.npmmirror.com/binaries/node` |

> 鲸鱼挂件余额需在**隔离** home 的凭据文件（`<app_data>/dsh-home/.credentials.yaml`）配置 `DEEPSEEK_API_KEY`，与终端里的 `~/.dsh` 互不影响。

## 🛠️ 本地构建（开发者）

```bash
npm install
npx tauri build --bundles app,dmg   # macOS
npx tauri build --bundles nsis      # Windows（需 Windows 机器）
npx tauri build --bundles deb,rpm   # Linux（需 Linux 机器）
```

`beforeBuildCommand` 会自动从镜像下载 pnpm 纯 JS 资源到 `src-tauri/pnpm/`（幂等，已就绪即跳过）。

> **Node 运行时为何不打包？** 安装包因此从 ~150MB 瘦身到 ~30MB。首次启动若检测到系统 Node ≥24 直接复用；否则自动从镜像下载 v26.5.0 到 `<app_data>/node-runtime/`（sha256 校验 + 断点续传 + 官方源兜底），仅首次需要联网。

## 🧠 桌面宿主能力

- **更新策略**：启动后延迟+抖动后台检查，每 6 小时复查；三选一「立即更新 / 跳过此版本 / 稍后」，跳过项持久化，手动检查不受影响
- **进程治理**：退出时优雅停止自管的 dsh 子进程（SIGTERM → 兜底强杀）；**绝不误杀外部 dsh**（端口占用时走"附加模式"）
- **安全模式**：`--safe-mode` 以仅含官方核心 bundle 的隔离 profile 启动，用于排查第三方插件故障；面板可一键退出
- **窗口恢复**：主窗口意外销毁后自动重建（限频防抖）；Dock 点击 / 托盘点击均可靠唤回
- **导航守卫**：仅允许加载客户端页面与本机服务，外部链接一律交系统浏览器
- **环境注入**：解析用户登录 shell 的完整环境并注入 dsh 子进程（Finder 启动也能找到 Homebrew / mise 等工具）
- **体验细节**：渐变流光加载进度条 + 可折叠日志、右键 Inspect 全面屏蔽、托盘常驻

## 📁 目录结构

```text
dsh-cockpit/
├── .github/workflows/build.yml   # 全平台一键构建
├── scripts/fetch-node.mjs        # 构建期：下载 pnpm 纯 JS 资源（幂等）
├── src/                          # 前端（无框架静态页）
└── src-tauri/                    # Tauri 2 外壳（Rust）
    ├── pnpm/                     # 构建产物（pnpm 资源，不提交）
    └── src/
        ├── lib.rs                # 入口、窗口、托盘、菜单、更新检查
        ├── dsh.rs                # DSH 内核管理：安装/更新/回退/冒烟/插件开关
        ├── npm.rs                # 经运行时 node 执行 pnpm
        ├── process_mgr.rs        # 子进程生命周期 + 端口探测 + Node 按需加载
        └── commands.rs           # 控制面板 IPC
```

## 🙏 致谢

- **[DeepSeek Harness](https://github.com/deepseek-ai)**（[@deepseek-ai](https://github.com/deepseek-ai)）：本项目驱动的 DSH 内核（`@deepseek-ai/dsh`）及全部运行时生态
- [dataelement/dsh-desktop](https://github.com/dataelement/dsh-desktop)：桌面宿主设计参考（环境注入、更新策略、安全模式、窗口恢复等）
- 内置插件作者：[dsh-market](https://github.com/dsh-market/dsh-market) · [MeteorNOX](https://github.com/MeteorNOX) · [HaoyueQin](https://github.com/HaoyueQin)

## ⚠️ 已知风险

1. **DSH 上游变更**：大版本可能调整 CLI / 端口 / 存储格式——更新走"询问 → 备份 → 冒烟 → 失败回退"，不会变砖
2. **网络波动**：版本查询 / 安装失败仅提示不阻塞，继续使用已装版本；registry 可随时切换镜像
3. **强杀兜底**：SIGTERM 后 3 秒未退出将 SIGKILL，极端情况下会话落盘可能不完整
4. **平台差异**：Windows 需 WebView2（Win10/11 自带）；Linux 包需在 Linux 上构建

## 📄 License

[MIT](LICENSE) © senkawu
