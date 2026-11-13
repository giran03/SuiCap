@echo off
setlocal EnableExtensions
cd /d "%~dp0.."
title SuiCap - Dev Mode

rem Usage: .scripts\SuiCap Dev.bat [quiet]
set "QUIET=0"
if /i "%~1"=="quiet" set "QUIET=1"

where cargo >nul 2>&1
if errorlevel 1 (
  echo [error] Rust ^(cargo^) was not found in PATH. Install from https://rustup.rs
  goto :fail
)
where bun >nul 2>&1
if errorlevel 1 (
  echo [error] Bun was not found in PATH. Install from https://bun.sh
  goto :fail
)
if not exist "vendor\ffmpeg\include\libavcodec\avcodec.h" (
  call "%~dp0SuiCap Fetch FFmpeg.bat" quiet || goto :fail
)
if not exist "node_modules\" (
  echo Installing front-end packages...
  call bun install || goto :fail
)

echo [info] Starting Tauri dev...
call bun run tauri dev
if errorlevel 1 goto :fail
exit /b 0

:fail
echo.
echo [error] Dev process exited with an error.
if "%QUIET%"=="0" pause
exit /b 1
