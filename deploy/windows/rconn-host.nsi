; rconn host installer (NSIS). Built by scripts/package.sh:
;   makensis -DARCH=x64|arm64 -DVERSION=0.1.0 -DSRC=<staging dir> -DOUT=<setup.exe> rconn-host.nsi
;
; Default: the public Rconn server, no account. The computer gets its ID from the server at
; the service's first start; connecting to it then needs its access password.
; Interactive: the server URL, pin and identity folder can be changed (self-hosted fleets).
; Silent:
;   rconn-host-setup.exe /S                                   (public server, no account)
;   rconn-host-setup.exe /S /SERVER=wss://rconn.example.com:443/v1/ws /PIN=sha256:... /IDENTITY=C:\path\host1
; Upgrading without /SERVER keeps the server already configured.

Unicode true
!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "getarg.nsh"

!define DEFAULT_SERVER "wss://connect.rconn.app/v1/ws"

Name "Rconn host ${VERSION} (${ARCH})"
OutFile "${OUT}"
InstallDir "$PROGRAMFILES64\rconn"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
BrandingText "Rconn"

Var Server
Var Pin
Var Identity
Var ServerField
Var PinField
Var IdentityField

!define MUI_ABORTWARNING
!define MUI_ICON "rconn.ico"
!define MUI_UNICON "rconn.ico"
!insertmacro MUI_PAGE_WELCOME
Page custom ServerPage ServerPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES
!insertmacro MUI_LANGUAGE "English"

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "Rconn requires 64-bit Windows." /SD IDOK
    Abort
  ${EndIf}
  SetRegView 64
  ${GetParameters} $0
  ; GetArg, not GetOptions: GetOptions cuts values at the first "/" (wss://...).
  !insertmacro GetArg $0 "/SERVER=" $Server
  !insertmacro GetArg $0 "/PIN=" $Pin
  !insertmacro GetArg $0 "/IDENTITY=" $Identity
  ; New install: the public server. Upgrade: an empty field keeps the current one.
  ${If} $Server == ""
  ${AndIfNot} ${FileExists} "$COMMONPROGRAMDATA\rconn\host.toml"
    StrCpy $Server "${DEFAULT_SERVER}"
  ${EndIf}
FunctionEnd

Function ServerPage
  !insertmacro MUI_HEADER_TEXT "Rconn server" "Keep the defaults to use the public Rconn server without an account."
  nsDialogs::Create 1018
  Pop $0
  ${NSD_CreateLabel} 0 0 100% 12u "Server URL (empty: keep the current one):"
  ${NSD_CreateText} 0 13u 100% 12u "$Server"
  Pop $ServerField
  ${NSD_CreateLabel} 0 32u 100% 12u "Server pin (sha256:..., leave empty for a public certificate):"
  ${NSD_CreateText} 0 45u 100% 12u "$Pin"
  Pop $PinField
  ${NSD_CreateLabel} 0 64u 100% 12u "Self-hosted fleets only: host identity folder from enroll-host (else leave empty):"
  ${NSD_CreateDirRequest} 0 77u 80% 12u "$Identity"
  Pop $IdentityField
  ${NSD_CreateBrowseButton} 82% 77u 18% 12u "Browse..."
  Pop $0
  ${NSD_OnClick} $0 BrowseIdentity
  nsDialogs::Show
FunctionEnd

Function BrowseIdentity
  nsDialogs::SelectFolderDialog "Host identity folder" "$Identity"
  Pop $0
  ${If} $0 != error
    ${NSD_SetText} $IdentityField $0
  ${EndIf}
FunctionEnd

Function ServerPageLeave
  ${NSD_GetText} $ServerField $Server
  ${NSD_GetText} $PinField $Pin
  ${NSD_GetText} $IdentityField $Identity
  ${If} $Identity != ""
  ${AndIfNot} ${FileExists} "$Identity\host.id"
    MessageBox MB_ICONEXCLAMATION "No host.id in $Identity. Choose the folder made by enroll-host, or leave it empty."
    Abort
  ${EndIf}
FunctionEnd

Section "Rconn host" SecHost
  SetRegView 64
  ${If} $Identity != ""
  ${AndIf} $Server == ""
    DetailPrint "/IDENTITY= needs /SERVER= (the server that enrolled it)."
    SetErrorLevel 2
    Abort
  ${EndIf}

  ; Upgrade in place: remove a running service and close the app first.
  ${If} ${FileExists} "$INSTDIR\rconn-host-service.exe"
    nsExec::ExecToLog '"$INSTDIR\rconn-host-service.exe" uninstall'
    Pop $0
  ${EndIf}
  nsExec::ExecToLog 'taskkill /IM rconn.exe /F'
  Pop $0

  SetOutPath "$INSTDIR"
  File "${SRC}\host\rconn-host-service.exe"
  File "${SRC}\host\rconn-host-agent.exe"
  File "${SRC}\host\openh264-*.dll"
  File "${SRC}\viewer\rconn-viewer.exe"
  File "${SRC}\viewer\rconn.exe"
  File "${SRC}\host\rconn-settings.exe"
  File "rconn.ico"

  ; Empty server on an upgrade: keep host.toml, only re-apply the folder's permissions.
  ${If} $Server == ""
    StrCpy $1 '"$INSTDIR\rconn-host-service.exe" configure --keep-server'
  ${Else}
    StrCpy $1 '"$INSTDIR\rconn-host-service.exe" configure --server "$Server"'
    ${If} $Pin != ""
      StrCpy $1 '$1 --fingerprint "$Pin"'
    ${EndIf}
  ${EndIf}
  ${If} $Identity != ""
    StrCpy $1 '$1 --identity-from "$Identity"'
  ${EndIf}
  nsExec::ExecToLog $1
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "Configuring the host failed (code $0). See the details above." /SD IDOK
    SetErrorLevel 3
    Abort
  ${EndIf}

  nsExec::ExecToLog '"$INSTDIR\rconn-host-service.exe" install'
  Pop $0
  ${If} $0 != 0
    MessageBox MB_ICONSTOP "Installing the service failed (code $0)." /SD IDOK
    SetErrorLevel 4
    Abort
  ${EndIf}

  ; Start Menu + desktop for everyone; the tray icon starts at every login.
  SetShellVarContext all
  ; Older installs named these in lowercase; Windows keeps a file's old case, so remove first.
  Delete "$SMPROGRAMS\rconn.lnk"
  Delete "$SMPROGRAMS\rconn host settings.lnk"
  Delete "$DESKTOP\rconn.lnk"
  CreateShortcut "$SMPROGRAMS\Rconn.lnk" "$INSTDIR\rconn.exe" "" "$INSTDIR\rconn.ico"
  CreateShortcut "$SMPROGRAMS\Rconn host settings.lnk" "$INSTDIR\rconn-settings.exe" "" "$INSTDIR\rconn.ico"
  CreateShortcut "$DESKTOP\Rconn.lnk" "$INSTDIR\rconn.exe" "" "$INSTDIR\rconn.ico"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "rconn" '"$INSTDIR\rconn.exe" --tray'

  WriteUninstaller "$INSTDIR\uninstall.exe"
  !define UNINST "Software\Microsoft\Windows\CurrentVersion\Uninstall\rconn-host"
  WriteRegStr HKLM "${UNINST}" "DisplayName" "Rconn host"
  WriteRegStr HKLM "${UNINST}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "${UNINST}" "Publisher" "Rconn"
  WriteRegStr HKLM "${UNINST}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "${UNINST}" "DisplayIcon" "$INSTDIR\rconn.ico"
  WriteRegStr HKLM "${UNINST}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "${UNINST}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegDWORD HKLM "${UNINST}" "NoModify" 1
  WriteRegDWORD HKLM "${UNINST}" "NoRepair" 1
  DetailPrint "Rconn host installed and running (service: rconn-host)."
  ; Open the app as the signed-in user (not elevated): explorer starts it unelevated.
  ${IfNot} ${Silent}
    Exec '"$WINDIR\explorer.exe" "$INSTDIR\rconn.exe"'
  ${EndIf}
SectionEnd

Section "Uninstall"
  SetRegView 64
  nsExec::ExecToLog '"$INSTDIR\rconn-host-service.exe" uninstall'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM rconn-host-agent.exe /F'
  Pop $0
  nsExec::ExecToLog 'taskkill /IM rconn.exe /F'
  Pop $0
  SetShellVarContext all
  Delete "$SMPROGRAMS\Rconn.lnk"
  Delete "$SMPROGRAMS\Rconn host settings.lnk"
  Delete "$DESKTOP\Rconn.lnk"
  DeleteRegValue HKLM "Software\Microsoft\Windows\CurrentVersion\Run" "rconn"
  Delete "$INSTDIR\rconn.exe"
  Delete "$INSTDIR\rconn-settings.exe"
  Delete "$INSTDIR\rconn.ico"
  Delete "$INSTDIR\rconn-host-service.exe"
  Delete "$INSTDIR\rconn-host-agent.exe"
  Delete "$INSTDIR\openh264-*.dll"
  Delete "$INSTDIR\rconn-viewer.exe"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"
  ; Settings, host identity and logs (admin-only folder).
  RMDir /r "$COMMONPROGRAMDATA\rconn"
  RMDir /r "$COMMONPROGRAMDATA\rconn-status"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\rconn-host"
SectionEnd
