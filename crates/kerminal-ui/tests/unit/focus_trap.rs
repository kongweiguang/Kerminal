//! Kit 焦点陷阱在重复句柄和空内部 Tab 顺序下的真实按键回归。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement,
    ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext, Window,
    base::{FocusTrapElement as _, Root, active_focus_trap},
    div, px,
};

#[derive(Clone, Copy)]
enum FocusFixtureKind {
    ForwardDuplicate,
    BackwardDuplicate,
    NoTabStopsInside,
}

struct FocusFixture {
    kind: FocusFixtureKind,
    trap: FocusHandle,
    repeated: FocusHandle,
    other: FocusHandle,
}

impl Render for FocusFixture {
    /// 重复渲染同一焦点句柄，保留真实 TabStopMap 的身份覆盖与顺序，模拟组合控件边界。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let first = div()
            .id("focus-repeat-first")
            .size(px(20.))
            .track_focus(&self.repeated);
        let second = div()
            .id("focus-repeat-second")
            .size(px(20.))
            .track_focus(&self.repeated);
        let other = div()
            .id("focus-other")
            .size(px(20.))
            .track_focus(&self.other);
        let trap = div()
            .size(px(20.))
            .focus_trap("focus-empty-trap", &self.trap);
        let content = div().size_full().flex();
        match self.kind {
            // Tab 顺序为 A / trap / B / A，真实遍历为 B -> A -> A。
            FocusFixtureKind::ForwardDuplicate => {
                content.child(first).child(trap).child(other).child(second)
            }
            // Shift+Tab 顺序为 B / trap / A / A，真实遍历为 B -> A -> A。
            FocusFixtureKind::BackwardDuplicate => {
                content.child(other).child(trap).child(first).child(second)
            }
            FocusFixtureKind::NoTabStopsInside => content.child(first).child(trap).child(other),
        }
    }
}

/// 独立窗口只初始化真实 Kit Root 和焦点句柄，不依赖设置页面的控件数量或异步状态。
///
/// @author kongweiguang
fn fixture(
    app: &mut TestAppContext,
    kind: FocusFixtureKind,
) -> (
    FocusHandle,
    FocusHandle,
    FocusHandle,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let trap = app.update(|cx| cx.focus_handle());
    let repeated = app.update(|cx| cx.focus_handle().tab_stop(true));
    let other = app.update(|cx| cx.focus_handle().tab_stop(true));
    let fixture_trap = trap.clone();
    let fixture_repeated = repeated.clone();
    let fixture_other = other.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let view = cx.new(|_| FocusFixture {
            kind,
            trap: fixture_trap,
            repeated: fixture_repeated,
            other: fixture_other,
        });
        Root::new(view, window, cx)
    });
    visual.update(|window, cx| {
        window.draw(cx).clear(cx);
        trap.focus(window, cx);
        assert_eq!(active_focus_trap(window, cx), Some(trap.clone()));
    });
    (trap, repeated, other, visual)
}

/// 先核对 GPUI 实际遍历形成不经过入口的环，再通过真实 Root 按键检查原焦点恢复。
///
/// @author kongweiguang
fn assert_duplicate_cycle_recovers(app: &mut TestAppContext, kind: FocusFixtureKind, key: &str) {
    let (trap, repeated, other, visual) = fixture(app, kind);
    visual.update(|window, cx| {
        let step = |window: &mut Window, cx: &mut gpui_kit::App| match kind {
            FocusFixtureKind::ForwardDuplicate => window.focus_next(cx),
            FocusFixtureKind::BackwardDuplicate => window.focus_prev(cx),
            FocusFixtureKind::NoTabStopsInside => unreachable!(),
        };
        step(window, cx);
        assert!(other.is_focused(window), "首次移动应到达外部其他句柄");
        step(window, cx);
        assert!(repeated.is_focused(window), "第二次移动应到达重复句柄");
        step(window, cx);
        assert!(repeated.is_focused(window), "重复句柄应形成不经过入口的环");
        trap.focus(window, cx);
        assert_eq!(active_focus_trap(window, cx), Some(trap.clone()));
    });
    visual.simulate_keystrokes(key);
    assert!(visual.update(|window, _| trap.is_focused(window)));
}

/// 正向遍历进入共享句柄自环时必须结束且保留 trap 入口焦点。
///
/// @author kongweiguang
#[gpui_kit::test]
fn focus_trap_forward_duplicate_handle_recovers(app: &mut TestAppContext) {
    assert_duplicate_cycle_recovers(app, FocusFixtureKind::ForwardDuplicate, "tab");
}

/// 反向遍历也必须识别不包含首次外部焦点的重复句柄环。
///
/// @author kongweiguang
#[gpui_kit::test]
fn focus_trap_backward_duplicate_handle_recovers(app: &mut TestAppContext) {
    assert_duplicate_cycle_recovers(app, FocusFixtureKind::BackwardDuplicate, "shift-tab");
}

/// 无内部 TabStop 时双向移动都应恢复入口，而不是让焦点落在背景控件。
///
/// @author kongweiguang
#[gpui_kit::test]
fn focus_trap_without_tab_stops_restores_entry(app: &mut TestAppContext) {
    let (trap, _, _, visual) = fixture(app, FocusFixtureKind::NoTabStopsInside);
    for key in ["tab", "shift-tab"] {
        visual.simulate_keystrokes(key);
        assert!(
            visual.update(|window, _| trap.is_focused(window)),
            "{key} must preserve the trap entry focus"
        );
    }
}
