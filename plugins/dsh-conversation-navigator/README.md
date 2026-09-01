# dsh-conversation-navigator

DSH 长对话快速导航条（P4 2.5.1）：会话右侧渲染刻度条，悬停预览消息摘要、点击平滑跳转。

## 行为

- MutationObserver 观察会话消息流 DOM，右侧 fixed 半透明刻度条
- 每个**用户消息**一个刻度标记
- 悬停 → tooltip 预览消息摘要（前 ~60 字符）
- 点击 → 平滑 `scrollIntoView` 跳转
- 会话消息数 < 阈值（默认 15 条）→ 自动隐藏
- 深浅色主题跟随（`[data-theme]`）

## 安装

```bash
# 发布到 npm 后（推荐）
dsh plugin --profile web add dsh-conversation-navigator

# 本地开发
dsh plugin --profile web add link:<本目录绝对路径>
```

## 配置

`apply(ctx, config)` 支持 `config.threshold`（消息数阈值，默认 15）。

## 设计约束

- 纯前端实现：宿主 webServer 注册 `/dsh-navigator/navigator.js` + `tapIndex` 注入
- 不依赖壳层：随 dsh 插件体系安装 / 面板热开关 / 卸载
- DOM 变更容错：容器找不到时仅功能降级，不影响会话本体
