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
  nsExec::ExecToStack '"$PLUGINSDIR\kerminal-migrate.exe" --dry-run'
  Pop $0
  Pop $1
  ${If} $0 != 0
    IfSilent +2 0
      MessageBox MB_OK|MB_ICONSTOP "Kerminal data preflight failed. Close all Kerminal instances and inspect the migration report before retrying."
    SetErrorLevel 1
    Abort
  ${EndIf}
SectionEnd
