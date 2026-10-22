// @author kongweiguang

use super::*;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::{PaneId, TabId};
use kerminal_domain::sessions::{SessionOwner, TerminalSize};
use kerminal_terminal::snapshot::{
    RenderSnapshot, SelectionViewport, TerminalAttributes, TerminalColor, TerminalCursor,
    TerminalCursorShape, TerminalModes, TerminalPalette, TerminalRowWitness, TerminalRun,
    TerminalStyle, TerminalTextCellSpan, UnderlineStyle,
};
use std::sync::Arc;

/// 创建固定的 UI session 标识，令回归只验证拖动回执策略而不依赖存储或窗口。
///
/// @author kongweiguang
fn fixture_session() -> SessionId {
    SessionId::parse("selection-clear-test-session").expect("fixture session ID is valid")
}

/// 用稳定 UI owner 和 generation 测试旧拖动回调不能作用于重连后的新目标。
///
/// @author kongweiguang
fn fixture_binding(session_id: SessionId, session_generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id,
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("selection-clear-test-tab").expect("fixture tab ID is valid"),
            pane_id: PaneId::parse("selection-clear-test-pane").expect("fixture pane ID is valid"),
        },
        session_generation,
    }
}

/// 用真实行文本与列跨度构造 viewport 证据，覆盖持续输出但鼠标仍停在同一 cell 的情况。
///
/// @author kongweiguang
fn fixture_selection_viewport(
    scrollback_offset: usize,
    scroll_origin: u64,
    grid_epoch: u64,
    text: &str,
) -> SelectionViewport {
    let length = u16::try_from(text.chars().count()).expect("fixture row fits terminal columns");
    let run = TerminalRun {
        start_column: 0,
        end_column: length,
        text: Arc::from(text),
        cell_spans: Arc::from([TerminalTextCellSpan {
            byte_range: 0..text.len(),
            column_range: 0..length,
        }]),
        style: TerminalStyle {
            foreground: TerminalColor::DefaultForeground,
            background: TerminalColor::DefaultBackground,
            underline_color: None,
            attributes: TerminalAttributes {
                underline: UnderlineStyle::None,
                ..TerminalAttributes::default()
            },
        },
        hyperlink: None,
    };
    SelectionViewport {
        size: TerminalSize::new(10, 4, 100, 80).expect("fixture terminal size is valid"),
        scrollback_offset,
        scroll_origin,
        grid_epoch,
        endpoint_row_witness: Some(Arc::new(TerminalRowWitness::from_runs(false, &[run]))),
    }
}

/// 确认点、viewport 代次和行证据取自同一快照，并按该尺寸夹住列坐标。
///
/// @author kongweiguang
#[test]
fn terminal_point_projection_keeps_its_viewport_metadata() {
    let size = TerminalSize::new(10, 4, 100, 80).expect("fixture terminal size is valid");
    let snapshot = RenderSnapshot {
        session_id: fixture_session(),
        revision: 81,
        size,
        scrollback_offset: 37,
        history_size: 90,
        scroll_origin: 1200,
        grid_epoch: 4,
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
            viewport_row: 2,
            absolute_line: -35,
            wrapped: true,
            runs: Arc::from(Vec::new()),
        }]),
        dirty_rows: Arc::from([]),
        selection: None,
        search_matches: Arc::from([]),
    };

    let (point, viewport) = terminal_point_and_viewport_from_cell(
        &snapshot,
        ViewportCell {
            row: 2,
            column: u16::MAX,
        },
    )
    .expect("the sparse snapshot contains the requested row");

    assert_eq!(
        point,
        TerminalPoint {
            line: -35,
            column: 9
        }
    );
    assert_eq!(viewport.size, size);
    assert_eq!(viewport.scrollback_offset, 37);
    assert_eq!(viewport.scroll_origin, 1200);
    assert_eq!(viewport.grid_epoch, 4);
    let witness = viewport
        .endpoint_row_witness
        .expect("the endpoint row in this snapshot has stable evidence");
    assert!(witness.wrapped);
    assert_eq!(&*witness.text, "");
}

/// 空白普通单击仅在 MouseUp 完成同点手势后才形成清理决定。
///
/// @author kongweiguang
#[test]
fn blank_plain_single_click_completes_deferred_clear_intent() {
    let session = fixture_session();
    let start = TerminalPoint { line: 4, column: 7 };
    let drag = ViewportSelectionDrag::new_with_clear_selection_on_click(
        session.clone(),
        start,
        false,
        true,
    );

    let completed = drag
        .finish_after_release(&session, start, true)
        .expect("matching session completes the click");

    assert!(completed.selection.is_none());
    assert!(completed.clear_selection);
}

/// 从空白起拖入文本必须只提交新范围，不能再发可能覆盖它的旧选择清除。
///
/// @author kongweiguang
#[test]
fn dragging_from_blank_cell_keeps_selection_and_suppresses_clear() {
    let session = fixture_session();
    let start = TerminalPoint { line: 4, column: 7 };
    let end = TerminalPoint {
        line: 4,
        column: 11,
    };
    let drag = ViewportSelectionDrag::new_with_clear_selection_on_click(
        session.clone(),
        start,
        false,
        true,
    );

    let completed = drag
        .finish_after_release(&session, end, true)
        .expect("matching session completes the drag");

    assert!(completed.selection.is_some());
    assert!(!completed.clear_selection);
}

/// release 出现修饰键或按下时没有核准清除意图时，普通 selection 不被清空。
///
/// @author kongweiguang
#[test]
fn modified_release_or_unqualified_press_suppresses_deferred_clear() {
    let session = fixture_session();
    let point = TerminalPoint { line: 2, column: 3 };

    let modified_release = ViewportSelectionDrag::new_with_clear_selection_on_click(
        session.clone(),
        point,
        false,
        true,
    )
    .finish_after_release(&session, point, false)
    .expect("matching session completes the click");
    let unqualified_press = ViewportSelectionDrag::new_with_clear_selection_on_click(
        session.clone(),
        point,
        false,
        false,
    )
    .finish_after_release(&session, point, true)
    .expect("matching session completes the click");

    assert!(!modified_release.clear_selection);
    assert!(!unqualified_press.clear_selection);
}

/// 旧 pane 的按下意图不能在目标 session 已切换后完成或清除新 pane 的选择。
///
/// @author kongweiguang
#[test]
fn selection_drag_rejects_a_different_session_at_release() {
    let session = fixture_session();
    let other =
        SessionId::parse("selection-clear-other-session").expect("fixture session ID is valid");
    let point = TerminalPoint { line: 2, column: 3 };
    let drag =
        ViewportSelectionDrag::new_with_clear_selection_on_click(session, point, false, true);

    assert!(drag.finish_after_release(&other, point, true).is_none());
}

/// 双击词、三击行与 Alt 矩形即便端点相同也必须形成可提交的非字符选择。
///
/// @author kongweiguang
#[test]
fn semantic_click_and_rectangular_gestures_keep_their_selection_kind() {
    let session = fixture_session();
    let binding = fixture_binding(session.clone(), 3);
    let cell = ViewportCell { row: 4, column: 7 };
    let point = TerminalPoint {
        line: 40,
        column: 7,
    };

    for kind in [
        TerminalSelectionKind::Word,
        TerminalSelectionKind::Line,
        TerminalSelectionKind::Block,
    ] {
        let mut drag = ViewportSelectionDrag::for_canvas_gesture(
            binding.clone(),
            1,
            101,
            point,
            cell,
            kind,
            false,
            false,
        );
        let viewport = fixture_selection_viewport(0, 1, 1, "semantic");
        // 首次观察当前投影建立去重基线；后续同点同 viewport 不重复发事件。
        assert!(drag.update(point, cell, 0, &viewport).is_some());
        assert!(drag.update(point, cell, 0, &viewport).is_none());
        let completed = drag
            .finish_after_release(&session, point, false)
            .expect("matching selection gesture completes");
        assert_eq!(
            completed.selection.map(|selection| selection.kind),
            Some(kind)
        );
    }
}

/// 实时端点变化只记录最新 cell 和边缘方向，并继续保留绝对起点供 scrollback 重映射。
///
/// @author kongweiguang
#[test]
fn live_gesture_tracks_absolute_endpoint_and_edge_direction() {
    let session = fixture_session();
    let binding = fixture_binding(session, 8);
    let start = TerminalPoint {
        line: 100,
        column: 2,
    };
    let cell = ViewportCell { row: 0, column: 3 };
    let mut drag = ViewportSelectionDrag::for_canvas_gesture(
        binding.clone(),
        11,
        211,
        start,
        cell,
        TerminalSelectionKind::Character,
        false,
        false,
    );
    let end = TerminalPoint {
        line: 99,
        column: 3,
    };
    let viewport = fixture_selection_viewport(0, 1, 1, "prompt");

    let live = drag
        .update(end, cell, -4, &viewport)
        .expect("a changed endpoint yields a live selection");
    let frame = drag
        .frame_state(&binding, 11)
        .expect("current owner and generation remain active");

    assert_eq!(live.start, start);
    assert_eq!(live.end, end);
    assert_eq!(frame.start, start);
    assert_eq!(frame.cell, cell);
    assert_eq!(frame.edge_direction, -1);
    assert!(drag.frame_state(&binding, 10).is_none());
    assert!(
        drag.frame_state(&fixture_binding(binding.session_id.clone(), 9), 11)
            .is_none()
    );
}

/// 同一 cell 的连续输出会改变 endpoint 投影，必须重发；稳定帧随后恢复去重。
///
/// @author kongweiguang
#[test]
fn same_cell_continuous_output_refreshes_projection_without_repeating_stable_frames() {
    let session = fixture_session();
    let binding = fixture_binding(session, 9);
    let point = TerminalPoint {
        line: 80,
        column: 4,
    };
    let cell = ViewportCell { row: 2, column: 4 };
    let mut drag = ViewportSelectionDrag::for_canvas_gesture(
        binding,
        9,
        901,
        point,
        cell,
        TerminalSelectionKind::Character,
        false,
        false,
    );
    let initial = fixture_selection_viewport(3, 30, 5, "prompt$");
    assert!(drag.update(point, cell, 0, &initial).is_some());
    assert!(drag.update(point, cell, 0, &initial).is_none());

    // 输出推进 scroll origin，但绝对 point 和鼠标 cell 均未变化。
    let advanced_origin = fixture_selection_viewport(3, 31, 5, "prompt$");
    assert!(drag.update(point, cell, 0, &advanced_origin).is_some());
    assert!(drag.update(point, cell, 0, &advanced_origin).is_none());

    // 同一坐标槽的新行内容也必须提交，避免 engine 以旧 row witness 拒绝后 UI 永久停住。
    let overwritten_row = fixture_selection_viewport(3, 31, 5, "output$");
    assert!(drag.update(point, cell, 0, &overwritten_row).is_some());
    assert!(drag.update(point, cell, 0, &overwritten_row).is_none());

    // resize/reflow 会推进 grid epoch；即使 endpoint 仍同坐标，也要用新快照重新投影。
    let reflowed = fixture_selection_viewport(3, 31, 6, "output$");
    assert!(drag.update(point, cell, 0, &reflowed).is_some());
    assert!(drag.update(point, cell, 0, &reflowed).is_none());
}

/// 验证滚动 ACK 必须匹配手势的完整 owner/generation，并保留最新指针 cell 供确认后映射。
///
/// @author kongweiguang
#[test]
fn pending_scroll_keeps_latest_pointer_without_rewriting_absolute_anchor() {
    let binding = fixture_binding(fixture_session(), 21);
    let start = TerminalPoint {
        line: 120,
        column: 4,
    };
    let initial_cell = ViewportCell { row: 7, column: 4 };
    let mut drag = ViewportSelectionDrag::for_canvas_gesture(
        binding.clone(),
        21,
        501,
        start,
        initial_cell,
        TerminalSelectionKind::Character,
        false,
        false,
    );
    let requested_at = Instant::now();
    // 同 session/pane 的旧 generation 才是应被拒绝的异步回调；原 fixture 参数完全相同。
    let stale_generation_binding = fixture_binding(binding.session_id.clone(), 20);
    assert_ne!(stale_generation_binding, binding);
    assert!(
        drag.register_pending_scroll(&stale_generation_binding, 501, 18, requested_at)
            .is_none()
    );
    assert!(
        drag.register_pending_scroll(&binding, 999, 18, requested_at)
            .is_none()
    );
    let request_id = drag
        .register_pending_scroll(&binding, 501, 18, requested_at)
        .expect("current gesture registers one scroll request");
    assert_eq!(request_id, 1);
    assert!(
        drag.frame_state(&binding, 21)
            .and_then(|frame| frame.pending_scroll)
            .is_some_and(|pending| !pending.is_acknowledged_by(u64::MAX))
    );
    assert!(
        drag.register_pending_scroll(&binding, 501, 19, requested_at)
            .is_none()
    );
    assert!(!drag.acknowledge_pending_scroll(&binding, 501, 2, 42));
    assert!(!drag.acknowledge_pending_scroll(&binding, 999, request_id, 42));
    // 当前完整 binding 的 request ACK 必须允许；相同 session/pane 的旧 generation 才拒绝。
    assert!(!drag.acknowledge_pending_scroll(&stale_generation_binding, 501, request_id, 42));
    assert!(drag.acknowledge_pending_scroll(&binding, 501, request_id, 42));
    let moved_cell = ViewportCell { row: 0, column: 9 };
    drag.update_pointer(moved_cell, -1);

    let frame = drag
        .frame_state(&binding, 21)
        .expect("same owner and generation retain the gesture");

    assert_eq!(frame.start, start);
    assert_eq!(frame.cell, moved_cell);
    assert_eq!(frame.pending_scroll.unwrap().offset, 18);
    assert_eq!(frame.pending_scroll.unwrap().requested_at, requested_at);
    let pending = frame.pending_scroll.unwrap();
    assert_eq!(pending.request_id, request_id);
    assert!(!pending.is_acknowledged_by(41));
    assert!(pending.is_acknowledged_by(42));
    // 之后输出可改变当前 offset；只有匹配请求的 ACK revision 决定何时映射当前快照。
    assert!(pending.is_acknowledged_by(43));

    drag.clear_pending_scroll();
    assert_eq!(
        drag.register_pending_scroll(&binding, 501, 19, requested_at),
        Some(2),
        "request ids increase within one gesture after the prior ACK is consumed"
    );

    drag.defer_release_until_scroll_ack(moved_cell, true, false, 1);
    let released_frame = drag
        .frame_state(&binding, 21)
        .expect("release remains attached to the same gesture");
    let release = released_frame.pending_release.unwrap();
    assert_eq!(release.cell, moved_cell);
    assert!(release.plain_unmodified_release);
    assert!(!release.modifier_is_held);
    assert_eq!(release.click_count, 1);
    assert_eq!(released_frame.edge_direction, 0);
}
