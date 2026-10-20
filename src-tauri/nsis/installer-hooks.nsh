; Parla's NSIS installer hooks (PARLA-102), listed as `installerHooks` in
; tauri.release.conf.json.
;
; "Start Parla with Windows" is one value, HKCU\...\Run\Parla, that starts the
; exe that wrote it. Tauri's uninstaller deletes that value on every real
; uninstall (not on an update), whichever exe it starts. If it starts another
; copy (a portable one, a dev build), removing the installed copy must not
; switch that off. So: remember a foreign value before the uninstall, put it
; back after. A value that starts the exe being uninstalled stays deleted.

!define PARLA_RUN_KEY "Software\Microsoft\Windows\CurrentVersion\Run"

Var ParlaForeignRun

!macro NSIS_HOOK_PREUNINSTALL
  StrCpy $ParlaForeignRun ""
  ${If} $UpdateMode <> 1
    ReadRegStr $0 HKCU "${PARLA_RUN_KEY}" "${PRODUCTNAME}"
    ; Parla writes the quoted path; accept it unquoted too. Compared without
    ; regard to case, like Windows paths.
    ${If} $0 != ""
    ${AndIf} $0 != "$\"$INSTDIR\${MAINBINARYNAME}.exe$\""
    ${AndIf} $0 != "$INSTDIR\${MAINBINARYNAME}.exe"
      StrCpy $ParlaForeignRun $0
    ${EndIf}
  ${EndIf}
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  ${If} $ParlaForeignRun != ""
    WriteRegStr HKCU "${PARLA_RUN_KEY}" "${PRODUCTNAME}" $ParlaForeignRun
  ${EndIf}
!macroend
