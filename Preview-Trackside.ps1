<#
.SYNOPSIS
    Preview the Trackside overlay's visuals WITHOUT the game.

.DESCRIPTION
    Builds trackside.dll and a tiny D3D11 host window, then launches the host
    which loads the overlay into itself. hudhook hooks the host's swapchain just
    like in-game, so the real sidebar menu, fonts, textures and animations render
    in a normal window - no Umamusume required. Press Insert in the window for the
    menu (same hotkey as in-game).

    The overlay's engine thread idles (no GameAssembly.dll), so game-backed panels
    (FPS, race telemetry, Team Trials) just show empty/default states. This is for
    iterating on visuals/layout, not game data.

.PARAMETER SkipBuild
    Don't rebuild the overlay DLL; just (re)build the host and launch. Use when you
    only changed the host, or already built the DLL.

.PARAMETER DebugBuild
    Build the overlay with the debug profile (faster compile) instead of --release.

.PARAMETER NoBanner
    Build the overlay without the `banner` feature (intro video/audio + sidebar art).

.PARAMETER Features
    Override the overlay's cargo feature set, e.g. -Features "racenet,races_on,freecam".
    Implies --no-default-features.

.EXAMPLE
    .\Preview-Trackside.ps1

.EXAMPLE
    # Iterate on the host only, reuse the last-built DLL:
    .\Preview-Trackside.ps1 -SkipBuild
#>

[CmdletBinding()]
param(
    [switch]$SkipBuild,
    [switch]$DebugBuild,
    [switch]$NoBanner,
    [string]$Features = '',
    # Open a window prepopulated with fabricated data for visual iteration (no game needed).
    # One of: 'skopt' (Skill Optimizer). Empty = none.
    [string]$Mock = ''
)

$ErrorActionPreference = 'Stop'

function Fail($msg) {
    Write-Host "  ERROR: $msg" -ForegroundColor Red
    exit 1
}

$repoDir   = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { $PSScriptRoot } else { (Get-Location).Path }
$nativeDir = Join-Path $repoDir 'native'
$hostDir   = Join-Path $repoDir 'preview-host'

if (-not (Test-Path -LiteralPath (Join-Path $nativeDir 'Cargo.toml'))) { Fail "Can't find native\Cargo.toml." }
if (-not (Test-Path -LiteralPath (Join-Path $hostDir 'Cargo.toml')))   { Fail "Can't find preview-host\Cargo.toml." }
if (-not (Get-Command cargo -ErrorAction SilentlyContinue))            { Fail "cargo not found on PATH." }

Write-Host ""
Write-Host "Trackside overlay - standalone preview" -ForegroundColor Cyan
Write-Host ""

# --- build the overlay DLL --------------------------------------------------
$profileName = if ($DebugBuild) { 'debug' } else { 'release' }

if (-not $SkipBuild) {
    $cargoArgs = @('build')
    if (-not $DebugBuild) { $cargoArgs += '--release' }
    if ($Features) {
        $cargoArgs += @('--no-default-features', '--features', $Features)
    } elseif ($NoBanner) {
        $cargoArgs += @('--no-default-features', '--features', 'racenet,races_on,freecam')
    }
    Write-Host "  Building overlay: cargo $($cargoArgs -join ' ')" -ForegroundColor DarkGray
    Push-Location $nativeDir
    try { & cargo @cargoArgs; $code = $LASTEXITCODE } finally { Pop-Location }
    if ($code -ne 0) { Fail "overlay build failed (cargo exit $code)." }
}

$dll = Join-Path $nativeDir "target\$profileName\trackside.dll"
if (-not (Test-Path -LiteralPath $dll)) {
    Fail "trackside.dll not found at $dll. Build it (drop -SkipBuild)."
}

# --- build the host ---------------------------------------------------------
Write-Host "  Building host: cargo build --release" -ForegroundColor DarkGray
Push-Location $hostDir
try { & cargo build --release; $code = $LASTEXITCODE } finally { Pop-Location }
if ($code -ne 0) { Fail "host build failed (cargo exit $code)." }

$exe = Join-Path $hostDir 'target\release\trackside-preview-host.exe'
if (-not (Test-Path -LiteralPath $exe)) { Fail "host exe not found at $exe." }

# --- mock data (read by the DLL at load; inherited by the host process) -----
if ($Mock -eq 'skopt') { $env:TRACKSIDE_SKOPT_MOCK = '1' }
if ($Mock) { Write-Host "  Mock: $Mock" -ForegroundColor Magenta }

# --- launch -----------------------------------------------------------------
$dllFull = (Resolve-Path -LiteralPath $dll).Path
Write-Host ""
Write-Host "  Launching preview host with:" -ForegroundColor Green
Write-Host "    $dllFull"
Write-Host "  (Press Insert in the window for the menu; close the window to quit.)"
Write-Host ""

& $exe $dllFull
