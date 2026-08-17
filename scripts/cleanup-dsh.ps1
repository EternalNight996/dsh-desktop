# 清理残留的 dsh web / agentmemory 重复进程（保留端口 53550 的正式 GUI 与 3113 的 agentmemory 服务）
# 用法: powershell -ExecutionPolicy Bypass -File scripts\cleanup-dsh.ps1
$ErrorActionPreference = 'SilentlyContinue'

Write-Host "== 清理残留 dsh / agentmemory 进程 ==" -ForegroundColor Cyan

# 1) 找所有 dsh web 实例（node bin.js web --port X），排除 53550（正式 GUI）
$dsh = Get-CimInstance Win32_Process | Where-Object {
  $_.CommandLine -like '*dsh*bin.js*web*' -and $_.Name -like 'node*'
}
foreach ($p in $dsh) {
  $keep = $p.CommandLine -like '*--port 53550*' -or $p.CommandLine -like '*--port 53550*'
  if ($keep) {
    Write-Host ("保留 GUI: PID " + $p.ProcessId) -ForegroundColor Green
  } else {
    Write-Host ("清理 dsh: PID " + $p.ProcessId) -ForegroundColor Yellow
    Stop-Process -Id $p.ProcessId -Force
  }
}

# 2) 清理 dev 构建的 node 父链（src-tauri\target\debug 下启动的 dsh）
$dev = Get-CimInstance Win32_Process | Where-Object {
  $_.CommandLine -like '*dsh-desktop*src-tauri*target*' -and
  $_.CommandLine -like '*dsh*web*' -and $_.Name -like 'node*'
}
foreach ($p in $dev) {
  Write-Host ("清理 dev 实例: PID " + $p.ProcessId) -ForegroundColor Yellow
  Stop-Process -Id $p.ProcessId -Force
}

# 3) 重复的 agentmemory MCP（保留 127.0.0.1:3113 服务）
$am = Get-CimInstance Win32_Process | Where-Object {
  $_.CommandLine -like '*agentmemory*mcp*' -and $_.Name -like 'node*'
}
foreach ($p in $am) {
  Write-Host ("清理 agentmemory MCP: PID " + $p.ProcessId) -ForegroundColor Yellow
  Stop-Process -Id $p.ProcessId -Force
}

Start-Sleep -Seconds 1
Write-Host ""
Write-Host "== 清理后剩余 dsh 进程 ==" -ForegroundColor Cyan
Get-CimInstance Win32_Process | Where-Object {
  $_.CommandLine -like '*dsh*bin.js*web*' -and $_.Name -like 'node*'
} | ForEach-Object {
  Write-Host ("PID " + $_.ProcessId + ": " + $_.CommandLine.Substring(0, [Math]::Min(100, $_.CommandLine.Length)))
}
Write-Host "完成。" -ForegroundColor Green
