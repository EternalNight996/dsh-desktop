# 抓取指定进程主窗口的截图（真实抓屏，供文档/测试报告使用）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/capture-screen.ps1 -Proc "dsh-desktop" -Out "assets/screen/dsh-in-window.png"
param(
    [string]$Proc = "dsh-desktop",
    [string]$Out = "assets/screen/app.png"
)
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms

$sig = @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int nCmdShow);
  public struct RECT { public int Left, Top, Right, Bottom; }
}
"@
Add-Type -TypeDefinition $sig

$procs = Get-Process -Name $Proc -ErrorAction SilentlyContinue
if (-not $procs) { Write-Output "process not found: $Proc"; exit 2 }
$h = $procs[0].MainWindowHandle
if ($h -eq [IntPtr]::Zero) { Write-Output "no main window handle"; exit 3 }

[Win32]::ShowWindow($h, 5) | Out-Null   # SW_SHOW
[Win32]::SetForegroundWindow($h) | Out-Null
Start-Sleep -Milliseconds 1200

$r = New-Object Win32+RECT
[Win32]::GetWindowRect($h, [ref]$r) | Out-Null
$w = $r.Right - $r.Left
$hh = $r.Bottom - $r.Top
if ($w -le 0 -or $hh -le 0) { Write-Output "bad rect"; exit 4 }

$bmp = New-Object System.Drawing.Bitmap($w, $hh)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($r.Left, $r.Top, 0, 0, $bmp.Size)
$g.Dispose()

$outDir = Split-Path $Out
if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }
$abs = Join-Path (Get-Location) $Out
$bmp.Save($abs, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "saved: $abs ($w x $hh)"
