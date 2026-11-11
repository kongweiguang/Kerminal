//! 把逐行选区区间合并成连续、低圆角的填充轮廓。
//!
//! 选区按连通区域一次填充实色，而不是逐 cell 或逐行画矩形：逐行矩形会在列边界留下
//! 细缝，并在反向或宽字符选择时放大相邻矩形之间的颜色断层。只有相邻行且像素区间正
//! 重叠时才归为同一区域，非重叠行不桥接，避免把未选中的列也涂上颜色。轮廓转折使用
//! 小半径二次曲线，半径夹紧到相邻边的一半，既不会越过窄 cell，也不会在视口裁切边
//! 长出被裁掉一半的圆头。
//!
//! @author kongweiguang

use gpui_kit::{Bounds, Path, PathBuilder, Pixels, point, px};

/// 选区转弯的基准半径；小圆角只用来柔化阶梯外轮廓，不改变 cell 的覆盖范围。
const SELECTION_CORNER_RADIUS: f32 = 2.0;
/// 相邻行需要超过半像素的正重叠才属于同一连通区域，避免亚像素接缝被误判为连通。
const MIN_CONNECTED_OVERLAP: f32 = 0.5;
/// 浮点坐标比较容差；几何输入来自整数 cell 网格，误差只应出现在乘法舍入上。
const EDGE_EPSILON: f32 = 0.01;

/// 某一 viewport row 上的半开列区间；Canvas 已用 spans_for_row 完成宽字符扩展和范围归一。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SelectionRowSpan {
    pub(super) viewport_row: u16,
    pub(super) start_column: u16,
    pub(super) end_column: u16,
}

/// 已裁剪到内容边界的单行选区矩形，坐标以 Canvas 像素表示，供轮廓与测试共用。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SelectionBand {
    pub(super) viewport_row: u16,
    pub(super) x0: f32,
    pub(super) x1: f32,
    pub(super) y0: f32,
    pub(super) y1: f32,
}

/// 轮廓转折点；落在 inset 内容边上的顶点保持直角，其余顶点允许小圆角。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct OutlineVertex {
    pub(super) x: f32,
    pub(super) y: f32,
    pub(super) on_boundary: bool,
}

/// 为每个连通选区生成一条实色填充路径，调用方只负责按顺序绘制。
///
/// @author kongweiguang
pub(super) fn selection_paths(
    spans: &[SelectionRowSpan],
    canvas_bounds: Bounds<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    inset: Pixels,
) -> Vec<Path<Pixels>> {
    let left = canvas_bounds.origin.x.as_f32() + inset.as_f32();
    let top = canvas_bounds.origin.y.as_f32() + inset.as_f32();
    let right =
        canvas_bounds.origin.x.as_f32() + canvas_bounds.size.width.as_f32() - inset.as_f32();
    let bottom =
        canvas_bounds.origin.y.as_f32() + canvas_bounds.size.height.as_f32() - inset.as_f32();
    if right <= left || bottom <= top {
        return Vec::new();
    }

    let cell_width = cell_width.as_f32();
    let line_height = line_height.as_f32();
    let mut paths = Vec::new();
    for region in selection_regions(spans, left, cell_width, right) {
        let bands = selection_bands(&region, left, right, top, bottom, cell_width, line_height);
        if bands.is_empty() {
            continue;
        }
        let vertices = outline_vertices(&bands, left, right, top, bottom);
        if vertices.len() < 3 {
            continue;
        }
        // 单个区域构建失败时只跳过这一块选区；终端正文与其他运行态仍应继续绘制。
        match build_outline_path(&vertices) {
            Ok(path) => paths.push(path),
            Err(error) => {
                tracing::warn!(error = %error, "terminal selection outline build failed");
            }
        }
    }
    paths
}

/// 把行区间按“相邻行且像素区间正重叠”合为连通区域，排序后重复行只保留最后一条。
///
/// 只与上一个区域比较，而不回看更早的连通块：两行之间没有重叠时，即使隔行区间再次
/// 相交也不桥接，否则会造成选中文字不连续却出现一整块背景的错觉。
///
/// @author kongweiguang
pub(super) fn selection_regions(
    spans: &[SelectionRowSpan],
    left: f32,
    cell_width: f32,
    right: f32,
) -> Vec<Vec<SelectionRowSpan>> {
    if cell_width <= 0.0 || right <= left {
        return Vec::new();
    }

    let mut rows: Vec<SelectionRowSpan> = spans
        .iter()
        .copied()
        .filter(|span| span.start_column < span.end_column)
        .collect();
    rows.sort_unstable_by_key(|span| (span.viewport_row, span.start_column, span.end_column));

    // 正常快照每行最多一个 Selection span；输入异常重复时按排序后的最后一条去重，
    // 结果与行内顺序无关，也不会把两段不相交的区间合并成跨列背景。
    let mut unique: Vec<SelectionRowSpan> = Vec::with_capacity(rows.len());
    for span in rows {
        match unique.last_mut() {
            Some(last) if last.viewport_row == span.viewport_row => *last = span,
            _ => unique.push(span),
        }
    }

    let mut regions: Vec<Vec<SelectionRowSpan>> = Vec::new();
    for span in unique {
        let Some((x0, x1)) = column_interval(span, left, cell_width, right) else {
            continue;
        };
        if let Some(region) = regions.last_mut()
            && let Some(previous) = region.last()
            && previous.viewport_row.checked_add(1) == Some(span.viewport_row)
            && let Some((previous_x0, previous_x1)) =
                column_interval(*previous, left, cell_width, right)
            && (x1.min(previous_x1) - x0.max(previous_x0)) > MIN_CONNECTED_OVERLAP
        {
            region.push(span);
            continue;
        }
        regions.push(vec![span]);
    }
    regions
}

/// 将一段列区间换算成可绘制像素宽度，并夹到内容右边界；负宽度直接丢弃。
///
/// @author kongweiguang
fn column_interval(
    span: SelectionRowSpan,
    left: f32,
    cell_width: f32,
    right: f32,
) -> Option<(f32, f32)> {
    let start = left + cell_width * f32::from(span.start_column);
    let end = (left + cell_width * f32::from(span.end_column)).min(right);
    let start = start.max(left);
    (end - start > EDGE_EPSILON).then_some((start, end))
}

/// 把连通区域投影为逐行像素矩形；顶部/底部被视口裁切的空白行直接丢弃。
///
/// @author kongweiguang
pub(super) fn selection_bands(
    region: &[SelectionRowSpan],
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
    cell_width: f32,
    line_height: f32,
) -> Vec<SelectionBand> {
    region
        .iter()
        .filter_map(|span| {
            let (x0, x1) = column_interval(*span, left, cell_width, right)?;
            let y0 = (top + line_height * f32::from(span.viewport_row)).max(top);
            let y1 = (y0 + line_height).min(bottom);
            (y1 - y0 > EDGE_EPSILON).then_some(SelectionBand {
                viewport_row: span.viewport_row,
                x0,
                x1,
                y0,
                y1,
            })
        })
        .collect()
}

/// 生成顺时针外轮廓：顶边向右、右侧阶梯向下、底边向左、左侧阶梯向上。
///
/// 同一列连续的多行只在真正改变 x 的位置放转折点，避免在直边中间堆积无意义顶点。
///
/// @author kongweiguang
pub(super) fn outline_vertices(
    bands: &[SelectionBand],
    left: f32,
    right: f32,
    top: f32,
    bottom: f32,
) -> Vec<OutlineVertex> {
    if bands.is_empty() {
        return Vec::new();
    }

    let mut points: Vec<(f32, f32)> = Vec::with_capacity(bands.len() * 2 + 4);
    let first = bands[0];
    points.push((first.x1, first.y0));
    for pair in bands.windows(2) {
        let (above, below) = (pair[0], pair[1]);
        if (below.x1 - above.x1).abs() > EDGE_EPSILON {
            points.push((above.x1, below.y0));
            points.push((below.x1, below.y0));
        }
    }
    let last = bands[bands.len() - 1];
    points.push((last.x1, last.y1));
    points.push((last.x0, last.y1));
    for index in (1..bands.len()).rev() {
        let (above, below) = (bands[index - 1], bands[index]);
        if (below.x0 - above.x0).abs() > EDGE_EPSILON {
            points.push((below.x0, below.y0));
            points.push((above.x0, below.y0));
        }
    }
    points.push((first.x0, first.y0));

    let mut vertices: Vec<OutlineVertex> = Vec::with_capacity(points.len());
    for (x, y) in points {
        if let Some(previous) = vertices.last()
            && (previous.x - x).abs() <= EDGE_EPSILON
            && (previous.y - y).abs() <= EDGE_EPSILON
        {
            continue;
        }
        vertices.push(OutlineVertex {
            x,
            y,
            // 视口内容边界上的转折保持直角，防止裁切后只留下半个圆角。
            on_boundary: x <= left + EDGE_EPSILON
                || x >= right - EDGE_EPSILON
                || y <= top + EDGE_EPSILON
                || y >= bottom - EDGE_EPSILON,
        });
    }
    vertices
}

/// 计算单个转折可用的圆角半径，夹紧到相邻两段长度的一半，保证相邻圆角互不重叠。
///
/// @author kongweiguang
pub(super) fn corner_rounding_radius(
    previous: (f32, f32),
    current: (f32, f32),
    next: (f32, f32),
    base: f32,
) -> f32 {
    let incoming = distance(previous, current);
    let outgoing = distance(current, next);
    if incoming <= EDGE_EPSILON || outgoing <= EDGE_EPSILON {
        return 0.0;
    }
    base.min(incoming * 0.5).min(outgoing * 0.5).max(0.0)
}

/// 二次曲线的起点、控制点和终点；单独命名以明确轮廓构建阶段的坐标含义。
type RoundedCorner = ((f32, f32), (f32, f32), (f32, f32));

/// 用二次曲线把尖角替换成小圆角，返回线段起点、控制点和终点。
///
/// @author kongweiguang
fn rounded_corner(
    previous: &OutlineVertex,
    current: &OutlineVertex,
    next: &OutlineVertex,
) -> Option<RoundedCorner> {
    if current.on_boundary {
        return None;
    }
    let previous_point = (previous.x, previous.y);
    let current_point = (current.x, current.y);
    let next_point = (next.x, next.y);
    let radius = corner_rounding_radius(
        previous_point,
        current_point,
        next_point,
        SELECTION_CORNER_RADIUS,
    );
    if radius <= EDGE_EPSILON {
        return None;
    }
    let incoming = distance(previous_point, current_point);
    let outgoing = distance(current_point, next_point);
    let start = (
        current.x + (previous.x - current.x) * (radius / incoming),
        current.y + (previous.y - current.y) * (radius / incoming),
    );
    let end = (
        current.x + (next.x - current.x) * (radius / outgoing),
        current.y + (next.y - current.y) * (radius / outgoing),
    );
    Some((start, current_point, end))
}

/// 把带直角标记的轮廓编译成一条填充路径；几何退化时返回错误交由调用方跳过。
///
/// @author kongweiguang
fn build_outline_path(vertices: &[OutlineVertex]) -> Result<Path<Pixels>, String> {
    if vertices.len() < 3 {
        return Err("selection outline needs at least three vertices".to_owned());
    }
    let count = vertices.len();
    let corner_at = |index: usize| {
        let previous = &vertices[(index + count - 1) % count];
        let current = &vertices[index];
        let next = &vertices[(index + 1) % count];
        rounded_corner(previous, current, next)
    };

    let mut builder = PathBuilder::fill();
    let start = corner_at(0).map_or((vertices[0].x, vertices[0].y), |corner| corner.0);
    builder.move_to(point(px(start.0), px(start.1)));
    for (index, vertex) in vertices.iter().enumerate() {
        match corner_at(index) {
            Some((corner_start, control, corner_end)) => {
                if index != 0 {
                    builder.line_to(point(px(corner_start.0), px(corner_start.1)));
                }
                builder.curve_to(
                    point(px(corner_end.0), px(corner_end.1)),
                    point(px(control.0), px(control.1)),
                );
            }
            None => {
                if index != 0 {
                    builder.line_to(point(px(vertex.x), px(vertex.y)));
                }
            }
        }
    }
    builder.close();
    builder.build().map_err(|error| error.to_string())
}

/// 平面两点距离；轮廓坐标均为像素值，不需要额外缩放。
///
/// @author kongweiguang
fn distance(left: (f32, f32), right: (f32, f32)) -> f32 {
    let dx = right.0 - left.0;
    let dy = right.1 - left.1;
    (dx * dx + dy * dy).sqrt()
}
