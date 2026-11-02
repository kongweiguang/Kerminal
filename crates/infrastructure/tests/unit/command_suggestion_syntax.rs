//! 验证 Shell 词法差异不会让追加结果改变原有参数。
//!
//! @author kongweiguang

use super::{ShellFamily, append_literal, parse_prefix};

/// Fish 双引号里的反引号是普通字符，不能套用 Bash 的反引号转义。
#[test]
fn fish_double_quote_keeps_backtick_literal() {
    let input = "cat \"fo";
    let cursor = input.chars().count();
    let prefix = parse_prefix(input, cursor, ShellFamily::Fish).unwrap();
    assert_eq!(
        append_literal(input, cursor, &prefix, "foo`bar"),
        Some("\"foo`bar\"".to_owned())
    );
    let escaped = "cat \"a\\`b";
    let prefix = parse_prefix(escaped, escaped.chars().count(), ShellFamily::Fish).unwrap();
    assert_eq!(prefix.token.value, "a\\`b");
    assert!(parse_prefix("cat \"a`b", 8, ShellFamily::Posix).is_none());
}

/// Fish 单引号中的反斜杠必须保护闭合引号，单引号本身通过安全拼接追加。
#[test]
fn fish_single_quote_preserves_backslash_and_quote() {
    let input = "cat 'foo";
    let cursor = input.chars().count();
    let prefix = parse_prefix(input, cursor, ShellFamily::Fish).unwrap();
    assert_eq!(
        append_literal(input, cursor, &prefix, "foo\\bar'qux"),
        Some("'foo\\\\bar'\\''qux'".to_owned())
    );
    let existing = "cat 'foo\\\\bar";
    let prefix = parse_prefix(existing, existing.chars().count(), ShellFamily::Fish).unwrap();
    assert_eq!(prefix.token.value, "foo\\bar");
}
