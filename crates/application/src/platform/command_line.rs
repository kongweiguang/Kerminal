//! 不执行 shell 的命令字符串分词，供需要保留旧命令格式的应用用例复用。
//!
//! @author kongweiguang

/// 命令分词失败类型保持窄化，调用方按自己的业务上下文映射安全错误文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommandLineParseError {
    UnclosedQuote,
}

/// 按引号边界生成 argv；不展开变量、不执行命令替换，也不调用系统 shell。
pub(crate) fn split_command_line(command: &str) -> Result<Vec<String>, CommandLineParseError> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut token_started = false;
    let mut characters = command.chars().peekable();

    while let Some(character) = characters.next() {
        if character == '\\' {
            if characters.peek().is_some_and(|next| {
                *next == '\\' || Some(*next) == quote || *next == '\'' || *next == '"'
            }) {
                if let Some(escaped) = characters.next() {
                    current.push(escaped);
                }
            } else {
                current.push(character);
            }
            token_started = true;
        } else if let Some(active_quote) = quote {
            if character == active_quote {
                quote = None;
            } else {
                current.push(character);
            }
            token_started = true;
        } else if character == '\'' || character == '"' {
            quote = Some(character);
            token_started = true;
        } else if character.is_whitespace() {
            if token_started {
                tokens.push(std::mem::take(&mut current));
                token_started = false;
            }
        } else {
            current.push(character);
            token_started = true;
        }
    }

    if quote.is_some() {
        return Err(CommandLineParseError::UnclosedQuote);
    }
    if token_started {
        tokens.push(current);
    }
    Ok(tokens)
}
