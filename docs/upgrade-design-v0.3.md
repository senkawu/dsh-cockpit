# DSH-Cockpit 0.3.0 需求设计（完整版）

> 对标参考项目 [dataelement/dsh-desktop](https://github.com/dataelement/dsh-desktop/tree/main)，为 DSH-Cockpit（Tauri 2 薄壳）做 0.3.0 功能升级设计：补齐桌面宿主能力、修复日志 Bug、规避三大短板。
> 基线：v0.2.1（隔离 pnpm 环境 + sidecar Node + 更新回退 + 安全模式 + 插件热开关 + CI 全平台发布）。

---

## 0. 设计原则

1. **薄壳解耦不破坏**：所有业务逻辑仍由 dsh 子进程承担；壳层只加"桌面宿主能力"，绝不 Fork 上游、绝不修改 Harness 内核。
2. **官方机制优先 / 插件生态兼容**：凡 dsh 已有官方扩展点（插件体系、`cordis.patch.yml` 补丁层、loopback API、HMR），一律复用。**UI 增强（对话导航条、字段汉化）以 dsh 插件形式提供**，壳层不重写 UI 逻辑；插件随 dsh 插件体系安装/热开关/卸载。
3. **短板规避是硬约束**：三大短板（Node 体积、配置隔离、UI 定制受限）在架构层解决，而非打补丁。
4. **可回退**：一切新增能力在 `--safe-mode` 下可整体关闭；升级路径保持"询问 → 备份 → 冒烟 → 失败回滚"。

---

## 1. 现状盘点：需求 × 现状 × 差距

| 需求 | DSH-Cockpit 现状（0.2.1） | 差距 |
| --- | --- | --- |
| ① 消除命令行门槛（自动管进程/空闲端口/就绪等待/异常提示） | ✅ sidecar node + 进程管理 + 就绪等待 + 状态页 | ⚠️ 端口固定 3080（未动态分配）；崩溃无一键重启对话框 |
| ② 完整继承官方能力（嵌入 WebUI 不改内核） | ✅ 完全符合 | — |
| ③ 桌面原生能力（窗口/托盘/单实例/崩溃提示/更新检查/日志可视化） | ✅ 窗口/托盘/单实例/内核更新 | ⚠️ 崩溃提示弱；日志可视化缺失；外壳自更新未闭环 |
| ④ 安全模式 | ✅ 已实现 + 面板一键退出 | 加"连续失败自动建议" |
| ⑤ Agent 资源管理（`.dshpreset` 导入导出） | ❌ 无 | 全新（见 2.2） |
| ⑥ 长对话快速导航（右侧刻度条） | ❌ 无 | 全新，插件化（见 2.5.1） |
| ⑦ 轨迹面板字段汉化 | ❌ 无（Trace 面板英文标签） | 全新，注入/插件（见 2.5.2） |
| 版本兼容校验 | ⚠️ semver + 冒烟 | 需兼容矩阵 + 更新预览 |
| 跨平台 CI + 签名 | ✅ CI 全平台；❌ 未签名 | 需签名/公证流水线 |
| **Bug：日志目录无日志文件** | ❌ **已定位根因**（见 3） | 修复中 |
| 短板① Node 体积 | ⚠️ sidecar Node 打进安装包（~145MB） | 按需下载 + 系统优先（见 2.4） |
| 短板② 配置隔离 | ❌ 隔离 `dsh-home`，与 `~/.dsh` 不互通 | 三层互通（见 2.3） |
| 短板③ UI 定制受限 | ❌ 纯嵌入 WebView | 扩展层（见 2.5） |

---

## 2. 功能设计

### 2.1 进程与生命周期强化

#### 2.1.1 动态端口分配（替代固定 3080）
- **三级策略**：
  1. 默认 `--port 0`（系统分配）：解析就绪行 URL 拿真实端口（`process_mgr::wait_for_url` 已有），端口写入 `StatusPayload`，主窗口/面板/托盘实时显示；
  2. 设置里可**固定端口**（`DSH_PORT` / 面板输入）：固定时先探测，占用则弹窗让用户选「换一个 / 附加现有服务」；
  3. **附加模式保留**：检测到外部 dsh 已在跑时提示"附加 or 另起"，绝不误杀外部进程（现有承诺不变）。
- 涉及：`process_mgr.rs`（`alloc_port` / `probe_ports`）、`dsh.rs::start`、`commands.rs`、`panel.js`。

#### 2.1.2 崩溃可视化 + 一键重启 + 连续失败诊断
- 子进程异常退出 → 主窗口**崩溃 overlay 卡片**：退出码/信号、崩溃前最后 50 行日志、「重启服务 / 进入安全模式 / 导出诊断包」三动作；
- **连续失败检测**：启动后 5s 内再崩计 1 次，累计 2 次自动询问"以安全模式启动排查插件？"（对接 2.8）。
- 涉及：`dsh.rs`（崩溃计数）、`lib.rs`（overlay）、`status.js`。

#### 2.1.3 后端日志流 → GUI
- 新事件 `dsh-log-line`：`process_mgr` 逐行转发 dsh stdout/stderr（级别着色），前端日志查看器消费；磁盘持久化由 tauri-plugin-log 承担（见 3 修复）。

### 2.2 Agent 预设管理（`.dshpreset` 导入导出）

**格式契约**（与 dataelement/dsh-desktop 生态互通）：
```text
.dshpreset = ZIP
├── manifest.json        # {format:"dsh-preset", version:1, id, name, description, sourceDshVersion, exportedAt}
└── preset/
    ├── agent.cordis.yml
    ├── preset.yml                 # optional
    └── skills / plugins / 预设自有资产
```
- **导出**：仅允许自定义 preset（内置需先复制）；ZIP 打包（拒符号链接、剔 `.DS_Store`/`Thumbs.db`/`desktop.ini`）；原生保存对话框；
- **导入两步式**：① 校验+预览（拒绝对路径、`../` 遍历、反斜杠、非法/不支持 manifest、超限体积 100MB、无效组合）② 确认安装（重名 id 改新 id → Harness preset 扫描器校验 → 原子 move）；
- **传输通道**：优先官方 loopback API（`GET/POST $DSH_WEB_URL/api/agent-preset.*`，壳层注入 `DSH_WEB_URL`）；当前 dsh 无此 API 时 CLI 降级或提示升级；
- **文件关联**：注册 `.dshpreset`，双击进导入向导；深层链接 `dsh-cockpit://import?path=...`。
- 涉及：新增 `src-tauri/src/preset.rs`、`commands.rs`、zip crate、`panel.js`、`tauri.conf.json`。

### 2.3 配置互通（规避短板②）

- **首启导入向导**：检测 `~/.dsh` 存在且隔离 home 为空 → 三选一：① 导入配置到隔离环境（复制 `credentials.yaml` + profiles，原目录不动，推荐默认）② 保持隔离 ③ 直接使用 `~/.dsh`（DSH_HOME 指向系统目录，命令行老用户专属，安全降级并提示风险）；
- **凭据单向同步开关**：启动时把系统 `~/.dsh/.credentials.yaml` 的 `DEEPSEEK_API_KEY` 等**单向复制**进隔离凭据（客户端不回写系统文件）；冲突 diff 提示，绝不静默覆盖；
- **一键导出备份**：导出隔离环境（`dsh-home` + 凭据，脱敏可选）为 zip，供换机迁移；
- 安全边界：凭据敏感——全程提示、默认单向、写入前备份、`--safe-mode` 停用同步。
- 涉及：`settings.rs`、`dsh.rs`（home 解析支持系统目录模式）、新向导页 `wizard.html/js`、`commands.rs`。

### 2.4 Node 运行时体积（规避短板①）

- **A. 分发包 + 首次按需下载（主）**：安装包只含「引导器 + npm/pnpm 纯 JS」（几 MB）；首次启动按平台从国内 CDN 下载 Node 到 `node-runtime/`，sha256 校验 + 断点续传 + 进度事件（复用 `dsh-update-progress` 与状态页进度条）；失败回退官方源。→ 安装包 **~150MB → ~20MB**；
- **B. 系统 Node 优先 + sidecar 兜底**：探测系统 node ≥24 直接使用（`DSH_USE_SYSTEM_NODE=1` 强制）；与 A 并存；
- C.（可选后期）UPX/分卷再压缩。
- 约束：koffi/node-pty 等原生模块两种运行时都要可构建（`ensure_node_launcher` + PATH 注入已有，补切换后的冒烟）。
- 涉及：`scripts/fetch-node.mjs`（出引导资源包 + checksum 清单）、`process_mgr.rs`（`resolve_runtime`）、`dsh.rs`、CI。

### 2.5 UI 扩展层（规避短板③，插件生态兼容）

> 原则：**对话导航条、字段汉化这类增强能力由 dsh 插件提供，壳层不重写 UI 逻辑**（技术架构优势 5）。

#### 2.5.1 长对话快速导航条（需求⑥）— 内置插件 `dsh-conversation-navigator`
- **形态**：dsh client 侧插件（cordis bundle，npm 发布 + `cordis.patch.yml` 注入，与现有插件热开关同一套机制）；
- **行为**：
  - MutationObserver 观察会话消息流 DOM，在会话容器右侧渲染**刻度条**（fixed 定位、半透明、不挡内容）；
  - 每个**用户消息**一个刻度标记；
  - 悬停刻度 → tooltip 预览消息摘要（取文本前 ~60 字符）；
  - 点击刻度 → 平滑 `scrollIntoView` 跳转到对应消息；
  - 会话消息数 < 阈值（默认 15 条）→ 自动隐藏；
  - 可配置：阈值、刻度条位置、深浅色主题跟随；
- **壳层职责**：仅负责随包安装 / 面板热开关 / 卸载（复用 `install_plugins` / `set_plugin_enabled`），插件自身不依赖壳层；
- 涉及：新插件仓库/包 + `dsh.rs` 内置插件清单追加 + 面板插件列表同步。

#### 2.5.2 轨迹面板字段汉化（需求⑦）
- **策略**：优先探测 dsh 上游 i18n/本地化能力（若官方已支持中文文案则直接启用）；否则走**宿主注入清单**（沿用 `FORCE_ZH_LOCALE_JS` 先例）做 DOM 文案替换；
- **映射表**（集中维护，随版本更新）：

  | 英文 | 中文 |
  | --- | --- |
  | Duration | 耗时 |
  | Turns | 轮次 |
  | Calls | 调用次数 |
  | Trace / Trace Panel | 轨迹 / 轨迹面板 |
  | …其余 Trace 相关英文标签 | 同步本地化 |

- **实现要点**：MutationObserver 处理动态渲染；映射表版本化（`INJECTION_MANIFEST`），注入失败静默降级，`--safe-mode` 整体关闭；
- 涉及：`lib.rs`（注入清单管理）、新注入脚本资源。

#### 2.5.3 宿主扩展层（通用底座）
1. **宿主补丁层注入（官方机制）**：桌面专属模块（设置面板入口、预设管理、诊断入口）做成宿主内置插件经 patch.yml 注入；
2. **JS 注入层（版本化、白名单）**：`INJECTION_MANIFEST`（脚本 id + 版本 + 注入时机 Started/Finished），页面脚本执行前注入；白名单 + safe-mode 可关 + 失败静默；
3. **原生叠加**：系统通知（`tauri-plugin-notification`）、全局快捷键（`Cmd+Shift+P` 宿主命令面板）、Windows 标题栏自定义；
4. **IPC 桥**：注入 `window.__dshCockpit`（白名单命令：开日志目录/导入预设/重启服务/读版本），`commands.rs` 守卫（仅 loopback 主窗口 + 参数校验）。

### 2.6 版本兼容性校验强化
1. **兼容矩阵**：内置 `KNOWN_INCOMPATIBLE`（CLI/端口/存储格式破坏性版本段），命中 → 更新弹窗降级为黄色警告；
2. **更新预览**：更新前拉取 changelog 摘要展示确认；
3. **冒烟增强**：临时端口冒烟 + 就绪后 5 分钟二次健康检查（HTTP 200 + 关键 API 存在），失败自动回滚。

### 2.7 日志可视化 + 诊断包（含 Bug 修复落地）
- **Bug 修复**（见 3）：日志目录与 `tauri-plugin-log` 写入位置对齐；
- **日志查看器**（面板内嵌 tab / 独立窗口）：聚合 dsh 子进程流（`dsh-log-line`）、pnpm 安装日志、外壳文件日志；级别过滤/关键字搜索/时间线/复制导出；
- **一键导出诊断包**：`logs/` + dsh 版本 + 平台信息 + 最近状态 → 单个 zip；
- **崩溃上下文**：自动截取崩溃前后 200 行。
- 涉及：`commands.rs`（`collect_diagnostics`）、`process_mgr.rs`、新页面。

### 2.8 安全模式增强
- 已有：`--safe-mode` 隔离 home、跳过插件与更新、面板一键退出；
- 新增：连续 2 次启动失败自动建议（对接 2.1.2）；banner 显示"已停用插件清单"；托盘加"安全模式重启"。

### 2.9 代码签名与发布流水线（CI）
- **macOS**：Developer ID + notarization（tauri `signingIdentity`/notarization 配置；CI 用 Secrets 存证书，keychain 准备脚本参考参考项目）；
- **Windows**：证书 thumbprint + signtool；EV 证书消除 SmartScreen；
- **外壳自更新闭环**：updater endpoints 指向 GitHub Releases，`tauri signer` 生成密钥对（公钥进 conf、私钥进 Secrets）→ "内核随 npm 更新 + 外壳随 Release 更新"双通道；
- **CI 改造**：签名 job"有 Secrets 就签、无则跳过"，保持开源可构建。

### 2.10 体验细节
- 托盘显示 dsh 版本与端口、快捷"重启/面板/退出"；
- 开机自启开关（`tauri-plugin-autostart`）；
- 窗口大小/位置记忆；
- 首启向导页（对接 2.3）+ 渐变加载页保留。

---

## 3. Bug 修复：日志目录无日志文件

### 3.1 根因（已实机定位）
- GUI 展示/「打开日志目录」指向：`<app_data>/logs`（`DshManager::log_dir`，macOS 为 `~/Library/Application Support/com.dshcockpit.desktop/logs`）——**空目录**；
- tauri-plugin-log 的 `TargetKind::LogDir { file_name }` 固定写入 `app.path().app_log_dir()`（macOS `~/Library/Logs/com.dshcockpit.desktop/dsh-cockpit.log`）——**日志一直在正常写入**（实机 13KB 持续增长）。
- 结论：不是流捕获/权限/刷盘问题，而是**日志目录指向不一致**：用户看到的目录与插件实际写入目录是两个位置。

### 3.2 修复方案（已实施）
1. `dsh.rs::DshManager::new`：`log_dir` 改为 `app.path().app_log_dir()?`（与 `tauri-plugin-log` 写入位置一致），并保持 `create_dir_all`；面板展示、`open-logs`、`about`、诊断包全部随之一致；
2. `lib.rs`：日志插件加 `.max_file_size(5MB)` 轮转，防无限增长；
3. `README.md`：架构图同步说明"运行日志位于系统日志目录"。

### 3.3 验证
- `cargo check` 通过；实机运行后 `~/Library/Logs/com.dshcockpit.desktop/dsh-cockpit.log` 与面板「日志目录」一致；
- 跨平台路径：Windows `%LOCALAPPDATA%\{identifier}\logs`、Linux `~/.local/share/{identifier}/logs`，均由 tauri `app_log_dir()` 统一解析，GUI 展示与实际写入天然一致。

---

## 4. 实施路线图（0.3.0 全集）

| 批次 | 内容 | 验收标准 |
| --- | --- | --- |
| P1 壳层基础 | 日志 Bug 修复（3）、动态端口（2.1.1）、日志流转发 + 查看器（2.1.3/2.7）、崩溃 overlay + 一键重启（2.1.2） | 日志目录一致且有文件；端口动态分配；崩溃可一键重启；日志可搜可导 |
| P2 资源管理 | `.dshpreset` 导入导出（2.2）、配置互通三层（2.3） | 预设双向互通、恶意包全拒；老用户配置一键迁移 |
| P3 体积分发 | Node 按需下载 + 系统优先（2.4） | 安装包 ≤30MB；首次启动自动就绪 |
| P4 UI 扩展层 | 导航条插件（2.5.1）、轨迹汉化（2.5.2）、宿主扩展底座（2.5.3）、外壳自更新闭环（2.9） | 导航条随插件热开关；汉化映射可版本化；safe-mode 可整体关闭 |
| 横切 | 版本兼容矩阵（2.6）、安全模式增强（2.8）、签名发布（2.9）、CI 调整 | 发布产物带签名；CI 无 Secrets 可降级 |

> 每个版本发布仍走"询问 → 备份 → 冒烟 → 失败回滚"，所有新能力 `--safe-mode` 下可整体关闭。

---

## 5. 风险与权衡

| 风险 | 缓解 |
| --- | --- |
| 按需下载 Node：首次启动依赖网络 | 国内 CDN + sha256 校验 + 断点续传 + 官方源回退 + 系统 Node 优先 |
| 配置互通：敏感凭据复制 | 默认单向、明确提示、写入前备份、冲突 diff、safe-mode 停用 |
| UI 注入/插件：上游 UI 变化致失效 | 注入清单版本化 + 失败静默降级 + safe-mode 关闭 + 插件可卸载 |
| 导航条插件与上游 DOM 耦合 | 插件独立版本、MutationObserver 容错、阈值可配、随插件体系可禁用 |
| `.dshpreset` API 依赖 dsh 版本 | 探测接口存在性，缺失则 CLI 降级或提示升级，不阻塞 |
| 签名密钥管理 | Secrets 缺失时跳过签名，开源可构建；密钥轮换脚本化 |
| 动态端口与附加模式并存复杂化 | 三级策略默认明确；附加模式仅用户确认后进入 |

---

## 6. 与参考项目的差异化定位

| 维度 | dataelement/dsh-desktop | DSH-Cockpit（本设计） |
| --- | --- | --- |
| 技术栈 | Electron + TS 工程 | Tauri 2 + Rust（更薄、更省资源） |
| 上游耦合 | patches/ 直接补丁官方包 | 纯壳层 + 官方补丁层/插件体系注入（不 Fork） |
| 运行时 | 内置 Node 打包装 | 按需下载 + 系统优先（体积优） |
| 配置 | 与 ~/.dsh 隔离 | 隔离默认 + 三层互通可选 |
| UI 定制 | 补丁官方包 + 注入 | 官方补丁层 + 插件化增强（导航条/汉化）+ 版本化注入清单 |
| 预设互通 | `.dshpreset` 契约 | 同一契约 + 官方 loopback API，双向兼容 |

**借鉴而不照搬**：参考其 update-policy / safe-mode / window-recovery 的边界划分、`.dshpreset` 契约、签名流水线脚本；不采用其 patches 官方包、重型多语言文档等做法。
