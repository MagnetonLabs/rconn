; Rconn "just for me" installer (NSIS): no administrator needed. Built by scripts/package.sh:
;   makensis -DARCH=x64|arm64 -DVERSION=0.1.0 -DSRC=<staging dir> -DOUT=<setup.exe> rconn-user.nsi
;
; Installs into %LOCALAPPDATA%\Programs\Rconn for the signed-in user only. There is no
; service: the app gets this computer an ID and runs the host while this user is signed in
; (`rconn --start-host` from the user's Run key). The sign-in screen and Windows' admin
; prompts (UAC) can't be seen or used remotely; for those, use the installer for everyone
; (rconn-host-setup-*.exe, needs an administrator).
; Silent: rconn-setup-user.exe /S

Unicode true
!include "MUI2.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"

Name "Rconn ${VERSION} (${ARCH}, just for me)"
OutFile "${OUT}"
InstallDir "$LOCALAPPDATA\Programs\Rconn"
RequestExecutionLevel user
SetCompressor /SOLID lzma
BrandingText "Rconn"

!define MUI_ABORTWARNING
!define MUI_ICON "rconn.ico"
!define MUI_UNICON "rconn.ico"
!define MUI_WELCOMEPAGE_TITLE "Rconn, just for you"
!define MUI_WELCOMEPAGE_TEXT "This installs Rconn for you only and needs no administrator.$\r$\n$\r$\nOther people can connect to this computer while you're signed in. The Windows sign-in screen and administrator prompts can't be used remotely; for those, use the Rconn installer for everyone instead.$\r$\n$\r$\nClick Next to continue."
!define MUI_FINISHPAGE_RUN "$INSTDIR\rconn.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Open Rconn"
!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

!define UNINST "Software\Microsoft\Windows\CurrentVersion\Uninstall\rconn-user"
!define RUN "Software\Microsoft\Windows\CurrentVersion\Run"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Rconn requires 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  ; Installed for everyone already: that one hosts this computer (with its own ID).
  ReadRegStr $0 HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\rconn-host" "InstallLocation"
  ${If} $0 != ""
    MessageBox MB_ICONINFORMATION "Rconn is already installed for everyone on this computer. Open it from the Start menu." /SD IDOK
    SetErrorLevel 5
    Abort
  ${EndIf}
FunctionEnd

Section "Rconn" SecApp
  SetShellVarContext current
  ; Upgrade in place: close this user's Rconn first (the host restarts below).
  nsExec::ExecToLog 'taskkill /IM rconn.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM rconn-host-agent.exe /F'
  Pop $0
  Sleep 500

  SetOutPath "$INSTDIR"
  File "${SRC}\viewer\rconn.exe"
  File "${SRC}\viewer\rconn-viewer.exe"
  File "${SRC}\host\rconn-host-agent.exe"
  File "${SRC}\host\openh264-*.dll"
  File "rconn.ico"

  Delete "$SMPROGRAMS\Rconn.lnk"
  CreateShortcut "$SMPROGRAMS\Rconn.lnk" "$INSTDIR\rconn.exe" "" "$INSTDIR\rconn.ico"
  CreateShortcut "$DESKTOP\Rconn.lnk" "$INSTDIR\rconn.exe" "" "$INSTDIR\rconn.ico"
  WriteRegStr HKCU "${RUN}" "rconn" '"$INSTDIR\rconn.exe" --tray'
  ; Upgrade: the host was on; start it again (the app turns it on at first launch).
  ReadRegStr $0 HKCU "${RUN}" "rconn-host"
  ${If} $0 != ""
    WriteRegStr HKCU "${RUN}" "rconn-host" '"$INSTDIR\rconn.exe" --start-host'
    Exec '"$INSTDIR\rconn.exe" --start-host'
  ${EndIf}

  WriteUninstaller "$INSTDIR\uninstall.exe"
  WriteRegStr HKCU "${UNINST}" "DisplayName" "Rconn (just for me)"
  WriteRegStr HKCU "${UNINST}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${UNINST}" "Publisher" "Rconn"
  WriteRegStr HKCU "${UNINST}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST}" "DisplayIcon" "$INSTDIR\rconn.ico"
  WriteRegStr HKCU "${UNINST}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKCU "${UNINST}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST}" "NoRepair" 1
  ; Silent installs open the app too: its first launch gets this computer an ID.
  ${If} ${Silent}
    Exec '"$INSTDIR\rconn.exe"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetShellVarContext current
  DeleteRegValue HKCU "${RUN}" "rconn"
  DeleteRegValue HKCU "${RUN}" "rconn-host"
  ; The host's supervisor stops once its command line is gone.
  Delete "$LOCALAPPDATA\rconn\data\host\autostart.json"
  nsExec::ExecToLog 'taskkill /IM rconn-host-agent.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM rconn.exe /F'
  Pop $0
  Sleep 1500
  Delete "$SMPROGRAMS\Rconn.lnk"
  Delete "$DESKTOP\Rconn.lnk"
  Delete "$INSTDIR\rconn.exe"
  Delete "$INSTDIR\rconn-viewer.exe"
  Delete "$INSTDIR\rconn-host-agent.exe"
  Delete "$INSTDIR\openh264-*.dll"
  Delete "$INSTDIR\rconn.ico"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; This computer's ID and settings (access password) for this user.
  RMDir /r "$LOCALAPPDATA\rconn\data\host"
  DeleteRegKey HKCU "${UNINST}"
SectionEnd
