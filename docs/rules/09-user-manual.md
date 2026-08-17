# 09 · 用户手册（简版 FAQ）

> 版本：v0.1 ｜ 日期：2026-08-14

## 一、这是什么

DeepSeek Harness Desktop 是一个桌面壳：把 DeepSeek Harness（`dsh`）的 Web UI 放进一个独立的原生窗口，方便作为桌面应用使用。

## 二、前提

1. 已安装 Node.js（≥ 22.19 或 ≥ 24，见 [dsh README](https://github.com/deepseek-ai/deepseek-harness)）。
2. 已安装 Rust 工具链（构建/开发时需要）。
3. 系统 WebView 依赖（按平台）：
   - **Windows**：WebView2 运行时（Win10/11 通常已自带）
   - **Ubuntu/Debian**：WebKitGTK 4.1 + 构建工具链，运行 `just setup-linux` 一键安装

## 三、快速开始

1. 先启动 dsh 服务：
   ```sh
   npx @deepseek-ai/dsh web
   ```
   默认监听 `http://127.0.0.1:3080`。

2. 再启动桌面壳：
   ```sh
   just dev        # 开发运行
   # 或
   just dist       # debug 构建，产出 src-tauri/target/debug/dsh-desktop.exe
   ```

3. 窗口内即显示 dsh 的 Web UI。

## 四、常见问题

| 问题 | 处理 |
|---|---|
| 窗口空白 / 打不开 | 确认已先运行 `npx @deepseek-ai/dsh web`，且端口 3080 未被占用 |
| 想改地址/端口 | 修改 `src-tauri/tauri.conf.json` 的 `app.windows[0].url`，同时 dsh 侧用对应 host/port 启动 |
| 打不开 exe | 检查是否安装 WebView2 运行时 |
| dsh 更新后界面异常 | dsh 处于 developer preview，版本迭代快，升级后重试或回退版本 |
