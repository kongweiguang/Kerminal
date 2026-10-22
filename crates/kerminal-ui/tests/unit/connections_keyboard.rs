//! 快捷保存不得打断文本与嵌套编辑任务。
//!
//! @author kongweiguang

use super::shortcut_may_save;

/// 保存必须由一次明确按键触发；文本编辑与最上层弹层的输入优先。
/// @author kongweiguang
#[test]
fn save_shortcut_respects_text_and_nested_owners() {
    assert!(shortcut_may_save(false, false, false, false));
    assert!(!shortcut_may_save(true, false, false, false));
    assert!(!shortcut_may_save(false, true, false, false));
    assert!(!shortcut_may_save(false, false, true, false));
    assert!(!shortcut_may_save(false, false, false, true));
}
