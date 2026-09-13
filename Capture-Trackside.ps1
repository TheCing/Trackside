<#
  Capture-Trackside.ps1 - screenshot the overlay from the preview host, without the game and
  without stealing focus. The first half of the design loop (see docs-internal/08-ui-cookbook.md,
  "Design canvas loop"): capture the real panel -> put it on a design canvas beside a draft.

    .\Capture-Trackside.ps1 -Tab "Race Director" -Scroll 0,6 -Prefix rd
        -> rd-0.png, rd-6.png (one per scroll depth, cumulative wheel ticks)
    .\Capture-Trackside.ps1 -Tab "" -Prefix win
        -> menu closed: floating windows only (Race summary, Optimizer, Oracle mocks)
    .\Capture-Trackside.ps1 -Tab "Plugins" -Prefix p -Crop 470,100,470,620 -Jpeg -MaxKB 70
        -> also writes p-0.crop.jpg, cropped and shrunk under 70 KB (a design-canvas image)
    .\Capture-Trackside.ps1 -Tab "" -Prefix rs -Background RaceResults
        -> the overlay drawn OVER a real game screen. A bare name is looked up in -ScreensDir
           (Night's ShareX folder by default: HomeScreen, MidRace, RaceResults); a path works too.
    .\Capture-Trackside.ps1 -Tab "" -Prefix rs -Background RaceResults -Mock rsum
        -> only the Race summary posed, so one window sits over the result screen, not three.

  Every mock is on (TRACKSIDE_*_MOCK), so panels pose mid-run. Needs a built
  native\target\release\trackside.dll and preview-host\target\release\trackside-preview-host.exe
  (run .\Preview-Trackside.ps1 once to build the host). Output lands in -OutDir (default: cwd).
#>
param(
  [string]$Tab = 'Gameplay',
  [int[]]$Scroll = @(0),
  [string]$Prefix = 'shot',
  [string]$OutDir = (Get-Location).Path,
  [int[]]$Crop,            # x,y,w,h in capture pixels; also writes <name>.crop.(png|jpg)
  [switch]$Jpeg,           # crop as JPEG (design-canvas images must stay small)
  [int]$MaxKB = 70,        # JPEG: lower quality until under this size
  [int]$X = 640, [int]$Y = 400,
  [int]$SettleMs = 4500,
  [string]$Mock = 'all',       # which panels pose mid-run: all | none | comma list of evoracle,skopt,roomfinder,roomwatch,ttplay,horseact,rsum
  [string]$Background = '',   # game screenshot (png/jpg path, or a name found in -ScreensDir)
  [string]$ScreensDir = 'C:\Users\jptyn\OneDrive\Documents\ShareX\Screenshots\2026-09'
)
$ErrorActionPreference = 'Stop'
$repo = $PSScriptRoot
$exe  = Join-Path $repo 'preview-host\target\release\trackside-preview-host.exe'
$dll  = Join-Path $repo 'native\target\release\trackside.dll'
if (-not (Test-Path $exe)) { throw "preview host not built - run .\Preview-Trackside.ps1 once" }
if (-not (Test-Path $dll)) { throw "no built DLL at $dll" }

Add-Type -AssemblyName System.Drawing
Add-Type @'
using System; using System.Runtime.InteropServices;
public static class CapW {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
  [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint m, IntPtr w, IntPtr l);
  [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
}
'@

function Capture-Client([IntPtr]$h, [string]$out) {
  $cr = New-Object CapW+RECT; [void][CapW]::GetClientRect($h, [ref]$cr)
  $w = $cr.R - $cr.L; $ht = $cr.B - $cr.T
  if ($w -le 0 -or $ht -le 0) { throw "window has no client area" }
  $bmp = New-Object System.Drawing.Bitmap $w, $ht
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $hdc = $g.GetHdc()
  # PW_CLIENTONLY | PW_RENDERFULLCONTENT: DWM-composited content, occluded or not - no focus theft.
  $ok = [CapW]::PrintWindow($h, $hdc, 3)
  $g.ReleaseHdc($hdc); $g.Dispose()
  $bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  "saved $out ($w x $ht$(if (-not $ok) { ', printwindow FAILED' }))"
}

function Crop-Image([string]$src, [int[]]$box, [bool]$jpeg, [int]$maxKb) {
  $img = [System.Drawing.Image]::FromFile($src)
  $r = New-Object System.Drawing.Rectangle $box[0], $box[1], $box[2], $box[3]
  $c = (New-Object System.Drawing.Bitmap $img).Clone($r, [System.Drawing.Imaging.PixelFormat]::Format24bppRgb)
  $img.Dispose()
  $base = [IO.Path]::ChangeExtension($src, $null).TrimEnd('.')
  if (-not $jpeg) {
    $out = "$base.crop.png"; $c.Save($out, [System.Drawing.Imaging.ImageFormat]::Png); $c.Dispose()
    return "cropped $out"
  }
  $out = "$base.crop.jpg"
  $codec = [System.Drawing.Imaging.ImageCodecInfo]::GetImageEncoders() | Where-Object { $_.MimeType -eq 'image/jpeg' }
  foreach ($q in 92, 88, 84, 80, 75, 70, 62) {
    $p = New-Object System.Drawing.Imaging.EncoderParameters 1
    $p.Param[0] = New-Object System.Drawing.Imaging.EncoderParameter ([System.Drawing.Imaging.Encoder]::Quality), ([long]$q)
    $c.Save($out, $codec, $p)
    if ((Get-Item $out).Length -le $maxKb * 1024) { break }
  }
  $c.Dispose()
  return "cropped $out ($([int]((Get-Item $out).Length / 1024)) KB, q$q)"
}

# Backdrop: the host reads a raw file (u32 w, u32 h, then RGBA8) so it needs no image decoder.
# Convert here with System.Drawing; GDI hands back BGRA, so the channels are swapped on the way.
function Convert-Backdrop([string]$spec, [string]$dir) {
  $src = $spec
  if (-not (Test-Path -LiteralPath $src)) {
    $hit = Get-ChildItem -LiteralPath $dir -File -ErrorAction SilentlyContinue |
      Where-Object { $_.BaseName -ieq $spec -and $_.Extension -match '^\.(png|jpe?g|bmp)$' } | Select-Object -First 1
    if (-not $hit) { throw "backdrop '$spec' is neither a file nor a name in $dir" }
    $src = $hit.FullName
  }
  $img = [System.Drawing.Image]::FromFile($src)
  $bmp = New-Object System.Drawing.Bitmap $img.Width, $img.Height, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $g = [System.Drawing.Graphics]::FromImage($bmp); $g.DrawImage($img, 0, 0, $img.Width, $img.Height); $g.Dispose(); $img.Dispose()
  $rect = New-Object System.Drawing.Rectangle 0, 0, $bmp.Width, $bmp.Height
  $data = $bmp.LockBits($rect, [System.Drawing.Imaging.ImageLockMode]::ReadOnly, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
  $n = $data.Stride * $bmp.Height
  $bgra = New-Object byte[] $n
  [System.Runtime.InteropServices.Marshal]::Copy($data.Scan0, $bgra, 0, $n)
  $bmp.UnlockBits($data)
  $w = $bmp.Width; $h = $bmp.Height; $stride = $data.Stride; $bmp.Dispose()
  $rgba = New-Object byte[] ($w * $h * 4)
  for ($y = 0; $y -lt $h; $y++) {
    $ro = $y * $stride; $wo = $y * $w * 4
    for ($x = 0; $x -lt $w; $x++) {
      $i = $ro + $x * 4; $o = $wo + $x * 4
      $rgba[$o] = $bgra[$i + 2]; $rgba[$o + 1] = $bgra[$i + 1]; $rgba[$o + 2] = $bgra[$i]; $rgba[$o + 3] = 255
    }
  }
  $out = Join-Path $env:TEMP ("trackside-backdrop-" + [IO.Path]::GetFileNameWithoutExtension($src) + ".rgba")
  $fs = [IO.File]::Create($out)
  $fs.Write([BitConverter]::GetBytes([uint32]$w), 0, 4); $fs.Write([BitConverter]::GetBytes([uint32]$h), 0, 4)
  $fs.Write($rgba, 0, $rgba.Length); $fs.Close()
  "backdrop: $src ($w x $h) -> $out"
  return $out
}

Get-Process -Name trackside-preview-host -ErrorAction SilentlyContinue | Stop-Process -Force
if ($Background) { $env:TRACKSIDE_PREVIEW_BG = (Convert-Backdrop $Background $ScreensDir | Select-Object -Last 1) } else { Remove-Item Env:\TRACKSIDE_PREVIEW_BG -ErrorAction SilentlyContinue }
Start-Sleep -Milliseconds 400
$allMocks = 'EVORACLE','SKOPT','ROOMFINDER','ROOMWATCH','TTPLAY','HORSEACT','RSUM'
$want = switch ($Mock.ToLower()) { 'all' { $allMocks } 'none' { @() } default { ($Mock -split ',') | ForEach-Object { $_.Trim().ToUpper() } } }
foreach ($m in $allMocks) { if ($want -contains $m) { Set-Item -Path "Env:TRACKSIDE_${m}_MOCK" -Value '1' } else { Remove-Item "Env:TRACKSIDE_${m}_MOCK" -ErrorAction SilentlyContinue } }
if ($Tab) { $env:TRACKSIDE_PREVIEW_OPEN = $Tab } else { Remove-Item Env:\TRACKSIDE_PREVIEW_OPEN -ErrorAction SilentlyContinue }
# Not minimized: a host started minimized never presents a frame and captures black.
$p = Start-Process -FilePath $exe -ArgumentList "`"$dll`"" -PassThru
Start-Sleep -Milliseconds $SettleMs
$p.Refresh(); $h = $p.MainWindowHandle
if ($h -eq [IntPtr]::Zero) { Start-Sleep -Milliseconds 1500; $p.Refresh(); $h = $p.MainWindowHandle }
if ($h -eq [IntPtr]::Zero) { Stop-Process -Id $p.Id -Force; throw "host window never appeared (crashed? see native\target\release\trackside-logs\trackside-crash.log)" }
[void][CapW]::ShowWindow($h, 4)   # SW_SHOWNOACTIVATE
Start-Sleep -Milliseconds 700
$wr = New-Object CapW+RECT; [void][CapW]::GetWindowRect($h, [ref]$wr)
$done = 0
foreach ($s in $Scroll) {
  for ($i = 0; $i -lt ($s - $done); $i++) {
    $lp = [IntPtr](($Y -shl 16) -bor $X)
    [void][CapW]::PostMessage($h, 0x0200, [IntPtr]0, $lp)                                  # WM_MOUSEMOVE
    $wl = [IntPtr]((($wr.T + $Y) -shl 16) -bor (($wr.L + $X) -band 0xFFFF))
    $wp = [IntPtr]((-120 -shl 16) -band 0xFFFFFFFF)                                       # one notch down
    [void][CapW]::PostMessage($h, 0x020A, $wp, $wl)                                        # WM_MOUSEWHEEL
    Start-Sleep -Milliseconds 60
  }
  $done = $s
  Start-Sleep -Milliseconds 900
  $out = Join-Path $OutDir "$Prefix-$s.png"
  Capture-Client $h $out
  if ($Crop) { Crop-Image $out $Crop $Jpeg.IsPresent $MaxKB }
}
Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
