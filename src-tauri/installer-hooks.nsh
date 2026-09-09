!macro NSIS_HOOK_PREINSTALL
  ; The service executable may be locked during an in-place upgrade.
  ; The per-machine installer is elevated, so sc.exe can stop it without a second UAC prompt.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" stop "CpumAffinityService"'
  Sleep 2000
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ; Rules must be accessible to the LocalSystem service. Preserve existing
  ; per-user rules when upgrading from an older release.
  CreateDirectory "$COMMONAPPDATA\cpum"
  IfFileExists "$COMMONAPPDATA\cpum\affinity_rules.json" rules_ready
  IfFileExists "$APPDATA\com.eason.cpum\affinity_rules.json" 0 +3
    CopyFiles /SILENT "$APPDATA\com.eason.cpum\affinity_rules.json" "$COMMONAPPDATA\cpum\affinity_rules.json"
    Goto rules_ready
  IfFileExists "$APPDATA\cpum\affinity_rules.json" 0 rules_ready
    CopyFiles /SILENT "$APPDATA\cpum\affinity_rules.json" "$COMMONAPPDATA\cpum\affinity_rules.json"

  rules_ready:
  ; Refresh an already installed service's executable and rules directory.
  ; sc.exe simply returns an error here when the optional service is absent.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" config "CpumAffinityService" binPath= "\"$INSTDIR\resources\cpum_service.exe\" \"$COMMONAPPDATA\cpum\"" start= auto'

  ; Ignore the result when the user has not installed the optional service yet.
  nsExec::ExecToLog '"$SYSDIR\sc.exe" start "CpumAffinityService"'
!macroend
