# dsh-desktop 构建入口
# 全部由 Rust（cargo + cargo-tauri）驱动；node/npm/dsh 作为 sidecar + resources 打包。
# 跨平台：Windows / macOS / Linux。
#
# WebView2 三种安装方式（配置于 bundle.windows.webviewInstallMode）：
#   默认「用系统自带」skip（tauri.conf.json）
#   在线「安装时下载」downloadBootstrapper（tauri.online.json）
#   离线「内置安装器」offlineInstaller（tauri.offline.json，并内置 dsh）

# Windows 下默认 shell 是 sh（常不存在），显式指定 PowerShell；Linux/macOS 自动用 sh/bash。
set windows-shell := ["powershell.exe", "-NoLogo", "-Command"]

app := "dsh-desktop"
dsh_version := "0.1.0-rc.6"
online_cfg := "src-tauri/tauri.online.json"
offline_cfg := "src-tauri/tauri.offline.json"

# 默认（直接 `just` 不带参数）：显示帮助
default:
    @just --list

# 安装 Rust 原生 Tauri CLI + 全局化 dsh CLI（跨平台，一次性；dsh 装到用户级 npm 前缀，新开终端即可用）
setup:
    cargo install tauri-cli --locked
    node scripts/globalize-dsh.mjs {{dsh_version}}

# 仅全局化 dsh CLI（终端直接可用 dsh 命令；Windows→%APPDATA%\npm，macOS/Linux→~/.npm-global）
setup-dsh:
    node scripts/globalize-dsh.mjs {{dsh_version}}

# 安装 Ubuntu/Debian 系统依赖（仅 Linux；Windows/macOS 跳过）
setup-linux:
    bash scripts/setup-linux.sh

# 准备运行时：node/npm sidecar +（离线需）dsh
vendor:
    node scripts/vendor.mjs {{dsh_version}}

# 检查并拉取最新 deepseek-harness (dsh)（不改源码，自动查 npm 最新版；离线方案随后重打 release-*-offline）
update:
    node scripts/vendor.mjs --update --latest

# 生成图标集
icon:
    cargo tauri icon assets/logo.png

# 生成自动更新签名密钥（一次性；私钥存 .tauri/updater.key，已 gitignore，公钥已写入 tauri.conf.json）
keygen:
    cargo tauri signer generate -w .tauri/updater.key --force --ci

# 发布带签名的更新到 GitHub Releases（需 gh 或 GITHUB_TOKEN；先构建：TAURI_SIGNING_PRIVATE_KEY* 见 README）
publish:
    node scripts/publish-update.mjs

# 一键发布自动更新（Windows）：带签名 NSIS 构建 → latest.json → GitHub Releases → 打 tag 推送双仓库
# 用法：just release-publish "更新说明"（位置参数；首次先 just keygen；需 gh 或 GITHUB_TOKEN）
# 跨平台发版请用 GitHub Actions（.github/workflows/release.yml），推 tag 自动三平台构建发布
release-publish notes="auto update release":
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-publish.ps1 -Notes "{{notes}}"

# 仅带签名构建（不发布）：产出 NSIS 安装包 + .sig，供后续 just publish
release-win-signed:
    powershell -NoProfile -ExecutionPolicy Bypass -File scripts/release-publish.ps1 -BuildOnly

# 开发运行（默认用系统自带）
dev:
    cargo tauri dev

# ===== debug 构建（当前平台）=====

# 默认：用系统自带 WebView2
dist:
    cargo tauri build --debug --no-bundle

# 在线：安装时下载 WebView2
dist-online:
    cargo tauri build --debug --no-bundle --config {{online_cfg}}

# 离线：内置 WebView2 + dsh（需先 just vendor）
dist-offline:
    cargo tauri build --debug --no-bundle --config {{offline_cfg}}

# ===== release 打包（按平台，默认「用系统自带」）=====

release-win:             # Windows x64
    cargo tauri build --target x86_64-pc-windows-msvc

release-mac:             # macOS Apple Silicon
    cargo tauri build --target aarch64-apple-darwin

release-mac-x64:         # macOS Intel
    cargo tauri build --target x86_64-apple-darwin

release-mac-universal:   # macOS 通用
    cargo tauri build --target universal-apple-darwin

release-linux:           # Linux x64
    cargo tauri build --target x86_64-unknown-linux-gnu

# ===== 在线打包（安装时下载 WebView2）=====

release-online:          # 当前平台在线安装
    cargo tauri build --config {{online_cfg}}

# ===== 离线打包（内置 WebView2 离线安装器 + dsh，客户零依赖）=====

release-win-offline:     # Windows x64
    cargo tauri build --config {{offline_cfg}} --target x86_64-pc-windows-msvc

release-mac-offline:     # macOS Apple Silicon
    cargo tauri build --config {{offline_cfg}} --target aarch64-apple-darwin

release-linux-offline:   # Linux x64
    cargo tauri build --config {{offline_cfg}} --target x86_64-unknown-linux-gnu
