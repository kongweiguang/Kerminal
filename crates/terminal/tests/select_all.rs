//! 覆盖全选在 scrollback、活动备用屏与空网格上的真实 Alacritty 边界。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::snapshot::{
    RenderSnapshotRequest, TerminalPoint, TerminalSelection, TerminalSelectionKind,
};

/// 构造小网格，使滚屏历史和屏幕末行都能由固定坐标验证。
///
/// @author kongweiguang
fn engine(columns: u16, rows: u16) -> TerminalEngine {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("valid terminal grid");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("bounded terminal grid")
}

/// 全选覆盖主屏实际历史与屏幕末行，不选择历史后一个并不存在的网格行。
///
/// @author kongweiguang
#[test]
fn selects_primary_scrollback_through_last_screen_row() {
    let mut engine = engine(16, 2);
    engine.process_output(b"first\r\nsecond\r\nthird");

    engine.select_all();

    let snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
    let selection = snapshot.selection.expect("non-empty primary grid selected");
    assert_eq!(selection.kind, TerminalSelectionKind::Character);
    assert!(
        selection.start.line < 0,
        "history begins at a negative grid line"
    );
    assert_eq!(selection.end.line, 1, "last valid row in the two-row grid");
    assert_eq!(selection.start.column, 0);
    assert_eq!(selection.end.column, 15);

    let text = engine.selected_text().expect("selected terminal text");
    assert!(text.contains("first"));
    assert!(text.contains("second"));
    assert!(text.contains("third"));
}

/// 空网格不生成无意义的全屏选择；备用屏全选只针对活动网格，不带入主屏历史。
///
/// @author kongweiguang
#[test]
fn empty_grid_clears_selection_and_alternate_screen_is_isolated() {
    let mut empty = engine(16, 2);
    empty.set_selection(Some(TerminalSelection {
        start: TerminalPoint { line: 0, column: 0 },
        end: TerminalPoint { line: 0, column: 0 },
        kind: TerminalSelectionKind::Character,
    }));
    empty.select_all();
    assert!(
        empty
            .render_snapshot(RenderSnapshotRequest::default())
            .selection
            .is_none()
    );

    let mut engine = engine(16, 2);
    engine.process_output(b"primary-old\r\nprimary-new\r\n");
    engine.process_output(b"\x1b[?1049halt-screen");

    engine.select_all();

    let snapshot = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(snapshot.modes.alternate_screen);
    let selection = snapshot.selection.expect("alternate screen selected");
    assert_eq!(
        selection.start.line, 0,
        "alternate grid has no primary history"
    );
    assert_eq!(
        selection.end.line, 1,
        "selection ends at its final screen row"
    );
    let text = engine
        .selected_text()
        .expect("selected alternate-screen text");
    assert!(text.contains("alt-screen"));
    assert!(!text.contains("primary-old"));

    engine.process_output(b"\x1b[?1049l");
    let restored = engine.render_snapshot(RenderSnapshotRequest::default());
    assert!(!restored.modes.alternate_screen);
}
