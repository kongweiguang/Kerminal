//! 显式输入前缀的有限 shell 词法分析；不读取屏幕缓冲区或执行文本。
//!
//! @author kongweiguang

use kerminal_application::connections::RemoteShellKind;

/// 已知 shell 使用有限语法扫描；未知 shell 只接受可跨解析器裸插入的安全 token。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShellFamily {
    Posix,
    Fish,
    PowerShell,
    Unknown,
}

/// 当前 token 保存 Unicode 标量起点和去引号文本，替换范围不使用 UTF-8 字节索引。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CurrentToken {
    pub(super) start: usize,
    pub(super) value: String,
    pub(super) quote: Option<char>,
}

/// 已完成词保留其标准化文本，用于识别当前输入中的命令位置和 Git 子命令。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ShellWord {
    pub(super) start: usize,
    pub(super) value: String,
}

/// 当前命令片段由有限扫描器得出；遇到复杂语法时调用方应跳过候选而不是猜测。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ParsedPrefix {
    pub(super) preceding_words: Vec<ShellWord>,
    pub(super) token: CurrentToken,
    pub(super) shell: ShellFamily,
}

/// 仅把已认证连接返回的 shell kind 映射到有限解析器，Other 与 Unknown 保持保守语义。
pub(super) fn remote_shell_family(kind: RemoteShellKind) -> ShellFamily {
    match kind {
        RemoteShellKind::Bash | RemoteShellKind::Zsh => ShellFamily::Posix,
        RemoteShellKind::Fish => ShellFamily::Fish,
        RemoteShellKind::PowerShell => ShellFamily::PowerShell,
        RemoteShellKind::Other | RemoteShellKind::Unknown => ShellFamily::Unknown,
    }
}

/// 编辑快照的精确名称用于纯本地词法编码；未知名称保持跨 Shell 的保守规则。
pub(super) fn named_shell_family(shell: Option<&str>) -> ShellFamily {
    let name = shell
        .unwrap_or_default()
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.trim_end_matches(".exe") {
        "pwsh" | "powershell" => ShellFamily::PowerShell,
        "bash" | "zsh" => ShellFamily::Posix,
        "fish" => ShellFamily::Fish,
        _ => ShellFamily::Unknown,
    }
}

/// 从光标前显式输入解析当前词；无法表达的 shell 语法会使本次 provider 安全跳过。
pub(super) fn parse_prefix(input: &str, cursor: usize, shell: ShellFamily) -> Option<ParsedPrefix> {
    if input.chars().any(char::is_control) {
        return None;
    }
    if shell == ShellFamily::Unknown {
        return parse_portable_prefix(input, cursor);
    }
    let chars = input.chars().take(cursor).collect::<Vec<_>>();
    let cursor = chars.len();
    let mut start = 0usize;
    let mut quote = None;
    let mut escaped = false;
    let mut value = String::new();
    let mut words = Vec::new();
    let mut current_started = false;
    let mut command_start = 0usize;
    let mut unsupported = false;

    for (index, character) in chars.iter().copied().enumerate() {
        if escaped {
            if (shell == ShellFamily::Posix
                && quote == Some('"')
                && !matches!(character, '$' | '`' | '"' | '\\'))
                || (shell == ShellFamily::Fish
                    && quote == Some('"')
                    && !matches!(character, '$' | '"' | '\\'))
                || (shell == ShellFamily::Fish
                    && quote == Some('\'')
                    && !matches!(character, '\'' | '\\'))
            {
                value.push('\\');
            }
            value.push(character);
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else if character == '\\' && shell == ShellFamily::Fish {
                    escaped = true;
                } else {
                    value.push(character);
                }
                continue;
            }
            Some('"') => {
                if character == '"' {
                    quote = None;
                    continue;
                }
                if character == '\\' && matches!(shell, ShellFamily::Posix | ShellFamily::Fish) {
                    escaped = true;
                    continue;
                }
                if character == '`' && shell == ShellFamily::PowerShell {
                    escaped = true;
                    continue;
                }
                if character == '$' || (character == '`' && shell == ShellFamily::Posix) {
                    unsupported = true;
                }
                value.push(character);
                continue;
            }
            Some('`') => {
                if shell != ShellFamily::PowerShell {
                    unsupported = true;
                } else if character == '`' {
                    quote = None;
                } else {
                    value.push(character);
                }
                continue;
            }
            _ => {}
        }

        match character {
            '\'' | '"' if !current_started => {
                current_started = true;
                start = index;
                quote = Some(character);
            }
            '\'' | '"' => {
                // 混合引号可表达拼接和变量展开；不做部分解析以免替换范围错误。
                unsupported = true;
                quote = Some(character);
            }
            '\\' if matches!(shell, ShellFamily::Posix | ShellFamily::Fish) => {
                if !current_started {
                    current_started = true;
                    start = index;
                }
                escaped = true;
            }
            '`' if shell == ShellFamily::PowerShell => {
                if !current_started {
                    current_started = true;
                    start = index;
                }
                escaped = true;
            }
            '$' | '*' | '?' | '[' | ']' | '{' | '}' => {
                // 不展开变量、glob 或 brace expansion；literal completion 仍可由普通文本触发。
                unsupported = true;
                current_started = true;
                value.push(character);
            }
            ' ' | '\t' if current_started => {
                words.push(ShellWord {
                    start,
                    value: std::mem::take(&mut value),
                });
                current_started = false;
                quote = None;
            }
            ' ' | '\t' => {}
            ';' | '|' | '&' => {
                words.clear();
                value.clear();
                quote = None;
                escaped = false;
                current_started = false;
                command_start = index + 1;
            }
            _ => {
                if !current_started {
                    current_started = true;
                    start = index;
                }
                value.push(character);
            }
        }
    }

    if unsupported || escaped {
        return None;
    }
    if !current_started {
        start = cursor;
    }
    // 只保留当前 command segment，offset 仍相对原输入开头计数。
    words.retain(|word| word.start >= command_start);
    Some(ParsedPrefix {
        preceding_words: words,
        token: CurrentToken {
            start,
            value,
            quote,
        },
        shell,
    })
}

/// 未知 shell 只解析普通字母、数字、点、横线、下划线和斜线，不套用猜测性的引号规则。
fn parse_portable_prefix(input: &str, cursor: usize) -> Option<ParsedPrefix> {
    let chars = input.chars().take(cursor).collect::<Vec<_>>();
    let cursor = chars.len();
    let mut words = Vec::new();
    let mut current = String::new();
    let mut start = cursor;
    for (index, character) in chars.iter().copied().enumerate() {
        if character == ' ' {
            if !current.is_empty() {
                words.push(ShellWord {
                    start,
                    value: std::mem::take(&mut current),
                });
            }
            start = index + 1;
            continue;
        }
        if !portable_character(character) {
            return None;
        }
        if current.is_empty() {
            start = index;
        }
        current.push(character);
    }
    Some(ParsedPrefix {
        preceding_words: words,
        token: CurrentToken {
            start,
            value: current,
            quote: None,
        },
        shell: ShellFamily::Unknown,
    })
}

/// 仅在第一位置补全命令名，避免把任意参数误识别成可执行命令搜索。
pub(super) fn is_command_position(prefix: &ParsedPrefix) -> bool {
    prefix.preceding_words.is_empty()
}

/// 返回不带路径的命令名，供有限路径场景和 Git 识别使用。
pub(super) fn command_name(prefix: &ParsedPrefix) -> Option<&str> {
    prefix
        .preceding_words
        .first()
        .map(|word| word.value.rsplit(['/', '\\']).next().unwrap_or(&word.value))
}

/// 只允许旧版有明确语义的路径型命令在裸参数位置触发路径候选。
pub(super) fn accepts_bare_path(command: &str) -> bool {
    matches!(
        command,
        "cd" | "cat"
            | "chmod"
            | "chown"
            | "cp"
            | "du"
            | "file"
            | "grep"
            | "head"
            | "less"
            | "ls"
            | "mkdir"
            | "mv"
            | "nano"
            | "rm"
            | "rmdir"
            | "sed"
            | "tail"
            | "tar"
            | "touch"
            | "vi"
            | "vim"
            | "zcat"
    )
}

/// 保留用户已输入的原始 token，只按当前引号状态编码新增字符，无法证明安全时不生成候选。
pub(super) fn append_literal(
    input: &str,
    cursor: usize,
    prefix: &ParsedPrefix,
    completed: &str,
) -> Option<String> {
    let suffix = completed.strip_prefix(&prefix.token.value)?;
    if completed.chars().any(char::is_control) {
        return None;
    }
    let raw = input
        .chars()
        .skip(prefix.token.start)
        .take(cursor.checked_sub(prefix.token.start)?)
        .collect::<String>();
    if prefix.token.quote.is_none() && (raw.contains('\'') || raw.contains('"')) {
        return None;
    }
    let mut result = raw;
    for character in suffix.chars() {
        match (prefix.shell, prefix.token.quote) {
            (ShellFamily::Unknown, None) if portable_character(character) => result.push(character),
            (ShellFamily::Unknown, _) => return None,
            (ShellFamily::Posix | ShellFamily::Fish, Some('\'')) if character == '\'' => {
                result.push_str("'\\''")
            }
            (ShellFamily::Fish, Some('\'')) if character == '\\' => result.push_str("\\\\"),
            (ShellFamily::PowerShell, Some('\'')) if character == '\'' => result.push_str("''"),
            (ShellFamily::Posix, Some('"')) if matches!(character, '$' | '`' | '"' | '\\') => {
                result.push('\\');
                result.push(character);
            }
            (ShellFamily::Fish, Some('"')) if matches!(character, '$' | '"' | '\\') => {
                result.push('\\');
                result.push(character);
            }
            (ShellFamily::PowerShell, Some('"')) if matches!(character, '$' | '`' | '"') => {
                result.push('`');
                result.push(character);
            }
            (_, Some(_)) => result.push(character),
            (ShellFamily::Posix | ShellFamily::Fish, None)
                if !is_unquoted_literal(character, prefix.shell) =>
            {
                result.push('\\');
                result.push(character);
            }
            (ShellFamily::PowerShell, None)
                if !is_unquoted_literal(character, ShellFamily::PowerShell) =>
            {
                result.push('`');
                result.push(character);
            }
            (_, None) => result.push(character),
        }
    }
    if let Some(quote) = prefix.token.quote {
        result.push(quote);
    }
    Some(result)
}

/// 只将跨当前 shell 普通参数上下文稳定的字符视为可裸追加。
fn is_unquoted_literal(character: char, shell: ShellFamily) -> bool {
    character.is_alphanumeric()
        || matches!(character, '.' | '_' | '-' | '/')
        || (shell == ShellFamily::PowerShell && matches!(character, '\\' | ':'))
}

/// 未知 shell 上允许裸插入的字符必须跨 POSIX、PowerShell 等常见解析器保持普通文本语义。
fn portable_character(character: char) -> bool {
    character.is_alphanumeric() || matches!(character, '.' | '_' | '-' | '/')
}

#[cfg(test)]
#[path = "../../../tests/unit/command_suggestion_syntax.rs"]
mod tests;
