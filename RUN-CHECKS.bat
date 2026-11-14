@echo off
rem Double click me. I build BM Osprey, run every check, and make a
rem osprey-results-<date>.zip in this folder to upload to Claude.
cd /d "%~dp0"
set "PAUSE_AT_END=1"
call "%~dp0dev.bat" report
