<#
.SYNOPSIS
    Build the Trackside overlay DLL and deploy it into the game folder, for fast
    dev iteration.

.DESCRIPTION
    Runs `cargo build` on native\ and copies the resulting trackside.dll
    into the Umamusume game folder (next to UmamusumePrettyDerby.exe), so the
    inner loop is just: edit -> run this script -> launch the game -> press Insert.

    Only trackside.dll is touched. The proxy (version.dll), its forward target
    (trackside_version.dll), and — if used — Hachimi must already be installed once
    from a release zip; after that you never replace them again.

    The DLL is locked while the game runs, so the script refuses to copy if the
    game is open (use -Kill to close it first). It can optionally relaunch the
    game for you with -Launch.

    Game-folder resolution matches Toggle-TracksideStack.ps1 and shares the same
    remembered-path file (.uma-gamedir.txt), so if you've used the toggler the
    path is already known.

.PARAMETER GameDir
    Explicit game folder (where UmamusumePrettyDerby.exe lives). If omitted, the
    script auto-resolves: -GameDir -> remembered path -> Steam auto-detect -> ask.

.PARAMETER DebugBuild
    Build the debug profile (faster to compile) instead of --release. The release
    profile uses LTO + codegen-units=1, so it links slowly; debug is handy for
    quick error-checking, release for actually testing in-game.

.PARAMETER Check
    Run `cargo check` only (fastest compile-error feedback) and skip the copy.

.PARAMETER NoBanner
    Build without the `banner` feature (the intro video/audio + sidebar art).
    Speeds up the build for pure logic work. Note this changes the menu's look.

.PARAMETER Features
    Override the cargo feature set entirely, e.g. -Features "racenet,races_on,freecam".
    Implies --no-default-features. Mutually exclusive with -NoBanner.

.PARAMETER Launch
    Start the game after a successful copy.

.PARAMETER Kill
    If the game is running, close it first (so the locked DLL can be replaced).

.EXAMPLE
    # Build release, copy into the game folder:
    .\Build-Trackside.ps1

.EXAMPLE
    # Fast compile-error check, no copy:
    .\Build-Trackside.ps1 -Check

.EXAMPLE
    # Close the game, rebuild, copy, relaunch - full one-shot iteration:
    .\Build-Trackside.ps1 -Kill -Launch
#>

[CmdletBinding()]
param(
    [string]$GameDir = '',
    [switch]$DebugBuild,
    [switch]$Check,
    [switch]$NoBanner,
    [string]$Features = '',
    [switch]$Launch,
    [switch]$Kill
)

$ErrorActionPreference = 'Stop'

$GameExe  = 'UmamusumePrettyDerby.exe'
$DllName  = 'trackside.dll'
$ProcName = 'UmamusumePrettyDerby'

function Fail($msg) {
    Write-Host "  ERROR: $msg" -ForegroundColor Red
    exit 1
}

# A folder is a valid game folder only if the game .exe is in it. Pure .NET so it
# returns $false (never throws) for disconnected/unknown drives.
function Test-GameDir($dir) {
    if ([string]::IsNullOrWhiteSpace($dir)) { return $false }
    try { return [System.IO.File]::Exists([System.IO.Path]::Combine($dir, $GameExe)) }
    catch { return $false }
}

# Steam install root, from the registry (default + custom installs).
function Get-SteamRoot {
    foreach ($k in @(
            'HKCU:\Software\Valve\Steam',
            'HKLM:\SOFTWARE\WOW6432Node\Valve\Steam',
            'HKLM:\SOFTWARE\Valve\Steam')) {
        try {
            $p = Get-ItemProperty -Path $k -ErrorAction Stop
            $val = if ($p.SteamPath) { $p.SteamPath } else { $p.InstallPath }
            if ($val -and (Test-Path -LiteralPath $val)) {
                return (Resolve-Path -LiteralPath $val).Path
            }
        } catch {}
    }
    return $null
}

# Every Steam library folder (main one + extra drives from libraryfolders.vdf).
function Get-SteamLibraries {
    $root = Get-SteamRoot
    $libs = New-Object System.Collections.Generic.List[string]
    if ($root) {
        $libs.Add($root)
        $vdf = Join-Path $root 'steamapps\libraryfolders.vdf'
        if (Test-Path -LiteralPath $vdf) {
            $txt = Get-Content -LiteralPath $vdf -Raw
            foreach ($m in [regex]::Matches($txt, '"path"\s*"([^"]+)"')) {
                $libs.Add(($m.Groups[1].Value -replace '\\\\', '\'))
            }
        }
    }
    return $libs | Select-Object -Unique
}

# Walk the Steam libraries looking for the game's common\ folder.
function Find-GameDirViaSteam {
    foreach ($lib in (Get-SteamLibraries)) {
        $cand = [System.IO.Path]::Combine($lib, 'steamapps', 'common', 'UmamusumePrettyDerby')
        if (Test-GameDir $cand) { return $cand }
    }
    return $null
}

# --- locate the repo + cargo ------------------------------------------------
$repoDir = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { $PSScriptRoot } else { (Get-Location).Path }
$nativeDir = Join-Path $repoDir 'native'
if (-not (Test-Path -LiteralPath (Join-Path $nativeDir 'Cargo.toml'))) {
    Fail "Can't find native\Cargo.toml next to this script. Run it from the repo root."
}
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    Fail "cargo not found on PATH. Install Rust (stable, MSVC toolchain) from https://rustup.rs."
}

# --- build ------------------------------------------------------------------
$profileName = if ($DebugBuild) { 'debug' } else { 'release' }

$cargoArgs = @(if ($Check) { 'check' } else { 'build' })
if (-not $DebugBuild) { $cargoArgs += '--release' }
if ($Features) {
    $cargoArgs += @('--no-default-features', '--features', $Features)
} elseif ($NoBanner) {
    $cargoArgs += @('--no-default-features', '--features', 'racenet,races_on,freecam')
}

Write-Host ""
Write-Host "Trackside overlay - build and deploy" -ForegroundColor Cyan
Write-Host "  cargo $($cargoArgs -join ' ')  (in native\)"
Write-Host ""

Push-Location $nativeDir
try {
    & cargo @cargoArgs
    $code = $LASTEXITCODE
} finally {
    Pop-Location
}
if ($code -ne 0) { Fail "cargo exited with code $code." }

if ($Check) {
    Write-Host ""
    Write-Host "  Check passed (no DLL produced)." -ForegroundColor Green
    Write-Host ""
    exit 0
}

$dllPath = Join-Path $nativeDir "target\$profileName\$DllName"
if (-not (Test-Path -LiteralPath $dllPath)) {
    Fail "Build succeeded but $DllName not found at $dllPath."
}

# --- resolve the game folder (shares Toggle-TracksideStack's remembered path) ---
$configFile = Join-Path $repoDir '.uma-gamedir.txt'
$resolved = $null
$how = ''

if (Test-GameDir $GameDir) { $resolved = $GameDir; $how = 'from -GameDir' }
elseif ((Test-Path -LiteralPath $configFile) -and (Test-GameDir ((Get-Content -LiteralPath $configFile -Raw).Trim()))) {
    $resolved = (Get-Content -LiteralPath $configFile -Raw).Trim()
    $how = "remembered ($([System.IO.Path]::GetFileName($configFile)))"
}
else {
    $auto = Find-GameDirViaSteam
    if (Test-GameDir $auto) { $resolved = $auto; $how = 'auto-detected via Steam' }
}

if (-not $resolved) {
    Write-Host ""
    Write-Host "Couldn't find your Umamusume folder automatically." -ForegroundColor Yellow
    for ($i = 0; $i -lt 3 -and -not $resolved; $i++) {
        $entry = (Read-Host "Paste the full path to the UmamusumePrettyDerby folder").Trim('"', ' ')
        if (Test-GameDir $entry) {
            $resolved = $entry
            $how = 'entered by you'
            try { Set-Content -LiteralPath $configFile -Value ((Resolve-Path -LiteralPath $entry).Path) -Encoding ASCII } catch {}
        }
        elseif ($entry) {
            Write-Host "  Not a valid game folder (no $GameExe there). Try again." -ForegroundColor Red
        }
    }
}
if (-not $resolved) { Fail 'Could not locate the game folder. Re-run with -GameDir "<full path>".' }

$GameDir = (Resolve-Path -LiteralPath $resolved).Path
Write-Host ""
Write-Host "  Game folder: $GameDir  ($how)"

# --- handle a running game (the DLL is locked while it runs) -----------------
$proc = Get-Process -Name $ProcName -ErrorAction SilentlyContinue
if ($proc) {
    if ($Kill) {
        Write-Host "  Game is running - closing it (-Kill)..." -ForegroundColor Yellow
        $proc | Stop-Process -Force
        Start-Sleep -Milliseconds 1500
    } else {
        Fail "The game is running ($DllName is locked). Close it, or re-run with -Kill."
    }
}

# --- deploy -----------------------------------------------------------------
# Warn (don't fail) if the proxy loader isn't installed yet - the overlay won't
# load without it, but copying the DLL is still harmless.
if (-not (Test-Path -LiteralPath (Join-Path $GameDir 'version.dll'))) {
    Write-Host "  NOTE: version.dll not found in the game folder - install the proxy" -ForegroundColor Yellow
    Write-Host "        loaders once from a release zip, or the overlay won't load." -ForegroundColor Yellow
}

$dest = Join-Path $GameDir $DllName
Copy-Item -LiteralPath $dllPath -Destination $dest -Force
$size = [math]::Round((Get-Item -LiteralPath $dest).Length / 1MB, 2)
Write-Host "  Copied $DllName ($size MB, $profileName) -> game folder" -ForegroundColor Green

# --- optionally launch ------------------------------------------------------
if ($Launch) {
    Write-Host "  Launching the game..." -ForegroundColor Cyan
    Start-Process -FilePath (Join-Path $GameDir $GameExe)
}

Write-Host ""
Write-Host "  Done. Launch the game (Windowed/Borderless) and press Insert." -ForegroundColor Cyan
Write-Host ""
