# ============================================================
# 一键部署 dsh-desktop 全套「原创插件」（安装到 web profile）
# 用法：
#   1) 已装 dsh-desktop（终端 dsh 命令已统一到全局）
#   2) 直接运行本脚本；或 `just install-plugins`
# 说明：全部用 `dsh plugin --profile web add github:EternalNight996/<repo>`
#       从 GitHub 源安装，不经过 npm，避免 npx 重复下载 dsh。
# ============================================================

$ErrorActionPreference = "Continue"

# 4 个原创插件（对应 EternalNight996 下的同名 GitHub 仓库）
$plugins = @(
    "dsh-theme",
    "dsh-memory-eternal",
    "dsh-ui-three-body",
    "dsh-ui-agents-pixe"
)

Write-Host ""
Write-Host "==> dsh-desktop 原创插件一键部署" -ForegroundColor Cyan
Write-Host "    目标：--profile web" -ForegroundColor DarkGray
Write-Host ""

# 检查 dsh 是否可用
if (-not (Get-Command dsh -ErrorAction SilentlyContinue)) {
    Write-Host "[!] 未找到 dsh 命令。请先安装/运行 dsh-desktop（它会统一终端 dsh 到全局）。" -ForegroundColor Yellow
    exit 1
}
Write-Host ("    使用 dsh v" + (dsh --version)) -ForegroundColor DarkGray

$ok = 0
$failed = @()
foreach ($p in $plugins) {
    $spec = "github:EternalNight996/$p"
    Write-Host ""
    Write-Host ("==> 安装 " + $spec + " ...") -ForegroundColor Cyan
    $cmd = "dsh plugin --profile web add " + $spec
    Write-Host ("    $ " + $cmd) -ForegroundColor DarkGray
    try {
        & dsh plugin --profile web add $spec
        if ($LASTEXITCODE -eq 0) {
            Write-Host ("    [OK] " + $p + " 已安装") -ForegroundColor Green
            $ok++
        } else {
            Write-Host ("    [FAIL] " + $p + " 安装失败（退出码 " + $LASTEXITCODE + "）") -ForegroundColor Red
            $failed += $p
        }
    } catch {
        Write-Host ("    [FAIL] " + $p + " 安装异常：" + $_.Exception.Message) -ForegroundColor Red
        $failed += $p
    }
}

Write-Host ""
if ($failed.Count -eq 0) {
    Write-Host ("==> 全部完成：4/" + $plugins.Count + " 个插件安装成功") -ForegroundColor Green
    Write-Host "    重启 dsh-desktop 后生效。" -ForegroundColor DarkGray
} else {
    Write-Host ("==> 完成：成功 " + $ok + "/" + $plugins.Count + "，失败：" + ($failed -join ", ")) -ForegroundColor Yellow
    Write-Host "    失败的可单独重试：dsh plugin --profile web add github:EternalNight996/<仓库名>" -ForegroundColor DarkGray
}
