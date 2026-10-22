//! 固定面板策略在容器变化和真实拖动后的尺寸回归。
//!
//! @author kongweiguang

use std::{cell::Cell, rc::Rc};

use gpui::{
    AppContext as _, Context, InteractiveElement as _, IntoElement, Modifiers, MouseButton,
    ParentElement as _, Pixels, Render, Styled as _, TestAppContext, VisualTestContext, Window,
    div, point, px,
};

use super::{ResizableState, h_resizable, resizable_panel};

struct LayoutHarness {
    width: Pixels,
    first_size: Option<Pixels>,
    second_size: Option<Pixels>,
    first_min: Pixels,
    second_min: Pixels,
    fixed_panel: Option<usize>,
    state: gpui::Entity<ResizableState>,
    resize_count: Rc<Cell<usize>>,
}

impl Render for LayoutHarness {
    /// 用真实 GPUI flex 布局测量尺寸，避免测试只覆盖手工计算结果。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let resize_count = self.resize_count.clone();
        let mut group = h_resizable("preserved-layout")
            .with_state(&self.state)
            .on_resize(move |_, _, _| resize_count.set(resize_count.get() + 1));
        if let Some(panel_ix) = self.fixed_panel {
            group = group.preserve_panel_size_on_container_resize(panel_ix);
        }

        let mut first = resizable_panel()
            .size_range(self.first_min..px(1_000.))
            .child(
                div()
                    .size_full()
                    .debug_selector(|| "preserved-first".into()),
            );
        if let Some(size) = self.first_size {
            first = first.size(size);
        }
        let mut second = resizable_panel()
            .size_range(self.second_min..px(1_000.))
            .child(
                div()
                    .size_full()
                    .debug_selector(|| "preserved-second".into()),
            );
        if let Some(size) = self.second_size {
            second = second.size(size);
        }

        div()
            .w(self.width)
            .h(px(100.))
            .child(group.child(first).child(second))
    }
}

/// 建立可改变宽度的真实 GPUI 面板组，让回归走实际边界测量路径。
///
/// @author kongweiguang
fn mount_layout(
    cx: &mut TestAppContext,
    width: Pixels,
    first_size: Option<Pixels>,
    second_size: Option<Pixels>,
    first_min: Pixels,
    second_min: Pixels,
    fixed_panel: Option<usize>,
) -> (
    gpui::Entity<LayoutHarness>,
    &mut VisualTestContext,
    gpui::Entity<ResizableState>,
    Rc<Cell<usize>>,
) {
    let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
    let resize_count = Rc::new(Cell::new(0));
    let (view, cx) = cx.add_window_view({
        let state = state.clone();
        let resize_count = resize_count.clone();
        move |_, _| LayoutHarness {
            width,
            first_size,
            second_size,
            first_min,
            second_min,
            fixed_panel,
            state,
            resize_count,
        }
    });
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
    });
    (view, cx, state, resize_count)
}

/// 侧栏临时收窄时让中心守住最小宽度，空间恢复后回到原像素首选。
///
/// @author kongweiguang
#[gpui::test]
fn fixed_first_panel_restores_preferred_width_after_temporary_clamp(cx: &mut TestAppContext) {
    let (view, cx, state, resize_count) = mount_layout(
        cx,
        px(700.),
        Some(px(220.)),
        None,
        px(120.),
        px(320.),
        Some(0),
    );
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(220.), px(480.)]
    );

    view.update(cx, |view, cx| {
        view.width = px(500.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(180.), px(320.)]
    );

    view.update(cx, |view, cx| {
        view.width = px(700.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(220.), px(480.)]
    );
    assert_eq!(resize_count.get(), 0, "容器变化不能冒充用户拖动回执");
}

/// Dock 位于第二槽时，拖动中心分隔线仍更新 Dock 首选宽度，收窄后可恢复。
///
/// @author kongweiguang
#[gpui::test]
fn fixed_second_panel_tracks_drag_and_restores_after_clamp(cx: &mut TestAppContext) {
    let (view, cx, state, resize_count) = mount_layout(
        cx,
        px(800.),
        Some(px(480.)),
        Some(px(320.)),
        px(320.),
        px(200.),
        Some(1),
    );
    let divider = cx.debug_bounds("preserved-second").unwrap().left();
    let start = point(divider - px(2.), px(50.));
    let destination = point(divider - px(80.), px(50.));
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(destination, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(destination, MouseButton::Left, Modifiers::default());

    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(400.), px(400.)]
    );
    assert_eq!(resize_count.get(), 1, "拖动释放只产生一份回执");

    view.update(cx, |view, cx| {
        view.width = px(600.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(320.), px(280.)]
    );

    view.update(cx, |view, cx| {
        view.width = px(900.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(500.), px(400.)]
    );
    assert_eq!(resize_count.get(), 1, "容器变化不额外发送拖动回执");
}

/// 未 opt-in 的面板组继续使用原来的比例缩放。
///
/// @author kongweiguang
#[gpui::test]
fn groups_without_preserve_policy_keep_proportional_resize(cx: &mut TestAppContext) {
    let (view, cx, state, _) =
        mount_layout(cx, px(800.), Some(px(200.)), None, px(120.), px(200.), None);
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(200.), px(600.)]
    );

    view.update(cx, |view, cx| {
        view.width = px(1_200.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(300.), px(900.)]
    );
}

/// 容器窄于所有面板最小宽度之和时允许溢出，但不覆盖恢复首选。
///
/// @author kongweiguang
#[gpui::test]
fn minimum_width_overflow_does_not_replace_preferred_width(cx: &mut TestAppContext) {
    let (view, cx, state, _) = mount_layout(
        cx,
        px(300.),
        Some(px(220.)),
        None,
        px(120.),
        px(320.),
        Some(0),
    );
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(120.), px(320.)]
    );

    view.update(cx, |view, cx| {
        view.width = px(700.);
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(220.), px(480.)]
    );
}
