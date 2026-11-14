@echo off
setlocal EnableExtensions
rem ======================================================================
rem  BM Osprey developer tool for Windows
rem
rem  Double click it for a menu, or use it from a terminal:
rem    dev check        check that Rust, C++ build tools, git and ffmpeg are ready
rem    dev build        debug build
rem    dev release      optimised build
rem    dev fast         release style build that uses less disk, in target\fast
rem    dev test         run all tests
rem    dev full         everything: setup check, format, lint, tests, release build
rem    dev run ARGS     build if needed, then run osprey with ARGS
rem    dev browser      build if needed, then open the BM Osprey browser
rem    dev page URL [S] open a page for S seconds (default 30), play it and
rem                     write what was found to logs\probe-latest.json
rem                     example: dev run get "https://site.example/video.m3u8"
rem    dev report       run every check and pack the results into a zip
rem                     to upload to Claude (RUN-CHECKS.bat does this)
rem    dev logs         open the logs folder
rem    dev clean        delete build files
rem
rem  Every action writes a log to logs\ and a copy to logs\latest.log.
rem  When something fails, send logs\latest.log to Claude.
rem ======================================================================

cd /d "%~dp0"
set "ROOT=%CD%"
set "LOGDIR=%ROOT%\logs"
if not exist "%LOGDIR%" mkdir "%LOGDIR%"

set "STAMP="
for /f "usebackq delims=" %%T in (`powershell -NoProfile -Command "Get-Date -Format yyyyMMdd-HHmmss" 2^>nul`) do set "STAMP=%%T"
if not defined STAMP set "STAMP=run%RANDOM%"

rem Plain text logs without colour codes, and full detail when something crashes.
set "CARGO_TERM_COLOR=never"
set "RUST_BACKTRACE=1"

rem Each compiler job can need 1 to 2 GB of memory on the big libraries
rem (Tauri, the Windows API). Running one per CPU core can run out of
rem memory, which crashes the compiler and leaves damaged build files.
rem So the number of jobs follows the free memory: about one per 2 GB.
set "FREEMB="
set "TOTALMB="
for /f "usebackq tokens=1,2" %%A in (`powershell -NoProfile -Command "$o=Get-CimInstance Win32_OperatingSystem; '{0} {1}' -f [int]($o.FreePhysicalMemory/1024),[int]($o.TotalVisibleMemorySize/1024)" 2^>nul`) do (
  set "FREEMB=%%A"
  set "TOTALMB=%%B"
)
if defined CARGO_BUILD_JOBS goto jobs_set
set "JOBS=2"
if defined FREEMB set /a JOBS=FREEMB/2000
if %JOBS% LSS 1 set "JOBS=1"
if defined NUMBER_OF_PROCESSORS if %JOBS% GTR %NUMBER_OF_PROCESSORS% set "JOBS=%NUMBER_OF_PROCESSORS%"
set "CARGO_BUILD_JOBS=%JOBS%"
:jobs_set
set "RETRIED="

rem The browser's build step uses the Windows SDK's resource compiler
rem (rc.exe) to put the icon and version into the .exe. Tauri looks for it
rem through the registry, which is sometimes missing, so it is found here
rem and passed on with RC. The newest SDK version wins.
set "RCEXE="
set "RC_BEFORE=%RC%"
call :find_rc "%ProgramFiles(x86)%"
if not defined RCEXE call :find_rc "%ProgramFiles%"
if not defined RCEXE goto rc_done
rem Set every name the build looks for, replacing any older RC setting,
rem and put the SDK folder on PATH so rc.exe is also found by name.
for %%D in ("%RCEXE%") do set "RCDIR=%%~dpD"
set "RC=%RCEXE%"
set "RC_x86_64-pc-windows-msvc=%RCEXE%"
set "RC_x86_64_pc_windows_msvc=%RCEXE%"
set "PATH=%RCDIR%;%PATH%"
:rc_done
set "EXE=%ROOT%\target\release\osprey.exe"
set "BROWSER=%ROOT%\target\release\bm-osprey.exe"
set "FAILS=0"
set "WARNS=0"
set "FAILED_STEPS="
set "WARNED_STEPS="
set "SUMMARY="
set "STEP_OPTIONAL="
set "REPORT_ZIP="
rem PAUSE_AT_END may come from RUN-CHECKS.bat, so it is not cleared here.

rem Remove logs older than 30 days.
forfiles /p "%LOGDIR%" /m *.log /d -30 /c "cmd /c del @path" >nul 2>&1

set "ACTION=%~1"
if "%ACTION%"=="" goto menu
set "PAGEURL="
set "PAGEWAIT="
if /i not "%ACTION%"=="page" goto not_page
set "PAGEURL=%~2"
set "PAGEWAIT=%~3"
:not_page

if /i not "%ACTION%"=="run" goto dispatch
set "RUNARGS="
:collect_args
shift
if "%~1"=="" goto dispatch
set RUNARGS=%RUNARGS% %1
goto collect_args

rem ----------------------------------------------------------------------
:menu
set "PAUSE_AT_END=1"
echo.
echo   BM Osprey dev tool
echo   -----------------
echo   1  Check my setup     Rust, C++ build tools, git, ffmpeg
echo   2  Build              debug build
echo   3  Build release      optimised build
echo   4  Run tests
echo   5  Full check         setup, format, lint, tests, release build
echo   6  Run osprey            you type the arguments
echo   B  Open the browser   builds it first if needed
echo   P  Test a page        opens it, plays it, reports what was found
echo   7  Open logs folder
echo   8  Clean build files
echo   9  Check everything and make a results zip for Claude
echo   0  Exit
echo.
set "CHOICE="
set /p "CHOICE=  Pick a number: "
set "ACTION="
if "%CHOICE%"=="1" set "ACTION=check"
if "%CHOICE%"=="2" set "ACTION=build"
if "%CHOICE%"=="3" set "ACTION=release"
if "%CHOICE%"=="4" set "ACTION=test"
if "%CHOICE%"=="5" set "ACTION=full"
if "%CHOICE%"=="6" set "ACTION=run"
if "%CHOICE%"=="7" set "ACTION=logs"
if "%CHOICE%"=="8" set "ACTION=clean"
if "%CHOICE%"=="9" set "ACTION=report"
if /i "%CHOICE%"=="B" set "ACTION=browser"
if /i "%CHOICE%"=="P" set "ACTION=page"
if "%CHOICE%"=="0" exit /b 0
if not defined ACTION goto menu
if /i not "%ACTION%"=="run" goto dispatch
echo.
echo   Type the osprey arguments. Put URLs in "double quotes".
echo   Examples:  probe "https://site.example/master.m3u8"
echo              get "https://site.example/watch/1" -q 720
echo              text "https://news.example/article"
echo.
set "RUNARGS="
set /p "RUNARGS=  osprey "

rem ----------------------------------------------------------------------
:dispatch
set "LOG=%LOGDIR%\%ACTION%-%STAMP%.log"
if /i "%ACTION%"=="check" goto do_check
if /i "%ACTION%"=="build" goto do_build
if /i "%ACTION%"=="release" goto do_release
if /i "%ACTION%"=="fast" goto do_fast
if /i "%ACTION%"=="test" goto do_test
if /i "%ACTION%"=="full" goto do_full
if /i "%ACTION%"=="run" goto do_run
if /i "%ACTION%"=="logs" goto do_logs
if /i "%ACTION%"=="clean" goto do_clean
if /i "%ACTION%"=="report" goto do_report
if /i "%ACTION%"=="browser" goto do_browser
if /i "%ACTION%"=="page" goto do_page
if /i "%ACTION%"=="help" goto usage
echo Unknown action "%ACTION%".
goto usage

:usage
echo.
echo Usage: dev [check ^| build ^| release ^| fast ^| test ^| full ^| report ^| run ARGS ^| browser ^| page URL [SECONDS] ^| logs ^| clean]
echo Run "dev" with no arguments for a menu.
exit /b 1

rem ----------------------------------------------------------------------
:do_check
call :log_header "Setup check"
call :doctor
goto finish

:do_build
call :log_header "Debug build"
call :need_cargo || goto finish
call :step "Build debug" cargo build
goto finish

:do_fast
call :log_header "Fast build"
call :need_cargo || goto finish
call :step "Build fast profile" cargo build --profile fast
goto finish

:do_release
call :log_header "Release build"
call :need_cargo || goto finish
call :step "Build release" cargo build --release
goto finish

:do_test
call :log_header "Tests"
call :need_cargo || goto finish
call :step "Tests" cargo test --all
goto finish

:do_full
call :log_header "Full check"
call :doctor
call :need_cargo || goto finish
call :step "Format check" cargo fmt --all -- --check
call :step "Lint (clippy)" cargo clippy --all-targets -- -D warnings
call :step "Tests" cargo test --all
call :step "Build release" cargo build --release
if not exist "%EXE%" goto finish
call :step "Smoke test: version" "%EXE%" --version
call :step "Smoke test: filters" "%EXE%" filters check "https://pubads.g.doubleclick.net/ad.mp4"
goto finish

:do_run
call :log_header "Run osprey"
call :need_cargo || goto finish
if exist "%EXE%" goto run_build_quiet
echo   First run, building osprey. This takes a few minutes the first time.
:run_build_quiet
call :step "Build release" cargo build --release
if not exist "%EXE%" goto finish
if not defined RUNARGS set "RUNARGS=--help"
>>"%LOG%" echo.
>>"%LOG%" echo ==== osprey%RUNARGS%
echo.
echo   osprey%RUNARGS%
echo.
rem osprey writes its own diagnostic lines into the same log through OSPREY_LOG,
rem while progress bars still show here in the window.
set "OSPREY_LOG=%LOG%"
"%EXE%" %RUNARGS%
set "RC=%ERRORLEVEL%"
set "OSPREY_LOG="
>>"%LOG%" echo ==== osprey exit code %RC%
if "%RC%"=="0" goto finish
set /a FAILS+=1
set "FAILED_STEPS=%FAILED_STEPS% [osprey run]"
goto finish

:do_browser
call :log_header "Open browser"
call :need_cargo || goto finish
if exist "%BROWSER%" goto browser_build_quiet
echo   First run, building the browser. This takes a few minutes the first time.
:browser_build_quiet
call :step "Build release" cargo build --release
if not exist "%BROWSER%" goto finish
call :step "Browser self-test" "%BROWSER%" --self-test
if not "%FAILS%"=="0" goto finish
start "" "%BROWSER%"
echo   Browser started. Crash details, if any, go to
echo   %APPDATA%\bm-osprey\browser-crash.log
goto finish

:do_page
call :log_header "Test a page"
call :need_cargo || goto finish
if not defined PAGEURL set /p "PAGEURL=  Page address: "
if not defined PAGEURL goto finish
if not defined PAGEWAIT set "PAGEWAIT=30"
call :step "Build release" cargo build --release
if not exist "%BROWSER%" goto finish
set "PROBEOUT=%LOGDIR%\probe-%STAMP%.json"
echo   Opening the page for %PAGEWAIT% seconds. The window closes by itself.
start "" /wait "%BROWSER%" --probe-page "%PAGEURL%" --probe-wait %PAGEWAIT% --probe-out "%PROBEOUT%"
if exist "%PROBEOUT%" goto page_report
echo   No report was written. See %APPDATA%\bm-osprey\browser-crash.log
set /a FAILS+=1
goto finish
:page_report
copy /y "%PROBEOUT%" "%LOGDIR%\probe-latest.json" >nul
set "OSPREY_PROBE=%PROBEOUT%"
powershell -NoProfile -Command "$r = Get-Content -Raw $env:OSPREY_PROBE | ConvertFrom-Json; $s = $r.summary; Write-Host ('  ' + $r.title); Write-Host ('  Reported ' + $s.reported + ', listed ' + $s.listed + ', dropped ' + $s.dropped + ', ads ' + $s.ad_filtered); foreach ($m in $r.listed) { $q = @($m.info.qualities).Count; Write-Host ('   ' + $m.kind + '  ' + $m.state + '  ' + $m.host + $(if ($q) { '  ' + $q + ' qualities' } else { '' })) }; foreach ($e in $r.events) { if ($e.what -eq 'dropped') { Write-Host ('   dropped: ' + $e.detail + '  ' + $e.url.Substring(0, [Math]::Min(80, $e.url.Length))) } }"
>>"%LOG%" type "%PROBEOUT%"
echo   Full report: logs\probe-latest.json
goto finish

:do_logs
start "" "%LOGDIR%"
exit /b 0

rem ----------------------------------------------------------------------
rem  report: every check, then a zip with the results to upload to Claude.
rem  Build and tests must pass. Internet, encrypted DNS and Tor checks
rem  are optional, because they depend on the network, not the code.
:do_report
set "REPDIR=%LOGDIR%\report-%STAMP%"
mkdir "%REPDIR%" >nul 2>&1
set "LOG=%REPDIR%\report.log"
set "SUMMARY=%REPDIR%\summary.txt"
rem A clean, separate home so saved settings and lists do not change the result.
set "OSPREY_HOME=%LOGDIR%\home-%STAMP%"
set "TMPDL=%LOGDIR%\download-%STAMP%"
mkdir "%TMPDL%" >nul 2>&1
rem osprey's own diagnostic lines go to their own file inside the report.
set "OSPREY_LOG=%REPDIR%\osprey-diagnostic.log"

>"%SUMMARY%" echo BM Osprey check report
>>"%SUMMARY%" echo Date   : %DATE% %TIME%
for /f "tokens=*" %%V in ('ver') do >>"%SUMMARY%" echo Windows: %%V
call :log_header "Check report"
echo   This runs every check. The first time it compiles everything,
echo   which can take 15 to 40 minutes on a busy computer. A progress
echo   line shows every 30 seconds. Later runs are much faster.
echo.

call :doctor
>>"%SUMMARY%" echo.
>>"%SUMMARY%" echo Setup:
findstr /b /c:"Rust:" /c:"C++ build tools:" /c:"Windows SDK rc.exe:" /c:"WebView2:" /c:"git:" /c:"ffmpeg:" /c:"Python:" /c:"Memory:" "%LOG%" >>"%SUMMARY%"
call :need_cargo || goto report_pack
call :code_version

>>"%SUMMARY%" echo.
>>"%SUMMARY%" echo Required checks:
rem Lint and tests cover the engine and the command line. The browser
rem pulls in hundreds of big libraries, so here it is built once, in
rem release mode, and checked with its self-test. GitHub's CI lints and
rem tests every part on every push.
call :step "Format check" cargo fmt --all -- --check
call :step "Lint: engine and command line" cargo clippy -p osprey-core -p osprey-cli --all-targets -- -D warnings
call :step "Tests: engine and command line" cargo test -p osprey-core -p osprey-cli
call :step "Build release (engine, command line, browser)" cargo build --release
if not exist "%EXE%" goto report_pack
call :step "osprey version" "%EXE%" --version
call :step "Filters block an ad video" "%EXE%" filters check "https://pubads.g.doubleclick.net/ad.mp4"
call :step "Sniff the sample page" "%EXE%" --direct sniff "%ROOT%\samples\sample-page.html" --show-blocked
call :step "Text-only mode on the sample page" "%EXE%" --direct text "%ROOT%\samples\sample-page.html" --width 80
call :step "Privacy settings readable" "%EXE%" privacy show
if exist "%BROWSER%" call :step "Browser self-test: gateway, ad blocking, fail closed" "%BROWSER%" --self-test
if not exist "%BROWSER%" call :step "Browser was built" cmd /c exit 1

>>"%SUMMARY%" echo.
>>"%SUMMARY%" echo Optional checks (need internet, Tor or ffmpeg):
call :optional "Internet: link bypass" "%EXE%" --direct bypass "http://github.com"
call :optional "Internet: read a public HLS test stream" "%EXE%" --direct probe "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"
call :optional "Internet: download a small video" "%EXE%" --direct get "https://www.w3schools.com/html/mov_bbb.mp4" -o "%TMPDL%\sample.mp4"
if not exist "%TMPDL%\sample.mp4" goto report_skip_ffmpeg
call :optional "ffmpeg: convert video to mp3" "%EXE%" convert "%TMPDL%\sample.mp4" "%TMPDL%\sample.mp3"
goto report_after_ffmpeg
:report_skip_ffmpeg
echo   [%TIME:~0,8%] ffmpeg: convert video to mp3 ... skipped, no sample video
>>"%SUMMARY%" echo   SKIP  ffmpeg: convert video to mp3 (the sample download did not work)
:report_after_ffmpeg
call :optional "Internet: download filter lists" "%EXE%" --direct filters update
call :optional "Encrypted DNS: Cloudflare 1.1.1.1" "%EXE%" --direct --dns cloudflare privacy check --hide-ip
call :optional "Encrypted DNS: Quad9" "%EXE%" --direct --dns quad9 privacy check --hide-ip
call :optional "Tor: only passes while Tor Browser is open" "%EXE%" --anon privacy check --hide-ip

:report_pack
set "OSPREY_LOG="
if exist "%APPDATA%\bm-osprey\browser-crash.log" copy /y "%APPDATA%\bm-osprey\browser-crash.log" "%REPDIR%\browser-crash.log" >nul 2>&1
if exist "%LOGDIR%\probe-latest.json" copy /y "%LOGDIR%\probe-latest.json" "%REPDIR%\probe-latest.json" >nul 2>&1
>>"%SUMMARY%" echo.
>>"%SUMMARY%" echo Failed: %FAILS%%FAILED_STEPS%
>>"%SUMMARY%" echo Warnings: %WARNS%%WARNED_STEPS%
rem Downloads and filter lists stay out of the zip.
rmdir /s /q "%TMPDL%" >nul 2>&1
rmdir /s /q "%OSPREY_HOME%" >nul 2>&1
set "REPORT_ZIP=%ROOT%\osprey-results-%STAMP%.zip"
echo.
echo   Packing the results ...
powershell -NoProfile -Command "Compress-Archive -Path '%REPDIR%\*' -DestinationPath '%REPORT_ZIP%' -Force" >nul 2>&1
if exist "%REPORT_ZIP%" goto report_zipped
echo   Could not make the zip. Upload the files in this folder instead:
echo   %REPDIR%
set "REPORT_ZIP="
goto finish
:report_zipped
explorer /select,"%REPORT_ZIP%"
goto finish

:do_clean
call :log_header "Clean"
call :need_cargo || goto finish
call :step "Clean" cargo clean
goto finish

rem ----------------------------------------------------------------------
:finish
if defined STEPOUT del "%STEPOUT%" "%STEPBAT%" "%STEPRC%" >nul 2>&1
copy /y "%LOG%" "%LOGDIR%\latest.log" >nul 2>&1
echo.
if defined REPORT_ZIP goto finish_report
if "%FAILS%"=="0" goto finish_ok
echo   ============================================================
echo   %FAILS% step(s) FAILED:%FAILED_STEPS%
echo   ============================================================
call :summarise
call :hints
echo.
echo   Full log:  %LOG%
echo   Send this file to Claude:  %LOGDIR%\latest.log
if defined PAUSE_AT_END pause
exit /b 1

:finish_report
echo   ============================================================
echo   Required checks failed: %FAILS%%FAILED_STEPS%
echo   Optional checks not passed: %WARNS%%WARNED_STEPS%
echo   ============================================================
if not "%FAILS%"=="0" call :hints
echo.
echo   Results zip, ready to upload to Claude:
echo   %REPORT_ZIP%
echo.
echo   It holds only the check logs and a summary. Your IP address is
echo   hidden in it, and no downloads or settings are included.
if defined PAUSE_AT_END pause
if "%FAILS%"=="0" exit /b 0
exit /b 1

:finish_ok
echo   All done, no problems.
echo   Log: %LOG%
if defined PAUSE_AT_END pause
exit /b 0

rem ======================================================================
rem  Helpers
rem ======================================================================

:log_header
>"%LOG%" echo BM Osprey dev tool log
>>"%LOG%" echo Action : %~1
>>"%LOG%" echo Started: %DATE% %TIME%
>>"%LOG%" echo Folder : %ROOT%
for /f "tokens=*" %%V in ('ver') do >>"%LOG%" echo Windows: %%V
>>"%LOG%" echo.
echo.
echo   %~1   (log: %LOG%)
echo.
exit /b 0

rem  :step "Label" command args...
rem  Runs a command, sends all of its output to the log, prints OK or FAILED.
:step
set "STEP_LABEL=%~1"
set "STEP_CMD="
shift
:step_args
if "%~1"=="" goto step_run
set STEP_CMD=%STEP_CMD% %1
shift
goto step_args
:step_run
echo   [%TIME:~0,8%] %STEP_LABEL% ...
>>"%LOG%" echo.
>>"%LOG%" echo ==== %STEP_LABEL%   [%TIME%]
>>"%LOG%" echo ^>%STEP_CMD%
set "STEPOUT=%LOGDIR%\step-%STAMP%.tmp"
set "STEPBAT=%LOGDIR%\step-%STAMP%.cmd"
set "STEPRC=%LOGDIR%\step-%STAMP%.rc"
del "%STEPRC%" >nul 2>&1
rem The step runs in the background, so this window can show that it is
rem still working. Its output goes to the step file, then to the log.
>"%STEPBAT%" echo @echo off
>>"%STEPBAT%" echo call %STEP_CMD% ^>"%STEPOUT%" 2^>^&1
>>"%STEPBAT%" echo ^>"%STEPRC%" echo %%ERRORLEVEL%%
start "" /b cmd /d /c "%STEPBAT%"
set "WAITED=0"
:step_wait
if exist "%STEPRC%" goto step_finished
ping -n 3 127.0.0.1 >nul
set /a WAITED+=2
set /a SHOW=WAITED %% 30
if not "%SHOW%"=="0" goto step_wait
call :step_progress
goto step_wait
:step_finished
rem Give the background step a moment to close its files.
ping -n 2 127.0.0.1 >nul
set "RC="
set /p RC=<"%STEPRC%"
if not defined RC set "RC=1"
set "RC=%RC: =%"
type "%STEPOUT%" >>"%LOG%"
>>"%LOG%" echo ==== exit code %RC%
if "%RC%"=="0" goto step_ok
call :recover_build
if "%RECOVER%"=="1" goto step_run
if defined STEP_OPTIONAL goto step_warn
echo              FAILED  exit code %RC%
set /a FAILS+=1
set "FAILED_STEPS=%FAILED_STEPS% [%STEP_LABEL%]"
call :sumline FAIL
exit /b %RC%
:step_warn
echo              not passed, optional  exit code %RC%
set /a WARNS+=1
set "WARNED_STEPS=%WARNED_STEPS% [%STEP_LABEL%]"
call :sumline WARN
exit /b 0
:step_ok
echo              OK
call :sumline PASS
exit /b 0

rem  Looks for rc.exe in the 64 bit folder of each Windows Kits 10 bin
rem  version under the folder it is given. The last one listed is newest.
:find_rc
if not exist "%~1\Windows Kits\10\bin" exit /b 0
for /f "delims=" %%F in ('dir /b /s /o:n "%~1\Windows Kits\10\bin\rc.exe" 2^>nul ^| findstr /i /r /c:"\\x64\\rc\.exe"') do set "RCEXE=%%F"
exit /b 0

rem  One progress line while a long step runs: how many libraries are done
rem  and which one is being built now.
:step_progress
set "DONE=0"
for /f %%N in ('findstr /r /c:"^ *Compiling " /c:"^ *Checking " "%STEPOUT%" 2^>nul ^| find /c /v ""') do set "DONE=%%N"
set "LAST="
for /f "tokens=1,2" %%A in ('findstr /r /c:"^ *Compiling " /c:"^ *Checking " /c:"^ *Running " /c:"^test " "%STEPOUT%" 2^>nul') do set "LAST=%%A %%B"
set /a MINS=WAITED/60
set /a SECS=WAITED %% 60
if %SECS% LSS 10 set "SECS=0%SECS%"
if defined LAST echo              still working %MINS%:%SECS%  %DONE% done, now: %LAST%
if not defined LAST echo              still working %MINS%:%SECS%
exit /b 0

rem  When a build ran out of memory, or left damaged files behind from an
rem  earlier crash, cleans the build folder and asks for one retry with
rem  fewer jobs. Happens at most once per run.
:recover_build
set "RECOVER=0"
if defined RETRIED exit /b 0
findstr /i /c:"rust_oom" /c:"memory allocation of" /c:"out of memory" /c:"os error 1455" /c:"invalid metadata files" /c:"only metadata stub" /c:"STATUS_STACK_BUFFER_OVERRUN" "%STEPOUT%" >nul 2>&1
if errorlevel 1 exit /b 0
set "RETRIED=1"
set "RECOVER=1"
set /a CARGO_BUILD_JOBS=CARGO_BUILD_JOBS/2
if %CARGO_BUILD_JOBS% LSS 1 set "CARGO_BUILD_JOBS=1"
echo              out of memory or damaged build files. Cleaning, then
echo              retrying with %CARGO_BUILD_JOBS% job(s) at a time. This takes a while.
>>"%LOG%" echo ==== Auto recovery: memory or damaged build files. cargo clean, then retry with %CARGO_BUILD_JOBS% jobs
cargo clean >>"%LOG%" 2>&1
exit /b 0

rem  Adds one line to the report summary, when a report is running.
rem  The redirect comes first so labels ending in a digit stay safe.
:sumline
if not defined SUMMARY exit /b 0
>>"%SUMMARY%" echo   %~1  %STEP_LABEL%
exit /b 0

rem  :optional "Label" command args...  Same as :step, but a failure only
rem  counts as a warning.
:optional
set "STEP_OPTIONAL=1"
call :step %*
set "STEP_OPTIONAL="
exit /b 0

rem  Records which version of the code is being checked.
:code_version
where git >nul 2>&1
if errorlevel 1 exit /b 0
>>"%LOG%" echo ==== Code version
git log -3 --oneline >>"%LOG%" 2>&1
git status --short >>"%LOG%" 2>&1
set "GITV="
for /f "tokens=*" %%G in ('git log -1 --oneline 2^>nul') do set "GITV=%%G"
if not defined GITV exit /b 0
>>"%SUMMARY%" echo.
>>"%SUMMARY%" echo Code version: %GITV%
exit /b 0

:need_cargo
where cargo >nul 2>&1
if not errorlevel 1 exit /b 0
echo   Rust is not installed, or this window cannot see it.
>>"%LOG%" echo cargo not found on PATH
set /a FAILS+=1
set "FAILED_STEPS=%FAILED_STEPS% [Rust missing]"
exit /b 1

rem  Checks every tool the project needs and writes versions to the log.
:doctor
>>"%LOG%" echo ==== Setup check
set "DOC_BAD=0"

where cargo >nul 2>&1
if errorlevel 1 goto doc_no_rust
set "V="
for /f "tokens=*" %%V in ('rustc --version 2^>nul') do set "V=%%V"
echo   OK       Rust          %V%
>>"%LOG%" echo Rust: %V%
>>"%LOG%" echo PATH to cargo:
where cargo >>"%LOG%" 2>&1
rustc -vV >>"%LOG%" 2>&1
rustup show active-toolchain >>"%LOG%" 2>&1
goto doc_vs
:doc_no_rust
echo   MISSING  Rust          install it from https://rustup.rs
echo                          or run: winget install Rustlang.Rustup
>>"%LOG%" echo Rust: MISSING
set /a DOC_BAD+=1

:doc_vs
rem Rust on Windows needs the Microsoft C++ build tools for linking.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
set "VSPATH="
if not exist "%VSWHERE%" goto doc_vs_missing
rem "call" in front stops cmd from stripping the quotes around a path with (x86) in it.
for /f "usebackq tokens=*" %%P in (`call "%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath`) do set "VSPATH=%%P"
if not defined VSPATH goto doc_vs_missing
echo   OK       C++ tools     %VSPATH%
>>"%LOG%" echo C++ build tools: %VSPATH%
goto doc_git
:doc_vs_missing
echo   MISSING  C++ tools     Rust needs the Visual Studio C++ build tools.
echo                          Install "Build Tools for Visual Studio" and tick
echo                          "Desktop development with C++".
echo                          Ignore this if you use the GNU Rust toolchain.
>>"%LOG%" echo C++ build tools: MISSING
set /a DOC_BAD+=1

:doc_git
if not defined RCEXE goto doc_no_rc
echo   OK       Windows SDK   %RCEXE%
>>"%LOG%" echo Windows SDK rc.exe: %RCEXE%
>>"%LOG%" echo RC before this run: [%RC_BEFORE%]
>>"%LOG%" echo RC now: [%RC%]
>>"%LOG%" echo where rc.exe:
where rc.exe >>"%LOG%" 2>&1
"%RCEXE%" /? >nul 2>&1
>>"%LOG%" echo rc.exe starts: exit code %ERRORLEVEL%
goto doc_wv2
:doc_no_rc
echo   MISSING  Windows SDK   the browser needs its resource compiler (rc.exe).
echo                          Open Visual Studio Installer, Modify, Individual components,
echo                          tick "Windows 11 SDK", or run:
echo                          winget install Microsoft.WindowsSDK.10.0.26100
>>"%LOG%" echo Windows SDK rc.exe: MISSING
set /a DOC_BAD+=1
:doc_wv2
rem The browser needs Microsoft Edge WebView2. Windows 10 and 11 normally have it.
set "WV2="
for /f "tokens=3" %%W in ('reg query "HKLM\SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" /v pv 2^>nul ^| findstr /i "pv"') do set "WV2=%%W"
if not defined WV2 for /f "tokens=3" %%W in ('reg query "HKCU\SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" /v pv 2^>nul ^| findstr /i "pv"') do set "WV2=%%W"
if not defined WV2 goto doc_no_wv2
echo   OK       WebView2      %WV2%
>>"%LOG%" echo WebView2: %WV2%
goto doc_git2
:doc_no_wv2
echo   MISSING  WebView2      the browser needs it. winget install Microsoft.EdgeWebView2Runtime
>>"%LOG%" echo WebView2: MISSING
:doc_git2
set "V="
for /f "tokens=*" %%V in ('git --version 2^>nul') do set "V=%%V"
if not defined V goto doc_no_git
echo   OK       git           %V%
>>"%LOG%" echo git: %V%
goto doc_ffmpeg
:doc_no_git
echo   MISSING  git           only needed for GitHub. winget install Git.Git
>>"%LOG%" echo git: MISSING

:doc_ffmpeg
set "FF=ffmpeg"
if defined OSPREY_FFMPEG set "FF=%OSPREY_FFMPEG%"
set "V="
for /f "tokens=1-3" %%A in ('call "%FF%" -version 2^>nul') do if not defined V set "V=%%A %%B %%C"
if not defined V goto doc_no_ffmpeg
echo   OK       ffmpeg        %V%
>>"%LOG%" echo ffmpeg: %V%
goto doc_python
:doc_no_ffmpeg
echo   WARNING  ffmpeg        not found. Building works without it, but merging
echo                          streams and converting need it.
echo                          winget install Gyan.FFmpeg  then open a new window
>>"%LOG%" echo ffmpeg: MISSING

:doc_python
set "V="
for /f "tokens=*" %%V in ('python --version 2^>nul') do set "V=%%V"
if not defined V goto doc_no_python
echo   OK       Python        %V%  (for the test server scripts)
>>"%LOG%" echo Python: %V%
goto doc_disk
:doc_no_python
echo   optional Python        only for scripts\dev_server.py
>>"%LOG%" echo Python: not found

:doc_disk
for /f "tokens=3" %%F in ('dir /-c "%ROOT%" 2^>nul ^| findstr /c:"bytes free"') do set "FREE=%%F"
>>"%LOG%" echo Free disk bytes: %FREE%
>>"%LOG%" echo Memory: %FREEMB% MB free of %TOTALMB% MB. Build jobs at once: %CARGO_BUILD_JOBS%
echo   info     Memory        %FREEMB% MB free of %TOTALMB% MB, building with %CARGO_BUILD_JOBS% job(s) at once
>>"%LOG%" echo.
if "%DOC_BAD%"=="0" exit /b 0
set /a FAILS+=1
set "FAILED_STEPS=%FAILED_STEPS% [setup: %DOC_BAD% missing]"
exit /b 1

rem  Prints the important lines from the log so the cause is visible at once.
:summarise
echo.
echo   Key lines from the log:
echo   -----------------------
findstr /n /r /c:"^error" /c:"^warning: unused" /c:"panicked at" /c:"^test .* FAILED" /c:"^ *--> " /c:"MISSING" /c:"CRASH" /c:"ERROR:" "%LOG%"
exit /b 0

rem  Known problems with a plain explanation of the fix.
:hints
set "HINTED="
findstr /i /c:"rust_oom" /c:"memory allocation of" /c:"os error 1455" /c:"invalid metadata files" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint1
echo.
echo   HINT  The compiler ran out of memory. Close big programs (browsers,
echo         games) while building, or build with fewer jobs:
echo         set CARGO_BUILD_JOBS=1  then run RUN-CHECKS.bat again.
echo         If the paging file is small, letting Windows manage it helps.
set "HINTED=1"
:hint1
findstr /i /c:"RC.EXE in your" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint1b
echo.
echo   HINT  The Windows SDK resource compiler (rc.exe) is missing. Open Visual
echo         Studio Installer, Modify, Individual components, tick "Windows 11
echo         SDK", or run: winget install Microsoft.WindowsSDK.10.0.26100
echo         Then open a new window and run RUN-CHECKS.bat again.
set "HINTED=1"
:hint1b
findstr /i /c:"link.exe" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint2
echo.
echo   HINT  The Microsoft C++ linker is missing. Install "Build Tools for
echo         Visual Studio" with "Desktop development with C++", then reopen.
set "HINTED=1"
:hint2
findstr /i /c:"os error 5" /c:"Access is denied" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint3
echo.
echo   HINT  Windows blocked a file. Close any running osprey.exe, then try again.
echo         If it keeps happening, your antivirus may be holding the file.
set "HINTED=1"
:hint3
findstr /i /c:"Blocking waiting for file lock" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint4
echo.
echo   HINT  Another build is already running. Wait for it or close it.
set "HINTED=1"
:hint4
findstr /i /c:"failed to download" /c:"spurious network error" /c:"failed to get" /c:"Couldn't resolve host" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint5
echo.
echo   HINT  Cargo could not download libraries. Check your internet
echo         connection, VPN or proxy, then try again.
set "HINTED=1"
:hint5
findstr /i /c:"ffmpeg was not found" /c:"could not start ffmpeg" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint6
echo.
echo   HINT  ffmpeg is needed for this. Run: winget install Gyan.FFmpeg
echo         then open a new terminal window.
set "HINTED=1"
:hint6
findstr /i /c:"is not installed for the toolchain" /c:"rustfmt" /c:"clippy" "%LOG%" | findstr /i /c:"not installed" /c:"no such command" >nul 2>&1
if errorlevel 1 goto hint7
echo.
echo   HINT  A Rust tool is missing. Run: rustup component add rustfmt clippy
set "HINTED=1"
:hint7
findstr /i /c:"Diff in" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint8
echo.
echo   HINT  Only formatting differs. Run: cargo fmt --all
set "HINTED=1"
:hint8
findstr /i /c:"os error 112" /c:"not enough space" /c:"No space left" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint9
echo.
echo   HINT  The disk is full. Free some space, or run: dev clean
set "HINTED=1"
:hint9
findstr /i /c:"Tor is not running" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint9b
echo.
echo   HINT  Anonymous or Tor mode is on but Tor is not running. Open Tor
echo         Browser and leave it open, then try again. Nothing was sent.
echo         To switch it off: osprey privacy off
set "HINTED=1"
:hint9b
findstr /i /c:"encrypted DNS" "%LOG%" | findstr /i /c:"did not work" /c:"FAILED" >nul 2>&1
if errorlevel 1 goto hint9c
echo.
echo   HINT  Encrypted DNS is blocked on this network. Try: osprey privacy dns quad9
echo         or check that your firewall or antivirus allows DNS over HTTPS.
set "HINTED=1"
:hint9c
findstr /i /c:"DRM protected" "%LOG%" >nul 2>&1
if errorlevel 1 goto hint10
echo.
echo   HINT  That stream uses DRM. BM Osprey does not download DRM content.
set "HINTED=1"
:hint10
findstr /i /c:"server replied 403" /c:"server replied 401" "%LOG%" >nul 2>&1
if errorlevel 1 goto hints_end
echo.
echo   HINT  The site refused the request. Try adding the page as referer:
echo         -H "Referer: https://the-page-you-watched-on/"
set "HINTED=1"
:hints_end
if defined HINTED exit /b 0
echo.
echo   No known cause matched. Send logs\latest.log to Claude.
exit /b 0
