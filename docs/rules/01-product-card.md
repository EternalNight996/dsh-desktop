# 01 · 产品卡（Product Card）

## 一句话

为 DeepSeek Harness（`dsh`）的 Web UI 提供一个原生桌面窗口，让用户无需浏览器也能以桌面应用形态使用它。

## 为谁解决什么

| 项 | 内容 |
|---|---|
| 用户 | 使用 `dsh`（DeepSeek Harness）的开发者 / 本地用户 |
| 痛点 | `dsh web` 是浏览器页面，没有独立桌面窗口、任务栏图标、独立进程 |
| 方案 | Rust + Tauri 2 壳：窗口直接加载 `dsh` 的 Web UI，独立成桌面应用 |
| 不做什么 | 不内置/不打包/不托管 Node 与 dsh 进程；不修改 deepseek-harness 源码 |

## 范围（Scope）

- **In scope**：Tauri 2 桌面壳、窗口加载 `http://127.0.0.1:3080`、图标、文档、双平台仓库
- **Out of scope**：内置 Node 运行时、托管/拉起 dsh 进程、改造 dsh UI、Windows 安装包（后续可选）

## 技术栈

- Rust + Tauri 2（WebView2），构建由 cargo + cargo-tauri 驱动
