//! 文件权限输入的边界测试，不创建窗口或访问本机文件。
//!
//! @author kongweiguang

use super::{format_octal_mode, format_symbolic_mode, parse_octal_mode};

/// 显式输入保留特殊位并拒绝非法数字与不完整 mode。
#[test]
fn parses_complete_octal_modes_without_guessing() {
    assert_eq!(parse_octal_mode("755"), Some(0o755));
    assert_eq!(parse_octal_mode("0755"), Some(0o755));
    assert_eq!(parse_octal_mode("0o4755"), Some(0o4755));
    assert_eq!(parse_octal_mode("07777"), Some(0o7777));
    assert_eq!(parse_octal_mode("0648"), None);
    assert_eq!(parse_octal_mode("75"), None);
    assert_eq!(parse_octal_mode("17777"), None);
    assert_eq!(parse_octal_mode(""), None);
}

/// POSIX 符号格式显式呈现 setuid、setgid 和 sticky 的执行位组合。
#[test]
fn formats_special_bits_and_full_octal_value() {
    assert_eq!(format_octal_mode(0o4755), "4755");
    assert_eq!(format_symbolic_mode(0o4755), "rwsr-xr-x");
    assert_eq!(format_symbolic_mode(0o2750), "rwxr-s---");
    assert_eq!(format_symbolic_mode(0o1640), "rw-r----T");
}
