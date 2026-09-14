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
if not exist "%UPDATER_KEY%" goto :nsi_no_key

echo       Found %UPDATER_KEY% - updater artifacts ^(.nsis.zip and .sig^) can be signed.
rem The CLI reads TAURI_SIGNING_PRIVATE_KEY and accepts either the key content or
rem the path to the key file. The path must be absolute because the CLI switches
rem its working directory to src-tauri before bundling.
set "TAURI_SIGNING_PRIVATE_KEY=%ROOT%%UPDATER_KEY%"
rem A key created by `tauri signer generate` is password protected. Without the
rem password the CLI stops at an interactive prompt, which hangs a
rem non-interactive build forever, so refuse to sign and say so instead.
if defined TAURI_SIGNING_PRIVATE_KEY_PASSWORD goto :nsi_signed
echo       WARNING: TAURI_SIGNING_PRIVATE_KEY_PASSWORD is not set, so updater
echo                artifacts ^(.nsis.zip and .sig^) will NOT be produced.
echo                Set that variable and re-run to sign them.

:nsi_no_key
if not exist "%UPDATER_KEY%" echo       No %UPDATER_KEY% found - skipping updater signing. Generate one with
if not exist "%UPDATER_KEY%" echo       "npx tauri signer generate -w %UPDATER_KEY%" to enable local signing.

:nsi_unsigned
call npx tauri build --bundles nsis --config "{\"bundle\":{\"createUpdaterArtifacts\":false}}"
goto :nsi_built

:nsi_signed
call npx tauri build --bundles nsis

:nsi_built
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
