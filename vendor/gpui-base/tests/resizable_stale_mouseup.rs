//! 固定全局鼠标监听器跨帧存活时，MouseUp 后的旧 MouseMove 不得继续改变分隔尺寸。
//!
//! @author kongweiguang

use std::{cell::Cell, rc::Rc};

use gpui::{
    AppContext as _, Context, InputEvent as _, InteractiveElement as _, IntoElement, Modifiers,
    MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement as _, Render, Styled as _,
    TestAppContext, Window, div, point, px,
};
use gpui_base::{ResizableState, h_resizable, resizable_panel};

struct ResizeHarness {
    state: gpui::Entity<ResizableState>,
    resize_count: Rc<Cell<usize>>,
}

impl Render for ResizeHarness {
    /// 使用固定双面板布局隔离真实拖动与释放后的迟到移动，避免测试尺寸依赖动态内容。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().w(px(400.)).h(px(100.)).child(
            h_resizable("stale-mouseup")
                .with_state(&self.state)
                .on_resize({
                    let resize_count = self.resize_count.clone();
                    move |_, _, _| resize_count.set(resize_count.get() + 1)
                })
                .child(
                    resizable_panel()
                        .size(px(150.))
                        .child(div().size_full().debug_selector(|| "first-panel".into())),
                )
                .child(
                    resizable_panel()
                        .size(px(250.))
                        .child(div().size_full().debug_selector(|| "second-panel".into())),
                ),
        )
    }
}

/// 在同一事件分发帧依次送入 MouseUp 与 MouseMove，复现绘制监听器尚未刷新的窗口。
///
/// @author kongweiguang
#[gpui::test]
fn mouse_move_after_mouse_up_in_same_frame_does_not_resize_again(cx: &mut TestAppContext) {
    let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
    let resize_count = Rc::new(Cell::new(0));
    let (_view, cx) = cx.add_window_view({
        let state = state.clone();
        let resize_count = resize_count.clone();
        move |_, _| ResizeHarness {
            state,
            resize_count,
        }
    });
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
    });

    let divider = cx.debug_bounds("second-panel").unwrap().left();
    cx.simulate_mouse_down(
        point(divider - px(2.), px(50.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(divider + px(18.), px(50.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    let after_drag = state.read_with(cx, |state, _| state.sizes().clone());
    assert_eq!(resize_count.get(), 0);

    cx.update(|window, app| {
        window.dispatch_event(
            MouseUpEvent {
                position: point(divider + px(18.), px(50.)),
                modifiers: Modifiers::default(),
                button: MouseButton::Left,
                click_count: 1,
            }
            .to_platform_input(),
            app,
        );
        window.dispatch_event(
            MouseMoveEvent {
                position: point(px(360.), px(50.)),
                modifiers: Modifiers::default(),
                pressed_button: None,
            }
            .to_platform_input(),
            app,
        );
    });

    let after_mouse_up_and_late_move = state.read_with(cx, |state, _| state.sizes().clone());
    assert_eq!(after_mouse_up_and_late_move, after_drag);
    assert_eq!(resize_count.get(), 1);
}

/// 同一 ResizableState 经完整重绘后仍可反向拖动，且每次释放只产生一份回执。
///
/// @author kongweiguang
#[gpui::test]
fn persistent_handle_resizes_in_opposite_directions_after_redraw(cx: &mut TestAppContext) {
    let state = cx.update(|cx| cx.new(|_| ResizableState::default()));
    let resize_count = Rc::new(Cell::new(0));
    let (view, cx) = cx.add_window_view({
        let state = state.clone();
        let resize_count = resize_count.clone();
        move |_, _| ResizeHarness {
            state,
            resize_count,
        }
    });
    cx.update(|window, cx| {
        window.draw(cx).clear(cx);
        window.draw(cx).clear(cx);
    });

    let initial_boundary = cx.debug_bounds("second-panel").unwrap().left();
    cx.simulate_mouse_down(
        point(initial_boundary - px(2.), px(50.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(initial_boundary + px(10.), px(50.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(px(220.), px(50.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    assert_eq!(
        resize_count.get(),
        0,
        "drag movement must not emit a receipt"
    );
    cx.simulate_mouse_up(
        point(px(220.), px(50.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(220.), px(180.)]
    );
    assert_eq!(resize_count.get(), 1, "first mouseup emits once");

    // 强制重绘后重新读取句柄边界，确保第二次拖动使用新一帧的 handle。
    view.update(cx, |_, cx| cx.notify());
    cx.update(|window, cx| window.draw(cx).clear(cx));
    cx.run_until_parked();
    let resized_boundary = cx.debug_bounds("second-panel").unwrap().left();
    assert_eq!(resized_boundary, px(220.));

    cx.simulate_mouse_down(
        point(resized_boundary - px(2.), px(50.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(resized_boundary - px(10.), px(50.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    cx.simulate_mouse_move(
        point(px(180.), px(50.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    assert_eq!(
        resize_count.get(),
        1,
        "second drag movement emits no receipt"
    );
    cx.simulate_mouse_up(
        point(px(180.), px(50.)),
        MouseButton::Left,
        Modifiers::default(),
    );

    assert_eq!(
        state.read_with(cx, |state, _| state.sizes().clone()),
        vec![px(180.), px(220.)]
    );
    assert_eq!(resize_count.get(), 2, "second mouseup emits exactly once");
}
