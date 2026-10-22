//! 终端 keydown 与平台文本通道的分流回归；不冒充真实窗口或 PTY 验收。
//!
//! @author kongweiguang

use kerminal_terminal::input::InputModifiers;

use super::{
    KeyPhase, TerminalClipboardAction, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
    TerminalModes, TerminalViewportScroll, encode_input, terminal_clipboard_action,
    terminal_key_input, terminal_key_release_input, viewport_scroll_action,
    viewport_scroll_action_for_keys,
};

/// 测试直接构造协议事件，避免依赖系统键盘布局掩盖普通字符被误消费的问题。
///
/// @author kongweiguang
fn key(key: TerminalKey, modifiers: InputModifiers) -> TerminalKeyEvent {
    TerminalKeyEvent {
        key,
        phase: KeyPhase::Press,
        modifiers,
    }
}

/// 普通字母、数字、空格和标点必须留给 WM_CHAR/平台 commit，不能向 FIFO 投递无效 Key。
///
/// @author kongweiguang
#[test]
fn legacy_printable_keys_remain_available_to_platform_text() {
    for character in ['e', 'f', '0', ' ', '.', '/', '_'] {
        assert!(
            terminal_key_input(
                key(TerminalKey::Character(character), InputModifiers::default()),
                TerminalModes::default(),
                Some(&character.to_string()),
            )
            .is_none(),
            "ordinary character must not suppress the platform text callback"
        );
    }
    assert!(
        terminal_key_input(
            key(
                TerminalKey::Character('a'),
                InputModifiers {
                    shift: true,
                    ..InputModifiers::default()
                },
            ),
            TerminalModes::default(),
            Some("A"),
        )
        .is_none()
    );
}

/// 控制键和终端导航键仍须消费，防止平台再提交回车等文本造成双投。
///
/// @author kongweiguang
#[test]
fn encoded_control_and_navigation_keys_remain_terminal_owned() {
    let modes = TerminalModes::default();
    for (event, expected) in [
        (
            key(TerminalKey::Enter, InputModifiers::default()),
            b"\r".as_slice(),
        ),
        (
            key(TerminalKey::Up, InputModifiers::default()),
            b"\x1b[A".as_slice(),
        ),
        (
            key(
                TerminalKey::Character('c'),
                InputModifiers {
                    control: true,
                    ..InputModifiers::default()
                },
            ),
            b"\x03".as_slice(),
        ),
    ] {
        let input =
            terminal_key_input(event, modes, None).expect("encoded key stays terminal-owned");
        assert_eq!(encode_input(&input, modes).as_deref(), Some(expected));
    }
}

/// Kitty 全键模式由同一编码器接管；只有带真实关联文本的事件才登记重复 commit 抑制。
///
/// @author kongweiguang
#[test]
fn kitty_report_all_retains_one_bound_key_with_associated_text() {
    let modes = TerminalModes {
        kitty_keyboard_flags: 0b1_1000,
        ..TerminalModes::default()
    };
    let input = terminal_key_input(
        key(TerminalKey::Character('a'), InputModifiers::default()),
        modes,
        Some("A"),
    )
    .expect("Kitty report-all must not fall through to raw text");
    assert!(matches!(&input, TerminalInputEvent::KeyWithText { text, .. } if text == "A"));
    assert!(encode_input(&input, modes).is_some());
}

/// 普通文本只沿 commit 通道产生一次字节；模拟分流而非发送任何窗口输入。
///
/// @author kongweiguang
#[test]
fn legacy_key_then_plain_commit_produces_text_once() {
    let modes = TerminalModes::default();
    let mut bytes = Vec::new();
    if let Some(input) = terminal_key_input(
        key(TerminalKey::Character('e'), InputModifiers::default()),
        modes,
        Some("e"),
    ) {
        bytes.extend(encode_input(&input, modes).expect("returned input encodes"));
    } else {
        bytes.extend(encode_input(&TerminalInputEvent::Text("e".to_owned()), modes).unwrap());
    }
    assert_eq!(bytes, b"e");
}

/// 所有 Kitty flags 共用协议编码结果，不因 UI 单独判断“可打印”而丢失全键事件。
///
/// @author kongweiguang
#[test]
fn key_consumption_follows_encoder_across_all_kitty_flags() {
    for flags in 0..32 {
        let modes = TerminalModes {
            kitty_keyboard_flags: flags,
            ..TerminalModes::default()
        };
        for mut event in [
            key(TerminalKey::Character('a'), InputModifiers::default()),
            key(TerminalKey::Enter, InputModifiers::default()),
            key(
                TerminalKey::Character('a'),
                InputModifiers {
                    control: true,
                    alt: true,
                    ..InputModifiers::default()
                },
            ),
        ] {
            for phase in [KeyPhase::Press, KeyPhase::Repeat, KeyPhase::Release] {
                event.phase = phase;
                let expected = encode_input(&TerminalInputEvent::Key(event), modes);
                let routed = terminal_key_input(event, modes, None);
                assert_eq!(routed.is_some(), expected.is_some());
                assert_eq!(
                    routed.and_then(|input| encode_input(&input, modes)),
                    expected
                );
            }
        }
    }
}

/// key-up 仅在编码器会产生真实协议字节时进入 FIFO，且 Kitty 事件类型报告仍可发送 release。
///
/// @author kongweiguang
#[test]
fn key_release_is_routed_only_when_the_terminal_protocol_encodes_it() {
    let modifiers = InputModifiers::default();
    assert!(
        terminal_key_release_input(TerminalKey::Up, modifiers, TerminalModes::default(),).is_none()
    );

    let kitty_modes = TerminalModes {
        kitty_keyboard_flags: 0b00010,
        ..TerminalModes::default()
    };
    let release = terminal_key_release_input(TerminalKey::Up, modifiers, kitty_modes)
        .expect("REPORT_EVENT_TYPES encodes a functional key release");
    assert!(encode_input(&release, kitty_modes).is_some());
}

/// Ctrl+C 有选择时复制、无选择时中断；Ctrl+Shift+C/V 固定保留剪贴板语义。
///
/// @author kongweiguang
#[test]
fn clipboard_shortcuts_route_selection_and_interrupt_without_selection() {
    let control = InputModifiers {
        control: true,
        ..InputModifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action(TerminalKey::Character('c'), control, false),
        Some(TerminalClipboardAction::Interrupt)
    );
    assert_eq!(
        terminal_clipboard_action(TerminalKey::Character('c'), control, true),
        Some(TerminalClipboardAction::CopySelection)
    );
    let control_shift = InputModifiers {
        control: true,
        shift: true,
        ..InputModifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action(TerminalKey::Character('C'), control_shift, false),
        Some(TerminalClipboardAction::CopySelection)
    );
    assert_eq!(
        terminal_clipboard_action(TerminalKey::Character('v'), control_shift, false),
        Some(TerminalClipboardAction::Paste)
    );
    assert_eq!(
        terminal_clipboard_action(
            TerminalKey::Character('c'),
            InputModifiers {
                control: true,
                alt: true,
                ..InputModifiers::default()
            },
            true,
        ),
        None
    );
}

/// 重复翻页使用相对步长，顶部/底部快捷键使用 Root 的明确绝对边界哨兵。
///
/// @author kongweiguang
#[test]
fn viewport_shortcuts_page_and_jump_only_on_primary_screen() {
    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };
    assert_eq!(
        viewport_scroll_action(TerminalKey::PageUp, shift, TerminalModes::default(), 24),
        Some(TerminalViewportScroll::Relative(24))
    );
    assert_eq!(
        viewport_scroll_action(TerminalKey::PageDown, shift, TerminalModes::default(), 24),
        Some(TerminalViewportScroll::Relative(-24))
    );
    let control_shift = InputModifiers {
        control: true,
        shift: true,
        ..InputModifiers::default()
    };
    assert_eq!(
        viewport_scroll_action(
            TerminalKey::Home,
            control_shift,
            TerminalModes::default(),
            24,
        ),
        Some(TerminalViewportScroll::Absolute(usize::MAX))
    );
    assert_eq!(
        viewport_scroll_action(
            TerminalKey::End,
            control_shift,
            TerminalModes::default(),
            24,
        ),
        Some(TerminalViewportScroll::Absolute(0))
    );
    assert_eq!(
        viewport_scroll_action(
            TerminalKey::PageUp,
            shift,
            TerminalModes {
                alternate_screen: true,
                ..TerminalModes::default()
            },
            24,
        ),
        None
    );
    assert_eq!(
        viewport_scroll_action(
            TerminalKey::PageUp,
            shift,
            TerminalModes {
                kitty_keyboard_flags: 1,
                ..TerminalModes::default()
            },
            24,
        ),
        None
    );
}

/// 历史 pane 没有 live TUI 输入目标，退出前的 alternate/Kitty 模式不能阻断本地翻页和跳转。
///
/// @author kongweiguang
#[test]
fn history_viewport_shortcuts_ignore_retired_tui_modes() {
    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };
    let control_shift = InputModifiers {
        control: true,
        shift: true,
        ..InputModifiers::default()
    };

    assert_eq!(
        viewport_scroll_action_for_keys(TerminalKey::PageUp, shift, 24),
        Some(TerminalViewportScroll::Relative(24))
    );
    assert_eq!(
        viewport_scroll_action_for_keys(TerminalKey::PageDown, shift, 24),
        Some(TerminalViewportScroll::Relative(-24))
    );
    assert_eq!(
        viewport_scroll_action_for_keys(TerminalKey::Home, control_shift, 24),
        Some(TerminalViewportScroll::Absolute(usize::MAX))
    );
    assert_eq!(
        viewport_scroll_action_for_keys(TerminalKey::End, control_shift, 24),
        Some(TerminalViewportScroll::Absolute(0))
    );
    assert_eq!(
        viewport_scroll_action_for_keys(TerminalKey::PageUp, InputModifiers::default(), 24),
        None
    );
}
