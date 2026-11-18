; @author kongweiguang
; 在安装器替换旧版之前只做预检；迁移事务由新版首次启动统一负责。
Section "-Kerminal migration preflight"
  StrCpy $1 0
  kerminal_wait_for_exit:
    nsis_tauri_utils::FindProcess "${MAINBINARYNAME}.exe"
    Pop $0
    ${If} $0 == 0
      ${If} $1 >= 60
        IfSilent +2 0
          MessageBox MB_OK|MB_ICONSTOP "Please close Kerminal before installing the update."
        SetErrorLevel 1
        Abort
      ${EndIf}
      Sleep 500
      IntOp $1 $1 + 1
      Goto kerminal_wait_for_exit
    ${EndIf}

  InitPluginsDir
  SetOutPath "$PLUGINSDIR"
  File /oname=kerminal-migrate.exe "@MIGRATION_HELPER@"
  ; 安装器专用预检输出 UTF-16LE BOM，且拒绝相对环境根，避免临时工作目录改变数据来源。
  nsExec::ExecToStack '"$PLUGINSDIR\kerminal-migrate.exe" --installer-dry-run'
  Pop $0
  Pop $1
  ${If} $0 != 0
    SetDetailsView show
    DetailPrint "Kerminal data preflight failed (exit code: $0)."
    ${If} $1 != ""
      DetailPrint "Preflight report (NSIS output limit may truncate it):"
      DetailPrint "$1"
    ${Else}
      DetailPrint "No preflight report was returned; the helper may not have started."
    ${EndIf}
    IfSilent +2 0
      MessageBox MB_OK|MB_ICONSTOP "Kerminal data preflight failed (exit code: $0). See the installation details for the report, which may be truncated. Close all Kerminal instances and retry. Your existing installation has not been changed."
    SetErrorLevel 1
    Abort
  ${EndIf}
SectionEnd
