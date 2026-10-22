//! shell startup plan 与远端 bootstrap 的公开契约测试。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use kerminal_domain::{connections::RemoteShellDialect, sessions::LocalSessionTarget};
use kerminal_infrastructure::shell_integration::{
    ShellIntegrationBootstrapStatus, ShellIntegrationUnavailableReason, prepare_local_shell,
};

/// 构造最小本地启动目标，让测试直接表达指定程序和原始 argv。
fn target(program: &str, args: &[&str]) -> LocalSessionTarget {
    LocalSessionTarget {
        program: OsString::from(program),
        args: args.iter().map(OsString::from).collect(),
        working_directory: Some(PathBuf::from(".")),
        environment: BTreeMap::new(),
    }
}

/// Bash 只在可维护交互启动语义时添加 hook，并保留调用者参数。
#[test]
fn local_bash_plan_keeps_arguments_and_owns_temporary_files() {
    let plan = prepare_local_shell(&target("bash", &["-i"])).expect("prepare bash startup");

    assert_eq!(
        plan.bootstrap_status,
        ShellIntegrationBootstrapStatus::Prepared
    );
    assert_eq!(plan.unavailable_reason, None);
    assert_eq!(plan.dialect, RemoteShellDialect::Posix);
    assert!(plan.lifetime_guard.is_some());
    assert_eq!(plan.args[0], "--rcfile");
    assert!(plan.args.iter().any(|argument| argument == "-i"));
}

/// 命令模式不得被包成持续会话，原始 argv 继续交给普通 shell 启动。
#[test]
fn command_mode_is_preserved_without_managed_integration() {
    let request = target("bash", &["-lc", "pwd"]);
    let plan = prepare_local_shell(&request).expect("preserve bash command mode");

    assert_eq!(plan.args, request.args);
    assert_eq!(
        plan.bootstrap_status,
        ShellIntegrationBootstrapStatus::Unsupported
    );
    assert_eq!(
        plan.unavailable_reason,
        Some(ShellIntegrationUnavailableReason::IncompatibleStartupArguments)
    );
    assert!(plan.lifetime_guard.is_none());
    assert_eq!(plan.dialect, RemoteShellDialect::Unknown);
}

/// `-NoProfile` 只跳过用户配置，仍允许在交互 shell 中追加本应用的临时 bootstrap。
///
/// @author kongweiguang
#[test]
fn local_powershell_noprofile_keeps_managed_bootstrap() {
    let request = target("powershell.exe", &["-NoLogo", "-NoProfile"]);
    let plan = prepare_local_shell(&request).expect("prepare PowerShell startup");

    assert_eq!(
        plan.bootstrap_status,
        ShellIntegrationBootstrapStatus::Prepared
    );
    assert_eq!(plan.unavailable_reason, None);
    assert_eq!(plan.dialect, RemoteShellDialect::PowerShell);
    assert!(plan.lifetime_guard.is_some());
    assert_eq!(&plan.args[..request.args.len()], request.args.as_slice());
    assert_eq!(plan.args[request.args.len()], "-NoExit");
    assert_eq!(plan.args[request.args.len() + 1], "-Command");
    assert!(
        plan.args[request.args.len() + 2]
            .to_string_lossy()
            .contains("powershell.ps1")
    );
}

/// `-NoProfile` 不会把脚本、显式命令或非交互模式变成交互 shell。
///
/// @author kongweiguang
#[test]
fn local_powershell_noprofile_keeps_explicit_execution_modes_unmanaged() {
    let cases: [&[&str]; 6] = [
        &["-NoProfile", "-Command", "Get-Date"],
        &["-NoProfile", "-File", "script.ps1"],
        &["-NoProfile", "-EncodedCommand", "R2V0LURhdGU="],
        &["-NoProfile", "-NonInteractive"],
        &["-NoProfile:unexpected"],
        &["-NoProfile=unexpected"],
    ];

    for arguments in cases {
        let request = target("powershell.exe", arguments);
        let plan = prepare_local_shell(&request).expect("preserve explicit PowerShell mode");

        assert_eq!(plan.args, request.args);
        assert_eq!(
            plan.bootstrap_status,
            ShellIntegrationBootstrapStatus::Unsupported
        );
        assert_eq!(
            plan.unavailable_reason,
            Some(ShellIntegrationUnavailableReason::IncompatibleStartupArguments)
        );
        assert!(plan.lifetime_guard.is_none());
        assert_eq!(plan.dialect, RemoteShellDialect::Unknown);
    }
}

/// 在系统自带 PowerShell 中执行真实脚本，验证旧 PSReadLine 及含中文、空格、#、% 的 OSC7 字节。
/// 子进程禁用 profile 且只修改自身按键绑定；自定义 Enter 必须完整保留，不能假报 integration 已启用。
/// 清除宿主 PSModulePath 后由该 shell 重建默认模块路径，避免测试误加载 pwsh 的新版 PSReadLine。
/// 失败 cmdlet 不得复用此前 native 命令的全局 LASTEXITCODE；不可归属的失败码保持 Unknown。
/// 显式禁止子进程创建控制台，后台验证不能打断正在使用电脑的用户。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_powershell_hook_supports_legacy_key_handler_and_real_osc_markers() {
    use std::{os::windows::process::CommandExt, process::Command};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let fixture = tempfile::tempdir().expect("create private cwd fixture");
    let directory = fixture.path().join("cwd 中文 #%");
    std::fs::create_dir(&directory).expect("create special-character cwd");

    let plan = prepare_local_shell(&target("powershell.exe", &["-NoLogo", "-NoProfile"]))
        .expect("prepare Windows PowerShell hook");
    let bootstrap = plan
        .args
        .last()
        .expect("bootstrap command")
        .to_str()
        .expect("UTF-8 bootstrap command");

    for custom_enter in [false, true] {
        let setup = if custom_enter {
            "Set-PSReadLineKeyHandler -Chord Enter -Function AddLine"
        } else {
            ""
        };
        let assertions = if custom_enter {
            r#"
if ($global:__kerminal_shell_integration_active) { throw 'Custom Enter was wrapped' }
if ($enter.Count -ne 1 -or $enter[0].Function -ne 'AddLine') { throw 'Custom Enter changed' }
if ((prompt) -cne 'KerminalPrompt> ') { throw 'Custom prompt changed' }
"#
        } else {
            r#"
if (-not $global:__kerminal_shell_integration_active) { throw 'Integration was not installed' }
if ($enter.Count -ne 1 -or $enter[0].Function -ne 'KerminalAcceptLine') { throw 'Enter hook missing' }
$directoryUri = [Uri]::EscapeDataString('/' + $PWD.ProviderPath.Replace('\', '/')).Replace('%2F', '/')
if (-not $directoryUri.EndsWith('/cwd%20%E4%B8%AD%E6%96%87%20%23%25')) { throw 'Cwd fixture encoding differs' }
$expected = "$([char]27)]7;file://$directoryUri`a$([char]27)]133;A`aKerminalPrompt> $([char]27)]133;B`a"
if ((prompt) -cne $expected) { throw 'Prompt OSC bytes differ' }
$global:__kerminal_command_active = $true
if ((prompt) -cne ("$([char]27)]133;D;0`a" + $expected)) { throw 'Completion OSC bytes differ' }
# Reproduce a prior native status followed by a distinct failed cmdlet.
& $env:ComSpec /d /c 'exit 23'
if ($LASTEXITCODE -ne 23) { throw 'Native exit-code fixture did not return 23' }
$global:__kerminal_command_active = $true
$missingPath = Join-Path $PWD ('kerminal-r43-missing-' + [guid]::NewGuid().ToString('N'))
Get-Item -LiteralPath $missingPath -ErrorAction SilentlyContinue
$failedPrompt = [string] (prompt)
if ($failedPrompt -cne ("$([char]27)]133;D`a" + $expected)) { throw 'Failed cmdlet reused a stale native exit code' }
if ($LASTEXITCODE -ne 23) { throw 'Shell integration changed the user LASTEXITCODE value' }
"#
        };
        let probe = format!(
            r#"
$ErrorActionPreference = 'Stop'
function global:prompt {{ 'KerminalPrompt> ' }}
{setup}
{bootstrap}
$enter = @(Get-PSReadLineKeyHandler | Where-Object {{ $_.Key -eq 'Enter' }})
{assertions}
$installedPrompt = ${{function:prompt}}.ToString()
{bootstrap}
if (${{function:prompt}}.ToString() -cne $installedPrompt) {{ throw 'Repeated installation changed prompt' }}
'KerminalHookProbeReady'
"#
        );
        let output = Command::new("powershell.exe")
            .creation_flags(CREATE_NO_WINDOW)
            .current_dir(&directory)
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .env_remove("PSModulePath")
            .arg(probe)
            .output()
            .expect("run isolated Windows PowerShell hook probe");

        assert!(
            output.status.success(),
            "hook probe failed (custom_enter={custom_enter}): {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "hook emitted startup errors: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("KerminalHookProbeReady"),
            "hook probe did not finish (custom_enter={custom_enter})"
        );
    }
}

/// Bash 编码器必须按 UTF8 字节处理特殊目录，且完整生产 hook 不依赖每次 prompt 的外部进程。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn bash_hook_reports_uri_encoded_directory() {
    use std::{fmt::Write, process::Command};

    let fixture = tempfile::tempdir().expect("create private cwd fixture");
    let directory = fixture.path().join("cwd 中文 #%");
    std::fs::create_dir(&directory).expect("create special-character cwd");
    let mut expected = String::from("\x1b]7;file://");
    for byte in directory.to_str().expect("UTF-8 fixture path").bytes() {
        if byte.is_ascii_alphanumeric() || b"/._~-".contains(&byte) {
            expected.push(char::from(byte));
        } else {
            write!(&mut expected, "%{byte:02X}").expect("write to String");
        }
    }
    expected.push('\x07');
    let hook = include_str!("../src/shell_integration/scripts/bash.sh");
    let output = Command::new("bash")
        .args(["--noprofile", "--norc", "-c"])
        .arg(format!("{hook}\n__kerminal_finish_prompt\n"))
        .current_dir(&directory)
        .output()
        .expect("run isolated Bash hook");

    assert!(output.status.success(), "Bash hook failed");
    assert!(output.stderr.is_empty(), "Bash hook emitted errors");
    assert_eq!(output.stdout, expected.as_bytes());
}

/// 用户 Fish prompt 可在打印过程中 cd；保留原输出逐字节顺序，并只在完成后报告最终目录。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn fish_hook_reports_directory_after_user_prompt_changes_it() {
    use std::{fmt::Write, io::ErrorKind, process::Command};

    let fixture = tempfile::tempdir().expect("create private Fish cwd fixture");
    let directory = fixture.path().join("prompt 中文 #%");
    std::fs::create_dir(&directory).expect("create prompt target directory");
    let mut expected = String::from("\x1b]133;A\x07user-before\nuser-after\x1b]7;file://");
    for byte in directory.to_str().expect("UTF-8 fixture path").bytes() {
        if byte.is_ascii_alphanumeric() || b"/._~-".contains(&byte) {
            expected.push(char::from(byte));
        } else {
            write!(&mut expected, "%{byte:02X}").expect("write to String");
        }
    }
    expected.push_str("\x07\x1b]133;B\x07");
    let hook = include_str!("../src/shell_integration/scripts/fish.fish");
    let output = Command::new("fish")
        .args(["--no-config", "-c"])
        .arg(format!(
            "function fish_prompt\n    printf 'user-before\\n'\n    builtin cd \"$KERMINAL_PROMPT_CWD\"; or exit 97\n    printf 'user-after'\nend\n{hook}\nfish_prompt\n"
        ))
        .env("KERMINAL_PROMPT_CWD", &directory)
        .current_dir(fixture.path())
        .output();
    let output = match output {
        Ok(output) => output,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            println!("SKIPPED: fish executable is unavailable on PATH");
            return;
        }
        Err(error) => panic!("run isolated Fish hook: {error}"),
    };

    assert!(output.status.success(), "Fish hook failed");
    assert!(output.stderr.is_empty(), "Fish hook emitted errors");
    assert_eq!(output.stdout, expected.as_bytes());
}
