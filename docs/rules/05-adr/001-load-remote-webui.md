# ADR-001 · 窗口加载远端 dsh Web UI，而非嵌入静态 dist

- 状态：已采纳
- 日期：2026-08-14

## 背景

要把 DeepSeek Harness 的 Web UI「加入」Tauri 2 桌面壳，直观想法是把前端静态构建产物（`apps/web` 的 `dist/`）嵌入 Tauri 的 `frontendDist`。

## 决策

窗口 `url` 直接指向已运行的 `dsh web` 服务地址（默认 `http://127.0.0.1:3080`），不嵌入静态 dist。

## 理由

dsh 的 Web UI **不是独立静态站点**：

1. `apps/web/vite.config.ts` 明确声明 `apps/web is not a standalone application`，并 `rejectStandaloneServe()` 禁止脱离 `dsh web` 单独 serve。
2. 前端 `packages/client/web/src/boot.tsx` 的启动内核必须解析服务端注入的 `window.__DSH_BOOT__`（BootManifest：模块 + 插件行），再经 client 模块系统按需拉取运行时插件 bundle。
3. 后端 API / WebSocket 由 `dsh web` 的 Node host 提供。

因此静态 dist 脱离 host 无法启动；正确的「嵌入」是让 Tauri 窗口加载该 host 提供的 Web UI。

## 结果

- Tauri 侧保持极简：零自定义命令、零 IPC，仅一个 `tauri::Builder`。
- host/port 通过 `tauri.conf.json` 的 `app.windows[0].url` 配置，改配置即可换地址。
- 不修改 deepseek-harness 任何代码。

## 否决的备选

| 备选 | 否决原因 |
|---|---|
| 嵌入静态 dist | 缺 `__DSH_BOOT__` 注入 + 插件 bundle + API，无法启动 |
| 内置 Node 运行时 + 拉起 dsh 进程 | 安装包大幅膨胀、打包复杂度高，且用户明确「Tauri 只提供 web 窗口」 |
