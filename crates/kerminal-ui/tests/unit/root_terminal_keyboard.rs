//! 终端 keydown 与平台文本通道的分流回归；不冒充真实窗口或 PTY 验收。
//!
//! @author kongweiguang

use gpui_kit::{KeyDownEvent, Keystroke};
use kerminal_terminal::input::InputModifiers;

use super::{
    KeyPhase, TerminalClipboardAction, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
    TerminalModes, TerminalViewportScroll, encode_input, resolve_terminal_shortcut,
    terminal_clipboard_action, terminal_key_input, terminal_key_release_input,
    viewport_scroll_action, viewport_scroll_action_for_keys,
};

/// 用户修改后只有新粘贴键在终端解析器生效，旧键不再被本地剪贴板手势抢占。
///
/// @author kongweiguang
#[test]
fn configured_paste_replaces_the_previous_terminal_shortcut() {
    let mut settings = kerminal_application::preferences::SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .unwrap();
    let old = if cfg!(target_os = "macos") {
        paste.mac_binding = "Cmd+Shift+F12".to_owned();
        "cmd-v"
    } else if cfg!(target_os = "windows") {
        paste.windows_binding = "Ctrl+Shift+F12".to_owned();
        "ctrl-shift-v"
    } else {
        paste.linux_binding = Some("Ctrl+Shift+F12".to_owned());
        "ctrl-shift-v"
    };
    let new = if cfg!(target_os = "macos") {
        "cmd-shift-f12"
    } else {
        "ctrl-shift-f12"
    };
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    let event = |text| KeyDownEvent {
        keystroke: Keystroke::parse(text).unwrap(),
        is_held: false,
        prefer_character_input: false,
    };
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event(old), false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event(new), false),
        super::keybindings::KeybindingDispatch::Action(
            crate::keybindings::KeybindingAction::TerminalPaste
        )
    );
}

/// 真实平台声明字符输入优先时，AltGr 按键不会触发匹配的应用快捷键。
///
/// @author kongweiguang
#[test]
fn character_preference_keeps_altgr_in_the_terminal_input_path() {
    let mut settings = kerminal_application::preferences::SettingsSnapshot::default();
    let binding = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.search")
        .unwrap();
    binding.windows_binding = "Ctrl+Alt+E".to_owned();
    binding.mac_binding = "Ctrl+Alt+E".to_owned();
    binding.linux_binding = Some("Ctrl+Alt+E".to_owned());
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    let mut event = KeyDownEvent {
        keystroke: Keystroke::parse("ctrl-alt-e").unwrap(),
        is_held: false,
        prefer_character_input: true,
    };
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event, false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    event.prefer_character_input = false;
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event, false),
        super::keybindings::KeybindingDispatch::Action(
            crate::keybindings::KeybindingAction::TerminalSearch
        )
    );
}

/// 已消费的 chord 每段只执行一次，长按和修饰键先松开都不得泄漏到 PTY。
///
/// @author kongweiguang
#[test]
fn consumed_terminal_shortcut_repeats_are_swallowed_until_key_release() {
    let mut settings = kerminal_application::preferences::SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .unwrap();
    paste.windows_binding = "Ctrl+K Ctrl+V".to_owned();
    paste.mac_binding = "Ctrl+K Ctrl+V".to_owned();
    paste.linux_binding = Some("Ctrl+K Ctrl+V".to_owned());
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    let event = |key: &str, is_held| KeyDownEvent {
        keystroke: Keystroke::parse(key).unwrap(),
        is_held,
        prefer_character_input: false,
    };
    let first = event("ctrl-k", false);
    let last = event("ctrl-v", false);
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &first, false),
        super::keybindings::KeybindingDispatch::Pending
    );
    runtime.remember_terminal_shortcut_key(&first.keystroke);
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event("ctrl-k", true), false),
        super::keybindings::KeybindingDispatch::Pending
    );
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &last, false),
        super::keybindings::KeybindingDispatch::Action(
            crate::keybindings::KeybindingAction::TerminalPaste
        )
    );
    runtime.remember_terminal_shortcut_key(&last.keystroke);
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event("v", true), false),
        super::keybindings::KeybindingDispatch::Pending
    );
    assert!(runtime.release_terminal_shortcut_key(&Keystroke::parse("v").unwrap()));
    assert!(!runtime.release_terminal_shortcut_key(&Keystroke::parse("v").unwrap()));
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event("v", true), false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event("ctrl-x", true), false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
}

/// 焦点和配置变更清除旧终端的已消费按键，防止吞掉新目标的普通重复输入。
///
/// @author kongweiguang
#[test]
fn terminal_shortcut_repeat_tracking_is_reset_by_focus_and_configuration() {
    let settings = kerminal_application::preferences::SettingsSnapshot::default();
    let key = Keystroke::parse("ctrl-shift-v").unwrap();
    let event = KeyDownEvent {
        keystroke: key.clone(),
        is_held: true,
        prefer_character_input: false,
    };
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.remember_terminal_shortcut_key(&key);
    runtime.clear_terminal_shortcut_keys();
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event, false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    runtime.remember_terminal_shortcut_key(&key);
    runtime.update(&settings.keybindings);
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &event, false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    runtime.remember_terminal_shortcut_key(&key);
    let mut altgr = event.clone();
    altgr.prefer_character_input = true;
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &altgr, false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
    assert!(!runtime.release_terminal_shortcut_key(&key));
}

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

/// 控制键、Tab 补全与终端导航键仍须消费，防止焦点导航或平台文本造成双投。
///
/// @author kongweiguang
#[test]
fn encoded_control_and_navigation_keys_remain_terminal_owned() {
    let modes = TerminalModes::default();
    for (event, expected) in [
        (
            key(TerminalKey::Tab, InputModifiers::default()),
            b"\t".as_slice(),
        ),
        (
            key(
                TerminalKey::Tab,
                InputModifiers {
                    shift: true,
                    ..InputModifiers::default()
                },
            ),
            b"\x1b[Z".as_slice(),
        ),
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

/// Ctrl+C 有选择时复制、无选择时中断；粘贴键必须交由用户配置的 keymap 解析。
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
        None
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
