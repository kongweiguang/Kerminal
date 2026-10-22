//! 真实 Canvas 尺寸换算与 resize 绑定失效边界测试。
//!
//! @author kongweiguang

use super::*;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::snapshot::TerminalRow;
use std::sync::Arc;

/// 构造稀疏快照中的简单 viewport row，只验证共享几何而不重复终端文本映射。
///
/// @author kongweiguang
fn viewport_row(viewport_row: u16) -> TerminalRow {
    TerminalRow {
        viewport_row,
        absolute_line: 90 + i64::from(viewport_row),
        wrapped: false,
        runs: Arc::from([]),
    }
}

/// 用尺寸快照计算完整 cell 数，并保留传给 PTY 的真实可用像素面积。
///
/// @author kongweiguang
#[test]
fn canvas_bounds_produce_the_drawable_terminal_grid() {
    let size = terminal_size_for_canvas_dimensions(640.0, 360.0, 8.0, 18.0, 12.0, 1.0)
        .expect("可见终端画布应产生有效网格");

    assert_eq!(size.columns(), 77);
    assert_eq!(size.rows(), 18);
    assert_eq!(size.pixel_width(), 616);
    assert_eq!(size.pixel_height(), 336);
}

/// 不完整 cell 向下取整，避免分屏和字体度量的小数余量导致列行溢出。
///
/// @author kongweiguang
#[test]
fn partial_cells_are_excluded_from_the_grid() {
    let size = terminal_size_for_canvas_dimensions(644.0, 362.0, 8.5, 18.0, 12.0, 1.0)
        .expect("含完整 cell 的可见区域应有效");

    assert_eq!(size.columns(), 72);
    assert_eq!(size.rows(), 18);
    assert_eq!(size.pixel_width(), 620);
    assert_eq!(size.pixel_height(), 338);
}

/// 非整数 DPI 只缩放 PTY 像素面积；列行仍由逻辑像素和字体度量决定。
///
/// @author kongweiguang
#[test]
fn fractional_dpi_scales_drawable_pixels_without_changing_the_grid() {
    let size = terminal_size_for_canvas_dimensions(102.0, 62.0, 8.5, 18.0, 12.0, 1.25)
        .expect("非整数 DPI 下的完整逻辑 cell 应形成有效网格");

    assert_eq!(size.columns(), 9);
    assert_eq!(size.rows(), 2);
    assert_eq!(size.pixel_width(), 98);
    assert_eq!(size.pixel_height(), 48);
}

/// 隐藏或最小化后不足一个 cell 的画布不应把仍在运行的 PTY resize 到 1×1。
///
/// @author kongweiguang
#[test]
fn hidden_or_too_small_canvas_does_not_request_resize() {
    assert_eq!(
        terminal_size_for_canvas_dimensions(23.0, 36.0, 8.0, 18.0, 12.0, 1.0),
        None
    );
    assert_eq!(
        terminal_size_for_canvas_dimensions(0.0, 360.0, 8.0, 18.0, 12.0, 1.0),
        None
    );
}

/// 不可信浮点布局结果不向 PTY 或领域尺寸构造器传播 NaN 和无穷值。
///
/// @author kongweiguang
#[test]
fn invalid_layout_metrics_are_ignored() {
    for (width, height, cell, line, inset, scale_factor) in [
        (f32::NAN, 360.0, 8.0, 18.0, 12.0, 1.0),
        (640.0, f32::INFINITY, 8.0, 18.0, 12.0, 1.0),
        (640.0, 360.0, 0.0, 18.0, 12.0, 1.0),
        (640.0, 360.0, 8.0, 0.0, 12.0, 1.0),
        (640.0, 360.0, 8.0, 18.0, -1.0, 1.0),
        (640.0, 360.0, 8.0, 18.0, 12.0, f32::NAN),
        (640.0, 360.0, 8.0, 18.0, 12.0, f32::INFINITY),
        (640.0, 360.0, 8.0, 18.0, 12.0, 0.0),
        (640.0, 360.0, 8.0, 18.0, 12.0, -1.0),
    ] {
        assert_eq!(
            terminal_size_for_canvas_dimensions(width, height, cell, line, inset, scale_factor),
            None
        );
    }
}

/// 极大窗口受 PTY u16 尺寸限制，领域边界仍返回最大合法列行和像素尺寸。
///
/// @author kongweiguang
#[test]
fn oversized_canvas_is_clamped_to_pty_limits() {
    let size = terminal_size_for_canvas_dimensions(100_000.0, 100_000.0, 1.0, 1.0, 0.0, 2.0)
        .expect("超大窗口仍映射到协议允许的上界");

    assert_eq!(size.columns(), u16::MAX);
    assert_eq!(size.rows(), u16::MAX);
    assert_eq!(size.pixel_width(), u16::MAX);
    assert_eq!(size.pixel_height(), u16::MAX);
}

/// 鼠标 cell 命中与 IME 使用同一 cell_origin/point_to_cell inset 变换，不因坐标入口分叉。
///
/// @author kongweiguang
#[test]
fn pointer_cell_inverse_matches_ime_cell_origin_and_clamps_at_grid_edges() {
    let settings = kerminal_application::preferences::TerminalSettings::default();
    let metrics = TerminalMetrics::from_settings(&settings, &[]);
    let origin = gpui_kit::point(gpui_kit::px(100.0), gpui_kit::px(50.0));
    let bounds = gpui_kit::bounds(
        origin,
        gpui_kit::size(gpui_kit::px(400.0), gpui_kit::px(200.0)),
    );
    let cell_width = gpui_kit::px(8.0);
    let rows = [viewport_row(0), viewport_row(1), viewport_row(2)];
    let ime_origin = metrics.cell_origin(origin, 1, 3, cell_width);

    assert_eq!(
        canvas_pointer_to_cell(&rows, 5, bounds, &metrics, ime_origin, cell_width,),
        Some(ViewportCell { row: 1, column: 3 })
    );
    assert_eq!(
        canvas_pointer_to_cell(
            &rows,
            5,
            bounds,
            &metrics,
            gpui_kit::point(gpui_kit::px(-100.0), gpui_kit::px(-100.0)),
            cell_width,
        ),
        Some(ViewportCell { row: 0, column: 0 })
    );
}

/// 滚动条按总历史与 viewport 比例缩放，底部/顶部锚点和拖动反向关系保持一致。
///
/// @author kongweiguang
#[test]
fn scrollbar_geometry_maps_bottom_top_and_drag_offsets() {
    let bottom =
        terminal_scrollbar_geometry(10.0, 100.0, 25, 75, 0).expect("存在历史时应有可拖滚动条");
    let top =
        terminal_scrollbar_geometry(10.0, 100.0, 25, 75, 75).expect("存在历史时应有可拖滚动条");

    assert_eq!(bottom.thumb_height, 25.0);
    assert_eq!(bottom.thumb_top, 85.0);
    assert_eq!(top.thumb_top, 10.0);
    assert_eq!(scrollbar_drag_offset(0, -75.0, 75, 75.0), Some(75));
    assert_eq!(scrollbar_drag_offset(75, 75.0, 75, 75.0), Some(0));
    assert!(terminal_scrollbar_geometry(0.0, 100.0, 25, 0, 0).is_none());
    assert_eq!(scrollbar_drag_offset(0, f32::NAN, 75, 75.0), None);
}

/// 构造具有稳定身份的 resize 请求，确保会话代次和资源 owner 都参与相等判断。
fn resize_request(binding: UiSessionBinding) -> CanvasResizeRequest {
    CanvasResizeRequest {
        binding,
        size: TerminalSize::new(100, 30, 800, 540).expect("测试尺寸有效"),
    }
}

/// UI pane resize 请求只能在其完整会话、pane 和连接代次仍然一致时通过。
#[test]
fn resize_request_rejects_replaced_session_owner_or_generation() {
    let original = UiSessionBinding {
        session_id: SessionId::parse("session-a").expect("有效测试 id"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-a").expect("有效测试 id"),
            pane_id: PaneId::parse("pane-a").expect("有效测试 id"),
        },
        session_generation: 4,
    };
    let request = resize_request(original.clone());

    assert!(binding_matches_current(&request, &original));
    assert!(!binding_matches_current(
        &request,
        &UiSessionBinding {
            session_generation: 5,
            ..original.clone()
        }
    ));
    assert!(!binding_matches_current(
        &request,
        &UiSessionBinding {
            owner: SessionOwner::Ui {
                tab_id: TabId::parse("tab-a").expect("有效测试 id"),
                pane_id: PaneId::parse("pane-b").expect("有效测试 id"),
            },
            ..original.clone()
        }
    ));
    assert!(!binding_matches_current(
        &request,
        &UiSessionBinding {
            session_id: SessionId::parse("session-b").expect("有效测试 id"),
            ..original.clone()
        }
    ));
}

/// Agent Canvas 用同一绑定校验，不会因切换右栏 Agent 而把旧尺寸发给新会话。
#[test]
fn resize_request_preserves_agent_owner_identity() {
    let first = UiSessionBinding {
        session_id: SessionId::parse("session-agent-a").expect("有效测试 id"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::parse("agent-a").expect("有效测试 id"),
        },
        session_generation: 8,
    };
    let second = UiSessionBinding {
        session_id: SessionId::parse("session-agent-b").expect("有效测试 id"),
        owner: SessionOwner::Agent {
            agent_session_id: AgentSessionId::parse("agent-b").expect("有效测试 id"),
        },
        session_generation: 8,
    };
    let request = resize_request(first.clone());

    assert!(binding_matches_current(&request, &first));
    assert!(!binding_matches_current(&request, &second));
}
