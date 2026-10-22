//! SFTP 预览的 UTF-8 截断边界回归测试。
//!
//! @author kongweiguang

use super::is_binary_preview;

/// UTF-8 多字节字符只在预算边界处不完整时保留可读前缀。
#[test]
fn incomplete_truncated_utf8_tail_is_not_binary() {
    assert!(!is_binary_preview(&[0xe4, 0xb8], true));
}

/// 非尾部坏字节、未截断不完整字符和 NUL 都维持二进制分类。
#[test]
fn invalid_utf8_or_nul_is_binary() {
    assert!(is_binary_preview(&[0xe4, 0xb8], false));
    assert!(is_binary_preview(&[b'a', 0xff], true));
    assert!(is_binary_preview(b"text\0tail", true));
}
