Unicode true
!include "LogicLib.nsh"
!include "WinVer.nsh"
!include "WinMessages.nsh"
!include "StrFunc.nsh"
${Using:StrFunc} StrRep
${Using:StrFunc} UnStrRep
!include "x64.nsh"

!ifndef VERSION
  !define VERSION "0.1.0"
!endif
!ifndef SOURCE_EXE
  !error "SOURCE_EXE must point to wmcterm.exe"
!endif
!ifndef SOURCE_GUI_EXE
  !error "SOURCE_GUI_EXE must point to wmcterm-gui.exe"
!endif
!ifndef OUTPUT_FILE
  !define OUTPUT_FILE "wmcterm-${VERSION}-windows-x86_64-setup.exe"
!endif
!ifndef PRODUCT_ICON
  !define PRODUCT_ICON "assets/icons/icon.ico"
!endif

Name "wmcterm"
Caption "wmcterm ${VERSION} Setup"
OutFile "${OUTPUT_FILE}"
InstallDir "$PROGRAMFILES64\wmcterm"
InstallDirRegKey HKLM "Software\wmcterm" "InstallLocation"
RequestExecutionLevel admin
SetCompressor /SOLID lzma
Icon "${PRODUCT_ICON}"
UninstallIcon "${PRODUCT_ICON}"
BrandingText "wmcterm"
VIProductVersion "0.1.0.0"
VIAddVersionKey /LANG=1033 "ProductName" "wmcterm"
VIAddVersionKey /LANG=1033 "FileDescription" "wmcterm installer"
VIAddVersionKey /LANG=1033 "FileVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "ProductVersion" "${VERSION}"
VIAddVersionKey /LANG=1033 "LegalCopyright" "Copyright 2026 wmcterm contributors"

Page directory
Page instfiles
UninstPage uninstConfirm
UninstPage instfiles

Function .onInit
  ${IfNot} ${RunningX64}
    MessageBox MB_ICONSTOP "wmcterm requires 64-bit Windows."
    Abort
  ${EndIf}
  SetRegView 64
  ${IfNot} ${AtLeastWin10}
    MessageBox MB_ICONSTOP "wmcterm requires Windows 10 version 1809 (build 17763) or later."
    Abort
  ${EndIf}
  ReadRegStr $0 HKLM "Software\Microsoft\Windows NT\CurrentVersion" "CurrentBuildNumber"
  IntCmp $0 17763 supported_windows unsupported_windows supported_windows
  unsupported_windows:
    MessageBox MB_ICONSTOP "wmcterm requires Windows 10 version 1809 (build 17763) or later."
    Abort
  supported_windows:
FunctionEnd

Function un.onInit
  SetRegView 64
FunctionEnd

Section "wmcterm" SEC_MAIN
  SetShellVarContext all
  SetOutPath "$INSTDIR"
  File /oname=wmcterm.exe "${SOURCE_EXE}"
  File /oname=wmcterm-gui.exe "${SOURCE_GUI_EXE}"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  ; Keep GUI launches out of the protected installation directory so agents
  ; can initialize shell setup files in the workspace.
  SetOutPath "$PROFILE"
  CreateDirectory "$SMPROGRAMS\wmcterm"
  CreateShortcut "$SMPROGRAMS\wmcterm\wmcterm.lnk" "$INSTDIR\wmcterm-gui.exe" "" "$INSTDIR\wmcterm.exe" 0
  CreateShortcut "$SMPROGRAMS\wmcterm\Uninstall wmcterm.lnk" "$INSTDIR\Uninstall.exe"
  CreateShortcut "$DESKTOP\wmcterm.lnk" "$INSTDIR\wmcterm-gui.exe" "" "$INSTDIR\wmcterm.exe" 0

  WriteRegStr HKLM "Software\wmcterm" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\App Paths\wmcterm.exe" "" "$INSTDIR\wmcterm.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\App Paths\wmcterm.exe" "Path" "$INSTDIR"
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path"
  ${StrRep} $1 "$0" ";$INSTDIR" ""
  ${StrRep} $1 "$1" "$INSTDIR;" ""
  ${If} $1 == "$INSTDIR"
    StrCpy $1 ""
  ${EndIf}
  ${If} $1 == ""
    StrCpy $1 "$INSTDIR"
  ${Else}
    StrCpy $1 "$1;$INSTDIR"
  ${EndIf}
  WriteRegExpandStr HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path" "$1"
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "DisplayName" "wmcterm"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "DisplayIcon" "$INSTDIR\wmcterm.exe"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "InstallLocation" "$INSTDIR"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm" "NoRepair" 1
SectionEnd

Section "Uninstall"
  SetShellVarContext all
  Delete "$DESKTOP\wmcterm.lnk"
  Delete "$SMPROGRAMS\wmcterm\wmcterm.lnk"
  Delete "$SMPROGRAMS\wmcterm\Uninstall wmcterm.lnk"
  RMDir "$SMPROGRAMS\wmcterm"
  Delete "$INSTDIR\wmcterm.exe"
  Delete "$INSTDIR\wmcterm-gui.exe"
  Delete "$INSTDIR\Uninstall.exe"
  RMDir "$INSTDIR"
  ReadRegStr $0 HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path"
  ${UnStrRep} $1 "$0" ";$INSTDIR" ""
  ${UnStrRep} $1 "$1" "$INSTDIR;" ""
  ${If} $1 == "$INSTDIR"
    StrCpy $1 ""
  ${EndIf}
  WriteRegExpandStr HKLM "SYSTEM\CurrentControlSet\Control\Session Manager\Environment" "Path" "$1"
  SendMessage ${HWND_BROADCAST} ${WM_SETTINGCHANGE} 0 "STR:Environment" /TIMEOUT=5000
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\App Paths\wmcterm.exe"
  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\wmcterm"
  DeleteRegKey HKLM "Software\wmcterm"
SectionEnd
