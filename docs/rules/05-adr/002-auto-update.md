# ADR-002：桌面壳自动更新（tauri-plugin-updater）

> 状态：已采纳 ｜ 日期：2026-08-15 ｜ 关联：[ADR-001](001-load-remote-webui.md)

## 背景

桌面壳是独立分发的安装包，升级靠用户手动下载重装，体验差。需要「启动自动检查、一键下载安装、自动重启」的平台级自动更新。

## 决策

- 使用 **`tauri-plugin-updater`**（Tauri 2 官方更新器）实现桌面壳自身更新：
  - 更新源：GitHub Releases 的 `latest.json`（`plugins.updater.endpoints`）
  - 签名：`tauri signer` 生成的密钥对；公钥写死进 `tauri.conf.json`，私钥（`.tauri/updater.key`）gitignore，构建时用 `TAURI_SIGNING_PRIVATE_KEY` 注入
  - 安装模式：Windows `passive`（安装时显示进度条，不打扰）
  - 交互：全部走 Rust 命令（`check_app_update` / `install_app_update` / `check_dsh_update` / `update_online_dsh` / `get_dsh_version` 等），设置窗口（`settings.html`）用 `withGlobalTauri` 注入的 `window.__TAURI__` 调用，不引入 npm 打包链；更新入口在**系统托盘**（右键：打开主窗口/设置/检查更新/退出）+ **设置窗口**（独立窗口，加载页保持纯净）；**每次启动后台自动检查**（受「启动时自动检查更新」开关控制），发现新版自动打开设置窗口弹「是否立即更新」，勾选「下次不提醒此版本」后按版本号持久化（`app_data/update-prefs.json`），同一版本不再弹、新版本仍会提醒
- dsh（被包装的 Node 应用）：**在线方案不做版本 pin，但不自动更新**——启动直接使用**全局安装**（`%APPDATA%\npm`，与终端 dsh 命令 `dsh` 同源；`DSH_GLOBAL_DIR` 可覆盖），未安装（首次运行）才联网安装官方最新版到全局；版本更新由**后台检查**（只查 npm registry 最新版，有新版发事件 `dsh-update-available` 提示）驱动，用户点「更新 dsh」手动触发 `update_online_dsh`（`npm install -g` 升级全局 dsh 并重启 dsh 进程）；在线方案更新 dsh 走 `just update`（升级全局 dsh），离线方案升级走构建期 `just update-offline` 重新打包。另有「检查 dsh 更新」命令（查 npm registry 最新版）供界面展示。
- dsh 版本**不写死在代码里**（曾用常量 pin，改为运行时读取实际安装/内置的 package.json；构建脚本默认跟随 latest）

## 理由

- 零侵入：不修改 dsh 任何代码；更新 UI 在壳自己的设置窗口与系统托盘
- 复用官方插件，避免自建更新协议；签名（`.sig`）由 tauri bundler 生成，`latest.json` 由发布脚本 `scripts/publish-update.mjs` 生成并统一上传
- Rust 侧完成下载/安装/重启，前端仅薄展示，加载页无需 bundler

## 代价

- 发布新版本必须用私钥签名（丢了私钥即无法再发布更新）
- 更新端点依赖 GitHub Releases 可访问；大陆网络下可能需配代理（Gitee 镜像可后续加 endpoint）
