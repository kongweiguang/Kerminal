//! 快捷键录制对 GPUI 规范化修饰键及终端保留键的回归。
//!
//! @author kongweiguang

use super::{is_modifier_key, shortcut_sequence_issue};

/// GPUI 将单独的系统键和 Fn 解析成 platform/function，不能误认作可执行主键。
///
/// @author kongweiguang
#[test]
fn standalone_gpui_modifiers_are_rejected() {
    for key in ["control", "shift", "alt", "platform", "function"] {
        assert!(is_modifier_key(key), "modifier must not be recorded: {key}");
    }
    for input in ["Ctrl", "Shift", "Alt", "Cmd", "Win", "Fn"] {
        assert!(
            shortcut_sequence_issue(input).is_some(),
            "modifier-only input must be rejected: {input}"
        );
    }
    assert!(shortcut_sequence_issue("Ctrl+Shift+F12").is_none());
}
