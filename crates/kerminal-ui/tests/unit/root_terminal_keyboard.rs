//! 终端 keydown 与平台文本通道的分流回归；不冒充真实窗口或 PTY 验收。
//!
//! @author kongweiguang

use gpui_kit::{KeyDownEvent, Keystroke, Modifiers};
use kerminal_terminal::input::InputModifiers;

// Ctrl+Insert 的覆盖优先级只在非 macOS 测试中断言，导入沿用同一平台条件。
#[cfg(not(target_os = "macos"))]
use super::ctrl_insert_override_wins;
use super::{
    KeyPhase, TerminalClipboardAction, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
    TerminalModes, TerminalViewportScroll, consumed_terminal_shortcut_repeat, encode_input,
    platform_preferred_terminal_text, resolve_ctrl_insert_override, resolve_terminal_shortcut,
    terminal_clipboard_action_for_platform, terminal_clipboard_shortcut, terminal_key_input,
    terminal_key_release_input, viewport_scroll_action, viewport_scroll_action_for_keys,
};

/// 右栏终端的复制、搜索和粘贴仍归原会话，中央布局动作只有中央 pane 可承接。
#[test]
fn agent_terminal_cannot_close_or_split_the_central_workspace() {
    use super::{KeybindingAction, SessionOwner, terminal_action_can_target_workspace};
    use kerminal_domain::identity::{AgentSessionId, PaneId, TabId};

    let agent = SessionOwner::Agent {
        agent_session_id: AgentSessionId::parse("agent-test").unwrap(),
    };
    let workspace = SessionOwner::Ui {
        tab_id: TabId::parse("tab-test").unwrap(),
        pane_id: PaneId::parse("pane-test").unwrap(),
    };
    for action in [
        KeybindingAction::TerminalCloseTab,
        KeybindingAction::TerminalClosePane,
        KeybindingAction::TerminalSplitHorizontal,
        KeybindingAction::TerminalSplitVertical,
    ] {
        assert!(!terminal_action_can_target_workspace(action, &agent));
        assert!(terminal_action_can_target_workspace(action, &workspace));
    }
    for action in [
        KeybindingAction::TerminalSearch,
        KeybindingAction::TerminalPaste,
    ] {
        assert!(terminal_action_can_target_workspace(action, &agent));
    }
}

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
    event.keystroke.key_char = Some("@".into());
    assert_eq!(platform_preferred_terminal_text(&event), Some("@"));
    event.keystroke.key_char = Some("\u{11}".into());
    assert_eq!(platform_preferred_terminal_text(&event), None);
    event.keystroke.key_char = Some("@".into());
    event.prefer_character_input = false;
    assert_eq!(platform_preferred_terminal_text(&event), None);
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
    let control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Character('c'), control, false, false),
        Some(TerminalClipboardAction::Interrupt)
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Character('c'), control, true, false),
        Some(TerminalClipboardAction::CopySelection)
    );
    let control_shift = Modifiers {
        control: true,
        shift: true,
        ..Modifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action_for_platform(
            TerminalKey::Character('C'),
            control_shift,
            false,
            false,
        ),
        Some(TerminalClipboardAction::CopySelection)
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(
            TerminalKey::Character('v'),
            control_shift,
            false,
            false,
        ),
        None
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(
            TerminalKey::Character('c'),
            Modifiers {
                control: true,
                alt: true,
                ..Modifiers::default()
            },
            true,
            false,
        ),
        None
    );
}

/// 构造单个普通 keydown；特判测试只关心 Keystroke 和平台优先级标记。
///
/// @author kongweiguang
fn key_down(text: &str) -> KeyDownEvent {
    KeyDownEvent {
        keystroke: Keystroke::parse(text).unwrap(),
        is_held: false,
        prefer_character_input: false,
    }
}

/// 把粘贴动作在三套平台字段上都设为同一序列，避免测试随宿主平台漂移。
///
/// @author kongweiguang
#[cfg(not(target_os = "macos"))]
fn settings_with_terminal_paste(
    sequence: &str,
) -> kerminal_application::preferences::SettingsSnapshot {
    let mut settings = kerminal_application::preferences::SettingsSnapshot::default();
    let paste = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.paste")
        .unwrap();
    paste.windows_binding = sequence.to_owned();
    paste.mac_binding = sequence.to_owned();
    paste.linux_binding = Some(sequence.to_owned());
    settings
}

/// 用户显式配置的 Ctrl+Insert 先经现有 keymap 解析；命中时跳过默认复制，未命中才复制。
///
/// @author kongweiguang
#[cfg(not(target_os = "macos"))]
#[test]
fn configured_ctrl_insert_precedes_the_default_copy_gesture() {
    let event = key_down("ctrl-insert");
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&kerminal_application::preferences::SettingsSnapshot::default().keybindings);
    let unconfigured =
        resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &event, false);
    assert_eq!(
        unconfigured,
        Some(super::keybindings::KeybindingDispatch::Unhandled)
    );
    assert!(!ctrl_insert_override_wins(unconfigured.as_ref()));
    assert_eq!(
        terminal_clipboard_shortcut(TerminalKey::Insert, &event, true, false),
        Some(TerminalClipboardAction::CopySelection)
    );

    let settings = settings_with_terminal_paste("Ctrl+Insert");
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    let configured = resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &event, false);
    assert_eq!(
        configured,
        Some(super::keybindings::KeybindingDispatch::Action(
            crate::keybindings::KeybindingAction::TerminalPaste
        ))
    );
    assert!(ctrl_insert_override_wins(configured.as_ref()));
}

/// Ctrl+Insert 作为已配置多段序列的后一段或前缀时都必须压过默认复制。
///
/// @author kongweiguang
#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_insert_keeps_configured_sequence_priority_over_local_copy() {
    let settings = settings_with_terminal_paste("Ctrl+K Ctrl+Insert");
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &key_down("ctrl-k"), false),
        super::keybindings::KeybindingDispatch::Pending
    );
    let second = resolve_ctrl_insert_override(
        &mut runtime,
        TerminalKey::Insert,
        &key_down("ctrl-insert"),
        false,
    );
    assert_eq!(
        second,
        Some(super::keybindings::KeybindingDispatch::Action(
            crate::keybindings::KeybindingAction::TerminalPaste
        ))
    );
    assert!(ctrl_insert_override_wins(second.as_ref()));

    // Ctrl+Insert 也可以是前缀：此时必须保持等待下一段，不能先复制选择。
    let settings = settings_with_terminal_paste("Ctrl+Insert Ctrl+V");
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    runtime.update(&settings.keybindings);
    let prefix = resolve_ctrl_insert_override(
        &mut runtime,
        TerminalKey::Insert,
        &key_down("ctrl-insert"),
        false,
    );
    assert_eq!(
        prefix,
        Some(super::keybindings::KeybindingDispatch::Pending)
    );
    assert!(ctrl_insert_override_wins(prefix.as_ref()));
}

/// 只有大小写/输入法无关的精确物理 Ctrl+Insert 才进入 keymap 优先判断。
///
/// @author kongweiguang
#[test]
fn ctrl_insert_override_only_accepts_exact_physical_shortcut() {
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    let event = key_down("ctrl-insert");
    for modifiers in [
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            platform: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            function: true,
            ..Modifiers::default()
        },
        Modifiers {
            platform: true,
            ..Modifiers::default()
        },
    ] {
        let mut mixed = event.clone();
        mixed.keystroke.modifiers = modifiers;
        assert_eq!(
            resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &mixed, false),
            None,
            "mixed modifiers must keep their original routing"
        );
    }

    let mut altgr = event.clone();
    altgr.prefer_character_input = true;
    assert_eq!(
        resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &altgr, false),
        None
    );
    assert_eq!(
        resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &event, true),
        None
    );
    // Ctrl+C 仍由原有剪贴板/中断分支处理，不进入 Ctrl+Insert 特判。
    assert_eq!(
        resolve_ctrl_insert_override(&mut runtime, TerminalKey::Character('c'), &event, false),
        None
    );
    if cfg!(target_os = "macos") {
        assert_eq!(
            resolve_ctrl_insert_override(&mut runtime, TerminalKey::Insert, &event, false),
            None
        );
    }
}

/// Zed/Windows Terminal 在 Windows/Linux 用 Ctrl+Insert 复制；mac 与混合修饰不得触发。
///
/// 无选择时仍返回复制动作，由选择工具以 None 结束，从而消费按键但不改剪贴板、不写终端。
///
/// @author kongweiguang
#[test]
fn ctrl_insert_is_a_windows_linux_copy_shortcut_only() {
    let control = Modifiers {
        control: true,
        ..Modifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Insert, control, false, false),
        Some(TerminalClipboardAction::CopySelection)
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Insert, control, true, false),
        Some(TerminalClipboardAction::CopySelection)
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Insert, control, true, true),
        None
    );
    for modifiers in [
        Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            platform: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            function: true,
            ..Modifiers::default()
        },
        Modifiers {
            control: true,
            alt: true,
            ..Modifiers::default()
        },
        Modifiers::default(),
    ] {
        assert_eq!(
            terminal_clipboard_action_for_platform(TerminalKey::Insert, modifiers, true, false),
            None,
            "mixed modifiers must not trigger the local copy shortcut"
        );
    }
}

/// 剪贴板只看物理修饰键：mac Option-as-Meta 关闭后 Cmd+Option+C 仍须排除；
/// Win+C 以及 Win/Ctrl/Shift/Fn 的混合组合不能误触发本地复制。
///
/// @author kongweiguang
#[test]
fn clipboard_uses_raw_physical_modifiers_without_mixed_shortcuts() {
    let command = Modifiers {
        platform: true,
        ..Modifiers::default()
    };
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Character('c'), command, false, true,),
        Some(TerminalClipboardAction::CopySelection)
    );
    assert_eq!(
        terminal_clipboard_action_for_platform(TerminalKey::Character('c'), command, false, false,),
        None,
        "Win+C is a system shortcut, not a terminal copy"
    );
    for modifiers in [
        Modifiers {
            platform: true,
            alt: true,
            ..Modifiers::default()
        },
        Modifiers {
            platform: true,
            control: true,
            ..Modifiers::default()
        },
        Modifiers {
            platform: true,
            shift: true,
            ..Modifiers::default()
        },
        Modifiers {
            platform: true,
            function: true,
            ..Modifiers::default()
        },
    ] {
        assert_eq!(
            terminal_clipboard_action_for_platform(
                TerminalKey::Character('c'),
                modifiers,
                true,
                true,
            ),
            None,
            "macOS copy must use exact raw Cmd+C modifiers"
        );
    }
}

/// IME 组合或 AltGr 声明字符优先时，即使物理组合像复制也必须交还文本通道。
///
/// @author kongweiguang
#[test]
fn clipboard_shortcut_honors_character_input_and_ime_gate() {
    let ctrl_insert = KeyDownEvent {
        keystroke: Keystroke::parse("ctrl-insert").unwrap(),
        is_held: false,
        prefer_character_input: false,
    };
    assert_eq!(
        terminal_clipboard_shortcut(TerminalKey::Insert, &ctrl_insert, true, false),
        terminal_clipboard_action_for_platform(
            TerminalKey::Insert,
            ctrl_insert.keystroke.modifiers,
            true,
            cfg!(target_os = "macos"),
        )
    );
    let mut gated = ctrl_insert.clone();
    gated.prefer_character_input = true;
    assert_eq!(
        terminal_clipboard_shortcut(TerminalKey::Insert, &gated, true, false),
        None
    );
    assert_eq!(
        terminal_clipboard_shortcut(TerminalKey::Insert, &ctrl_insert, true, true),
        None
    );

    let ctrl_c = KeyDownEvent {
        keystroke: Keystroke::parse("ctrl-c").unwrap(),
        is_held: false,
        prefer_character_input: false,
    };
    assert_eq!(
        terminal_clipboard_shortcut(TerminalKey::Character('c'), &ctrl_c, false, false),
        Some(TerminalClipboardAction::Interrupt)
    );
}

/// 本地复制 press 登记物理主键后，修饰键先松开的重复与 keyup 都按已消费处理。
///
/// @author kongweiguang
#[test]
fn local_copy_repeat_tracking_survives_modifier_release() {
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    let press = KeyDownEvent {
        keystroke: Keystroke::parse("ctrl-insert").unwrap(),
        is_held: false,
        prefer_character_input: false,
    };
    runtime.remember_terminal_shortcut_key(&press.keystroke);
    let repeat_without_control = KeyDownEvent {
        keystroke: Keystroke::parse("insert").unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &repeat_without_control, false),
        super::keybindings::KeybindingDispatch::Pending
    );
    assert!(runtime.release_terminal_shortcut_key(&repeat_without_control.keystroke));
    assert_eq!(
        resolve_terminal_shortcut(&mut runtime, &repeat_without_control, false),
        super::keybindings::KeybindingDispatch::Unhandled
    );
}

/// 已消费的复制/粘贴长按必须先于动作重算被识别：选择消失后的 Ctrl+C 不能改判成
/// Interrupt；自定义的 Shift+Insert 在 Shift 先松开时也不能从更早的剪贴板或滚动分支漏进终端。
///
/// @author kongweiguang
#[test]
fn consumed_copy_and_paste_repeats_cannot_recompute_into_other_actions() {
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    let copy_press = Keystroke::parse("ctrl-c").unwrap();
    runtime.remember_terminal_shortcut_key(&copy_press);
    let copy_repeat = KeyDownEvent {
        keystroke: copy_press,
        is_held: true,
        prefer_character_input: false,
    };
    // 选择消失后原始组合会重算为 Interrupt，但 tracking 必须仍然优先。
    assert_eq!(
        terminal_clipboard_action_for_platform(
            TerminalKey::Character('c'),
            copy_repeat.keystroke.modifiers,
            false,
            false,
        ),
        Some(TerminalClipboardAction::Interrupt)
    );
    assert!(consumed_terminal_shortcut_repeat(&runtime, &copy_repeat));

    let mut paste_runtime = super::keybindings::RootKeybindingRuntime::default();
    paste_runtime.remember_terminal_shortcut_key(&Keystroke::parse("shift-insert").unwrap());
    let paste_repeat = KeyDownEvent {
        keystroke: Keystroke::parse("insert").unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    assert!(consumed_terminal_shortcut_repeat(
        &paste_runtime,
        &paste_repeat
    ));
    assert_eq!(
        terminal_clipboard_action_for_platform(
            TerminalKey::Insert,
            paste_repeat.keystroke.modifiers,
            true,
            false,
        ),
        None
    );

    let untracked_repeat = KeyDownEvent {
        keystroke: Keystroke::parse("ctrl-c").unwrap(),
        is_held: true,
        prefer_character_input: false,
    };
    assert!(!consumed_terminal_shortcut_repeat(
        &super::keybindings::RootKeybindingRuntime::default(),
        &untracked_repeat,
    ));
    let mut prefer_character_input = copy_repeat.clone();
    prefer_character_input.prefer_character_input = true;
    assert!(!consumed_terminal_shortcut_repeat(
        &runtime,
        &prefer_character_input
    ));
}

/// 本地滚动保持连续重复：模式与修饰键仍匹配时继续滚动；Kitty/alternate 切换后
/// 同一个 tracked 主键由更早的重复门禁吞掉，keyup 再清除记录。
///
/// @author kongweiguang
#[test]
fn tracked_scroll_repeats_keep_scrolling_and_swallow_orphans() {
    let mut runtime = super::keybindings::RootKeybindingRuntime::default();
    let press = Keystroke::parse("shift-pageup").unwrap();
    runtime.remember_terminal_shortcut_key(&press);
    let repeat = KeyDownEvent {
        keystroke: press.clone(),
        is_held: true,
        prefer_character_input: false,
    };
    let shift = InputModifiers {
        shift: true,
        ..InputModifiers::default()
    };
    assert!(consumed_terminal_shortcut_repeat(&runtime, &repeat));
    assert!(!runtime.is_terminal_scroll_key(&press));
    runtime.remember_terminal_scroll_key(&press);
    assert!(runtime.is_terminal_scroll_key(&press));
    assert_eq!(
        viewport_scroll_action(TerminalKey::PageUp, shift, TerminalModes::default(), 24),
        Some(TerminalViewportScroll::Relative(24))
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
    assert!(runtime.release_terminal_shortcut_key(&press));
    assert!(!consumed_terminal_shortcut_repeat(&runtime, &repeat));
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
    // 保留原有键位，Shift+方向键和 Shift+Home/End 由终端程序处理。
    for key in [
        TerminalKey::Up,
        TerminalKey::Down,
        TerminalKey::Home,
        TerminalKey::End,
    ] {
        assert_eq!(
            viewport_scroll_action(key, shift, TerminalModes::default(), 24),
            None
        );
    }
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
