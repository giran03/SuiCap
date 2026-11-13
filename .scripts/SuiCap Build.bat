@echo off
setlocal EnableExtensions
cd /d "%~dp0.."
title SuiCap - Build Installer

rem Usage: .scripts\SuiCap Build.bat [noclean] [quiet]
rem Builds the NSIS installer into release\ and the CLI harness into release\suicap-cli\.
set "DO_CLEAN=1"
set "QUIET=0"
for %%A in (%*) do (
  if /i "%%~A"=="noclean" set "DO_CLEAN=0"
  if /i "%%~A"=="quiet" set "QUIET=1"
)

where cargo >nul 2>&1
if errorlevel 1 (
  echo [error] Rust ^(cargo^) was not found. Install it from https://rustup.rs and try again.
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

echo ==== Building the app and compressed NSIS installer ====
call bun run tauri build
if errorlevel 1 goto :fail

set "BUNDLE=target\release\bundle\nsis"
set "SETUP="
for /f "delims=" %%F in ('dir /b /o-d "%BUNDLE%\*-setup.exe" 2^>nul') do (
  if not defined SETUP set "SETUP=%%F"
)
if not defined SETUP (
  echo [error] Build finished but no NSIS installer was found in %BUNDLE%.
  goto :fail
)
if not exist "release\" mkdir "release"
copy /y "%BUNDLE%\%SETUP%" "release\%SETUP%" >nul || goto :fail
for %%A in ("release\%SETUP%") do set "SIZE=%%~zA"
set /a SIZE_MB=%SIZE% / 1048576
echo Installer ready: release\%SETUP%  ^(about %SIZE_MB% MB^)

echo ==== Building the CLI harness ====
cargo build --release -p suicap-cli || goto :fail
if not exist "release\suicap-cli\" mkdir "release\suicap-cli"
copy /y "target\release\suicap-cli.exe" "release\suicap-cli\" >nul || goto :fail
for %%D in (avcodec-63 avformat-63 avutil-61 swresample-7) do (
  copy /y "vendor\ffmpeg\bin\%%D.dll" "release\suicap-cli\" >nul || goto :fail
)
echo CLI ready: release\suicap-cli\suicap-cli.exe

if "%DO_CLEAN%"=="1" (
  echo Cleaning temporary build files...
  call "%~dp0SuiCap Clean.bat" quiet
)

echo Done.
if "%QUIET%"=="0" pause
exit /b 0

:fail
echo.
echo Build FAILED. Nothing was cleaned up, so you can inspect the output above.
if "%QUIET%"=="0" pause
exit /b 1
