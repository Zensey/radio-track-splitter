; Radio Track Splitter installer (NSIS 3, Unicode).
;
; Installs the two release binaries per-user (no admin rights needed), then
; downloads what is too big to ship inside the installer, next to the .exe files:
;   - FFmpeg (ffmpeg/ffprobe/ffplay). Windows searches the application's own
;     directory first, so the app finds them without PATH edits.
;   - The VGGish weights. The app checks its own directory first (see
;     ensure_weights in src/vggish.rs).
; Both downloads are pinned to a SHA-256. Build with installer\build.ps1.

Unicode true
SetCompressor /SOLID lzma
RequestExecutionLevel user
ManifestDPIAware true

!ifndef VERSION
  !define VERSION "0.0.0"
!endif

!define NAME      "Radio Track Splitter"
!define REGKEY    "Software\Microsoft\Windows\CurrentVersion\Uninstall\RadioTrackSplitter"
!define APPKEY    "Software\RadioTrackSplitter"
!define GUI_EXE   "radio-track-splitter.exe"
!define CLI_EXE   "radio-track-splitter-cli.exe"

; --- downloads (change URL and hash together) ---
!define FFMPEG_URL    "https://github.com/GyanD/codexffmpeg/releases/download/9.0.2/ffmpeg-9.0.2-essentials_build.zip"
!define FFMPEG_SHA256 "60f467265b1e312373dbcd92200c2618a74850f98d3d078e94296bb3fa2047ba"
!define WEIGHTS_NAME  "vggish-10086976.pth"
!define WEIGHTS_URL   "https://github.com/harritaylor/torchvggish/releases/download/v0.1/${WEIGHTS_NAME}"
!define WEIGHTS_SHA256 "10086976245803799d9194e9a73d9b6c1549c71d1b80106f5cade5608a561f4b"

Name "${NAME}"
!ifndef OUTFILE
  !define OUTFILE "..\dist\RadioTrackSplitter-Setup-${VERSION}.exe"
!endif
OutFile "${OUTFILE}"
InstallDir "$LOCALAPPDATA\Programs\${NAME}"
InstallDirRegKey HKCU "${APPKEY}" "InstallDir"

VIProductVersion "${VERSION}.0"
VIAddVersionKey "ProductName" "${NAME}"
VIAddVersionKey "FileDescription" "${NAME} installer"
VIAddVersionKey "LegalCopyright" "Radio Track Splitter"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"

!addplugindir /x86-unicode "plugins\x86-unicode"

!include MUI2.nsh
!include LogicLib.nsh
!include StrFunc.nsh
${StrStr}

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN
!define MUI_FINISHPAGE_RUN_TEXT "Start ${NAME}"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchApp

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ---------------------------------------------------------------------------
; Download helper: Download <label> <url> <dest> <sha256>  ->  $0 = "ok" | "skipped"
; Streams to <dest>.partial (resumable), verifies the hash, then renames.
; ---------------------------------------------------------------------------
!macro Download LABEL URL DEST SHA
  Push "${SHA}"
  Push "${DEST}"
  Push "${URL}"
  Push "${LABEL}"
  Call DownloadVerified
  Pop $0
!macroend

Function DownloadVerified
  Pop $R3 ; label
  Pop $R2 ; url
  Pop $R1 ; dest
  Pop $R0 ; sha256 (lower-case hex)

attempt:
  DetailPrint "Downloading $R3..."
  NScurl::http GET "$R2" "$R1.partial" /CANCEL /RESUME /END
  Pop $1
  ${If} $1 != "OK"
    MessageBox MB_ABORTRETRYIGNORE|MB_ICONEXCLAMATION "Could not download $R3:$\r$\n$1$\r$\n$\r$\n$R2" /SD IDIGNORE IDRETRY attempt IDIGNORE skip
    Abort "Download of $R3 failed."
  ${EndIf}

  DetailPrint "Verifying $R3..."
  nsExec::ExecToStack '"$SYSDIR\certutil.exe" -hashfile "$R1.partial" SHA256'
  Pop $1 ; exit code
  Pop $2 ; output
  ${StrStr} $3 "$2" "$R0"
  ${If} $3 == ""
    Delete "$R1.partial"
    MessageBox MB_ABORTRETRYIGNORE|MB_ICONEXCLAMATION "The downloaded $R3 failed its integrity check (SHA-256 mismatch)." /SD IDIGNORE IDRETRY attempt IDIGNORE skip
    Abort "Integrity check of $R3 failed."
  ${EndIf}

  Delete "$R1"
  Rename "$R1.partial" "$R1"
  Push "ok"
  Return

skip:
  DetailPrint "Skipped $R3 - the app will need it before it can process recordings."
  Push "skipped"
FunctionEnd

; ---------------------------------------------------------------------------
; Sections
; ---------------------------------------------------------------------------
Section "${NAME}" SecMain
  SectionIn RO
  SetOutPath "$INSTDIR"
  File "..\target\release\${GUI_EXE}"
  File "..\target\release\${CLI_EXE}"
  WriteUninstaller "$INSTDIR\Uninstall.exe"

  CreateShortcut "$SMPROGRAMS\${NAME}.lnk" "$INSTDIR\${GUI_EXE}"

  WriteRegStr HKCU "${APPKEY}" "InstallDir" "$INSTDIR"
  WriteRegStr HKCU "${REGKEY}" "DisplayName" "${NAME}"
  WriteRegStr HKCU "${REGKEY}" "DisplayVersion" "${VERSION}"
  WriteRegStr HKCU "${REGKEY}" "DisplayIcon" "$INSTDIR\${GUI_EXE}"
  WriteRegStr HKCU "${REGKEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${REGKEY}" "UninstallString" '"$INSTDIR\Uninstall.exe"'
  WriteRegStr HKCU "${REGKEY}" "QuietUninstallString" '"$INSTDIR\Uninstall.exe" /S'
  WriteRegDWORD HKCU "${REGKEY}" "NoModify" 1
  WriteRegDWORD HKCU "${REGKEY}" "NoRepair" 1
SectionEnd

Section "FFmpeg (download, ~110 MB)" SecFFmpeg
  AddSize 308000 ; extracted ffmpeg + ffprobe + ffplay
  SetOutPath "$INSTDIR"
  InitPluginsDir ; $PLUGINSDIR is empty until this (or a plugin call) has run
  !insertmacro Download "FFmpeg" "${FFMPEG_URL}" "$PLUGINSDIR\ffmpeg.zip" "${FFMPEG_SHA256}"
  ${If} $0 == "ok"
    DetailPrint "Extracting FFmpeg..."
    nsExec::ExecToLog '"$SYSDIR\tar.exe" -xf "$PLUGINSDIR\ffmpeg.zip" -C "$INSTDIR" --strip-components=2 "*/bin/ffmpeg.exe" "*/bin/ffprobe.exe" "*/bin/ffplay.exe"'
    Pop $1
    Delete "$PLUGINSDIR\ffmpeg.zip"
    ${If} $1 != 0
      MessageBox MB_OK|MB_ICONEXCLAMATION "Extracting FFmpeg failed (tar exit code $1). Install FFmpeg manually (winget install ffmpeg)." /SD IDOK
    ${EndIf}
  ${EndIf}
SectionEnd

Section "VGGish weights (download, ~275 MB)" SecWeights
  AddSize 281800
  SetOutPath "$INSTDIR"
  !insertmacro Download "VGGish weights" "${WEIGHTS_URL}" "$INSTDIR\${WEIGHTS_NAME}" "${WEIGHTS_SHA256}"
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecMain} "The Radio Track Splitter editor and command-line tool."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecFFmpeg} "Downloads FFmpeg, used to decode, play and losslessly cut recordings. Unticked automatically if it is already on this PC."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecWeights} "Downloads the VGGish neural network weights used to detect track boundaries, into the install folder. Unticked automatically if already present."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ---------------------------------------------------------------------------
Function .onInit
!ifndef FORCE_DOWNLOADS ; test builds: makensis /DFORCE_DOWNLOADS ...
  ; Skip downloads for things that are already available.
  SearchPath $0 "ffmpeg.exe"
  SearchPath $1 "ffprobe.exe"
  SearchPath $2 "ffplay.exe"
  ${If} $0 != ""
  ${AndIf} $1 != ""
  ${AndIf} $2 != ""
    Call DeselectFFmpeg
  ${ElseIf} ${FileExists} "$INSTDIR\ffmpeg.exe"
    Call DeselectFFmpeg
  ${EndIf}

  ; Any location the app searches counts as present.
  ${If} ${FileExists} "$INSTDIR\${WEIGHTS_NAME}"
  ${OrIf} ${FileExists} "$LOCALAPPDATA\radio-track-splitter\${WEIGHTS_NAME}"
  ${OrIf} ${FileExists} "$LOCALAPPDATA\split_radio\${WEIGHTS_NAME}"
  ${OrIf} ${FileExists} "$PROFILE\.cache\torch\hub\checkpoints\${WEIGHTS_NAME}"
    SectionSetFlags ${SecWeights} 0
    SectionSetText ${SecWeights} "VGGish weights (already present)"
  ${EndIf}
!endif
FunctionEnd

Function DeselectFFmpeg
  SectionSetFlags ${SecFFmpeg} 0
  SectionSetText ${SecFFmpeg} "FFmpeg (already installed)"
FunctionEnd

Function LaunchApp
  ExecShell "open" "$SMPROGRAMS\${NAME}.lnk"
FunctionEnd

; ---------------------------------------------------------------------------
; Uninstaller: remove only what we installed (no recursive delete of $INSTDIR).
; ---------------------------------------------------------------------------
Section "Uninstall"
  Delete "$INSTDIR\${GUI_EXE}"
  Delete "$INSTDIR\${CLI_EXE}"
  Delete "$INSTDIR\ffmpeg.exe"
  Delete "$INSTDIR\ffprobe.exe"
  Delete "$INSTDIR\ffplay.exe"
  Delete "$INSTDIR\${WEIGHTS_NAME}"
  Delete "$INSTDIR\vggish_cache_*.bin" ; embeddings cached by the app
  Delete "$INSTDIR\${WEIGHTS_NAME}.partial"
  Delete "$INSTDIR\Uninstall.exe"
  Delete "$SMPROGRAMS\${NAME}.lnk"

  DeleteRegKey HKCU "${REGKEY}"
  DeleteRegKey HKCU "${APPKEY}"

  SetOutPath "$TEMP"
  RMDir "$INSTDIR"
SectionEnd
