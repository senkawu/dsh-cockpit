# dsh-desktop-tauri — deepseek harness 桌面客户端（Tauri 2）

外壳与 DSH 内核**完全解耦**的桌面客户端：

- **Tauri 安装包只含**：外壳二进制 + node 运行时 sidecar + npm/pnpm 纯 JS 资源；
  **不打包 @deepseek-ai/dsh**（硬性约束）。
- **DSH 内核**装在系统应用数据目录的隔离 pnpm 环境，从 npm 源拉取 latest，
  更新 DSH 无需重新发布安装包；外壳用 tauri-plugin-updater 独立升级。

## 目录结构

```text
dsh-desktop-tauri/
├── package.json              # @tauri-apps/cli（构建工具）
├── scripts/
│   └── fetch-node.mjs        # 构建时：下载 node → sidecar；拆出 npm/pnpm → 资源
├── src/                      # 前端（无框架静态页，withGlobalTauri）
│   ├── status.html/js        # 主窗口启动状态页（安装/更新/启动进度）
│   ├── index.html + panel.js # 控制面板（版本、检查更新、重启、设置）
│   └── style.css
└── src-tauri/
    ├── Cargo.toml            # tauri 2 + 插件（shell/single-instance/dialog/log/updater）
    ├── tauri.conf.json       # 双窗口、externalBin(node)、resources(npm/pnpm)
    ├── capabilities/default.json
    ├── binaries/             # 构建产物：node-aarch64-apple-darwin 等（sidecar）
    ├── npm/  pnpm/           # 构建产物：npm-cli.js / pnpm.cjs（纯 JS 资源）
    └── src/
        ├── main.rs / lib.rs  # 入口、单实例、托盘、窗口、后台引导
        ├── dsh.rs            # 内核管理器：隔离环境安装/更新/回退/冒烟/启动
        ├── npm.rs            # 经 sidecar node 执行 npm/pnpm
        ├── process_mgr.rs    # 子进程生命周期 + 端口探测 + 优雅停止
        ├── commands.rs       # 控制面板 IPC 命令
        ├── settings.rs       # config.json（自动检查更新开关等）
        └── tray.rs           # 托盘与窗口显示
```

## 隔离目录（应用数据目录 <app_data>）

| 目录 | 作用 |
| --- | --- |
| `dsh-env/` | 隔离 pnpm 环境（dsh 安装在此；含 .npmrc / pnpm-workspace.yaml） |
| `dsh-home/` | 隔离的 DSH_HOME（profile/凭据/会话，与用户 ~/.dsh 互不干扰） |
| `pnpm-store/` | pnpm 内容寻址 store（安装/更新加速，跨版本复用） |
| `npm-cache/` | npm 缓存（`npm view` 版本查询用） |
| `logs/` | 运行日志（tauri-plugin-log） |

## 构建

```bash
npm install                 # 安装 @tauri-apps/cli
npm run build               # tauri build（release + dmg；beforeBuild 自动准备 sidecar）
npx tauri build --debug     # 快速调试构建（生成 .app）
```

构建前提：

- **Rust** 工具链（rustup），macOS 需 Xcode CLT；Windows 需在 Windows 上构建（MSVC + WebView2）。
- `beforeBuildCommand` 自动运行 `scripts/fetch-node.mjs`：
  - 从 `cdn.npmmirror.com`（可 `DSH_NODE_MIRROR` 覆盖）下载 Node v26.5.0（`DSH_NODE_VERSION` 可覆盖）；
  - node 可执行文件 → `binaries/node-<triple>`（externalBin sidecar）；
  - npm（node 发行包内）→ `npm/`；pnpm（npm 镜像 tgz）→ `pnpm/`（纯 JS 资源）。
  - 幂等：产物与版本标记一致时跳过。
- 运行环境变量（测试/多实例）：`DSH_PORT`、`DSH_REGISTRY`、`DSH_TAG`、
  `DSH_DESKTOP_NODE_VERSION/MIRROR`（构建期）。

## 运行流程

1. 启动 → 单实例锁（重复启动聚焦主窗口）→ 初始化隔离目录 → 托盘。
2. 若 `dsh-env` 无 dsh → **pnpm install**（实测 6~8 秒装完整个依赖树）。
3. 自动检查更新（可关）：`npm view @deepseek-ai/dsh version` vs 已安装版本
   （semver 比较）→ 有新版本弹窗询问「立即更新 / 稍后」（不强制）。
4. 更新流程：优雅停止子进程 → 备份 dsh-env + dsh-home（.bak）→ pnpm install
   → 冒烟测试（临时端口启动 + HTTP 探测）→ 失败自动回退 .bak。
5. 启动 dsh web（`--port 0`/配置端口，DSH_HOME 隔离）→ 轮询端口就绪 →
   主窗口导航到 `http://127.0.0.1:<port>`。
6. 端口被占用（如用户外部已跑 dsh）→ 附加模式：直接加载、不管理、退出不杀。

## 进程与退出语义

- 完全退出（托盘「完全退出」/ 面板按钮）：先 SIGTERM 优雅停止**自己管理**的 dsh
  子进程，3 秒后兜底强杀；`app.exit(0)` 前还有 Exit 事件兜底清理。
- **不杀外部 dsh 进程**：只按我们 spawn 的 pid 管理；端口占用时走附加模式。
- 主窗口关闭 → 隐藏到托盘；托盘菜单：显示窗口 / 控制面板 / 重启 dsh 服务 / 完全退出。
- dsh 崩溃 → 事件 + 日志（面板可一键重启）。

## 两套更新

| 对象 | 机制 | 触发 |
| --- | --- | --- |
| DSH 内核 | npm 源 pnpm 安装/更新，隔离环境 | 启动自动检查（可关）+ 面板手动 |
| Tauri 外壳 | tauri-plugin-updater | 面板「检查外壳更新」 |

> 外壳更新需先配置 `plugins.updater.endpoints` 与签名公钥（`tauri signer` 生成），
> 并发布到你的更新服务器；未配置时按钮返回友好提示。

## 内置插件（三个，全部走 npm 源，国内无需访问 GitHub）

首次启动会在隔离 DSH_HOME 的 profile 里自动安装三个插件（幂等，失败不阻塞启动）：

| 插件 | npm 包名 | 开关 id |
| --- | --- | --- |
| [dsh-market](https://github.com/dsh-market/dsh-market) 插件市场 | `dshmarket` | `dsh-market` |
| [DeepSeek-Balance-Whale-Widget](https://github.com/MeteorNOX/DeepSeek-Balance-Whale-Widget) 小鲸鱼余额挂件 | `dsh-whale-widget`（0.2.10 起已发布 npm） | `dsh-whale-widget` |
| [dsh-usage-statistics-panel](https://github.com/HaoyueQin/dsh-usage-statistics-panel) 用量统计面板 | `dsh-usage-statistics-panel` | `usage-stats` |

**国内 GitHub 访问不畅的处理**：
- 三个内置插件**全部从 npm registry 安装**（默认 npmmirror），完全不经过 GitHub；
- 兜底：未来若需安装 `github:` 源插件，设 `DSH_GIT_MIRROR`（如 `https://ghfast.top/`），
  客户端用 git 的 env 级 `insteadOf` 把 github.com 透明重写到镜像（不动用户全局 git 配置）；
- 插件市场里安装社区 github 插件时同样可用该镜像（走同一 PATH 环境）。

控制面板「插件」区有三个**开关**（持久化到 profile 的 `cordis.patch.yml`，
写 `- id: X` + `disabled: true|false`，与 dsh-plugin-hub / dsh-market 同款官方补丁机制）：
dsh 的 HMR 约 1 秒内热生效，**无需重启**。安装依赖 pnpm——客户端自动生成
`<app_data>/bin/pnpm` shim（指向内置 node + pnpm.cjs），用户机器无需预装 pnpm
（`github:` 源插件仍需系统 git：macOS 自带，Windows 需装 [Git](https://git-scm.com)）。

> 鲸鱼挂件余额需要 DeepSeek API Key：配在**隔离** home 的凭据里
> （`<app_data>/dsh-home/.credentials.yaml` 的 `DEEPSEEK_API_KEY`），
> 与控制面板/终端里的 `~/.dsh` 互不影响。插件本身也可以在 GUI 里更新。

## 打包目标（Mac / Windows / Debian / RedHat）

`bundle.targets = "all"`，在各平台本机构建对应格式（Tauri 的 Linux 包必须在 Linux 上构建）：

| 平台 | 命令 | 产物 |
| --- | --- | --- |
| macOS（Apple Silicon / Intel） | `npx tauri build --bundles app,dmg` | `.app` + `.dmg` |
| Windows（x64 / arm64） | `npx tauri build --bundles nsis`（Windows 机器） | `Setup.exe` |
| Debian / Ubuntu | `npx tauri build --bundles deb`（Linux 机器） | `.deb` |
| RedHat / Fedora | `npx tauri build --bundles rpm`（Linux 机器） | `.rpm` |

Linux 构建机依赖：Rust、`libwebkit2gtk-4.1-dev`、`libgtk-3-dev`、`librsvg2-dev`、
`libappindicator3-dev`、`libsoup-3.0-dev`（deb 运行依赖已声明在 `tauri.conf.json`；
rpm 另需 `rpm-build`）。sidecar（node）与 npm/pnpm 资源由 `beforeBuildCommand`
按平台自动准备（Linux 用 `node-vX-linux-x64/aarch64` 发行包，目标 triple
`x86_64/aarch64-unknown-linux-gnu` 已支持）。

## 融合自 dataelement/dsh-desktop 的能力

参考 [dataelement/dsh-desktop](https://github.com/dataelement/dsh-desktop) 并移植其可落地的优点
（其"把 dsh 打进安装包"的打包路线与本项目硬性约束冲突，未采纳）：

- **用户环境注入**：GUI（Finder/托盘）启动时 PATH 精简，客户端用登录 shell（macOS/Linux
  `$SHELL -l -i -c env`，Windows 带 `$PROFILE` 的 PowerShell）解析一次用户完整环境，
  注入 dsh 子进程及其 bash 工具（Homebrew/mise 等路径可见）。
- **更新策略**：启动后延迟 10~20s（带 jitter）后台检查更新，之后每 6h 复查；
  弹窗三选一「立即更新 / 跳过此版本 / 稍后」——"跳过此版本"持久化（config.json），
  自动检查不再打扰，面板手动检查忽略跳过。
- **安全模式**：`--safe-mode`（或 `DSH_SAFE_MODE=1`）→ 使用隔离的全新 DSH_HOME
  （`dsh-home-safe`，仅官方核心 bundle），跳过插件安装与更新检查；面板显示安全模式
  徽章 +「退出安全模式并重启」。
- **主窗口恢复**：主窗口意外销毁后自动重建（5s 冷却、最多 3 次），dsh 已就绪则直接导航。
- **导航守卫**：窗口只允许加载客户端页面与本机 127.0.0.1；外部链接一律转系统浏览器，
  `window.open`/`target=_blank` 全部拦截。
- **补丁层安全写入**：插件开关写 `cordis.patch.yml` 前备份、写后 serde_yaml 校验，
  非法即回滚（绝不把用户补丁文件写坏，同 dataelement 的 patch-layer 原则）。

> Tauri 限制无法移植的（参考项目有但 Electron 专属）：原生右键菜单、GPU 进程崩溃
> 自动 reload、Chromium cookie 清理、launchd 守护/自启（可用 tauri-plugin-autostart
> 后续补）、插件级故障定位 UI。

## 安全与体验

- **屏蔽 Inspect**：窗口级 `devtools(false)` + 每次页面加载注入 JS 拦截（右键菜单、
  F12 / Cmd+Opt+I / Ctrl+Shift+I 全部无效），覆盖状态页、控制面板与 dsh 页面。
- **酷炫加载页**：渐变流光进度条（按下载 MB 实时换算 + 阶段推进），附「查看日志」
  折叠按钮，点击才展开完整安装/更新日志，平时保持简洁。
- **git 傻瓜式**：内置插件全部走 npm（npmmirror），**用户无需安装 Git**；仅当未来
  安装 `github:` 源插件且系统缺 git 时，客户端弹窗一键打开 Git 官方下载页引导。

## 一键构建全平台（GitHub Actions）

已提供 `.github/workflows/build.yml`：推送到 GitHub 后手动 Run workflow（或 push
触发），自动产出 **macOS arm64/x64（dmg）、Windows x64/arm64（NSIS）、Debian（deb）、
RedHat（rpm）** 六个安装包，在每次运行的 Artifacts 里下载。Windows/Linux 的包
必须在对应平台构建（Tauri 平台限制），本工作流即"一站式"解决方案。

## 已知风险

1. **dsh 上游 breaking change**：dsh 大版本升级可能改变 CLI 参数/端口/存储格式。
   缓解：更新走「询问→备份→冒烟→失败回退」，冒烟失败自动回滚，不会变砖。
2. **网络失败**：版本查询/安装失败只报错不阻塞（使用已装版本）；registry 可配
   npmmirror（`DSH_REGISTRY` / config.json）。
3. **npm 11 vs 12 行为差异**：隔离环境用 pnpm 安装（避开 npm 的慢解析与脚本拦截）；
   `npm view` 仍用内置 npm。pnpm 的构建脚本白名单写在 pnpm-workspace.yaml（allowBuilds）。
4. **pnpm hoisted 布局**：与 dsh profile 的 nodeLinker: hoisted 一致；若未来 dsh
   改为非 hoisted 依赖方式，需同步调整 `.npmrc`。
5. **子进程强杀**：SIGTERM 后 3 秒兜底 SIGKILL，极端情况下 dsh 的会话落盘可能不完整。
6. **Windows**：需 WebView2（Win10/11 自带），不支持 Win7（按要求）；交叉编译需
   在 Windows 机器上构建（MSVC）。
7. **附加模式端口占用**：若 3080 被非 dsh 服务占用，客户端会直接加载该页面——
   属“不杀外部进程”需求的妥协，日志会告警。

## 与参考项目（dataelement/dsh-desktop）的区别

参考项目把 dsh 以 tgz 形式**内置进安装包**（`file:packages/...`），违反本项目的
硬性约束（禁止把 dsh 打进安装包）；本项目坚持「外壳 + dsh 内核解耦、内核随 npm
更新」的架构，并针对国内网络把安装器从 npm 换成 pnpm（实测同树安装从 8 分钟+
降到 6~8 秒）。
