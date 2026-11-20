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

/// 消歧模式仍使用功能键的指定 CSI 格式，并为冲突的字符组合提供独立编码。
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
        Some(b"\x1b[P".to_vec())
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

/// 验证方向键的默认按下与显式重复、释放能被区分，同时覆盖 Escape 和 Ctrl 字母。
/// @author kongweiguang
#[test]
fn event_types_are_reported_for_navigation_and_modified_text_keys() {
    let event_flags = 0b0001 | 0b0010;
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Press),
            modes(event_flags)
        ),
        Some(b"\x1b[A".to_vec())
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

/// Kitty 的导航键即使全键报告也保留独立 CSI 格式，避免客户端误解键身份。
/// @author kongweiguang
#[test]
fn kitty_navigation_keeps_dedicated_sequences_across_enhancements() {
    for (terminal_key, press, release) in [
        (TerminalKey::Left, "D", "1;5:3D"),
        (TerminalKey::Right, "C", "1;5:3C"),
        (TerminalKey::Up, "A", "1;5:3A"),
        (TerminalKey::Down, "B", "1;5:3B"),
        (TerminalKey::Home, "H", "1;5:3H"),
        (TerminalKey::End, "F", "1;5:3F"),
        (TerminalKey::Insert, "2~", "2;5:3~"),
        (TerminalKey::Delete, "3~", "3;5:3~"),
        (TerminalKey::PageUp, "5~", "5;5:3~"),
        (TerminalKey::PageDown, "6~", "6;5:3~"),
    ] {
        let pressed = key(terminal_key, InputModifiers::default(), KeyPhase::Press);
        for flags in [0b00001, 0b00010, 0b01000, 0b01010] {
            let modes = TerminalModes {
                application_cursor: true,
                ..modes(flags)
            };
            assert_eq!(
                encode_input(&pressed, modes),
                Some(format!("\x1b[{press}").into_bytes()),
                "{terminal_key:?}, flags {flags:05b}"
            );
        }
        let released = key(
            terminal_key,
            InputModifiers {
                control: true,
                ..InputModifiers::default()
            },
            KeyPhase::Release,
        );
        assert_eq!(encode_input(&released, modes(0b01000)), None);
        assert_eq!(
            encode_input(&released, modes(0b01010)),
            Some(format!("\x1b[{release}").into_bytes())
        );
    }
}

/// 全键模式仍按键类型选择专用序列或 CSI-u，并避免额外抄送原始文本。
/// @author kongweiguang
#[test]
fn report_all_uses_dedicated_key_formats_without_leaking_raw_text() {
    let all_with_types = 0b01000 | 0b00010;
    assert_eq!(
        encode_input(
            &key(TerminalKey::Up, InputModifiers::default(), KeyPhase::Press),
            modes(all_with_types)
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
            modes(all_with_types)
        ),
        Some(b"\x1b[P".to_vec())
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

/// Kitty legacy Ctrl 表把 `[ \ ]` 与数字键映射到同一组 VT 控制字节。
///
/// 这些标点不在字母区间内，必须显式映射，否则会以可打印字符发送；
/// 同时确认 Ctrl+Shift 的歧义编码仍走 CSI-u，不会被 VT 字节覆盖。
/// @author kongweiguang
#[test]
fn legacy_ctrl_brackets_map_to_vt_control_bytes() {
    for (character, expected) in [
        ('[', 0x1b),
        ('\\', 0x1c),
        (']', 0x1d),
        ('^', 0x1e),
        ('_', 0x1f),
        ('/', 0x1f),
        ('4', 0x1c),
        ('5', 0x1d),
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
            Some(vec![expected]),
            "Ctrl+{character:?} must use the VT control byte"
        );
    }
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character('['),
                InputModifiers {
                    shift: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press,
            ),
            TerminalModes::default(),
        ),
        Some(b"\x1b[91;6u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Character(']'),
                InputModifiers {
                    alt: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Press,
            ),
            TerminalModes::default(),
        ),
        Some(vec![0x1b, 0x1d])
    );
}

/// DECCKM 只改变未修饰 Home/End 的终结形式；组合键保持 CSI 以便应用区分。
///
/// 方向键继续沿用原有 SS3 行为，Insert/PageUp 等 tilde 键不受该模式影响。
/// @author kongweiguang
#[test]
fn application_cursor_home_end_use_ss3_only_without_modifiers() {
    let application = TerminalModes {
        application_cursor: true,
        ..TerminalModes::default()
    };
    let plain = |terminal_key| key(terminal_key, InputModifiers::default(), KeyPhase::Press);

    assert_eq!(
        encode_input(&plain(TerminalKey::Home), TerminalModes::default()),
        Some(b"\x1b[H".to_vec())
    );
    assert_eq!(
        encode_input(&plain(TerminalKey::End), TerminalModes::default()),
        Some(b"\x1b[F".to_vec())
    );
    assert_eq!(
        encode_input(&plain(TerminalKey::Home), application),
        Some(b"\x1bOH".to_vec())
    );
    assert_eq!(
        encode_input(&plain(TerminalKey::End), application),
        Some(b"\x1bOF".to_vec())
    );

    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };
    let control = InputModifiers {
        control: true,
        ..InputModifiers::default()
    };
    assert_eq!(
        encode_input(&key(TerminalKey::Home, shift, KeyPhase::Press), application),
        Some(b"\x1b[1;2H".to_vec())
    );
    assert_eq!(
        encode_input(&key(TerminalKey::End, shift, KeyPhase::Press), application),
        Some(b"\x1b[1;2F".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(TerminalKey::Home, control, KeyPhase::Press),
            application
        ),
        Some(b"\x1b[1;5H".to_vec())
    );

    assert_eq!(
        encode_input(&plain(TerminalKey::Up), application),
        Some(b"\x1bOA".to_vec())
    );
    assert_eq!(
        encode_input(&plain(TerminalKey::Insert), application),
        Some(b"\x1b[2~".to_vec())
    );
    assert_eq!(
        encode_input(&plain(TerminalKey::PageUp), application),
        Some(b"\x1b[5~".to_vec())
    );
}

/// Kitty progressive 模式用 CSI 13~ 代替会被误认成光标位置报告的 F3 序列。
///
/// 全键模式同样遵循 F3 的专用格式，旧模式保留应用依赖的 SS3 R。
/// @author kongweiguang
#[test]
fn kitty_f3_avoids_cursor_position_report_in_progressive_modes() {
    let f3 = |modifiers: InputModifiers, phase| key(TerminalKey::Function(3), modifiers, phase);
    let control = InputModifiers {
        control: true,
        ..InputModifiers::default()
    };
    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };

    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Press),
            TerminalModes::default()
        ),
        Some(b"\x1bOR".to_vec())
    );
    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Release),
            TerminalModes::default()
        ),
        None
    );

    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Press),
            modes(0b0001)
        ),
        Some(b"\x1b[13~".to_vec())
    );
    assert_eq!(
        encode_input(&f3(shift, KeyPhase::Press), modes(0b0001)),
        Some(b"\x1b[13;2~".to_vec())
    );

    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Press),
            modes(0b0010)
        ),
        Some(b"\x1b[13~".to_vec())
    );
    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Repeat),
            modes(0b0010)
        ),
        Some(b"\x1b[13;1:2~".to_vec())
    );
    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Release),
            modes(0b0010)
        ),
        Some(b"\x1b[13;1:3~".to_vec())
    );
    assert_eq!(
        encode_input(&f3(control, KeyPhase::Press), modes(0b0011)),
        Some(b"\x1b[13;5~".to_vec())
    );
    assert_eq!(
        encode_input(&f3(control, KeyPhase::Release), modes(0b0011)),
        Some(b"\x1b[13;5:3~".to_vec())
    );

    for (number, final_byte) in [(1u8, b'P'), (2, b'Q'), (4, b'S')] {
        assert_eq!(
            encode_input(
                &key(
                    TerminalKey::Function(number),
                    InputModifiers::default(),
                    KeyPhase::Press
                ),
                modes(0b0010)
            ),
            Some(format!("\x1b[{}", char::from(final_byte)).into_bytes())
        );
    }

    assert_eq!(
        encode_input(
            &f3(InputModifiers::default(), KeyPhase::Press),
            modes(0b01000)
        ),
        Some(b"\x1b[13~".to_vec())
    );
    assert_eq!(
        encode_input(&f3(control, KeyPhase::Release), modes(0b01010)),
        Some(b"\x1b[13;5:3~".to_vec())
    );
}

/// 非全键模式下保留重置终端所需的输入语义，连带修饰键也不报告这些键的释放。
/// @author kongweiguang
#[test]
fn tab_enter_and_backspace_release_require_report_all_keys_flag() {
    for (terminal_key, code) in [(TerminalKey::Enter, 13), (TerminalKey::Backspace, 127)] {
        let release = key(terminal_key, InputModifiers::default(), KeyPhase::Release);
        assert_eq!(encode_input(&release, modes(0b0010)), None);
        assert_eq!(
            encode_input(&release, modes(0b1010)),
            Some(format!("\x1b[{code};1:3u").into_bytes())
        );
    }
    let ctrl = InputModifiers {
        control: true,
        ..InputModifiers::default()
    };
    let super_key = InputModifiers {
        super_key: true,
        ..InputModifiers::default()
    };

    assert_eq!(
        encode_input(&key(TerminalKey::Tab, ctrl, KeyPhase::Press), modes(0b0010)),
        Some(b"\x1b[9;5:1u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(TerminalKey::Tab, ctrl, KeyPhase::Release),
            modes(0b0010)
        ),
        None
    );
    assert_eq!(
        encode_input(
            &key(TerminalKey::Tab, super_key, KeyPhase::Release),
            modes(0b0010)
        ),
        None
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::BackTab,
                InputModifiers {
                    shift: true,
                    control: true,
                    ..InputModifiers::default()
                },
                KeyPhase::Release,
            ),
            modes(0b0010)
        ),
        None
    );
    assert_eq!(
        encode_input(
            &key(TerminalKey::Tab, InputModifiers::default(), KeyPhase::Press),
            modes(0b0010)
        ),
        Some(b"\t".to_vec())
    );

    assert_eq!(
        encode_input(
            &key(TerminalKey::Tab, ctrl, KeyPhase::Release),
            modes(0b1010)
        ),
        Some(b"\x1b[9;5:3u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Tab,
                InputModifiers::default(),
                KeyPhase::Release
            ),
            modes(0b1010)
        ),
        Some(b"\x1b[9;1:3u".to_vec())
    );
}

/// disambiguate 模式按 Kitty 功能键表把 F13–F35 提升为 PUA 码，传统 CSI 25~ 只留给 legacy。
///
/// 增强标志组合不应改变功能键身份，F12 仍保留规范的 tilde 编号。
/// @author kongweiguang
#[test]
fn disambiguation_promotes_f13_and_above_to_kitty_pua_codes() {
    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };

    for (number, legacy, pua) in [
        (13u8, b"\x1b[25~".as_slice(), 57376u32),
        (20, b"\x1b[34~".as_slice(), 57383),
        (21, b"\x1b[57384u".as_slice(), 57384),
    ] {
        let press = key(
            TerminalKey::Function(number),
            InputModifiers::default(),
            KeyPhase::Press,
        );
        assert_eq!(
            encode_input(&press, TerminalModes::default()),
            Some(legacy.to_vec())
        );
        for flags in [0b00001, 0b00010, 0b01000, 0b01010] {
            assert_eq!(
                encode_input(&press, modes(flags)),
                Some(format!("\x1b[{pua}u").into_bytes())
            );
        }
    }

    assert_eq!(
        encode_input(
            &key(TerminalKey::Function(13), shift, KeyPhase::Press),
            modes(0b0001)
        ),
        Some(b"\x1b[57376;2u".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Function(12),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(0b0001)
        ),
        Some(b"\x1b[24~".to_vec())
    );
    assert_eq!(
        encode_input(
            &key(
                TerminalKey::Function(12),
                InputModifiers::default(),
                KeyPhase::Press
            ),
            modes(0b0010)
        ),
        Some(b"\x1b[24~".to_vec())
    );
}
