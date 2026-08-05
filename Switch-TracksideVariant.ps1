<#
.SYNOPSIS
    Swap the installed Trackside between the PLAIN build and the HACHIMI-compatible build, so both
    can be tested without hand-editing anything.

.DESCRIPTION
    IMPORTANT - what the two variants actually are (it is easy to guess wrong):

    They are two BUILDS of the same DLL, not two loading mechanisms. BOTH are loaded by our own
    version.dll proxy, under the same name, `trackside.dll`:

        Trackside.zip          trackside.dll = default features        + version.dll + trackside_version.dll
        Trackside+Hachimi.zip  trackside.dll = --features hachimi      + version.dll + trackside_version.dll

    Identical three files; only the build differs (README "Install", and Release-Trackside.ps1:325,
    which packs the hachimi build INTO the zip under the name trackside.dll). `trackside_hh.dll` is
    a RELEASE ASSET NAME only - it exists so the self-updater can tell the variants apart
    (selfupdate.rs:136) - and never appears in a game folder.

    Hachimi is ORTHOGONAL and is never touched by this script. It hooks the game at its own entry
    point (winhttp.dll here; also unityplayer.dll or, pre-Edge, cri_mana_vpx.dll), which is a
    different file from our version.dll, so the two coexist with no renaming and no config changes.
    That is the supported, shipped arrangement - README: "keep your existing Hachimi install as-is".

    The `hachimi` build is not a different loader, then: it is the same overlay compiled to CEDE
    hooks Hachimi already owns (il2cpp.rs:367 - it detects an existing detour and stands aside,
    recording the cede in the arbiter) instead of double-detouring and corrupting trampolines.

    Do NOT be tempted to treat these as rival loaders - parking version.dll, disabling Hachimi, or
    registering us in Hachimi's `load_libraries` so it LoadLibraryW's us as a plugin. That last one
    does work end to end, and nothing ships that way: no release produces a trackside_hh.dll in a
    game folder and no README step adds a load_libraries entry, so it tests a configuration no user
    has while leaving the real one untested.

.PARAMETER Mode
    plain | hachimi | status  (default: status)   [`native` accepted as an alias for `plain`]

.PARAMETER Build
    Build the variant before installing it. Both builds get -Features; `hachimi` adds its own.

.PARAMETER Features
    Extra cargo features, comma separated (e.g. "devtools").

.PARAMETER GameDir
    Game folder. Default: remembered .uma-gamedir.txt next to this script, else Steam auto-detect.

.EXAMPLE
    .\Switch-TracksideVariant.ps1 status
.EXAMPLE
    .\Switch-TracksideVariant.ps1 hachimi -Build -Features "devtools"
.EXAMPLE
    .\Switch-TracksideVariant.ps1 plain -Build -Features "devtools"
#>
[CmdletBinding()]
param(
    [ValidateSet('plain','native','hachimi','status')]
    [string]$Mode = 'status',
    [switch]$Build,
    [string]$Features = '',
    [string]$GameDir = ''
)
$ErrorActionPreference = 'Stop'
if ($Mode -eq 'native') { $Mode = 'plain' }

$scriptDir = if ($PSScriptRoot) { $PSScriptRoot } else { Split-Path -Parent $PSCommandPath }
function Fail($m) { Write-Host "  ERROR: $m" -ForegroundColor Red; exit 1 }
function Step($m) { Write-Host ""; Write-Host "== $m" -ForegroundColor Cyan }

# --- game folder (same resolution order as Deploy-Trackside) ------------------
function Test-UmaDir($dir) {
    if ([string]::IsNullOrWhiteSpace($dir)) { return $false }
    try { return [System.IO.File]::Exists([System.IO.Path]::Combine($dir, 'UmamusumePrettyDerby.exe')) }
    catch { return $false }
}
if (-not (Test-UmaDir $GameDir)) {
    $remembered = Join-Path $scriptDir '.uma-gamedir.txt'
    if ((Test-Path -LiteralPath $remembered) -and (Test-UmaDir ((Get-Content -LiteralPath $remembered -Raw).Trim()))) {
        $GameDir = (Get-Content -LiteralPath $remembered -Raw).Trim()
    }
}
if (-not (Test-UmaDir $GameDir)) {
    $roots = New-Object System.Collections.Generic.List[string]
    foreach ($k in @('HKCU:\Software\Valve\Steam','HKLM:\SOFTWARE\WOW6432Node\Valve\Steam','HKLM:\SOFTWARE\Valve\Steam')) {
        try {
            $pr = Get-ItemProperty -Path $k -ErrorAction Stop
            $v = if ($pr.SteamPath) { $pr.SteamPath } else { $pr.InstallPath }
            if ($v -and (Test-Path -LiteralPath $v)) { $roots.Add((Resolve-Path -LiteralPath $v).Path) }
        } catch {}
    }
    foreach ($r in @($roots)) {
        $vdf = Join-Path $r 'steamapps\libraryfolders.vdf'
        if (Test-Path -LiteralPath $vdf) {
            foreach ($m in [regex]::Matches((Get-Content -LiteralPath $vdf -Raw), '"path"\s*"([^"]+)"')) {
                $roots.Add(($m.Groups[1].Value -replace '\\\\','\'))
            }
        }
    }
    foreach ($r in ($roots | Select-Object -Unique)) {
        $cand = [System.IO.Path]::Combine($r, 'steamapps', 'common', 'UmamusumePrettyDerby')
        if (Test-UmaDir $cand) { $GameDir = $cand; break }
    }
}
if (-not (Test-UmaDir $GameDir)) { Fail "Could not locate the game folder. Pass -GameDir '<path>'." }

$overlay   = Join-Path $GameDir 'trackside.dll'
$proxy     = Join-Path $GameDir 'version.dll'
$proxyFwd  = Join-Path $GameDir 'trackside_version.dll'
$parked    = Join-Path $GameDir '_variant_off'
# Hachimi Edge accepts exactly two proxy names (src/windows/hook.rs:50); cri_mana_vpx is the stale
# pre-Edge one the official docs still print. Detect all three so status can say what is loading it.
$HH_PROXIES = @('winhttp.dll','unityplayer.dll','cri_mana_vpx.dll')

# Which build is installed? The hachimi build embeds its own self-update asset name,
# "trackside_hh.dll" (selfupdate.rs:137); the plain build embeds "trackside.dll" instead. Verified
# against both builds before relying on it - and a byte scan of the file, not a guess from mtime.
function Test-Contains([string]$path, [string]$needle) {
    if (-not (Test-Path -LiteralPath $path)) { return $false }
    $b = [System.IO.File]::ReadAllBytes($path)
    $n = [System.Text.Encoding]::ASCII.GetBytes($needle)
    $lim = $b.Length - $n.Length
    for ($i = 0; $i -le $lim; $i++) {
        if ($b[$i] -eq $n[0]) {
            $ok = $true
            for ($k = 1; $k -lt $n.Length; $k++) { if ($b[$i + $k] -ne $n[$k]) { $ok = $false; break } }
            if ($ok) { return $true }
        }
    }
    return $false
}

function Get-HhConfig {
    $p = Join-Path $GameDir 'hachimi\config.json'
    if (Test-Path -LiteralPath $p) { return $p }
    return $null
}

function Get-Status {
    # Presence of the NAME proves nothing - all three are also legitimate DLLs, and unityplayer.dll
    # is the engine itself, so a name check claims "Hachimi installed" on every install forever,
    # even with Hachimi parked. Ask the file what it is: Hachimi's proxies carry ProductName
    # "Hachimi Edge" whatever they are renamed to, where the real UnityPlayer.dll says "Unity".
    $hhProxy = $null
    foreach ($n in $HH_PROXIES) {
        $p = Join-Path $GameDir $n
        if (-not (Test-Path -LiteralPath $p)) { continue }
        $product = ''
        try { $product = (Get-Item -LiteralPath $p).VersionInfo.ProductName } catch { }
        if ($product -like 'Hachimi*') { $hhProxy = $n; break }
    }
    $cfg = Get-HhConfig
    $stray = @()
    if ($cfg) {
        try {
            $j = Get-Content -LiteralPath $cfg -Raw | ConvertFrom-Json
            $stray = @(@($j.load_libraries) | Where-Object { $_ -like 'trackside*' })
        } catch { }
    }
    [pscustomobject]@{
        Overlay      = Test-Path -LiteralPath $overlay
        Variant      = if (Test-Path -LiteralPath $overlay) {
                           if (Test-Contains $overlay 'trackside_hh.dll') { 'hachimi' } else { 'plain' }
                       } else { $null }
        Proxy        = Test-Path -LiteralPath $proxy
        ProxyFwd     = Test-Path -LiteralPath $proxyFwd
        HhProxy      = $hhProxy
        HhConfig     = $cfg
        StrayEntries = $stray
    }
}

function Show-Status($s) {
    Write-Host ""
    Write-Host "  Game folder : $GameDir"
    Write-Host "  version.dll           : $(if ($s.Proxy) { 'present' } else { 'MISSING - nothing loads Trackside' })"
    Write-Host "  trackside_version.dll : $(if ($s.ProxyFwd) { 'present' } else { 'MISSING - the game will not start' })"
    Write-Host "  trackside.dll         : $(if ($s.Overlay) { 'present' } else { 'MISSING' })"
    Write-Host "  Hachimi               : $(if ($s.HhProxy) { "installed (loads via $($s.HhProxy))" } else { 'not installed' })"
    Write-Host ""
    Write-Host "  INSTALLED VARIANT: $(if ($s.Variant) { $s.Variant.ToUpper() } else { 'none' })" -ForegroundColor Green
    if ($s.Variant -eq 'plain' -and $s.HhProxy) {
        Write-Host "  NOTE: Hachimi is installed but this is the PLAIN build - the two will contend for" -ForegroundColor Yellow
        Write-Host "        shared hooks. Run: .\Switch-TracksideVariant.ps1 hachimi -Build" -ForegroundColor Yellow
    }
    if ($s.Variant -eq 'hachimi' -and -not $s.HhProxy) {
        Write-Host "  NOTE: hachimi build installed but Hachimi is not - harmless, just the plain build" -ForegroundColor Yellow
        Write-Host "        minus the hooks it cedes. Run: .\Switch-TracksideVariant.ps1 plain -Build" -ForegroundColor Yellow
    }
    if ($s.StrayEntries.Count) {
        Write-Host "  WARNING: Hachimi load_libraries still lists $($s.StrayEntries -join ', ')." -ForegroundColor Red
        Write-Host "           No release installs Trackside as a Hachimi plugin; that entry loads a" -ForegroundColor Red
        Write-Host "           SECOND copy of the overlay. Re-run with a mode to strip it." -ForegroundColor Red
    }
}

function Invoke-Build([string]$extra) {
    $feat = @()
    if ($Features) { $feat += $Features.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ } }
    if ($extra)    { $feat += $extra }
    $cargoArgs = @('build','--release','--manifest-path', (Join-Path $scriptDir 'native\Cargo.toml'))
    if ($feat.Count) { $cargoArgs += @('--features', ($feat -join ',')) }
    Write-Host "  cargo $($cargoArgs -join ' ')"
    & cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { Fail "cargo build failed." }
    Join-Path $scriptDir 'native\target\release\trackside.dll'
}

# Strip any Trackside entry from Hachimi's load_libraries. Registering us there loads a SECOND
# overlay next to the proxy-loaded one - two Present hooks, two imgui contexts.
function Clear-StrayEntries {
    $cfg = Get-HhConfig
    if (-not $cfg) { return }
    $j = Get-Content -LiteralPath $cfg -Raw | ConvertFrom-Json
    if (-not ($j.PSObject.Properties.Name -contains 'load_libraries')) { return }
    $keep = @(@($j.load_libraries) | Where-Object { $_ -notlike 'trackside*' })
    if ($keep.Count -eq @($j.load_libraries).Count) { return }
    $j.load_libraries = @($keep)
    Copy-Item -LiteralPath $cfg -Destination "$cfg.bak" -Force
    # UTF-8 with NO BOM: Hachimi parses with serde_json::from_str(fs::read_to_string(..)), which
    # FAILS on a BOM (core/hachimi.rs:192), and PowerShell 5.1's -Encoding UTF8 writes one.
    [System.IO.File]::WriteAllText($cfg, ($j | ConvertTo-Json -Depth 20), (New-Object System.Text.UTF8Encoding($false)))
    Write-Host "  stripped Trackside from Hachimi load_libraries (not a shipped install path)"
}

# Older runs of this script parked these; put them back rather than leaving a half install.
function Restore-Parked([string]$name) {
    $live = Join-Path $GameDir $name
    $off  = Join-Path $parked $name
    if ((-not (Test-Path -LiteralPath $live)) -and (Test-Path -LiteralPath $off)) {
        Move-Item -LiteralPath $off -Destination $live -Force
        Write-Host "  restored $name from _variant_off\"
    }
}

if (Get-Process -Name UmamusumePrettyDerby -ErrorAction SilentlyContinue) {
    Fail "Game is running - close it first (the DLLs are locked)."
}

if ($Mode -eq 'status') { Show-Status (Get-Status); return }

Step "Installing the $($Mode.ToUpper()) build"
$built = if ($Build) { Invoke-Build $(if ($Mode -eq 'hachimi') { 'hachimi' } else { $null }) }
         else { Join-Path $scriptDir 'native\target\release\trackside.dll' }
if (-not (Test-Path -LiteralPath $built)) { Fail "No built DLL at $built - pass -Build." }
# Guard against installing the wrong build: without -Build this is whatever was compiled last.
$isHh = Test-Contains $built 'trackside_hh.dll'
if ($isHh -ne ($Mode -eq 'hachimi')) {
    Fail "$built is the $(if ($isHh) { 'hachimi' } else { 'plain' }) build, not $Mode. Re-run with -Build."
}

Restore-Parked 'version.dll'
Restore-Parked 'trackside_version.dll'
Clear-StrayEntries

# Deploy through Deploy-Trackside.ps1, never a raw copy: it backs the current DLL up to
# trackside.dll.prev (so -Rollback works) and refuses a build that has silently lost a feature the
# installed one had - which is exactly how it caught the hachimi build shipping without the GL
# advisor UI.
& (Join-Path $scriptDir 'Deploy-Trackside.ps1') -Source $built -GameDir $GameDir
# NOT $LASTEXITCODE: that only tracks NATIVE processes, so after a successful `cargo` it stays 0
# and would wave a failed deploy straight through. Deploy-Trackside throws on failure, and
# $ErrorActionPreference = 'Stop' at the top makes that terminate us - so reaching here IS the
# success check. Verify the file actually landed rather than trusting either.
if (-not (Test-Path -LiteralPath $overlay)) { Fail "deploy reported success but $overlay is missing." }

Show-Status (Get-Status)
Write-Host ""
Write-Host "  Undo: .\Switch-TracksideVariant.ps1 $(if ($Mode -eq 'plain') { 'hachimi' } else { 'plain' }) -Build" -ForegroundColor DarkGray
Write-Host ""
