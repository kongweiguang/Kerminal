//! 按终端画布的真实布局尺寸计算 PTY 网格。
//!
//! @author kongweiguang

use gpui_kit::{Bounds, Pixels, Point, point, px};
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::snapshot::TerminalRow;

use crate::features::terminal_tools::selection::ViewportCell;
use crate::terminal_metrics::TerminalMetrics;

const MIN_SCROLLBAR_THUMB_HEIGHT: f32 = 22.0;

/// 可见滚动轨道和 thumb 的纯几何投影，避免 Canvas 事件与绘制各自推导 offset 比例。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalScrollbarGeometry {
    pub track_top: f32,
    pub track_height: f32,
    pub thumb_top: f32,
    pub thumb_height: f32,
    pub max_offset: usize,
}

/// 以相同 inset、等宽列和行高把 IME/选择/链接的窗口点映射到真实快照行。
///
/// @author kongweiguang
pub(crate) fn canvas_pointer_to_cell(
    rows: &[TerminalRow],
    columns: u16,
    bounds: Bounds<Pixels>,
    metrics: &TerminalMetrics,
    position: Point<Pixels>,
    cell_width: Pixels,
) -> Option<ViewportCell> {
    if columns == 0 || rows.is_empty() {
        return None;
    }
    let cell_width_f32 = cell_width.as_f32();
    let line_height = metrics.line_height;
    let inset = metrics.inset;
    let coordinates = [
        position.x.as_f32(),
        position.y.as_f32(),
        bounds.origin.x.as_f32(),
        bounds.origin.y.as_f32(),
        cell_width_f32,
        line_height,
        inset,
    ];
    if coordinates.iter().any(|coordinate| !coordinate.is_finite())
        || cell_width_f32 <= 0.0
        || line_height <= 0.0
        || inset < 0.0
    {
        return None;
    }

    let left = bounds.origin.x + px(inset);
    let top = bounds.origin.y + px(inset);
    let right = left + cell_width * usize::from(columns.saturating_sub(1));
    let grid_rows = rows.len().min(usize::from(u16::MAX)).max(1) as u16;
    let bottom = top + px(line_height * f32::from(grid_rows.saturating_sub(1)));
    let clamped = point(
        px(position.x.as_f32().clamp(left.as_f32(), right.as_f32())),
        px(position.y.as_f32().clamp(top.as_f32(), bottom.as_f32())),
    );
    let cell = metrics.point_to_cell(bounds.origin, clamped, cell_width)?;
    crate::terminal_canvas::interaction::clamp_viewport_cell(rows, columns, cell)
}

/// 依照总历史和视口行数计算可拖 thumb；offset=0 锚定底部，最大 offset 锚定顶部。
///
/// @author kongweiguang
pub(crate) fn terminal_scrollbar_geometry(
    track_top: f32,
    track_height: f32,
    viewport_rows: u16,
    max_offset: usize,
    offset: usize,
) -> Option<TerminalScrollbarGeometry> {
    if !track_top.is_finite()
        || !track_height.is_finite()
        || track_height <= 0.0
        || viewport_rows == 0
        || max_offset == 0
    {
        return None;
    }
    let history_rows = max_offset as f64;
    let total_rows = history_rows + f64::from(viewport_rows);
    let proportional_height = track_height * (f64::from(viewport_rows) / total_rows) as f32;
    let thumb_height = proportional_height
        .max(MIN_SCROLLBAR_THUMB_HEIGHT.min(track_height))
        .min(track_height);
    let travel = (track_height - thumb_height).max(0.0);
    let clamped_offset = offset.min(max_offset);
    let bottom_distance = max_offset - clamped_offset;
    let thumb_top = track_top + travel * (bottom_distance as f64 / history_rows) as f32;
    Some(TerminalScrollbarGeometry {
        track_top,
        track_height,
        thumb_top,
        thumb_height,
        max_offset,
    })
}

/// 将滚动条 thumb 位移转换为 scrollback offset，向上增加历史量且夹在引擎边界内。
///
/// @author kongweiguang
pub(crate) fn scrollbar_drag_offset(
    initial_offset: usize,
    pointer_delta_y: f32,
    max_offset: usize,
    thumb_travel: f32,
) -> Option<usize> {
    if !pointer_delta_y.is_finite() || !thumb_travel.is_finite() || thumb_travel <= 0.0 {
        return None;
    }
    if max_offset == 0 {
        return Some(0);
    }
    let offset_delta = f64::from(pointer_delta_y) * max_offset as f64 / f64::from(thumb_travel);
    let offset = initial_offset as f64 - offset_delta;
    Some(offset.round().clamp(0.0, max_offset as f64) as usize)
}

/// Canvas 首次布局或分屏变化后送往 Root 的不可变 resize 请求。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CanvasResizeRequest {
    pub binding: UiSessionBinding,
    pub size: TerminalSize,
}

/// 只在匹配同一个 pane、session owner 与重连代次时接受迟到的 Canvas 请求。
///
/// @author kongweiguang
pub(crate) fn binding_matches_current(
    request: &CanvasResizeRequest,
    current: &UiSessionBinding,
) -> bool {
    &request.binding == current
}

/// 用 Canvas 逻辑 bounds 推导网格，并将 drawable pixel 尺寸换算为设备像素供 PTY 使用。
///
/// @author kongweiguang
pub(crate) fn terminal_size_for_canvas(
    bounds: Bounds<Pixels>,
    metrics: &TerminalMetrics,
    cell_width: Pixels,
    scale_factor: f32,
) -> Option<TerminalSize> {
    terminal_size_for_canvas_dimensions(
        bounds.size.width.as_f32(),
        bounds.size.height.as_f32(),
        cell_width.as_f32(),
        metrics.line_height,
        metrics.inset,
        scale_factor,
    )
}

/// 网格保持在 GPUI 逻辑像素中计算，只有 PTY 的 drawable pixel 字段按 DPI 换算到设备像素。
///
/// @author kongweiguang
pub(crate) fn terminal_size_for_canvas_dimensions(
    width: f32,
    height: f32,
    cell_width: f32,
    line_height: f32,
    inset: f32,
    scale_factor: f32,
) -> Option<TerminalSize> {
    if !width.is_finite()
        || !height.is_finite()
        || !cell_width.is_finite()
        || !line_height.is_finite()
        || !inset.is_finite()
        || !scale_factor.is_finite()
        || width <= 0.0
        || height <= 0.0
        || cell_width <= 0.0
        || line_height <= 0.0
        || inset < 0.0
        || scale_factor <= 0.0
    {
        return None;
    }

    let content_width = width - inset * 2.0;
    let content_height = height - inset * 2.0;
    if content_width < cell_width || content_height < line_height {
        return None;
    }

    let columns = (content_width / cell_width)
        .floor()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let rows = (content_height / line_height)
        .floor()
        .clamp(1.0, f32::from(u16::MAX)) as u16;
    let pixel_width = device_pixel_extent(content_width, scale_factor)?;
    let pixel_height = device_pixel_extent(content_height, scale_factor)?;

    TerminalSize::new(columns, rows, pixel_width, pixel_height).ok()
}

/// 把逻辑 drawable extent 换成物理设备像素；舍入方式与 GPUI `to_device_pixels` 保持一致。
///
/// @author kongweiguang
fn device_pixel_extent(logical_extent: f32, scale_factor: f32) -> Option<u16> {
    let device_extent = logical_extent * scale_factor;
    if !device_extent.is_finite() || device_extent <= 0.0 {
        return None;
    }

    Some(device_extent.round().clamp(1.0, f32::from(u16::MAX)) as u16)
}

#[cfg(test)]
#[path = "../../tests/unit/terminal_canvas_geometry.rs"]
mod tests;
