//! 分屏广播工具栏的目标选择、发送和同步输入交互回归。
//!
//! @author kongweiguang

use kerminal_domain::identity::PaneId;

use crate::features::workspace_broadcast::{
    WorkspaceBroadcastBarProjection, WorkspaceBroadcastTargetMode,
    WorkspaceBroadcastTargetProjection,
    toolbar::{target_mode_option_labels, target_mode_trigger_label},
};

/// 夹具显式区分可发送、当前和已选择 pane，防止布局数被误当作目标数。
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

/// 菜单文案按当前 pane 和可发送目标数量解释三种稳定模式。
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
    assert!(options[0].1.contains("当前分屏") || options[0].1.contains("Current pane"));
    assert_eq!(options[0].2, "本地终端");
    assert!(!options[0].1.contains("本地终端"));
    assert_eq!(options[1].0, WorkspaceBroadcastTargetMode::All);
    assert!(options[1].1.contains("全部分屏") || options[1].1.contains("All panes"));
    assert!(options[1].2.contains("2"));
    assert_eq!(options[2].0, WorkspaceBroadcastTargetMode::Custom);
    assert!(options[2].1.contains("自定义") || options[2].1.contains("Custom"));
    assert!(options[2].2.contains("1"));
}

/// 触发标题保留短摘要，避免长主机名挤压命令输入。
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
    let options = target_mode_option_labels(&projection);
    assert!(options[0].2.contains("an-extremely-long-host-name"));

    projection.mode = WorkspaceBroadcastTargetMode::Custom;
    assert!(target_mode_trigger_label(&projection).contains('1'));
}

/// Kit Popover 保留 Custom 连续选择目标，并在 Current 或 All 选定后收起。
///
/// @author kongweiguang
#[cfg(test)]
mod toolbar_interaction_gpui {
    use super::*;
    use crate::features::workspace_broadcast::{WorkspaceBroadcastBar, WorkspaceBroadcastBarEvent};
    use gpui_kit as gpui;
    use gpui_kit::{
        AppContext as _, Context, Entity, IntoElement, MouseButton, ParentElement as _, Render,
        Styled as _, Subscription, TestAppContext, VisualTestContext, Window, div,
    };
    use std::{cell::RefCell, rc::Rc};

    struct BroadcastToolbarHarness {
        bar: Entity<WorkspaceBroadcastBar>,
        events: Rc<RefCell<Vec<WorkspaceBroadcastBarEvent>>>,
        _subscription: Subscription,
    }

    impl Render for BroadcastToolbarHarness {
        /// 交互断言经过生产 Render 树，保留 GPUI hitbox 与事件分发行为。
        ///
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().size_full().child(self.bar.clone())
        }
    }

    /// 创建真实 Kit Root 和事件订阅，测试仅观察 Root 收到的用户意图。
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

    /// 刷新 hitbox 并处理控件更新，让下一次鼠标事件命中最新一帧。
    ///
    /// @author kongweiguang
    fn draw(visual: &mut VisualTestContext) {
        visual.run_until_parked();
        visual.update(|window, app| window.draw(app).clear(app));
        visual.run_until_parked();
    }

    /// 按 GPUI 鼠标事件顺序点击当前帧中的控件。
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

    /// 读取真实组件订阅事件，检测每次用户操作是否只提交一个动作。
    ///
    /// @author kongweiguang
    fn events(
        harness: &Entity<BroadcastToolbarHarness>,
        visual: &mut VisualTestContext,
    ) -> Vec<WorkspaceBroadcastBarEvent> {
        visual.update(|_, app| harness.read_with(app, |harness, _| harness.events.borrow().clone()))
    }

    /// 一个发送点击只发一个事件；空草稿、忙碌和无目标都拒绝发送。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn send_button_emits_once_and_rejects_invalid_states(cx: &mut TestAppContext) {
        let tab_id = kerminal_domain::identity::TabId::new();
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: 2,
            selected_count: 2,
            targets: vec![
                target("pane-one", "本地主机", true, true, true),
                target("pane-two", "172.16.41.45", false, true, true),
            ],
            draft: "echo hello".to_owned(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection.clone());
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));

        click(visual, "workspace-broadcast-send-button");
        let send = WorkspaceBroadcastBarEvent::SendCommand {
            tab_id: tab_id.clone(),
        };
        assert_eq!(events(&harness, visual), vec![send.clone()]);

        let mut invalid = projection.clone();
        invalid.draft.clear();
        visual.update(|window, app| {
            bar.update(app, |bar, cx| bar.set_projection(invalid, window, cx))
        });
        click(visual, "workspace-broadcast-send-button");
        assert_eq!(events(&harness, visual), vec![send.clone()]);

        let mut invalid = projection.clone();
        invalid.execution_busy = true;
        visual.update(|window, app| {
            bar.update(app, |bar, cx| bar.set_projection(invalid, window, cx))
        });
        click(visual, "workspace-broadcast-send-button");
        assert_eq!(events(&harness, visual), vec![send.clone()]);

        for draft in [
            " ".to_owned(),
            "x".repeat(16 * 1024 + 1),
            "echo\0unsafe".to_owned(),
        ] {
            let mut invalid = projection.clone();
            invalid.draft = draft;
            visual.update(|window, app| {
                bar.update(app, |bar, cx| bar.set_projection(invalid, window, cx))
            });
            draw(visual);
            click(visual, "workspace-broadcast-send-button");
            assert_eq!(events(&harness, visual), vec![send.clone()]);
        }
        let mut invalid = projection;
        invalid.selected_count = 0;
        invalid.targets.clear();
        visual.update(|window, app| {
            bar.update(app, |bar, cx| bar.set_projection(invalid, window, cx))
        });
        click(visual, "workspace-broadcast-send-button");
        assert_eq!(events(&harness, visual), vec![send]);
    }

    /// 触发区固定为 main 的 36 高，目标小标题和数量始终紧邻目标列表。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn mode_trigger_and_target_heading_follow_main_geometry(cx: &mut TestAppContext) {
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(kerminal_domain::identity::TabId::new()),
            pane_count: 2,
            selected_count: 2,
            targets: vec![
                target("pane-one", "本地主机", true, true, true),
                target("pane-two", "172.16.41.45", false, true, true),
            ],
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (_, visual) = fixture(cx, projection);
        draw(visual);
        let trigger = visual
            .debug_bounds("workspace-broadcast-mode-trigger")
            .expect("mode trigger visible");
        assert_eq!(trigger.size.height, gpui::px(36.));

        click(visual, "workspace-broadcast-mode-trigger");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-target-heading")
                .is_some()
        );
        assert!(
            visual
                .debug_bounds("workspace-broadcast-target-count")
                .is_some()
        );
        let option = visual
            .debug_bounds("workspace-broadcast-mode-option-Current")
            .expect("current mode visible");
        let label = visual
            .debug_bounds("workspace-broadcast-mode-label-Current")
            .expect("mode label visible");
        let detail = visual
            .debug_bounds("workspace-broadcast-mode-detail-Current")
            .expect("mode detail visible");
        assert!(label.right() <= detail.left());
        assert!(detail.right() <= option.right());
    }

    /// Current/All 完成选择后收起菜单，Custom 继续展开以便连续勾选目标。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn target_mode_dismissal_matches_selection_workflow(cx: &mut TestAppContext) {
        let tab_id = kerminal_domain::identity::TabId::new();
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: 2,
            selected_count: 2,
            targets: vec![
                target("pane-one", "本地主机", true, true, true),
                target("pane-two", "172.16.41.45", false, true, true),
            ],
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection);

        click(visual, "workspace-broadcast-mode-trigger");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-target-heading")
                .is_some()
        );
        click(visual, "workspace-broadcast-mode-option-Current");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-mode-option-Custom")
                .is_none()
        );

        click(visual, "workspace-broadcast-mode-trigger");
        click(visual, "workspace-broadcast-mode-option-Custom");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-mode-option-All")
                .is_some()
        );

        click(visual, "workspace-broadcast-mode-option-All");
        assert!(
            visual
                .debug_bounds("workspace-broadcast-mode-option-Custom")
                .is_none()
        );
        assert_eq!(
            events(&harness, visual),
            vec![
                WorkspaceBroadcastBarEvent::SetMode {
                    tab_id: tab_id.clone(),
                    mode: WorkspaceBroadcastTargetMode::Current,
                },
                WorkspaceBroadcastBarEvent::SetMode {
                    tab_id: tab_id.clone(),
                    mode: WorkspaceBroadcastTargetMode::Custom,
                },
                WorkspaceBroadcastBarEvent::SetMode {
                    tab_id,
                    mode: WorkspaceBroadcastTargetMode::All,
                },
            ],
        );
    }

    /// 打开目标弹层查看大量异常明细；三种主题和窄窗均保留主行输入与发送位置。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn delivery_feedback_keeps_input_usable_in_all_themes(cx: &mut TestAppContext) {
        use crate::features::workspace_broadcast::WorkspaceBroadcastDeliveryProjection;
        use kerminal_application::preferences::ThemeMode;
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(kerminal_domain::identity::TabId::new()),
            pane_count: 64,
            selected_count: 64,
            draft: "echo hello".to_owned(),
            result_summary: Some("已发送 32/64；其余目标未发送或结果未知。".to_owned()),
            result_details: (0..64)
                .map(|index| WorkspaceBroadcastDeliveryProjection {
                    title: format!("long-target-name-{index}"),
                    state: "未发送".to_owned(),
                })
                .collect(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection);
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));
        click(visual, "workspace-broadcast-mode-trigger");
        draw(visual);
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            for width in [900., 480.] {
                visual.simulate_resize(gpui::size(gpui::px(width), gpui::px(400.)));
                visual.update(|window, app| {
                    bar.update(app, |_, cx| {
                        crate::theme::apply_theme_mode(mode, window.appearance(), cx);
                    })
                });
                draw(visual);
                let input = visual
                    .debug_bounds("workspace-broadcast-draft-input-slot")
                    .expect("input visible");
                let actions = visual
                    .debug_bounds("workspace-broadcast-actions")
                    .expect("actions visible");
                let details = visual
                    .debug_bounds("workspace-broadcast-deliveries")
                    .expect("details visible");
                assert!(
                    visual
                        .debug_bounds("workspace-broadcast-result-summary")
                        .is_some()
                );
                assert!(input.size.width >= gpui::px(80.));
                assert!(input.right() <= actions.left());
                assert!(actions.right() <= gpui::px(width));
                assert!(details.size.height <= gpui::px(96.));
            }
        }
    }

    /// 完整成功不产生常驻提示；目标弹层仍可正常打开并查看可选分屏。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn successful_result_does_not_show_a_delivery_notice(cx: &mut TestAppContext) {
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(kerminal_domain::identity::TabId::new()),
            pane_count: 2,
            selected_count: 2,
            draft: "echo hello".to_owned(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (_, visual) = fixture(cx, projection);
        draw(visual);
        click(visual, "workspace-broadcast-mode-trigger");
        draw(visual);
        assert!(
            visual
                .debug_bounds("workspace-broadcast-result-summary")
                .is_none()
        );
        assert!(
            visual
                .debug_bounds("workspace-broadcast-deliveries")
                .is_none()
        );
    }

    /// 发送中与其它 Tab 的群发收尾期间均禁用按钮，避免用户重复派发命令。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn pending_and_busy_broadcasts_reject_repeat_send(cx: &mut TestAppContext) {
        let initial = WorkspaceBroadcastBarProjection {
            tab_id: Some(kerminal_domain::identity::TabId::new()),
            pane_count: 2,
            selected_count: 2,
            draft: "echo hello".to_owned(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, initial.clone());
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));
        for (pending, busy) in [(true, true), (false, true)] {
            let mut projection = initial.clone();
            projection.command_pending = pending;
            projection.execution_busy = busy;
            visual.update(|window, app| {
                bar.update(app, |bar, cx| bar.set_projection(projection, window, cx));
            });
            draw(visual);
            click(visual, "workspace-broadcast-send-button");
        }
        assert!(
            events(&harness, visual)
                .iter()
                .all(|event| !matches!(event, WorkspaceBroadcastBarEvent::SendCommand { .. }))
        );
    }

    /// 组合文本的 Enter 不提交；切 Tab 清除组合标记并将草稿和后续发送归属新 Tab。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn composition_enter_and_tab_switch_do_not_send_stale_draft(cx: &mut TestAppContext) {
        use gpui::EntityInputHandler as _;
        let first = kerminal_domain::identity::TabId::new();
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(first),
            pane_count: 2,
            selected_count: 2,
            draft: "echo old".to_owned(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection.clone());
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));
        visual.update(|window, app| {
            bar.update(app, |bar, cx| {
                bar.focus_draft(window, cx);
                bar.draft.update(cx, |input, cx| {
                    input.replace_and_mark_text_in_range(None, "组合文本", None, window, cx);
                });
            })
        });
        draw(visual);
        visual.update(|window, app| {
            bar.update(app, |bar, cx| {
                bar.draft.update(cx, |input, cx| {
                    assert!(input.marked_text_range(window, cx).is_some());
                });
            });
        });
        visual.simulate_keystrokes("enter");
        assert!(
            !events(&harness, visual)
                .iter()
                .any(|event| matches!(event, WorkspaceBroadcastBarEvent::SendCommand { .. }))
        );
        let second = kerminal_domain::identity::TabId::new();
        let mut next = projection;
        next.tab_id = Some(second.clone());
        next.draft = "echo new".to_owned();
        visual.update(|window, app| {
            bar.update(app, |bar, cx| {
                bar.set_projection(next, window, cx);
                bar.draft.update(cx, |input, cx| {
                    assert!(input.marked_text_range(window, cx).is_none());
                    assert_eq!(input.value().as_ref(), "echo new");
                });
            })
        });
        draw(visual);
        click(visual, "workspace-broadcast-send-button");
        let sends = events(&harness, visual)
            .into_iter()
            .filter(|event| matches!(event, WorkspaceBroadcastBarEvent::SendCommand { .. }))
            .collect::<Vec<_>>();
        assert_eq!(
            sends,
            vec![WorkspaceBroadcastBarEvent::SendCommand { tab_id: second }]
        );
    }

    /// 同步开关的鼠标按下不得夺走草稿输入焦点。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn sync_switch_preserves_draft_focus_and_emits_once(cx: &mut TestAppContext) {
        let tab_id = kerminal_domain::identity::TabId::new();
        let projection = WorkspaceBroadcastBarProjection {
            tab_id: Some(tab_id.clone()),
            pane_count: 2,
            selected_count: 2,
            targets: vec![
                target("pane-one", "本地主机", true, true, true),
                target("pane-two", "172.16.41.45", false, true, true),
            ],
            draft: "echo hello".to_owned(),
            ..WorkspaceBroadcastBarProjection::default()
        };
        let (harness, visual) = fixture(cx, projection);
        let bar = visual.update(|_, app| harness.read_with(app, |harness, _| harness.bar.clone()));
        visual.update(|window, app| bar.update(app, |bar, cx| bar.focus_draft(window, cx)));
        draw(visual);
        let focus_before = visual.update(|window, app| window.focused(app));
        assert!(focus_before.is_some());

        click(visual, "workspace-broadcast-sync-toggle");

        assert_eq!(
            visual.update(|window, app| window.focused(app)),
            focus_before
        );
        assert_eq!(
            events(&harness, visual),
            vec![WorkspaceBroadcastBarEvent::ToggleSync { tab_id }],
        );
    }
}

/// 发送预算以 UTF-8 字节计量，空字符拒绝发送；边界内的多字节草稿仍可操作。
///
/// @author kongweiguang
#[test]
fn draft_validation_uses_utf8_budget_without_truncating_text() {
    use crate::features::workspace_broadcast::broadcast_draft_error;
    assert!(broadcast_draft_error(&"x".repeat(16 * 1024)).is_none());
    assert!(broadcast_draft_error(&"x".repeat(16 * 1024 + 1)).is_some());
    assert!(broadcast_draft_error(&"中".repeat(5461)).is_none());
    assert!(broadcast_draft_error(&"中".repeat(5462)).is_some());
    assert!(broadcast_draft_error("echo\0unsafe").is_some());
}
