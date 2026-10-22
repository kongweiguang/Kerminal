//! 搜索工具纯数据边界回归测试。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_domain::identity::SessionId;
use kerminal_terminal::snapshot::{TerminalPoint, TerminalRow, TerminalSelectionKind};
use kerminal_ui::features::terminal_tools::selection::{ViewportCell, ViewportSelectionDrag};
use kerminal_ui::features::terminal_tools::{
    TerminalLinkError, TextHighlightRange, command_block_timeline_visible,
    normalized_text_segments, selection_from_viewport_drag, validate_terminal_link,
};

/// 命令块右栏遵循设置开关，打开后也只展示真实存在的命令块。
#[test]
fn command_block_timeline_requires_policy_and_real_blocks() {
    assert!(!command_block_timeline_visible(false, false));
    assert!(!command_block_timeline_visible(false, true));
    assert!(!command_block_timeline_visible(true, false));
    assert!(command_block_timeline_visible(true, true));
}

/// OSC 8 外链只允许 HTTP(S)，并拒绝凭据和控制字符注入。
#[test]
fn osc8_link_accepts_only_public_http_schemes_without_credentials() {
    let link = validate_terminal_link("https://example.com/docs?q=gpui").unwrap();
    assert_eq!(link.as_str(), "https://example.com/docs?q=gpui");

    assert_eq!(
        validate_terminal_link("file:///etc/passwd"),
        Err(TerminalLinkError::UnsupportedScheme)
    );
    assert_eq!(
        validate_terminal_link("https://user:secret@example.com/"),
        Err(TerminalLinkError::UserInfo)
    );
    assert_eq!(
        validate_terminal_link("https://example.com/\r\nmalicious"),
        Err(TerminalLinkError::ForbiddenCharacters)
    );
    assert_eq!(
        validate_terminal_link(&format!("https://example.com/{}", "x".repeat(4096))),
        Err(TerminalLinkError::TooLong)
    );
}

/// 关键词偏移按 UTF-8 字符边界合并，避免高亮分段切坏中文字符。
#[test]
fn text_highlights_are_merged_and_never_split_utf8_characters() {
    let text = "A你好B";
    let segments = normalized_text_segments(
        text,
        &[
            TextHighlightRange {
                start_byte: 2,
                end_byte: 5,
            },
            TextHighlightRange {
                start_byte: 6,
                end_byte: 8,
            },
            TextHighlightRange {
                start_byte: 90,
                end_byte: 120,
            },
        ],
    );

    assert_eq!(
        segments
            .iter()
            .map(|segment| (segment.text.as_str(), segment.highlighted))
            .collect::<Vec<_>>(),
        vec![("A", false), ("你好B", true)]
    );
    assert_eq!(
        segments
            .iter()
            .map(|segment| segment.text.as_str())
            .collect::<Vec<_>>()
            .concat(),
        text
    );
    assert!(
        normalized_text_segments(
            text,
            &[TextHighlightRange {
                start_byte: 2,
                end_byte: 2,
            }],
        )
        .iter()
        .all(|segment| !segment.highlighted)
    );
}

/// 画布 cell 位置映射到稳定的绝对终端行号并限制越界列。
#[test]
fn viewport_drag_maps_rows_to_absolute_lines_and_clamps_columns() {
    let rows = [
        TerminalRow {
            viewport_row: 0,
            absolute_line: 41,
            wrapped: false,
            runs: Arc::from([]),
        },
        TerminalRow {
            viewport_row: 4,
            absolute_line: 42,
            wrapped: false,
            runs: Arc::from([]),
        },
    ];

    let selection = selection_from_viewport_drag(
        &rows,
        4,
        ViewportCell { row: 0, column: 8 },
        ViewportCell { row: 4, column: 2 },
        TerminalSelectionKind::Character,
    )
    .unwrap();

    assert_eq!(selection.start.line, 41);
    assert_eq!(selection.start.column, 3);
    assert_eq!(selection.end.line, 42);
    assert_eq!(selection.end.column, 2);
    assert!(
        selection_from_viewport_drag(
            &rows,
            0,
            ViewportCell { row: 0, column: 0 },
            ViewportCell { row: 1, column: 0 },
            TerminalSelectionKind::Character,
        )
        .is_none()
    );
    assert!(
        selection_from_viewport_drag(
            &rows,
            4,
            ViewportCell { row: 1, column: 0 },
            ViewportCell { row: 4, column: 0 },
            TerminalSelectionKind::Character,
        )
        .is_none()
    );
}

/// 拖动跨 scrollback 变化仍绑定按下时的绝对行号，异会话结束和单击都不能伪造选择。
#[test]
fn selection_drag_state_keeps_absolute_anchor_across_scrollback_changes() {
    let session = SessionId::new();
    let other_session = SessionId::new();
    let start = TerminalPoint {
        line: 41,
        column: 1,
    };
    let end = TerminalPoint {
        line: 52,
        column: 2,
    };

    assert_eq!(
        ViewportSelectionDrag::new(session.clone(), start, false).finish(&other_session, end),
        None
    );
    assert_eq!(
        ViewportSelectionDrag::new(session.clone(), start, true)
            .finish(&session, start)
            .map(|completed| (
                completed.start,
                completed.open_link_on_click,
                completed.selection
            )),
        Some((start, true, None))
    );

    let completed = ViewportSelectionDrag::new(session.clone(), start, false)
        .finish(&session, end)
        .unwrap();
    let selection = completed.selection.unwrap();
    assert_eq!(selection.start.line, 41);
    assert_eq!(selection.start.column, 1);
    assert_eq!(selection.end.line, 52);
    assert_eq!(selection.end.column, 2);
    assert!(!completed.open_link_on_click);
}
