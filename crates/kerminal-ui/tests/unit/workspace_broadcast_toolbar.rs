//! 群发目标弹层的显示规则，覆盖模式目标数和固定宽度触发标题。
//!
//! @author kongweiguang

use kerminal_domain::identity::PaneId;

use crate::features::workspace_broadcast::{
    WorkspaceBroadcastBarProjection, WorkspaceBroadcastTargetMode,
    WorkspaceBroadcastTargetProjection,
    toolbar::{target_mode_option_labels, target_mode_trigger_label},
};

/// 测试夹具显式标记可发送与当前 pane，避免测试把布局数量误当作目标数量。
///
/// @author kongweiguang
fn target(
    id: &str,
    title: &str,
    current: bool,
    selected: bool,
    enabled: bool,
) -> WorkspaceBroadcastTargetProjection {
    WorkspaceBroadcastTargetProjection {
        pane_id: PaneId::parse(id).expect("valid pane id"),
        title: title.to_string(),
        detail: format!("{title} detail"),
        protocol: "SSH".to_string(),
        selected,
        enabled,
        current,
    }
}

/// 当前项展示当前标题，All 只计可发送 pane，Custom 显示切入时保留的目标数。
///
/// @author kongweiguang
#[test]
fn mode_menu_explains_current_target_and_effective_counts() {
    let projection = WorkspaceBroadcastBarProjection {
        pane_count: 3,
        selected_count: 1,
        targets: vec![
            target("pane-current", "本地终端", true, true, true),
            target("pane-ready", "172.16.41.45", false, false, true),
            target("pane-offline", "离线主机", false, false, false),
        ],
        ..WorkspaceBroadcastBarProjection::default()
    };
    let options = target_mode_option_labels(&projection);

    assert_eq!(options[0].0, WorkspaceBroadcastTargetMode::Current);
    assert!(options[0].1.contains("本地终端"));
    assert_eq!(options[1].0, WorkspaceBroadcastTargetMode::All);
    assert!(options[1].1.contains("2"));
    assert_eq!(options[2].0, WorkspaceBroadcastTargetMode::Custom);
    assert!(options[2].1.contains("1"));
}

/// 主行触发文本保持短，完整 current pane 标题只出现在弹层，不改变输入区宽度。
///
/// @author kongweiguang
#[test]
fn trigger_label_stays_compact_for_current_and_custom_modes() {
    let mut projection = WorkspaceBroadcastBarProjection {
        pane_count: 2,
        selected_count: 1,
        targets: vec![
            target(
                "pane-current",
                "an-extremely-long-host-name-that-would-expand-the-toolbar",
                true,
                true,
                true,
            ),
            target("pane-ready", "other", false, false, true),
        ],
        ..WorkspaceBroadcastBarProjection::default()
    };

    projection.mode = WorkspaceBroadcastTargetMode::Current;
    let current = target_mode_trigger_label(&projection);
    assert!(!current.contains("extremely-long"));

    projection.mode = WorkspaceBroadcastTargetMode::Custom;
    assert!(target_mode_trigger_label(&projection).contains('1'));
}

/// GPUI 夹具保留生产工具栏、Popover 和受控控件，只在 Root 边界记录类型化用户意图。
///
/// @author kongweiguang
#[cfg(test)]
mod toolbar_interaction_gpui {
    use super::*;
    use crate::features::workspace_broadcast::{
        WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent, WorkspaceBroadcastDeliveryProjection,
        WorkspaceBroadcastSnippetProjection,
    };
    use gpui_kit as gpui;
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, MouseButton, Render, Subscription,
        TestAppContext, VisualTestContext, Window, div, prelude::*,
    };
    use std::{cell::RefCell, rc::Rc};

    struct BroadcastToolbarHarness {
        bar: Entity<WorkspaceBroadcastBar>,
        events: Rc<RefCell<Vec<WorkspaceBroadcastBarEvent>>>,
        _subscription: Subscription,
    }

    impl Render for BroadcastToolbarHarness {
        /// 事件断言必须经过真实工具栏的 Render 树，避免直接调用回调掩盖命中与传播问题。
        ///
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.bar.clone())
        }
    }

    /// 建立真实 Kit Root，并把投影中的 Tab 与 pane 身份固定在此次鼠标交互期间。
    ///
    /// @author kongweiguang
    fn fixture(
        app: &mut TestAppContext,
        projection: WorkspaceBroadcastBarProjection,
    ) -> (Entity<BroadcastToolbarHarness>, &mut VisualTestContext) {
        app.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let slot = Rc::new(RefCell::new(None));
        let returned = slot.clone();
        let (_, visual) = app.add_window_view(move |window, app| {
            let bar = app.new(|cx| WorkspaceBroadcastBar::new(window, cx));
            bar.update(app, |bar, cx| bar.set_projection(projection, window, cx));
            let events = Rc::new(RefCell::new(Vec::new()));
            let recorded_events = events.clone();
            let subscription = app.subscribe(&bar, move |_, _, event, _| {
                recorded_events.borrow_mut().push(event.clone());
            });
            let harness = app.new(|_| BroadcastToolbarHarness {
                bar,
                events,
                _subscription: subscription,
            });
            *returned.borrow_mut() = Some(harness.clone());
            gpui::base::Root::new(harness, window, app)
        });
        let harness = slot.borrow_mut().take().expect("broadcast toolbar mounted");
        draw(visual);
        (harness, visual)
    }

    /// 刷新真实窗口 hitbox 并排空受控 Popover 的状态更新，避免复用上一帧的鼠标区域。
    ///
    /// @author kongweiguang
    fn draw(visual: &mut VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, app| window.draw(app).clear(app));
        visual.run_until_parked();
    }

    /// 按 GPUI 的移动、按下、抬起顺序点击渲染中的目标，覆盖 Checkbox 和 Switch 的实际事件路由。
    ///
    /// @author kongweiguang
    fn click(visual: &mut VisualTestContext, selector: &'static str) {
        let bounds = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing rendered control {selector}"));
        let position = bounds.center();
        visual.simulate_mouse_move(position, None, Default::default());
        draw(visual);
        visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
        draw(visual);
        visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
        draw(visual);
    }

    /// 读取订阅到的生产组件事件，便于验证一个鼠标动作只产生一个 Root 意图。
    ///
    /// @author kongweiguang
    fn events(
        harness: &Entity<BroadcastToolbarHarness>,
        visual: &mut VisualTestContext,
    ) -> Vec<WorkspaceBroadcastBarEvent> {
        visual.update(|_, app| harness.read_with(app, |harness, _| harness.events.borrow().clone()))
    }

    /// 目标菜单保留打开状态供连续调整；同步输入由受控 Kit Switch 发出单次切换意图。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn target_menu_and_sync_switch_emit_single_events(cx: &mut TestAppContext) {
        let tab_id = kerminal_domain::identity::TabId::new();
        let first = target("pane-one", "本地主机", true, true, true);
        let second = target("pane-two", "172.16.41.45", false, true, true);
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: 2,
            selected_count: 2,
            targets: vec![first.clone(), second],
            mode: WorkspaceBroadcastTargetMode::All,
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection.clone());
        let draft_before = visual
            .debug_bounds("workspace-broadcast-draft-input-slot")
            .expect("draft input is rendered");
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));
        let mut sending_projection = projection.clone();
        sending_projection.command_pending = true;
        sending_projection.can_cancel = true;
        visual.update(|window, app| {
            bar.update(app, |bar, cx| {
                bar.set_projection(sending_projection, window, cx)
            });
        });
        draw(visual);
        let draft_while_sending = visual
            .debug_bounds("workspace-broadcast-draft-input-slot")
            .expect("draft input remains rendered while sending");
        assert_eq!(draft_before.origin, draft_while_sending.origin);
        assert_eq!(draft_before.size, draft_while_sending.size);
        assert!(
            visual
                .debug_bounds("workspace-broadcast-cancel-button")
                .is_some()
        );

        let mut completed_projection = projection;
        completed_projection.status = Some(
            "已发送 2/2；这是一段足以触发旧版工具栏换行的状态反馈，终端区域应保持稳定。".to_owned(),
        );
        completed_projection.result_details = vec![WorkspaceBroadcastDeliveryProjection {
            title: "本地主机".to_owned(),
            state: "已发送".to_owned(),
        }];
        visual.update(|window, app| {
            bar.update(app, |bar, cx| {
                bar.set_projection(completed_projection, window, cx)
            });
        });
        draw(visual);
        let draft_after_sending = visual
            .debug_bounds("workspace-broadcast-draft-input-slot")
            .expect("draft input remains rendered after delivery");
        assert_eq!(draft_before.origin, draft_after_sending.origin);
        assert_eq!(draft_before.size, draft_after_sending.size);

        click(visual, "workspace-broadcast-mode-trigger");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-target-pane-one")
                .is_some()
        );

        click(visual, "workspace-broadcast-target-pane-one");
        click(visual, "workspace-broadcast-mode-option-Current");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-mode-option-Custom")
                .is_some(),
            "choosing a mode must leave the target menu open for further selection"
        );
        click(visual, "workspace-broadcast-mode-trigger");
        click(visual, "workspace-broadcast-sync-toggle");

        let emitted = events(&harness, visual);
        assert_eq!(
            emitted,
            vec![
                WorkspaceBroadcastBarEvent::ToggleTarget {
                    tab_id: tab_id.clone(),
                    pane_id: first.pane_id,
                },
                WorkspaceBroadcastBarEvent::SetMode {
                    tab_id: tab_id.clone(),
                    mode: WorkspaceBroadcastTargetMode::Current,
                },
                WorkspaceBroadcastBarEvent::ToggleSync { tab_id },
            ],
        );
        assert!(
            visual
                .debug_bounds("workspace-broadcast-close-button")
                .is_none()
        );
    }

    /// 片段点击只发送一次稳定选择意图；终端发送和失败处理由 Root 继续完成。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn snippet_click_emits_one_choose_event_without_toolbar_close_control(cx: &mut TestAppContext) {
        let tab_id = kerminal_domain::identity::TabId::new();
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: 1,
            selected_count: 1,
            targets: vec![target("pane-one", "本地主机", true, true, true)],
            snippets_open: true,
            snippets: vec![WorkspaceBroadcastSnippetProjection {
                id: "snippet-one".to_owned(),
                title: "查看状态".to_owned(),
            }],
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection);
        assert!(
            visual
                .debug_bounds("workspace-broadcast-close-button")
                .is_none()
        );
        click(visual, "workspace-broadcast-snippet-row-snippet-one");

        assert_eq!(
            events(&harness, visual),
            vec![WorkspaceBroadcastBarEvent::ChooseSnippet {
                tab_id,
                snippet_id: "snippet-one".to_owned(),
            }],
        );
    }
}
