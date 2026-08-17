# dsh-desktop 开发规则

> 使用方：EternalNight ｜ slug：`dsh-desktop` ｜ 桌面壳软件项目（Windows）

## 一句话定位

用 Rust + Tauri 2 给 [deepseek-harness](https://github.com/deepseek-ai/deepseek-harness)（`dsh`，DeepSeek 开源 agent harness）的 Web UI 套一个**自包含桌面壳**：内置 node sidecar 自动拉起 dsh、窗口加载其 Web UI。不修改 deepseek-harness 任何代码。两套打包方案（justfile 可切换）：③ 离线内置 dsh（客户零依赖）/ ② 运行时 npx 拉取（轻量、好跟官方更新）。

## 技术栈

- 桌面壳：**Rust + Tauri 2**（webview：Windows 用 WebView2，macOS 用 WKWebView，Linux 用 WebKitGTK）
- 目标平台：**Windows / macOS / Linux**
- 被包装对象：`dsh` Web UI（DeepSeek Harness，Node.js 应用）
- 进程编排：`tauri-plugin-shell` sidecar 拉起 dsh；dsh 两种来源（内置 resources / npx 拉取）
- 构建链路：**cargo + cargo-tauri 驱动**；`just vendor` 用 Node 准备运行时（node/npm/dsh）

## 目录约定

- 加载页：`src/`（`index.html`，dsh 启动前的占位页）
- Rust 后端：`src-tauri/`（`tauri.conf.json` 决定窗口 URL；`binaries/` 放 node sidecar；`tauri.offline.json` 加 dsh 资源）
- 运行时产物：`vendor/`（`just vendor` 生成，gitignore）：`dsh-runtime/` + `node-runtime/`
- 文档：`docs/`（`docs/rules/` 阶段产物）
- 截图/素材：`assets/screen/`（真实抓屏）、`assets/logo.svg|png`
- 脚本：`scripts/`（`vendor.mjs`、`gen-logo.ps1` 等）

## 常用命令

- 安装 CLI：`just setup`（= `cargo install tauri-cli --locked` + 全局化 dsh CLI，装到用户级 npm 前缀，新开终端即可用 `dsh`；只装 dsh 可单独跑 `just setup-dsh`）
- Linux 系统依赖：`just setup-linux`（仅 Ubuntu/Debian）
- 准备运行时：`just vendor`（node/npm sidecar + 内置 dsh）
- 一键更新 dsh：`just update`（自动查 npm 官方最新版，不改源码）
- 生成更新签名密钥：`just keygen`（一次性；私钥 `.tauri/updater.key` 已 gitignore，公钥已写入 tauri.conf.json）
- 跨平台自动发布：GitHub Actions `.github/workflows/release.yml`（推 v* tag 自动三平台构建签名，发布 GitHub + Gitee 双发行版；需仓库 secret `TAURI_SIGNING_PRIVATE_KEY` + `GITEE_TOKEN`）
- Windows 一键发布：`just release-publish "更新说明"`（`scripts/release-publish.ps1`：带签名 NSIS 构建 → 生成 latest.json → GitHub Releases → 打 tag 推送；需 gh 或 GITHUB_TOKEN）
- 仅带签名构建：`just release-win-signed`（产出 setup.exe + .sig）
- 手动发布：`just publish`（`scripts/publish-update.mjs`，需 gh 或 GITHUB_TOKEN，先做带签名构建）
- 生成图标：`just icon`
- 拉取构建（默认）：`just dist` / `just release-*`
- 离线构建：`just dist-offline` / `just release-*-offline`
- 开发运行：`just dev`

## 提交规范

- 提交信息前缀：feat/fix/chore/docs/refactor/test
- 版本：semver，完成一个功能打一个 tag（功能=minor、修复=patch）
- 远程仓库：
  - Gitee `https://gitee.com/eternalnight996/dsh-desktop`（分支 `gitee`）
  - GitHub `https://github.com/EternalNight996/dsh-desktop`（分支 `master`）
- README 已统一：**两个仓库同一份 `README.md`，一律中文为主**，不再区分语言版本，也不再用同步脚本（`scripts/sync-readme-gitee.mjs` 已删除）
- README 改动同步：改完 master 后 `git checkout gitee && git merge master --no-edit && git push origin gitee && git checkout master`（冲突时手动解）

## 项目约定

- 中文注释；文档中涉及 GUI 的必须附实际运行截图（真实抓屏，禁占位图）
- 文档示例必须贴真实可运行输出
- 关键架构决策写入 `docs/rules/05-adr/`

## 阶段产物

- 按 product-rules 生命周期推进，产物放 `docs/rules/`，gate 欠账记 `docs/rules/DEBTS.md`

## 共享记忆

- 本产品**不依赖外部记忆服务**（agentmemory 已全面剥离）；会话记忆 = git 提交 + `docs/rules/` 文档
- 跨会话经验通过提交记录与文档（ADR / DEBTS / 阶段产物）沉淀：先结论、后细节、附实测证据
- 像素办公室 `dsh-ui-agents-pixe` 为独立 npm 包（无 scope，独立仓库维护），**桌面壳不内置**，按需 `dsh plugin add dsh-ui-agents-pixe` 安装
