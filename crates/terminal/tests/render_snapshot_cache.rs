//! 行缓存回归覆盖：保留完整快照 DTO，同时验证干净 runs 真正跨 revision 复用。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::snapshot::{
    RenderSnapshot, RenderSnapshotRequest, TerminalColor, TerminalPalette, TerminalPoint,
    TerminalRgb, TerminalSearchRequest, TerminalSelection, TerminalSelectionKind,
};

/// 创建小型真实 VT 网格，让缓存命中和各类失效行为保持可读且确定。
///
/// @author kongweiguang
fn engine(columns: u16, rows: u16) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive terminal grid");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default()).expect("bounded engine")
}

/// 读取每行已合并的 UTF-8 文本，避免测试依赖 UI 字体和渲染器。
///
/// @author kongweiguang
fn row_text(snapshot: &RenderSnapshot, row: usize) -> String {
    snapshot.rows[row]
        .runs
        .iter()
        .map(|run| run.text.as_ref())
        .collect::<Vec<_>>()
        .join("")
        .trim_end()
        .to_owned()
}

/// 验证一次只改一行时，干净行复用原 runs Arc，修改行重新解析并冻结 runs。
///
/// @author kongweiguang
#[test]
fn partial_damage_rebuilds_only_the_changed_row_runs() {
    let mut engine = engine(16, 3);
    engine.process_output(b"row0\r\nrow1\r\nrow2");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());

    engine.process_output(b"\x1b[2;1Hchanged");
    let after = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(before.revision),
    });

    assert_eq!(
        after.rows.len(),
        3,
        "snapshot keeps the complete viewport contract"
    );
    assert_eq!(row_text(&after, 1).trim_end(), "changed");
    assert!(Arc::ptr_eq(&before.rows[0].runs, &after.rows[0].runs));
    assert!(!Arc::ptr_eq(&before.rows[1].runs, &after.rows[1].runs));
    assert!(after.dirty_rows.contains(&1));
}

/// 验证 cursor-only 更新仍使用 Alacritty 的 damage 行刷新，同时快照光标与屏幕文本保持同步。
///
/// @author kongweiguang
#[test]
fn cursor_movement_refreshes_cursor_without_changing_screen_text() {
    let mut engine = engine(16, 2);
    engine.process_output(b"abcdef");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());

    engine.process_output(b"\x1b[1;5H");
    let after = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(before.revision),
    });

    assert_eq!(after.cursor.column, 4);
    assert!(after.cursor.visible);
    assert_eq!(
        after.cursor_cell.as_ref().map(|cell| cell.text.as_ref()),
        Some("e")
    );
    assert_eq!(row_text(&before, 0), row_text(&after, 0));
    assert!(after.dirty_rows.contains(&0));
}

/// 验证 selection/theme/search 元数据变更会照常出现在快照，但不重建独立的 cell runs。
///
/// @author kongweiguang
#[test]
fn selection_palette_and_search_keep_cached_cell_runs() {
    let mut engine = engine(16, 2);
    engine.process_output(b"searchable text");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());

    engine.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 3 },
        kind: TerminalSelectionKind::Character,
    }));
    engine.set_palette(TerminalPalette::with_defaults(
        TerminalRgb::new(0x11, 0x22, 0x33),
        TerminalRgb::new(0x04, 0x05, 0x06),
        TerminalRgb::new(0x77, 0x88, 0x99),
    ));
    let search = engine.search(&TerminalSearchRequest {
        query: "searchable".to_owned(),
        case_sensitive: true,
        regex: false,
        backwards: false,
        max_matches: 4,
    });
    assert!(search.is_ok());

    let after = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(before.revision),
    });

    assert!(after.selection.is_some());
    assert_eq!(after.palette.foreground, TerminalRgb::new(0x11, 0x22, 0x33));
    assert!(after.dirty_rows.contains(&0));
    assert!(Arc::ptr_eq(&before.rows[0].runs, &after.rows[0].runs));
}

/// 重复应用相同主题色盘不推进 revision，也不伪造全屏 damage 或重建语义 cell runs。
///
/// @author kongweiguang
#[test]
fn identical_palette_does_not_advance_revision_or_damage_rows() {
    let mut engine = engine(16, 2);
    engine.process_output(b"stable palette");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());

    engine.set_palette(before.palette);
    let after = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(before.revision),
    });

    assert_eq!(after.revision, before.revision);
    assert_eq!(after.palette, before.palette);
    assert!(after.dirty_rows.is_empty());
    assert!(Arc::ptr_eq(&before.rows[0].runs, &after.rows[0].runs));
}

/// 真正改变主题色仍推进一次 revision 并标记全屏 damage，同时让语义色解析到新 RGB。
///
/// @author kongweiguang
#[test]
fn changed_palette_advances_revision_damages_all_rows_and_updates_resolved_color() {
    let mut engine = engine(16, 2);
    engine.process_output(b"\x1b[39mX");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());
    let palette = TerminalPalette::with_defaults(
        TerminalRgb::new(0x11, 0x22, 0x33),
        TerminalRgb::new(0x04, 0x05, 0x06),
        TerminalRgb::new(0x77, 0x88, 0x99),
    );

    engine.set_palette(palette);
    let after = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(before.revision),
    });

    assert_eq!(after.revision, before.revision.wrapping_add(1));
    assert_eq!(after.dirty_rows.as_ref(), &[0, 1]);
    assert_eq!(
        after.palette.resolve(TerminalColor::DefaultForeground),
        palette.foreground
    );
    assert!(Arc::ptr_eq(&before.rows[0].runs, &after.rows[0].runs));
}

/// 验证 OSC override/reset 与主题切换共享一份有效色盘，cell run 只保存稳定语义色。
///
/// @author kongweiguang
#[test]
fn osc_palette_and_theme_changes_keep_effective_colors_consistent() {
    let mut engine = engine(16, 2);
    let default = TerminalPalette::default();
    let base_color = TerminalRgb::new(0x10, 0x20, 0x30);
    let mut base_ansi = default.ansi;
    base_ansi[0] = base_color;
    let base = TerminalPalette::from_ansi(
        default.foreground,
        default.background,
        default.cursor,
        base_ansi,
    );
    engine.set_palette(base);
    engine.process_output(b"\x1b[30mX\r\ncursor");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());
    let original_style = before.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("indexed-color glyph exists before OSC override")
        .style
        .foreground;
    assert_eq!(original_style, TerminalColor::Ansi(0));
    assert_eq!(before.palette.resolve(original_style), base_color);

    let override_color = TerminalRgb::new(0x12, 0x34, 0x56);
    engine.process_output(b"\x1b]4;0;rgb:12/34/56\x07");
    let overridden = engine.render_snapshot(RenderSnapshotRequest::default());
    let run = overridden.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("ANSI-color glyph remains in grid");

    assert_eq!(
        overridden.palette.resolve(run.style.foreground),
        override_color
    );
    assert_eq!(run.style.foreground, original_style);

    let theme_color = TerminalRgb::new(0x70, 0x80, 0x90);
    let mut theme_ansi = base.ansi;
    theme_ansi[0] = theme_color;
    let theme =
        TerminalPalette::from_ansi(base.foreground, base.background, base.cursor, theme_ansi);
    engine.set_palette(theme);
    let themed = engine.render_snapshot(RenderSnapshotRequest::default());
    let themed_run = themed.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("ANSI-color glyph remains after theme change");
    assert_eq!(
        themed.palette.resolve(themed_run.style.foreground),
        override_color
    );
    assert!(Arc::ptr_eq(&overridden.rows[0].runs, &themed.rows[0].runs));

    engine.process_output(b"\x1b]104;0\x07");
    let reset = engine.render_snapshot(RenderSnapshotRequest::default());
    let reset_run = reset.rows[0]
        .runs
        .iter()
        .find(|run| run.text.contains('X'))
        .expect("ANSI-color glyph remains after reset");
    assert_eq!(
        reset.palette.resolve(reset_run.style.foreground),
        theme_color
    );
    assert_eq!(reset_run.style.foreground, original_style);
}

/// 验证滚动、resize 与备用屏切换不会把另一 viewport 或 screen 的缓存内容重放出来。
///
/// @author kongweiguang
#[test]
fn scroll_resize_and_alternate_screen_invalidate_viewport_cache() {
    let mut engine = engine(12, 2);
    engine.process_output(b"first\r\nsecond\r\nthird");
    let bottom = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(row_text(&bottom, 1).contains("third"));

    engine.scroll(1);
    let scrolled = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(bottom.revision),
    });
    assert_eq!(scrolled.scrollback_offset, 1);
    assert!(scrolled.history_size >= scrolled.scrollback_offset);
    assert!(
        scrolled
            .rows
            .iter()
            .any(|row| row_text(&scrolled, usize::from(row.viewport_row)).contains("first"))
    );

    let resized = TerminalSize::new(20, 3, 0, 0).expect("valid resized grid");
    assert!(engine.resize(resized).is_ok());
    let resized_snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(resized_snapshot.size, resized);
    assert_eq!(resized_snapshot.rows.len(), 3);

    engine.process_output(b"\x1b[?1049hALT");
    let alternate = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(alternate.modes.alternate_screen);
    assert!(
        alternate
            .rows
            .iter()
            .any(|row| row_text(&alternate, usize::from(row.viewport_row)).contains("ALT"))
    );
    assert!(
        !alternate
            .rows
            .iter()
            .any(|row| row_text(&alternate, usize::from(row.viewport_row)).contains("first"))
    );

    engine.process_output(b"\x1b[?1049l");
    let main = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(!main.modes.alternate_screen);
    assert!(
        main.rows
            .iter()
            .any(|row| row_text(&main, usize::from(row.viewport_row)).contains("third"))
    );
}

/// 已离开底部的视口在持续输出时保持同一阅读锚点，快照刷新只读引擎当前 offset。
///
/// @author kongweiguang
#[test]
fn scrolled_snapshot_stays_anchored_while_output_continues() {
    let mut engine = engine(16, 2);
    engine.process_output(b"line-0\r\nline-1\r\nline-2\r\nline-3");

    engine.scroll(1);
    let paused = engine.render_snapshot(RenderSnapshotRequest::default());
    let paused_rows = paused
        .rows
        .iter()
        .map(|row| row_text(&paused, usize::from(row.viewport_row)))
        .collect::<Vec<_>>();
    assert_eq!(paused.scrollback_offset, 1);

    engine.process_output(b"\r\nline-4");
    let after_output = engine.render_snapshot(RenderSnapshotRequest {
        since_revision: Some(paused.revision),
    });

    assert!(after_output.scrollback_offset > paused.scrollback_offset);
    assert_eq!(
        after_output
            .rows
            .iter()
            .map(|row| row_text(&after_output, usize::from(row.viewport_row)))
            .collect::<Vec<_>>(),
        paused_rows,
        "Alacritty's increased offset must keep the previously visible lines anchored"
    );
}

/// 验证 damage 后宽字符、组合字符和 OSC 8 超链接都从当前 cell 重建，而不是命中旧 runs。
///
/// @author kongweiguang
#[test]
fn damaged_wide_combining_and_hyperlink_cells_are_rebuilt() {
    let mut engine = engine(16, 2);
    engine.process_output(b"\x1b]8;;https://old.example\x1b\\");
    engine.process_output("界e\u{301}".as_bytes());
    engine.process_output(b"\x1b]8;;\x1b\\");
    let before = engine.render_snapshot(RenderSnapshotRequest::default());
    assert_eq!(row_text(&before, 0), "界e\u{301}");

    engine.process_output(b"\r\x1b]8;;https://new.example\x1b\\");
    engine.process_output("猫o\u{301}".as_bytes());
    engine.process_output(b"\x1b]8;;\x1b\\");
    let after = engine.render_snapshot(RenderSnapshotRequest::default());

    assert_eq!(row_text(&after, 0), "猫o\u{301}");
    let run = after.rows[0]
        .runs
        .iter()
        .find(|run| !run.text.is_empty())
        .expect("updated linked glyph run");
    assert_eq!(run.cell_spans[0].column_range, 0..2);
    assert_eq!(run.cell_spans[1].column_range, 2..3);
    assert_eq!(
        run.hyperlink.as_ref().map(|link| link.uri.as_ref()),
        Some("https://new.example")
    );
    assert!(!Arc::ptr_eq(&before.rows[0].runs, &after.rows[0].runs));
}
