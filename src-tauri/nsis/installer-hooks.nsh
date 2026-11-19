; P3 packaging hooks. ASCII-only on purpose (NSIS-safe).
; R13 (build #6): hidden watermark ".ys-mark" (content: "Y's" / "trial 2026-11-18").
; R14 (build #8): per-user install dir switched to "%LOCALAPPDATA%\Programs\DesktopEasyNote"
;                (ASCII path, separate from the data root "%LOCALAPPDATA%\DesktopEasyNote").

!macro NSIS_HOOK_PREINSTALL
  ; R14: install into an ASCII path. Runs after the template's initial
  ; "SetOutPath $INSTDIR" but before any files/shortcuts/registry writes, so
  ; both $INSTDIR and $OUTDIR must be switched here.
  StrCpy $INSTDIR "$LOCALAPPDATA\Programs\DesktopEasyNote"
  SetOutPath "$INSTDIR"
  ; The template's initial SetOutPath may have just created the default
  ; legacy-name directory (%LOCALAPPDATA%\${PRODUCTNAME}). Remove it only if it is
  ; empty - real old data/siblings are never touched (RMDir fails on non-empty dirs).
  RMDir "$LOCALAPPDATA\${PRODUCTNAME}"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  FileOpen $0 "$INSTDIR\.ys-mark" w
  FileWrite $0 "Y's$\r$\n"
  FileWrite $0 "trial 2026-11-18"
  FileClose $0
  SetFileAttributes "$INSTDIR\.ys-mark" HIDDEN
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  SetFileAttributes "$INSTDIR\.ys-mark" NORMAL
  Delete "$INSTDIR\.ys-mark"
!macroend
