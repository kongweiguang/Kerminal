//! 选择器与命令输入共用按键边界，避免文本按键连带触发父弹层动作。
//!
//! @author kongweiguang

use gpui_kit::{
    App, Entity, EntityInputHandler as _, KeyBinding, NoAction, Window,
    component::input::InputState,
};

pub(crate) const PICKER_INPUT_CONTEXT: &str = "KerminalPickerInput";

/// Kit 的 Popover 同时把 Enter 与 Space 绑定为 Confirm；选择器正文增加更深的
/// context，仅屏蔽父级确认，保留更深 Input 的 Enter action 与普通空格文本输入。
pub(crate) fn init(cx: &mut App) {
    cx.bind_keys(picker_input_bindings());
}

/// 返回应用持有的作用域绑定，测试直接经过实际 Keymap 排序，避免只验证自写分支。
fn picker_input_bindings() -> [KeyBinding; 2] {
    [
        KeyBinding::new("enter", NoAction, Some(PICKER_INPUT_CONTEXT)),
        KeyBinding::new("space", NoAction, Some(PICKER_INPUT_CONTEXT)),
    ]
}

/// 必须在 Input 的 Escape action 前调用：Kit 会先清除组合再向父级传播，
/// 因而父级事后检查已无法区分“取消组合”和“关闭弹层”。只有确有组合时消费按键。
pub(crate) fn cancel_input_composition(
    input: &Entity<InputState>,
    window: &mut Window,
    cx: &mut App,
) {
    if input.read(cx).is_composing() {
        input.update(cx, |input, cx| input.unmark_text(window, cx));
        window.prevent_default();
        cx.stop_propagation();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/input_composition.rs"]
mod tests;
