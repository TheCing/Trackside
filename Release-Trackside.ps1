<#
.SYNOPSIS
    Cut a PUBLIC Trackside release: build both variants, hash, package, tag, and publish.

.DESCRIPTION
    The one script for a release. It is version-driven — the version always comes from
    native\Cargo.toml, never a hardcoded string — so it works for any release.

    Order of operations:
      1. GUARDS   - clean tree, no TRACKSIDE_DEV in the environment, and (after building)
                    the public DLL must NOT contain the Event Oracle sentinel. That last
                    check is the important one: it makes it impossible to publish the
                    private build by accident.
      2. BUILD    - native default            -> trackside.dll
                    native --features hachimi -> trackside_hh.dll
                    proxy                     -> version.dll
                    %WINDIR%\System32\version.dll -> trackside_version.dll (proxy's forward target)
                    Built WITHOUT TRACKSIDE_DEV so the in-game self-updater stays live for users.
      3. HASH     - FNV-1a/64 as 16 hex chars into <dll>.hash. selfupdate.rs compares these
                    for the same-tag hotfix check, so they must ship with the loose DLLs.
      4. PACKAGE  - Trackside.zip          = trackside.dll + version.dll + trackside_version.dll
                    Trackside+Hachimi.zip  = trackside_hh.dll (as trackside.dll) + version.dll
      5. STAGE    - everything into release-v<version>\ alongside NOTES.md
      6. RELEASE  - create the git tag v<version>, then a GitHub release via gh with all
                    assets attached.

    Publishing is opt-in. Without -Publish you get a DRAFT release you can review and
    publish from the GitHub UI; the tag is created locally but not pushed.

.PARAMETER Notes
    Release-notes markdown. Default: release-v<version>\NOTES.md (a release needs notes —
    the in-game updater renders them as the changelog).
.PARAMETER Publish
    Publish for real: pushes the tag and creates a published (non-draft) GitHub release.
.PARAMETER StageOnly
    Build/hash/package only. No git tag, no GitHub interaction.
.PARAMETER SkipBuild
    Reuse the DLLs already staged in release-v<version>\ (re-package / re-tag only).
.PARAMETER Force
    Proceed even if the working tree is dirty.

.EXAMPLE
    # Build + package + draft release for review:
    .\Release-Trackside.ps1
.EXAMPLE
    # Everything, published live:
    .\Release-Trackside.ps1 -Publish
.EXAMPLE
    # Just produce the artifacts, touch nothing remote:
    .\Release-Trackside.ps1 -StageOnly
#>
[CmdletBinding()]
param(
    [string]$Notes = '',
    [switch]$Publish,
    [switch]$StageOnly,
    [switch]$SkipBuild,
    [switch]$Force
)
$ErrorActionPreference = 'Stop'

$RepoDir   = if ($PSScriptRoot) { $PSScriptRoot } else { (Get-Location).Path }
$NativeDir = Join-Path $RepoDir 'native'
$ProxyDir  = Join-Path $RepoDir 'proxy'
$ORACLE_SENTINEL = 'event_oracle'   # present ONLY in private builds

function Fail($m) { Write-Host "  ERROR: $m" -ForegroundColor Red; exit 1 }
function Step($m) { Write-Host ""; Write-Host "== $m" -ForegroundColor Cyan }

# FNV-1a/64 in C# — ulong math is unchecked (wraps) and this runs over ~10MB per DLL, which
# is far too slow in pure PowerShell.
if (-not ('TracksideFnv' -as [type])) {
    Add-Type -TypeDefinition @"
    public static class TracksideFnv {
        public static string Hash(byte[] d) {
            ulong h = 14695981039346656037UL;
            for (int i = 0; i < d.Length; i++) { h ^= d[i]; h *= 1099511628211UL; }
            return h.ToString("x16");
        }
    }
"@
}
function Get-Fnv1a([string]$path) { [TracksideFnv]::Hash([System.IO.File]::ReadAllBytes($path)) }

function Test-DllHas([string]$path, [string]$needle) {
    if (-not (Test-Path -LiteralPath $path)) { return $false }
    $text = [System.Text.Encoding]::GetEncoding('ISO-8859-1').GetString([System.IO.File]::ReadAllBytes($path))
    return $text.IndexOf($needle, [System.StringComparison]::Ordinal) -ge 0
}

# --- version (single source of truth: native\Cargo.toml) ---------------------
$cargoToml = Join-Path $NativeDir 'Cargo.toml'
if (-not (Test-Path -LiteralPath $cargoToml)) { Fail "native\Cargo.toml not found — run from the repo root." }
$verLine = (Get-Content -LiteralPath $cargoToml) | Where-Object { $_ -match '^\s*version\s*=\s*"' } | Select-Object -First 1
if ($verLine -notmatch '"([^"]+)"') { Fail "Couldn't read version from native\Cargo.toml." }
$Version   = $Matches[1]
$Tag       = "v$Version"
$StageDir  = Join-Path $RepoDir "release-$Tag"
$branch    = (& git -C $RepoDir rev-parse --abbrev-ref HEAD 2>$null)
$commit    = (& git -C $RepoDir rev-parse --short HEAD 2>$null)

Write-Host ""
Write-Host "Trackside release $Tag" -ForegroundColor Cyan
Write-Host "  branch : $branch @ $commit"
Write-Host "  stage  : $StageDir"

# --- guards ------------------------------------------------------------------
Step "Guards"
if ($env:TRACKSIDE_DEV) {
    Fail "TRACKSIDE_DEV is set in this shell. A release built with it has self-update DISABLED. Open a clean shell."
}
# Private update-channel vars must NOT leak into a public build: TRACKSIDE_UPDATE_TOKEN would bake
# the private repo's PAT into a DLL published to the world, and TRACKSIDE_CHANNEL would point every
# public user at the private repo. Both are silent at compile time (option_env!), so guard here.
foreach ($v in 'TRACKSIDE_CHANNEL', 'TRACKSIDE_UPDATE_TOKEN', 'TRACKSIDE_UPDATE_SENTINEL') {
    if (Get-Item "Env:\$v" -ErrorAction SilentlyContinue) {
        Fail "$v is set in this shell — that is a PRIVATE-channel build var and must never be baked into a public release. Open a clean shell."
    }
}
$dirty = (& git -C $RepoDir status --porcelain --untracked-files=no)
if ($dirty -and -not $Force) {
    Write-Host $dirty
    Fail "Working tree is dirty. Commit first (or pass -Force)."
}
if (& git -C $RepoDir tag --list $Tag) {
    Write-Host "  NOTE: tag $Tag already exists locally — it will be reused." -ForegroundColor Yellow
}

# Re-hash guard. Builds are not byte-reproducible, so rebuilding an ALREADY-PUBLISHED tag yields a
# different DLL — and a different <dll>.hash. The updater's same-tag hotfix check compares exactly
# that, so re-uploading would prompt every existing user with a "hotfix" for a build containing no
# actual changes. Bump the version instead; -SkipBuild re-uploads the staged artifacts untouched.
if (-not $SkipBuild -and -not $Force -and (Get-Command gh -ErrorAction SilentlyContinue)) {
    $pub = (& gh release view $Tag --json isDraft 2>$null)
    if ($pub -and ($pub | ConvertFrom-Json).isDraft -eq $false) {
        Fail @"
Release $Tag is already PUBLISHED, and rebuilding would change the DLL hash.
Every user on $Tag would be prompted with a spurious "hotfix" for an identical build.
  * shipping changes?  bump the version in native\Cargo.toml
  * re-uploading only? re-run with -SkipBuild (keeps the staged artifacts + hashes)
  * really meant it?   re-run with -Force
"@
    }
}
Write-Host "  clean tree, no dev/private build vars, tag not already published." -ForegroundColor Green

New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

# --- build -------------------------------------------------------------------
if (-not $SkipBuild) {
    Step "Build (public — no TRACKSIDE_DEV)"
    Remove-Item Env:\TRACKSIDE_DEV -ErrorAction SilentlyContinue

    Push-Location $NativeDir
    try {
        Write-Host "  cargo build --release            (default features)"
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { Fail "cargo build (default) failed." }
        Copy-Item (Join-Path $NativeDir 'target\release\trackside.dll') (Join-Path $StageDir 'trackside.dll') -Force

        Write-Host "  cargo build --release --features hachimi"
        & cargo build --release --features hachimi
        if ($LASTEXITCODE -ne 0) { Fail "cargo build (hachimi) failed." }
        Copy-Item (Join-Path $NativeDir 'target\release\trackside.dll') (Join-Path $StageDir 'trackside_hh.dll') -Force
    } finally { Pop-Location }

    Push-Location $ProxyDir
    try {
        Write-Host "  cargo build --release            (proxy -> version.dll)"
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { Fail "cargo build (proxy) failed." }
    } finally { Pop-Location }

    $proxyDll = @(
        (Join-Path $ProxyDir 'target\release\version.dll'),
        (Join-Path $RepoDir  'target\release\version.dll')
    ) | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if (-not $proxyDll) { Fail "proxy build succeeded but version.dll wasn't found." }
    Copy-Item $proxyDll (Join-Path $StageDir 'version.dll') -Force

    # The proxy forwards the real exports to the genuine system DLL, shipped alongside as
    # trackside_version.dll (see deploy-on-exit.ps1).
    Copy-Item (Join-Path $env:WINDIR 'System32\version.dll') (Join-Path $StageDir 'trackside_version.dll') -Force
} else {
    Step "Build skipped (-SkipBuild) — reusing staged DLLs"
}

foreach ($f in 'trackside.dll','trackside_hh.dll','version.dll','trackside_version.dll') {
    if (-not (Test-Path -LiteralPath (Join-Path $StageDir $f))) { Fail "missing artifact: $f" }
}

# --- CRITICAL: never publish the private build -------------------------------
Step "Public-build verification"
foreach ($f in 'trackside.dll','trackside_hh.dll') {
    $p = Join-Path $StageDir $f
    if (Test-DllHas $p $ORACLE_SENTINEL) {
        Fail "$f contains the Event Oracle sentinel — that's the PRIVATE build. Refusing to package a public release. (Are you on '$branch' instead of a public branch?)"
    }
}
Write-Host "  both DLLs are Oracle-free (safe to publish)." -ForegroundColor Green

# --- hashes ------------------------------------------------------------------
Step "Hashes (FNV-1a/64 — the updater's hotfix check)"
foreach ($f in 'trackside.dll','trackside_hh.dll') {
    $p = Join-Path $StageDir $f
    $h = Get-Fnv1a $p
    Set-Content -LiteralPath "$p.hash" -Value $h -NoNewline -Encoding ASCII
    Write-Host ("  {0,-20} {1}" -f $f, $h)
}

# --- package -----------------------------------------------------------------
Step "Package"
Add-Type -AssemblyName System.IO.Compression.FileSystem
function New-Zip([string]$zipPath, [hashtable]$entries) {
    if (Test-Path -LiteralPath $zipPath) { Remove-Item -LiteralPath $zipPath -Force }
    $z = [System.IO.Compression.ZipFile]::Open($zipPath, 'Create')
    try {
        foreach ($nameInZip in $entries.Keys) {
            $null = [System.IO.Compression.ZipFileExtensions]::CreateEntryFromFile($z, $entries[$nameInZip], $nameInZip)
        }
    } finally { $z.Dispose() }
    Write-Host ("  {0,-24} {1}" -f (Split-Path $zipPath -Leaf), (("{0:N2} MB" -f ((Get-Item $zipPath).Length / 1MB))))
}
# NOTE: inside the Hachimi zip the hachimi build is named trackside.dll — that variant
# replaces the same file in the game folder.
New-Zip (Join-Path $StageDir 'Trackside.zip') ([ordered]@{
    'trackside.dll'         = (Join-Path $StageDir 'trackside.dll')
    'version.dll'           = (Join-Path $StageDir 'version.dll')
    'trackside_version.dll' = (Join-Path $StageDir 'trackside_version.dll')
})
New-Zip (Join-Path $StageDir 'Trackside+Hachimi.zip') ([ordered]@{
    'trackside.dll' = (Join-Path $StageDir 'trackside_hh.dll')
    'version.dll'   = (Join-Path $StageDir 'version.dll')
})

# --- notes -------------------------------------------------------------------
$notesPath = if ($Notes) { $Notes } else { Join-Path $StageDir 'NOTES.md' }
if (-not (Test-Path -LiteralPath $notesPath)) {
    Fail "No release notes at $notesPath. The in-game updater shows these as the changelog — write them first (or pass -Notes <file>)."
}
if ($notesPath -ne (Join-Path $StageDir 'NOTES.md')) {
    Copy-Item -LiteralPath $notesPath -Destination (Join-Path $StageDir 'NOTES.md') -Force
}

Write-Host ""
Write-Host "  Staged in $StageDir" -ForegroundColor Green
Get-ChildItem $StageDir | ForEach-Object { Write-Host ("    {0,-24} {1,10:N0} bytes" -f $_.Name, $_.Length) }

if ($StageOnly) {
    Write-Host ""
    Write-Host "  -StageOnly: nothing tagged or uploaded." -ForegroundColor Cyan
    Write-Host ""
    exit 0
}

# --- tag ---------------------------------------------------------------------
Step "Tag"
if (-not (& git -C $RepoDir tag --list $Tag)) {
    & git -C $RepoDir tag -a $Tag -m "Trackside $Tag"
    if ($LASTEXITCODE -ne 0) { Fail "git tag failed." }
    Write-Host "  created local tag $Tag" -ForegroundColor Green
} else {
    Write-Host "  local tag $Tag already exists" -ForegroundColor DarkGray
}

# --- GitHub release ----------------------------------------------------------
Step "GitHub release"
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) {
    Fail "gh CLI not found — install it, or re-run with -StageOnly and upload manually."
}
$assets = @(
    (Join-Path $StageDir 'trackside.dll'),
    (Join-Path $StageDir 'trackside.dll.hash'),
    (Join-Path $StageDir 'trackside_hh.dll'),
    (Join-Path $StageDir 'trackside_hh.dll.hash'),
    (Join-Path $StageDir 'Trackside.zip'),
    (Join-Path $StageDir 'Trackside+Hachimi.zip')
)

if ($Publish) {
    # Push the BRANCH as well as the tag. Pushing only the tag publishes the released code (the tag
    # makes those commits reachable) while leaving origin/<branch> pointing at the previous release,
    # so GitHub's default view shows stale source. v1.0.6 shipped with origin/main 21 commits behind
    # for exactly this reason.
    Write-Host "  pushing $branch..." -ForegroundColor Yellow
    & git -C $RepoDir push origin $branch
    if ($LASTEXITCODE -ne 0) { Fail "pushing the branch failed (rebase/pull, then re-run with -SkipBuild)." }

    Write-Host "  pushing tag $Tag..." -ForegroundColor Yellow
    & git -C $RepoDir push origin $Tag
    if ($LASTEXITCODE -ne 0) { Fail "pushing the tag failed." }
}

$existing = (& gh release view $Tag --json tagName 2>$null)
if ($existing) {
    Write-Host "  release $Tag already exists — uploading assets with --clobber." -ForegroundColor Yellow
    & gh release upload $Tag @assets --clobber
    if ($LASTEXITCODE -ne 0) { Fail "asset upload failed." }
} else {
    $ghArgs = @('release','create',$Tag,'--title',"Trackside $Tag",'--notes-file',(Join-Path $StageDir 'NOTES.md'))
    if (-not $Publish) { $ghArgs += '--draft' }
    $ghArgs += $assets
    & gh @ghArgs
    if ($LASTEXITCODE -ne 0) { Fail "gh release create failed." }
}

Write-Host ""
if ($Publish) {
    Write-Host "  PUBLISHED $Tag — users will be offered the update." -ForegroundColor Green
} else {
    Write-Host "  DRAFT $Tag created with all assets attached." -ForegroundColor Green
    Write-Host "  Review it on GitHub and hit Publish, or re-run with -Publish." -ForegroundColor DarkGray
}
Write-Host ""
