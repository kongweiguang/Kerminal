//! 私有快照的字符索引与有界解码必须在进入会话状态前一致。
//!
//! @author kongweiguang

use kerminal_terminal::shell_editing::{EditingMessage, EditingShell, parse_editing_message};

const NONCE: &str = "0123456789abcdef0123456789abcdef";

/// 同一行横跨 UTF-8、UTF-16 与标量协议时，光标只能落在完整字符边界。
#[test]
fn unicode_cursor_units_resolve_to_the_same_scalar_position() {
    // A、中文、emoji、组合字符都保留原文，协议不擅自做 Unicode 规范化。
    let text = "A中😀e\u{301}";
    let encoded = "A%E4%B8%AD%F0%9F%98%80e%CC%81";
    for (unit, end) in [("byte", text.len()), ("utf16", 6), ("scalar", 5)] {
        let payload =
            format!("1;S;{NONCE};powershell;{unit};{end};7.5;C%3A/space%20dir;{encoded};0");
        let Some(EditingMessage::Snapshot {
            input,
            cursor,
            cwd,
            shell,
            prediction_visible,
            ..
        }) = parse_editing_message(payload.as_bytes())
        else {
            panic!("valid {unit} snapshot rejected")
        };
        assert_eq!(input, text);
        assert_eq!(cursor, 5);
        assert_eq!(cwd.as_deref(), Some("C:/space dir"));
        assert_eq!(shell, EditingShell::PowerShell);
        assert_eq!(prediction_visible, Some(false));
    }
    for (unit, invalid) in [("byte", 2), ("byte", 5), ("utf16", 3), ("scalar", 6)] {
        let payload = format!("1;S;{NONCE};bash;{unit};{invalid};5.2;-;{encoded};?");
        assert!(
            parse_editing_message(payload.as_bytes()).is_none(),
            "{unit}/{invalid}"
        );
    }
}

/// 错误编码、字段注入、超限或未经协商的版本不得建立可接受输入。
#[test]
fn invalid_or_oversized_messages_are_rejected() {
    for payload in [
        format!("2;H;{NONCE};bash;1;5.2"),
        format!("1;H;{NONCE};cmd;1;1"),
        "1;H;short;bash;1;5.2".to_owned(),
        format!("1;H;{NONCE};bash;2;5.2"),
        format!("1;H;{NONCE};bash;1;5.2;extra"),
        format!("1;H;{NONCE};bash;1;%1b"),
        format!("1;S;{NONCE};bash;scalar;0;5.2;/tmp;%FF;0"),
        format!("1;S;{NONCE};bash;scalar;0;5.2;/tmp;%2;0"),
        format!("1;S;{NONCE};bash;scalar;0;5.2;/tmp;unencoded space;0"),
        format!("1;S;{NONCE};bash;scalar;0;5.2;%0A;;0"),
        format!("1;S;{NONCE};bash;scalar;0;5.2;-;;invalid"),
        format!("1;S;{NONCE};bash;scalar;4001;5.2;-;{};0", "x".repeat(4001)),
    ] {
        assert!(parse_editing_message(payload.as_bytes()).is_none());
    }
}

/// 未知目录和预测状态必须保留未知；绑定冲突也不能被解码成已启用。
#[test]
fn capability_and_unknown_fields_keep_their_meaning() {
    let hello = format!("1;H;{NONCE};fish;0;4.0");
    assert!(matches!(
        parse_editing_message(hello.as_bytes()),
        Some(EditingMessage::Hello {
            installed: false,
            shell: EditingShell::Fish,
            ..
        })
    ));
    let snapshot = format!("1;S;{NONCE};zsh;scalar;0;5.9;-;;?");
    assert!(matches!(
        parse_editing_message(snapshot.as_bytes()),
        Some(EditingMessage::Snapshot {
            cwd: None,
            prediction_visible: None,
            cursor: 0,
            ..
        })
    ));
}

/// 超长响应只携带身份和固定原因，不允许附加正文或用未知原因伪装完成探测。
///
/// @author kongweiguang
#[test]
fn oversized_refusal_has_strict_authenticated_envelope_without_input() {
    let refusal = format!("1;R;{NONCE};bash;input_too_long;5.2");
    assert!(parse_editing_message(refusal.as_bytes()).is_some());
    for invalid in [
        "1;R;short;bash;input_too_long;5.2".to_owned(),
        format!("1;R;{NONCE};cmd;input_too_long;5.2"),
        format!("1;R;{NONCE};bash;other;5.2"),
        format!("1;R;{NONCE};bash;input_too_long;%1B"),
        format!("1;R;{NONCE};bash;input_too_long;5.2;private-input"),
    ] {
        assert!(parse_editing_message(invalid.as_bytes()).is_none());
    }
}

/// 长度边界按 Unicode 标量而非 UTF-8 字节或 UTF-16 单元验证，避免拒绝合法 emoji 输入。
///
/// @author kongweiguang
#[test]
fn maximum_snapshot_length_counts_unicode_scalars() {
    for (unit, input, end) in [
        ("scalar", "x".repeat(4_000), 4_000),
        ("byte", "%F0%9F%98%80".repeat(4_000), 16_000),
        ("utf16", "%F0%9F%98%80".repeat(4_000), 8_000),
    ] {
        let message = format!("1;S;{NONCE};powershell;{unit};{end};7.5;-;{input};0");
        assert!(matches!(
            parse_editing_message(message.as_bytes()),
            Some(EditingMessage::Snapshot { cursor: 4_000, .. })
        ));
    }
}
