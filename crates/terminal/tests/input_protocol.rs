//! Keyboard protocol contract tests kept outside terminal production sources.
//!
//! @author kongweiguang

use kerminal_terminal::{
    TerminalModes,
    input::{
        InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyAlternates,
        TerminalKeyEvent, encode_input,
    },
};

/// 仅打开测试指定的 Kitty flags，避免默认模式掩盖未协商能力。
/// @author kongweiguang
fn modes(flags: u8) -> TerminalModes {
    TerminalModes {
        kitty_keyboard_flags: flags,
        ..TerminalModes::default()
    }
}

/// 统一构造按键事件，使协议断言只表达按键与 modifier 差异。
/// @author kongweiguang
fn key(key: TerminalKey, modifiers: InputModifiers, phase: KeyPhase) -> TerminalInputEvent {
    TerminalInputEvent::Key(TerminalKeyEvent {
        key,
        modifiers,
        phase,
    })
}

/// Disambiguation 只提升歧义字符，避免把导航键映射成错误的 CSI-u PUA 码。
/// @author kongweiguang
#[test]
fn disambiguation_keeps_canonical_navigation_and_function_sequences() {
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Press),
            modes(1)
        ),
        Some(b"\x1b[A".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Function(1),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(1)
        ),
        Some(b"\x1bOP".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Insert,
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(1)
        ),
        Some(b"\x1b[2~".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character('i'),
                InputModifiers {
                    shift: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press,
            ),
            modes(1),
        ),
        Some(b"\x1b[105;6u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Escape,
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(1)
        ),
        Some(b"\x1b[27u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Function(21),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            TerminalModes::default()
        ),
        Some(b"\x1b[57384u".to_vec())
    );
}

/// Event-type 编码把 :1/:2/:3 放在 modifier 字段后，也覆盖 Escape 的非文本事件。
/// @author kongweiguang
#[test]
fn event_types_are_reported_for_navigation_and_modified_text_keys() {
    let event_flags = 0b0001 | 0b0010;
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Press),
            modes(event_flags)
        ),
        Some(b"\x1b[1;1:1A".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Escape,
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(0b0010)
        ),
        Some(b"\x1b[27;1:1u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Repeat),
            modes(event_flags)
        ),
        Some(b"\x1b[1;1:2A".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Up,
                InputModifiers::default(),
                KeyPhase::Release
            ),
            modes(event_flags)
        ),
        Some(b"\x1b[1;1:3A".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character('c'),
                InputModifiers {
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Release,
            ),
            modes(event_flags),
        ),
        Some(b"\x1b[99;5:3u".to_vec())
    );
}

/// 全键模式使用 Kitty keycode；无事件字段时省略默认 modifier=1，文本仅随 flag 16 发送。
/// @author kongweiguang
#[test]
fn report_all_uses_kitty_codes_and_does_not_leak_raw_text() {
    let all_with_types = 0b01000 | 0b00010;
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Press),
            modes(all_with_types)
        ),
        Some(b"\x1b[57352;1:1u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Function(1),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(all_with_types)
        ),
        Some(b"\x1b[57364;1:1u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Enter,
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(all_with_types)
        ),
        Some(b"\x1b[13;1:1u".to_vec())
    );
    assert_eq!(
        encode_input(&TerminalInputEvent::Text("A".into()), modes(0b01000)),
        None
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character('a'),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(0b01000)
        ),
        Some(b"\x1b[97u".to_vec())
    );
}

/// 按键关联文本与已知备用键码合并为一条 CSI-u，不猜测键盘布局信息。
/// @author kongweiguang
#[test]
fn key_with_text_reports_codepoints_and_available_shifted_key() {
    let event = TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Character('a'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                shift: true,
                ..InputModifiers::default()
            },
        },
        text: "A🙂".into(),
        alternates: TerminalKeyAlternates {
            shifted: Some('A'),
            base_layout: None,
        },
    };
    assert_eq!(
        encode_input(&event, modes(0b01000 | 0b10000 | 0b00100 | 0b00010)),
        Some(b"\x1b[97:65;2:1;65:128578u".to_vec())
    );
    let release = TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Character('a'),
            phase: KeyPhase::Release,
            modifiers: InputModifiers::default(),
        },
        text: "a".into(),
        alternates: TerminalKeyAlternates::default(),
    };
    assert_eq!(
        encode_input(&release, modes(0b01000 | 0b10000 | 0b00010)),
        Some(b"\x1b[97;1:3u".to_vec())
    );
}

/// 独立 IME commit 使用 key=0；协议禁止的 C0/C1 控制字符不会进入文本字段。
/// @author kongweiguang
#[test]
fn text_only_and_associated_text_respect_kitty_codepoint_rules() {
    assert_eq!(
        encode_input(
            &TerminalInputEvent::Text("汉字".into()),
            modes(0b01000 | 0b10000)
        ),
        Some(b"\x1b[0;;27721:23383u".to_vec())
    );
    let event = TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Character('a'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        },
        text: "a\nb".into(),
        alternates: TerminalKeyAlternates::default(),
    };
    assert_eq!(
        encode_input(&event, modes(0b01000 | 0b10000)),
        Some(b"\x1b[97;1;97:98u".to_vec())
    );
    assert_eq!(
        encode_input(&TerminalInputEvent::Text("IME".into()), modes(0b10000)),
        Some(b"IME".to_vec())
    );
}

/// Legacy Ctrl、Alt、Shift 组合保留 VT 字节语义，歧义的 Ctrl+Shift 使用 CSI-u。
/// @author kongweiguang
#[test]
fn legacy_modifiers_follow_vt_and_kitty_mappings() {
    for (character, expected) in [
        ('3', 0x1b),
        ('4', 0x1c),
        ('5', 0x1d),
        ('6', 0x1e),
        ('7', 0x1f),
        ('8', 0x7f),
        ('?', 0x7f),
    ] {
        assert_eq!(
            encode_input(
                &key(
                    TerminalKey::Character(character),
                    InputModifiers {
                        control: true,
                        ..InputModifiers::default()
                    },
                    KeyPhase::Press,
                ),
                TerminalModes::default(),
            ),
            Some(vec![expected])
        );
    }
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character('i'),
                InputModifiers {
                    shift: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press,
            ),
            TerminalModes::default(),
        ),
        Some(b"\x1b[105;6u".to_vec())
    );
    let alt_shift = TerminalInputEvent::KeyWithText {
        event: TerminalKeyEvent {
            key: TerminalKey::Character('a'),
            phase: KeyPhase::Press,
            modifiers: InputModifiers {
                shift: true,
                alt: true,
                ..InputModifiers::default()
            },
        },
        text: "A".into(),
        alternates: TerminalKeyAlternates::default(),
    };
    assert_eq!(
        encode_input(&alt_shift, TerminalModes::default()),
        Some(b"\x1bA".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Tab,
                InputModifiers {
                    shift: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press
            ),
            TerminalModes::default()
        ),
        Some(b"\x1b[Z".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Tab,
                InputModifiers {
                    shift: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press
            ),
            TerminalModes::default()
        ),
        Some(b"\x1b[9;6u".to_vec())
    );
}

/// Legacy modifier disambiguation encodes Unicode scalar values, never the printable glyph itself.
///
/// @author kongweiguang
#[test]
fn legacy_kitty_compatible_character_keys_use_unicode_codepoints() {
    for (character, modifiers, expected) in [
        (
            'i',
            InputModifiers {
                shift: true,
                control: true,
                ..InputModifiers::default()
            },
            b"\x1b[105;6u".to_vec(),
        ),
        (
            'I',
            InputModifiers {
                super_key: true,
                ..InputModifiers::default()
            },
            b"\x1b[73;9u".to_vec(),
        ),
        (
            '汉',
            InputModifiers {
                shift: true,
                control: true,
                ..InputModifiers::default()
            },
            b"\x1b[27721;6u".to_vec(),
        ),
        (
            '🙂',
            InputModifiers {
                super_key: true,
                ..InputModifiers::default()
            },
            b"\x1b[128578;9u".to_vec(),
        ),
    ] {
        assert_eq!(
            encode_input(
                &key(
                    TerminalKey::Character(character),
                    modifiers,
                    KeyPhase::Press
                ),
                TerminalModes::default(),
            ),
            Some(expected),
            "character {character:?} must be encoded as its Unicode codepoint"
        );
    }
}

/// Bracketed paste 保持独立事件，Kitty 全键 flags 不改写粘贴边界。
/// @author kongweiguang
#[test]
fn paste_keeps_bracketed_paste_framing_in_kitty_modes() {
    let event = TerminalInputEvent::Paste("one\n二".into());
    assert_eq!(
        encode_input(
            &event,
            TerminalModes {
                bracketed_paste: true,
                kitty_keyboard_flags: 0b11000,
                ..TerminalModes::default()
            },
        ),
        Some("\x1b[200~one\n二\x1b[201~".as_bytes().to_vec())
    );
}

/// 普通模式把 CRLF/LF 变成一个 Enter，并保留孤立 CR，防止双重换行。
/// @author kongweiguang
#[test]
fn ordinary_paste_normalizes_line_endings_without_duplicate_carriage_returns() {
    let event = TerminalInputEvent::Paste("前\r\n中\n后\r单\r".into());
    assert_eq!(
        encode_input(&event, TerminalModes::default()),
        Some("前\r中\r后\r单\r".as_bytes().to_vec())
    );
}

/// Bracketed paste 保留 Unicode 和原始 CR/LF 组合，由终端应用自行解释粘贴内容。
/// @author kongweiguang
#[test]
fn bracketed_paste_preserves_unicode_and_line_endings() {
    let event = TerminalInputEvent::Paste("汉\r\n字\n🙂\r尾".into());
    assert_eq!(
        encode_input(
            &event,
            TerminalModes {
                bracketed_paste: true,
                ..TerminalModes::default()
            }
        ),
        Some("\x1b[200~汉\r\n字\n🙂\r尾\x1b[201~".as_bytes().to_vec())
    );
}

/// Bracketed payload 移除 ESC 与 ETX，伪造的结束序列不能在真实帧结束前逃逸。
/// @author kongweiguang
#[test]
fn bracketed_paste_filters_escape_and_interrupt_bytes_from_payload() {
    let event = TerminalInputEvent::Paste("前\x1b[201~中\x03后\x1b[200~".into());
    let encoded = encode_input(
        &event,
        TerminalModes {
            bracketed_paste: true,
            ..TerminalModes::default()
        },
    )
    .unwrap();

    assert_eq!(encoded, "\x1b[200~前[201~中后[200~\x1b[201~".as_bytes());
    assert_eq!(
        encoded
            .windows(b"\x1b[201~".len())
            .filter(|window| window.starts_with(b"\x1b[201~"))
            .count(),
        1
    );
}

/// 空粘贴维持既有 Some 语义，bracketed 模式仍发送成对 framing 标记。
/// @author kongweiguang
#[test]
fn empty_paste_keeps_existing_mode_dependent_encoding() {
    assert_eq!(
        encode_input(
            &TerminalInputEvent::Paste(String::new()),
            TerminalModes::default()
        ),
        Some(Vec::new())
    );
    assert_eq!(
        encode_input(
            &TerminalInputEvent::Paste(String::new()),
            TerminalModes {
                bracketed_paste: true,
                ..TerminalModes::default()
            }
        ),
        Some(b"\x1b[200~\x1b[201~".to_vec())
    );
}

/// 普通 Text 仍按原 UTF-8 字节直传，不套用 Paste 的换行和边界过滤规则。
/// @author kongweiguang
#[test]
fn text_input_keeps_raw_bytes_independent_of_bracketed_paste_mode() {
    let text = "汉\r\n字\n\x1b[201~\x03";
    assert_eq!(
        encode_input(
            &TerminalInputEvent::Text(text.to_owned()),
            TerminalModes {
                bracketed_paste: true,
                ..TerminalModes::default()
            }
        ),
        Some(text.as_bytes().to_vec())
    );
}
