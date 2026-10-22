//! 终端搜索浮层的防抖、IME、关闭和挂起生命周期回归。
//!
//! @author kongweiguang

use super::{TerminalSearchEvent, TerminalSearchToolbar, next_search_match_index};
use gpui_kit as gpui;
use gpui_kit::{
    AppContext as _, Context, Entity, EntityInputHandler, IntoElement, Render, Subscription,
    TestAppContext, VisualTestContext, Window, div, prelude::*,
};
use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{
    TerminalPoint, TerminalSearchMatch, TerminalSearchRequest, TerminalSearchResult,
};
use std::{cell::RefCell, rc::Rc, sync::Arc, time::Duration};

struct SearchInteractionHarness {
    toolbar: Entity<TerminalSearchToolbar>,
    events: Rc<RefCell<Vec<TerminalSearchEvent>>>,
    _subscription: Subscription,
}

impl Render for SearchInteractionHarness {
    /// 测试保留生产 toolbar entity 和输入焦点，只把异步服务替换成可观察的事件订阅。
    ///
    /// @author kongweiguang
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.toolbar.clone())
    }
}

/// 初始化真实 GPUI 输入控件，并记录它发给 Root 的搜索生命周期事件。
///
/// @author kongweiguang
fn fixture(cx: &mut TestAppContext) -> (Entity<SearchInteractionHarness>, &mut VisualTestContext) {
    cx.update(|app| {
        gpui::init(app);
        app.set_reduce_motion(true);
    });
    let slot = Rc::new(RefCell::new(None));
    let returned = slot.clone();
    let (_, visual) = cx.add_window_view(move |window, cx| {
        let session_id = SessionId::new();
        let toolbar = cx.new(|cx| TerminalSearchToolbar::new(Some(session_id), window, cx));
        let events = Rc::new(RefCell::new(Vec::new()));
        let recorded_events = events.clone();
        let subscription = cx.subscribe(&toolbar, move |_, _, event, _| {
            recorded_events.borrow_mut().push(event.clone());
        });
        let harness = cx.new(|_| SearchInteractionHarness {
            toolbar,
            events,
            _subscription: subscription,
        });
        *returned.borrow_mut() = Some(harness.clone());
        gpui::base::Root::new(harness, window, cx)
    });
    let harness = slot.borrow_mut().take().expect("toolbar harness mounted");
    draw(visual);
    (harness, visual)
}

/// 驱动窗口两帧以完成焦点节点和输入处理器注册，避免测试绕过真实 GPUI 输入路径。
///
/// @author kongweiguang
fn draw(cx: &mut VisualTestContext) {
    cx.run_until_parked();
    cx.update(|window, app| window.draw(app).clear(app));
    cx.update(|window, app| window.draw(app).clear(app));
}

/// 从测试宿主取回稳定的 toolbar entity，后续替换文本仍使用生产 InputState。
///
/// @author kongweiguang
fn toolbar_entity(
    harness: &Entity<SearchInteractionHarness>,
    cx: &mut VisualTestContext,
) -> Entity<TerminalSearchToolbar> {
    cx.update(|_, app| harness.read_with(app, |harness, _| harness.toolbar.clone()))
}

/// 聚焦工具条并写入新查询；replace_all 会触发 InputEvent::Change 和生产防抖路径。
///
/// @author kongweiguang
fn replace_query(toolbar: &Entity<TerminalSearchToolbar>, query: &str, cx: &mut VisualTestContext) {
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.query.update(cx, |input, cx| {
                input.replace_all(query.to_owned(), window, cx);
            });
        });
    });
    cx.run_until_parked();
}

/// 读取 Root 边界收到的搜索请求，测试只断言请求意图和代次而不伪造服务结果。
///
/// @author kongweiguang
fn search_events(
    harness: &Entity<SearchInteractionHarness>,
    cx: &mut VisualTestContext,
) -> Vec<(u64, TerminalSearchRequest, bool)> {
    cx.update(|_, app| {
        harness.read_with(app, |harness, _| {
            harness
                .events
                .borrow()
                .iter()
                .filter_map(|event| match event {
                    TerminalSearchEvent::Search {
                        request_id,
                        request,
                        navigate,
                        ..
                    } => Some((*request_id, request.clone(), *navigate)),
                    _ => None,
                })
                .collect()
        })
    })
}

/// 仅推进 GPUI 虚拟时钟并排空当前可运行任务，准确验证 debounce 边界而不等待真实时间。
///
/// @author kongweiguang
fn advance(cx: &mut VisualTestContext, duration: Duration) {
    cx.executor().advance_clock(duration);
    cx.run_until_parked();
}

/// 提供最小非空结果，用来证明 suspended 状态会拒绝已经在途的旧 request 回投。
///
/// @author kongweiguang
fn one_match_result() -> TerminalSearchResult {
    TerminalSearchResult {
        matches: Arc::from([TerminalSearchMatch {
            range: TerminalPoint { line: 1, column: 0 }..TerminalPoint { line: 1, column: 4 },
        }]),
        selected_match: Some(0),
        truncated: false,
        projection: None,
    }
}

/// 快速编辑只提交最后的预览；防抖等待和预览扫描期间 Enter 均立即产生定位请求。
///
/// @author kongweiguang
#[gpui::test]
fn rapid_edits_preview_latest_query_and_enter_flushes_pending_or_preview(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    replace_query(&toolbar, "stale", cx);
    replace_query(&toolbar, "latest", cx);

    advance(cx, Duration::from_millis(199));
    assert!(search_events(&harness, cx).is_empty());
    advance(cx, Duration::from_millis(1));
    let searches = search_events(&harness, cx);
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].1.query, "latest");
    assert!(!searches[0].2, "debounced input must preserve the viewport");
    cx.update(|_, app| {
        toolbar.read_with(app, |toolbar, _| {
            assert_eq!(toolbar.selected_match, None);
            assert_eq!(toolbar.matches.len(), 0);
        });
    });

    replace_query(&toolbar, "enter-before-debounce", cx);
    cx.simulate_keystrokes("enter");
    let searches = search_events(&harness, cx);
    assert_eq!(searches.len(), 2);
    assert_eq!(searches[1].1.query, "enter-before-debounce");
    assert!(
        searches[1].2,
        "Enter must reveal the first match immediately"
    );
    advance(cx, Duration::from_millis(200));
    assert_eq!(search_events(&harness, cx).len(), 2);

    replace_query(&toolbar, "preview-in-flight", cx);
    advance(cx, Duration::from_millis(200));
    assert_eq!(search_events(&harness, cx).len(), 3);
    assert!(!search_events(&harness, cx)[2].2);
    cx.simulate_keystrokes("enter");
    let searches = search_events(&harness, cx);
    assert_eq!(searches.len(), 4);
    assert_eq!(searches[3].1.query, "preview-in-flight");
    assert!(
        searches[3].2,
        "Enter replaces a preview still scanning in the background"
    );
}

/// 真实 InputState marked range 验证 Esc 交给输入法、Enter 只确认组合，提交后才启动预览。
///
/// @author kongweiguang
#[gpui::test]
fn ime_composition_delays_search_and_escape_does_not_dismiss(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.query.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "组合查询", None, window, cx);
                assert!(input.marked_text_range(window, cx).is_some());
            });
        });
    });
    cx.run_until_parked();
    advance(cx, Duration::from_millis(500));
    assert!(search_events(&harness, cx).is_empty());

    // Esc 应在组合仍有效时先交给输入法；InputState 会取消标记，但不能关闭搜索浮层。
    cx.simulate_keystrokes("escape");
    assert!(search_events(&harness, cx).is_empty());
    cx.update(|_, app| {
        harness.read_with(app, |harness, _| {
            assert!(
                !harness
                    .events
                    .borrow()
                    .iter()
                    .any(|event| matches!(event, TerminalSearchEvent::Close { .. }))
            );
        });
        toolbar.read_with(app, |toolbar, _| assert!(toolbar.is_open));
    });

    // 重新开始一次真实 marked transaction，Enter 确认候选不得触发搜索导航。
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.query.update(cx, |input, cx| {
                input.set_value("", window, cx);
                input.replace_and_mark_text_in_range(None, "组合查询", None, window, cx);
                assert!(input.marked_text_range(window, cx).is_some());
            });
        });
    });
    cx.run_until_parked();
    cx.simulate_event(gpui::KeyDownEvent {
        keystroke: gpui::Keystroke::parse("enter").unwrap(),
        is_held: false,
        prefer_character_input: false,
    });
    cx.run_until_parked();
    assert!(
        search_events(&harness, cx).is_empty(),
        "IME confirmation Enter must not navigate or submit a search"
    );
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar
                .query
                .update(cx, |input, cx| input.unmark_text(window, cx));
        });
    });
    cx.run_until_parked();
    advance(cx, Duration::from_millis(50));
    assert!(search_events(&harness, cx).is_empty());
    advance(cx, Duration::from_millis(199));
    assert!(search_events(&harness, cx).is_empty());
    advance(cx, Duration::from_millis(1));
    let searches = search_events(&harness, cx);
    let state = cx.update(|window, app| toolbar.update(app, |toolbar, cx| {
        let composing = toolbar.query_is_composing(window, cx);
        format!("open={} suspended={} query={:?} composing={} rev={} submitted={:?} generation={} task={}",
            toolbar.is_open, toolbar.suspended, toolbar.query.read(cx).value(), composing,
            toolbar.query_revision, toolbar.submitted_query_revision,
            toolbar.debounce_generation, toolbar.debounce_task.is_some())
    }));
    assert_eq!(searches.len(), 1, "{state}");
    assert_eq!(searches[0].1.query, "组合查询");
    assert!(!searches[0].2);
    cx.update(|_, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.query.update(cx, |_, cx| cx.notify());
        })
    });
    cx.run_until_parked();
    advance(cx, Duration::from_millis(200));
    assert_eq!(
        search_events(&harness, cx).len(),
        1,
        "post-commit repaint must not resubmit"
    );
}

/// × 关闭不受 IME 标记限制，并撤销组合期间已启动的预览任务。
///
/// @author kongweiguang
#[gpui::test]
fn explicit_close_dismisses_ime_composition_and_cancels_pending_preview(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.query.update(cx, |input, cx| {
                input.replace_and_mark_text_in_range(None, "组合查询", None, window, cx);
                assert!(input.marked_text_range(window, cx).is_some());
            });
        });
    });
    cx.run_until_parked();

    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            assert!(toolbar.close(window, cx));
            assert!(!toolbar.is_open);
            assert!(!toolbar.query_is_composing(window, cx));
            assert_eq!(toolbar.query.read(cx).value(), "组合查询");
        });
    });
    cx.update(|_, app| {
        harness.read_with(app, |harness, _| {
            assert_eq!(
                harness
                    .events
                    .borrow()
                    .iter()
                    .filter(|event| matches!(event, TerminalSearchEvent::Close { .. }))
                    .count(),
                1
            );
        });
    });
    advance(cx, Duration::from_millis(500));
    assert!(search_events(&harness, cx).is_empty());
}

/// 关闭保留 pane 查询与选项并取消 timer；挂起可重复调用，迟到结果不能恢复高亮。
///
/// @author kongweiguang
#[gpui::test]
fn close_reopen_and_suspend_keep_state_and_reject_late_results(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    replace_query(&toolbar, "retained query", cx);
    cx.update(|_, app| {
        toolbar.update(app, |toolbar, _| {
            toolbar.case_sensitive = true;
            toolbar.regex = true;
        });
    });
    cx.simulate_keystrokes("escape");
    cx.update(|_, app| {
        toolbar.read_with(app, |toolbar, _| {
            assert!(!toolbar.is_open);
            assert_eq!(toolbar.query.read(app).value(), "retained query");
            assert!(toolbar.case_sensitive && toolbar.regex);
        });
    });
    advance(cx, Duration::from_millis(200));
    assert!(search_events(&harness, cx).is_empty());

    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    advance(cx, Duration::from_millis(200));
    let searches = search_events(&harness, cx);
    assert_eq!(searches.len(), 1);
    assert_eq!(searches[0].1.query, "retained query");
    let old_request_id = searches[0].0;
    let session_id = cx
        .update(|_, app| toolbar.read_with(app, |toolbar, _| toolbar.session_id.clone().unwrap()));

    cx.update(|_, app| {
        toolbar.update(app, |toolbar, cx| {
            assert!(toolbar.suspend_pending(cx));
            assert!(!toolbar.suspend_pending(cx));
            toolbar.set_result(
                &session_id,
                old_request_id,
                one_match_result(),
                Some(false),
                cx,
            );
            assert!(toolbar.matches.is_empty());
            assert_eq!(toolbar.query.read(cx).value(), "retained query");
            assert!(toolbar.case_sensitive && toolbar.regex);
            assert!(toolbar.is_open);
            assert!(toolbar.suspended);
        });
    });

    cx.update(|_, app| {
        toolbar.update(app, |toolbar, cx| {
            assert!(toolbar.resume_pending(cx));
            assert!(!toolbar.resume_pending(cx));
        });
    });
    advance(cx, Duration::from_millis(200));
    let searches = search_events(&harness, cx);
    assert_eq!(searches.len(), 2);
    assert_eq!(searches[1].1.query, "retained query");
    assert!(!searches[1].2);
}

/// 非法正则服务错误保留查询供修改重试，导航索引在两端环绕且不依赖方向重排结果。
///
/// @author kongweiguang
#[gpui::test]
fn failed_query_feedback_preserves_query_and_navigation_wraps(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    replace_query(&toolbar, "[invalid", cx);
    let (session_id, request_id) = cx.update(|_, app| {
        toolbar.read_with(app, |toolbar, _| {
            (toolbar.session_id.clone().unwrap(), toolbar.request_id)
        })
    });
    cx.update(|_, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.set_error(&session_id, request_id, "正则表达式无效", cx);
            assert_eq!(toolbar.error.as_deref(), Some("正则表达式无效"));
            assert_eq!(toolbar.query.read(cx).value(), "[invalid");
        });
    });
    assert_eq!(next_search_match_index(Some(0), 3, true), Some(2));
    assert_eq!(next_search_match_index(Some(2), 3, false), Some(0));
    assert!(search_events(&harness, cx).is_empty());
}

/// 置底只是当前命中的避让回退，下次显式导航重新使用顶部边距并能无焦点变化地恢复锚点。
///
/// @author kongweiguang
#[gpui::test]
fn bottom_overlay_retries_top_margin_for_next_navigation(cx: &mut TestAppContext) {
    let (harness, cx) = fixture(cx);
    let toolbar = toolbar_entity(&harness, cx);
    cx.update(|window, app| toolbar.update(app, |toolbar, cx| toolbar.focus(window, cx)));
    replace_query(&toolbar, "match", cx);
    cx.update(|window, app| {
        toolbar.update(app, |toolbar, cx| {
            toolbar.set_overlay_metrics(gpui::px(440.), 3, cx);
            toolbar.submit(false, true, cx);
            let session = toolbar.session_id.clone().unwrap();
            toolbar.set_result(
                &session,
                toolbar.request_id,
                one_match_result(),
                Some(false),
                cx,
            );
            assert_eq!(
                toolbar.overlay_placement,
                super::SearchOverlayPlacement::BottomRight
            );
            toolbar.move_match(false, cx);
            toolbar.set_result(
                &session,
                toolbar.request_id,
                one_match_result(),
                Some(true),
                cx,
            );
            assert_eq!(
                toolbar.overlay_placement,
                super::SearchOverlayPlacement::TopRight
            );
            assert!(toolbar.query_focus_handle(cx).is_focused(window));
        });
    });
    cx.run_until_parked();
    cx.update(|_, app| {
        harness.read_with(app, |harness, _| {
            assert!(harness.events.borrow().iter().any(|event| matches!(
                event,
                TerminalSearchEvent::SelectMatch {
                    safe_top_rows: 3,
                    ..
                }
            )));
        });
    });
}
