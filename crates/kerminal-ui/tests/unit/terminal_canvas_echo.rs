//! Canvas 单字符验收只接受快照 cell span 与成功绘制列一致的 `x`。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_terminal::snapshot::{
    TerminalAttributes, TerminalColor, TerminalRow, TerminalRun, TerminalStyle,
    TerminalTextCellSpan,
};

use super::{
    TerminalPaintEchoCandidate, echo_target_for_painted_columns, snapshot_cell_is_probe_character,
};
use crate::acceptance_metrics::TerminalEchoTarget;

/// 为候选 probe 构造一个含真实字节到 cell 映射的行，不依赖字体 shaping 或原生窗口。
///
/// @author kongweiguang
fn row_with_cell_spans(
    text: &str,
    spans: impl IntoIterator<Item = (std::ops::Range<usize>, std::ops::Range<u16>)>,
    hidden: bool,
) -> TerminalRow {
    let spans = spans
        .into_iter()
        .map(|(byte_range, column_range)| TerminalTextCellSpan {
            byte_range,
            column_range,
        })
        .collect::<Vec<_>>();
    TerminalRow {
        viewport_row: 3,
        absolute_line: 3,
        wrapped: false,
        runs: Arc::from([TerminalRun {
            start_column: spans
                .iter()
                .map(|span| span.column_range.start)
                .min()
                .unwrap_or_default(),
            end_column: spans
                .iter()
                .map(|span| span.column_range.end)
                .max()
                .unwrap_or_default(),
            text: Arc::from(text),
            cell_spans: spans.into(),
            style: TerminalStyle {
                foreground: TerminalColor::DefaultForeground,
                background: TerminalColor::DefaultBackground,
                underline_color: None,
                attributes: TerminalAttributes {
                    hidden,
                    ..TerminalAttributes::default()
                },
            },
            hyperlink: None,
        }]),
    }
}

/// 中文前缀可与 probe 同属一 run；只有目标 cell 的 span 字节恰为 `x` 才算输入字符。
///
/// @author kongweiguang
#[test]
fn probe_cell_matches_ascii_x_after_wide_unicode_prefix() {
    let row = row_with_cell_spans("界x", [(0..3, 0..2), (3..4, 2..3)], false);

    assert!(snapshot_cell_is_probe_character(&row, 2));
    assert!(!snapshot_cell_is_probe_character(&row, 0));
    assert!(!snapshot_cell_is_probe_character(&row, 1));
}

/// 整段、多字节、隐藏或错列的 span 不能冒充单 ASCII 目标 cell。
///
/// @author kongweiguang
#[test]
fn probe_cell_rejects_non_exact_or_hidden_spans() {
    let combined_bytes = row_with_cell_spans("xy", [(0..2, 0..1)], false);
    let wrong_column = row_with_cell_spans("x", [(0..1, 0..1)], false);
    let hidden = row_with_cell_spans("x", [(0..1, 2..3)], true);

    assert!(!snapshot_cell_is_probe_character(&combined_bytes, 0));
    assert!(!snapshot_cell_is_probe_character(&wrong_column, 1));
    assert!(!snapshot_cell_is_probe_character(&hidden, 2));
    assert!(!snapshot_cell_is_probe_character(&hidden, u16::MAX));
}

/// 只把目标行列所覆盖的 terminal text run 关联到 probe，边界列和其他行均拒绝。
///
/// @author kongweiguang
#[test]
fn paint_run_must_cover_exact_target_row_and_column() {
    let candidate = Some(TerminalPaintEchoCandidate {
        target: TerminalEchoTarget {
            viewport_row: 3,
            column: 2,
        },
        cursor_visible: true,
        cursor_row: 3,
        cursor_column: 3,
        at_bottom: true,
    });

    assert_eq!(
        echo_target_for_painted_columns(candidate, 3, &(1..4)),
        candidate.map(|candidate| candidate.target)
    );
    assert_eq!(echo_target_for_painted_columns(candidate, 3, &(0..2)), None);
    assert_eq!(echo_target_for_painted_columns(candidate, 4, &(0..4)), None);
}
