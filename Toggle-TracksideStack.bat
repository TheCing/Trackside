@echo off
REM Double-click wrapper for Toggle-TracksideStack.ps1
REM Flips the Trackside + Hachimi + Hakuraku mod stack on/off.
REM Keep this .bat in the SAME folder as Toggle-TracksideStack.ps1, and drop both
REM next to UmamusumePrettyDerby.exe.

REM Folder this .bat lives in, with the trailing backslash stripped (so it does
REM not escape the closing quote when passed to PowerShell).
set "HERE=%~dp0"
set "HERE=%HERE:~0,-1%"

cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%HERE%\Toggle-TracksideStack.ps1" -GameDir "%HERE%" %*

echo.
pause
