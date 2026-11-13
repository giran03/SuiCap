@echo off
setlocal EnableExtensions
cd /d "%~dp0.."
title SuiCap - Check

rem Standing checks (plan section 8). Usage: .scripts\SuiCap Check.bat [quiet]
set "QUIET=0"
if /i "%~1"=="quiet" set "QUIET=1"

if not exist "vendor\ffmpeg\include\libavcodec\avcodec.h" (
  call "%~dp0SuiCap Fetch FFmpeg.bat" quiet || goto :fail
)

if not exist "node_modules\" (
  echo Installing front-end packages...
  call bun install || goto :fail
)
echo ==== UI: svelte-check + build ====
call bun run check || goto :fail
call bun run build || goto :fail

echo ==== cargo fmt --check ====
cargo fmt --all --check || goto :fail
echo ==== cargo clippy ====
cargo clippy --workspace --all-targets -- -D warnings || goto :fail
echo ==== cargo test ====
cargo test --workspace || goto :fail
echo ==== zero-copy guard: no Map( on frame textures ====
powershell -NoProfile -Command "$hits = Get-ChildItem crates -Recurse -Filter *.rs | Select-String -CaseSensitive -Pattern '\.Map\(' ; if ($hits) { $hits; exit 1 }" || goto :fail

echo.
echo All checks passed.
if "%QUIET%"=="0" pause
exit /b 0

:fail
echo.
echo [error] Checks FAILED.
if "%QUIET%"=="0" pause
exit /b 1
