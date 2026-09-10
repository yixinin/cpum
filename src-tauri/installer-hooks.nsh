!macro NSIS_HOOK_PREINSTALL
  ; The service executable may be locked during an in-place upgrade.
  ; The per-machine installer is elevated, so sc.exe can stop it without a second UAC prompt.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop "CpumAffinityService"'
  Sleep 2000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Rules must be accessible to the LocalSystem service. $COMMONAPPDATA is not
  ; a valid NSIS constant (it stays literal and silently creates a junk folder
  ; inside $INSTDIR), so read the machine-level ProgramData directory from the
  ; environment, mirroring machine_rules_dir() in lib.rs.
  Push $R0
  Push $R1
  ReadEnvStr $R0 "ProgramData"
  StrCmp "$R0" "" 0 +2
    StrCpy $R0 "C:\ProgramData"
  StrCpy $R0 "$R0\cpum"
  CreateDirectory "$R0"

  ; Preserve existing rules when upgrading from an older release.
  IfFileExists "$R0\affinity_rules.json" cpum_rules_ready
  IfFileExists "$APPDATA\com.eason.cpum\affinity_rules.json" 0 cpum_check_legacy_rules
    CopyFiles /SILENT "$APPDATA\com.eason.cpum\affinity_rules.json" "$R0\affinity_rules.json"
    Goto cpum_rules_ready
  cpum_check_legacy_rules:
  IfFileExists "$APPDATA\cpum\affinity_rules.json" 0 cpum_rules_ready
    CopyFiles /SILENT "$APPDATA\cpum\affinity_rules.json" "$R0\affinity_rules.json"

  cpum_rules_ready:
  ; Tauri maps bundle resources next to the installed executable, but older
  ; builds shipped them in a resources subdirectory. Detect the actual location
  ; so upgrades keep working across layout changes.
  IfFileExists "$INSTDIR\cpum_service.exe" 0 cpum_svc_in_resources
    StrCpy $R1 "$INSTDIR\cpum_service.exe"
    Goto cpum_svc_path_ready
  cpum_svc_in_resources:
    StrCpy $R1 "$INSTDIR\resources\cpum_service.exe"
  cpum_svc_path_ready:

  ; Refresh an already installed service's executable and rules directory.
  ; sc.exe simply returns an error here when the optional service is absent.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" config "CpumAffinityService" binPath= "\"$R1\" \"$R0\"" start= auto'

  ; Ignore the result when the user has not installed the optional service yet.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" start "CpumAffinityService"'

  Pop $R1
  Pop $R0
!macroend
