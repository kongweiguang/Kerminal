//! 确保触发阈值的首个拖动移动已按最终指针位置调整分隔面板。
//!
//! @author kongweiguang

use gpui::{InputEvent as _, Modifiers, MouseButton, TestAppContext, point, px};

use super::harness;

/// 首次超过拖动阈值的 MouseMove 必须立即生效，且只有 MouseUp 发出一次完成回执。
///
/// @author kongweiguang
#[gpui::test]
fn first_threshold_move_resizes_and_mouse_up_emits_once(cx: &mut TestAppContext) {
    let (cx, state, resize_count) = harness(cx);
    let before = state.read_with(cx, |state, _| state.sizes().clone());
    let divider = cx.debug_bounds("second-panel").unwrap().left();
    let start = point(divider - px(2.), px(50.));
    let destination = point(divider + px(44.), px(50.));

    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(destination, Some(MouseButton::Left), Modifiers::default());

    let after_first_move = state.read_with(cx, |state, _| state.sizes().clone());
    assert_ne!(after_first_move, before, "首个拖动移动应立即调整尺寸");
    assert!(after_first_move[0] > before[0]);
    assert_eq!(resize_count.get(), 0, "拖动移动不应提前发完成回执");

    cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::default());

    assert_eq!(resize_count.get(), 1, "释放时应且仅应发一份完成回执");
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        after_first_move,
        "释放不应重复应用或改变最后一次拖动位置"
    );
}
