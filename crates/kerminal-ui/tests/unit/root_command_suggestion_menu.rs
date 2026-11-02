//! 命令建议菜单的选择状态回归测试。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc, sync::Arc};

use gpui_kit::{
    AppContext as _, Context, Entity, FocusHandle, IntoElement, MouseButton, Render,
    TestAppContext, VisualTestContext, Window, div, prelude::*, px, size,
};
use kerminal_application::sessions::ShellEditingUnavailableReason;
use kerminal_domain::commands::{
    CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionReplacementRange,
    CommandSuggestionSensitivity, SuggestionPresentation, SuggestionProviderKind,
};

use super::{
    MenuSelection, TerminalCommandSuggestionMenu, TerminalCommandSuggestionMenuEvent,
    TerminalCommandSuggestionMenuKey, TerminalSuggestionMenuProjection,
    TerminalSuggestionQueryState, bounded_tooltip_text, candidate_description,
};

/// 保留真实菜单事件订阅，避免只检查视图状态而漏掉错误的接受或重试意图。
struct MenuEventObserver {
    _subscription: gpui_kit::Subscription,
}

/// 为真实菜单保留终端焦点宿主，模拟非模态覆盖层与终端输入共享的焦点关系。
struct MenuFocusHost {
    terminal: FocusHandle,
    menu: Entity<TerminalCommandSuggestionMenu>,
}

impl Render for MenuFocusHost {
    /// 按生产树将菜单放进终端焦点范围，不能以独立按钮的焦点状态代替终端断言。
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .track_focus(&self.terminal)
            .child(self.menu.clone())
    }
}

/// 构造只有菜单所需信息的候选，确保选择逻辑不依赖 provider 正文或业务副作用。
fn candidate(id: &str) -> CommandSuggestionCandidate {
    CommandSuggestionCandidate {
        id: id.to_owned(),
        provider: SuggestionProviderKind::History,
        activation: CommandSuggestionActivation::Insert,
        display_text: id.to_owned(),
        replacement_text: id.to_owned(),
        replacement_range: CommandSuggestionReplacementRange { start: 0, end: 0 },
        suffix: String::new(),
        score: 1.0,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: None,
        merged_source_explanations: Vec::new(),
        source_id: None,
        allowed_presentations: vec![SuggestionPresentation::Menu],
        accept_boundaries: Vec::new(),
        context_key: None,
    }
}

/// 只移除历史来源的固定重复说明，真实上下文及其他来源的同名描述仍应保留。
#[test]
fn redundant_history_description_is_hidden_without_discarding_context() {
    let mut item = candidate("git");
    item.description = Some(" 命令历史 ".into());
    assert_eq!(candidate_description(&item), None);
    item.description = Some("最近用于当前目录".into());
    assert_eq!(candidate_description(&item), Some("最近用于当前目录"));
    item.provider = SuggestionProviderKind::Spec;
    item.description = Some("命令历史".into());
    assert_eq!(candidate_description(&item), Some("命令历史"));
}

/// 悬浮说明按 Unicode 标量限制长度，既不截断 UTF-8，也不因异常长输入无界膨胀。
#[test]
fn tooltip_preview_is_unicode_safe_and_bounded() {
    assert_eq!(bounded_tooltip_text("中文"), "中文");
    let preview = bounded_tooltip_text(&"中".repeat(513));
    assert_eq!(preview.chars().count(), 513);
    assert!(preview.ends_with('…'));
}

/// 同一请求更新候选时保留仍有效的高亮 ID，避免异步排序变化误选另一命令。
#[test]
fn same_generation_preserves_selected_candidate_by_id() {
    let mut selection = MenuSelection::default();
    selection.reconcile(
        7,
        &[candidate("first"), candidate("second")],
        &["first".into(), "second".into()],
    );
    assert!(selection.select(7, 1));

    selection.reconcile(
        7,
        &[candidate("second"), candidate("first")],
        &["second".into(), "first".into()],
    );

    assert_eq!(selection.selected_id(), Some("second"));
}

/// 新查询代次重置旧高亮，防止菜单快速重开时以旧索引接受不同输入的候选。
#[test]
fn new_generation_resets_highlight_to_first_candidate() {
    let mut selection = MenuSelection::default();
    selection.reconcile(
        7,
        &[candidate("first"), candidate("second")],
        &["first".into(), "second".into()],
    );
    assert!(selection.select(7, 1));

    selection.reconcile(
        8,
        &[candidate("new-first"), candidate("new-second")],
        &["new-first".into(), "new-second".into()],
    );

    assert_eq!(selection.selected_id(), Some("new-first"));
}

/// 键盘导航在列表首尾环绕，空结果时不创建虚假的可选项。
#[test]
fn navigation_wraps_and_empty_candidates_remain_unselected() {
    let mut selection = MenuSelection::default();
    selection.reconcile(
        7,
        &[candidate("first"), candidate("second")],
        &["first".into(), "second".into()],
    );

    assert!(selection.move_by(-1));
    assert_eq!(selection.selected_id(), Some("second"));
    assert!(selection.move_by(1));
    assert_eq!(selection.selected_id(), Some("first"));

    selection.reconcile(8, &[], &[]);
    assert!(!selection.move_by(1));
    assert_eq!(selection.selected_id(), None);
}

/// 同一追加内容被新来源替换时保留选择，同时将接受目标更新为新候选 ID。
#[test]
fn same_generation_preserves_selection_when_provider_changes() {
    let mut selection = MenuSelection::default();
    selection.reconcile(
        7,
        &[candidate("old"), candidate("other")],
        &["same-suffix".into(), "other".into()],
    );
    assert!(selection.select(7, 0));
    selection.reconcile(
        7,
        &[candidate("other"), candidate("new")],
        &["other".into(), "same-suffix".into()],
    );
    assert_eq!(selection.selected_id(), Some("new"));
}

/// 候选 ID 被复用但追加内容变化时，不得把用户选择转移到不同命令。
#[test]
fn same_id_with_changed_append_content_clears_explicit_selection() {
    let mut selection = MenuSelection::default();
    selection.reconcile(7, &[candidate("same-id")], &["append:before".into()]);
    assert!(selection.select(7, 0));

    selection.reconcile(7, &[candidate("same-id")], &["append:after".into()]);

    assert_eq!(selection.selected_id(), None);
    assert!(selection.move_by(1));
    assert_eq!(selection.selected_id(), Some("same-id"));
}

/// 后续高分结果挤掉用户已选中的末项时，不能把回车目标悄悄改成另一条。
#[test]
fn same_generation_eviction_clears_explicit_selection_until_navigation() {
    let mut selection = MenuSelection::default();
    let previous = (0..20)
        .map(|index| candidate(&format!("candidate-{index}")))
        .collect::<Vec<_>>();
    let previous_keys = previous
        .iter()
        .map(|item| item.id.clone())
        .collect::<Vec<_>>();
    selection.reconcile(7, &previous, &previous_keys);
    assert!(selection.select(7, 19));

    let mut next = vec![candidate("new-high-score")];
    next.extend(previous.into_iter().take(19));
    let next_keys = next.iter().map(|item| item.id.clone()).collect::<Vec<_>>();
    selection.reconcile(7, &next, &next_keys);
    assert_eq!(selection.selected_id(), None);

    selection.reconcile(7, &next, &next_keys);
    assert_eq!(selection.selected_id(), None);
    assert!(selection.move_by(1));
    assert_eq!(selection.selected_id(), Some("new-high-score"));
}

/// 用户尚未主动选择时，首项变化仍可自动高亮，避免影响常见的直接回车补全。
#[test]
fn same_generation_replaces_default_selection_when_untouched_candidate_disappears() {
    let mut selection = MenuSelection::default();
    selection.reconcile(7, &[candidate("first")], &["first".into()]);
    selection.reconcile(7, &[candidate("replacement")], &["replacement".into()]);
    assert_eq!(selection.selected_id(), Some("replacement"));
}

/// 每个候选使用真实菜单行，避免将不定行高简化成固定像素推算。
fn menu_projection(
    generation: u64,
    candidates: Vec<CommandSuggestionCandidate>,
) -> TerminalSuggestionMenuProjection {
    let keys = candidates
        .iter()
        .map(|candidate| candidate.id.clone())
        .collect::<Vec<_>>();
    TerminalSuggestionMenuProjection {
        request_generation: generation,
        state: TerminalSuggestionQueryState::Ready,
        partial_accept: false,
        candidates: Arc::from(candidates),
        selection_keys: Arc::from(keys),
        partial_failure: false,
    }
}

/// 推进布局与 prepaint，使断言读取滚动请求生效后的实际子项几何。
fn draw_menu(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| window.draw(cx).clear(cx));
    visual.update(|window, cx| window.draw(cx).clear(cx));
}

/// 结合滚动偏移检查真实候选行，防止高亮位于裁剪区域之外。
fn assert_selection_visible(
    menu: &Entity<TerminalCommandSuggestionMenu>,
    visual: &mut VisualTestContext,
) {
    draw_menu(visual);
    visual.update(|_, cx| {
        let menu = menu.read(cx);
        let viewport = menu.scroll_handle.bounds();
        let row = menu
            .scroll_handle
            .bounds_for_item(menu.selection.selected_index.expect("a row is selected"))
            .expect("selected row has been laid out");
        let offset = menu.scroll_handle.offset().y;
        assert!(viewport.size.height > px(0.));
        assert!(row.top() + offset >= viewport.top() - px(1.));
        assert!(row.bottom() + offset <= viewport.bottom() + px(1.));
    });
}

/// 没有可选项时方向键留给 Shell；回车与关闭仍由菜单处理，防止误执行。
#[gpui_kit::test]
fn empty_and_pending_states_release_navigation_but_keep_enter_and_escape(app: &mut TestAppContext) {
    app.update(crate::init);
    let initial = menu_projection(7, Vec::new());
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            for state in [
                TerminalSuggestionQueryState::Loading,
                TerminalSuggestionQueryState::Empty,
                TerminalSuggestionQueryState::Failed,
                TerminalSuggestionQueryState::Ready,
            ] {
                let mut projection = menu_projection(7, Vec::new());
                projection.state = state;
                menu.set_projection(projection, cx);
                assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx));
                assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
                assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Enter, cx));
                assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Escape, cx));
            }
            menu.set_projection(menu_projection(8, vec![candidate("first")]), cx);
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx));
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
        });
    });
}

/// 菜单导航、增量重排和首尾环绕都必须把当前高亮留在真实滚动视口内。
#[gpui_kit::test]
fn keyboard_selection_remains_visible_after_navigation_and_reordering(app: &mut TestAppContext) {
    app.update(crate::init);
    let mut candidates = (0..20)
        .map(|index| candidate(&format!("candidate-{index}")))
        .collect::<Vec<_>>();
    for (index, candidate) in candidates.iter_mut().enumerate() {
        if index % 3 == 0 {
            candidate.description =
                Some("A longer description gives this candidate a second line".into());
        }
    }
    let initial = menu_projection(7, candidates.clone());
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(260.)));
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    assert_selection_visible(&menu, visual);

    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            for _ in 0..12 {
                assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
            }
        })
    });
    assert_selection_visible(&menu, visual);
    visual.update(|_, cx| assert_eq!(menu.read(cx).selection.selected_id(), Some("candidate-12")));

    candidates.reverse();
    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            menu.set_projection(menu_projection(7, candidates.clone()), cx)
        })
    });
    assert_selection_visible(&menu, visual);
    visual.update(|_, cx| assert_eq!(menu.read(cx).selection.selected_id(), Some("candidate-12")));

    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            menu.set_projection(menu_projection(8, candidates), cx);
            menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx);
        })
    });
    assert_selection_visible(&menu, visual);
    visual.update(|_, cx| assert_eq!(menu.read(cx).selection.selected_index, Some(19)));
    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx);
        })
    });
    assert_selection_visible(&menu, visual);
    visual.update(|_, cx| assert_eq!(menu.read(cx).selection.selected_index, Some(0)));
}

/// 显式选择被增量结果挤出后回车只被消费；再次选择后才可发出接受事件。
#[gpui_kit::test]
fn enter_after_selected_candidate_eviction_waits_for_new_selection(app: &mut TestAppContext) {
    app.update(crate::init);
    let previous = (0..20)
        .map(|index| candidate(&format!("candidate-{index}")))
        .collect::<Vec<_>>();
    let initial = menu_projection(7, previous.clone());
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });

    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx));
            assert_eq!(menu.selection.selected_id(), Some("candidate-19"));
            let mut next = vec![candidate("new-high-score")];
            next.extend(previous.into_iter().take(19));
            menu.set_projection(menu_projection(7, next), cx);
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Enter, cx));
        })
    });
    visual.run_until_parked();
    assert!(events.borrow().is_empty());

    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Enter, cx));
        })
    });
    visual.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[TerminalCommandSuggestionMenuEvent::Accept {
            request_generation: 7,
            candidate_id: "new-high-score".into(),
            partial_accept: false,
        }],
    );
}

/// 重排时释放不能误选另一命令；后续正常点击仍必须接受当前行。
#[gpui_kit::test]
fn reorder_between_pointer_down_and_up_does_not_accept_another_candidate(app: &mut TestAppContext) {
    app.update(crate::init);
    let initial = menu_projection(7, vec![candidate("first"), candidate("second")]);
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(320.)));
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });
    draw_menu(visual);
    let position = visual.update(|_, cx| {
        let menu = menu.read(cx);
        let row = menu
            .scroll_handle
            .bounds_for_item(0)
            .expect("first row laid out");
        row.center() + menu.scroll_handle.offset()
    });
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            menu.set_projection(
                menu_projection(7, vec![candidate("second"), candidate("first")]),
                cx,
            );
        })
    });
    draw_menu(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    visual.run_until_parked();
    assert!(
        events.borrow().is_empty(),
        "a reordered row must not activate"
    );

    draw_menu(visual);
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_click(position, Default::default());
    visual.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[TerminalCommandSuggestionMenuEvent::Accept {
            request_generation: 7,
            candidate_id: "second".into(),
            partial_accept: false,
        }],
    );
}

/// 按下后追加内容或查询代次改变时取消旧点击，随后新点击仍可正常接受。
#[gpui_kit::test]
fn changed_append_or_generation_cancels_inflight_pointer_click(app: &mut TestAppContext) {
    app.update(crate::init);
    let projection = |generation, replacement: &str| {
        let mut item = candidate("same-id");
        item.display_text = replacement.into();
        item.replacement_text = replacement.into();
        let mut projection = menu_projection(generation, vec![item]);
        projection.selection_keys = Arc::from(vec![format!("append:{replacement}")]);
        projection
    };
    let initial = projection(7, "before");
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(320.)));
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });
    draw_menu(visual);
    let position = visual.update(|_, cx| {
        let menu = menu.read(cx);
        let row = menu.scroll_handle.bounds_for_item(0).expect("row laid out");
        row.center() + menu.scroll_handle.offset()
    });

    for next in [projection(7, "after"), projection(8, "after")] {
        visual.simulate_mouse_move(position, None, Default::default());
        visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
        visual.update(|_, cx| menu.update(cx, |menu, cx| menu.set_projection(next, cx)));
        draw_menu(visual);
        visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
        visual.run_until_parked();
        assert!(
            events.borrow().is_empty(),
            "a stale click must not activate"
        );
    }

    draw_menu(visual);
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_click(position, Default::default());
    visual.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[TerminalCommandSuggestionMenuEvent::Accept {
            request_generation: 8,
            candidate_id: "same-id".into(),
            partial_accept: false,
        }],
    );
}

/// 候选列表达到最大高度时仍须保留来源失败提示和可实际点击的重试按钮。
#[gpui_kit::test]
fn partial_failure_retry_remains_visible_and_clickable_with_many_candidates(
    app: &mut TestAppContext,
) {
    app.update(crate::init);
    let candidates = (0..20)
        .map(|index| candidate(&format!("candidate-{index}")))
        .collect();
    let mut initial = menu_projection(7, candidates);
    initial.partial_failure = true;
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(320.)));
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });
    draw_menu(visual);
    let retry_bounds = visual.update(|window, _| {
        let menu_bounds = window
            .debug_selector_bounds("terminal-command-suggestion-menu")
            .expect("menu bounds available");
        let retry = gpui_kit::base::test_support::find(
            window,
            &[],
            &"suggestion-menu-retry-partial".into(),
        )
        .expect("retry button observed");
        assert!(retry.visible(), "retry must be visible");
        let bounds = retry.bounds();
        assert!(bounds.top() >= menu_bounds.top());
        assert!(
            bounds.bottom() <= menu_bounds.bottom(),
            "retry must not be clipped"
        );
        bounds
    });
    visual.simulate_mouse_move(retry_bounds.center(), None, Default::default());
    visual.simulate_click(retry_bounds.center(), Default::default());
    visual.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[TerminalCommandSuggestionMenuEvent::Retry {
            request_generation: 7,
        }],
    );
}

/// 窄 pane、长无空格文本和 DPI 缩放下，候选正文不得挤压来源或覆盖重试与关闭操作。
#[gpui_kit::test]
fn narrow_menu_keeps_long_candidate_and_controls_inside_bounds(app: &mut TestAppContext) {
    app.update(crate::init);
    let mut first = candidate("long");
    first.provider = SuggestionProviderKind::RemoteCommand;
    first.display_text = "long-command-中文".repeat(180);
    first.description = Some("description_without_breaks_中文".repeat(100));
    let mut candidates = vec![first];
    candidates.extend((0..19).map(|index| candidate(&format!("candidate-{index}"))));
    let mut initial = menu_projection(7, candidates);
    initial.partial_failure = true;
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    let menu = slot.borrow_mut().take().expect("production menu mounted");

    for width in [220., 320.] {
        for scale in [1., 1.5] {
            visual.simulate_resize(size(px(width), px(260.)));
            visual.simulate_scale_factor_change(scale);
            draw_menu(visual);
            visual.update(|window, cx| {
                let bounds = |selector| {
                    window
                        .debug_selector_bounds(selector)
                        .expect("menu element laid out")
                };
                let menu_bounds = bounds("terminal-command-suggestion-menu");
                let row = bounds("suggestion-menu-candidate-0");
                let title = bounds("suggestion-menu-candidate-title-0");
                let provider = bounds("suggestion-menu-candidate-provider-0");
                let description = bounds("suggestion-menu-candidate-description-0");
                let close = gpui_kit::base::test_support::find(
                    window,
                    &[],
                    &"suggestion-menu-dismiss".into(),
                )
                .expect("close button available");
                let retry = gpui_kit::base::test_support::find(
                    window,
                    &[],
                    &"suggestion-menu-retry-partial".into(),
                )
                .expect("retry button available");
                let viewport = menu.read(cx).scroll_handle.bounds();
                let details = format!(
                    "width={width} scale={scale} window={:?} menu={menu_bounds:?} row={row:?} title={title:?} provider={provider:?} description={description:?} viewport={viewport:?} close={:?} retry={:?}",
                    window.viewport_size(),
                    close.bounds(),
                    retry.bounds(),
                );
                assert!(title.size.width > px(0.), "{details}");
                assert!(title.right() <= provider.left() + px(1.), "{details}");
                assert!(provider.right() <= row.right() + px(1.), "{details}");
                assert!(description.right() <= row.right() + px(1.), "{details}");
                assert!(viewport.size.height >= row.size.height, "{details}");
                assert!(close.visible() && retry.visible(), "{details}");
                assert!(close.bounds().right() <= menu_bounds.right(), "{details}");
                assert!(retry.bounds().right() <= menu_bounds.right(), "{details}");
                assert!(retry.bounds().bottom() <= menu_bounds.bottom(), "{details}");
                assert!(viewport.bottom() <= retry.bounds().top() + px(1.), "{details}");
            });
        }
    }
}

/// 重试与关闭均通过真实鼠标事件派发，按钮不得夺走终端输入焦点。
#[gpui_kit::test]
fn pointer_actions_preserve_terminal_focus(app: &mut TestAppContext) {
    app.update(crate::init);
    let mut initial = menu_projection(7, vec![candidate("first")]);
    initial.partial_failure = true;
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        let host = cx.new(|cx| MenuFocusHost {
            terminal: cx.focus_handle(),
            menu,
        });
        *mounted.borrow_mut() = Some(host.clone());
        gpui_kit::base::Root::new(host, window, cx)
    });
    visual.simulate_resize(size(px(320.), px(260.)));
    let host = slot.borrow_mut().take().expect("focus host mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|window, cx| {
        let terminal = host.read(cx).terminal.clone();
        let menu = host.read(cx).menu.clone();
        window.focus(&terminal, cx);
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });
    draw_menu(visual);
    for (button, expected) in [
        (
            "suggestion-menu-retry-partial",
            TerminalCommandSuggestionMenuEvent::Retry {
                request_generation: 7,
            },
        ),
        (
            "suggestion-menu-dismiss",
            TerminalCommandSuggestionMenuEvent::Dismiss {
                request_generation: 7,
            },
        ),
    ] {
        let position = visual.update(|window, _| {
            gpui_kit::base::test_support::find(window, &[], &button.into())
                .expect("button available")
                .bounds()
                .center()
        });
        visual.simulate_mouse_move(position, None, Default::default());
        visual.simulate_click(position, Default::default());
        visual.run_until_parked();
        assert_eq!(events.borrow().last(), Some(&expected));
        visual.update(|window, cx| {
            assert!(host.read(cx).terminal.is_focused(window));
        });
    }

    visual.update(|_, cx| {
        let menu = host.read(cx).menu.clone();
        menu.update(cx, |menu, cx| {
            let mut failed = menu_projection(8, Vec::new());
            failed.state = TerminalSuggestionQueryState::Failed;
            menu.set_projection(failed, cx);
        });
    });
    visual.simulate_resize(size(px(220.), px(260.)));
    draw_menu(visual);
    let position = visual.update(|window, _| {
        let menu_bounds = window
            .debug_selector_bounds("terminal-command-suggestion-menu")
            .expect("menu bounds available");
        let retry =
            gpui_kit::base::test_support::find(window, &[], &"suggestion-menu-retry".into())
                .expect("retry button available")
                .bounds();
        assert!(retry.right() <= menu_bounds.right());
        assert!(retry.bottom() <= menu_bounds.bottom());
        retry.center()
    });
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_click(position, Default::default());
    visual.run_until_parked();
    assert_eq!(
        events.borrow().last(),
        Some(&TerminalCommandSuggestionMenuEvent::Retry {
            request_generation: 8,
        })
    );
    visual.update(|window, cx| assert!(host.read(cx).terminal.is_focused(window)));
}

/// 能力不可用时只提供关闭与原因指引，回车不得冒充可恢复的查询重试。
#[gpui_kit::test]
fn unavailable_reasons_do_not_offer_or_emit_retry(app: &mut TestAppContext) {
    app.update(crate::init);
    let initial = menu_projection(7, Vec::new());
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    visual.simulate_resize(size(px(360.), px(320.)));
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });

    for reason in [
        ShellEditingUnavailableReason::Unsupported,
        ShellEditingUnavailableReason::IntegrationNotObserved,
        ShellEditingUnavailableReason::KeyBindingConflict,
        ShellEditingUnavailableReason::PromptNotReady,
        ShellEditingUnavailableReason::ProbeTimedOut,
        ShellEditingUnavailableReason::SessionChanged,
    ] {
        let mut projection = menu_projection(7, Vec::new());
        projection.state = TerminalSuggestionQueryState::Unavailable(reason);
        visual.update(|_, cx| {
            menu.update(cx, |menu, cx| {
                menu.set_projection(projection, cx);
                assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx));
                assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
                assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Enter, cx));
            })
        });
        draw_menu(visual);
        visual.update(|window, _| {
            assert!(
                window
                    .debug_selector_bounds("suggestion-menu-unavailable")
                    .is_some()
            );
            assert!(
                gpui_kit::base::test_support::find(window, &[], &"suggestion-menu-retry".into(),)
                    .is_none()
            );
        });
        assert!(events.borrow().is_empty());
    }
}

/// 超长输入必须先缩短再恢复，回车不得执行或触发对同一输入的重试。
#[gpui_kit::test]
fn oversized_input_does_not_offer_or_emit_retry(app: &mut TestAppContext) {
    app.update(crate::init);
    let mut initial = menu_projection(7, Vec::new());
    initial.state = TerminalSuggestionQueryState::InputTooLong;
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let menu = cx.new(|_| TerminalCommandSuggestionMenu::new(initial));
        *mounted.borrow_mut() = Some(menu.clone());
        gpui_kit::base::Root::new(menu, window, cx)
    });
    let menu = slot.borrow_mut().take().expect("production menu mounted");
    let events = Rc::new(RefCell::new(Vec::new()));
    let _observer = visual.update(|_, cx| {
        let events = events.clone();
        cx.new(|cx| MenuEventObserver {
            _subscription: cx.subscribe(
                &menu,
                move |_: &mut MenuEventObserver,
                      _,
                      event: &TerminalCommandSuggestionMenuEvent,
                      _| {
                    events.borrow_mut().push(event.clone());
                },
            ),
        })
    });
    draw_menu(visual);
    visual.update(|window, cx| {
        assert!(
            window
                .debug_selector_bounds("suggestion-menu-input-too-long")
                .is_some()
        );
        assert!(
            gpui_kit::base::test_support::find(window, &[], &"suggestion-menu-retry".into())
                .is_none()
        );
        menu.update(cx, |menu, cx| {
            assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Up, cx));
            assert!(!menu.handle_key(TerminalCommandSuggestionMenuKey::Down, cx));
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Enter, cx));
        });
    });
    visual.run_until_parked();
    assert!(events.borrow().is_empty());
    visual.update(|_, cx| {
        menu.update(cx, |menu, cx| {
            assert!(menu.handle_key(TerminalCommandSuggestionMenuKey::Escape, cx));
        });
    });
    visual.run_until_parked();
    assert_eq!(
        events.borrow().as_slice(),
        &[TerminalCommandSuggestionMenuEvent::Dismiss {
            request_generation: 7,
        }],
    );
}
