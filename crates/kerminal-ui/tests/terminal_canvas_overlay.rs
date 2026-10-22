//! 终端 overlay 的单元契约覆盖，重点验证绝对行、半开范围和 Unicode grapheme 对齐。
//!
//! @author kongweiguang

#[path = "../src/terminal_canvas/overlay.rs"]
mod overlay;

use std::ops::Range;
use std::sync::Arc;

use kerminal_terminal::snapshot::{
    TerminalAttributes, TerminalColor, TerminalPoint, TerminalRgb, TerminalRow, TerminalRun,
    TerminalSelection, TerminalSelectionKind, TerminalStyle, TerminalTextCellSpan,
};
use overlay::{
    CellOverlayPaint, CellOverlaySpan, CellTextSegment, TerminalCellOverlay, TerminalOverlayKind,
    TerminalOverlayStyle, search_result_revision_matches, spans_for_row, text_overlay_segments,
};

/// 搜索覆盖层只允许在服务结果对应的 engine revision 上显示。
///
/// @author kongweiguang
#[test]
fn search_highlight_requires_matching_snapshot_revision() {
    assert!(search_result_revision_matches(Some(52), 52));
    assert!(!search_result_revision_matches(Some(52), 53));
    assert!(!search_result_revision_matches(None, 52));
}

/// 构造只含行身份的快照视图，测试聚焦坐标语义而不重复终端引擎的绘制数据。
///
/// @author kongweiguang
fn row(absolute_line: i64, viewport_row: u16) -> TerminalRow {
    TerminalRow {
        viewport_row,
        absolute_line,
        wrapped: false,
        runs: Arc::from([]),
    }
}

/// 为 wide-cell 映射验收构造最小行，避免依赖字体猜测或完整终端引擎。
///
/// @author kongweiguang
fn row_with_wide_grapheme(absolute_line: i64, viewport_row: u16) -> TerminalRow {
    TerminalRow {
        viewport_row,
        absolute_line,
        wrapped: false,
        runs: Arc::from([TerminalRun {
            start_column: 1,
            end_column: 3,
            text: Arc::from("中"),
            cell_spans: Arc::from([TerminalTextCellSpan {
                byte_range: 0.."中".len(),
                column_range: 1..3,
            }]),
            style: TerminalStyle {
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                underline_color: None,
                attributes: TerminalAttributes::default(),
            },
            hyperlink: None,
        }]),
    }
}

/// 高亮命中部分宽字符时扩展到整个 grapheme，组合符仍与基字符同属一个范围。
///
/// @author kongweiguang
#[test]
fn overlay_segments_expand_to_full_wide_and_combining_graphemes() {
    let spans = [
        CellOverlaySpan {
            start_column: 2,
            end_column: 3,
            kind: TerminalOverlayKind::Search,
            style: None,
        },
        CellOverlaySpan {
            start_column: 5,
            end_column: 6,
            kind: TerminalOverlayKind::Keyword,
            style: None,
        },
    ];

    let segments = text_overlay_segments("A中e\u{301}🙂", &[], 0, 6, &spans);

    assert_eq!(
        segments,
        vec![
            CellTextSegment {
                byte_range: 0..1,
                column_range: 0..1,
                visible_columns: 0..1,
                paint: None,
            },
            CellTextSegment {
                byte_range: 1..4,
                column_range: 1..3,
                visible_columns: 1..3,
                paint: Some(CellOverlayPaint {
                    kind: TerminalOverlayKind::Search,
                    style: None,
                }),
            },
            CellTextSegment {
                byte_range: 4..7,
                column_range: 3..4,
                visible_columns: 3..4,
                paint: None,
            },
            CellTextSegment {
                byte_range: 7..11,
                column_range: 4..6,
                visible_columns: 4..6,
                paint: Some(CellOverlayPaint {
                    kind: TerminalOverlayKind::Keyword,
                    style: None,
                }),
            },
        ]
    );
}

/// 一个 cell 同时命中多个覆盖层时保留优先级最高的 active search。
///
/// @author kongweiguang
#[test]
fn overlay_segments_choose_highest_priority_for_overlapping_ranges() {
    let spans = [
        CellOverlaySpan {
            start_column: 0,
            end_column: 2,
            kind: TerminalOverlayKind::Keyword,
            style: None,
        },
        CellOverlaySpan {
            start_column: 1,
            end_column: 2,
            kind: TerminalOverlayKind::ActiveSearch,
            style: None,
        },
    ];

    assert_eq!(
        text_overlay_segments("xy", &[], 0, 2, &spans),
        vec![
            CellTextSegment {
                byte_range: 0..1,
                column_range: 0..1,
                visible_columns: 0..1,
                paint: Some(CellOverlayPaint {
                    kind: TerminalOverlayKind::Keyword,
                    style: None,
                }),
            },
            CellTextSegment {
                byte_range: 1..2,
                column_range: 1..2,
                visible_columns: 1..2,
                paint: Some(CellOverlayPaint {
                    kind: TerminalOverlayKind::ActiveSearch,
                    style: None,
                }),
            },
        ]
    );
}

/// 选择端点使用 inclusive 语义而搜索 range 使用 exclusive 语义，且按绝对行过滤。
///
/// @author kongweiguang
#[test]
fn row_spans_resolve_reverse_selection_and_absolute_overlay_ranges() {
    let selected = TerminalSelection {
        start: TerminalPoint {
            line: 42,
            column: 5,
        },
        end: TerminalPoint {
            line: 42,
            column: 3,
        },
        kind: TerminalSelectionKind::Character,
    };
    let overlay = TerminalCellOverlay {
        range: TerminalPoint {
            line: 42,
            column: 1,
        }..TerminalPoint {
            line: 42,
            column: 3,
        },
        kind: TerminalOverlayKind::Search,
        style: None,
    };

    let spans = spans_for_row(&row(42, 7), 8, Some(selected), &[overlay]);

    assert_eq!(
        spans,
        vec![
            CellOverlaySpan {
                start_column: 1,
                end_column: 3,
                kind: TerminalOverlayKind::Search,
                style: None,
            },
            CellOverlaySpan {
                start_column: 3,
                end_column: 6,
                kind: TerminalOverlayKind::Selection,
                style: None,
            },
        ]
    );
    assert!(spans_for_row(&row(43, 8), 8, Some(selected), &[]).is_empty());
}

/// 引擎已把语义词选区展开为真实端点，Canvas 按完整 cell 区间绘制与字符选择相同的反色。
///
/// @author kongweiguang
#[test]
fn semantic_word_selection_uses_engine_expanded_range() {
    let selected = TerminalSelection {
        start: TerminalPoint {
            line: 42,
            column: 2,
        },
        end: TerminalPoint {
            line: 42,
            column: 5,
        },
        kind: TerminalSelectionKind::Word,
    };

    assert_eq!(
        spans_for_row(&row(42, 0), 8, Some(selected), &[]),
        vec![CellOverlaySpan {
            start_column: 2,
            end_column: 6,
            kind: TerminalOverlayKind::Selection,
            style: None,
        }]
    );
}

/// 拒绝空网格与行外命中，避免窄窗口下产生溢出的 cell quad。
///
/// @author kongweiguang
#[test]
fn row_spans_clip_to_columns_and_ignore_empty_ranges() {
    let overlays = [
        TerminalCellOverlay {
            range: TerminalPoint { line: 4, column: 6 }..TerminalPoint {
                line: 4,
                column: 20,
            },
            kind: TerminalOverlayKind::Search,
            style: None,
        },
        TerminalCellOverlay {
            range: TerminalPoint { line: 5, column: 0 }..TerminalPoint { line: 5, column: 4 },
            kind: TerminalOverlayKind::Search,
            style: None,
        },
    ];
    assert_eq!(
        spans_for_row(&row(4, 0), 8, None, &overlays),
        vec![CellOverlaySpan {
            start_column: 6,
            end_column: 8,
            kind: TerminalOverlayKind::Search,
            style: None,
        }]
    );
    assert!(spans_for_row(&row(4, 0), 0, None, &overlays).is_empty());
}

/// 编译期固定 overlay range 的标准类型为半开 TerminalPoint 区间。
///
/// @author kongweiguang
#[test]
fn overlay_ranges_are_end_exclusive() {
    let range: Range<TerminalPoint> = TerminalPoint {
        line: -2,
        column: 1,
    }..TerminalPoint {
        line: -2,
        column: 2,
    };
    let overlay = TerminalCellOverlay {
        range,
        kind: TerminalOverlayKind::Keyword,
        style: None,
    };

    assert_eq!(
        spans_for_row(&row(-2, 0), 10, None, &[overlay])[0].end_column,
        2
    );
}

/// 使用 engine 提供的 cell span 而非猜测字体宽度，并保留关键词当前主题颜色。
///
/// @author kongweiguang
#[test]
fn overlay_uses_engine_cell_mapping_and_resolved_keyword_colors() {
    let style = TerminalOverlayStyle {
        foreground: Some(TerminalRgb::new(0x11, 0x22, 0x33)),
        background: Some(TerminalRgb::new(0x44, 0x55, 0x66)),
    };
    let overlay = TerminalCellOverlay {
        range: TerminalPoint { line: 9, column: 3 }..TerminalPoint { line: 9, column: 4 },
        kind: TerminalOverlayKind::Keyword,
        style: Some(style),
    };
    let spans = spans_for_row(&row(9, 0), 8, None, &[overlay]);
    let cell_spans = [TerminalTextCellSpan {
        byte_range: 0.."中".len(),
        column_range: 2..4,
    }];

    assert_eq!(
        text_overlay_segments("中", &cell_spans, 0, 8, &spans),
        vec![CellTextSegment {
            byte_range: 0.."中".len(),
            column_range: 2..4,
            visible_columns: 2..4,
            paint: Some(CellOverlayPaint {
                kind: TerminalOverlayKind::Keyword,
                style: Some(style),
            }),
        }]
    );
}

/// 覆盖命中宽字符尾列时背景和文本都扩展至 Alacritty 提供的完整列范围。
///
/// @author kongweiguang
#[test]
fn overlay_on_wide_grapheme_trailing_cell_expands_to_full_cell_span() {
    let overlay = TerminalCellOverlay {
        range: TerminalPoint { line: 9, column: 2 }..TerminalPoint { line: 9, column: 3 },
        kind: TerminalOverlayKind::Search,
        style: None,
    };

    assert_eq!(
        spans_for_row(&row_with_wide_grapheme(9, 0), 8, None, &[overlay]),
        vec![CellOverlaySpan {
            start_column: 1,
            end_column: 3,
            kind: TerminalOverlayKind::Search,
            style: None,
        }]
    );
}

/// 即使宽字符只剩末列可见，也从原始 glyph 起点绘制并把可见绘制区裁到窗口内。
///
/// @author kongweiguang
#[test]
fn wide_grapheme_keeps_original_cell_origin_when_partially_clipped() {
    let segment = text_overlay_segments(
        "中",
        &[TerminalTextCellSpan {
            byte_range: 0.."中".len(),
            column_range: 1..3,
        }],
        0,
        2,
        &[],
    );

    assert_eq!(
        segment,
        vec![CellTextSegment {
            byte_range: 0.."中".len(),
            column_range: 1..3,
            visible_columns: 1..2,
            paint: None,
        }]
    );
}
