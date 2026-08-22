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
online_cfg := "src-tauri/tauri.online.json"
offline_cfg := "src-tauri/tauri.offline.json"

# 自动加载更新签名私钥：
#   直接读取 .tauri/updater.key（由 `just keygen` 生成）的绝对路径并注入
#   TAURI_SIGNING_PRIVATE_KEY，所有 release-* 打包都会自动带签名，无需手动设环境变量。
tauri_key := justfile_directory() + "/.tauri/updater.key"
export TAURI_SIGNING_PRIVATE_KEY := tauri_key

# 默认（直接 `just` 不带参数）：显示帮助
default:
    @just --list

# 安装 Rust 原生 Tauri CLI + 全局化 dsh CLI（跨平台，一次性；dsh 装到用户级 npm 前缀，新开终端即可用）
setup:
    cargo install tauri-cli --locked
    node scripts/globalize-dsh.mjs

# 仅全局化 dsh CLI（终端直接可用 dsh 命令；Windows→%APPDATA%\npm，macOS/Linux→~/.npm-global）
setup-dsh:
    node scripts/globalize-dsh.mjs

# 安装 Ubuntu/Debian 系统依赖（仅 Linux；Windows/macOS 跳过）
setup-linux:
    bash scripts/setup-linux.sh

# 准备运行时：node/npm sidecar +（离线需）dsh
vendor:
    node scripts/vendor.mjs

# 一键部署全套「原创插件」到 web profile（从 GitHub 源安装，不经 npm，避免 npx 重复下载 dsh）
# Windows 用 PowerShell 脚本（也可直接双击 scripts/install-plugins.ps1）；macOS/Linux 用 sh 循环。
install-plugins:
    @{{ if os() == "windows" {
        "powershell -NoProfile -ExecutionPolicy Bypass -File scripts/install-plugins.ps1"
    } else {
        "for p in dsh-theme dsh-memory-eternal dsh-ui-three-body dsh-ui-agents-pixe; do dsh plugin --profile web add github:EternalNight996/$p; done"
    } }}

# 更新全局 dsh（默认）：自动查 npm 官方最新版并一键升级终端 dsh 命令 + 桌面壳在线 dsh
#（两者同源：Windows→%APPDATA%\npm / macOS-Linux→~/.npm-global，改源码；用后需重开终端，重启桌面壳生效）
update:
    node scripts/globalize-dsh.mjs

# 更新离线内置 dsh（仅离线打包方案用）：拉取最新版到 vendor/dsh-runtime，随后重打 release-*-offline
update-offline:
    node scripts/vendor.mjs --update --latest

# 生成图标集
icon:
    cargo tauri icon assets/logo.png

# 生成自动更新签名密钥（一次性；私钥存 .tauri/updater.key，已 gitignore，公钥已写入 tauri.conf.json）
# 生成后 justfile 会自动加载该私钥，后续直接 `just release-*` 即可。
keygen:
    cargo tauri signer generate -w .tauri/updater.key --force --ci

# 发布带签名的更新到 GitHub Releases（需 gh 或 GITHUB_TOKEN；先构建：`just release-win-signed`）
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

# 一键本地启动调试：增量构建并直接运行 debug 版桌面壳（比 dev 更快，无 Rust 热重载）
# 先同步本地开发的 dsh-beast-master 到 profile 副本，避免 dsh 跑旧代码导致界面进不去。
run:
    @powershell -NoProfile -ExecutionPolicy Bypass -Command "if (Test-Path 'F:\MyApp\eternal\dsh-beast-master\scripts\sync-profile.mjs') { cd 'F:\MyApp\eternal\dsh-beast-master'; node scripts/sync-profile.mjs }"
    cargo build --manifest-path src-tauri/Cargo.toml
    @{{ if os() == "windows" { "src-tauri/target/debug/dsh-desktop.exe" } else { "src-tauri/target/debug/dsh-desktop" } }}

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

# 以下 release-* 都会自动读取 .tauri/updater.key 进行签名，无需手动设置环境变量。

release-win:             # Windows x64（NSIS；如需 MSI 可去掉 --bundles nsis）
    cargo tauri build --target x86_64-pc-windows-msvc --bundles nsis

release-mac:             # macOS Apple Silicon
    cargo tauri build --target aarch64-apple-darwin

release-mac-x64:         # macOS Intel
    cargo tauri build --target x86_64-apple-darwin

release-mac-universal:   # macOS 通用
    cargo tauri build --target universal-apple-darwin

release-linux:           # Linux x64
    cargo tauri build --target x86_64-unknown-linux-gnu

# 当前平台 release 打包（Windows x64 / macOS Apple Silicon / Linux x64）
release:
    @just {{ if os() == "windows" { "release-win" } else if os() == "macos" { "release-mac" } else { "release-linux" } }}

# ===== 在线打包（安装时下载 WebView2）=====

release-online:          # 当前平台在线安装
    cargo tauri build --config {{online_cfg}}

# ===== 离线打包（内置 WebView2 离线安装器 + dsh，客户零依赖）=====

release-win-offline:     # Windows x64（NSIS；如需 MSI 可去掉 --bundles nsis）
    cargo tauri build --config {{offline_cfg}} --target x86_64-pc-windows-msvc --bundles nsis

release-mac-offline:     # macOS Apple Silicon
    cargo tauri build --config {{offline_cfg}} --target aarch64-apple-darwin

release-linux-offline:   # Linux x64
    cargo tauri build --config {{offline_cfg}} --target x86_64-unknown-linux-gnu

# 当前平台离线 release 打包（Windows x64 / macOS Apple Silicon / Linux x64）
release-offline:
    @just {{ if os() == "windows" { "release-win-offline" } else if os() == "macos" { "release-mac-offline" } else { "release-linux-offline" } }}
