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
                                             + trackside_version.dll (same 3 files; only the
                                             overlay build differs). Hachimi installs at its OWN
                                             hijack point (cri_mana_vpx/UnityPlayer/winhttp), so it
                                             does NOT go in the trackside_version.dll slot.
      5. STAGE    - everything into release-v<version>\ alongside NOTES.md
      6. RELEASE  - create the git tag v<version>, then a GitHub release via gh with all
                    assets attached.

    Publishing is opt-in. Without -Publish you get a DRAFT release you can review and
    publish from the GitHub UI; the tag is created locally but not pushed.

    PARTIAL FAILURE RULE: if any step fails, fix the cause and RE-RUN THIS SCRIPT - it is
    idempotent (an existing release re-uploads with --clobber). NEVER hand-finish a release with
    ad-hoc gh calls: v1.0.8 shipped missing trackside.dll and Trackside+Hachimi.zip exactly that
    way, breaking the standard-install updater until it was caught.

    SYMBOLS: each build's trackside.pdb is archived into the stage folder (staged only, never
    uploaded). Keep the stage folder - it is the only way to resolve a user's [watchdog] stack
    frames back to function names after target/release has been rebuilt.

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

# Run gh and judge success by its EXIT CODE, not by whether it touched stderr. gh writes ordinary
# progress and "release not found" results to stderr, and with $ErrorActionPreference='Stop' (set
# above) Windows PowerShell promotes ANY native-command stderr into a TERMINATING error - even with
# 2>$null - which aborts the script on a perfectly successful upload. Ported from
# Release-TracksidePrivate.ps1, where this exact bug was already fixed.
$script:GhExit = 0
function Invoke-Gh {
    param([Parameter(ValueFromRemainingArguments = $true)]$GhArgs)
    # Guard the comma-operator footgun: an unquoted `--json a,b` arrives as a nested array and would
    # splat to gh as multiple arguments, failing confusingly. Catch it loudly. Quote it: --json 'a,b'.
    foreach ($a in $GhArgs) {
        if ($a -is [System.Array]) {
            Fail "Invoke-Gh got an array argument ([$($a -join ',')]) - an unquoted comma-list. Quote it: --json 'field1,field2'."
        }
    }
    $prev = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $out = & gh @GhArgs 2>&1
        $script:GhExit = $LASTEXITCODE
        return (($out | Where-Object { $_ -isnot [System.Management.Automation.ErrorRecord] }) -join "`n")
    } finally { $ErrorActionPreference = $prev }
}
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

# Skill-data staleness guard. data\*.json are baked into the DLL via include_str!, so skills added by
# a game update are invisible to the optimizer until they're regenerated. Nothing forced that to
# happen, so the bundle silently drifted 33 skills behind before v1.0.7. Verify against the live
# master.mdb; no-ops with a warning on a machine without the game installed.
if (-not $SkipBuild) {
    $refresh = Join-Path $RepoDir 'refresh_skill_data.py'
    if (Test-Path -LiteralPath $refresh) {
        & python $refresh --check
        if ($LASTEXITCODE -ne 0) {
            if ($Force) { Write-Host "  (skill data stale — continuing because -Force)" -ForegroundColor Yellow }
            else { Fail "Bundled skill data is behind master.mdb (see above). Run: python refresh_skill_data.py — then commit and re-run." }
        }
    }
}
# Overlay menu map. Regenerate so docs-internal/menu-map.md matches the menu being shipped, and FAIL
# if the generator cannot attribute every section to a tab. A stale or partial map is how a panel
# ends up in the wrong section unnoticed - Streamer mode shipped under "Companion plugins" for
# exactly that reason. ASCII-only messages here on purpose: this file is read by Windows PowerShell
# 5.1, which mis-decodes non-ASCII without a BOM and turns a stray smart quote into a parse error.
if (-not $SkipBuild) {
    $menumap = Join-Path $RepoDir 'native/tools/gen_menu_map.py'
    if (Test-Path -LiteralPath $menumap) {
        & python $menumap --write
        if ($LASTEXITCODE -ne 0) {
            if ($Force) { Write-Host "  (menu map incomplete - continuing because -Force)" -ForegroundColor Yellow }
            else { Fail "Menu map generator could not attribute every section (see above). Fix native/tools/gen_menu_map.py, then re-run." }
        }
    }
}
if (& git -C $RepoDir tag --list $Tag) {
    Write-Host "  NOTE: tag $Tag already exists locally — it will be reused." -ForegroundColor Yellow
}

# Re-hash guard. Builds are not byte-reproducible, so rebuilding an ALREADY-PUBLISHED tag yields a
# different DLL — and a different <dll>.hash. The updater's same-tag hotfix check compares exactly
# that, so re-uploading would prompt every existing user with a "hotfix" for a build containing no
# actual changes. Bump the version instead; -SkipBuild re-uploads the staged artifacts untouched.
if (-not $SkipBuild -and -not $Force -and (Get-Command gh -ErrorAction SilentlyContinue)) {
    $pub = Invoke-Gh release view $Tag --json isDraft
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
Write-Host "  clean tree, no dev/private build vars." -ForegroundColor Green

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
        # Archive THIS build's symbols now - the hachimi build below reuses the same output
        # path and would overwrite trackside.pdb. Without per-release symbols a user's watchdog
        # stack is only module+offset and cannot be resolved once target/release is rebuilt
        # (exactly what blocked the v1.0.8 field report).
        Copy-Item (Join-Path $NativeDir 'target\release\trackside.pdb') (Join-Path $StageDir 'trackside.pdb') -Force

        Write-Host "  cargo build --release --features hachimi"
        & cargo build --release --features hachimi
        if ($LASTEXITCODE -ne 0) { Fail "cargo build (hachimi) failed." }
        Copy-Item (Join-Path $NativeDir 'target\release\trackside.dll') (Join-Path $StageDir 'trackside_hh.dll') -Force
        Copy-Item (Join-Path $NativeDir 'target\release\trackside.pdb') (Join-Path $StageDir 'trackside_hh.pdb') -Force
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
    'trackside.dll'         = (Join-Path $StageDir 'trackside_hh.dll')
    'version.dll'           = (Join-Path $StageDir 'version.dll')
    'trackside_version.dll' = (Join-Path $StageDir 'trackside_version.dll')
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

$existing = Invoke-Gh release view $Tag --json tagName
if ($existing) {
    Write-Host "  release $Tag already exists — uploading assets with --clobber." -ForegroundColor Yellow
    $null = Invoke-Gh release upload $Tag @assets --clobber
    if ($GhExit -ne 0) { Fail "asset upload failed." }
    # A release that already exists is still a DRAFT unless we flip it. Without this, re-running with
    # -Publish uploads the assets and then reports success while silently leaving it unpublished.
    if ($Publish) {
        $null = Invoke-Gh release edit $Tag --draft=false
        if ($GhExit -ne 0) { Fail "publishing the existing draft failed." }
        Write-Host "  flipped the existing draft to published." -ForegroundColor Green
    }
} else {
    $ghArgs = @('release','create',$Tag,'--title',"Trackside $Tag",'--notes-file',(Join-Path $StageDir 'NOTES.md'))
    if (-not $Publish) { $ghArgs += '--draft' }
    $ghArgs += $assets
    $null = Invoke-Gh @ghArgs
    if ($GhExit -ne 0) { Fail "gh release create failed." }
}

# --- asset completeness vs the previous release ------------------------------
# Catches a partial upload no matter how it happened. Compares this release against the previous
# published one; anything the last release shipped that this one lacks fails the run. When an
# asset is REMOVED deliberately (e.g. dropping the Hachimi variant), edit this check in the same
# commit that removes the asset from staging.
$relJson = Invoke-Gh release list --limit 10 --json tagName,isDraft
if ($GhExit -eq 0 -and $relJson) {
    $prevTag = ($relJson | ConvertFrom-Json) | Where-Object { -not $_.isDraft -and $_.tagName -ne $Tag } |
        Select-Object -First 1 -ExpandProperty tagName
    if ($prevTag) {
        $prevAssets = @(((Invoke-Gh release view $prevTag --json assets | ConvertFrom-Json).assets | ForEach-Object { $_.name }))
        $curAssets  = @(((Invoke-Gh release view $Tag    --json assets | ConvertFrom-Json).assets | ForEach-Object { $_.name }))
        $missing = @($prevAssets | Where-Object { $curAssets -notcontains $_ })
        if ($missing.Count) {
            Fail "release $Tag is MISSING assets that $prevTag shipped: $($missing -join ', ')"
        }
        Write-Host "  asset set verified against $prevTag ($($curAssets.Count) assets)" -ForegroundColor Green
    }
}

Write-Host ""
if ($Publish) {
    Write-Host "  PUBLISHED $Tag — users will be offered the update." -ForegroundColor Green
} else {
    Write-Host "  DRAFT $Tag created with all assets attached." -ForegroundColor Green
    Write-Host "  Review it on GitHub and hit Publish, or re-run with -Publish." -ForegroundColor DarkGray
}
Write-Host ""
