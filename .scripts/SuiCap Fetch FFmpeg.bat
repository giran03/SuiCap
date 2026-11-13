@echo off
setlocal EnableExtensions
cd /d "%~dp0.."
title SuiCap - Fetch FFmpeg

rem Usage: .scripts\SuiCap Fetch FFmpeg.bat [force] [quiet]
set "FORCE="
set "QUIET=0"
for %%A in (%*) do (
  if /i "%%~A"=="force" set "FORCE=-Force"
  if /i "%%~A"=="quiet" set "QUIET=1"
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0fetch-ffmpeg.ps1" %FORCE%
if errorlevel 1 goto :fail
if "%QUIET%"=="0" pause
exit /b 0

:fail
echo.
echo [error] Fetching FFmpeg failed.
if "%QUIET%"=="0" pause
exit /b 1
