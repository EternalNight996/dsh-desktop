# assets/screen shrinker (PNG->WebP + GIF reencode)
# Usage: powershell -NoProfile -ExecutionPolicy Bypass -File scripts/shrink-assets.ps1
# Output: .webp replaces .png, .gif is re-encoded in-place; original PNGs should be removed via `git rm`.
# Requires: ffmpeg.exe in PATH.
param([switch]$DryRun)

$ErrorActionPreference = "Stop"
$root = (Get-Location).Path
$screenDir = Join-Path $root "assets/screen"

if (-not (Get-Command ffmpeg -ErrorAction SilentlyContinue)) {
  Write-Error "ffmpeg not in PATH"
  exit 2
}

function Run-FFmpeg([string[]]$argList) {
  # Quote args containing spaces (e.g. -i "with space.png"); leave others bare.
  $sb = New-Object System.Text.StringBuilder
  foreach ($a in $argList) {
    if (-not $a) { continue }
    if ($a -match '\s') { [void]$sb.Append('"' + $a + '"') } else { [void]$sb.Append($a) }
    [void]$sb.Append(' ')
  }
  $argsString = $sb.ToString().TrimEnd()
  $log = [System.IO.Path]::GetTempFileName()
  $p = Start-Process -FilePath ffmpeg -ArgumentList $argsString -NoNewWindow -Wait -PassThru -RedirectStandardOutput "$log.out" -RedirectStandardError "$log.err"
  if ($p.ExitCode -ne 0) {
    Write-Warning "ffmpeg exit $($p.ExitCode)"
    $tail = Get-Content "$log.err" -Raw -ErrorAction SilentlyContinue
    if ($tail) { Write-Warning ($tail.Substring(0, [Math]::Min(400, $tail.Length))) }
  }
  Remove-Item "$log.out","$log.err" -ErrorAction SilentlyContinue
  return $p.ExitCode
}

function Fmt-KB([long]$bytes) {
  return [string]([math]::Round($bytes/1KB, 0))
}

# 1. PNG -> WebP
$imgs = @(
  @{ src="dsh-desktop auto update.png";          w=520 },
  @{ src="dsh-desktop.png";                      w=720 },
  @{ src="dsh-memory-eternal grap view.png";     w=720 },
  @{ src="dsh-memory-eternal.png";               w=720 },
  @{ src="dsh-theme setting.png";                w=720 },
  @{ src="dsh-theme setting2.png";               w=720 },
  @{ src="dsh-ui-agents-pixe setting.png";       w=720 },
  @{ src="dsh-ui-agents-pixe.png";               w=720 },
  @{ src="dsh-ui-three-body setting.png";        w=720 },
  @{ src="dsh-ui-three-body setting2.png";       w=440 }
)

New-Item -ItemType Directory -Path "$screenDir\_tmp" -Force | Out-Null

foreach ($i in $imgs) {
  $src = Join-Path $screenDir $i.src
  $dst = [System.IO.Path]::ChangeExtension($src, ".webp")
  if (-not (Test-Path $src)) { Write-Warning "missing $src"; continue }
  $args = @("-y", "-loglevel", "error", "-i", $src, "-vf", "scale=$($i.w):-2", "-lossless", "0", "-q:v", "75", "-preset", "picture", "-compression_level", "6", $dst)
  if ($DryRun) { Write-Host "[dry-run] ffmpeg $($args -join ' ')"; continue }
  $rc = Run-FFmpeg $args
  if ($rc -eq 0) {
    $oldSize = (Get-Item $src).Length
    $newSize = (Get-Item $dst).Length
    $ratio = [math]::Round($newSize / $oldSize * 100, 1)
    $line = $i.src.PadRight(42) + "  " + (Fmt-KB $oldSize) + " KB -> " + (Fmt-KB $newSize) + " KB  (" + $ratio + "%)"
    Write-Output $line
  }
}

# After PNG->WebP, the originals are no longer needed (WebP supports transparency and is universally supported by modern browsers / GitHub / Gitee).
# Cleanup step: git rm the original PNGs so the repo size stays small.
if (-not $DryRun) {
  $pngs = Get-ChildItem -Path $screenDir -Filter "*.png" -File
  if ($pngs.Count -gt 0) {
    Write-Output ""
    Write-Output "Removing original PNGs (replaced by WebP):"
    foreach ($p in $pngs) {
      Write-Output ("  rm " + $p.Name)
      Remove-Item -Force $p.FullName
    }
  }
}

# 2. GIF reencode
$gif = Join-Path $screenDir "dsh-desktop.gif"
$palette = Join-Path $screenDir "_tmp\palette.png"
$gifTmp = Join-Path $screenDir "_tmp\dsh-desktop.gif"
if (Test-Path $gif) {
  if (-not $DryRun) {
    $rc1 = Run-FFmpeg @("-y", "-loglevel", "error", "-i", $gif, "-vf", "scale=720:-2:flags=lanczos,fps=12,palettegen=max_colors=64:stats_mode=diff", $palette)
    if ($rc1 -eq 0) {
      $rc2 = Run-FFmpeg @("-y", "-loglevel", "error", "-i", $gif, "-i", $palette, "-filter_complex", "scale=720:-2:flags=lanczos,fps=12,paletteuse=dither=sierra2_4a:diff_mode=rectangle", "-loop", "0", $gifTmp)
      if ($rc2 -eq 0) {
        Move-Item -Force $gifTmp $gif
        Remove-Item $palette -ErrorAction SilentlyContinue
        $gifLine = "dsh-desktop.gif".PadRight(42) + "  -> " + (Fmt-KB (Get-Item $gif).Length) + " KB"
        Write-Output $gifLine
      }
    }
  } else { Write-Host "[dry-run] gif reencode" }
}

Remove-Item "$screenDir\_tmp" -Recurse -Force -ErrorAction SilentlyContinue

Write-Output ""
Write-Output "=== Summary ==="
$total = (Get-ChildItem -Path $screenDir -File | Measure-Object -Property Length -Sum).Sum
$totalMB = $total/1MB
$totalStr = $totalMB.ToString([System.Globalization.CultureInfo]::InvariantCulture)
Write-Output ("Total: {0} MB (was 19.67 MB)" -f $totalStr)
Write-Output "WebP sizes:"
Get-ChildItem -Path $screenDir -Filter "*.webp" -File | Sort-Object Length -Descending | ForEach-Object {
  $l = "  " + $_.Name.PadRight(42) + " " + (Fmt-KB $_.Length) + " KB"
  Write-Output $l
}