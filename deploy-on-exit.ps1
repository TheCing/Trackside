$ErrorActionPreference = 'Stop'
$repo = 'C:\Users\jptyn\Dev\Heaven-Internal-Public-Version-'
$game = 'G:\SteamLibrary\steamapps\common\UmamusumePrettyDerby'

Write-Host 'Waiting for the game to close...'
while (Get-Process -Name UmamusumePrettyDerby -ErrorAction SilentlyContinue) { Start-Sleep -Seconds 3 }
Start-Sleep -Seconds 2

# Overlay
Copy-Item -LiteralPath "$repo\native\target\release\trackside.dll" -Destination "$game\trackside.dll" -Force
$size = [math]::Round((Get-Item -LiteralPath "$game\trackside.dll").Length / 1MB, 2)
Write-Host "DEPLOYED trackside.dll ($size MB) at $(Get-Date)"

# Loader proxy (replaces the old Heaven master proxy; forwards to heaven_version.dll
# if present so the Hachimi chain keeps working)
Copy-Item -LiteralPath "$repo\proxy\target\release\version.dll" -Destination "$game\version.dll" -Force
Write-Host 'DEPLOYED version.dll (Trackside loader proxy)'

# One-time fork switchover: the old overlay must GO, or the old proxy would have
# loaded both overlays side by side.
if (Test-Path -LiteralPath "$game\heaven_overlay.dll") {
    Remove-Item -LiteralPath "$game\heaven_overlay.dll" -Force
    Write-Host 'REMOVED heaven_overlay.dll (superseded by trackside.dll)'
}
