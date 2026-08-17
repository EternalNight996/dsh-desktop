# 生成 assets/logo.png（1024x1024，与 assets/logo.svg 同款配色/文案）
# 用法：powershell -NoProfile -ExecutionPolicy Bypass -File scripts/gen-logo.ps1
$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

$outDir = Join-Path $PSScriptRoot "..\assets"
$out = Join-Path $outDir "logo.png"
if (-not (Test-Path $outDir)) { New-Item -ItemType Directory -Path $outDir | Out-Null }

$size = 1024
$bmp = New-Object System.Drawing.Bitmap($size, $size)
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
$g.TextRenderingHint = [System.Drawing.Text.TextRenderingHint]::AntiAlias

# 圆角渐变背景
$rect = New-Object System.Drawing.Rectangle(0, 0, $size, $size)
$brush = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
    $rect,
    [System.Drawing.Color]::FromArgb(255, 30, 58, 138),
    [System.Drawing.Color]::FromArgb(255, 14, 165, 233),
    45)
$path = New-Object System.Drawing.Drawing2D.GraphicsPath
$r = 192
$path.AddArc(0, 0, $r, $r, 180, 90)
$path.AddArc($size - $r, 0, $r, $r, 270, 90)
$path.AddArc($size - $r, $size - $r, $r, $r, 0, 90)
$path.AddArc(0, $size - $r, $r, $r, 90, 90)
$path.CloseFigure()
$g.FillPath($brush, $path)

$sf = New-Object System.Drawing.StringFormat
$sf.Alignment = [System.Drawing.StringAlignment]::Center
$sf.LineAlignment = [System.Drawing.StringAlignment]::Center

$fontMain = New-Object System.Drawing.Font("Segoe UI", 300, [System.Drawing.FontStyle]::Bold, [System.Drawing.GraphicsUnit]::Pixel)
$g.DrawString("DSH", $fontMain, [System.Drawing.Brushes]::White,
    (New-Object System.Drawing.RectangleF(0, 60, $size, 560)), $sf)

$fontSub = New-Object System.Drawing.Font("Segoe UI", 58, [System.Drawing.FontStyle]::Regular, [System.Drawing.GraphicsUnit]::Pixel)
$subBrush = New-Object System.Drawing.SolidBrush([System.Drawing.Color]::FromArgb(255, 191, 219, 254))
$g.DrawString("DeepSeek Harness Desktop", $fontSub, $subBrush,
    (New-Object System.Drawing.RectangleF(0, 720, $size, 160)), $sf)

$g.Dispose()
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
Write-Output "saved: $out"
