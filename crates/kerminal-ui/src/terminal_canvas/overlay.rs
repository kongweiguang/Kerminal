//! 把绝对终端坐标映射成 grapheme 对齐的绘制区间。
//!
//! @author kongweiguang

use std::ops::Range;

use kerminal_terminal::snapshot::{
    TerminalPoint, TerminalRgb, TerminalRow, TerminalSelection, TerminalSelectionKind,
    TerminalTextCellSpan,
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// 屏幕覆盖层以终端绝对行和 cell 列表达，结束点使用半开区间。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TerminalCellOverlay {
    pub range: Range<TerminalPoint>,
    pub kind: TerminalOverlayKind,
    pub style: Option<TerminalOverlayStyle>,
}

/// 搜索或关键词覆盖层的可选颜色；值已在 UI 边界解析为当前主题下的真实 RGB。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TerminalOverlayStyle {
    pub foreground: Option<TerminalRgb>,
    pub background: Option<TerminalRgb>,
}

/// 高亮类型只描述展示优先级，颜色由 Canvas 使用当前终端 palette 决定。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum TerminalOverlayKind {
    Keyword,
    Search,
    ActiveSearch,
    Selection,
}

/// 某一 viewport row 上的半开 cell 区间，用于把异步搜索和选择映射到可见网格。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CellOverlaySpan {
    pub start_column: u16,
    pub end_column: u16,
    pub kind: TerminalOverlayKind,
    pub style: Option<TerminalOverlayStyle>,
}

/// 已解析的文本覆盖层外观，用于合并相邻且最终绘制效果相同的 grapheme。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CellOverlayPaint {
    pub kind: TerminalOverlayKind,
    pub style: Option<TerminalOverlayStyle>,
}

/// 可选 RGB 维持 None 优先及通道词典序，类型别名不改变覆盖排序或产生新分配。
type OverlayColorSortKey = Option<(u8, u8, u8)>;

/// 一个完整 grapheme 对应的原始 cell 范围和当前可见范围；渲染从原始起点定位并按可见区裁剪。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CellTextSegment {
    pub byte_range: Range<usize>,
    pub column_range: Range<u16>,
    pub visible_columns: Range<u16>,
    pub paint: Option<CellOverlayPaint>,
}

/// 搜索范围是 engine 某个 revision 的绝对坐标，快照前进后必须隐藏旧投影并等待新查询。
///
/// @author kongweiguang
pub(crate) fn search_result_revision_matches(
    result_revision: Option<u64>,
    snapshot_revision: u64,
) -> bool {
    result_revision == Some(snapshot_revision)
}

/// 将选择与外部搜索/关键词投影裁成当前行列范围，避免滚动缓存外的坐标进入绘制阶段。
///
/// @author kongweiguang
pub(crate) fn spans_for_row(
    row: &TerminalRow,
    columns: u16,
    selection: Option<TerminalSelection>,
    overlays: &[TerminalCellOverlay],
) -> Vec<CellOverlaySpan> {
    if columns == 0 {
        return Vec::new();
    }

    let mut spans = Vec::new();
    if let Some(selection) = selection
        && let Some((start_column, end_column)) =
            selection_span(selection, row.absolute_line, columns)
    {
        spans.push(CellOverlaySpan {
            start_column,
            end_column,
            kind: TerminalOverlayKind::Selection,
            style: None,
        });
    }

    spans.extend(overlays.iter().filter_map(|overlay| {
        range_span(&overlay.range, row.absolute_line, columns).map(|(start_column, end_column)| {
            CellOverlaySpan {
                start_column,
                end_column,
                kind: overlay.kind,
                style: overlay.style,
            }
        })
    }));
    expand_spans_to_cell_boundaries(&mut spans, row);
    for span in &mut spans {
        span.start_column = span.start_column.min(columns);
        span.end_column = span.end_column.min(columns);
    }
    spans.retain(|span| span.start_column < span.end_column);
    spans.sort_unstable_by_key(|span| {
        (
            span.start_column,
            span.end_column,
            span.kind,
            style_sort_key(span.style),
        )
    });
    spans
}

/// 任一覆盖命中宽字符的部分列时扩展到完整 glyph，避免绘制半个字符背景或反色文本。
///
/// @author kongweiguang
fn expand_spans_to_cell_boundaries(spans: &mut [CellOverlaySpan], row: &TerminalRow) {
    for span in spans {
        for run in row.runs.iter() {
            for cell in run.cell_spans.iter() {
                if cell.column_range.start < span.end_column
                    && span.start_column < cell.column_range.end
                {
                    span.start_column = span.start_column.min(cell.column_range.start);
                    span.end_column = span.end_column.max(cell.column_range.end);
                }
            }
        }
    }
}

/// 把一段终端文本按 grapheme 和 overlay 优先级切分，宽字符命中会扩展到完整 cell 区间。
///
/// @author kongweiguang
pub(crate) fn text_overlay_segments(
    text: &str,
    cell_spans: &[TerminalTextCellSpan],
    start_column: u16,
    end_column: u16,
    spans: &[CellOverlaySpan],
) -> Vec<CellTextSegment> {
    if text.is_empty() || end_column <= start_column {
        return Vec::new();
    }

    let mut segments = Vec::new();
    let mut next_span = 0;
    let mut next_cell_span = 0;
    let mut next_column = start_column;
    let mut active_spans = Vec::new();
    for (byte_start, grapheme) in text.grapheme_indices(true) {
        let byte_end = byte_start + grapheme.len();
        let (column_start, column_end) = grapheme_columns(
            byte_start..byte_end,
            grapheme,
            cell_spans,
            &mut next_cell_span,
            next_column,
        );
        next_column = next_column.max(column_end);
        let visible_start = column_start.max(start_column);
        let visible_end = column_end.min(end_column);
        if visible_end <= visible_start {
            continue;
        }

        active_spans.retain(|span: &CellOverlaySpan| span.end_column > visible_start);
        while let Some(span) = spans
            .get(next_span)
            .filter(|span| span.start_column < visible_end)
        {
            if span.end_column > visible_start {
                active_spans.push(*span);
            }
            next_span += 1;
        }
        let paint = active_spans
            .iter()
            .filter(|span| span.start_column < visible_end && visible_start < span.end_column)
            .max_by_key(|span| (span.kind, style_sort_key(span.style)))
            .map(|span| CellOverlayPaint {
                kind: span.kind,
                style: span.style,
            });
        segments.push(CellTextSegment {
            byte_range: byte_start..byte_end,
            column_range: column_start..column_end,
            visible_columns: visible_start..visible_end,
            paint,
        });
    }
    segments
}

/// 优先使用引擎给出的 UTF-8 到 cell 映射；仅对缺失映射的退化快照按 grapheme 宽度兜底。
///
/// @author kongweiguang
fn grapheme_columns(
    byte_range: Range<usize>,
    grapheme: &str,
    cell_spans: &[TerminalTextCellSpan],
    next_cell_span: &mut usize,
    start_column: u16,
) -> (u16, u16) {
    while cell_spans
        .get(*next_cell_span)
        .is_some_and(|span| span.byte_range.end <= byte_range.start)
    {
        *next_cell_span += 1;
    }
    let mut mapped: Option<(u16, u16)> = None;
    while let Some(span) = cell_spans
        .get(*next_cell_span)
        .filter(|span| span.byte_range.start < byte_range.end)
    {
        mapped = Some(match mapped {
            Some((start, end)) => (
                start.min(span.column_range.start),
                end.max(span.column_range.end),
            ),
            None => (span.column_range.start, span.column_range.end),
        });
        *next_cell_span += 1;
    }
    mapped.unwrap_or_else(|| {
        let width = UnicodeWidthStr::width(grapheme)
            .max(1)
            .min(usize::from(u16::MAX)) as u16;
        (start_column, start_column.saturating_add(width))
    })
}

/// 用可选 RGB 的原词典序为同优先级覆盖层排序，避免重叠关键词随输入顺序闪烁换色。
///
/// @author kongweiguang
pub(super) fn style_sort_key(
    style: Option<TerminalOverlayStyle>,
) -> (OverlayColorSortKey, OverlayColorSortKey) {
    let color_key = |color: Option<TerminalRgb>| color.map(|rgb| (rgb.red, rgb.green, rgb.blue));
    style.map_or((None, None), |style| {
        (color_key(style.foreground), color_key(style.background))
    })
}

/// 把绝对行区间裁成单行的半开列范围；行外区间不会产生绘制对象。
///
/// @author kongweiguang
fn range_span(range: &Range<TerminalPoint>, line: i64, columns: u16) -> Option<(u16, u16)> {
    if line < range.start.line || line > range.end.line || range.start.line > range.end.line {
        return None;
    }
    let start = if line == range.start.line {
        range.start.column.min(columns)
    } else {
        0
    };
    let end = if line == range.end.line {
        range.end.column.min(columns)
    } else {
        columns
    };
    (start < end).then_some((start, end))
}

/// 将 Alacritty 的起止方向选择转成可见半开列范围，保留按行、字符和块选择的差异。
///
/// @author kongweiguang
fn selection_span(selection: TerminalSelection, line: i64, columns: u16) -> Option<(u16, u16)> {
    let (start, end) = if point_order(selection.start, selection.end).is_gt() {
        (selection.end, selection.start)
    } else {
        (selection.start, selection.end)
    };
    if line < start.line || line > end.line {
        return None;
    }

    match selection.kind {
        TerminalSelectionKind::Line => Some((0, columns)),
        TerminalSelectionKind::Block => inclusive_columns(start.column, end.column, columns),
        TerminalSelectionKind::Character | TerminalSelectionKind::Word => {
            let first = if line == start.line { start.column } else { 0 };
            let last_inclusive = if line == end.line {
                end.column
            } else {
                columns.saturating_sub(1)
            };
            inclusive_columns(first, last_inclusive, columns)
        }
    }
}

/// 把包含端点的文本选区转换成有界半开列范围。
///
/// @author kongweiguang
fn inclusive_columns(start: u16, end: u16, columns: u16) -> Option<(u16, u16)> {
    if columns == 0 {
        return None;
    }
    let start = start.min(columns.saturating_sub(1));
    let end = end.min(columns.saturating_sub(1));
    let (start, end) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    Some((start, end.saturating_add(1).min(columns)))
}

/// 用稳定的行列顺序归一化用户反向拖拽形成的选择端点。
///
/// @author kongweiguang
fn point_order(left: TerminalPoint, right: TerminalPoint) -> std::cmp::Ordering {
    (left.line, left.column).cmp(&(right.line, right.column))
}
