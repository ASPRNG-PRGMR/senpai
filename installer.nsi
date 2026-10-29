!define APP_NAME "SENPAI"
!define APP_VERSION "0.2.2"
!define INSTALL_DIR "$PROGRAMFILES64\SENPAI"

# Source of the files to install. Defaults to the staging directory that
# package-windows.sh produces, so the installer and the zip ship exactly
# the same bundle -- in particular the same DLL set, derived from the
# exe's PE import table rather than maintained by hand in two places.
#
# Build the staging dir first:
#   ./package-windows.sh
#   makensis installer.nsi
#
# Override with: makensis -DSTAGE=some/other/dir installer.nsi
!ifndef STAGE
    !define STAGE "target/package/senpai-${APP_VERSION}-windows-x64"
!endif

Name "${APP_NAME}"
OutFile "SenpaiSetup.exe"
InstallDir "${INSTALL_DIR}"
RequestExecutionLevel admin

Page directory
Page instfiles

Section "Install"
    SetOutPath "$INSTDIR"

    File "${STAGE}/senpai.exe"
    File "${STAGE}/senpai.ico"
    File "${STAGE}/README.txt"
    File "${STAGE}/*.dll"

    CreateDirectory "$SMPROGRAMS\${APP_NAME}"
    CreateShortcut "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk" "$INSTDIR\senpai.exe" "" "$INSTDIR\senpai.ico"
    CreateShortcut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\senpai.exe" "" "$INSTDIR\senpai.ico"

    WriteUninstaller "$INSTDIR\Uninstall.exe"
    CreateShortcut "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk" "$INSTDIR\Uninstall.exe"
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\senpai.exe"
    Delete "$INSTDIR\senpai.ico"
    Delete "$INSTDIR\README.txt"
    Delete "$INSTDIR\*.dll"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\${APP_NAME}\${APP_NAME}.lnk"
    Delete "$SMPROGRAMS\${APP_NAME}\Uninstall.lnk"
    RMDir "$SMPROGRAMS\${APP_NAME}"
    Delete "$DESKTOP\${APP_NAME}.lnk"
SectionEnd
