; P3 trial package watermark (build #6, R13). ASCII-only on purpose (NSIS-safe).
; POSTINSTALL: write hidden watermark ".ys-mark" into the install directory.
; PREUNINSTALL: remove it before the uninstaller deletes files.
; Content (UTF-8 no BOM, two lines): "Y's" / "trial 2026-11-18".

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
