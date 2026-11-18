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
  ; nsExec 按系统 ANSI 解码裸 UTF-8；固定转换器将 helper 的 UTF-8 JSON 转成带 BOM 的 UTF-16，保留中文路径。
  System::Call 'kernel32::SetEnvironmentVariableW(w "KERMINAL_INSTALLER_HELPER", w "$PLUGINSDIR\kerminal-migrate.exe") i.r2'
  ${If} $2 == 0
    DetailPrint "Unable to prepare the migration helper."
    IfSilent +2 0
      MessageBox MB_OK|MB_ICONSTOP "Unable to prepare the migration helper. Your existing installation has not been changed."
    SetErrorLevel 1
    Abort
  ${EndIf}
  nsExec::ExecToStack '"$SYSDIR\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -Command "try{$$p=New-Object Diagnostics.Process;$$s=$$p.StartInfo;$$s.FileName=$$env:KERMINAL_INSTALLER_HELPER;$$s.Arguments=$\'--dry-run$\';$$s.UseShellExecute=0;$$s.RedirectStandardOutput=1;$$s.StandardOutputEncoding=[Text.Encoding]::UTF8;$$p.Start()|Out-Null;$$o=$$p.StandardOutput.ReadToEnd();$$p.WaitForExit();$$b=[Text.Encoding]::Unicode.GetBytes($$o);if($$b.Length){$$x=[Console]::OpenStandardOutput();$$x.WriteByte(255);$$x.WriteByte(254);$$x.Write($$b,0,$$b.Length)};exit $$p.ExitCode}catch{exit 127}"'
  Pop $0
  Pop $1
  System::Call 'kernel32::SetEnvironmentVariableW(w "KERMINAL_INSTALLER_HELPER", p 0) i.r2'
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
