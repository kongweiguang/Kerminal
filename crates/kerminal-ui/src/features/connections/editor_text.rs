//! 表单列表使用可逆的文本表示；引号只用于分隔特殊值，反斜杠始终保留字面含义。
//!
//! @author kongweiguang

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ListSeparator {
    Lines,
    Commas,
}

impl ListSeparator {
    /// 参数逐行且保留空项；标签延续逗号分隔和外围空白忽略的输入习惯。
    fn character(self) -> char {
        match self {
            Self::Lines => '\n',
            Self::Commas => ',',
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(super) struct ListTextError;

/// 特殊项采用 CSV 双引号规则；不使用反斜杠转义，避免 Windows 路径被当作控制字符。
pub(super) fn format_list(values: &[String], separator: ListSeparator) -> String {
    values
        .iter()
        .map(|value| {
            if value.is_empty()
                || value.contains(['"', '\r', '\n', separator.character()])
                || (separator == ListSeparator::Commas && value.trim() != value)
            {
                format!("\"{}\"", value.replace('"', "\"\""))
            } else {
                value.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(match separator {
            ListSeparator::Lines => "\n",
            ListSeparator::Commas => ", ",
        })
}

/// 空文本代表零项，`""` 明确代表一个空项；格式有误时拒绝提交，不能猜测或拆坏原配置。
pub(super) fn parse_list(
    source: &str,
    separator: ListSeparator,
) -> Result<Vec<String>, ListTextError> {
    if source.is_empty() {
        return Ok(Vec::new());
    }
    let delimiter = separator.character();
    let mut remaining = source;
    let mut values = Vec::new();
    loop {
        if separator == ListSeparator::Commas {
            remaining = remaining.trim_start_matches([' ', '\t']);
        }
        let value;
        let is_quoted = remaining.starts_with('"');
        if let Some(quoted) = remaining.strip_prefix('"') {
            let mut content = String::new();
            let mut chars = quoted.char_indices().peekable();
            let end = loop {
                match chars.next() {
                    Some((_, '"')) if chars.peek().is_some_and(|(_, ch)| *ch == '"') => {
                        chars.next();
                        content.push('"');
                    }
                    Some((offset, '"')) => break offset + 1,
                    Some((_, ch)) => content.push(ch),
                    None => return Err(ListTextError),
                }
            };
            remaining = quoted[end..].trim_start_matches([' ', '\t']);
            if separator == ListSeparator::Lines && remaining.starts_with("\r\n") {
                remaining = &remaining[1..];
            }
            if !remaining.is_empty() && !remaining.starts_with(delimiter) {
                return Err(ListTextError);
            }
            value = content;
        } else {
            let end = remaining.find(delimiter).unwrap_or(remaining.len());
            let mut content = &remaining[..end];
            if separator == ListSeparator::Commas {
                content = content.trim();
            } else if end < remaining.len() {
                content = content.strip_suffix('\r').unwrap_or(content);
            }
            value = content.to_owned();
            remaining = &remaining[end..];
        }
        if separator == ListSeparator::Lines || is_quoted || !value.is_empty() {
            values.push(value);
        }
        if remaining.is_empty() {
            return Ok(values);
        }
        remaining = &remaining[delimiter.len_utf8()..];
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/connections_editor_text.rs"]
mod tests;
