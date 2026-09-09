@echo off
setlocal

cd /d "%~dp0"

echo [1/4] Refreshing application icons...
call npx tauri icon src/assets/icon.png
if errorlevel 1 goto :failed

echo [2/4] Building frontend...
call npm run build
if errorlevel 1 goto :failed

echo [3/4] Building Windows service...
pushd src-tauri
cargo build --release --bin cpum_service
if errorlevel 1 (
  popd
  goto :failed
)
popd

echo [4/4] Building NSIS installer...
call npx tauri build --bundles nsis
if errorlevel 1 goto :failed

echo.
echo Build completed:
echo src-tauri\target\release\bundle\nsis\CPU Manager_0.1.0_x64-setup.exe
exit /b 0

:failed
echo.
echo Build failed.
exit /b 1
