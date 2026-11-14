@echo off
setlocal EnableExtensions
rem ======================================================================
rem  BM Osprey project status
rem
rem  Shows where the work stands: recent commits, files changed but not
rem  committed, what is new since Claude's last handoff, the progress
rem  notes and the latest check results. Then it can pack a small zip
rem  with those changes to upload to Claude, so it can pick up where the
rem  work stopped.
rem
rem  Put this file in the bm-osprey folder and double click it.
rem ======================================================================

cd /d "%~dp0"
set "ROOT=%CD%"
rem The last commit Claude handed over. Everything after it is new.
set "BASE=74bd1fa"

set "STAMP="
for /f "usebackq delims=" %%T in (`powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss" 2^>nul`) do set "STAMP=%%T"
if not defined STAMP set "STAMP=run%RANDOM%"
if not exist "%ROOT%\logs" mkdir "%ROOT%\logs"
set "OUT=%ROOT%\logs\status-%STAMP%"
mkdir "%OUT%"
set "REP=%OUT%\status.txt"
set "OSPREY_OUT=%OUT%"

if not exist "%ROOT%\Cargo.toml" goto wrong_folder

>"%REP%" echo BM Osprey status, %STAMP%
>>"%REP%" echo Folder: %ROOT%
>>"%REP%" echo.

where git >nul 2>&1
if errorlevel 1 goto no_git
if not exist "%ROOT%\.git" goto no_repo

>>"%REP%" echo === Last 15 commits ===
git log --oneline -15 >>"%REP%" 2>&1
>>"%REP%" echo.

>>"%REP%" echo === Changed but not committed ===
git status --short >>"%REP%" 2>&1
git diff --stat HEAD >>"%REP%" 2>&1
>>"%REP%" echo.

git rev-parse --verify --quiet %BASE%^^{commit} >nul 2>&1
if errorlevel 1 goto base_missing
>>"%REP%" echo === Commits since Claude's handoff (%BASE%) ===
git log --oneline %BASE%..HEAD >>"%REP%" 2>&1
git diff --stat %BASE% HEAD >>"%REP%" 2>&1
>>"%REP%" echo.
mkdir "%OUT%\patches"
git format-patch --quiet -o "%OUT%\patches" %BASE%..HEAD >nul 2>&1
goto after_base

:base_missing
>>"%REP%" echo Commit %BASE% is not in this copy, so this folder is older than
>>"%REP%" echo Claude's last handoff or was set up separately.
>>"%REP%" echo.

:after_base
git diff HEAD >"%OUT%\uncommitted.patch" 2>nul
git ls-files --others --exclude-standard >"%OUT%\new-files.txt" 2>nul
goto after_git

:no_git
>>"%REP%" echo git was not found, so commits and changes cannot be listed.
>>"%REP%" echo Install Git for Windows from git-scm.com for a full report.
>>"%REP%" echo.
goto after_git

:no_repo
>>"%REP%" echo This folder has no git history, so changes cannot be listed.
>>"%REP%" echo.

:after_git
>>"%REP%" echo === Progress notes (PROGRESS.md) ===
if exist "%ROOT%\PROGRESS.md" type "%ROOT%\PROGRESS.md" >>"%REP%"
if not exist "%ROOT%\PROGRESS.md" >>"%REP%" echo No PROGRESS.md yet.
>>"%REP%" echo.

>>"%REP%" echo === Latest results ===
if exist "%ROOT%\logs\latest.log" copy /y "%ROOT%\logs\latest.log" "%OUT%\latest.log" >nul
if exist "%ROOT%\logs\latest.log" >>"%REP%" echo logs\latest.log copied
if exist "%ROOT%\logs\probe-latest.json" copy /y "%ROOT%\logs\probe-latest.json" "%OUT%\probe-latest.json" >nul
if exist "%ROOT%\logs\probe-latest.json" >>"%REP%" echo logs\probe-latest.json copied
if exist "%APPDATA%\bm-osprey\browser-crash.log" copy /y "%APPDATA%\bm-osprey\browser-crash.log" "%OUT%\browser-crash.log" >nul
if exist "%APPDATA%\bm-osprey\browser-crash.log" >>"%REP%" echo browser-crash.log copied
if exist "%ROOT%\target\release\bm-osprey.exe" >>"%REP%" echo Release browser is built.
if not exist "%ROOT%\target\release\bm-osprey.exe" >>"%REP%" echo Release browser is not built.

cls
type "%REP%"
echo.
echo ----------------------------------------------------------------------
choice /c YN /m "Make a zip of this for Claude"
if errorlevel 2 goto done

echo.
echo   The last Claude Code session log shows exactly where the work
echo   stopped. It holds that session's chat and the commands it ran.
choice /c YN /m "Include it"
if errorlevel 2 goto copy_source
powershell -NoProfile -Command "$d = Join-Path $env:USERPROFILE '.claude\projects'; $f = $null; if (Test-Path $d) { $f = Get-ChildItem $d -Directory | Where-Object { $_.Name -like '*bm-osprey*' -or $_.Name -like '*bm-fetch*' } | ForEach-Object { Get-ChildItem $_.FullName -Filter *.jsonl -File } | Sort-Object LastWriteTime -Descending | Select-Object -First 1 }; if ($f) { Copy-Item $f.FullName (Join-Path $env:OSPREY_OUT 'claude-session.jsonl'); Write-Host ('  Added the session from ' + $f.LastWriteTime) } else { Write-Host '  No Claude Code session found for this folder.' }"

:copy_source
echo   Copying the source code (no build files)...
robocopy "%ROOT%" "%OUT%\source" /E /XD target logs gen node_modules .git /XF *.zip *.exe /NFL /NDL /NJH /NJS /NP >nul
set "ZIP=%ROOT%\bm-osprey-status-%STAMP%.zip"
set "OSPREY_ZIP=%ZIP%"
tar -a -c -f "%ZIP%" -C "%OUT%" . >nul 2>&1
if exist "%ZIP%" goto zipped
powershell -NoProfile -Command "Compress-Archive -Path (Join-Path $env:OSPREY_OUT '*') -DestinationPath $env:OSPREY_ZIP" >nul 2>&1

:zipped
if not exist "%ZIP%" goto zip_failed
echo.
echo   Zip ready: %ZIP%
echo   Upload it to Claude.
explorer /select,"%ZIP%"
goto done

:zip_failed
echo   The zip could not be made. The files are in %OUT%
goto done

:wrong_folder
echo   Put STATUS.bat in the bm-osprey folder, next to Cargo.toml, and run it again.

:done
echo.
pause
endlocal
