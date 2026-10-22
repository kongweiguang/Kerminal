//! 文件编辑器额外粘贴快捷键的输入边界测试。
//!
//! @author kongweiguang

use super::is_shift_insert_paste;
use gpui_kit::{KeyDownEvent, Keystroke};

/// 构造携带 AltGr/IME 优先语义的按键事件，验证是否由编辑器快捷键接管。
///
/// @author kongweiguang
fn key_event(binding: &str, prefer_character_input: bool) -> KeyDownEvent {
    KeyDownEvent {
        keystroke: Keystroke::parse(binding).expect("test keystroke must be valid"),
        is_held: false,
        prefer_character_input,
    }
}

/// 仅纯 Shift+Insert 映射到 Paste；IME 字符、额外修饰键和其它键保持原路由。
///
/// @author kongweiguang
#[test]
fn shift_insert_paste_respects_ime_and_modifier_boundaries() {
    assert!(is_shift_insert_paste(&key_event("shift-insert", false)));
    assert!(!is_shift_insert_paste(&key_event("shift-insert", true)));
    assert!(!is_shift_insert_paste(&key_event(
        "ctrl-shift-insert",
        false
    )));
    assert!(!is_shift_insert_paste(&key_event(
        "alt-shift-insert",
        false
    )));
    assert!(!is_shift_insert_paste(&key_event("shift-delete", false)));
}
