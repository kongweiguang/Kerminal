//! Canvas 分屏候选的纯几何门禁回归，不启动 GPUI 窗口或终端资源。
//!
//! @author kongweiguang

use kerminal_domain::workspace::SplitDirection;

use super::{
    SPLIT_PROBE_RETRY_FRAME_LIMIT, SplitProbeRect, SplitResizerGeometrySnapshot,
    clear_split_probe_failure, should_request_split_probe_retry, split_candidate_geometry_failure,
};

/// 测试用布尔适配器复用生产阶段门禁，不在 crate::src 保留 test-only symbol。
///
/// @author kongweiguang
fn split_candidate_geometry_is_stable(
    canvas_bounds: SplitProbeRect,
    clip_bounds: SplitProbeRect,
    path: &[SplitResizerGeometrySnapshot],
) -> bool {
    split_candidate_geometry_failure(canvas_bounds, clip_bounds, path).is_none()
}

/// 有限重试预算按操作和布局代次隔离，避免 Root notify 无限刷帧或继承旧布局预算。
///
/// @author kongweiguang
#[test]
fn split_probe_retry_budget_is_bounded_per_layout_generation() {
    let operation_id = "unit-test-split-probe-retry-budget";
    let layout_generation = u64::MAX - 1;

    clear_split_probe_failure(operation_id, layout_generation);
    clear_split_probe_failure(operation_id, layout_generation + 1);
    for _ in 0..SPLIT_PROBE_RETRY_FRAME_LIMIT {
        assert!(should_request_split_probe_retry(
            operation_id,
            layout_generation
        ));
    }
    assert!(!should_request_split_probe_retry(
        operation_id,
        layout_generation
    ));
    assert!(should_request_split_probe_retry(
        operation_id,
        layout_generation + 1
    ));

    clear_split_probe_failure(operation_id, layout_generation);
    clear_split_probe_failure(operation_id, layout_generation + 1);
}

/// 创建纯数值矩形，避免几何门禁测试依赖窗口或设备状态。
///
/// @author kongweiguang
fn rect(x: f64, y: f64, width: f64, height: f64) -> SplitProbeRect {
    SplitProbeRect {
        x,
        y,
        width,
        height,
    }
}

/// 生成完整的单层横向 split 样本，供失败场景只替换对应几何字段。
///
/// @author kongweiguang
fn horizontal_sample() -> SplitResizerGeometrySnapshot {
    SplitResizerGeometrySnapshot {
        split_id: "split-root".to_owned(),
        direction: SplitDirection::Horizontal,
        child_index: 1,
        group_bounds: Some(rect(0.0, 0.0, 200.0, 120.0)),
        container_extent: Some(200.0),
        child_bounds: vec![
            Some(rect(0.0, 0.0, 100.0, 120.0)),
            Some(rect(100.0, 0.0, 100.0, 120.0)),
        ],
        state_sizes: vec![100.0, 100.0],
    }
}

/// 缺路径、缺 group 回执时拒绝一次性准入，让下一帧重新采样。
///
/// @author kongweiguang
#[test]
fn missing_path_or_group_sample_is_not_admitted() {
    let canvas = rect(100.0, 0.0, 100.0, 120.0);
    let clip = rect(0.0, 0.0, 300.0, 300.0);
    assert!(!split_candidate_geometry_is_stable(canvas, clip, &[]));

    let mut sample = horizontal_sample();
    sample.group_bounds = None;
    assert!(!split_candidate_geometry_is_stable(canvas, clip, &[sample]));

    let mut zero_container = horizontal_sample();
    zero_container.container_extent = Some(0.0);
    assert!(!split_candidate_geometry_is_stable(
        canvas,
        clip,
        &[zero_container]
    ));
}

/// 与 ResizableState 最小值不符的旧 slot 尺寸必须继续等待。
///
/// @author kongweiguang
#[test]
fn stale_panel_extent_is_rejected() {
    let mut sample = horizontal_sample();
    sample.child_bounds[1] = Some(rect(100.0, 0.0, 94.0, 120.0));
    assert!(!split_candidate_geometry_is_stable(
        rect(100.0, 0.0, 94.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[sample],
    ));
}

/// 递归候选只有在每层祖先 slot 与下一层 group 连续时才通过。
///
/// @author kongweiguang
#[test]
fn nested_ancestor_path_is_admitted_when_each_level_matches() {
    let root = SplitResizerGeometrySnapshot {
        split_id: "split-root".to_owned(),
        direction: SplitDirection::Horizontal,
        child_index: 1,
        group_bounds: Some(rect(0.0, 0.0, 300.0, 240.0)),
        container_extent: Some(300.0),
        child_bounds: vec![
            Some(rect(0.0, 0.0, 150.0, 240.0)),
            Some(rect(150.0, 0.0, 150.0, 240.0)),
        ],
        state_sizes: vec![150.0, 150.0],
    };
    let nested = SplitResizerGeometrySnapshot {
        split_id: "split-nested".to_owned(),
        direction: SplitDirection::Vertical,
        child_index: 1,
        group_bounds: Some(rect(150.0, 0.0, 150.0, 240.0)),
        container_extent: Some(240.0),
        child_bounds: vec![
            Some(rect(150.0, 0.0, 150.0, 120.0)),
            Some(rect(150.0, 120.0, 150.0, 120.0)),
        ],
        state_sizes: vec![120.0, 120.0],
    };

    assert!(split_candidate_geometry_is_stable(
        rect(150.0, 120.0, 150.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[root, nested],
    ));
}

/// 多层 pane 在 Windows 高 DPI 布局中的亚像素边界误差不应使候选永久等待。
///
/// @author kongweiguang
#[test]
fn nested_fractional_bounds_within_tolerance_are_admitted() {
    let root = SplitResizerGeometrySnapshot {
        split_id: "split-root".to_owned(),
        direction: SplitDirection::Horizontal,
        child_index: 1,
        group_bounds: Some(rect(0.0, 0.0, 300.0, 240.0)),
        container_extent: Some(300.0),
        child_bounds: vec![
            Some(rect(0.0, 0.0, 150.0, 240.0)),
            Some(rect(150.0, 0.0, 150.0, 240.0)),
        ],
        state_sizes: vec![150.0, 150.0],
    };
    let nested = SplitResizerGeometrySnapshot {
        split_id: "split-nested".to_owned(),
        direction: SplitDirection::Vertical,
        child_index: 1,
        group_bounds: Some(rect(150.5, 0.5, 149.0, 240.0)),
        container_extent: Some(240.0),
        child_bounds: vec![
            Some(rect(150.5, 0.5, 149.0, 120.0)),
            Some(rect(150.5, 120.5, 149.0, 120.0)),
        ],
        state_sizes: vec![120.0, 120.0],
    };

    assert!(split_candidate_geometry_is_stable(
        rect(150.5, 120.5, 149.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[root, nested],
    ));
}

/// 窄容器下超出 overflow clip 的 Canvas 网格须拒绝，避免用被裁掉的尺寸启动终端。
///
/// @author kongweiguang
#[test]
fn narrow_container_rejects_canvas_clipped_on_either_axis() {
    let mut sample = horizontal_sample();
    sample.group_bounds = Some(rect(0.0, 0.0, 150.0, 120.0));
    sample.container_extent = Some(150.0);
    sample.child_bounds = vec![
        Some(rect(0.0, 0.0, 100.0, 120.0)),
        Some(rect(100.0, 0.0, 100.0, 120.0)),
    ];
    sample.state_sizes = vec![72.0, 78.0];

    let canvas = rect(100.0, 0.0, 100.0, 120.0);
    assert!(!split_candidate_geometry_is_stable(
        canvas,
        rect(0.0, 0.0, 150.0, 120.0),
        std::slice::from_ref(&sample),
    ));
    assert!(!split_candidate_geometry_is_stable(
        canvas,
        rect(100.0, 0.0, 100.0, 60.0),
        &[sample],
    ));
}

/// ResizableState 低于 Kit 下限时仍按实际 100px slot 接受完整可见的 Canvas 网格。
///
/// @author kongweiguang
#[test]
fn small_state_accepts_canvas_when_its_full_grid_is_visible() {
    let mut sample = horizontal_sample();
    sample.group_bounds = Some(rect(0.0, 0.0, 150.0, 120.0));
    sample.container_extent = Some(150.0);
    sample.child_bounds = vec![
        Some(rect(0.0, 0.0, 100.0, 120.0)),
        Some(rect(100.0, 0.0, 100.0, 120.0)),
    ];
    sample.state_sizes = vec![72.0, 78.0];
    sample.child_index = 0;

    assert!(split_candidate_geometry_is_stable(
        rect(0.0, 0.0, 100.0, 120.0),
        rect(0.0, 0.0, 150.0, 120.0),
        &[sample],
    ));
}

/// 缺任一祖先直系 slot 时不把不完整路径当作真实 Canvas 网格。
///
/// @author kongweiguang
#[test]
fn missing_child_slot_is_rejected() {
    let mut sample = horizontal_sample();
    sample.child_bounds[0] = None;
    assert!(!split_candidate_geometry_is_stable(
        rect(100.0, 0.0, 100.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[sample],
    ));
}

/// 只接受落入叶 slot 的真实 Canvas bounds，并由现有尺寸函数按字体和 DPI 计算网格。
///
/// @author kongweiguang
#[test]
fn leaf_slot_bounds_and_canvas_grid_are_used() {
    let sample = horizontal_sample();
    let canvas = rect(100.0, 0.0, 100.0, 120.0);
    assert!(split_candidate_geometry_is_stable(
        canvas,
        rect(0.0, 0.0, 300.0, 300.0),
        std::slice::from_ref(&sample)
    ));
    assert!(!split_candidate_geometry_is_stable(
        rect(199.0, 0.0, 100.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[sample],
    ));
    assert!(split_candidate_geometry_is_stable(
        rect(100.5, 0.0, 100.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[horizontal_sample()],
    ));
    assert!(!split_candidate_geometry_is_stable(
        rect(101.25, 0.0, 100.0, 120.0),
        rect(0.0, 0.0, 300.0, 300.0),
        &[horizontal_sample()],
    ));

    let size = super::geometry::terminal_size_for_canvas_dimensions(
        canvas.width as f32,
        canvas.height as f32,
        8.0,
        16.0,
        2.0,
        1.5,
    )
    .expect("有效 Canvas bounds 可计算终端网格");
    assert_eq!(size.columns(), 12);
    assert_eq!(size.rows(), 7);
    assert_eq!(size.pixel_width(), 144);
    assert_eq!(size.pixel_height(), 174);
}
