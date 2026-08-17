# ADR-002：桌面壳自动更新（tauri-plugin-updater）

> 状态：已采纳 ｜ 日期：2026-08-15 ｜ 关联：[ADR-001](001-load-remote-webui.md)

## 背景

桌面壳是独立分发的安装包，升级靠用户手动下载重装，体验差。需要「启动自动检查、一键下载安装、自动重启」的平台级自动更新。

## 决策

- 使用 **`tauri-plugin-updater`**（Tauri 2 官方更新器）实现桌面壳自身更新：
  - 更新源：GitHub Releases 的 `latest.json`（`plugins.updater.endpoints`）
  - 签名：`tauri signer` 生成的密钥对；公钥写死进 `tauri.conf.json`，私钥（`.tauri/updater.key`）gitignore，构建时用 `TAURI_SIGNING_PRIVATE_KEY` 注入
  - 安装模式：Windows `passive`（安装时显示进度条，不打扰）
  - 交互：全部走 Rust 命令（`check_app_update` / `install_app_update`），前端加载页用 `withGlobalTauri` 注入的 `window.__TAURI__` 调用，不引入 npm 打包链
- dsh（被包装的 Node 应用）**不做运行时更新**：离线方案把 dsh 打进 resources（只读），拉取方案按版本 pin。改为提供「检查 dsh 更新」命令（查 npm registry 最新版），实际升级走构建期 `just update` 重新打包。

## 理由

- 零侵入：不修改 dsh 任何代码；更新 UI 只出现在壳自己的加载页
- 复用官方插件，避免自建更新协议；签名（`.sig`）由 tauri bundler 生成，`latest.json` 由发布脚本 `scripts/publish-update.mjs` 生成并统一上传
- Rust 侧完成下载/安装/重启，前端仅薄展示，加载页无需 bundler

## 代价

- 发布新版本必须用私钥签名（丢了私钥即无法再发布更新）
- 更新端点依赖 GitHub Releases 可访问；大陆网络下可能需配代理（Gitee 镜像可后续加 endpoint）
