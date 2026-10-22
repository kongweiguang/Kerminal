//! 将结构化命令转换为 SSH exec 单条 shell payload。
//!
//! russh 的 exec API 接受 shell 命令字符串，不提供 argv 语义，因此所有参数必须由此处按
//! 已验证的目标 shell 方言引用；未知方言拒绝执行。
//!
//! @author kongweiguang

use kerminal_application::connections::{ExecRequest, ShellScriptRequest};
use kerminal_domain::connections::RemoteShellDialect;

/// 编码明确要求 shell 语义的原始脚本，并将临时环境值作为 shell literal 注入。
pub fn encode_shell_script(
    request: &ShellScriptRequest,
    dialect: RemoteShellDialect,
) -> Result<String, ExecEncodingError> {
    validate_atom(&request.script)?;
    for (name, value) in &request.environment {
        if !valid_environment_name(name) {
            return Err(ExecEncodingError::InvalidEnvironmentName);
        }
        validate_atom(value)?;
    }
    if request.environment.is_empty() {
        return Ok(request.script.clone());
    }

    let assignments = match dialect {
        RemoteShellDialect::Posix => request
            .environment
            .iter()
            .map(|(name, value)| format!("export {name}={}", quote_posix(value)))
            .collect::<Vec<_>>(),
        RemoteShellDialect::Fish => request
            .environment
            .iter()
            .map(|(name, value)| format!("set -gx {name} {}", quote_fish(value)))
            .collect::<Vec<_>>(),
        RemoteShellDialect::PowerShell => request
            .environment
            .iter()
            .map(|(name, value)| format!("$env:{name} = {}", quote_powershell(value)))
            .collect::<Vec<_>>(),
        RemoteShellDialect::Unknown => return Err(ExecEncodingError::UnknownShellDialect),
    };
    let separator = match dialect {
        RemoteShellDialect::Posix | RemoteShellDialect::Fish | RemoteShellDialect::PowerShell => {
            ";\n"
        }
        RemoteShellDialect::Unknown => return Err(ExecEncodingError::UnknownShellDialect),
    };
    let mut command = assignments.join(separator);
    if !request.script.is_empty() {
        command.push_str(separator);
        command.push_str(&request.script);
    }
    Ok(command)
}

/// 按可信 host profile 的 shell 方言安全编码程序、参数、环境和工作目录。
pub fn encode_exec_command(
    request: &ExecRequest,
    dialect: RemoteShellDialect,
) -> Result<String, ExecEncodingError> {
    if request.program.is_empty() {
        return Err(ExecEncodingError::EmptyProgram);
    }
    if request.program.starts_with('-') {
        return Err(ExecEncodingError::OptionLikeProgram);
    }
    validate_atom(&request.program)?;
    for argument in &request.args {
        validate_atom(argument)?;
    }
    for (name, value) in &request.environment {
        if !valid_environment_name(name) {
            return Err(ExecEncodingError::InvalidEnvironmentName);
        }
        validate_atom(value)?;
    }
    if let Some(path) = &request.working_directory {
        validate_atom(path)?;
    }

    match dialect {
        RemoteShellDialect::Posix => encode_posix(request),
        RemoteShellDialect::Fish => encode_fish(request),
        RemoteShellDialect::PowerShell => encode_powershell(request),
        RemoteShellDialect::Unknown => Err(ExecEncodingError::UnknownShellDialect),
    }
}

/// 为交互 PTY 编码可选的初始命令或指定 cwd；没有 cwd 时保留原生 shell 请求。
pub(crate) fn encode_terminal_shell_command(
    command: Option<&str>,
    working_directory: Option<&str>,
    dialect: RemoteShellDialect,
) -> Result<Option<String>, ExecEncodingError> {
    if let Some(command) = command {
        validate_atom(command)?;
    }
    let Some(directory) = working_directory else {
        return Ok(command.map(str::to_owned));
    };
    validate_atom(directory)?;
    let prefix = match dialect {
        RemoteShellDialect::Posix => {
            format!("cd {}", quote_posix(&normalize_posix_path(directory)))
        }
        RemoteShellDialect::PowerShell => {
            format!("Set-Location -LiteralPath {}", quote_powershell(directory))
        }
        RemoteShellDialect::Fish => format!("cd -- {}", quote_fish(directory)),
        RemoteShellDialect::Unknown => return Err(ExecEncodingError::UnknownShellDialect),
    };
    let tail = command.map(str::to_owned).unwrap_or_else(|| match dialect {
        RemoteShellDialect::Posix => "exec \"${SHELL:-/bin/sh}\" -l".to_owned(),
        RemoteShellDialect::Fish => "exec fish -l".to_owned(),
        RemoteShellDialect::PowerShell => "powershell.exe -NoLogo".to_owned(),
        RemoteShellDialect::Unknown => String::new(),
    });
    Ok(Some(match dialect {
        RemoteShellDialect::Posix | RemoteShellDialect::Fish => format!("{prefix} && {tail}"),
        RemoteShellDialect::PowerShell => format!("{prefix}; {tail}"),
        RemoteShellDialect::Unknown => return Err(ExecEncodingError::UnknownShellDialect),
    }))
}

/// POSIX payload 通过单引号字面量编码数据，并让 `cd` 成功后才执行程序。
fn encode_posix(request: &ExecRequest) -> Result<String, ExecEncodingError> {
    let mut pieces = Vec::with_capacity(request.args.len() + request.environment.len() + 2);
    if let Some(path) = &request.working_directory {
        let path = if !path.starts_with('/') && path.starts_with('-') {
            format!("./{path}")
        } else {
            path.clone()
        };
        pieces.push(format!("cd {}", quote_posix(&path)));
    }
    for (key, value) in &request.environment {
        pieces.push(format!("export {key}={}", quote_posix(value)));
    }
    let mut invocation = vec![format!("exec {}", quote_posix(&request.program))];
    invocation.extend(request.args.iter().map(|value| quote_posix(value)));
    pieces.push(invocation.join(" "));
    Ok(pieces.join(" && "))
}

/// 按 Fish 原生规则逐个引用 argv 与环境项，不复用 POSIX 对反斜杠的不同处理。
fn encode_fish(request: &ExecRequest) -> Result<String, ExecEncodingError> {
    let mut pieces = Vec::with_capacity(request.args.len() + request.environment.len() + 2);
    if let Some(path) = &request.working_directory {
        pieces.push(format!("cd -- {}", quote_fish(path)));
    }
    for (key, value) in &request.environment {
        pieces.push(format!("set -gx {key} {}", quote_fish(value)));
    }
    let mut invocation = vec![format!("exec {}", quote_fish(&request.program))];
    invocation.extend(request.args.iter().map(|value| quote_fish(value)));
    pieces.push(invocation.join(" "));
    Ok(pieces.join(" && "))
}

/// PowerShell payload 捕获启动错误与 cmdlet 异常，同时保留 native 程序的真实退出码。
fn encode_powershell(request: &ExecRequest) -> Result<String, ExecEncodingError> {
    let mut setup = Vec::with_capacity(request.environment.len() + 1);
    for (key, value) in &request.environment {
        setup.push(format!("$env:{key} = {}", quote_powershell(value)));
    }
    if let Some(path) = &request.working_directory {
        setup.push(format!(
            "Set-Location -LiteralPath {}",
            quote_powershell(path)
        ));
    }

    let mut invocation = vec![format!("& {}", quote_powershell(&request.program))];
    invocation.extend(request.args.iter().map(|value| quote_powershell(value)));

    let mut pieces = vec![
        "$ErrorActionPreference = 'Stop'".to_owned(),
        "$PSNativeCommandUseErrorActionPreference = $false".to_owned(),
        "$__kerminalExit = 127".to_owned(),
        "$LASTEXITCODE = $null".to_owned(),
        "try {".to_owned(),
    ];
    pieces.extend(setup.into_iter().map(|statement| format!("  {statement}")));
    pieces.push(format!("  {}", invocation.join(" ")));
    pieces.push("  if ($null -ne $LASTEXITCODE) { $__kerminalExit = $LASTEXITCODE } else { $__kerminalExit = 0 }".to_owned());
    pieces.push("} catch { [Console]::Error.WriteLine($_.Exception.Message) }".to_owned());
    pieces.push("exit $__kerminalExit".to_owned());
    Ok(pieces.join("; "))
}

/// 在 POSIX 单引号字面量中把单引号编码为离开并重新进入单引号的固定序列。
fn quote_posix(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 在 PowerShell 单引号字面量中用两个单引号表示数据里的单引号。
fn quote_powershell(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Fish 单引号仅对反斜杠和单引号有转义语义，换行与 UTF-8 字符保持 literal。
fn quote_fish(value: &str) -> String {
    let escaped = value.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{escaped}'")
}

/// 让相对目录以 `./` 起始，避免 POSIX shell 将以连字符开头的路径当作 cd 选项。
fn normalize_posix_path(path: &str) -> String {
    if !path.starts_with('/') && path.starts_with('-') {
        format!("./{path}")
    } else {
        path.to_owned()
    }
}

/// 仅接受 ASCII shell 标识符，避免环境变量键被解释为赋值语法。
fn valid_environment_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    matches!(bytes.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// 拒绝 NUL，避免结构化数据无法无歧义映射到 SSH 命令 payload。
fn validate_atom(value: &str) -> Result<(), ExecEncodingError> {
    if value.contains('\0') {
        return Err(ExecEncodingError::NulByte);
    }
    Ok(())
}

/// 编码失败只保留原因类别，错误值不包含命令、路径或环境内容。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecEncodingError {
    UnknownShellDialect,
    InvalidEnvironmentName,
    NulByte,
    EmptyProgram,
    OptionLikeProgram,
}
