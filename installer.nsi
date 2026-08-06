!define APP_NAME "SENPAI"
!define APP_VERSION "0.1.0"
!define INSTALL_DIR "$PROGRAMFILES64\SENPAI"

Name "${APP_NAME}"
OutFile "SenpaiSetup.exe"
InstallDir "${INSTALL_DIR}"
RequestExecutionLevel admin

Page directory
Page instfiles

Section "Install"
    SetOutPath "$INSTDIR"

    File "senpai-release-staging\senpai.exe"
    File "senpai-release-staging\senpai.ico"
    File "senpai-release-staging\*.dll"

    CreateDirectory "$SMPROGRAMS\${APP_NAME}"
    CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\senpai.exe" "" "$INSTDIR\senpai.ico"
    CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\senpai.exe" "" "$INSTDIR\senpai.ico"

    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\senpai.exe"
    Delete "$INSTDIR\senpai.ico"
    Delete "$INSTDIR\*.dll"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
    Delete "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk"
    RMDir "$SMPROGRAMS\${APP_NAME}"
    Delete "$DESKTOP\${APP_NAME}.lnk"
SectionEnd
