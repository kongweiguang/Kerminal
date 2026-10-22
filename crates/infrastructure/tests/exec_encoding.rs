//! 对 SSH exec quoting 做本机 shell 端到端检查。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, process::Command};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use kerminal_application::connections::{ExecRequest, ShellScriptRequest};
use kerminal_domain::connections::RemoteShellDialect;

#[allow(
    dead_code,
    reason = "测试通过 path import 复用产品模块，但只覆盖其中一部分能力"
)]
#[path = "../src/connections/exec.rs"]
mod exec;

use exec::{ExecEncodingError, encode_exec_command, encode_shell_script};

/// 建立可执行的 PowerShell 结构化请求，参数包含会触发变量展开的 shell 字符。
fn request(program: &str, args: &[&str]) -> ExecRequest {
    ExecRequest {
        program: program.to_owned(),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        environment: BTreeMap::new(),
        working_directory: None,
    }
}

/// 为原始 shell 脚本显式提供测试环境，覆盖旧 MCP 临时代理环境的同一安全编码路径。
fn shell_script(script: &str, environment: &[(&str, &str)]) -> ShellScriptRequest {
    ShellScriptRequest {
        script: script.to_owned(),
        environment: environment
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
    }
}

/// 通过 pwsh 验证编码结果；Windows 探测必须禁用控制台窗口，避免后台测试抢占焦点。
fn run_powershell(command: &str) -> Option<std::process::Output> {
    let mut process = Command::new("pwsh");
    #[cfg(windows)]
    process.creation_flags(0x0800_0000);
    process
        .args(["-NoProfile", "-NonInteractive", "-Command", command])
        .output()
        .ok()
}

/// 使用可用的 fish 验证编码结果；原生进程与 WSL 探测在 Windows 均不得创建控制台窗口。
fn run_fish(command: &str) -> Option<std::process::Output> {
    let mut native = Command::new("fish");
    #[cfg(windows)]
    native.creation_flags(0x0800_0000);
    if let Ok(output) = native.args(["-c", command]).output() {
        return Some(output);
    }
    let mut wsl = Command::new("wsl.exe");
    #[cfg(windows)]
    wsl.creation_flags(0x0800_0000);
    wsl.args(["-e", "fish", "-c", command]).output().ok()
}

/// 确认表达式注入载荷到达目标程序时仍是原始参数文本。
#[test]
fn powershell_preserves_shell_metacharacters_as_argument_data() {
    let request = request("Write-Output", &["$(throw 'executed'); 'quoted'"]);
    let command = encode_exec_command(&request, RemoteShellDialect::PowerShell).unwrap();
    let Some(output) = run_powershell(&command) else {
        eprintln!("pwsh is unavailable; skipping shell execution check");
        return;
    };

    assert!(output.status.success(), "encoded PowerShell command failed");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "$(throw 'executed'); 'quoted'"
    );
}

/// 确认 native 程序的非零退出状态不会被 PowerShell 包装层吞掉。
#[test]
fn powershell_preserves_native_nonzero_exit_code() {
    let request = request("cmd.exe", &["/c", "exit", "37"]);
    let command = encode_exec_command(&request, RemoteShellDialect::PowerShell).unwrap();
    let Some(output) = run_powershell(&command) else {
        eprintln!("pwsh is unavailable; skipping shell execution check");
        return;
    };
    if !cfg!(windows) {
        eprintln!("cmd.exe is unavailable on this platform; skipping native exit-code check");
        return;
    }

    assert_eq!(output.status.code(), Some(37));
}

/// 确认启动失败不会沿用前一命令退出码并伪装成功。
#[test]
fn powershell_reports_missing_program_as_failure() {
    let request = request("kerminal-program-that-does-not-exist", &[]);
    let command = encode_exec_command(&request, RemoteShellDialect::PowerShell).unwrap();
    let Some(output) = run_powershell(&command) else {
        eprintln!("pwsh is unavailable; skipping shell execution check");
        return;
    };

    assert!(!output.status.success());
}

/// 拒绝由连字符开头的程序名，防止 POSIX shell 内建命令把它解释为选项。
#[test]
fn option_like_program_is_rejected() {
    let request = request("-program", &[]);
    assert_eq!(
        encode_exec_command(&request, RemoteShellDialect::Posix),
        Err(ExecEncodingError::OptionLikeProgram)
    );
}

/// 原始脚本没有额外环境值时保留 SSH exec 的原生脚本语义，包括未知方言主机。
#[test]
fn raw_shell_script_without_environment_is_passed_through() {
    let request = shell_script("printf '%s\\n' user-script", &[]);
    assert_eq!(
        encode_shell_script(&request, RemoteShellDialect::Unknown),
        Ok(request.script)
    );
}

/// PowerShell 环境值按 literal 编码，脚本本身仍是调用方明确提交的 shell source。
#[test]
fn powershell_shell_environment_value_is_literal() {
    let request = shell_script(
        "Write-Output $env:KERNEL_TEST_PROXY",
        &[("KERNEL_TEST_PROXY", "$(throw 'injected'); value")],
    );
    let command = encode_shell_script(&request, RemoteShellDialect::PowerShell).unwrap();
    let Some(output) = run_powershell(&command) else {
        eprintln!("pwsh is unavailable; skipping shell execution check");
        return;
    };

    assert!(output.status.success(), "encoded shell environment failed");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "$(throw 'injected'); value"
    );
}

/// 只有设置环境变量时才要求已知 shell 方言，避免未知方言误编码 assignment。
#[test]
fn shell_environment_rejects_unknown_dialect_and_invalid_names() {
    let request = shell_script("true", &[("VALID_NAME", "value")]);
    assert_eq!(
        encode_shell_script(&request, RemoteShellDialect::Unknown),
        Err(ExecEncodingError::UnknownShellDialect)
    );
    let request = shell_script("true", &[("BAD;NAME", "value")]);
    assert_eq!(
        encode_shell_script(&request, RemoteShellDialect::Posix),
        Err(ExecEncodingError::InvalidEnvironmentName)
    );
}

/// Fish argv 编码应把单引号、反斜杠、换行、中文和命令替换字符原样交给目标程序。
#[test]
fn fish_preserves_quotes_newlines_and_unicode_as_argument_data() {
    let payload = "中文\nquote' backslash\\ ; printf KERM_INJECTED $(printf INJECTED)";
    let request = request("printf", &["<%s>\n", payload]);
    let command = encode_exec_command(&request, RemoteShellDialect::Fish).unwrap();
    let Some(output) = run_fish(&command) else {
        eprintln!("fish is unavailable; skipping shell execution check");
        return;
    };

    assert!(output.status.success(), "encoded fish command failed");
    assert_eq!(output.stdout, format!("<{payload}>\n").as_bytes());
}

/// Fish 的导出变量与原始脚本处于同一 exec shell scope，且环境值不会被二次解释。
#[test]
fn fish_shell_environment_stays_in_scope_and_literal() {
    let payload = "中文\nquoted' slash\\ $(printf INJECTED)";
    let request = shell_script(
        "printf '<%s>\\n' \"$KERM_TEST_VALUE\"",
        &[("KERM_TEST_VALUE", payload)],
    );
    let command = encode_shell_script(&request, RemoteShellDialect::Fish).unwrap();
    let Some(output) = run_fish(&command) else {
        eprintln!("fish is unavailable; skipping shell execution check");
        return;
    };

    assert!(output.status.success(), "encoded fish environment failed");
    assert_eq!(output.stdout, format!("<{payload}>\n").as_bytes());
}
