//! 快捷键录制对 GPUI 规范化修饰键及终端保留键的回归。
//!
//! @author kongweiguang

use super::shortcut_sequence_issue;
use crate::keybindings::KeybindingPlatform::{Linux, Mac, Windows};

/// GPUI 将单独的系统键和 Fn 解析成 platform/function，不能误认作可执行主键。
///
/// @author kongweiguang
#[test]
fn standalone_gpui_modifiers_are_rejected() {
    for input in ["Ctrl", "Shift", "Alt", "Cmd", "Win", "Fn"] {
        assert!(
            shortcut_sequence_issue(input, Windows).is_some(),
            "modifier-only input must be rejected: {input}"
        );
    }
    assert!(shortcut_sequence_issue("Ctrl+Shift+F12", Windows).is_none());
}

/// Ctrl+Insert 在 Windows/Linux 可录制：没有匹配配置时才用默认复制，配置命中时优先。
/// Ctrl+C、Ctrl+Shift+C 与 macOS 的 Cmd+C 仍由终端复制/中断先消费。
///
/// @author kongweiguang
#[test]
fn ctrl_insert_is_recordable_on_windows_and_linux() {
    for platform in [Windows, Linux, Mac] {
        assert!(
            shortcut_sequence_issue("Ctrl+Insert", platform).is_none(),
            "Ctrl+Insert must stay configurable beside the default copy gesture"
        );
        assert!(shortcut_sequence_issue("Shift+Insert", platform).is_none());
    }
    for platform in [Windows, Linux, Mac] {
        assert!(shortcut_sequence_issue("Ctrl+C", platform).is_some());
        assert!(shortcut_sequence_issue("Ctrl+Shift+C", platform).is_some());
    }
    assert!(shortcut_sequence_issue("Cmd+C", Mac).is_some());
}

/// 编辑键仍可用于已有配置；录制控件的 Tab 导航规则不用于拒绝用户保存。
///
/// @author kongweiguang
#[test]
fn existing_editing_keys_are_not_rejected() {
    for input in [
        "Tab",
        "Shift+Tab",
        "Enter",
        "Space",
        "Backspace",
        "Ctrl+Tab",
    ] {
        assert!(
            shortcut_sequence_issue(input, Windows).is_none(),
            "existing binding must stay valid: {input}"
        );
    }
}
