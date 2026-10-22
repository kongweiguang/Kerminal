//! 本机文件预览的 UTF-8 截断边界回归测试。
//!
//! @author kongweiguang

use super::is_binary_preview;

/// 中文字符在预览上限处被截断时仍应显示文本前缀，而不把它送进编辑器。
#[test]
fn incomplete_truncated_utf8_tail_is_not_binary() {
    assert!(!is_binary_preview(&[0xe4, 0xb8], true));
}

/// 未截断的无效序列和文件内 NUL 都代表二进制内容，避免错误地提供文本预览。
#[test]
fn invalid_utf8_or_nul_is_binary() {
    assert!(is_binary_preview(&[0xe4, 0xb8], false));
    assert!(is_binary_preview(&[b'a', 0xff], true));
    assert!(is_binary_preview(b"text\0tail", true));
}
