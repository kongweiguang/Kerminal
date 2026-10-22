//! 将 GPUI 剪贴板条目转换为终端可粘贴文本，并按目标 shell 转义文件路径。
//!
//! 文本条目沿用 GPUI 的优先级与原内容；只有 `ExternalPaths` 才按会话的真实 shell 方言
//! 编码，避免把本机系统类型误当成远端 shell。
//!
//! @author kongweiguang

use gpui_kit::{ClipboardEntry, ClipboardItem};
use kerminal_domain::connections::RemoteShellDialect;

use crate::shell_arguments::quote_shell_argument;

/// 文件路径无法安全投影到当前终端时返回的稳定原因，供 Root 映射为可见提示。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TerminalClipboardError {
    UnknownShellDialect,
    NonUnicodePath,
    ControlCharacterInPath,
}

impl TerminalClipboardError {
    /// 返回不包含目标路径或剪贴板内容的提示，避免把用户文件名带入状态栏日志。
    ///
    /// @author kongweiguang
    pub(crate) const fn message(self) -> &'static str {
        match self {
            Self::UnknownShellDialect => "无法确认当前终端的 shell 类型，不能安全粘贴文件路径",
            Self::NonUnicodePath => "剪贴板中的文件路径不是有效 Unicode，未粘贴",
            Self::ControlCharacterInPath => "剪贴板中的文件路径含终端控制字符，未粘贴",
        }
    }
}

/// 判断剪贴板是否应走 shell 路径编码，以便 Root 只为文件路径异步读取会话元数据。
///
/// GPUI 的 `ClipboardItem::text()` 先合并非空字符串，再在结果为空时拼接路径；这里保持
/// 同一优先级，同时识别路径分支，避免普通文本粘贴等待 shell 元数据。
///
/// @author kongweiguang
pub(crate) fn requires_shell_dialect(item: &ClipboardItem) -> bool {
    let has_text = item
        .entries
        .iter()
        .any(|entry| matches!(entry, ClipboardEntry::String(value) if !value.text.is_empty()));
    if has_text {
        return false;
    }

    item.entries
        .iter()
        .any(|entry| matches!(entry, ClipboardEntry::ExternalPaths(paths) if !paths.0.is_empty()))
}

/// 保留剪贴板文本优先级；文件路径按真实目标 shell 分别转义后用空格连接。
///
/// 不调用 GPUI 的 `ClipboardItem::text()`，因为 0.3.9 会把多条 ExternalPaths 无分隔拼接。
/// 未知方言、非 Unicode 路径和控制字符均 fail closed；普通文本不依赖方言，保持原样。
///
/// @author kongweiguang
pub(crate) fn terminal_paste_text(
    item: &ClipboardItem,
    dialect: RemoteShellDialect,
) -> Result<Option<String>, TerminalClipboardError> {
    let mut text = String::new();
    for entry in &item.entries {
        if let ClipboardEntry::String(value) = entry {
            text.push_str(&value.text);
        }
    }
    if !text.is_empty() {
        return Ok(Some(text));
    }

    let mut quoted_paths = Vec::new();
    for entry in &item.entries {
        let ClipboardEntry::ExternalPaths(paths) = entry else {
            continue;
        };
        for path in &paths.0 {
            let path = path
                .to_str()
                .ok_or(TerminalClipboardError::NonUnicodePath)?;
            if path.chars().any(|character| character.is_control()) {
                return Err(TerminalClipboardError::ControlCharacterInPath);
            }
            quoted_paths.push(
                quote_shell_argument(path, dialect)
                    .ok_or(TerminalClipboardError::UnknownShellDialect)?,
            );
        }
    }

    if quoted_paths.is_empty() {
        Ok(None)
    } else {
        Ok(Some(quoted_paths.join(" ")))
    }
}

#[cfg(test)]
#[path = "../tests/unit/terminal_clipboard.rs"]
mod tests;
