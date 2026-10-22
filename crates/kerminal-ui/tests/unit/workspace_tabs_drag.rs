//! WorkspaceTabs 的真实 GPUI 拖动取消回归。
//!
//! @author kongweiguang

use super::{TabId, WorkspaceTabEvent, WorkspaceTabKind, WorkspaceTabProjection, WorkspaceTabs};

/// 让焦点位于标签行之外，同时订阅生产组件事件，验证取消不依赖 Tab 键盘焦点。
///
/// @author kongweiguang
mod workspace_tabs_drag_gpui {
    use super::*;
    use gpui::{
        AppContext as _, Context, Entity, FocusHandle, InteractiveElement as _, IntoElement,
        Modifiers, MouseButton, ParentElement as _, Render, Styled as _, Subscription,
        TestAppContext, VisualTestContext, Window, div, point, px, size,
    };
    use gpui_kit as gpui;
    use std::{cell::RefCell, rc::Rc, time::Duration};

    struct DragHarness {
        tabs: Entity<WorkspaceTabs>,
        events: Rc<RefCell<Vec<WorkspaceTabEvent>>>,
        outside_focus: FocusHandle,
        _event_subscription: Subscription,
    }

    impl DragHarness {
        /// 固定稳定 TabId 并挂载真实 WorkspaceTabs；事件仅观测，不复刻 Controller 行为。
        ///
        /// @author kongweiguang
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let projection = (0..8)
                .map(|index| WorkspaceTabProjection {
                    id: TabId::parse(&format!("drag-tab-{index}")).expect("valid test TabId"),
                    title: format!("Terminal {index}").into(),
                    group_id: None,
                    active: index == 0,
                    kind: WorkspaceTabKind::Terminal,
                    layout: None,
                })
                .collect();
            let tabs = cx.new(|_| WorkspaceTabs::new(projection));
            let events = Rc::new(RefCell::new(Vec::new()));
            let recorded_events = events.clone();
            let event_subscription =
                cx.subscribe(&tabs, move |_, _, event: &WorkspaceTabEvent, _| {
                    recorded_events.borrow_mut().push(event.clone());
                });
            let outside_focus = cx.focus_handle();
            outside_focus.focus(window, cx);

            Self {
                tabs,
                events,
                outside_focus,
                _event_subscription: event_subscription,
            }
        }
    }

    impl Render for DragHarness {
        /// 单独保留非 Tab 焦点节点，确保拖动与取消不靠焦点落在标签按钮上。
        ///
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .flex()
                .flex_col()
                .child(div().size_0().track_focus(&self.outside_focus))
                .child(self.tabs.clone())
        }
    }

    /// 通过 GPUI Kit Root 挂载实际标签视图，并把窗口收窄到需要边缘滚动的宽度。
    ///
    /// @author kongweiguang
    fn fixture(cx: &mut TestAppContext) -> (Entity<DragHarness>, &mut VisualTestContext) {
        cx.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let slot = Rc::new(RefCell::new(None));
        let returned = slot.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| DragHarness::new(window, cx));
            *returned.borrow_mut() = Some(harness.clone());
            gpui::base::Root::new(harness, window, cx)
        });
        let harness = slot.borrow_mut().take().expect("drag harness mounted");
        // 测试平台默认未激活；显式模拟前台窗口，生产滚动会在失焦时主动结束拖动。
        visual.update(|window, _| window.activate_window());
        visual.simulate_resize(size(px(480.), px(160.)));
        draw(visual);
        assert!(visual.update(|window, _| window.is_window_active()));
        (harness, visual)
    }

    /// 两帧后读取真实滚动测量，避免测试用未布局的标签坐标启动拖动。
    ///
    /// @author kongweiguang
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        cx.update(|window, app| window.draw(app).clear(app));
    }

    /// 活动拖动取消须同步停止 GPUI payload 与边缘滚动，并且不产生一次 Drop 意图。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn cancel_drag_stops_payload_and_edge_scroll_without_drop_submission(cx: &mut TestAppContext) {
        let (harness, visual) = fixture(cx);
        let source_tab = visual
            .debug_bounds("workspace-tab-hitbox-drag-tab-1")
            .expect("production Tab hitbox is measured");
        let tabs =
            visual.update(|_, app| harness.read_with(app, |harness, _| harness.tabs.clone()));
        let (viewport, max_offset, initial_offset) = visual.update(|_, app| {
            tabs.read_with(app, |tabs, _| {
                (
                    tabs.scroll_handle.bounds(),
                    tabs.scroll_handle.max_offset(),
                    tabs.scroll_handle.offset(),
                )
            })
        });
        assert!(
            max_offset.x > px(0.),
            "fixture must have horizontal overflow"
        );
        assert!(
            viewport.size.width > px(32.),
            "scroll viewport must be measurable"
        );

        let start = point(
            source_tab.origin.x + px(40.),
            source_tab.origin.y + source_tab.size.height / 2.,
        );
        let edge = point(
            viewport.right() - px(2.),
            viewport.origin.y + viewport.size.height / 2.,
        );
        visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
        visual.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
        assert!(visual.update(|_, app| app.has_active_drag()));
        let edge_scroll_started = visual.update(|_, app| {
            tabs.read_with(app, |tabs, _| {
                tabs.drag_scroll_task.is_some() && tabs.drag_scroll_source.is_some()
            })
        });
        assert!(
            edge_scroll_started,
            "the production edge-scroll loop must start"
        );

        visual.executor().advance_clock(Duration::from_millis(60));
        visual.run_until_parked();
        let offset_during_drag =
            visual.update(|_, app| tabs.read_with(app, |tabs, _| tabs.scroll_handle.offset()));
        assert!(
            offset_during_drag.x < initial_offset.x,
            "dragging at the right viewport edge should advance the real scroll offset"
        );

        let outside_focus = visual
            .update(|_, app| harness.read_with(app, |harness, _| harness.outside_focus.clone()));
        visual.update(|window, app| window.focus(&outside_focus, app));
        assert!(visual.update(|window, _app| outside_focus.is_focused(window)));
        assert!(visual.update(|_, app| app.has_active_drag()));

        let cancelled = visual.update(|window, app| {
            tabs.update(app, |tabs, cx| tabs.cancel_workspace_tab_drag(window, cx))
        });
        assert!(
            cancelled,
            "the active Tab drag must be cancelled exactly once"
        );
        assert!(!visual.update(|_, app| app.has_active_drag()));
        let (drag_state_cleared, events_after_cancel) = visual.update(|_, app| {
            let drag_state_cleared = tabs.read_with(app, |tabs, _| {
                tabs.drag_scroll_task.is_none() && tabs.drag_scroll_source.is_none()
            });
            let events = harness.read_with(app, |harness, _| harness.events.borrow().clone());
            (drag_state_cleared, events)
        });
        assert!(
            drag_state_cleared,
            "edge-scroll task and source must be cleared"
        );
        assert!(
            events_after_cancel.is_empty(),
            "cancel must not emit a Drop event"
        );

        visual.simulate_mouse_up(edge, MouseButton::Left, Modifiers::default());
        let cancelled_again = visual.update(|window, app| {
            tabs.update(app, |tabs, cx| tabs.cancel_workspace_tab_drag(window, cx))
        });
        assert!(!cancelled_again, "repeated cancellation must be a no-op");
        visual.executor().advance_clock(Duration::from_millis(250));
        visual.run_until_parked();

        visual.update(|_, app| {
            assert!(!app.has_active_drag());
            let offset_after_cancel = tabs.read_with(app, |tabs, _| tabs.scroll_handle.offset());
            assert_eq!(offset_after_cancel, offset_during_drag);
            harness.read_with(app, |harness, _| {
                assert!(
                    harness.events.borrow().is_empty(),
                    "mouse-up after cancellation must not submit a stale Drop"
                );
            });
        });
    }
}
