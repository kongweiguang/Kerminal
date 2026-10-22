//! 终端搜索导航刷新与结果索引环绕行为测试。
//!
//! @author kongweiguang

use super::{
    next_search_match_index, should_submit_before_navigation, submitted_search_refresh_request,
};

/// Next/Prev 在旧结果边界环绕，并把已经失效的本地索引夹到可用范围。
///
/// @author kongweiguang
#[test]
fn search_navigation_wraps_and_clamps_its_target_index() {
    assert_eq!(next_search_match_index(Some(1), 2, false), Some(0));
    assert_eq!(next_search_match_index(Some(0), 2, true), Some(1));
    assert_eq!(next_search_match_index(Some(9), 2, true), Some(0));
    assert_eq!(next_search_match_index(Some(0), 0, false), None);
}

/// 已提交查询若为零命中，后续 Enter 会启动新扫描而不是永久停留在空结果。
///
/// @author kongweiguang
#[test]
fn unchanged_zero_match_query_can_be_resubmitted() {
    assert!(should_submit_before_navigation(Some(8), 8, 0));
    assert!(!should_submit_before_navigation(Some(8), 8, 1));
    assert!(should_submit_before_navigation(Some(7), 8, 1));
}

/// 视图 revision 刷新复用最后提交的查询选项和当前位置；草稿或未提交输入不会启动重搜。
///
/// @author kongweiguang
#[test]
fn view_revision_refresh_preserves_submitted_query_options_and_selected_match() {
    let request = kerminal_terminal::snapshot::TerminalSearchRequest {
        query: "R38-130中文é".to_owned(),
        case_sensitive: true,
        regex: true,
        backwards: true,
        max_matches: 128,
    };

    assert_eq!(
        submitted_search_refresh_request(Some(7), 7, Some(&request), Some(3)),
        Some((request.clone(), Some(3))),
    );
    assert_eq!(
        submitted_search_refresh_request(Some(6), 7, Some(&request), Some(3)),
        None,
    );
    assert_eq!(
        submitted_search_refresh_request(Some(7), 7, None, Some(3)),
        None,
    );
}

/// GPUI 搜索视图的集成夹具只替代 Root 的异步服务边界；子实体订阅和结果绘制仍走生产代码。
///
/// @author kongweiguang
#[cfg(test)]
mod accepted_revision_gpui {
    use crate::features::terminal_tools::TerminalSearchToolbar;
    use crate::features::terminal_tools::{TerminalToolsEvent, TerminalToolsView};
    use crate::terminal_canvas::{TerminalCellOverlay, TerminalOverlayKind};
    use gpui::{
        AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, Styled as _,
        Subscription, TestAppContext, VisualTestContext, Window, div,
    };
    use gpui_kit as gpui;
    use kerminal_domain::identity::SessionId;
    use kerminal_domain::sessions::TerminalSize;
    use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
    use kerminal_terminal::snapshot::{
        RenderSnapshot, RenderSnapshotRequest, TerminalCursor, TerminalCursorShape, TerminalModes,
        TerminalPalette, TerminalPoint, TerminalRow, TerminalSearchMatch, TerminalSearchRequest,
        TerminalSearchResult,
    };
    use std::{cell::RefCell, ops::Range, rc::Rc, sync::Arc, time::Duration};

    struct SearchRefreshHarness {
        tools: Entity<TerminalToolsView>,
        session_id: SessionId,
        events: Rc<RefCell<Vec<TerminalToolsEvent>>>,
        _event_subscription: Subscription,
    }

    impl SearchRefreshHarness {
        /// Keep the production tools and toolbar entities while replacing only Root's search service.
        /// This lets the test observe the real typed event and feed its response back by request id.
        ///
        /// @author kongweiguang
        fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
            let session_id = SessionId::new();
            Self::with_binding(session_id.clone(), Some(session_id), window, cx)
        }

        /// 构造可模拟 Agent 初始无目标的真实工具视图，同时保留后续绑定所需的会话身份。
        ///
        /// @author kongweiguang
        fn new_unbound(window: &mut Window, cx: &mut Context<Self>) -> Self {
            Self::with_binding(SessionId::new(), None, window, cx)
        }

        /// 让夹具保留同一事件订阅，只替换工具视图的初始 binding 状态以复现目标恢复流程。
        ///
        /// @author kongweiguang
        fn with_binding(
            session_id: SessionId,
            initial_binding: Option<SessionId>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) -> Self {
            let tools = cx.new(|cx| TerminalToolsView::new(initial_binding, window, cx));
            let events = Rc::new(RefCell::new(Vec::new()));
            let recorded_events = events.clone();
            let event_subscription = cx.subscribe(&tools, move |_, _, event, _| {
                recorded_events.borrow_mut().push(event.clone());
            });
            Self {
                tools,
                session_id,
                events,
                _event_subscription: event_subscription,
            }
        }
    }

    impl Render for SearchRefreshHarness {
        /// Mount the tools and the same search entity returned to the terminal canvas overlay.
        ///
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let mut root = div().size_full().child(self.tools.clone());
            if let Some(search) = self.tools.read(cx).search_overlay() {
                root = root.child(search);
            }
            root
        }
    }

    /// Initialize the same GPUI Kit root used by native windows so entity event subscriptions run.
    ///
    /// @author kongweiguang
    fn fixture(cx: &mut TestAppContext) -> (Entity<SearchRefreshHarness>, &mut VisualTestContext) {
        fixture_with_binding(cx, true)
    }

    /// 以无 session 的生产视图起步，覆盖 Agent 创建后目标稍晚绑定的生命周期。
    ///
    /// @author kongweiguang
    fn unbound_fixture(
        cx: &mut TestAppContext,
    ) -> (Entity<SearchRefreshHarness>, &mut VisualTestContext) {
        fixture_with_binding(cx, false)
    }

    /// 同一绘制与订阅路径仅切换初始 binding，避免用简化 mock 掩盖 request-id 接线问题。
    ///
    /// @author kongweiguang
    fn fixture_with_binding(
        cx: &mut TestAppContext,
        initially_bound: bool,
    ) -> (Entity<SearchRefreshHarness>, &mut VisualTestContext) {
        cx.update(|app| {
            gpui::init(app);
            app.set_reduce_motion(true);
        });
        let slot = Rc::new(RefCell::new(None));
        let returned = slot.clone();
        let (_, visual) = cx.add_window_view(move |window, cx| {
            let harness = cx.new(|cx| {
                if initially_bound {
                    SearchRefreshHarness::new(window, cx)
                } else {
                    SearchRefreshHarness::new_unbound(window, cx)
                }
            });
            *returned.borrow_mut() = Some(harness.clone());
            gpui::base::Root::new(harness, window, cx)
        });
        let harness = slot.borrow_mut().take().expect("search harness mounted");
        draw(visual);
        (harness, visual)
    }

    /// 只推进 GPUI 虚拟时钟，验证搜索防抖和异步事件的真实边界而不等待墙钟。
    ///
    /// @author kongweiguang
    fn advance(cx: &mut VisualTestContext, duration: Duration) {
        cx.executor().advance_clock(duration);
        cx.run_until_parked();
    }

    /// Flush entity events and draw twice to settle the search overlay mount before input mutation.
    ///
    /// @author kongweiguang
    fn draw(cx: &mut VisualTestContext) {
        cx.run_until_parked();
        cx.update(|window, app| window.draw(app).clear(app));
        cx.update(|window, app| window.draw(app).clear(app));
    }

    /// Return the existing toolbar entity only while the production view considers search visible.
    ///
    /// @author kongweiguang
    fn search_toolbar(
        harness: &Entity<SearchRefreshHarness>,
        cx: &mut VisualTestContext,
    ) -> Entity<TerminalSearchToolbar> {
        cx.update(|_, app| {
            harness.read_with(app, |harness, app| {
                harness
                    .tools
                    .read(app)
                    .search_overlay()
                    .expect("search overlay is visible")
            })
        })
    }

    /// Replace through InputState's editing path so the production Change subscription invalidates the query.
    /// The silent set_value initialization API would bypass the user-edit event on subsequent queries.
    ///
    /// @author kongweiguang
    fn submit_query(
        toolbar: &Entity<TerminalSearchToolbar>,
        query: &str,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, app| {
            toolbar.update(app, |toolbar, cx| {
                toolbar.query.update(cx, |input, cx| {
                    input.replace_all(query.to_owned(), window, cx);
                });
            });
        });
        cx.run_until_parked();
        cx.update(|_, app| {
            toolbar.update(app, |toolbar, cx| toolbar.navigate_or_submit(false, cx));
        });
        cx.run_until_parked();
    }

    /// 通过真实 InputState 编辑但不触发 Enter，确保测试经过预览防抖而非直接提交捷径。
    ///
    /// @author kongweiguang
    fn edit_query(
        toolbar: &Entity<TerminalSearchToolbar>,
        query: &str,
        cx: &mut VisualTestContext,
    ) {
        cx.update(|window, app| {
            toolbar.update(app, |toolbar, cx| {
                toolbar.query.update(cx, |input, cx| {
                    input.replace_all(query.to_owned(), window, cx);
                });
            });
        });
        cx.run_until_parked();
    }

    /// Build two real renderer snapshots with a size and revision change but stable absolute hit coordinates.
    ///
    /// @author kongweiguang
    fn snapshot(session_id: &SessionId, revision: u64, size: TerminalSize) -> RenderSnapshot {
        RenderSnapshot {
            session_id: session_id.clone(),
            revision,
            size,
            scrollback_offset: 0,
            history_size: 0,
            scroll_origin: 0,
            grid_epoch: 1,
            title: None,
            current_working_directory: None,
            cursor: TerminalCursor {
                row: 0,
                column: 0,
                shape: TerminalCursorShape::Block,
                style_from_terminal: false,
                blinking: false,
                visible: false,
            },
            cursor_cell: None,
            modes: TerminalModes::default(),
            palette: TerminalPalette::default(),
            rows: Arc::from([TerminalRow {
                viewport_row: 0,
                absolute_line: 42,
                wrapped: false,
                runs: Arc::from([]),
            }]),
            dirty_rows: Arc::from([]),
            selection: None,
            search_matches: Arc::from([]),
        }
    }

    /// Return a deterministic hit whose range exposes both the active-search kind and cell bounds.
    ///
    /// @author kongweiguang
    fn result() -> TerminalSearchResult {
        TerminalSearchResult {
            matches: Arc::from([TerminalSearchMatch {
                range: TerminalPoint {
                    line: 42,
                    column: 2,
                }..TerminalPoint {
                    line: 42,
                    column: 8,
                },
            }]),
            selected_match: Some(0),
            truncated: false,
            projection: None,
        }
    }

    /// Agent 初始无 session 时挂起后再绑定，预览事件必须仍通过 view 的精确 request-id 门禁。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn unbound_agent_suspend_bind_resume_emits_preview_and_accepts_its_receipt(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = unbound_fixture(cx);
        let session_id =
            cx.update(|_, app| harness.read_with(app, |harness, _| harness.session_id.clone()));
        let tools = cx.update(|_, app| harness.read_with(app, |harness, _| harness.tools.clone()));

        cx.update(|window, app| {
            harness.update(app, |harness, cx| {
                assert!(
                    harness
                        .tools
                        .update(cx, |tools, cx| { tools.set_search_suspended(true, cx) })
                );
                assert!(
                    !harness
                        .tools
                        .update(cx, |tools, cx| { tools.set_search_suspended(true, cx) })
                );
                harness.tools.update(cx, |tools, cx| {
                    tools.set_session(Some(session_id.clone()), window, cx);
                    // set_session clears the previous suspended state while advancing both request gates.
                    assert!(!tools.set_search_suspended(false, cx));
                    assert!(tools.open_search_for(&session_id, window, cx));
                });
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        edit_query(&toolbar, "agent-preview", cx);

        advance(cx, Duration::from_millis(199));
        assert!(cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness.events.borrow().iter().all(|event| {
                    !matches!(
                        event,
                        TerminalToolsEvent::Search { request, .. }
                            if request.query == "agent-preview"
                    )
                })
            })
        }));
        advance(cx, Duration::from_millis(1));

        let request_id =
            cx.update(|_, app| toolbar.read_with(app, |toolbar, _| toolbar.request_id));

        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&session_id, request_id, result(), 7, None, cx);
            });
        });
        let settled = cx.update(|_, app| {
            toolbar.read_with(app, |toolbar, _| {
                !toolbar.busy && toolbar.matches.len() == 1 && toolbar.selected_match == Some(0)
            })
        });
        let preview_forwarded = cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness.events.borrow().iter().any(|event| {
                    matches!(
                        event,
                        TerminalToolsEvent::Search {
                            session_id: emitted_session,
                            request_id: emitted_id,
                            request: emitted_request,
                            navigate: false,
                            ..
                        } if emitted_session == &session_id
                            && *emitted_id == request_id
                            && emitted_request.query == "agent-preview"
                    )
                })
            })
        });
        assert!(
            preview_forwarded && settled,
            "preview forwarded={preview_forwarded}; matching response settled={settled}"
        );
    }

    /// 已绑定 pane 挂起会同步推进视图门禁；旧回执在恢复后的新请求期间不得清除忙碌态。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn bound_suspend_resume_is_idempotent_and_rejects_stale_search_receipt(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = fixture(cx);
        let session_id =
            cx.update(|_, app| harness.read_with(app, |harness, _| harness.session_id.clone()));
        let tools = cx.update(|_, app| harness.read_with(app, |harness, _| harness.tools.clone()));
        cx.update(|window, app| {
            tools.update(app, |tools, cx| {
                assert!(tools.open_search_for(&session_id, window, cx));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        edit_query(&toolbar, "current", cx);
        advance(cx, Duration::from_millis(200));
        let (first_request_id, _) = latest_search_submission(&harness, cx);
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&session_id, first_request_id, result(), 9, None, cx);
            });
        });

        cx.update(|_, app| {
            assert!(tools.update(app, |tools, cx| tools.set_search_suspended(true, cx)));
            assert!(!tools.update(app, |tools, cx| tools.set_search_suspended(true, cx)));
        });
        cx.run_until_parked();
        cx.update(|_, app| {
            assert!(tools.update(app, |tools, cx| tools.set_search_suspended(false, cx)));
        });
        advance(cx, Duration::from_millis(200));
        let (second_request_id, second_request) = latest_search_submission(&harness, cx);
        assert_ne!(second_request_id, first_request_id);
        assert_eq!(second_request.query, "current");

        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&session_id, first_request_id, result(), 10, None, cx);
            });
        });
        cx.update(|_, app| {
            toolbar.read_with(app, |toolbar, _| {
                assert!(
                    toolbar.busy,
                    "stale response must not settle the current request"
                );
            });
        });
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&session_id, second_request_id, result(), 10, None, cx);
            });
        });
        cx.update(|_, app| {
            toolbar.read_with(app, |toolbar, _| {
                assert!(
                    !toolbar.busy,
                    "current response must settle after stale rejection"
                );
            });
        });
    }

    /// 同一 app update 内挂起旧 session 并切换目标；异步旧 Clear 和旧结果都不能错位或污染新搜索。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn bound_suspend_then_immediate_target_switch_keeps_request_gate_aligned(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = fixture(cx);
        let old_session =
            cx.update(|_, app| harness.read_with(app, |harness, _| harness.session_id.clone()));
        let new_session = SessionId::new();
        let tools = cx.update(|_, app| harness.read_with(app, |harness, _| harness.tools.clone()));
        cx.update(|window, app| {
            tools.update(app, |tools, cx| {
                assert!(tools.open_search_for(&old_session, window, cx));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        edit_query(&toolbar, "old-target", cx);
        advance(cx, Duration::from_millis(200));
        let (old_request_id, old_request) = latest_search_submission(&harness, cx);
        assert_eq!(old_request.query, "old-target");

        // 两个运行态变更处于同一个 Entity update，中间刻意不排空排队的旧 session Clear。
        cx.update(|window, app| {
            tools.update(app, |tools, cx| {
                assert!(tools.set_search_suspended(true, cx));
                tools.set_session(Some(new_session.clone()), window, cx);
                assert!(tools.open_search_for(&new_session, window, cx));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        edit_query(&toolbar, "new-target", cx);
        advance(cx, Duration::from_millis(200));

        let current_search = cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness
                    .events
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        TerminalToolsEvent::Search {
                            session_id,
                            request_id,
                            request,
                            navigate: false,
                            ..
                        } if session_id == &new_session => Some((*request_id, request.clone())),
                        _ => None,
                    })
                    .expect("the new session preview must pass the strict request gate")
            })
        });
        assert_eq!(current_search.1.query, "new-target");
        assert_ne!(current_search.0, old_request_id);

        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&old_session, old_request_id, result(), 11, None, cx);
            });
        });
        cx.update(|_, app| {
            toolbar.read_with(app, |toolbar, _| {
                assert!(
                    toolbar.busy,
                    "old-session response must not settle the new request"
                );
                assert!(toolbar.matches.is_empty());
            });
        });

        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(&new_session, current_search.0, result(), 12, None, cx);
            });
        });
        cx.update(|_, app| {
            toolbar.read_with(app, |toolbar, _| {
                assert!(
                    !toolbar.busy,
                    "current-session response must settle normally"
                );
                assert_eq!(toolbar.matches.len(), 1);
            });
        });
    }

    /// After Root accepts a resized snapshot, the search entity emits RefreshSearch; the service
    /// response then repopulates the real Canvas overlay projection for that exact revision.
    /// The Root SessionCommandPump/acceptance callback itself remains outside this isolated view fixture.
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn accepted_resize_revision_refreshes_search_and_canvas_overlay(cx: &mut TestAppContext) {
        let (harness, cx) = fixture(cx);
        let session_id =
            cx.update(|_, app| harness.read_with(app, |harness, _| harness.session_id.clone()));
        cx.update(|window, app| {
            harness.update(app, |harness, cx| {
                let session_id = harness.session_id.clone();
                assert!(harness.tools.update(cx, |tools, cx| {
                    tools.open_search_for(&session_id, window, cx)
                }));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        submit_query(&toolbar, "needle", cx);

        let initial_search = cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness
                    .events
                    .borrow()
                    .iter()
                    .find_map(|event| match event {
                        TerminalToolsEvent::Search {
                            session_id,
                            request_id,
                            request,
                            ..
                        } => Some((session_id.clone(), *request_id, request.clone())),
                        _ => None,
                    })
                    .expect("submitted search event is forwarded by TerminalToolsView")
            })
        });
        assert_eq!(initial_search.0, session_id);
        assert_eq!(
            initial_search.2,
            TerminalSearchRequest {
                query: "needle".to_owned(),
                case_sensitive: false,
                regex: false,
                backwards: false,
                max_matches: 512,
            }
        );

        let old_size = TerminalSize::new(80, 24, 0, 0).expect("old fixture size is valid");
        let resized_size = TerminalSize::new(100, 30, 0, 0).expect("resized fixture size is valid");
        let old_snapshot = snapshot(&session_id, 100, old_size);
        let resized_snapshot = snapshot(&session_id, 101, resized_size);
        let tools = cx.update(|_, app| harness.read_with(app, |harness, _| harness.tools.clone()));
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(
                    &session_id,
                    initial_search.1,
                    result(),
                    old_snapshot.revision,
                    None,
                    cx,
                );
            });
        });
        let initial_overlay = cx.update(|_, app| tools.read(app).render_overlays(&old_snapshot));
        assert_eq!(
            initial_overlay.as_ref(),
            &[TerminalCellOverlay {
                range: TerminalPoint {
                    line: 42,
                    column: 2,
                }..TerminalPoint {
                    line: 42,
                    column: 8,
                },
                kind: TerminalOverlayKind::ActiveSearch,
                style: None,
            }]
        );

        // This is the production view entry invoked after Root accepts the post-ACK snapshot.
        cx.update(|_, app| {
            assert!(tools.update(app, |tools, cx| {
                tools.refresh_search_after_view_revision(&session_id, cx)
            }));
        });
        cx.run_until_parked();
        let refresh = cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness
                    .events
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        TerminalToolsEvent::RefreshSearch {
                            session_id,
                            request_id,
                            request,
                            selected_match,
                        } => Some((
                            session_id.clone(),
                            *request_id,
                            request.clone(),
                            *selected_match,
                        )),
                        _ => None,
                    })
                    .expect("resize refresh is forwarded as a typed search event")
            })
        });
        assert_eq!(refresh.0, session_id);
        assert_eq!(refresh.2, initial_search.2);
        assert_eq!(refresh.3, Some(0));
        assert_ne!(refresh.1, initial_search.1);
        assert!(cx.update(|_, app| tools.read(app).render_overlays(&old_snapshot).is_empty()));

        // The fake service boundary returns the new engine revision under the refresh request id.
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(
                    &session_id,
                    refresh.1,
                    result(),
                    resized_snapshot.revision,
                    None,
                    cx,
                );
            });
        });
        let resized_overlay =
            cx.update(|_, app| tools.read(app).render_overlays(&resized_snapshot));
        assert_eq!(
            resized_overlay.as_ref(),
            &[TerminalCellOverlay {
                range: TerminalPoint {
                    line: 42,
                    column: 2,
                }..TerminalPoint {
                    line: 42,
                    column: 8,
                },
                kind: TerminalOverlayKind::ActiveSearch,
                style: None,
            }]
        );
        assert!(cx.update(|_, app| { tools.read(app).render_overlays(&old_snapshot).is_empty() }));
    }
    /// 引擎与 UI 共用会话身份；真实 VT 行证据用于跨输出 revision 保留命中并拒绝被覆写的行。
    ///
    /// @author kongweiguang
    fn engine_with_searchable_output(session_id: &SessionId) -> TerminalEngine {
        let size = TerminalSize::new(24, 5, 0, 0).expect("fixture terminal size is valid");
        let mut engine = TerminalEngine::new(session_id.clone(), size, TerminalLimits::default())
            .expect("fixture engine respects terminal limits");
        engine.process_output(b"top\r\nneedle\r\nfresh");
        engine
    }

    /// 从真实子控件事件链取最近提交的查询，忽略输入编辑或关闭时伴随的 Clear/Close 事件。
    ///
    /// @author kongweiguang
    fn latest_search_submission(
        harness: &Entity<SearchRefreshHarness>,
        cx: &mut VisualTestContext,
    ) -> (u64, TerminalSearchRequest) {
        cx.update(|_, app| {
            harness.read_with(app, |harness, _| {
                harness
                    .events
                    .borrow()
                    .iter()
                    .rev()
                    .find_map(|event| match event {
                        TerminalToolsEvent::Search {
                            request_id,
                            request,
                            ..
                        } => Some((*request_id, request.clone())),
                        _ => None,
                    })
                    .expect("submitted query reaches the Root event boundary")
            })
        })
    }

    /// 按 overlay 类型和精确 cell 范围检查 Canvas 实际消费的数据，不依赖主题颜色取值。
    ///
    /// @author kongweiguang
    fn active_search_range(overlays: &[TerminalCellOverlay]) -> Option<Range<TerminalPoint>> {
        overlays
            .iter()
            .find(|overlay| overlay.kind == TerminalOverlayKind::ActiveSearch)
            .map(|overlay| overlay.range.clone())
    }

    /// 串联真实引擎输出、搜索投影、GPUI 事件、结果回投和 Canvas overlay；新查询、关闭及覆写行均验证缓存失效。
    ///
    /// @author kongweiguang
    #[gpui::test]
    fn engine_search_projection_tracks_stable_rows_and_rejects_stale_cache(
        cx: &mut TestAppContext,
    ) {
        let (harness, cx) = fixture(cx);
        let session_id =
            cx.update(|_, app| harness.read_with(app, |harness, _| harness.session_id.clone()));
        let mut engine = engine_with_searchable_output(&session_id);
        cx.update(|window, app| {
            harness.update(app, |harness, cx| {
                let session_id = harness.session_id.clone();
                assert!(harness.tools.update(cx, |tools, cx| {
                    tools.open_search_for(&session_id, window, cx)
                }));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        let tools = cx.update(|_, app| harness.read_with(app, |harness, _| harness.tools.clone()));

        submit_query(&toolbar, "needle", cx);
        let (needle_request_id, needle_request) = latest_search_submission(&harness, cx);
        let original_snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
        let needle_result = engine
            .search(&needle_request)
            .expect("real terminal engine searches its output");
        assert!(needle_result.projection.is_some());
        let original_projection = needle_result
            .project_visible_matches(&original_snapshot)
            .expect("the matching original row is visible and unchanged");
        assert_eq!(original_projection.len(), 1);
        assert_eq!(original_projection[0].0, 0);
        assert_eq!(original_projection[0].1, needle_result.matches[0]);
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(
                    &session_id,
                    needle_request_id,
                    needle_result.clone(),
                    original_snapshot.revision,
                    None,
                    cx,
                );
            });
        });
        assert_eq!(
            active_search_range(
                &cx.update(|_, app| {
                    tools.read(app).render_overlays(&original_snapshot).to_vec()
                })
            ),
            Some(needle_result.matches[0].range.clone())
        );

        // 在其他行写入会推进 render revision，但搜索行证据不变；命中应重新投影并保留。
        engine.process_output(b"\x1b[5;1Htail");
        let stable_row_snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
        assert_ne!(original_snapshot.revision, stable_row_snapshot.revision);
        let stable_projection = needle_result
            .project_visible_matches(&stable_row_snapshot)
            .expect("unrelated output preserves the searched row witness");
        assert_eq!(stable_projection.len(), 1);
        assert_eq!(
            active_search_range(&cx.update(|_, app| {
                tools
                    .read(app)
                    .render_overlays(&stable_row_snapshot)
                    .to_vec()
            })),
            Some(stable_projection[0].1.range.clone())
        );

        // 新查询结果尚未返回时，先使旧结果及缓存失效；即使 Canvas 重绘同一快照也不能复用旧高亮。
        cx.update(|_, app| {
            let toolbar = toolbar.read(app);
            assert!(
                !toolbar.busy,
                "accepted request {needle_request_id} must clear toolbar busy (current request {})",
                toolbar.request_id,
            );
        });
        submit_query(&toolbar, "fresh", cx);
        let (fresh_request_id, fresh_request) = latest_search_submission(&harness, cx);
        assert_eq!(fresh_request.query, "fresh");
        assert!(cx.update(|_, app| {
            tools
                .read(app)
                .render_overlays(&stable_row_snapshot)
                .is_empty()
        }));
        let fresh_result = engine
            .search(&fresh_request)
            .expect("real terminal engine searches the second query");
        assert!(
            fresh_result
                .project_visible_matches(&stable_row_snapshot)
                .is_some()
        );
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(
                    &session_id,
                    fresh_request_id,
                    fresh_result.clone(),
                    stable_row_snapshot.revision,
                    None,
                    cx,
                );
            });
        });
        assert_eq!(
            active_search_range(&cx.update(|_, app| {
                tools
                    .read(app)
                    .render_overlays(&stable_row_snapshot)
                    .to_vec()
            })),
            Some(fresh_result.matches[0].range.clone())
        );

        // 关闭操作经过真实工具栏事件订阅；已有投影缓存也必须先清除，再绘制同一快照。
        cx.update(|window, app| {
            toolbar.update(app, |toolbar, cx| toolbar.close(window, cx));
        });
        cx.run_until_parked();
        assert!(cx.update(|_, app| {
            tools
                .read(app)
                .render_overlays(&stable_row_snapshot)
                .is_empty()
        }));

        // 重开搜索并重新查询原文本，再用 VT 覆写对应行；旧结果仍在视图时，变化的行证据也必须阻止旧高亮。
        cx.update(|window, app| {
            tools.update(app, |tools, cx| {
                assert!(tools.open_search_for(&session_id, window, cx));
            });
        });
        draw(cx);
        let toolbar = search_toolbar(&harness, cx);
        submit_query(&toolbar, "needle", cx);
        let (latest_needle_request_id, latest_needle_request) =
            latest_search_submission(&harness, cx);
        let latest_needle_result = engine
            .search(&latest_needle_request)
            .expect("needle still exists before overwrite");
        cx.update(|_, app| {
            tools.update(app, |tools, cx| {
                tools.set_search_result(
                    &session_id,
                    latest_needle_request_id,
                    latest_needle_result.clone(),
                    stable_row_snapshot.revision,
                    None,
                    cx,
                );
            });
        });
        assert_eq!(
            active_search_range(&cx.update(|_, app| {
                tools
                    .read(app)
                    .render_overlays(&stable_row_snapshot)
                    .to_vec()
            })),
            Some(latest_needle_result.matches[0].range.clone())
        );
        engine.process_output(b"\x1b[2;1Hxxxxxx");
        let overwritten_snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
        assert_ne!(stable_row_snapshot.revision, overwritten_snapshot.revision);
        assert!(cx.update(|_, app| {
            tools
                .read(app)
                .render_overlays(&overwritten_snapshot)
                .is_empty()
        }));
    }
}
