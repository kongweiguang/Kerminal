//! 回归测试覆盖跨块解析、bounded 文本窗口、damage 合并与 OSC palette 回复。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits, TerminalResizePolicy};
use kerminal_terminal::snapshot::{
    PlainTextSnapshotRequest, RenderSnapshot, RenderSnapshotRequest, SelectionViewport,
    TerminalColor, TerminalPalette, TerminalPoint, TerminalRgb, TerminalRowWitness,
    TerminalSearchRequest, TerminalSelection, TerminalSelectionKind,
};

/// 创建小网格以让 scrollback、换行和字节预算行为能在固定断言中观察。
///
/// @author kongweiguang
fn engine(columns: u16, rows: u16) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive terminal grid");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default()).expect("bounded engine")
}

/// 通过显式 policy 构造器覆盖 transport 差异，同时保留默认 helper 的兼容性检查。
///
/// @author kongweiguang
fn engine_with_resize_policy(
    columns: u16,
    rows: u16,
    policy: TerminalResizePolicy,
) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive terminal grid");
    TerminalEngine::new_with_resize_policy(
        SessionId::new(),
        size,
        TerminalLimits::default(),
        policy,
    )
    .expect("bounded engine with transport resize policy")
}

/// Capture the exact source row alongside a point so async selection can reject content that moved into its old screen slot.
///
/// @author kongweiguang
fn selection_viewport(snapshot: &RenderSnapshot, point: TerminalPoint) -> SelectionViewport {
    let viewport_row = i128::from(point.line) + snapshot.scrollback_offset as i128;
    let endpoint_row_witness = usize::try_from(viewport_row)
        .ok()
        .and_then(|row| snapshot.rows.get(row))
        .map(|row| row.witness());
    SelectionViewport {
        size: snapshot.size,
        scrollback_offset: snapshot.scrollback_offset,
        scroll_origin: snapshot.scroll_origin,
        grid_epoch: snapshot.grid_epoch,
        endpoint_row_witness,
    }
}

/// 诊断 cursor getter 与固定 ASCII 终端位置一致，且读取不得推进 VT revision。
///
/// @author kongweiguang
#[test]
fn diagnostic_cursor_position_is_read_only_and_matches_live_cursor() {
    let mut terminal = engine(8, 2);
    terminal.process_output(b"abc");
    let revision = terminal.revision();

    assert_eq!(terminal.diagnostic_cursor_position(), (0, 3));
    assert_eq!(terminal.revision(), revision);
}

/// 即使用户停留在 scrollback，取诊断坐标也不能把 viewport 拉回活动屏幕。
///
/// @author kongweiguang
#[test]
fn diagnostic_cursor_position_preserves_scrollback_view() {
    let mut terminal = engine(8, 2);
    terminal.process_output(b"one\r\ntwo\r\nthree");
    let visible = PlainTextSnapshotRequest {
        include_scrollback: false,
        max_lines: 2,
        max_bytes: Some(128),
    };
    let live_text = terminal.plaintext_snapshot(visible).text;
    terminal.scroll(1);
    let scrolled_text = terminal.plaintext_snapshot(visible).text;
    assert_ne!(
        scrolled_text, live_text,
        "fixture must create a scrollback viewport"
    );

    let revision = terminal.revision();
    let _ = terminal.diagnostic_cursor_position();

    assert_eq!(terminal.revision(), revision);
    assert_eq!(
        terminal.plaintext_snapshot(visible).text,
        scrolled_text,
        "diagnostic sampling must not alter the selected viewport"
    );
}

/// 验证 parser 跨 read chunk 保留 UTF-8 与 CSI parser 状态，避免 PTY read 边界破坏文本。
///
/// @author kongweiguang
#[test]
fn output_chunks_preserve_utf8_and_ansi_state() {
    let mut engine = engine(20, 2);
    engine.process_output(b"caf\xc3");
    engine.process_output(b"\xa9\x1b[31mred\x1b[0m");

    let snapshot = engine.plaintext_snapshot(PlainTextSnapshotRequest {
        include_scrollback: false,
        max_lines: 2,
        max_bytes: Some(128),
    });

    assert!(
        snapshot.text.contains("caféred"),
        "actual text: {:?}",
        snapshot.text
    );
}

/// 纯 OSC7 没有可见字形也必须推进 revision；目录只由原 PTY 的合法协议通知替换。
///
/// @author kongweiguang
#[test]
fn directory_notifications_update_snapshot_metadata_without_visible_output() {
    let mut engine = engine(20, 2);
    let before = engine.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(before.current_working_directory, None);
    engine.process_output(b"\x1b]7;file:///srv/%E4%B8%AD%20%23%25\x07");
    let after = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(after.revision > before.revision);
    assert_eq!(
        after.current_working_directory.as_deref(),
        Some("/srv/中 #%")
    );
    assert_eq!(after.session_id, before.session_id);
    assert!(
        engine
            .plaintext_snapshot(PlainTextSnapshotRequest {
                include_scrollback: false,
                max_lines: 2,
                max_bytes: Some(128),
            })
            .text
            .trim()
            .is_empty()
    );

    engine.process_output(b"\x1b]7;file:///bad%00path\x07");
    assert_eq!(
        engine
            .render_snapshot(RenderSnapshotRequest::default())
            .current_working_directory
            .as_deref(),
        Some("/srv/中 #%"),
    );
}

/// 重连必须同时丢弃旧目录和未完成的 OSC，避免旧 PTY 的后半段被新实例继承。
///
/// @author kongweiguang
#[test]
fn reconnect_clears_directory_and_partial_notifications() {
    let mut terminal = engine(20, 2);
    terminal.process_output(b"\x1b]7;file:///old\x07\x1b]7;file:///partial");
    let old_revision = terminal.revision();
    terminal.reset_for_reconnect().expect("reset engine");
    terminal.process_output(b"\x07");
    let snapshot = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(snapshot.revision > old_revision);
    assert_eq!(snapshot.current_working_directory, None);

    let mut another = engine(20, 2);
    another.process_output(b"\x1b]7;file:///other\x07");
    assert_eq!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .current_working_directory,
        None
    );
    assert_eq!(
        another
            .render_snapshot(RenderSnapshotRequest::default())
            .current_working_directory
            .as_deref(),
        Some("/other")
    );
}

/// 确认字节限额从历史尾部保留完整 UTF-8 字符，且 first_line 指向实际返回的末行。
///
/// @author kongweiguang
#[test]
fn plaintext_budget_keeps_tail_and_reports_actual_line() {
    let mut engine = engine(12, 2);
    engine.process_output("one\r\ntwo\r\nthree\r\n四尾".as_bytes());

    let snapshot = engine.plaintext_snapshot(PlainTextSnapshotRequest {
        include_scrollback: true,
        max_lines: 20,
        max_bytes: Some(4),
    });

    assert_eq!(snapshot.text, "尾");
    assert_eq!(snapshot.line_count, 1);
    assert!(snapshot.truncated);
    assert!(snapshot.first_line >= 0);
}

/// 验证 watch 合并两次读通知后 dirty rows 是两个输出批次的并集。
///
/// @author kongweiguang
#[test]
fn dirty_rows_union_multiple_output_chunks() {
    let mut engine = engine(12, 3);
    let baseline = engine
        .render_snapshot(RenderSnapshotRequest::default())
        .revision;
    engine.process_output(b"first");
    engine.process_output(b"\x1b[2;1Hsecond");

    let snapshot = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(baseline),
    });

    assert!(
        snapshot.dirty_rows.contains(&0),
        "dirty rows: {:?}",
        snapshot.dirty_rows
    );
    assert!(
        snapshot.dirty_rows.contains(&1),
        "dirty rows: {:?}",
        snapshot.dirty_rows
    );
}

/// 将 DEC private mode 1007 暴露给输入层，且 reset 后立即返回关闭状态。
///
/// @author kongweiguang
#[test]
fn alternate_scroll_mode_tracks_dec_private_mode_1007() {
    let mut terminal = engine(12, 2);
    terminal.process_output(b"\x1b[?1007h");
    assert!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .modes
            .alternate_scroll
    );

    terminal.process_output(b"\x1b[?1007l");
    assert!(
        !terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .modes
            .alternate_scroll
    );
}

/// 通用扩行会把历史末尾拉入视口并移动光标，但仍须保留完整文本，避免误改 SSH/Unix 行为。
///
/// @author kongweiguang
#[test]
fn generic_row_growth_keeps_alacritty_history_pull_behavior() {
    let mut terminal = engine(52, 15);
    let output = (1..=160)
        .map(|line| format!("R40-LINE-{line:03}\r\n"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let request = PlainTextSnapshotRequest {
        include_scrollback: true,
        max_lines: 512,
        max_bytes: Some(64 * 1024),
    };
    let before = terminal.plaintext_snapshot(request);
    assert_eq!(before.text.matches("R40-LINE-").count(), 160);
    let history_before = before.total_scrollback_lines;
    let cursor_before = terminal.diagnostic_cursor_position();

    terminal
        .resize(TerminalSize::new(52, 17, 0, 0).expect("valid generic row growth"))
        .expect("resize generic terminal");
    let after = terminal.plaintext_snapshot(request);

    assert_eq!(after.total_scrollback_lines, history_before - 2);
    assert_eq!(after.text, before.text);
    assert_eq!(
        terminal.diagnostic_cursor_position(),
        (cursor_before.0 + 2, cursor_before.1)
    );
}

/// Windows ConPTY 扩行增加空白屏幕行但保留全部历史、旧 cursor 坐标与完整输出。
///
/// @author kongweiguang
#[test]
fn conpty_row_growth_preserves_history_and_cursor() {
    let mut terminal = engine_with_resize_policy(52, 15, TerminalResizePolicy::ConPty);
    let output = (1..=160)
        .map(|line| format!("R40-LINE-{line:03}\r\n"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let request = PlainTextSnapshotRequest {
        include_scrollback: true,
        max_lines: 512,
        max_bytes: Some(64 * 1024),
    };
    let before = terminal.plaintext_snapshot(request);
    let cursor_before = terminal.diagnostic_cursor_position();
    assert_eq!(before.text.matches("R40-LINE-").count(), 160);

    terminal
        .resize(TerminalSize::new(52, 17, 0, 0).expect("valid ConPTY row growth"))
        .expect("resize ConPTY terminal");
    let after = terminal.plaintext_snapshot(request);

    assert_eq!(after.total_scrollback_lines, before.total_scrollback_lines);
    assert_eq!(after.text.matches("R40-LINE-").count(), 160);
    assert!(!after.truncated);
    assert_eq!(terminal.diagnostic_cursor_position(), cursor_before);

    terminal
        .reset_for_reconnect()
        .expect("reset keeps transport resize policy");
    terminal.process_output(output.as_bytes());
    let before_reconnect_resize = terminal.plaintext_snapshot(request);
    terminal
        .resize(TerminalSize::new(52, 19, 0, 0).expect("valid post-reconnect growth"))
        .expect("resize after reconnect");
    let after_reconnect_resize = terminal.plaintext_snapshot(request);
    assert_eq!(
        after_reconnect_resize.total_scrollback_lines,
        before_reconnect_resize.total_scrollback_lines
    );
    assert_eq!(
        after_reconnect_resize.text.matches("R40-LINE-").count(),
        160
    );
}

/// 阅读历史时扩行不应移动既有首行，避免真实重画修复破坏非底部阅读锚点。
///
/// @author kongweiguang
#[test]
fn conpty_row_growth_preserves_scrolled_reading_anchor() {
    let mut terminal = engine_with_resize_policy(52, 15, TerminalResizePolicy::ConPty);
    let output = (1..=160)
        .map(|line| format!("R40-ANCHOR-{line:03}\r\n"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    terminal.scroll(25);
    let before = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(before.scrollback_offset, 25);

    terminal
        .resize(TerminalSize::new(52, 17, 0, 0).expect("row-only growth while reading"))
        .expect("resize scrolled ConPTY grid");
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(after.scrollback_offset, before.scrollback_offset);
    assert_eq!(after.rows[0].absolute_line, before.rows[0].absolute_line);
    assert_eq!(after.rows[0].runs, before.rows[0].runs);
}

/// ConPTY 行扩展与列缩窄同时发生时，软折行文本仍按 Alacritty 的 reflow 规则可搜索。
///
/// @author kongweiguang
#[test]
fn conpty_row_growth_keeps_column_reflow_searchable() {
    let mut terminal = engine_with_resize_policy(6, 2, TerminalResizePolicy::ConPty);
    terminal.process_output(b"abcdefgh");
    terminal
        .resize(TerminalSize::new(4, 4, 0, 0).expect("valid reflow and row-growth size"))
        .expect("reflow under ConPTY policy");

    let result = terminal
        .search(&TerminalSearchRequest {
            query: "efgh".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search reflowed ConPTY grid");
    assert_eq!(result.matches.len(), 1);
    assert_eq!(result.matches[0].range.start.column, 0);
    assert_eq!(result.matches[0].range.end.column, 4);
}

/// 备用屏活动时同步扩展两张 grid，退出后 primary 历史仍可读。
///
/// @author kongweiguang
#[test]
fn conpty_row_growth_preserves_inactive_primary() {
    let mut terminal = engine_with_resize_policy(16, 2, TerminalResizePolicy::ConPty);
    terminal.process_output(b"primary-01\r\nprimary-02\r\nprimary-03\r\nprimary-04");

    terminal.process_output(b"\x1b[?1049halt-screen");
    terminal
        .resize(TerminalSize::new(16, 4, 0, 0).expect("valid alternate-screen growth"))
        .expect("resize while alternate screen is active");
    assert!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .modes
            .alternate_screen
    );
    terminal.process_output(b"\x1b[?1049l");

    let snapshot = terminal.plaintext_snapshot(PlainTextSnapshotRequest {
        include_scrollback: true,
        max_lines: 32,
        max_bytes: Some(4096),
    });
    assert!(snapshot.text.contains("primary-01"));
    assert!(snapshot.text.contains("primary-04"));
}

/// ConPTY 行增长只增加空行，不重设 Alacritty 的有效选择或改变复制文本。
///
/// @author kongweiguang
#[test]
fn conpty_row_growth_preserves_selection_text() {
    let mut terminal = engine_with_resize_policy(16, 2, TerminalResizePolicy::ConPty);
    terminal.process_output(b"alpha beta");
    terminal.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 7 },
        end: TerminalPoint { line: 0, column: 7 },
        kind: TerminalSelectionKind::Word,
    }));
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));

    terminal
        .resize(TerminalSize::new(16, 4, 0, 0).expect("valid selection-preserving growth"))
        .expect("resize selected ConPTY grid");

    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));
    assert_eq!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .selection
            .map(|selection| selection.kind),
        Some(TerminalSelectionKind::Word)
    );
}

/// Word selection 使用 Alacritty Semantic 规则，resize 清除选择时快照与复制结果同步变空。
///
/// @author kongweiguang
#[test]
fn word_selection_snapshot_tracks_alacritty_effective_selection_after_resize() {
    let mut terminal = engine(16, 2);
    terminal.process_output(b"alpha beta gamma");
    terminal.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 7 },
        end: TerminalPoint { line: 0, column: 7 },
        kind: TerminalSelectionKind::Word,
    }));
    let selected = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(
        selected.selection.map(|selection| selection.kind),
        Some(TerminalSelectionKind::Word)
    );
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));

    let resized = TerminalSize::new(8, 2, 0, 0).expect("valid resized grid");
    terminal.resize(resized).expect("resize terminal grid");
    let after_resize = terminal.render_snapshot(RenderSnapshotRequest::default());

    assert_eq!(after_resize.selection, None);
    assert_eq!(terminal.selected_text(), None);
}

/// scrollback 被限额裁掉时，快照端点取 Alacritty clamp 后的有效范围而非旧 UI 坐标。
///
/// @author kongweiguang
#[test]
fn selection_snapshot_projects_effective_range_after_scrollback_trim() {
    let mut terminal = engine(12, 2);
    let output = (0..8)
        .map(|line| format!("trim-{line:02}\r\n"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    terminal.select_all();
    let before = terminal.render_snapshot(RenderSnapshotRequest::default());
    let original_selection = before.selection.expect("all visible history is selected");

    assert_eq!(terminal.set_scrollback_limit(1), 1);
    let after = terminal.render_snapshot(RenderSnapshotRequest::default());

    let effective_selection = after
        .selection
        .expect("remaining visible grid still has an effective selection");
    assert_eq!(after.history_size, 1);
    assert_ne!(effective_selection.start, original_selection.start);
    assert!(terminal.selected_text().is_some());
}

/// 持续输出时同一拖选保留 Alacritty 原 anchor，且历史裁剪后旧 token 不能重新创建选择。
///
/// @author kongweiguang
#[test]
fn selection_gesture_preserves_anchor_and_rejects_trimmed_revival() {
    let mut terminal = engine(12, 2);
    terminal.set_scrollback_limit(4);
    terminal.process_output(b"anchor");
    let gesture_id = 41;
    let original = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 5 },
        kind: TerminalSelectionKind::Character,
    };

    assert!(terminal.update_selection_gesture(gesture_id, original));
    assert_eq!(terminal.selected_text().as_deref(), Some("anchor"));
    let anchor_revision = terminal.revision();

    terminal.process_output(b"\r\nline-1\r\nline-2");
    assert!(terminal.update_selection_gesture_at_revision(
        gesture_id,
        TerminalSelection {
            start: original.start,
            end: TerminalPoint { line: 0, column: 5 },
            kind: original.kind,
        },
        Some(anchor_revision),
    ));
    assert!(
        terminal
            .selected_text()
            .as_deref()
            .is_some_and(|text| text.contains("anchor")),
        "same gesture must retain its original Alacritty anchor while output advances"
    );

    let trim_output = (0..16)
        .map(|line| format!("\r\ntrim-{line:02}"))
        .collect::<String>();
    terminal.process_output(trim_output.as_bytes());
    assert_eq!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .selection,
        None,
        "trimmed anchor must no longer be an effective selection"
    );
    assert!(
        !terminal.update_selection_gesture(
            gesture_id,
            TerminalSelection {
                start: TerminalPoint { line: 0, column: 0 },
                end: TerminalPoint { line: 0, column: 5 },
                kind: TerminalSelectionKind::Character,
            },
        ),
        "the same gesture token must not resurrect its trimmed selection"
    );
    assert_eq!(terminal.selected_text(), None);
    assert_eq!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .selection,
        None
    );
}

/// 绘制后收到新的 PTY 行时，按累计滚屏量与行证据选择旧快照中的原始内容。
///
/// @author kongweiguang
#[test]
fn viewport_selection_start_rebases_after_continuous_output() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"old-top\r\nold-bottom");
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let old_row = painted.rows[0].absolute_line;
    assert_eq!(painted.scrollback_offset, 0);

    terminal.process_output(b"\r\nnew-bottom");
    let selection = TerminalSelection {
        start: TerminalPoint {
            line: old_row,
            column: 0,
        },
        end: TerminalPoint {
            line: old_row,
            column: 9,
        },
        kind: TerminalSelectionKind::Character,
    };

    let viewport = selection_viewport(&painted, selection.end);
    assert!(terminal.update_selection_gesture_in_viewport(601, selection, viewport));
    assert_eq!(terminal.selected_text().as_deref(), Some("old-top"));
}

/// 用户停留在 scrollback 时，Alacritty 自动增加 display offset；旧快照行仍映射回阅读锚点。
///
/// @author kongweiguang
#[test]
fn viewport_selection_start_tracks_read_anchor_during_output() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"one\r\ntwo\r\nthree\r\nfour\r\nfive");
    terminal.scroll(2);
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let old_row = painted.rows[0].absolute_line;
    assert_eq!(painted.scrollback_offset, 2);

    terminal.process_output(b"\r\nsix");
    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(current.scrollback_offset, painted.scrollback_offset + 1);

    let selection = TerminalSelection {
        start: TerminalPoint {
            line: old_row,
            column: 0,
        },
        end: TerminalPoint {
            line: old_row,
            column: 2,
        },
        kind: TerminalSelectionKind::Character,
    };
    assert!(terminal.update_selection_gesture_in_viewport(
        602,
        selection,
        selection_viewport(&painted, selection.end),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("two"));
}

/// 网格尺寸变化后拒绝旧快照手势，并 tombstone token 保留此前已确认的复制内容。
///
/// @author kongweiguang
#[test]
fn viewport_selection_rejects_resized_snapshot_and_keeps_confirmed_selection() {
    let mut terminal = engine(16, 2);
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let stale = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 4 },
        kind: TerminalSelectionKind::Character,
    };
    let old_viewport = selection_viewport(&painted, stale.end);

    terminal
        .resize(TerminalSize::new(16, 3, 0, 0).expect("valid resized grid"))
        .expect("resize terminal grid");
    terminal.process_output(b"\x1b[1;1Hconfirmed\x1b[K");
    terminal.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 8 },
        kind: TerminalSelectionKind::Character,
    }));
    assert_eq!(terminal.selected_text().as_deref(), Some("confirmed"));

    assert!(!terminal.update_selection_gesture_in_viewport(603, stale, old_viewport));
    assert_eq!(terminal.selected_text().as_deref(), Some("confirmed"));

    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(!terminal.update_selection_gesture_in_viewport(
        603,
        stale,
        selection_viewport(&current, stale.end),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("confirmed"));
}

/// 手势已建立后，输出移动 Alacritty anchor；后续 viewport Move 只重基准 end 并保持原选区起点。
///
/// @author kongweiguang
#[test]
fn viewport_selection_move_after_output_keeps_original_anchor() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"anchor\r\nbottom");
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let anchor = TerminalPoint {
        line: painted.rows[0].absolute_line,
        column: 0,
    };
    let start = TerminalSelection {
        start: anchor,
        end: TerminalPoint {
            line: anchor.line,
            column: 5,
        },
        kind: TerminalSelectionKind::Character,
    };
    assert!(terminal.update_selection_gesture_in_viewport(
        604,
        start,
        selection_viewport(&painted, start.end),
    ));

    terminal.process_output(b"\r\nnew-one\r\nnew-two");
    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(terminal.update_selection_gesture_in_viewport(
        604,
        TerminalSelection {
            start: anchor,
            end: TerminalPoint {
                line: current.rows[1].absolute_line,
                column: 6,
            },
            kind: TerminalSelectionKind::Character,
        },
        selection_viewport(
            &current,
            TerminalPoint {
                line: current.rows[1].absolute_line,
                column: 6,
            },
        ),
    ));
    let selected = terminal
        .selected_text()
        .expect("active range remains selected");
    assert!(selected.contains("anchor"));
    assert!(selected.contains("new-two"));
}

/// 多次全屏滚动后按旧快照行选择仍绑定原始字符，而不是碰巧进入旧屏幕槽位的新行。
///
/// @author kongweiguang
#[test]
fn viewport_selection_maps_original_row_after_multiple_full_screen_scrolls() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(32);
    terminal.process_output(b"original\r\nbottom");
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let original = TerminalSelection {
        start: TerminalPoint {
            line: painted.rows[0].absolute_line,
            column: 0,
        },
        end: TerminalPoint {
            line: painted.rows[0].absolute_line,
            column: 7,
        },
        kind: TerminalSelectionKind::Character,
    };

    let output = (0..7)
        .map(|line| format!("\r\nnew-{line:02}"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(current.scroll_origin, painted.scroll_origin + 7);

    assert!(terminal.update_selection_gesture_in_viewport(
        605,
        original,
        selection_viewport(&painted, original.end),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("original"));
}

/// 历史容量满后旧行已裁剪时拒绝建立新 anchor，并保留当前已确认的复制范围。
///
/// @author kongweiguang
#[test]
fn viewport_selection_rejects_trimmed_row_without_clamping_to_new_content() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(2);
    terminal.process_output(b"expired\r\nbottom");
    let painted = terminal.render_snapshot(RenderSnapshotRequest::default());
    let stale = TerminalSelection {
        start: TerminalPoint {
            line: painted.rows[0].absolute_line,
            column: 0,
        },
        end: TerminalPoint {
            line: painted.rows[0].absolute_line,
            column: 6,
        },
        kind: TerminalSelectionKind::Character,
    };
    let old_viewport = selection_viewport(&painted, stale.end);

    let output = (0..7)
        .map(|line| format!("\r\nlatest-{line:02}"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    let confirmed = TerminalSelection {
        start: TerminalPoint {
            line: current.rows[1].absolute_line,
            column: 0,
        },
        end: TerminalPoint {
            line: current.rows[1].absolute_line,
            column: 8,
        },
        kind: TerminalSelectionKind::Character,
    };
    terminal.set_selection(Some(confirmed));
    let confirmed_text = terminal.selected_text();

    assert!(!terminal.update_selection_gesture_in_viewport(606, stale, old_viewport));
    assert_eq!(terminal.selected_text(), confirmed_text);
}

/// 行证据忽略颜色分段，只保留文字、列跨度和软换行这些影响终端坐标的事实。
///
/// @author kongweiguang
#[test]
fn row_witness_ignores_visual_style_changes() {
    let mut terminal = engine(8, 1);
    terminal.process_output(b"\x1b[31mred");
    let snapshot = terminal.render_snapshot(RenderSnapshotRequest::default());
    let row = &snapshot.rows[0];
    let original = row.witness();
    let mut recolored = row.runs.to_vec();
    recolored[0].style.foreground = TerminalColor::Ansi(2);

    assert_eq!(
        *original,
        TerminalRowWitness::from_runs(row.wrapped, &recolored),
    );
}

/// 旧快照的 MouseDown anchor 被拒绝且同 token Move 不得复活，刷新后新手势仍能选择当前行。
///
/// @author kongweiguang
#[test]
fn stale_selection_gesture_start_preserves_confirmed_selection() {
    let mut terminal = engine(16, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"confirmed");
    let confirmed = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 9 },
        kind: TerminalSelectionKind::Character,
    };
    terminal.set_selection(Some(confirmed));
    let stale_revision = terminal
        .render_snapshot(RenderSnapshotRequest::default())
        .revision;

    terminal.process_output(b"\r\nother\r\nmore");
    let before_rejected_start = terminal.revision();
    let stale_press = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 0 },
        kind: TerminalSelectionKind::Character,
    };
    assert!(
        !terminal.update_selection_gesture_at_revision(301, stale_press, Some(stale_revision),)
    );
    assert_eq!(terminal.revision(), before_rejected_start);
    assert_eq!(terminal.selected_text().as_deref(), Some("confirmed"));

    assert!(
        !terminal.update_selection_gesture_at_revision(301, stale_press, None),
        "a delayed move from a rejected start must remain tombstoned"
    );
    assert_eq!(terminal.selected_text().as_deref(), Some("confirmed"));

    let fresh_revision = terminal
        .render_snapshot(RenderSnapshotRequest::default())
        .revision;
    assert!(terminal.update_selection_gesture_at_revision(
        302,
        TerminalSelection {
            start: TerminalPoint { line: 1, column: 0 },
            end: TerminalPoint { line: 1, column: 4 },
            kind: TerminalSelectionKind::Character,
        },
        Some(fresh_revision),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("more"));
}

/// 快速连续语义点击期间仅选区 revision 前进时，旧快照坐标仍属于同一网格并可接受。
///
/// @author kongweiguang
#[test]
fn semantic_gesture_accepts_snapshot_after_selection_only_change() {
    let mut terminal = engine(16, 2);
    terminal.process_output(b"alpha beta");
    let mouse_down_revision = terminal
        .render_snapshot(RenderSnapshotRequest::default())
        .revision;
    terminal.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 5 },
        kind: TerminalSelectionKind::Character,
    }));
    assert!(terminal.revision() > mouse_down_revision);

    let word_click = TerminalSelection {
        start: TerminalPoint { line: 0, column: 7 },
        end: TerminalPoint { line: 0, column: 7 },
        kind: TerminalSelectionKind::Word,
    };
    assert!(terminal.update_selection_gesture_at_revision(
        401,
        word_click,
        Some(mouse_down_revision),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));
}

/// viewport scroll 只改变可见行映射，不使仍在 grid 中的 absolute TerminalPoint 坐标过期。
///
/// @author kongweiguang
#[test]
fn viewport_scroll_does_not_invalidate_grid_coordinate_revision() {
    let mut terminal = engine(12, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"one\r\ntwo\r\nthree\r\nfour");
    let before_scroll = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(before_scroll.history_size > 0);
    assert_eq!(before_scroll.scrollback_offset, 0);

    terminal.scroll(1);
    let after_scroll = terminal.render_snapshot(RenderSnapshotRequest::default());
    assert!(after_scroll.scrollback_offset > before_scroll.scrollback_offset);
    assert!(terminal.revision() > before_scroll.revision);

    assert!(terminal.update_selection_gesture_at_revision(
        501,
        TerminalSelection {
            start: TerminalPoint { line: 0, column: 0 },
            end: TerminalPoint { line: 0, column: 1 },
            kind: TerminalSelectionKind::Character,
        },
        Some(before_scroll.revision),
    ));
}

/// resize 结束活动拖选 token 但保留已确认选择；迟到 Move 不得覆盖复制内容，新 token 仍可开始。
///
/// @author kongweiguang
#[test]
fn resize_ends_gesture_token_without_losing_confirmed_selection() {
    let mut terminal = engine(16, 2);
    terminal.process_output(b"alpha beta");
    let alpha = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 4 },
        kind: TerminalSelectionKind::Character,
    };
    let beta = TerminalSelection {
        start: TerminalPoint { line: 0, column: 6 },
        end: TerminalPoint { line: 0, column: 9 },
        kind: TerminalSelectionKind::Character,
    };
    let old_gesture = 100;

    assert!(terminal.update_selection_gesture(old_gesture, alpha));
    assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
    let pre_resize_grid_revision = terminal.revision();

    terminal
        .resize(TerminalSize::new(16, 2, 0, 0).expect("valid grid"))
        .expect("resize keeps bounded grid");

    assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));
    assert!(
        !terminal.update_selection_gesture(old_gesture, beta),
        "a delayed move from the gesture ended by resize must be rejected"
    );
    assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));

    assert!(
        !terminal.update_selection_gesture_at_revision(
            old_gesture + 1,
            beta,
            Some(pre_resize_grid_revision),
        ),
        "a new gesture may not reuse row coordinates from before resize"
    );
    assert_eq!(terminal.selected_text().as_deref(), Some("alpha"));

    let resized_revision = terminal.revision();
    assert!(terminal.update_selection_gesture_at_revision(
        old_gesture + 2,
        beta,
        Some(resized_revision),
    ));
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));
}

/// 新拖选建立后，乱序到达的旧 token 不得重新锚定并覆盖较新的选择。
///
/// @author kongweiguang
#[test]
fn delayed_older_gesture_cannot_overwrite_newer_gesture() {
    let mut terminal = engine(16, 2);
    terminal.process_output(b"alpha beta");
    let alpha = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 5 },
        kind: TerminalSelectionKind::Character,
    };
    let beta = TerminalSelection {
        start: TerminalPoint { line: 0, column: 6 },
        end: TerminalPoint {
            line: 0,
            column: 10,
        },
        kind: TerminalSelectionKind::Character,
    };

    assert!(terminal.update_selection_gesture(200, alpha));
    assert!(terminal.update_selection_gesture(201, beta));
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));

    assert!(
        !terminal.update_selection_gesture(200, alpha),
        "an older gesture cannot replace the active selection after a newer id began"
    );
    assert_eq!(terminal.selected_text().as_deref(), Some("beta"));
}

/// MouseDown 的空 Character anchor 必须随持续输出进入历史，首个 Move 再选到原始按下文本。
///
/// @author kongweiguang
#[test]
fn empty_selection_gesture_anchor_survives_output_before_first_move() {
    let mut terminal = engine(12, 2);
    terminal.set_scrollback_limit(8);
    terminal.process_output(b"anchor");
    let gesture_id = 73;
    let press = TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 0 },
        kind: TerminalSelectionKind::Character,
    };

    assert!(terminal.update_selection_gesture(gesture_id, press));
    assert_eq!(
        terminal.selected_text(),
        None,
        "press must keep an empty anchor"
    );

    let output = (0..6)
        .map(|line| format!("\r\nrow-{line:02}"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    assert!(terminal.update_selection_gesture(
        gesture_id,
        TerminalSelection {
            start: press.start,
            end: TerminalPoint { line: 0, column: 5 },
            kind: TerminalSelectionKind::Character,
        },
    ));

    assert!(
        terminal
            .selected_text()
            .as_deref()
            .is_some_and(|text| text.contains("anchor")),
        "first move must extend the Alacritty anchor created at MouseDown"
    );
}

/// 空 press 锚点在滚屏越过 history 上限后必须结束手势，不能被 Alacritty clamp 到新内容。
///
/// MouseDown 已按既定交互替换旧选择；拒绝首个 Move 时应保持空选择，并留下 token tombstone。
///
/// @author kongweiguang
#[test]
fn trimmed_empty_selection_anchor_rejects_first_move_without_selecting_new_history() {
    let mut terminal = engine(12, 2);
    terminal.set_scrollback_limit(1);
    terminal.process_output(b"expired\r\nbottom");
    let pressed_snapshot = terminal.render_snapshot(RenderSnapshotRequest::default());
    let press = TerminalSelection {
        start: TerminalPoint {
            line: pressed_snapshot.rows[0].absolute_line,
            column: 0,
        },
        end: TerminalPoint {
            line: pressed_snapshot.rows[0].absolute_line,
            column: 0,
        },
        kind: TerminalSelectionKind::Character,
    };

    assert!(terminal.update_selection_gesture_in_viewport(
        707,
        press,
        selection_viewport(&pressed_snapshot, press.end),
    ));
    assert_eq!(terminal.selected_text(), None);

    let output = (0..8)
        .map(|line| format!("\r\nnew-{line:02}"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let current = terminal.render_snapshot(RenderSnapshotRequest::default());
    let mapped_anchor_line = i128::from(press.start.line)
        + i128::from(pressed_snapshot.scroll_origin)
        - i128::from(current.scroll_origin);
    assert!(mapped_anchor_line < -i128::try_from(current.history_size).expect("bounded history"));
    let end = TerminalPoint {
        line: current.rows[1].absolute_line,
        column: 4,
    };

    assert!(
        !terminal.update_selection_gesture_in_viewport(
            707,
            TerminalSelection { end, ..press },
            selection_viewport(&current, end),
        ),
        "the first Move must be rejected after its empty anchor was trimmed"
    );
    assert_eq!(terminal.selected_text(), None);
    assert_eq!(
        terminal
            .render_snapshot(RenderSnapshotRequest::default())
            .selection,
        None,
        "an expired press must not clamp into the new oldest history row"
    );
    assert!(
        !terminal.update_selection_gesture(707, TerminalSelection { end, ..press }),
        "the ended token must not revive through the compatibility entry point"
    );
}

/// 搜索开始后的取消在有限扫描检查点终止查询，未完成命中不会被包装成成功结果。
///
/// @author kongweiguang
#[test]
fn search_cancellation_aborts_in_progress_scan_without_partial_matches() {
    let mut terminal = engine(12, 2);
    let output = (0..64)
        .map(|line| format!("search-row-{line:02}\r\n"))
        .collect::<String>();
    terminal.process_output(output.as_bytes());
    let request = TerminalSearchRequest {
        query: "search-row".to_owned(),
        case_sensitive: true,
        regex: false,
        backwards: false,
        max_matches: 128,
    };
    let mut checkpoints = 0;

    let result = terminal.search_cancellable(&request, || {
        checkpoints += 1;
        checkpoints >= 5
    });

    assert!(matches!(
        result,
        Err(kerminal_terminal::engine::TerminalEngineError::SearchCancelled)
    ));
    assert!(checkpoints >= 5);
}

/// 以相同纯 palette 验证 Named/默认颜色解析和 OSC 10 查询回复不依赖 UI 的另一套 ANSI 表。
///
/// @author kongweiguang
#[test]
fn palette_resolution_and_osc_color_query_share_defaults() {
    let palette = TerminalPalette::default();
    assert_eq!(
        palette.resolve(TerminalColor::DefaultForeground),
        TerminalRgb::new(0xd0, 0xd0, 0xd0)
    );
    assert_eq!(
        palette.named_index(0),
        Some(palette.resolve(TerminalColor::Ansi(0)))
    );
    for index in [16_u8, 231, 232, 255] {
        assert_eq!(
            palette.named_index(usize::from(index)),
            Some(palette.resolve(TerminalColor::Indexed(index)))
        );
    }

    let mut engine = engine(12, 2);
    let response = engine.process_output(b"\x1b]10;?\x1b\\");
    let response_text =
        String::from_utf8(response.pty_writes.concat()).expect("OSC reply is UTF-8/ASCII");
    assert!(
        response_text.contains("rgb:"),
        "OSC response: {response_text:?}"
    );
    assert!(
        response_text.contains("d0d0"),
        "OSC response: {response_text:?}"
    );
}

/// 验证 OSC 4 请求编号和 xterm 256 palette 下标一致，而非错误地从 16 开始二次偏移。
///
/// @author kongweiguang
#[test]
fn osc4_indices_match_snapshot_indexed_colors() {
    let palette = TerminalPalette::default();
    let mut engine = engine(12, 2);

    for index in [16_u8, 231, 232, 255] {
        let query = format!("\x1b]4;{index};?\x1b\\");
        let reply = engine.process_output(query.as_bytes()).pty_writes.concat();
        let reply = String::from_utf8(reply).expect("OSC 4 reply is ASCII");
        let expected = palette.resolve(TerminalColor::Indexed(index));
        let red = format!("{:02x}{:02x}", expected.red, expected.red);
        let green = format!("{:02x}{:02x}", expected.green, expected.green);
        let blue = format!("{:02x}{:02x}", expected.blue, expected.blue);
        let expected_rgb = format!("rgb:{red}/{green}/{blue}");
        assert!(
            reply.contains(&expected_rgb),
            "OSC {index} reply {reply:?}, expected {expected_rgb}"
        );
    }
}

/// 验证 OSC 动态改色同时改变快照解析与查询回复，OSC reset 恢复用户 base palette。
///
/// @author kongweiguang
#[test]
fn osc_palette_override_is_shared_by_render_and_query_then_reset() {
    let mut engine = engine(12, 2);
    let base = TerminalPalette::default();
    engine.process_output(b"\x1b]4;16;rgb:12/34/56\x07\x1b[38;5;16mX");

    let snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
    let run = snapshot.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("colored glyph run");
    assert_eq!(
        snapshot.palette.resolve(run.style.foreground),
        TerminalRgb::new(0x12, 0x34, 0x56)
    );

    let reply = engine
        .process_output(b"\x1b]4;16;?\x07")
        .pty_writes
        .concat();
    let reply = String::from_utf8(reply).expect("OSC color response is ASCII");
    assert!(
        reply.contains("rgb:1212/3434/5656"),
        "OSC 4 reply: {reply:?}"
    );

    engine.process_output(b"\x1b]104;16\x07");
    let reset = engine.render_snapshot(RenderSnapshotRequest::default());
    let run = reset.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("glyph remains in grid");
    assert_eq!(
        reset.palette.resolve(run.style.foreground),
        base.resolve(TerminalColor::Indexed(16))
    );
}

/// 以 Alacritty 的实际 cell metadata 提供光标 glyph、原样式与宽字符列数。
///
/// @author kongweiguang
#[test]
fn cursor_snapshot_exposes_blank_and_wide_glyph_cells() {
    let mut engine = engine(12, 2);
    let blank = engine.render_snapshot(RenderSnapshotRequest::default());
    let blank_cell = blank.cursor_cell.expect("visible cursor has a cell");
    assert_eq!(blank_cell.text.as_ref(), " ");
    assert_eq!(blank_cell.columns, 1);
    assert_eq!(blank_cell.start_column, blank.cursor.column);

    engine.process_output("\x1b[1;1H界\x1b[1;1H".as_bytes());
    let wide = engine.render_snapshot(RenderSnapshotRequest::default());
    let wide_cell = wide.cursor_cell.expect("wide glyph cursor cell");
    assert_eq!(wide_cell.text.as_ref(), "界");
    assert_eq!(wide_cell.columns, 2);
    assert_eq!(wide_cell.start_column, 0);
}

/// 按 Alacritty 对 DECSCUSR 的解码结果传递光标闪烁意图，不让 UI 从 shape 猜测。
///
/// @author kongweiguang
#[test]
fn cursor_snapshot_preserves_decscusr_blink_flag() {
    let mut engine = engine(12, 2);
    for (sequence, expected_shape, expected_blinking) in [
        (
            b"\x1b[1 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Block,
            true,
        ),
        (
            b"\x1b[2 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Block,
            false,
        ),
        (
            b"\x1b[3 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Underline,
            true,
        ),
        (
            b"\x1b[4 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Underline,
            false,
        ),
        (
            b"\x1b[5 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Beam,
            true,
        ),
        (
            b"\x1b[6 q".as_slice(),
            kerminal_terminal::snapshot::TerminalCursorShape::Beam,
            false,
        ),
    ] {
        engine.process_output(sequence);
        let cursor = engine
            .render_snapshot(RenderSnapshotRequest::default())
            .cursor;
        assert_eq!(cursor.shape, expected_shape);
        assert_eq!(cursor.blinking, expected_blinking);
    }
}

/// 确认 UTF-8 byte range 到 cell column 的映射遵循宽字与组合字符的真实网格占用。
///
/// @author kongweiguang
#[test]
fn render_runs_map_utf8_graphemes_to_terminal_columns() {
    let mut engine = engine(12, 2);
    engine.process_output("A界e\u{301}".as_bytes());

    let snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
    let run = snapshot.rows[0]
        .runs
        .iter()
        .find(|run| run.text.starts_with("A界e\u{301}"))
        .expect("glyphs share the default style run");
    assert_eq!(run.cell_spans[0].byte_range, 0..1);
    assert_eq!(run.cell_spans[0].column_range, 0..1);
    assert_eq!(run.cell_spans[1].byte_range, 1..4);
    assert_eq!(run.cell_spans[1].column_range, 1..3);
    assert_eq!(run.cell_spans[2].byte_range, 4..7);
    assert_eq!(run.cell_spans[2].column_range, 3..4);
}

/// 搜索命中坐标必须覆盖宽字完整列并让组合字符映射回基字符的 cell。
///
/// @author kongweiguang
#[test]
fn search_ranges_cover_wide_and_combining_cells() {
    let mut engine = engine(12, 2);
    engine.process_output("界 e\u{301}".as_bytes());
    let search = |query: &str| TerminalSearchRequest {
        query: query.to_owned(),
        case_sensitive: true,
        regex: false,
        backwards: false,
        max_matches: 10,
    };

    let wide = engine.search(&search("界")).expect("search wide character");
    assert_eq!(wide.matches[0].range.start.column, 0);
    assert_eq!(wide.matches[0].range.end.column, 2);
    let combining = engine
        .search(&search("e\u{301}"))
        .expect("search composed grapheme");
    assert_eq!(combining.matches[0].range.start.column, 3);
    assert_eq!(combining.matches[0].range.end.column, 4);
}

/// 搜索应跨越 WRAPLINE 拼接自动折行文本，并返回可供每行分别高亮的绝对网格范围。
///
/// @author kongweiguang
#[test]
fn search_matches_across_soft_wrap_and_projects_physical_cell_range() {
    let mut engine = engine(4, 2);
    engine.process_output(b"abcdef");
    let result = engine
        .search(&TerminalSearchRequest {
            query: "cdef".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search across soft wrap");

    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].range,
        TerminalPoint { line: 0, column: 2 }..TerminalPoint { line: 1, column: 2 }
    );
}

/// 硬回车仍结束搜索逻辑行，避免把相邻但独立的命令输出拼成一次命中。
///
/// @author kongweiguang
#[test]
fn search_does_not_join_across_hard_newline() {
    let mut engine = engine(8, 2);
    engine.process_output(b"ab\r\ncd");
    let result = engine
        .search(&TerminalSearchRequest {
            query: "bcd".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search hard line boundary");

    assert!(result.matches.is_empty());
}

/// 跨折行命中要覆盖宽字符的两列，并将组合字符映射回其基字符 cell。
///
/// @author kongweiguang
#[test]
fn search_projects_wide_and_combining_graphemes_across_soft_wrap() {
    let mut engine = engine(5, 2);
    engine.process_output("ab界e\u{301}x".as_bytes());
    let result = engine
        .search(&TerminalSearchRequest {
            query: "界e\u{301}x".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search unicode across soft wrap");

    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].range,
        TerminalPoint { line: 0, column: 2 }..TerminalPoint { line: 1, column: 1 }
    );
}

/// 宽字无法放入末列时，前行的 leading spacer 只用于占位，不应插入逻辑搜索文本。
///
/// @author kongweiguang
#[test]
fn search_skips_leading_wide_spacer_at_soft_wrap_boundary() {
    let mut engine = engine(4, 2);
    engine.process_output("abc界Z".as_bytes());
    let result = engine
        .search(&TerminalSearchRequest {
            query: "c界Z".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search wide glyph moved to next row");

    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].range,
        TerminalPoint { line: 0, column: 2 }..TerminalPoint { line: 1, column: 3 }
    );
}

/// 搜索覆盖 bounded scrollback 中的软折行边界，坐标保留 Alacritty 的负绝对历史行号。
///
/// @author kongweiguang
#[test]
fn search_matches_soft_wrap_inside_scrollback() {
    let mut engine = engine(4, 2);
    engine.process_output(b"abcdefghijkl");
    let result = engine
        .search(&TerminalSearchRequest {
            query: "cdef".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search scrollback wrap");

    assert_eq!(result.matches.len(), 1);
    let range = &result.matches[0].range;
    assert!(range.start.line < 0, "match starts in scrollback");
    assert_eq!(range.end.line, range.start.line + 1);
    assert_eq!(range.start.column, 2);
    assert_eq!(range.end.column, 2);
}

/// 调整列宽后重新读取 Alacritty 已 reflow 的网格，搜索坐标随当前物理行布局更新。
///
/// @author kongweiguang
#[test]
fn search_uses_reflowed_physical_coordinates_after_resize() {
    let mut engine = engine(6, 2);
    engine.process_output(b"abcdefgh");
    engine
        .resize(TerminalSize::new(4, 2, 0, 0).expect("valid resized grid"))
        .expect("reflow terminal grid");

    let result = engine
        .search(&TerminalSearchRequest {
            query: "efgh".to_owned(),
            case_sensitive: true,
            regex: false,
            backwards: false,
            max_matches: 10,
        })
        .expect("search reflowed grid");

    assert_eq!(result.matches.len(), 1);
    assert_eq!(
        result.matches[0].range,
        TerminalPoint { line: 1, column: 0 }..TerminalPoint { line: 1, column: 4 }
    );
}

/// 空正则命中不生成不可绘制的零宽范围，同时仍将无效正则明确返回为搜索错误。
///
/// @author kongweiguang
#[test]
fn search_skips_empty_matches_and_rejects_invalid_regex() {
    let mut engine = engine(8, 2);
    engine.process_output(b"alpha");
    let request = |query: &str| TerminalSearchRequest {
        query: query.to_owned(),
        case_sensitive: true,
        regex: true,
        backwards: false,
        max_matches: 10,
    };

    let empty = engine.search(&request("^")).expect("zero-width regex");
    assert!(empty.matches.is_empty());
    assert!(engine.search(&request("[")).is_err());
}

/// 证明 OSC 133 只来自 parser 的原始 PTY 输入，并按 read chunk 与协议顺序传给 runtime。
///
/// @author kongweiguang
#[test]
fn shell_integration_events_follow_real_fragmented_pty_bytes() {
    use kerminal_terminal::ShellIntegrationMarker;

    let mut engine = engine(12, 2);
    assert_eq!(
        engine
            .process_output(b"\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C")
            .shell_integration,
        vec![
            ShellIntegrationMarker::PromptStart,
            ShellIntegrationMarker::PromptEnd
        ],
    );
    assert_eq!(
        engine
            .process_output(b"\x07\x1b]133;D;0\x1b\\")
            .shell_integration,
        vec![
            ShellIntegrationMarker::CommandStart,
            ShellIntegrationMarker::CommandFinished { exit_code: Some(0) },
        ],
    );
}

/// 重连会以新 engine/parser 替换旧代状态，不能把旧 OSC 尾字节拼成新命令事件。
///
/// @author kongweiguang
#[test]
fn reconnect_discards_incomplete_shell_integration_marker() {
    use kerminal_terminal::ShellIntegrationMarker;

    let mut engine = engine(12, 2);
    assert!(
        engine
            .process_output(b"\x1b]133;D;")
            .shell_integration
            .is_empty()
    );
    engine
        .reset_for_reconnect()
        .expect("same-size engine reset");
    assert!(engine.process_output(b"0\x07").shell_integration.is_empty());
    assert_eq!(
        engine.process_output(b"\x1b]133;A\x07").shell_integration,
        vec![ShellIntegrationMarker::PromptStart],
    );
}
