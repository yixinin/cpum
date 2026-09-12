@echo off
setlocal EnableExtensions EnableDelayedExpansion
set "ROOT=%~dp0"
cd /d "%ROOT%"

set "BUNDLE_ROOT=src-tauri\target\release\bundle"
set "BUNDLE_DIR=%BUNDLE_ROOT%\nsis"
set "RELEASE_DIR=release"
set "UPDATER_KEY=src-tauri\tauri.key"

echo [1/5] Refreshing application icons...
call npx tauri icon src/assets/icon.png
if errorlevel 1 goto :failed

echo [2/5] Building frontend...
call npm run build
if errorlevel 1 goto :failed

echo [3/5] Building Windows service...
pushd src-tauri
cargo build --release -p cpum-service --bin cpum_service
if errorlevel 1 (
  popd
  goto :failed
)
popd

echo [4/5] Building NSIS installer...
if exist "%UPDATER_KEY%" (
  echo       Found %UPDATER_KEY% - updater artifacts (.nsis.zip and .sig) will be signed.
  set "TAURI_SIGNING_PRIVATE_KEY_PATH=%UPDATER_KEY%"
  call npx tauri build --bundles nsis --config src-tauri\tauri.updater.conf.json
) else (
  echo       No %UPDATER_KEY% found - skipping updater signing. Generate one with
  echo       "npx tauri signer generate -w %UPDATER_KEY%" to enable local signing.
  call npx tauri build --bundles nsis
)
if errorlevel 1 goto :failed

echo [5/5] Collecting updater artifacts and writing SHA256SUMS...
if not exist "%BUNDLE_DIR%" (
  echo   No NSIS bundle directory found at %BUNDLE_DIR%, skipping.
  goto :summary
)
if not exist "%RELEASE_DIR%" mkdir "%RELEASE_DIR%"
if errorlevel 1 goto :failed

rem Copy the installer with a stable name.
for %%I in ("%BUNDLE_DIR%\*-setup.exe") do (
  copy /Y "%%~fI" "%RELEASE_DIR%\CPU-Manager_local_windows-x64-setup.exe" >nul
  if errorlevel 1 goto :failed
)

rem Copy updater artifacts when Tauri produced them (requires tauri.key on PATH).
set "HAVE_UPDATER=0"
for %%I in ("%BUNDLE_DIR%\*.nsis.zip" "%BUNDLE_DIR%\*.nsis.zip.sig" "%BUNDLE_DIR%\*-setup.exe.sig") do (
  if exist "%%~fI" (
    copy /Y "%%~fI" "%RELEASE_DIR%\" >nul
    if errorlevel 1 goto :failed
    set "HAVE_UPDATER=1"
  )
)

if "!HAVE_UPDATER!"=="1" (
  echo   Updater artifacts copied to %RELEASE_DIR%.
) else (
  echo   No updater artifacts produced.
)

rem Emit SHA256SUMS.txt (LF line endings, no BOM) using PowerShell so local
rem builds ship a checksum file identical in format to the release pipeline.
powershell -NoProfile -Command ^
  "$root = Resolve-Path '%RELEASE_DIR%';" ^
  "$lines = [System.Collections.Generic.List[string]]::new();" ^
  "Get-ChildItem $root -File | Sort-Object Name | ForEach-Object {" ^
  "  $hash = (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower();" ^
  "  $lines.Add(\"$hash  $($_.Name)\")" ^
  "};" ^
  "$content = ($lines -join \"`n\") + \"`n\";" ^
  "$utf8 = New-Object System.Text.UTF8Encoding($false);" ^
  "[System.IO.File]::WriteAllText((Join-Path $root 'SHA256SUMS.txt'), $content, $utf8)"
if errorlevel 1 goto :failed

:summary
echo.
echo Build completed. Artifacts:
if exist "%BUNDLE_DIR%\*-setup.exe" (
  for %%I in ("%BUNDLE_DIR%\*-setup.exe") do echo   %%~nxI
)
if exist "%RELEASE_DIR%\SHA256SUMS.txt" (
  echo.
  echo Checksums:
  type "%RELEASE_DIR%\SHA256SUMS.txt"
)
exit /b 0

:failed
echo.
echo Build failed.
exit /b 1
