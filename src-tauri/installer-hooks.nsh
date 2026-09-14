; ---------------------------------------------------------------------------
; CPU Manager NSIS hooks
;
; The installer is a per-user ("currentUser") installer: it runs un-elevated
; and installs into %LOCALAPPDATA%, and the desktop app itself never needs
; administrator rights. Only Windows service management requires elevation,
; so every service command below is skipped when the service is not installed
; and is otherwise re-launched through an elevated helper (one UAC prompt).
;
; Rules live in the per-user Tauri data directory (%APPDATA%\com.eason.cpum)
; because an un-elevated app cannot write to machine-level locations such as
; %ProgramData%. The service receives that directory as its startup argument
; and runs as LocalSystem, which can read it.
; ---------------------------------------------------------------------------

; Run <bat> through an elevated (UAC) cmd.exe and wait for it.
; Input:  $R1 = full path of the batch file to run elevated
!macro CpumRunElevatedBat
  Push $R0
  StrCpy $R0 'powershell.exe -NoProfile -NonInteractive -Command "Start-Process -FilePath cmd.exe -ArgumentList \"/c\",\"$R1\" -Verb RunAs -Wait"'
  nsExec::ExecToLog '$R0'
  Pop $R0
  Sleep 1000
  Pop $R0
!macroend

!macro NSIS_HOOK_PREINSTALL
  ; The service keeps cpum_service.exe open, which would block the file copy
  ; during an in-place upgrade. Try an un-elevated stop first: most users have
  ; no service installed, and this stays completely silent in that case.
  Push $R0
  Push $R1

  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop "CpumAffinityService"'
  Pop $R0
  StrCmp "$R0" "0" cpum_pre_stopped
  ; 1060 = service not installed, 1062 = installed but not running.
  StrCmp "$R0" "1060" cpum_pre_done
  StrCmp "$R0" "1062" cpum_pre_done

  ; Anything else (typically 5 = access denied) means the service exists but
  ; we may not control it. Elevate so the binary can be replaced.
  StrCpy $R1 "$TEMP\cpum_service_stop.bat"
  Push $R0
  FileOpen $R0 "$R1" w
  FileWrite $R0 '@echo off$\r$\n'
  FileWrite $R0 'sc.exe stop "CpumAffinityService"$\r$\n'
  FileWrite $R0 'exit /b 0$\r$\n'
  FileClose $R0
  Pop $R0
  !insertmacro CpumRunElevatedBat
  Delete "$R1"

  cpum_pre_stopped:
  Sleep 2000

  cpum_pre_done:
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_POSTINSTALL
  Push $R0
  Push $R1
  Push $R2
  Push $R3

  ; ---- Migrate rule files into the per-user data directory ----------------
  ; $COMMONAPPDATA / $PROGRAMDATA are not valid NSIS constants (they stay
  ; literal and silently create a junk folder inside $INSTDIR), so read the
  ; machine-level ProgramData directory from the environment.
  StrCpy $R0 "$APPDATA\com.eason.cpum"
  CreateDirectory "$R0"

  ReadEnvStr $R2 "ProgramData"
  StrCmp "$R2" "" 0 +2
    StrCpy $R2 "C:\ProgramData"
  StrCpy $R2 "$R2\cpum"

  IfFileExists "$R0\affinity_rules.json" cpum_rules_migrated
  IfFileExists "$R2\affinity_rules.json" 0 cpum_try_legacy_rules
    CopyFiles /SILENT "$R2\affinity_rules.json" "$R0\affinity_rules.json"
    Goto cpum_rules_migrated
  cpum_try_legacy_rules:
  IfFileExists "$APPDATA\cpum\affinity_rules.json" 0 cpum_rules_migrated
    CopyFiles /SILENT "$APPDATA\cpum\affinity_rules.json" "$R0\affinity_rules.json"
  cpum_rules_migrated:

  IfFileExists "$R0\probalance.json" cpum_pb_migrated
  IfFileExists "$R2\probalance.json" 0 cpum_pb_migrated
    CopyFiles /SILENT "$R2\probalance.json" "$R0\probalance.json"
  cpum_pb_migrated:

  ; ---- Refresh an already installed service -------------------------------
  ; Installing or updating the app never creates the service; that is an
  ; explicit, elevated action performed from inside the app. Only rebind an
  ; existing service to the new install directory / rules directory.
  nsExec::ExecToStack '"$SYSDIR\sc.exe" query "CpumAffinityService"'
  Pop $R3
  Pop $R1
  StrCmp "$R3" "0" 0 cpum_post_done

  ; Tauri maps bundle resources next to the installed executable, but older
  ; builds shipped them in a resources subdirectory. Detect the actual
  ; location so upgrades keep working across layout changes.
  StrCpy $R1 "$INSTDIR\cpum_service.exe"
  IfFileExists "$R1" 0 cpum_svc_in_resources
    Goto cpum_svc_path_ready
  cpum_svc_in_resources:
    StrCpy $R1 "$INSTDIR\resources\cpum_service.exe"
  cpum_svc_path_ready:

  StrCpy $R3 "$TEMP\cpum_service_update.bat"
  FileOpen $R2 "$R3" w
  FileWrite $R2 '@echo off$\r$\n'
  FileWrite $R2 'sc.exe config "CpumAffinityService" binPath= "\"$R1\" \"$R0\"" start= auto$\r$\n'
  FileWrite $R2 'sc.exe start "CpumAffinityService"$\r$\n'
  FileWrite $R2 'exit /b 0$\r$\n'
  FileClose $R2
  StrCpy $R1 "$R3"
  !insertmacro CpumRunElevatedBat
  Delete "$R3"

  cpum_post_done:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  ; Remove the optional service when the app is uninstalled, otherwise it
  ; would keep pointing at a deleted cpum_service.exe. Skipped while updating,
  ; because the updater runs the previous uninstaller with /UPDATE first.
  Push $R0
  Push $R1
  Push $R2

  ${If} $UpdateMode <> 1
    nsExec::ExecToStack '"$SYSDIR\sc.exe" query "CpumAffinityService"'
    Pop $R2
    Pop $R1
    StrCmp "$R2" "0" 0 cpum_preun_done

    StrCpy $R1 "$TEMP\cpum_service_remove.bat"
    FileOpen $R0 "$R1" w
    FileWrite $R0 '@echo off$\r$\n'
    FileWrite $R0 'sc.exe stop "CpumAffinityService"$\r$\n'
    FileWrite $R0 'sc.exe delete "CpumAffinityService"$\r$\n'
    FileWrite $R0 'exit /b 0$\r$\n'
    FileClose $R0
    !insertmacro CpumRunElevatedBat
    Delete "$R1"
    Sleep 2000
  ${EndIf}

  cpum_preun_done:
  Pop $R2
  Pop $R1
  Pop $R0
!macroend
