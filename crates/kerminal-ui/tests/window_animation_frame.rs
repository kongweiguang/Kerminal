//! 回归 GPUI 在渲染阶段之外接收动画帧请求的窗口契约。
//!
//! @author kongweiguang

use gpui_kit::{Context, Render, TestAppContext, Window, div, point, prelude::*, px};

struct PointerAnimation;

impl Render for PointerAnimation {
    /// 通过绘制时注册的真实监听器覆盖输入分发，此时没有正在渲染的视图。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().on_mouse_move(|_, window, _| {
            window.request_animation_frame();
        })
    }
}

/// 输入回调应成功安排下一帧，并在执行后清空该次请求，避免无条件持续刷新。
#[gpui_kit::test]
fn mouse_move_can_schedule_animation_without_a_rendering_view(cx: &mut TestAppContext) {
    let handle = cx.add_window(|_, _| PointerAnimation);
    cx.update(|cx| {
        gpui_kit::AnyWindowHandle::from(handle)
            .update(cx, |_, window, cx| {
                let _ = window.draw(cx);
                assert_eq!(window.simulate_next_frame(cx), 0);
                window.simulate_mouse_move(point(px(10.), px(10.)), cx);
                assert_eq!(window.simulate_next_frame(cx), 1);
                assert_eq!(window.simulate_next_frame(cx), 0);
            })
            .expect("测试窗口应保持存活");
    });
}
