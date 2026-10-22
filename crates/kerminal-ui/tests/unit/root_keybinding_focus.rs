//! 焦点 context 分类和 chord 焦点边界的回归测试。
//!
//! @author kongweiguang

use super::{
    KeybindingFocusTracker, classify_focus_scope, is_text_input_context,
    modal_key_uses_platform_text, pane_key_is_owned_by_ime, suggestion_overlays_visible,
};
use crate::root::keybindings::KeybindingFocusScope;
use gpui_kit::{KeyContext, KeyDownEvent, Keystroke, Modifiers};

/// 平台提交普通字符仍需 WM_CHAR；模态的文本焦点不会因此开放终端或应用快捷键。
#[test]
fn modal_text_fields_allow_native_characters_but_not_application_chords() {
    let text_path = contexts(&["KerminalApp", "Modal", "Input"]);
    let button_path = contexts(&["KerminalApp", "Modal"]);
    for text in ["a", "A", "1", " ", "!", "你"] {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                key: text.to_owned(),
                key_char: Some(text.to_owned()),
                ..Default::default()
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert!(modal_key_uses_platform_text(&text_path, &event));
        assert!(!modal_key_uses_platform_text(&button_path, &event));
    }
    for modifiers in [
        Modifiers {
            control: true,
            ..Default::default()
        },
        Modifiers {
            platform: true,
            ..Default::default()
        },
        Modifiers {
            alt: true,
            ..Default::default()
        },
    ] {
        let event = KeyDownEvent {
            keystroke: Keystroke {
                key: "q".to_owned(),
                key_char: Some("q".to_owned()),
                modifiers,
            },
            is_held: false,
            prefer_character_input: false,
        };
        assert!(!modal_key_uses_platform_text(&text_path, &event));
    }
    for key in ["escape", "tab", "enter"] {
        let event = KeyDownEvent {
            keystroke: Keystroke::parse(key).expect("valid key"),
            is_held: false,
            prefer_character_input: false,
        };
        assert!(!modal_key_uses_platform_text(&text_path, &event));
    }
}

/// 字符优先由平台判断，AltGr 和死键不因 Ctrl/Alt 修饰而误判为快捷键。
#[test]
fn modal_text_fields_preserve_platform_character_preference() {
    let event = KeyDownEvent {
        keystroke: Keystroke {
            key: "e".to_owned(),
            key_char: Some("€".to_owned()),
            modifiers: Modifiers {
                control: true,
                alt: true,
                ..Default::default()
            },
        },
        prefer_character_input: true,
        is_held: false,
    };
    assert!(modal_key_uses_platform_text(&contexts(&["Input"]), &event));
    assert!(!modal_key_uses_platform_text(
        &contexts(&["KerminalTerminal"]),
        &event
    ));
}

/// 从简短的 GPUI context 表达式构造与实际焦点路径相同的测试输入。
fn contexts(source: &[&str]) -> Vec<KeyContext> {
    source
        .iter()
        .map(|context| KeyContext::parse(context).expect("valid key context"))
        .collect()
}

/// Kit Input 必须压过 Terminal/Workspace 祖先，否则搜索和 Agent 文本框会执行快捷键。
#[test]
fn generic_input_context_precedes_terminal_and_workspace() {
    let path = contexts(&[
        "KerminalApp",
        "KerminalWorkspace",
        "KerminalTerminal",
        "Input",
    ]);

    assert_eq!(
        classify_focus_scope(&path, false),
        KeybindingFocusScope::Editor
    );
    assert!(is_text_input_context(&path));
}

/// 应用自有的文本标记和模态控件仍保持保护，并且 Terminal context 只在无输入框时生效。
#[test]
fn app_text_marker_modal_and_terminal_paths_are_distinct() {
    let app_text = contexts(&["KerminalApp", "KerminalTextInput"]);
    let terminal = contexts(&["KerminalApp", "KerminalWorkspace", "KerminalTerminal"]);
    let modal = contexts(&["KerminalApp", "KerminalWorkspace", "Menu"]);

    assert_eq!(
        classify_focus_scope(&app_text, false),
        KeybindingFocusScope::Editor
    );
    assert_eq!(
        classify_focus_scope(&terminal, false),
        KeybindingFocusScope::Terminal
    );
    assert_eq!(
        classify_focus_scope(&modal, false),
        KeybindingFocusScope::Modal
    );
    assert_eq!(
        classify_focus_scope(&terminal, true),
        KeybindingFocusScope::Modal
    );
}

/// 同一焦点保持 chord；控件迁移、失焦和明确 reset 都建立新的输入边界。
#[test]
fn focus_tracker_clears_chord_only_at_identity_boundaries() {
    let mut tracker = KeybindingFocusTracker::default();

    assert!(tracker.observe(Some(10_u8)));
    assert!(!tracker.observe(Some(10_u8)));
    assert!(tracker.observe(Some(11_u8)));
    assert!(tracker.observe(None));
    assert!(!tracker.observe(None));
    tracker.reset();
    assert!(tracker.observe(None));
}

/// IME 预编辑压过菜单候选和终端按键；完成提交后才重新显示绑定候选。
#[test]
fn ime_composition_owns_pane_keys_and_hides_suggestion_overlays() {
    assert!(pane_key_is_owned_by_ime(true));
    assert!(!suggestion_overlays_visible(true, true, true));
    assert!(!pane_key_is_owned_by_ime(false));
    assert!(suggestion_overlays_visible(false, true, false));
    assert!(suggestion_overlays_visible(false, false, true));
    assert!(!suggestion_overlays_visible(false, false, false));
}
