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

!ifndef CPUM_SERVICE_NAME
  !define CPUM_SERVICE_NAME "CpumAffinityService"
!endif

; Run the cmd.exe command line held in $R1 through an elevated (UAC) cmd.exe
; and wait for it to finish.
;
; Notes on why this is written the way it is:
;   * `Start-Process -Verb RunAs -Wait` is used because NSIS cannot wait for an
;     elevated child started with `ExecShell "runas"`, and the uninstaller must
;     not race ahead and delete files the service still holds open.
;   * The command line is passed as a single-quoted PowerShell argument. An
;     earlier version pointed at a helper batch file with
;     `-ArgumentList '/c','<path>'`; PowerShell joins the array elements with a
;     plain space, so any path containing a space (a user name with a space is
;     enough) silently broke the call. Passing cmd's command text instead means
;     no temporary file is written and no path has to survive the trip.
;   * `cmd /c` treats everything after /c as the command, so spaces inside the
;     command text are harmless.
;
; $R1 is preserved; $R0 is clobbered but saved and restored.
!macro CpumRunElevatedCmd
  Push $R0
  Push $R1
  DetailPrint "CPUM: running elevated: $R1"
  StrCpy $R0 "powershell.exe -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command $\"Start-Process -FilePath cmd.exe -ArgumentList '/c','$R1' -Verb RunAs -Wait$\""
  nsExec::ExecToLog '$R0'
  Pop $R0
  StrCmp "$R0" "0" +2
  DetailPrint "CPUM: elevated helper exited with $R0 (5 = access denied, 1223 = UAC declined)"
  Pop $R1
  Pop $R0
!macroend

; ---------------------------------------------------------------------------
; Before install / upgrade: the service keeps cpum_service.exe open, which
; would block the file copy. Try an un-elevated stop first (silent, and the
; common case is "no service installed at all"), then escalate.
; ---------------------------------------------------------------------------
!macro NSIS_HOOK_PREINSTALL
  Push $R0
  Push $R1
  Push $R2

  DetailPrint "CPUM: stopping ${CPUM_SERVICE_NAME} before install"
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop "${CPUM_SERVICE_NAME}"'
  Pop $R2
  ; 1060 = not installed, 1062 = already stopped, 1072 = already marked for
  ; deletion: nothing to wait for. Anything else means the service exists and
  ; we need to make sure it actually let go of the binary.
  StrCmp "$R2" "1060" cpum_pre_done
  StrCmp "$R2" "1062" cpum_pre_done
  StrCmp "$R2" "1072" cpum_pre_done
  StrCmp "$R2" "0" 0 cpum_pre_elevate

  ; The stop was accepted; if the binary is already writable we are done.
  IfFileExists "$INSTDIR\cpum_service.exe" 0 cpum_pre_done
  ClearErrors
  FileOpen $R0 "$INSTDIR\cpum_service.exe" a
  IfErrors 0 cpum_pre_unlocked
  FileClose $R0

  ; Still locked, or the stop was refused (typically access denied): escalate.
  cpum_pre_elevate:
  StrCpy $R1 'sc.exe stop "${CPUM_SERVICE_NAME}" & ping -n 4 127.0.0.1 >nul'
  !insertmacro CpumRunElevatedCmd

  ; Wait until the old binary is writable again; a service that is still
  ; shutting down keeps it locked and would break the extraction below.
  IfFileExists "$INSTDIR\cpum_service.exe" 0 cpum_pre_done
  StrCpy $R2 0
  cpum_pre_wait:
    ClearErrors
    FileOpen $R0 "$INSTDIR\cpum_service.exe" a
    IfErrors 0 cpum_pre_unlocked
    FileClose $R0
    IntOp $R2 $R2 + 1
    IntCmp $R2 20 cpum_pre_done
    Sleep 500
    Goto cpum_pre_wait

  cpum_pre_unlocked:
  FileClose $R0

  cpum_pre_done:
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; ---------------------------------------------------------------------------
; After install / upgrade: migrate rule files into the per-user data directory
; and rebind an already installed service to the new layout.
; ---------------------------------------------------------------------------
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
  nsExec::ExecToStack '"$SYSDIR\sc.exe" query "${CPUM_SERVICE_NAME}"'
  Pop $R3
  Pop $R1
  StrCmp "$R3" "0" 0 cpum_post_done

  ; Tauri maps bundle resources next to the installed executable, but older
  ; builds shipped them in a resources subdirectory. Detect the actual
  ; location so upgrades keep working across layout changes.
  StrCpy $R3 "$INSTDIR\cpum_service.exe"
  IfFileExists "$R3" 0 cpum_svc_in_resources
    Goto cpum_svc_path_ready
  cpum_svc_in_resources:
    StrCpy $R3 "$INSTDIR\resources\cpum_service.exe"
  cpum_svc_path_ready:

  StrCpy $R1 'sc.exe config "${CPUM_SERVICE_NAME}" binPath= "\"$R3\" \"$R0\"" start= auto & sc.exe start "${CPUM_SERVICE_NAME}"'
  !insertmacro CpumRunElevatedCmd

  cpum_post_done:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
!macroend

; ---------------------------------------------------------------------------
; Before uninstall: stop and delete the optional service, otherwise it keeps
; pointing at a deleted cpum_service.exe and lingers forever. Skipped while
; updating, because the updater runs the previous uninstaller with /UPDATE.
; ---------------------------------------------------------------------------
!macro NSIS_HOOK_PREUNINSTALL
  Push $R0
  Push $R1
  Push $R2

  ; The updater only wants the files replaced; the service must survive it.
  StrCmp "$UpdateMode" "1" cpum_preun_done

  DetailPrint "CPUM: checking for ${CPUM_SERVICE_NAME}"
  nsExec::ExecToStack '"$SYSDIR\sc.exe" query "${CPUM_SERVICE_NAME}"'
  Pop $R2
  Pop $R1
  StrCmp "$R2" "0" 0 cpum_preun_done

  ; Stop without elevation first: it succeeds on setups where the user owns the
  ; service and keeps that path completely UAC-free.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop "${CPUM_SERVICE_NAME}"'
  Pop $R2

  ; One elevated pass for stop + kill + delete. `taskkill` is the safety net
  ; for a service that ignores the stop control: without it `sc delete` only
  ; marks the service and the process keeps holding the binary.
  StrCpy $R1 'sc.exe stop "${CPUM_SERVICE_NAME}" & ping -n 4 127.0.0.1 >nul & taskkill /F /IM cpum_service.exe >nul 2>&1 & sc.exe delete "${CPUM_SERVICE_NAME}"'
  !insertmacro CpumRunElevatedCmd

  ; Wait until the binary can actually be removed, so the Delete in the
  ; uninstall section does not silently fail and leave a half-removed folder.
  StrCpy $R2 0
  cpum_preun_wait:
    IfFileExists "$INSTDIR\cpum_service.exe" 0 cpum_preun_done
    Delete "$INSTDIR\cpum_service.exe"
    IfFileExists "$INSTDIR\cpum_service.exe" 0 cpum_preun_done
    IntOp $R2 $R2 + 1
    IntCmp $R2 20 cpum_preun_done
    Sleep 500
    Goto cpum_preun_wait

  cpum_preun_done:
  Pop $R2
  Pop $R1
  Pop $R0
!macroend
