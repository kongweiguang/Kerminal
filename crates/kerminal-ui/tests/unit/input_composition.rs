//! 通过真实 Kit/GPUI keymap 验证选择器正文不会把文本按键传播给父 Popover。
//!
//! @author kongweiguang

use super::{PICKER_INPUT_CONTEXT, picker_input_bindings};
use gpui_kit::{
    KeyBinding, KeyContext, Keymap, Keystroke, base::actions::Confirm, component::input::Enter,
};

/// fixture 沿用锁定 Kit 的 Popover 与单行 Input 绑定，应用规则仅追加自己的上下文。
fn picker_keymap() -> Keymap {
    let mut keymap = Keymap::new(vec![
        KeyBinding::new("enter", Confirm { secondary: false }, Some("Popover")),
        KeyBinding::new("space", Confirm { secondary: false }, Some("Popover")),
        KeyBinding::new(
            "enter",
            Enter {
                secondary: false,
                shift: false,
            },
            Some("Input"),
        ),
    ]);
    keymap.add_bindings(picker_input_bindings());
    keymap
}

/// 正文空格必须留给文本系统；Enter 只保留 Input action，供选择器捕获一次选择。
#[test]
fn picker_keeps_input_enter_and_suppresses_parent_confirm() {
    let keymap = picker_keymap();
    let contexts = [
        KeyContext::parse("Popover").unwrap(),
        KeyContext::parse(PICKER_INPUT_CONTEXT).unwrap(),
        KeyContext::parse("Input").unwrap(),
    ];
    let (enter, pending) =
        keymap.bindings_for_input(&[Keystroke::parse("enter").unwrap()], &contexts);
    assert!(!pending);
    assert_eq!(enter.len(), 1);
    assert!(enter[0].action().as_any().is::<Enter>());
    let (space, pending) =
        keymap.bindings_for_input(&[Keystroke::parse("space").unwrap()], &contexts);
    assert!(!pending);
    assert!(space.is_empty());
}

/// 未使用选择器正文的普通弹层仍可用 Enter/Space 确认，不扩大应用级覆盖范围。
#[test]
fn ordinary_popover_confirmation_is_unchanged() {
    let keymap = picker_keymap();
    let contexts = [KeyContext::parse("Popover").unwrap()];
    for key in ["enter", "space"] {
        let (bindings, pending) =
            keymap.bindings_for_input(&[Keystroke::parse(key).unwrap()], &contexts);
        assert!(!pending);
        assert_eq!(bindings.len(), 1);
        assert!(bindings[0].action().as_any().is::<Confirm>());
    }
}
