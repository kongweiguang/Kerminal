//! 选区连续轮廓的纯几何回归，不依赖字体、窗口或终端引擎。
//!
//! @author kongweiguang

use gpui_kit::{Bounds, Path, Pixels, bounds, point, px, size};

use super::selection_geometry::{
    OutlineVertex, SelectionRowSpan, corner_rounding_radius, outline_vertices, selection_bands,
    selection_paths, selection_regions,
};

/// 路径由 lyon 三角化后取边界，允许乘法舍入带来的极小坐标误差。
const TOLERANCE: f32 = 0.02;

/// 构造测试用 Canvas 内容区域。
///
/// @author kongweiguang
fn canvas(origin_x: f32, origin_y: f32, width: f32, height: f32) -> Bounds<Pixels> {
    bounds(
        point(px(origin_x), px(origin_y)),
        size(px(width), px(height)),
    )
}

/// 构造单行半开列区间。
///
/// @author kongweiguang
fn row_span(viewport_row: u16, start_column: u16, end_column: u16) -> SelectionRowSpan {
    SelectionRowSpan {
        viewport_row,
        start_column,
        end_column,
    }
}

/// 浮点近似比较集中在这里，避免每个断言重复容差逻辑。
///
/// @author kongweiguang
fn close(actual: f32, expected: f32) -> bool {
    (actual - expected).abs() <= TOLERANCE
}

/// 断言路径包围盒与期望的 Canvas 像素区域一致。
///
/// @author kongweiguang
fn assert_path_bounds(path: &Path<Pixels>, x: f32, y: f32, width: f32, height: f32) {
    let actual = path.bounds;
    assert!(
        close(actual.origin.x.as_f32(), x)
            && close(actual.origin.y.as_f32(), y)
            && close(actual.size.width.as_f32(), width)
            && close(actual.size.height.as_f32(), height),
        "expected ({x}, {y}, {width}, {height}), got ({}, {}, {}, {})",
        actual.origin.x.as_f32(),
        actual.origin.y.as_f32(),
        actual.size.width.as_f32(),
        actual.size.height.as_f32(),
    );
}

/// 轮廓辅助是否包含指定顶点；用于证明阶梯转折存在且没有跨列桥接。
///
/// @author kongweiguang
fn has_vertex(vertices: &[OutlineVertex], x: f32, y: f32) -> bool {
    vertices
        .iter()
        .any(|vertex| close(vertex.x, x) && close(vertex.y, y))
}

/// 单行选区只生成一条路径，包围盒严格贴合起始/结束列和行高。
///
/// @author kongweiguang
#[test]
fn single_row_selection_matches_cell_grid() {
    let paths = selection_paths(
        &[row_span(2, 3, 5)],
        canvas(10.0, 20.0, 100.0, 100.0),
        px(8.0),
        px(16.0),
        px(4.0),
    );

    assert_eq!(paths.len(), 1);
    // x = 10 + 4 + 8 * 3，y = 20 + 4 + 16 * 2；宽 2 列、高 1 行。
    assert_path_bounds(&paths[0], 38.0, 56.0, 16.0, 16.0);
}

/// 相邻两行按正重叠合并为一条阶梯轮廓，宽行不会把顶行未选列一起涂色。
///
/// @author kongweiguang
#[test]
fn adjacent_rows_form_one_staircase_without_bridging_unselected_columns() {
    let spans = [row_span(0, 0, 4), row_span(1, 0, 6)];
    let regions = selection_regions(&spans, 0.0, 10.0, 200.0);
    assert_eq!(regions.len(), 1);

    let bands = selection_bands(&regions[0], 0.0, 200.0, 0.0, 100.0, 10.0, 20.0);
    let vertices = outline_vertices(&bands, 0.0, 200.0, 0.0, 100.0);
    // 右侧在行交界处从第 4 列台阶到第 6 列，说明没有把整块矩形铺到最宽列。
    assert!(has_vertex(&vertices, 40.0, 20.0));
    assert!(has_vertex(&vertices, 60.0, 20.0));
    assert!(!has_vertex(&vertices, 60.0, 0.0));

    let paths = selection_paths(
        &spans,
        canvas(0.0, 0.0, 200.0, 100.0),
        px(10.0),
        px(20.0),
        px(0.0),
    );
    assert_eq!(paths.len(), 1);
    assert_path_bounds(&paths[0], 0.0, 0.0, 60.0, 40.0);
}

/// 相邻行但列区间不相交时必须分成两条路径，不能用一条斜边或整块背景桥接。
///
/// @author kongweiguang
#[test]
fn adjacent_rows_without_column_overlap_stay_separate() {
    let spans = [row_span(0, 30, 80), row_span(1, 0, 10)];
    let regions = selection_regions(&spans, 0.0, 5.0, 500.0);
    assert_eq!(regions.len(), 2);

    let paths = selection_paths(
        &spans,
        canvas(0.0, 0.0, 500.0, 100.0),
        px(5.0),
        px(20.0),
        px(0.0),
    );
    assert_eq!(paths.len(), 2);
    assert_path_bounds(&paths[0], 150.0, 0.0, 250.0, 20.0);
    assert_path_bounds(&paths[1], 0.0, 20.0, 50.0, 20.0);
}

/// 选区被视口四边裁切时，贴边角保持直角，路径仍覆盖完整内容边界且不产生越界行。
///
/// @author kongweiguang
#[test]
fn viewport_clipped_corners_stay_sharp_on_content_edges() {
    // 列区间 0..20 超出 10 列可见网格，行 5 已落到内容底部之外，均应由几何裁剪掉。
    let spans: Vec<SelectionRowSpan> = (0..=5).map(|row| row_span(row, 0, 20)).collect();
    let content = canvas(0.0, 0.0, 100.0, 50.0);
    let paths = selection_paths(&spans, content, px(10.0), px(10.0), px(0.0));
    assert_eq!(paths.len(), 1);
    assert_path_bounds(&paths[0], 0.0, 0.0, 100.0, 50.0);

    let regions = selection_regions(&spans, 0.0, 10.0, 100.0);
    assert_eq!(regions.len(), 1);
    let bands = selection_bands(&regions[0], 0.0, 100.0, 0.0, 50.0, 10.0, 10.0);
    let vertices = outline_vertices(&bands, 0.0, 100.0, 0.0, 50.0);

    assert_eq!(vertices.len(), 4);
    assert!(vertices.iter().all(|vertex| vertex.on_boundary));
    for corner in [(0.0, 0.0), (100.0, 0.0), (100.0, 50.0), (0.0, 50.0)] {
        assert!(
            has_vertex(&vertices, corner.0, corner.1),
            "missing sharp content-edge corner {corner:?}"
        );
    }
}

/// 极窄 cell 的圆角半径被夹到边长一半，路径仍为正尺寸且不越出 cell。
///
/// @author kongweiguang
#[test]
fn narrow_single_cell_clamps_corner_radius() {
    assert!(close(
        corner_rounding_radius((0.0, 0.0), (1.0, 0.0), (1.0, 1.0), 2.0),
        0.5
    ));

    let paths = selection_paths(
        &[row_span(0, 0, 1)],
        canvas(0.0, 0.0, 5.0, 5.0),
        px(1.0),
        px(1.0),
        px(0.0),
    );
    assert_eq!(paths.len(), 1);
    assert_path_bounds(&paths[0], 0.0, 0.0, 1.0, 1.0);

    let actual = paths[0].bounds;
    let left = actual.origin.x.as_f32();
    let top = actual.origin.y.as_f32();
    let right = left + actual.size.width.as_f32();
    let bottom = top + actual.size.height.as_f32();
    for vertex in &paths[0].vertices {
        let x = vertex.xy_position.x.as_f32();
        let y = vertex.xy_position.y.as_f32();
        assert!(x >= left - TOLERANCE);
        assert!(x <= right + TOLERANCE);
        assert!(y >= top - TOLERANCE);
        assert!(y <= bottom + TOLERANCE);
    }
}

/// 反向行输入先排序再合组；同一行重复时按排序后的最后一条去重，结果稳定且不跨列合并。
///
/// @author kongweiguang
#[test]
fn reversed_rows_sort_stably_and_duplicate_rows_keep_last_sorted_span() {
    let reversed = [row_span(1, 0, 4), row_span(0, 2, 6)];
    let regions = selection_regions(&reversed, 0.0, 10.0, 200.0);
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0][0].viewport_row, 0);
    assert_eq!(regions[0][1].viewport_row, 1);
    let paths = selection_paths(
        &reversed,
        canvas(0.0, 0.0, 200.0, 100.0),
        px(10.0),
        px(20.0),
        px(0.0),
    );
    assert_eq!(paths.len(), 1);
    assert_path_bounds(&paths[0], 0.0, 0.0, 60.0, 40.0);

    let duplicated = [row_span(0, 5, 7), row_span(0, 0, 2)];
    let regions = selection_regions(&duplicated, 0.0, 10.0, 200.0);
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].len(), 1);
    assert_eq!(regions[0][0].start_column, 5);
    assert_eq!(regions[0][0].end_column, 7);
}
