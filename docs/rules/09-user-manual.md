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
   dsh web   # 桌面壳已把 dsh 统一到全局（%APPDATA%\npm），直接用 dsh；不要用 npx @deepseek-ai/dsh（会重新下载）
   ```
   默认监听 `http://127.0.0.1:3080`。

2. 再启动桌面壳：
   ```sh
   just run        # 一键本地启动调试（增量构建后直接运行 debug 版 exe）
   # 或
   just dev        # 开发运行（带 Rust 热重载）
   # 或
   just dist       # debug 构建，产出 src-tauri/target/debug/dsh-desktop.exe
   ```

3. 窗口内即显示 dsh 的 Web UI。

## 四、常见问题

| 问题 | 处理 |
|---|---|
| 窗口空白 / 打不开 | 确认已先运行 `dsh web`，且端口 3080 未被占用 |
| 想改地址/端口 | 修改 `src-tauri/tauri.conf.json` 的 `app.windows[0].url`，同时 dsh 侧用对应 host/port 启动 |
| 打不开 exe | 检查是否安装 WebView2 运行时 |
| dsh 更新后界面异常 | dsh 处于 developer preview，版本迭代快，升级后重试或回退版本 |
| dsh 升级后插件加载报错（如 `keyed slot "settings.plugin.item" requires options.key`） | dsh 新版把 `settings.plugin.item` 改为 keyed slot，要求注册时传 `key`；旧版第三方插件只传 `id` 会报错。升级插件到适配版本：`dsh plugin --profile web add <插件>@<新版>`（如 `dsh-vision-router@1.5.3`）；官方未适配的插件可临时在其 `client.js` 的 `settings.plugin.item` 注册里补 `key: '<id>'`，待官方更新后重新安装覆盖 |
| 启动时闪出 cmd 黑窗口 | 旧版在线方案的已知问题（npm exec 经 cmd 拉起 dsh）；新版已改为把 dsh 全局安装（`%APPDATA%\npm`）后由内置 node 直接运行，不再弹窗；若仍出现请更新桌面壳 |
| 更新入口在哪 | 系统托盘右键：打开主窗口 / 设置 / 检查更新 / 退出；设置窗口内可手动检查桌面壳与 dsh 更新；启动发现新版会自动打开设置窗口弹「是否更新」（可勾选「下次不提醒此版本」） |
