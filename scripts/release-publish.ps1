# 一键发布桌面壳自动更新（Windows）
#   1. 校验签名私钥（.tauri/updater.key，由 just keygen 生成）
#   2. 带签名 NSIS 构建（cargo tauri build --bundles nsis --ci，产出 setup.exe + .sig）
#   3. 生成 latest.json 并发布到 GitHub Releases（scripts/publish-update.mjs，需 gh 或 GITHUB_TOKEN）
#   4. 打本地 git tag v<版本> 并推送到 GitHub / Gitee
# 用法：
#   powershell -File scripts/release-publish.ps1                 # 构建 + 发布
#   powershell -File scripts/release-publish.ps1 -BuildOnly      # 只构建不发布
#   powershell -File scripts/release-publish.ps1 -Notes "更新说明"
param(
  [string]$Notes = "auto update release",
  [switch]$BuildOnly
)
$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot          # 仓库根目录
$keyPath = Join-Path $root '.tauri\updater.key'
$manifest = Join-Path $root 'scripts\publish-update.mjs'

# ---- 0. 校验密钥 ----
if (!(Test-Path $keyPath)) {
  Write-Host "[release-publish] 未找到签名私钥: $keyPath" -ForegroundColor Red
  Write-Host "[release-publish] 请先执行: just keygen" -ForegroundColor Yellow
  exit 1
}

# ---- 1. 带签名构建（--ci 跳过密钥密码交互；TAURI_SIGNING_PRIVATE_KEY 填密钥文件路径，bundler 会自动读取）----
$env:TAURI_SIGNING_PRIVATE_KEY = $keyPath
Write-Host "[release-publish] 带签名构建开始（NSIS，--ci）..." -ForegroundColor Cyan
Push-Location (Join-Path $root 'src-tauri')
cargo tauri build --target x86_64-pc-windows-msvc --bundles nsis --ci
$buildExit = $LASTEXITCODE
Pop-Location
if ($buildExit -ne 0) { Write-Host "[release-publish] 构建失败 (exit $buildExit)" -ForegroundColor Red; exit $buildExit }
Write-Host "[release-publish] 构建完成: src-tauri\target\x86_64-pc-windows-msvc\release\bundle\nsis\*.exe + .sig" -ForegroundColor Green

if ($BuildOnly) {
  Write-Host "[release-publish] -BuildOnly：跳过发布。" -ForegroundColor Yellow
  exit 0
}

# ---- 2. 发布到 GitHub Releases ----
Write-Host "[release-publish] 发布中（需 gh CLI 或 GITHUB_TOKEN）..." -ForegroundColor Cyan
node $manifest --notes $Notes
if ($LASTEXITCODE -ne 0) { Write-Host "[release-publish] 发布失败 (exit $LASTEXITCODE)" -ForegroundColor Red; exit $LASTEXITCODE }

# ---- 3. 打本地 tag 并推送双仓库 ----
$version = (Select-String -Path (Join-Path $root 'src-tauri\Cargo.toml') -Pattern '^version = "([^"]+)"').Matches[0].Groups[1].Value
$tag = "v$version"
if (-not (git -C $root tag -l $tag)) { git -C $root tag $tag }
git -C $root push github $tag
git -C $root push origin $tag

Write-Host "[release-publish] 发布完成 OK" -ForegroundColor Green
Write-Host "  Release: https://github.com/EternalNight996/dsh-desktop/releases/tag/$tag"
Write-Host "  Manifest: https://github.com/EternalNight996/dsh-desktop/releases/latest/download/latest.json"
Write-Host "  gitee 分支文档同步（如需）:" -ForegroundColor Yellow
Write-Host "    git checkout gitee && git merge master --no-edit && git push origin gitee && git checkout master"
