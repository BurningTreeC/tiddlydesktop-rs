; TiddlyDesktop NSIS Installer Template
; Based on Tauri's default template with Install/Portable mode selection

!include "MUI2.nsh"
!include "FileFunc.nsh"
!include "x64.nsh"
!include "WordFunc.nsh"
!include "nsDialogs.nsh"

; Tauri will replace these variables at build time
!define PRODUCTNAME "{{product_name}}"
!define VERSION "{{version}}"
!define VERSIONWITHBUILD "{{version_with_build}}"
!define SHORTDESCRIPTION "{{short_description}}"
!define INSTALLERICON "{{installer_icon}}"
!define SIDEBARIMAGE "{{sidebar_image}}"
!define HEADERIMAGE "{{header_image}}"

; Set the installer and uninstaller icons
!define MUI_ICON "${INSTALLERICON}"
!define MUI_UNICON "${INSTALLERICON}"
!define MAINBINARYNAME "{{main_binary_name}}"
!define MAINBINARYSRCPATH "{{main_binary_path}}"
!define BUNDLEID "{{bundle_id}}"
!define OUTFILE "{{out_file}}"
!define ARCH "{{arch}}"
!define ALLOWDOWNGRADES "{{allow_downgrades}}"
!define DISPLAYLANGUAGESELECTOR "{{display_language_selector}}"
!define INSTALLMODE "{{install_mode}}"
!define LICENSEFILEPATH "{{license_file_path}}"
!define INSTALLWEBVIEW2MODE "{{install_webview2_mode}}"
!define WEBVIEW2INSTALLERARGS "{{webview2_installer_args}}"
!define WEBVIEW2BOOTSTRAPPERPATH "{{webview2_bootstrapper_path}}"
!define WEBVIEW2INSTALLERPATH "{{webview2_installer_path}}"
!define MINIMUMWEBVIEW2VERSION "{{minimum_webview2_version}}"

Unicode true
Name "${PRODUCTNAME}"
BrandingText "TiddlyDesktop"
OutFile "${OUTFILE}"

; Default to current user install (will change if user selects portable or all users)
RequestExecutionLevel user
InstallDir "$LOCALAPPDATA\${PRODUCTNAME}"

!insertmacro MUI_PAGE_WELCOME

; Custom page for Install Mode selection
Page custom InstallModePage InstallModePageLeave

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; Variables for mode selection
Var InstallModeChoice  ; "install" or "portable"
Var Dialog
Var RadioInstall
Var RadioPortable
Var LabelDescription

; Install Mode Selection Page
Function InstallModePage
    nsDialogs::Create 1018
    Pop $Dialog

    ${If} $Dialog == error
        Abort
    ${EndIf}

    ; Title and description
    ${NSD_CreateLabel} 0 0 100% 24u "Choose how you want to use TiddlyDesktop:"
    Pop $0

    ; Install mode radio button
    ${NSD_CreateRadioButton} 20u 35u 100% 12u "Install (recommended)"
    Pop $RadioInstall
    ${NSD_SetState} $RadioInstall ${BST_CHECKED}
    ${NSD_OnClick} $RadioInstall UpdateModeDescription

    ; Portable mode radio button
    ${NSD_CreateRadioButton} 20u 50u 100% 12u "Portable (no installation, run from any folder)"
    Pop $RadioPortable
    ${NSD_OnClick} $RadioPortable UpdateModeDescription

    ; Description label
    ${NSD_CreateLabel} 20u 75u 90% 40u "Install mode: TiddlyDesktop will be installed to your user folder with Start Menu shortcuts. Your wiki list will be stored in your user data folder."
    Pop $LabelDescription

    ; Initialize with Install mode selected
    StrCpy $InstallModeChoice "install"

    nsDialogs::Show
FunctionEnd

Function UpdateModeDescription
    ${NSD_GetState} $RadioInstall $0
    ${If} $0 == ${BST_CHECKED}
        ${NSD_SetText} $LabelDescription "Install mode: TiddlyDesktop will be installed to your user folder with Start Menu shortcuts. Your wiki list will be stored in your user data folder."
    ${Else}
        ${NSD_SetText} $LabelDescription "Portable mode: TiddlyDesktop will be extracted to a folder of your choice. You can move this folder anywhere and it will carry your wiki list with it. No system changes are made."
    ${EndIf}
FunctionEnd

Function InstallModePageLeave
    ${NSD_GetState} $RadioInstall $0
    ${If} $0 == ${BST_CHECKED}
        StrCpy $InstallModeChoice "install"
        ; Set normal install directory
        StrCpy $INSTDIR "$LOCALAPPDATA\${PRODUCTNAME}"
    ${Else}
        StrCpy $InstallModeChoice "portable"
        ; Default portable to Downloads folder (user can still change on next page)
        StrCpy $INSTDIR "$PROFILE\Downloads\${PRODUCTNAME}"
    ${EndIf}
FunctionEnd

Section "Main Application" SecMain
    SetOutPath "$INSTDIR"

    ; Clean old resources so stale files from a previous version don't linger
    RMDir /r "$INSTDIR\resources"

    ; Install main binary
    File "${MAINBINARYSRCPATH}"

    ; Create resource directories
    {{#each resources_dirs}}
    CreateDirectory "$INSTDIR\\{{this}}"
    {{/each}}

    ; Install resource files
    {{#each resources}}
    File /a "/oname={{this.[1]}}" "{{no-escape @key}}"
    {{/each}}

    ; Install additional binaries
    {{#each binaries}}
    File /a "/oname={{this}}" "{{no-escape @key}}"
    {{/each}}

    ; Create portable marker file if in portable mode
    ${If} $InstallModeChoice == "portable"
        FileOpen $0 "$INSTDIR\portable" w
        FileClose $0
    ${EndIf}

    ; Only create shortcuts and registry entries for install mode
    ${If} $InstallModeChoice == "install"
        ; Create Start Menu shortcuts
        CreateDirectory "$SMPROGRAMS\${PRODUCTNAME}"
        CreateShortCut "$SMPROGRAMS\${PRODUCTNAME}\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"
        CreateShortCut "$SMPROGRAMS\${PRODUCTNAME}\Uninstall.lnk" "$INSTDIR\uninstall.exe"

        ; Create Desktop shortcut
        CreateShortCut "$DESKTOP\${PRODUCTNAME}.lnk" "$INSTDIR\${MAINBINARYNAME}.exe"

        ; Write registry entries
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "DisplayName" "${PRODUCTNAME}"
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "DisplayIcon" "$INSTDIR\${MAINBINARYNAME}.exe"
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "DisplayVersion" "${VERSION}"
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "Publisher" "TiddlyDesktop"
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "InstallLocation" "$INSTDIR"
        WriteRegStr SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "UninstallString" "$INSTDIR\uninstall.exe"
        WriteRegDWORD SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "NoModify" 1
        WriteRegDWORD SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "NoRepair" 1

        ; Calculate and write install size
        ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
        IntFmt $0 "0x%08X" $0
        WriteRegDWORD SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "EstimatedSize" "$0"

        ; Register file associations for HTML files
        ; Create ProgID for TiddlyDesktop HTML files
        WriteRegStr SHCTX "Software\Classes\${PRODUCTNAME}.html" "" "TiddlyWiki HTML File"
        WriteRegStr SHCTX "Software\Classes\${PRODUCTNAME}.html\DefaultIcon" "" "$INSTDIR\${MAINBINARYNAME}.exe,0"
        WriteRegStr SHCTX "Software\Classes\${PRODUCTNAME}.html\shell\open\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'

        ; Associate .html and .htm extensions with our ProgID
        WriteRegStr SHCTX "Software\Classes\.html\OpenWithProgids" "${PRODUCTNAME}.html" ""
        WriteRegStr SHCTX "Software\Classes\.htm\OpenWithProgids" "${PRODUCTNAME}.html" ""

        ; Register in Applications list
        WriteRegStr SHCTX "Software\Classes\Applications\${MAINBINARYNAME}.exe" "FriendlyAppName" "${PRODUCTNAME}"
        WriteRegStr SHCTX "Software\Classes\Applications\${MAINBINARYNAME}.exe\shell\open\command" "" '"$INSTDIR\${MAINBINARYNAME}.exe" "%1"'
        WriteRegStr SHCTX "Software\Classes\Applications\${MAINBINARYNAME}.exe\SupportedTypes" ".html" ""
        WriteRegStr SHCTX "Software\Classes\Applications\${MAINBINARYNAME}.exe\SupportedTypes" ".htm" ""

        ; Create uninstaller
        WriteUninstaller "$INSTDIR\uninstall.exe"
    ${EndIf}

SectionEnd

; Uninstaller section (only used for install mode)
Section "Uninstall"
    ; Remove shortcuts
    Delete "$SMPROGRAMS\${PRODUCTNAME}\${PRODUCTNAME}.lnk"
    Delete "$SMPROGRAMS\${PRODUCTNAME}\Uninstall.lnk"
    RMDir "$SMPROGRAMS\${PRODUCTNAME}"
    Delete "$DESKTOP\${PRODUCTNAME}.lnk"

    ; Remove files
    RMDir /r "$INSTDIR"

    ; Remove registry entries
    DeleteRegKey SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}"

    ; Remove file association registry entries
    DeleteRegKey SHCTX "Software\Classes\${PRODUCTNAME}.html"
    DeleteRegValue SHCTX "Software\Classes\.html\OpenWithProgids" "${PRODUCTNAME}.html"
    DeleteRegValue SHCTX "Software\Classes\.htm\OpenWithProgids" "${PRODUCTNAME}.html"
    DeleteRegKey SHCTX "Software\Classes\Applications\${MAINBINARYNAME}.exe"
SectionEnd

; WebView2 installation (Tauri standard)
Function InstallWebView2
    {{#if install_webview2_mode}}
    ; Check if WebView2 is already installed
    ReadRegStr $0 HKLM "SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    ${If} $0 == ""
        ReadRegStr $0 HKLM "SOFTWARE\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}" "pv"
    ${EndIf}

    ${If} $0 == ""
        ; WebView2 not installed, install it
        {{#if webview2_bootstrapper_path}}
        SetOutPath "$TEMP"
        File "${WEBVIEW2BOOTSTRAPPERPATH}"
        ExecWait '"$TEMP\MicrosoftEdgeWebview2Setup.exe" ${WEBVIEW2INSTALLERARGS}'
        Delete "$TEMP\MicrosoftEdgeWebview2Setup.exe"
        {{/if}}
    ${EndIf}
    {{/if}}
FunctionEnd

Function .onInit
    ; Kill any running instance so files aren't locked during upgrade
    nsExec::Exec 'taskkill /f /im "${MAINBINARYNAME}.exe"'
    Sleep 1000

    ; Detect previous install location from registry so upgrades go to the same directory
    ReadRegStr $R0 SHCTX "Software\Microsoft\Windows\CurrentVersion\Uninstall\${PRODUCTNAME}" "InstallLocation"
    ${If} $R0 != ""
        StrCpy $INSTDIR $R0
    ${EndIf}

    Call InstallWebView2
FunctionEnd
