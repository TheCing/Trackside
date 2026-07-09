<#
.SYNOPSIS
    Enable or disable the whole mod stack (Trackside overlay + Hachimi +
    Hakuraku/horseACT) for Umamusume Pretty Derby in one go.

.DESCRIPTION
    The mods are chained off a single proxy DLL:

        version.dll           = Trackside's proxy  -> loads trackside.dll
                                                  -> forwards version APIs to trackside_version.dll
        trackside_version.dll = the genuine Windows version.dll (standalone),
                                OR Hachimi's proxy when running Hachimi (reads hachimi\config.json)
        hachimi\              = Hachimi config + the plugins it load_libraries:
                                  horseACT.dll  (Hakuraku exporter)
                                  CarrotBlender.dll (Uma Launcher bridge)

    "Disable" MOVES those files out of the game folder into a "_mods_disabled"
    subfolder, so the game launches completely clean. "Enable" moves them back.
    Nothing is ever deleted, and settings/logs/screenshots and ReShade are left
    untouched, so it is fully reversible.

.PARAMETER GameDir
    The game folder (where UmamusumePrettyDerby.exe lives). Defaults to the folder
    this script sits in, so the easiest use is to drop this .ps1 next to the game
    .exe and run it. Otherwise pass -GameDir "<path>".

.PARAMETER Action
    enable | disable | toggle (default: toggle).

.EXAMPLE
    # Dropped next to UmamusumePrettyDerby.exe:
    .\Toggle-TracksideStack.ps1

.EXAMPLE
    .\Toggle-TracksideStack.ps1 -GameDir "G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby" -Action disable
#>

[CmdletBinding()]
param(
    # Optional explicit game folder. If omitted, the script finds it automatically
    # (its own folder -> remembered path -> Steam auto-detect -> ask you once).
    [string]$GameDir = '',
    [ValidateSet('enable', 'disable', 'toggle')]
    [string]$Action = 'toggle'
)

$ErrorActionPreference = 'Stop'

$GameExe = 'UmamusumePrettyDerby.exe'

# Items that make up the stack. version.dll MUST stay first: it is both the master
# entry point and the file we use to detect the current state (present = enabled).
# The whole hachimi\ folder moves as one unit, taking Hachimi's config plus the
# horseACT (Hakuraku) and CarrotBlender plugins with it.
$Items = @(
    'version.dll',            # Trackside proxy (master loader)
    'trackside.dll',          # Trackside overlay
    'trackside_version.dll',  # genuine version.dll (standalone) OR Hachimi's proxy (Trackside forwards here)
    'hachimi'                 # Hachimi config + horseACT/Hakuraku + CarrotBlender
)

$DisabledDirName = '_mods_disabled'

function Fail($msg) {
    Write-Host "  ERROR: $msg" -ForegroundColor Red
    exit 1
}

# A folder is a valid game folder only if the game .exe is in it (the .exe is
# always present regardless of whether the mods are enabled or disabled).
# Uses pure .NET so it returns $false (never throws) for disconnected/unknown
# drives that can appear in Steam's library list.
function Test-GameDir($dir) {
    if ([string]::IsNullOrWhiteSpace($dir)) { return $false }
    try { return [System.IO.File]::Exists([System.IO.Path]::Combine($dir, $GameExe)) }
    catch { return $false }
}

# Steam install root, from the registry (covers default + custom Steam installs).
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

# Every Steam library folder (the main one + extra drives from libraryfolders.vdf).
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

# --- resolve the game folder ------------------------------------------------
# Where to remember a manually chosen path so we never have to ask twice.
$selfDir = if (-not [string]::IsNullOrWhiteSpace($PSScriptRoot)) { $PSScriptRoot } else { (Get-Location).Path }
$configFile = Join-Path $selfDir '.uma-gamedir.txt'

$resolved = $null
$how = ''

# 1) Explicit -GameDir wins, if it actually contains the game.
if (Test-GameDir $GameDir) { $resolved = $GameDir; $how = 'from -GameDir' }

# 2) The script's own folder (the "drop both files in the game folder" case).
elseif (Test-GameDir $selfDir) { $resolved = $selfDir; $how = 'script is in the game folder' }

# 3) A path we remembered from a previous run.
elseif ((Test-Path -LiteralPath $configFile) -and (Test-GameDir ((Get-Content -LiteralPath $configFile -Raw).Trim()))) {
    $resolved = (Get-Content -LiteralPath $configFile -Raw).Trim()
    $how = "remembered ($([System.IO.Path]::GetFileName($configFile)))"
}

# 4) Auto-detect through Steam's library config.
else {
    $auto = Find-GameDirViaSteam
    if (Test-GameDir $auto) { $resolved = $auto; $how = 'auto-detected via Steam' }
}

# 5) Last resort: ask the user, then remember the answer.
if (-not $resolved) {
    Write-Host ""
    Write-Host "Couldn't find your Umamusume folder automatically." -ForegroundColor Yellow
    Write-Host "Tip: the easiest fix is to put this script + .bat directly in the game folder."
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

if (-not $resolved) {
    Fail "Could not locate the game folder. Re-run with -GameDir ""<full path>""."
}

$GameDir = (Resolve-Path -LiteralPath $resolved).Path
$DisabledDir = Join-Path $GameDir $DisabledDirName

Write-Host ""
Write-Host "Trackside + Hachimi + Hakuraku stack toggler" -ForegroundColor Cyan
Write-Host "  Game folder: $GameDir  ($how)"

# --- refuse to touch locked DLLs while the game is running ------------------
if (Get-Process -Name 'UmamusumePrettyDerby' -ErrorAction SilentlyContinue) {
    Fail "The game is running. Close it first (the DLLs are locked while it runs)."
}

# --- detect current state ---------------------------------------------------
# Enabled = version.dll (the master proxy) is present in the game folder.
$enabledNow = Test-Path -LiteralPath (Join-Path $GameDir 'version.dll')
$disabledStashExists = Test-Path -LiteralPath (Join-Path $DisabledDir 'version.dll')

if ($Action -eq 'toggle') {
    $Action = if ($enabledNow) { 'disable' } else { 'enable' }
}

Write-Host ("  Current state: {0}" -f ($(if ($enabledNow) { 'ENABLED' } else { 'disabled' })))
Write-Host ("  Action:        {0}" -f $Action.ToUpper())
Write-Host ""

# --- helper: move one item between two folders if present ------------------
function Move-Item-IfPresent($name, $fromDir, $toDir) {
    $src = Join-Path $fromDir $name
    $dst = Join-Path $toDir $name
    if (-not (Test-Path -LiteralPath $src)) {
        Write-Host "  - $name (not found, skipped)" -ForegroundColor DarkGray
        return
    }
    if (Test-Path -LiteralPath $dst) {
        # Destination already has a stale copy; remove it so the move succeeds.
        Remove-Item -LiteralPath $dst -Recurse -Force
    }
    Move-Item -LiteralPath $src -Destination $dst -Force
    Write-Host "  - $name" -ForegroundColor Green
}

# --- apply ------------------------------------------------------------------
switch ($Action) {
    'disable' {
        if (-not $enabledNow) {
            Write-Host "  Already disabled. Nothing to do." -ForegroundColor Yellow
            exit 0
        }
        New-Item -ItemType Directory -Path $DisabledDir -Force | Out-Null
        foreach ($item in $Items) { Move-Item-IfPresent $item $GameDir $DisabledDir }
        Write-Host ""
        Write-Host "  Stack DISABLED. The game will now launch clean (vanilla)." -ForegroundColor Cyan
    }
    'enable' {
        if ($enabledNow) {
            Write-Host "  Already enabled. Nothing to do." -ForegroundColor Yellow
            exit 0
        }
        if (-not $disabledStashExists) {
            Fail "Nothing to enable: no '$DisabledDirName\version.dll' found. Are the mods installed?"
        }
        foreach ($item in $Items) { Move-Item-IfPresent $item $DisabledDir $GameDir }
        # Tidy up the stash folder if it is now empty.
        if ((Test-Path -LiteralPath $DisabledDir) -and -not (Get-ChildItem -LiteralPath $DisabledDir -Force)) {
            Remove-Item -LiteralPath $DisabledDir -Force
        }
        Write-Host ""
        Write-Host "  Stack ENABLED (Trackside + Hachimi + Hakuraku). Launch and press Insert." -ForegroundColor Cyan
    }
}

Write-Host ""
