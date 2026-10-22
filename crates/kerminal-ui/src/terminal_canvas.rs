//! 基于引擎不可变渲染快照批量绘制原生终端内容。
//!
//! GPUI 的 `shape_line_by_hash` 以文本、字体和 run 样式命中已有 layout cache；快照 revision
//! 只表达终端 damage，不进入文本 cache key，从而让没有变化的行跨 revision 继续复用 layout。
//!
//! @author kongweiguang

mod geometry;
mod interaction;
mod links;
mod overlay;
mod style;

pub(crate) use links::{
    TerminalLinkCandidateId, TerminalLinkDiscoveryCache, TerminalLinkSnapshot,
    TerminalLinkSnapshotIdentity, TerminalLinkSource,
};
pub(crate) use overlay::{
    TerminalCellOverlay, TerminalOverlayKind, TerminalOverlayStyle, search_result_revision_matches,
};

use std::collections::{HashMap, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::ops::Range;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyElement, App, Bounds, ContentMask, ElementInputHandler, Entity, FocusHandle, Hsla,
    IntoElement, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    ScrollWheelEvent, ShapedLine, SharedString, TextAlign, TextRun, TextStyle,
    UnderlineStyle as GpuiUnderlineStyle, WeakEntity, Window, canvas,
    component::menu::ContextMenuExt, div, fill, point, prelude::*, px, size,
};
use kerminal_application::preferences::TerminalRightClickBehavior;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::identity::{PaneId, TabId};
use kerminal_domain::workspace::SplitDirection;
use kerminal_terminal::input::{
    InputModifiers, TerminalInputEvent, TerminalMouseAction, TerminalMouseButton,
    TerminalMouseEvent,
};
use kerminal_terminal::snapshot::{
    MouseTrackingMode, RenderSnapshot, TerminalColor, TerminalCursorShape, TerminalPalette,
    TerminalRun, TerminalSelectionKind,
};

#[cfg(feature = "acceptance-metrics")]
use crate::acceptance_metrics::{
    TerminalEchoPaintObservation, TerminalEchoTarget, terminal_echo_probe_registry,
};
use crate::features::terminal_tools::{
    TerminalToolsView,
    selection::{
        TerminalSelectionGestureToken, ViewportCell, terminal_point_and_viewport_from_cell,
    },
};
use crate::features::workspace_panes::{
    SplitCandidateMeasurement, SplitCandidateProbeContext, SplitResizerAncestorProbe,
};
use crate::root::{
    RootView, pane_input::PaneTerminalInputHandler,
    terminal_context_menu::build_terminal_context_menu,
};
use crate::terminal_input_handler::TerminalImePreferenceHandler;
use crate::terminal_interaction::TerminalRightClickAction;
use crate::terminal_metrics::{TerminalMetrics, effective_cursor_shape, font_weight};
use crate::theme::ThemeTokens;
#[cfg(feature = "acceptance-metrics")]
use kerminal_terminal::snapshot::TerminalRow;

pub(crate) use self::geometry::scrollbar_drag_offset;
pub(crate) use self::geometry::{
    CanvasResizeRequest, binding_matches_current, terminal_size_for_canvas,
};
use self::geometry::{canvas_pointer_to_cell, terminal_scrollbar_geometry};
use self::interaction::{allows_osc8_click, should_defer_selection_clear_on_mouse_down};
use self::links::link_at_point;
use self::overlay::{
    CellOverlayPaint, CellOverlaySpan, CellTextSegment, spans_for_row, style_sort_key,
    text_overlay_segments,
};
use self::style::{
    apply_overlay_paint, apply_terminal_style, default_overlay_colors, hide_text, terminal_rgb,
};

/// 用无输入行为的 Canvas 测量真实首帧网格；弱 Root 与恢复代次阻止迟到 prepaint 复活旧 pane。
///
/// @author kongweiguang
pub(crate) fn workspace_restore_size_probe(
    root: WeakEntity<RootView>,
    tab_id: TabId,
    pane_id: PaneId,
    restore_generation: Option<u64>,
    metrics: TerminalMetrics,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            if !window.is_window_active() {
                return;
            }
            let Some(size) = terminal_size_for_canvas(
                bounds,
                &metrics,
                metrics.cell_width(window),
                window.scale_factor(),
            ) else {
                return;
            };
            let root = root.clone();
            let tab_id = tab_id.clone();
            let pane_id = pane_id.clone();
            cx.defer(move |cx| {
                let _ = root.update(cx, |root, cx| {
                    root.handle_workspace_restore_size_measurement(
                        tab_id,
                        pane_id,
                        restore_generation,
                        size,
                        cx,
                    );
                });
            });
        },
        |_, _, _, _| {},
    )
    .size_full()
}

/// 只为 view-only CreatePane candidate 发出同帧几何回执；祖先 ElementExt 回调晚于叶 Canvas，故延迟到本帧 effect 尾读取。
/// 需完整可见于 GPUI 累积裁剪区后才准入，重试只等待新的布局帧，不放宽几何门禁。
///
/// @author kongweiguang
pub(crate) fn workspace_split_candidate_size_probe(
    root: WeakEntity<RootView>,
    probe: SplitCandidateProbeContext,
    metrics: TerminalMetrics,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            if !window.is_window_active() {
                return;
            }
            let canvas_bounds = split_probe_rect_from_bounds(bounds);
            let clip_bounds = split_probe_rect_from_bounds(window.content_mask().bounds);
            let scale_factor = window.scale_factor();
            let cell_width = metrics.cell_width(window);
            let size = terminal_size_for_canvas(bounds, &metrics, cell_width, scale_factor);

            // 候选 Canvas 位于祖先 split 的子树中，其 prepaint 先于祖先 ElementExt 探针；
            // 只在 defer 阶段读取同帧 Rc 回执，避免每次重绘都拿到尚未写入的空几何。
            cx.defer(move |cx| {
                let Some(size) = size else {
                    let failure = SplitCandidateProbeFailure::TerminalGridUnavailable;
                    if should_log_split_probe_failure(
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        failure,
                    ) {
                        tracing::warn!(
                            target: "kerminal::split_measurement",
                            operation_id = %probe.preview.operation_id,
                            layout_generation = probe.layout_generation,
                            reason = ?failure,
                            canvas = ?canvas_bounds,
                            clip = ?clip_bounds,
                            cell_width = %cell_width,
                            line_height = metrics.line_height,
                            inset = metrics.inset,
                            scale_factor,
                            "split candidate measurement was rejected repeatedly"
                        );
                    }
                    defer_split_probe_retry(
                        root,
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        cx,
                    );
                    return;
                };

                let mut path = Vec::with_capacity(probe.ancestors.len());
                let mut unavailable_ancestor = None;
                for ancestor in &probe.ancestors {
                    if let Some(snapshot) = split_resizer_geometry_snapshot(ancestor, cx) {
                        path.push(snapshot);
                    } else {
                        unavailable_ancestor = Some(ancestor.split_id.clone());
                        break;
                    }
                }
                if let Some(split_id) = unavailable_ancestor {
                    let failure = SplitCandidateProbeFailure::AncestorGeometryUnavailable;
                    if should_log_split_probe_failure(
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        failure,
                    ) {
                        tracing::warn!(
                            target: "kerminal::split_measurement",
                            operation_id = %probe.preview.operation_id,
                            layout_generation = probe.layout_generation,
                            reason = ?failure,
                            split_id,
                            canvas = ?canvas_bounds,
                            clip = ?clip_bounds,
                            ancestor_count = probe.ancestors.len(),
                            "split candidate measurement was rejected repeatedly"
                        );
                    }
                    defer_split_probe_retry(
                        root,
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        cx,
                    );
                    return;
                }

                if let Some(geometry_failure) =
                    split_candidate_geometry_failure(canvas_bounds, clip_bounds, &path)
                {
                    let failure = SplitCandidateProbeFailure::Geometry(geometry_failure);
                    if should_log_split_probe_failure(
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        failure,
                    ) {
                        tracing::warn!(
                            target: "kerminal::split_measurement",
                            operation_id = %probe.preview.operation_id,
                            layout_generation = probe.layout_generation,
                            reason = ?failure,
                            canvas = ?canvas_bounds,
                            clip = ?clip_bounds,
                            path = ?path,
                            "split candidate measurement was rejected repeatedly"
                        );
                    }
                    defer_split_probe_retry(
                        root,
                        &probe.preview.operation_id,
                        probe.layout_generation,
                        cx,
                    );
                    return;
                }

                let measurement = SplitCandidateMeasurement {
                    preview: probe.preview,
                    layout_generation: probe.layout_generation,
                    size,
                };
                let _ = root.update(cx, |root, cx| {
                    root.handle_workspace_split_size_measurement(measurement, cx);
                });
            });
        },
        |_, _, _, _| {},
    )
    .size_full()
}

const SPLIT_GEOMETRY_TOLERANCE: f64 = 1.0;
const RESIZABLE_PANEL_MINIMUM: f64 = 100.0;
const SPLIT_PROBE_FAILURE_REPEAT_THRESHOLD: u8 = 12;
const SPLIT_PROBE_FAILURE_CACHE_LIMIT: usize = 24;
const SPLIT_PROBE_RETRY_FRAME_LIMIT: u8 = 24;
const SELECTION_SCROLL_ACK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq)]
struct SplitProbeRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Debug, PartialEq)]
struct SplitResizerGeometrySnapshot {
    split_id: String,
    direction: SplitDirection,
    child_index: usize,
    group_bounds: Option<SplitProbeRect>,
    container_extent: Option<f64>,
    child_bounds: Vec<Option<SplitProbeRect>>,
    state_sizes: Vec<f64>,
}

/// 几何校验失败按门禁类别区分，避免 UI 把所有失败都归因于窗口过窄。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitCandidateGeometryFailure {
    EmptyPath,
    InvalidCanvas,
    CanvasOutsideClip,
    EmptySplitId,
    DuplicateSplitId,
    MissingGroupBounds,
    InvalidGroupBounds,
    NestedGroupOutsideParentSlot,
    InvalidContainerExtent,
    GroupExtentMismatch,
    InvalidChildCount,
    MissingChildBounds(usize),
    InvalidChildBounds(usize),
    InvalidChildStateSize(usize),
    ChildExtentMismatch(usize),
    MissingCandidateSlot,
    CanvasOutsideLeafSlot,
}

/// 测量诊断区分网格无效、祖先 prepaint 未完成与真实路径几何不匹配。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SplitCandidateProbeFailure {
    TerminalGridUnavailable,
    AncestorGeometryUnavailable,
    Geometry(SplitCandidateGeometryFailure),
    RootAdmission(&'static str),
}

#[derive(Clone, Copy)]
struct SplitProbeFailureRecord {
    failure: SplitCandidateProbeFailure,
    consecutive_frames: u8,
    scheduled_frames: u8,
    reported: bool,
}

#[derive(Default)]
struct SplitProbeFailureCache {
    entries: HashMap<(String, u64), SplitProbeFailureRecord>,
    order: VecDeque<(String, u64)>,
}

static SPLIT_PROBE_FAILURE_CACHE: OnceLock<Mutex<SplitProbeFailureCache>> = OnceLock::new();

/// 仅同一操作和布局代次连续拒绝 12 帧才记录一次诊断，过滤正常首帧布局收敛。
///
/// @author kongweiguang
fn should_log_split_probe_failure(
    operation_id: &str,
    layout_generation: u64,
    failure: SplitCandidateProbeFailure,
) -> bool {
    let cache = SPLIT_PROBE_FAILURE_CACHE.get_or_init(Default::default);
    let Ok(mut cache) = cache.lock() else {
        return true;
    };
    let key = (operation_id.to_owned(), layout_generation);
    if !cache.entries.contains_key(&key) {
        while cache.entries.len() >= SPLIT_PROBE_FAILURE_CACHE_LIMIT {
            let Some(expired) = cache.order.pop_front() else {
                break;
            };
            cache.entries.remove(&expired);
        }
        cache.order.push_back(key.clone());
        cache.entries.insert(
            key.clone(),
            SplitProbeFailureRecord {
                failure,
                consecutive_frames: 0,
                scheduled_frames: 0,
                reported: false,
            },
        );
    }
    let Some(record) = cache.entries.get_mut(&key) else {
        return false;
    };
    if record.failure != failure {
        record.failure = failure;
        record.consecutive_frames = 0;
    }
    record.consecutive_frames = record.consecutive_frames.saturating_add(1);
    if record.consecutive_frames >= SPLIT_PROBE_FAILURE_REPEAT_THRESHOLD && !record.reported {
        record.reported = true;
        true
    } else {
        false
    }
}

/// 几何恢复稳定后释放该候选的临时失败计数，避免长期保存已完成目标。
///
/// @author kongweiguang
pub(crate) fn clear_split_probe_failure(operation_id: &str, layout_generation: u64) {
    let cache = SPLIT_PROBE_FAILURE_CACHE.get_or_init(Default::default);
    let Ok(mut cache) = cache.lock() else {
        return;
    };
    let key = (operation_id.to_owned(), layout_generation);
    cache.entries.remove(&key);
    cache.order.retain(|candidate| candidate != &key);
}

/// Root 端拒绝已稳定几何时也复用同一有界计数，避免测量回调每帧刷日志。
///
/// @author kongweiguang
pub(crate) fn should_log_split_root_rejection(
    operation_id: &str,
    layout_generation: u64,
    stage: &'static str,
) -> bool {
    should_log_split_probe_failure(
        operation_id,
        layout_generation,
        SplitCandidateProbeFailure::RootAdmission(stage),
    )
}

/// 为候选的自然 GPUI prepaint 收敛请求有限重绘；超过预算后交给用户取消，避免常驻轮询。
///
/// @author kongweiguang
pub(crate) fn should_request_split_probe_retry(operation_id: &str, layout_generation: u64) -> bool {
    let cache = SPLIT_PROBE_FAILURE_CACHE.get_or_init(Default::default);
    let Ok(mut cache) = cache.lock() else {
        return false;
    };
    let key = (operation_id.to_owned(), layout_generation);
    if !cache.entries.contains_key(&key) {
        while cache.entries.len() >= SPLIT_PROBE_FAILURE_CACHE_LIMIT {
            let Some(expired) = cache.order.pop_front() else {
                break;
            };
            cache.entries.remove(&expired);
        }
        cache.order.push_back(key.clone());
        cache.entries.insert(
            key.clone(),
            SplitProbeFailureRecord {
                failure: SplitCandidateProbeFailure::RootAdmission("measurement_pending"),
                consecutive_frames: 0,
                scheduled_frames: 0,
                reported: false,
            },
        );
    }
    let Some(record) = cache.entries.get_mut(&key) else {
        return false;
    };
    if record.scheduled_frames >= SPLIT_PROBE_RETRY_FRAME_LIMIT {
        return false;
    }
    record.scheduled_frames = record.scheduled_frames.saturating_add(1);
    true
}

/// 在拒绝采样后请求一次有限 Root 重绘，让 ResizableState 的 defer 可在新帧收敛。
///
/// @author kongweiguang
fn defer_split_probe_retry(
    root: WeakEntity<RootView>,
    operation_id: &str,
    layout_generation: u64,
    cx: &mut App,
) {
    if !should_request_split_probe_retry(operation_id, layout_generation) {
        return;
    }
    cx.defer(move |cx| {
        let _ = root.update(cx, |_, cx| cx.notify());
    });
}

/// 将 Canvas 的真实 GPUI bounds 保留为无窗口依赖的几何值，供延迟门禁使用。
///
/// @author kongweiguang
fn split_probe_rect_from_bounds(bounds: Bounds<Pixels>) -> SplitProbeRect {
    SplitProbeRect {
        x: bounds.origin.x.to_f64(),
        y: bounds.origin.y.to_f64(),
        width: bounds.size.width.to_f64(),
        height: bounds.size.height.to_f64(),
    }
}

/// 在整帧 prepaint 后读取一个祖先的 group、所有直系 slot 和 Kit 状态快照。
///
/// 返回 `None` 表示父级回执尚缺或正在借用，调用方应让本帧候选继续等待下一帧。
///
/// @author kongweiguang
fn split_resizer_geometry_snapshot(
    ancestor: &SplitResizerAncestorProbe,
    cx: &App,
) -> Option<SplitResizerGeometrySnapshot> {
    let group_bounds = ancestor
        .group_bounds
        .try_borrow()
        .ok()?
        .as_ref()
        .copied()
        .map(split_probe_rect_from_bounds)?;
    let child_bounds = ancestor
        .child_bounds
        .try_borrow()
        .ok()?
        .iter()
        .map(|bounds| bounds.as_ref().copied().map(split_probe_rect_from_bounds))
        .collect();
    let state = ancestor.state.read(cx);

    Some(SplitResizerGeometrySnapshot {
        split_id: ancestor.split_id.clone(),
        direction: ancestor.direction,
        child_index: ancestor.child_index,
        group_bounds: Some(group_bounds),
        container_extent: Some(state.container_size().to_f64()),
        child_bounds,
        state_sizes: state.sizes().iter().map(|size| size.to_f64()).collect(),
    })
}

/// 按具体失败阶段区分不完整 prepaint、Resizer 未收敛和坐标越界，供有界诊断复用。
///
/// @author kongweiguang
fn split_candidate_geometry_failure(
    canvas_bounds: SplitProbeRect,
    clip_bounds: SplitProbeRect,
    path: &[SplitResizerGeometrySnapshot],
) -> Option<SplitCandidateGeometryFailure> {
    if path.is_empty() {
        return Some(SplitCandidateGeometryFailure::EmptyPath);
    }
    if !split_probe_rect_is_nonzero(canvas_bounds) {
        return Some(SplitCandidateGeometryFailure::InvalidCanvas);
    }
    if !split_probe_rect_contains(clip_bounds, canvas_bounds) {
        return Some(SplitCandidateGeometryFailure::CanvasOutsideClip);
    }

    let mut previous_child_slot = None;
    let mut seen_split_ids: Vec<&str> = Vec::with_capacity(path.len());
    for ancestor in path {
        if ancestor.split_id.is_empty() {
            return Some(SplitCandidateGeometryFailure::EmptySplitId);
        }
        if seen_split_ids.contains(&ancestor.split_id.as_str()) {
            return Some(SplitCandidateGeometryFailure::DuplicateSplitId);
        }
        seen_split_ids.push(ancestor.split_id.as_str());

        let Some(group_bounds) = ancestor.group_bounds else {
            return Some(SplitCandidateGeometryFailure::MissingGroupBounds);
        };
        let Some(container_extent) = ancestor.container_extent else {
            return Some(SplitCandidateGeometryFailure::InvalidContainerExtent);
        };
        if !split_probe_rect_is_nonzero(group_bounds) {
            return Some(SplitCandidateGeometryFailure::InvalidGroupBounds);
        }
        if let Some(parent_slot) = previous_child_slot
            && !split_probe_rect_contains(parent_slot, group_bounds)
        {
            return Some(SplitCandidateGeometryFailure::NestedGroupOutsideParentSlot);
        }

        let group_extent = split_probe_extent(group_bounds, &ancestor.direction);
        if !container_extent.is_finite() || container_extent <= 0.0 {
            return Some(SplitCandidateGeometryFailure::InvalidContainerExtent);
        }
        if (group_extent - container_extent).abs() > SPLIT_GEOMETRY_TOLERANCE {
            return Some(SplitCandidateGeometryFailure::GroupExtentMismatch);
        }
        if ancestor.child_bounds.len() < 2
            || ancestor.child_bounds.len() != ancestor.state_sizes.len()
            || ancestor.child_index >= ancestor.child_bounds.len()
        {
            return Some(SplitCandidateGeometryFailure::InvalidChildCount);
        }

        for (index, (child_bounds, state_size)) in ancestor
            .child_bounds
            .iter()
            .zip(&ancestor.state_sizes)
            .enumerate()
        {
            let Some(child_bounds) = child_bounds else {
                return Some(SplitCandidateGeometryFailure::MissingChildBounds(index));
            };
            let actual_extent = split_probe_extent(*child_bounds, &ancestor.direction);
            let expected_extent = (*state_size).max(RESIZABLE_PANEL_MINIMUM);
            if !split_probe_rect_is_nonzero(*child_bounds) {
                return Some(SplitCandidateGeometryFailure::InvalidChildBounds(index));
            }
            if !state_size.is_finite() || *state_size < 0.0 {
                return Some(SplitCandidateGeometryFailure::InvalidChildStateSize(index));
            }
            if (actual_extent - expected_extent).abs() > SPLIT_GEOMETRY_TOLERANCE {
                return Some(SplitCandidateGeometryFailure::ChildExtentMismatch(index));
            }
        }

        let Some(child_slot) = ancestor.child_bounds[ancestor.child_index] else {
            return Some(SplitCandidateGeometryFailure::MissingCandidateSlot);
        };
        previous_child_slot = Some(child_slot);
    }

    match previous_child_slot {
        Some(leaf_slot) if split_probe_rect_contains(leaf_slot, canvas_bounds) => None,
        Some(_) => Some(SplitCandidateGeometryFailure::CanvasOutsideLeafSlot),
        None => Some(SplitCandidateGeometryFailure::MissingCandidateSlot),
    }
}

/// 验证 bounds 的坐标和两个 extent 有限且均为正值。
///
/// @author kongweiguang
fn split_probe_rect_is_nonzero(bounds: SplitProbeRect) -> bool {
    let right = bounds.x + bounds.width;
    let bottom = bounds.y + bounds.height;
    bounds.x.is_finite()
        && bounds.y.is_finite()
        && bounds.width.is_finite()
        && bounds.height.is_finite()
        && right.is_finite()
        && bottom.is_finite()
        && bounds.width > 0.0
        && bounds.height > 0.0
}

/// 返回按该 Resizable 方向分配的实际轴向尺寸。
///
/// @author kongweiguang
fn split_probe_extent(bounds: SplitProbeRect, direction: &SplitDirection) -> f64 {
    match direction {
        SplitDirection::Horizontal => bounds.width,
        SplitDirection::Vertical => bounds.height,
    }
}

/// 允许最多 1px 的布局舍入误差，避免嵌套 split 的亚像素边界积累永久拒绝候选。
///
/// @author kongweiguang
fn split_probe_rect_contains(parent: SplitProbeRect, child: SplitProbeRect) -> bool {
    if !split_probe_rect_is_nonzero(parent) || !split_probe_rect_is_nonzero(child) {
        return false;
    }

    parent.x - SPLIT_GEOMETRY_TOLERANCE <= child.x
        && parent.y - SPLIT_GEOMETRY_TOLERANCE <= child.y
        && child.x + child.width <= parent.x + parent.width + SPLIT_GEOMETRY_TOLERANCE
        && child.y + child.height <= parent.y + parent.height + SPLIT_GEOMETRY_TOLERANCE
}

struct PaintedRun {
    origin: Point<Pixels>,
    clip: Bounds<Pixels>,
    line_height: Pixels,
    line: ShapedLine,
    #[cfg(feature = "acceptance-metrics")]
    echo_target: Option<TerminalEchoTarget>,
}

struct TerminalPaintState {
    runs: Vec<PaintedRun>,
    terminal_backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    overlay_backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    cursor_backgrounds: Vec<(Bounds<Pixels>, Hsla)>,
    cursor_glyphs: Vec<PaintedRun>,
    cursor_marks: Vec<(Bounds<Pixels>, Hsla)>,
    scroll_chrome: Option<TerminalScrollChromePaint>,
    #[cfg(feature = "acceptance-metrics")]
    echo_candidate: Option<TerminalPaintEchoCandidate>,
}

/// Paint 候选只保存 session 代次、目标 cell 与门禁观测，不缓存终端行文本。
#[cfg(feature = "acceptance-metrics")]
#[derive(Clone, Copy)]
struct TerminalPaintEchoCandidate {
    target: TerminalEchoTarget,
    cursor_visible: bool,
    cursor_row: u16,
    cursor_column: u16,
    at_bottom: bool,
}

/// 绘制期缓存细滚动条与回到底部按钮的形状，所有 hit-test 复用同一份 bounds。
struct TerminalScrollChromePaint {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
    return_button: Option<Bounds<Pixels>>,
    return_arrow: Option<(ShapedLine, Point<Pixels>, Pixels)>,
    track_color: Hsla,
    thumb_color: Hsla,
    button_color: Hsla,
}

/// 指针交互使用同一渲染几何，滚动条比例、点击区和拖动位移不能在事件闭包里各自重算。
#[derive(Clone, Copy)]
struct TerminalScrollControlLayout {
    track: Bounds<Pixels>,
    thumb: Bounds<Pixels>,
    return_button: Option<Bounds<Pixels>>,
    max_offset: usize,
    thumb_travel: f32,
}

struct TextBatch {
    byte_range: Range<usize>,
    columns: Range<u16>,
    text_runs: Vec<TextRun>,
    last_paint: Option<Option<CellOverlayPaint>>,
    #[cfg(feature = "acceptance-metrics")]
    echo_target: Option<TerminalEchoTarget>,
}

/// 同一帧的终端内容与显示参数一起转移；不复制快照或为分组额外分配绘制数据。
///
/// @author kongweiguang
pub(crate) struct TerminalCanvasFrame {
    pub(crate) view_binding: Option<UiSessionBinding>,
    pub(crate) snapshot: Option<RenderSnapshot>,
    pub(crate) ime_preview: Option<(String, Range<usize>)>,
    pub(crate) mac_option_is_meta: bool,
    pub(crate) metrics: TerminalMetrics,
    pub(crate) overlays: Arc<[TerminalCellOverlay]>,
    pub(crate) cursor_blink_phase_visible: bool,
    pub(crate) theme: ThemeTokens,
}

/// 指针事件借用构造时的同一快照、几何和输入 Entity，保持旧 Canvas 的固定目标校验。
///
/// @author kongweiguang
struct TerminalPointerContext<'a> {
    view_binding: Option<&'a UiSessionBinding>,
    snapshot: Option<&'a RenderSnapshot>,
    bounds: &'a Arc<Mutex<Option<Bounds<Pixels>>>>,
    metrics: &'a TerminalMetrics,
    input_handler: Option<&'a Entity<PaneTerminalInputHandler>>,
}

/// 一次 prepaint 测得的网格几何供各批次借用，避免背景、字形和裁剪重复解释同一组尺寸。
///
/// @author kongweiguang
struct TerminalPaintGrid<'a> {
    canvas_bounds: Bounds<Pixels>,
    metrics: &'a TerminalMetrics,
    cell_width: Pixels,
    line_height: Pixels,
    inset: Pixels,
}

/// 仅凭快照 cell span 证明目标恰为单字节 `x`，允许同一 run 前面包含中文等 Unicode 文本。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn snapshot_cell_is_probe_character(row: &TerminalRow, target_column: u16) -> bool {
    let Some(target_end) = target_column.checked_add(1) else {
        return false;
    };
    row.runs.iter().any(|run| {
        !run.style.attributes.hidden
            && run.cell_spans.iter().any(|span| {
                span.column_range == (target_column..target_end)
                    && run.text.as_bytes().get(span.byte_range.clone()) == Some(&b"x"[..])
            })
    })
}

/// 只给实际覆盖目标终端列的可绘制批次附加 probe 标识，避免相邻背景或 cursor run 误报。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn echo_target_for_painted_columns(
    candidate: Option<TerminalPaintEchoCandidate>,
    viewport_row: u16,
    columns: &Range<u16>,
) -> Option<TerminalEchoTarget> {
    let target = candidate?.target;
    (target.viewport_row == viewport_row && columns.contains(&target.column)).then_some(target)
}

/// 只从当前 primary 可见网格挑选已入队的单字符目标；最终光标门禁仍由 registry 校验。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn terminal_echo_candidate(
    snapshot: &RenderSnapshot,
    binding: &UiSessionBinding,
    row_limit: u16,
    visible_columns: u16,
) -> Option<TerminalPaintEchoCandidate> {
    if snapshot.session_id != binding.session_id
        || snapshot.modes.alternate_screen
        || snapshot.scrollback_offset != 0
    {
        return None;
    }
    let target = terminal_echo_probe_registry()
        .paint_target(binding.session_id.as_str(), binding.session_generation)?;
    if target.viewport_row >= row_limit || target.column >= visible_columns {
        return None;
    }
    let row = snapshot
        .rows
        .iter()
        .find(|row| row.viewport_row == target.viewport_row)?;
    if !snapshot_cell_is_probe_character(row, target.column) {
        return None;
    }
    Some(TerminalPaintEchoCandidate {
        target,
        cursor_visible: snapshot.cursor.visible,
        cursor_row: snapshot.cursor.row,
        cursor_column: snapshot.cursor.column,
        at_bottom: snapshot.scrollback_offset == 0,
    })
}

/// 按快照中的 cell 绘制终端，并用固定输入绑定处理鼠标策略以避免跨会话重定向。
///
/// 右键设置作为构造快照传入；Canvas 不读取全局设置，也不让 Kit 菜单接管 TUI 鼠标协议。
/// 滚轮同样携带该 pane 的完整绑定进入 Root FIFO；Canvas 不直接操作会话资源或抢焦点。
/// Canvas 是自定义 Element，父级填满空间不会替它推导高度，因此必须显式填满父容器，
/// 否则 request_layout 的 auto 尺寸会让行数计算为零并呈现空白终端。
/// 帧上下文只转移原参数；只读绘制绑定用于已结束 pane 的历史操作，输入仍绑定独立 handler。
/// feature-only echo 样本在匹配文字 paint 后读取窗口活动位，作为观测字段，不增设轮询或准入条件。
///
/// @author kongweiguang
pub(crate) fn terminal_canvas(
    frame: TerminalCanvasFrame,
    view: Entity<RootView>,
    input_handler: Option<Entity<PaneTerminalInputHandler>>,
    tools: Entity<TerminalToolsView>,
    focus_handle: FocusHandle,
    right_click_behavior: TerminalRightClickBehavior,
) -> impl IntoElement {
    let TerminalCanvasFrame {
        view_binding,
        snapshot,
        ime_preview,
        mac_option_is_meta,
        metrics,
        overlays,
        cursor_blink_phase_visible,
        theme,
    } = frame;
    let pointer_bounds = Arc::new(Mutex::new(None::<Bounds<Pixels>>));
    let paint_bounds = pointer_bounds.clone();
    let paint_snapshot = snapshot.clone();
    let paint_resize_view_binding = view_binding.clone();
    #[cfg(feature = "acceptance-metrics")]
    let paint_echo_snapshot = snapshot.clone();
    let paint_input_handler = input_handler.clone();
    let paint_resize_handler = input_handler.clone();
    let paint_view = view.clone();
    let paint_metrics = metrics.clone();
    let paint_overlays = overlays.clone();
    #[cfg(feature = "acceptance-metrics")]
    let paint_echo_binding = view_binding.clone();
    #[cfg(feature = "acceptance-metrics")]
    let prepaint_echo_binding = paint_echo_binding.clone();
    let paint_scroll_chrome_enabled = view_binding.is_some();
    let paint_cursor_blink_phase_visible = cursor_blink_phase_visible;
    let paint_theme = theme;
    let canvas = canvas(
        move |bounds, window, cx| {
            let metrics = &paint_metrics;
            let overlays = &paint_overlays;
            let cursor_blink_phase_visible = paint_cursor_blink_phase_visible;
            let theme = paint_theme;
            if let Ok(mut stored_bounds) = paint_bounds.lock() {
                *stored_bounds = Some(bounds);
            }
            let mut runs = Vec::new();
            let mut terminal_backgrounds = Vec::new();
            let mut overlay_backgrounds = Vec::new();
            let mut cursor_backgrounds = Vec::new();
            let mut cursor_glyphs = Vec::new();
            let mut cursor_marks = Vec::new();
            #[cfg(feature = "acceptance-metrics")]
            let mut echo_candidate = None;
            let font_size = px(metrics.font_size);
            let line_height = px(metrics.line_height);
            let inset = px(metrics.inset);
            let cell_width = metrics.cell_width(window);
            let grid = TerminalPaintGrid {
                canvas_bounds: bounds,
                metrics,
                cell_width,
                line_height,
                inset,
            };
            if let Some(snapshot) = paint_snapshot.as_ref() {
                let handler_binding = paint_resize_handler
                    .as_ref()
                    .map(|input_handler| input_handler.read(cx).binding().clone());
                let binding = handler_binding.or_else(|| paint_resize_view_binding.clone());
                if let Some(binding) = binding
                    && paint_resize_view_binding
                        .as_ref()
                        .is_none_or(|view_binding| view_binding == &binding)
                    && snapshot.session_id == binding.session_id
                    && let Some(size) =
                        terminal_size_for_canvas(bounds, metrics, cell_width, window.scale_factor())
                    && snapshot.size != size
                {
                    let request = CanvasResizeRequest { binding, size };
                    let view = paint_view.clone();
                    // prepaint 已拿到精确 bounds；defer 到绘制事务结束后再让 Root 入现有会话 FIFO。
                    cx.defer(move |cx| {
                        view.update(cx, |root, cx| {
                            root.handle_terminal_canvas_resize(request, cx);
                        });
                    });
                }
            }
            let mut text_style = window.text_style();
            text_style.font_family = metrics.font_family.clone();
            text_style.font_size = font_size.into();
            text_style.line_height = line_height.into();
            text_style.font_weight = font_weight(metrics.font_weight);

            let content_width = (bounds.size.width - inset * 2.0).max(px(0.0));
            let content_height = (bounds.size.height - inset * 2.0).max(px(0.0));
            let row_limit = (content_height / line_height)
                .floor()
                .clamp(0.0, f32::from(u16::MAX)) as u16;
            let column_limit = (content_width / cell_width)
                .floor()
                .clamp(0.0, f32::from(u16::MAX)) as u16;

            if let Some(snapshot) = paint_snapshot.as_ref() {
                let visible_columns = snapshot.size.columns().min(column_limit);
                #[cfg(feature = "acceptance-metrics")]
                {
                    echo_candidate = prepaint_echo_binding.as_ref().and_then(|binding| {
                        terminal_echo_candidate(snapshot, binding, row_limit, visible_columns)
                    });
                }
                for row in snapshot.rows.iter() {
                    if row.viewport_row >= row_limit {
                        continue;
                    }
                    let row_origin =
                        metrics.cell_origin(bounds.origin, row.viewport_row, 0, cell_width);
                    let row_spans =
                        spans_for_row(row, visible_columns, snapshot.selection, overlays);
                    collect_overlay_backgrounds(
                        &row_spans,
                        row_origin,
                        &grid,
                        &snapshot.palette,
                        &mut overlay_backgrounds,
                    );

                    for run in row.runs.iter() {
                        collect_terminal_background(
                            run,
                            row.viewport_row,
                            visible_columns,
                            &grid,
                            &snapshot.palette,
                            &mut terminal_backgrounds,
                        );
                        if run.text.is_empty() || run.start_column >= visible_columns {
                            continue;
                        }
                        let run_end_column = run.end_column.min(visible_columns);
                        if run_end_column <= run.start_column {
                            continue;
                        }
                        let segments = text_overlay_segments(
                            &run.text,
                            &run.cell_spans,
                            run.start_column,
                            run_end_column,
                            &row_spans,
                        );
                        let mut ascii_batch = None;
                        for segment in &segments {
                            if segment.byte_range.is_empty() {
                                continue;
                            }
                            let grapheme = &run.text[segment.byte_range.clone()];
                            let is_single_ascii_cell = grapheme.len() == 1
                                && grapheme.is_ascii()
                                && segment
                                    .column_range
                                    .end
                                    .saturating_sub(segment.column_range.start)
                                    == 1
                                && segment.visible_columns == segment.column_range;
                            if is_single_ascii_cell {
                                let can_extend =
                                    ascii_batch.as_ref().is_some_and(|batch: &TextBatch| {
                                        batch.byte_range.end == segment.byte_range.start
                                            && batch.columns.end == segment.column_range.start
                                    });
                                if !can_extend {
                                    flush_ascii_batch(
                                        ascii_batch.take(),
                                        &run.text,
                                        row.viewport_row,
                                        &grid,
                                        font_size,
                                        window,
                                        &mut runs,
                                    );
                                    ascii_batch = Some(TextBatch {
                                        byte_range: segment.byte_range.clone(),
                                        columns: segment.column_range.clone(),
                                        text_runs: Vec::new(),
                                        last_paint: None,
                                        #[cfg(feature = "acceptance-metrics")]
                                        echo_target: echo_target_for_painted_columns(
                                            echo_candidate,
                                            row.viewport_row,
                                            &segment.column_range,
                                        ),
                                    });
                                }
                                if let Some(batch) = ascii_batch.as_mut() {
                                    #[cfg(feature = "acceptance-metrics")]
                                    if let Some(target) = echo_target_for_painted_columns(
                                        echo_candidate,
                                        row.viewport_row,
                                        &segment.column_range,
                                    ) {
                                        batch.echo_target = Some(target);
                                    }
                                    append_text_segment(
                                        batch,
                                        segment,
                                        &text_style,
                                        run.style,
                                        &snapshot.palette,
                                    );
                                }
                                continue;
                            }

                            flush_ascii_batch(
                                ascii_batch.take(),
                                &run.text,
                                row.viewport_row,
                                &grid,
                                font_size,
                                window,
                                &mut runs,
                            );
                            let text_run = text_run_for_segment(
                                &text_style,
                                segment.byte_range.len(),
                                run.style,
                                segment.paint,
                                &snapshot.palette,
                            );
                            if let Some(clip) =
                                cell_clip(&grid, row.viewport_row, segment.visible_columns.clone())
                            {
                                let line = shape_cached_line(
                                    window,
                                    grapheme,
                                    font_size,
                                    &[text_run],
                                    None,
                                );
                                let origin = metrics.cell_origin(
                                    bounds.origin,
                                    row.viewport_row,
                                    segment.column_range.start,
                                    cell_width,
                                );
                                runs.push(PaintedRun {
                                    origin,
                                    clip,
                                    line_height,
                                    line,
                                    #[cfg(feature = "acceptance-metrics")]
                                    echo_target: echo_target_for_painted_columns(
                                        echo_candidate,
                                        row.viewport_row,
                                        &segment.visible_columns,
                                    ),
                                });
                            }
                        }
                        flush_ascii_batch(
                            ascii_batch,
                            &run.text,
                            row.viewport_row,
                            &grid,
                            font_size,
                            window,
                            &mut runs,
                        );
                    }
                }

                if let Some((text, selection)) = &ime_preview
                    && !text.is_empty()
                    && snapshot.cursor.row < row_limit
                {
                    let origin = metrics.cell_origin(
                        bounds.origin,
                        snapshot.cursor.row,
                        snapshot.cursor.column,
                        cell_width,
                    );
                    let foreground =
                        terminal_rgb(snapshot.palette.resolve(TerminalColor::DefaultForeground));
                    let selected_background =
                        terminal_rgb(snapshot.palette.resolve(TerminalColor::Cursor));
                    let selected_foreground =
                        terminal_rgb(snapshot.palette.resolve(TerminalColor::DefaultBackground));
                    let (selection_start, selection_end) = utf8_safe_range(text, selection.clone());
                    let mut preview_runs = Vec::with_capacity(3);
                    if selection_start > 0 {
                        preview_runs.push(ime_preview_run(
                            text_style.to_run(selection_start),
                            foreground,
                            None,
                        ));
                    }
                    if selection_end > selection_start {
                        preview_runs.push(ime_preview_run(
                            text_style.to_run(selection_end - selection_start),
                            selected_foreground,
                            Some(selected_background),
                        ));
                    }
                    if selection_end < text.len() {
                        preview_runs.push(ime_preview_run(
                            text_style.to_run(text.len() - selection_end),
                            foreground,
                            None,
                        ));
                    }
                    if preview_runs.is_empty() {
                        preview_runs.push(ime_preview_run(
                            text_style.to_run(text.len()),
                            foreground,
                            None,
                        ));
                    }
                    let line = shape_cached_line(window, text, font_size, &preview_runs, None);
                    if let Some(clip) = cell_clip(&grid, snapshot.cursor.row, 0..visible_columns) {
                        runs.push(PaintedRun {
                            origin,
                            clip,
                            line_height,
                            line,
                            #[cfg(feature = "acceptance-metrics")]
                            echo_target: None,
                        });
                    }
                }

                let ime_is_active = ime_preview
                    .as_ref()
                    .is_some_and(|(text, _)| !text.is_empty());
                if snapshot.cursor.visible
                    && (!snapshot.cursor.blinking
                        || !metrics.cursor_blink
                        || cursor_blink_phase_visible)
                    && snapshot.scrollback_offset == 0
                    && !ime_is_active
                    && snapshot.cursor.row < row_limit
                    && snapshot.cursor.column < visible_columns
                {
                    let mut cursor_column = snapshot.cursor.column;
                    let mut cursor_columns = 1usize;
                    if let Some(cell) = snapshot.cursor_cell.as_ref() {
                        cursor_column = cell.start_column;
                        cursor_columns = usize::from(cell.columns.max(1));
                    }
                    cursor_columns = cursor_columns
                        .min(usize::from(visible_columns.saturating_sub(cursor_column)));
                    let origin = metrics.cell_origin(
                        bounds.origin,
                        snapshot.cursor.row,
                        cursor_column,
                        cell_width,
                    );
                    let remaining_width =
                        (bounds.origin.x + bounds.size.width - inset - origin.x).max(px(0.0));
                    if cursor_columns > 0 && remaining_width > px(0.0) {
                        let cursor_color =
                            terminal_rgb(snapshot.palette.resolve(TerminalColor::Cursor));
                        let cursor_shape = effective_cursor_shape(
                            metrics.cursor_style,
                            snapshot.cursor.shape,
                            snapshot.cursor.style_from_terminal,
                        );
                        match cursor_shape {
                            TerminalCursorShape::Block => {
                                let width = (cell_width * cursor_columns).min(remaining_width);
                                cursor_backgrounds.push((
                                    gpui_kit::bounds(origin, size(width, line_height)),
                                    cursor_color,
                                ));
                                if let Some(cell) = snapshot.cursor_cell.as_ref() {
                                    let mut glyph_run = text_style.to_run(cell.text.len());
                                    apply_terminal_style(
                                        &mut glyph_run,
                                        cell.style,
                                        &snapshot.palette,
                                    );
                                    if !cell.style.attributes.hidden {
                                        let original_background = if cell.style.attributes.inverse {
                                            cell.style.foreground
                                        } else {
                                            cell.style.background
                                        };
                                        glyph_run.color = terminal_rgb(
                                            snapshot.palette.resolve(original_background),
                                        );
                                        glyph_run.background_color = None;
                                    }
                                    let glyph = shape_cached_line(
                                        window,
                                        &cell.text,
                                        font_size,
                                        &[glyph_run],
                                        None,
                                    );
                                    if let Some(clip) = cell_clip(
                                        &grid,
                                        snapshot.cursor.row,
                                        cursor_column
                                            ..cursor_column.saturating_add(cursor_columns as u16),
                                    ) {
                                        cursor_glyphs.push(PaintedRun {
                                            origin,
                                            clip,
                                            line_height,
                                            line: glyph,
                                            #[cfg(feature = "acceptance-metrics")]
                                            echo_target: None,
                                        });
                                    }
                                }
                            }
                            TerminalCursorShape::Beam => {
                                cursor_marks.push((
                                    gpui_kit::bounds(
                                        origin,
                                        size(px(1.5).min(remaining_width), line_height),
                                    ),
                                    cursor_color,
                                ));
                            }
                            TerminalCursorShape::Underline => {
                                let height = px(2.0).min(line_height);
                                cursor_marks.push((
                                    gpui_kit::bounds(
                                        point(origin.x, origin.y + line_height - height),
                                        size(cell_width.min(remaining_width), height),
                                    ),
                                    cursor_color,
                                ));
                            }
                        }
                    }
                }
            }

            let scroll_chrome = paint_scroll_chrome_enabled
                .then(|| {
                    paint_snapshot.as_ref().and_then(|snapshot| {
                        let layout = terminal_scroll_control_layout(bounds, snapshot, metrics)?;
                        let arrow = layout.return_button.map(|button| {
                            let mut style = window.text_style();
                            style.font_family = metrics.font_family.clone();
                            style.font_size = px(13.0).into();
                            style.color = theme.foreground;
                            let line =
                                shape_cached_line(window, "↓", px(13.0), &[style.to_run(3)], None);
                            let origin = point(
                                button.origin.x + (button.size.width - line.width()) / 2.0,
                                button.origin.y + px(3.0),
                            );
                            (line, origin, px(18.0))
                        });
                        Some(TerminalScrollChromePaint {
                            track: layout.track,
                            thumb: layout.thumb,
                            return_button: layout.return_button,
                            return_arrow: arrow,
                            track_color: theme.border.opacity(0.24),
                            thumb_color: theme.muted.opacity(0.72),
                            button_color: theme.panel,
                        })
                    })
                })
                .flatten();

            TerminalPaintState {
                runs,
                terminal_backgrounds,
                overlay_backgrounds,
                cursor_backgrounds,
                cursor_glyphs,
                cursor_marks,
                scroll_chrome,
                #[cfg(feature = "acceptance-metrics")]
                echo_candidate,
            }
        },
        move |bounds, state, window, cx| {
            if let Some(input_handler) = paint_input_handler.as_ref() {
                window.handle_input(
                    &focus_handle,
                    TerminalImePreferenceHandler::new(
                        ElementInputHandler::new(bounds, input_handler.clone()),
                        mac_option_is_meta,
                    ),
                    cx,
                );
            }
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                #[cfg(feature = "acceptance-metrics")]
                let echo_candidate = state.echo_candidate;
                for (background, color) in state.terminal_backgrounds {
                    window.paint_quad(fill(background, color));
                }
                for (background, color) in state.overlay_backgrounds {
                    window.paint_quad(fill(background, color));
                }
                for run in state.runs {
                    #[cfg(feature = "acceptance-metrics")]
                    let echo_target = run.echo_target;
                    let paint_result = window.with_content_mask(
                        Some(ContentMask { bounds: run.clip }),
                        |window| {
                            run.line.paint(
                                run.origin,
                                run.line_height,
                                TextAlign::Left,
                                None,
                                window,
                                cx,
                            )
                        },
                    );
                    #[cfg(not(feature = "acceptance-metrics"))]
                    let _ = paint_result;
                    #[cfg(feature = "acceptance-metrics")]
                    if paint_result.is_ok()
                        && let (Some(candidate), Some(target), Some(binding), Some(snapshot)) = (
                            echo_candidate,
                            echo_target,
                            paint_echo_binding.as_ref(),
                            paint_echo_snapshot.as_ref(),
                        )
                        && target == candidate.target
                    {
                        let painted_at = Instant::now();
                        let observation = TerminalEchoPaintObservation {
                            target_cell_is_character: true,
                            painted_run_covers_target: true,
                            cursor_visible: candidate.cursor_visible,
                            cursor_row: candidate.cursor_row,
                            cursor_column: candidate.cursor_column,
                            at_bottom: candidate.at_bottom,
                            window_active_at_paint: window.is_window_active(),
                        };
                        terminal_echo_probe_registry().record_canvas_character_paint(
                            binding.session_id.as_str(),
                            binding.session_generation,
                            snapshot.revision,
                            target,
                            observation,
                            painted_at,
                        );
                    }
                }
                for (cursor, color) in state.cursor_backgrounds {
                    window.paint_quad(fill(cursor, color));
                }
                for run in state.cursor_glyphs {
                    window.with_content_mask(Some(ContentMask { bounds: run.clip }), |window| {
                        let _ = run.line.paint(
                            run.origin,
                            run.line_height,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        );
                    });
                }
                for (cursor, color) in state.cursor_marks {
                    window.paint_quad(fill(cursor, color));
                }
                if let Some(chrome) = state.scroll_chrome {
                    window.paint_quad(fill(chrome.track, chrome.track_color));
                    window.paint_quad(fill(chrome.thumb, chrome.thumb_color));
                    if let Some(button) = chrome.return_button {
                        window.paint_quad(fill(button, chrome.button_color));
                    }
                    if let Some((arrow, origin, line_height)) = chrome.return_arrow {
                        window.with_content_mask(Some(ContentMask { bounds }), |window| {
                            let _ = arrow.paint(
                                origin,
                                line_height,
                                TextAlign::Center,
                                None,
                                window,
                                cx,
                            );
                        });
                    }
                }
            });
        },
    )
    .size_full();

    let down_snapshot = snapshot.clone();
    let down_view_binding = view_binding.clone();
    let down_bounds = pointer_bounds.clone();
    let down_metrics = metrics.clone();
    let down_tools = tools.clone();
    let down_input_handler = input_handler.clone();
    let down_view = view.clone();
    let mut element = div()
        .size_full()
        .on_any_mouse_down(move |event, window, cx| {
            handle_mouse_down(
                &TerminalPointerContext {
                    view_binding: down_view_binding.as_ref(),
                    snapshot: down_snapshot.as_ref(),
                    bounds: &down_bounds,
                    metrics: &down_metrics,
                    input_handler: down_input_handler.as_ref(),
                },
                &down_tools,
                &down_view,
                right_click_behavior,
                event,
                window,
                cx,
            );
        });

    for button in [MouseButton::Left, MouseButton::Middle, MouseButton::Right] {
        let up_snapshot = snapshot.clone();
        let up_view_binding = view_binding.clone();
        let up_bounds = pointer_bounds.clone();
        let up_metrics = metrics.clone();
        let up_tools = tools.clone();
        let up_input_handler = input_handler.clone();
        let up_view = view.clone();
        element = element.on_mouse_up(button, move |event, window, cx| {
            handle_mouse_up(
                &TerminalPointerContext {
                    view_binding: up_view_binding.as_ref(),
                    snapshot: up_snapshot.as_ref(),
                    bounds: &up_bounds,
                    metrics: &up_metrics,
                    input_handler: up_input_handler.as_ref(),
                },
                &up_tools,
                &up_view,
                event,
                window,
                cx,
            );
        });

        let up_snapshot = snapshot.clone();
        let up_view_binding = view_binding.clone();
        let up_bounds = pointer_bounds.clone();
        let up_metrics = metrics.clone();
        let up_tools = tools.clone();
        let up_input_handler = input_handler.clone();
        let up_view = view.clone();
        element = element.on_mouse_up_out(button, move |event, window, cx| {
            handle_mouse_up(
                &TerminalPointerContext {
                    view_binding: up_view_binding.as_ref(),
                    snapshot: up_snapshot.as_ref(),
                    bounds: &up_bounds,
                    metrics: &up_metrics,
                    input_handler: up_input_handler.as_ref(),
                },
                &up_tools,
                &up_view,
                event,
                window,
                cx,
            );
        });
    }

    let wheel_snapshot = snapshot.clone();
    let wheel_bounds = pointer_bounds.clone();
    let wheel_metrics = metrics.clone();
    let wheel_input_handler = input_handler.clone();
    let wheel_view_binding = view_binding.clone();
    let wheel_view = view.clone();
    element = element.on_scroll_wheel(move |event: &ScrollWheelEvent, window, cx| {
        #[cfg(feature = "acceptance-metrics")]
        terminal_echo_probe_registry()
            .record_canvas_wheel_callback(event.touch_phase, &event.delta);

        let (Some(snapshot), Some(bounds)) =
            (wheel_snapshot.as_ref(), cached_canvas_bounds(&wheel_bounds))
        else {
            return;
        };
        let binding = wheel_view_binding.as_ref().or_else(|| {
            wheel_input_handler
                .as_ref()
                .map(|handler| handler.read(cx).binding())
        });
        let Some(binding) = binding.cloned() else {
            return;
        };
        let cell = pointer_cell(
            snapshot,
            bounds,
            &wheel_metrics,
            event.position,
            wheel_metrics.cell_width(window),
        )
        .map(|cell| (cell.column, cell.row));
        let request = crate::root::terminal_wheel::TerminalWheelRequest {
            binding,
            snapshot_session_id: snapshot.session_id.clone(),
            snapshot_size: snapshot.size,
            delta: event.delta,
            phase: event.touch_phase,
            line_height: px(wheel_metrics.line_height),
            shift: event.modifiers.shift,
            modifiers: InputModifiers {
                shift: event.modifiers.shift,
                alt: event.modifiers.alt,
                control: event.modifiers.control,
                super_key: event.modifiers.platform,
            },
            mouse_tracking: snapshot.modes.mouse_tracking,
            alternate_screen: snapshot.modes.alternate_screen,
            alternate_scroll: snapshot.modes.alternate_scroll,
            application_cursor: snapshot.modes.application_cursor,
            cell,
        };
        let handled = wheel_view.update(cx, |root, cx| {
            crate::root::terminal_wheel::handle(root, request, cx)
        });
        if handled {
            // Canvas owns wheel routing even for sub-line deltas or alt-screen no-ops; ancestors
            // must not turn a terminal gesture into workbench navigation.
            cx.stop_propagation();
        }
    });

    let move_snapshot = snapshot;
    let move_view_binding = view_binding;
    let move_bounds = pointer_bounds;
    let move_metrics = metrics;
    let move_tools = tools.clone();
    let move_input_handler = input_handler;
    let move_view = view.clone();
    let move_window_tools = tools.clone();
    let menu_snapshot = move_snapshot.clone();
    let menu_input_handler = move_input_handler.clone();
    let menu_view_binding = move_view_binding.clone();
    let menu_tools = move_tools.clone();
    let menu_root = view.clone();
    element = element.on_mouse_move(move |event, window, cx| {
        handle_mouse_move(
            &TerminalPointerContext {
                view_binding: move_view_binding.as_ref(),
                snapshot: move_snapshot.as_ref(),
                bounds: &move_bounds,
                metrics: &move_metrics,
                input_handler: move_input_handler.as_ref(),
            },
            &move_window_tools,
            &move_view,
            event,
            window,
            cx,
        );
    });

    let element: AnyElement = if context_menu_should_attach(
        right_click_behavior,
        menu_snapshot
            .as_ref()
            .map(|snapshot| snapshot.modes.mouse_tracking),
        menu_view_binding.is_some(),
    ) {
        if let (Some(snapshot), Some(binding)) = (menu_snapshot, menu_view_binding) {
            let session_id = snapshot.session_id.clone();
            let snapshot_has_selection = snapshot.selection.is_some();
            element
                .id(format!("terminal-context-menu-{}", session_id.as_str()))
                .context_menu(move |menu, _, cx| {
                    // 菜单必须读打开瞬间的确认态；MouseUp 后新选择尚未回写 snapshot 时也能复制/清除。
                    let has_selection = snapshot_has_selection
                        || menu_tools.read(cx).has_active_selection_gesture(&binding)
                        || menu_root.read(cx).terminal_selection_may_be_present(
                            &binding,
                            &menu_tools,
                            cx,
                        );
                    build_terminal_context_menu(
                        menu,
                        menu_root.clone(),
                        menu_input_handler.clone(),
                        menu_tools.clone(),
                        binding.clone(),
                        has_selection,
                        cx,
                    )
                })
                .child(canvas)
                .into_any_element()
        } else {
            element.child(canvas).into_any_element()
        }
    } else {
        element.child(canvas).into_any_element()
    };
    element
}

/// 从右键偏好生成 Canvas 本地动作；Agent 操作菜单当前没有可用项，不启用 Shift 覆盖。
///
/// @author kongweiguang
fn right_click_action(behavior: TerminalRightClickBehavior) -> TerminalRightClickAction {
    match behavior {
        TerminalRightClickBehavior::None => TerminalRightClickAction::FocusOnly,
        TerminalRightClickBehavior::Paste => TerminalRightClickAction::Paste,
        TerminalRightClickBehavior::Menu => TerminalRightClickAction::Menu,
    }
}

/// 终端左键和 TUI 鼠标事件必须在发送 press 前激活同一 pane；普通菜单右键不切换焦点。
///
/// @author kongweiguang
fn mouse_down_should_focus_target(
    button: MouseButton,
    mouse_tracking: bool,
    right_click_action: TerminalRightClickAction,
) -> bool {
    if mouse_tracking {
        return terminal_mouse_button(button).is_some();
    }
    match button {
        MouseButton::Left => true,
        MouseButton::Right => right_click_action != TerminalRightClickAction::Menu,
        MouseButton::Middle | MouseButton::Navigate(_) => false,
    }
}

/// 只接受与渲染 view 同 owner/generation 的绑定；已退出 pane 可只读命中，但 handler 不能替换它。
///
/// @author kongweiguang
fn pointer_binding(pointer: &TerminalPointerContext<'_>, cx: &App) -> Option<UiSessionBinding> {
    let handler_binding = pointer
        .input_handler
        .map(|handler| handler.read(cx).binding());
    match (pointer.view_binding, handler_binding) {
        (Some(view_binding), Some(handler_binding)) if view_binding == handler_binding => {
            Some(view_binding.clone())
        }
        (Some(_), Some(_)) => None,
        (Some(view_binding), None) => Some(view_binding.clone()),
        (None, Some(handler_binding)) => Some(handler_binding.clone()),
        (None, None) => None,
    }
}

/// 只在快照可用且 DEC mouse tracking 关闭时装载原生菜单；只读历史 pane 也可看选择菜单。
///
/// @author kongweiguang
fn context_menu_should_attach(
    behavior: TerminalRightClickBehavior,
    mouse_tracking: Option<MouseTrackingMode>,
    has_view_binding: bool,
) -> bool {
    behavior == TerminalRightClickBehavior::Menu
        && has_view_binding
        && mouse_tracking == Some(MouseTrackingMode::None)
}

/// 从预绘制阶段缓存的 bounds 读取当前 Canvas 几何；锁失效时交互失败关闭。
///
/// @author kongweiguang
fn cached_canvas_bounds(bounds: &Arc<Mutex<Option<Bounds<Pixels>>>>) -> Option<Bounds<Pixels>> {
    bounds.lock().ok().and_then(|bounds| *bounds)
}

/// 使用 TerminalMetrics 的 inset、行高和字体宽度，把指针夹到目标网格后再转成 cell。
///
/// @author kongweiguang
fn pointer_cell(
    snapshot: &RenderSnapshot,
    bounds: Bounds<Pixels>,
    metrics: &TerminalMetrics,
    position: Point<Pixels>,
    cell_width: Pixels,
) -> Option<ViewportCell> {
    canvas_pointer_to_cell(
        &snapshot.rows,
        snapshot.size.columns(),
        bounds,
        metrics,
        position,
        cell_width,
    )
}

/// 根据同一历史比例构造绘制和事件共享的细轨、thumb 与回底按钮 hit bounds。
///
/// @author kongweiguang
fn terminal_scroll_control_layout(
    bounds: Bounds<Pixels>,
    snapshot: &RenderSnapshot,
    metrics: &TerminalMetrics,
) -> Option<TerminalScrollControlLayout> {
    let track_top = bounds.origin.y.as_f32() + metrics.inset;
    let track_height = bounds.size.height.as_f32() - metrics.inset * 2.0;
    let geometry = terminal_scrollbar_geometry(
        track_top,
        track_height,
        snapshot.size.rows(),
        snapshot.history_size,
        snapshot.scrollback_offset,
    )?;
    let track_width = px(4.0);
    let track_x = bounds.origin.x + bounds.size.width - px(6.0);
    let track = gpui_kit::bounds(
        point(track_x, px(geometry.track_top)),
        size(track_width, px(geometry.track_height)),
    );
    let thumb = gpui_kit::bounds(
        point(track_x, px(geometry.thumb_top)),
        size(track_width, px(geometry.thumb_height)),
    );
    let return_button =
        (snapshot.scrollback_offset > 0 && geometry.track_height >= 42.0).then(|| {
            gpui_kit::bounds(
                point(
                    track_x - px(29.0),
                    px(geometry.track_top + geometry.track_height - 32.0),
                ),
                size(px(24.0), px(24.0)),
            )
        });
    Some(TerminalScrollControlLayout {
        track,
        thumb,
        return_button,
        max_offset: geometry.max_offset,
        thumb_travel: geometry.track_height - geometry.thumb_height,
    })
}

/// 细 thumb 周围留出宽命中带，点击轨道和按钮不会落入终端文本选择。
///
/// @author kongweiguang
fn scroll_control_hit(
    layout: TerminalScrollControlLayout,
    position: Point<Pixels>,
) -> Option<TerminalScrollControlHit> {
    if layout
        .return_button
        .is_some_and(|button| point_in_bounds(button, position))
    {
        return Some(TerminalScrollControlHit::ReturnToBottom);
    }
    if point_in_bounds(
        gpui_kit::bounds(
            point(layout.track.origin.x - px(5.0), layout.track.origin.y),
            size(layout.track.size.width + px(10.0), layout.track.size.height),
        ),
        position,
    ) {
        return Some(if point_in_bounds(layout.thumb, position) {
            TerminalScrollControlHit::Thumb
        } else {
            TerminalScrollControlHit::Track
        });
    }
    None
}

/// GPUI Bounds containment in global logical coordinates shared by the painted controls and handlers.
///
/// @author kongweiguang
fn point_in_bounds(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
    position.x >= bounds.origin.x
        && position.y >= bounds.origin.y
        && position.x <= bounds.origin.x + bounds.size.width
        && position.y <= bounds.origin.y + bounds.size.height
}

/// 计算 pointer 是否位于可接收拖选自动滚动的顶部或底部边缘带。
///
/// @author kongweiguang
fn selection_edge_direction(
    bounds: Bounds<Pixels>,
    metrics: &TerminalMetrics,
    position: Point<Pixels>,
) -> i8 {
    let top = bounds.origin.y.as_f32() + metrics.inset;
    let bottom = bounds.origin.y.as_f32() + bounds.size.height.as_f32() - metrics.inset;
    let band = (metrics.line_height * 1.5).clamp(12.0, 28.0);
    if position.y.as_f32() < top + band {
        -1
    } else if position.y.as_f32() >= bottom - band {
        1
    } else {
        0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TerminalScrollControlHit {
    Thumb,
    Track,
    ReturnToBottom,
}

/// 双击使用引擎的 semantic word 扩展，三击扩展整行，Alt 拖动使用矩形块选择。
///
/// @author kongweiguang
fn selection_kind_for_gesture(click_count: usize, alt: bool) -> Option<TerminalSelectionKind> {
    if alt {
        Some(TerminalSelectionKind::Block)
    } else {
        match click_count {
            1 => Some(TerminalSelectionKind::Character),
            2 => Some(TerminalSelectionKind::Word),
            3 => Some(TerminalSelectionKind::Line),
            _ => None,
        }
    }
}

/// 点击轨道时以 thumb 中心对齐指针，保持与滚动条实际比例相同的 offset 映射。
///
/// @author kongweiguang
fn scrollbar_offset_for_track_click(
    layout: TerminalScrollControlLayout,
    pointer_y: f32,
) -> Option<usize> {
    if !pointer_y.is_finite() || layout.max_offset == 0 || layout.thumb_travel <= 0.0 {
        return None;
    }
    let thumb_top =
        (pointer_y - layout.track.origin.y.as_f32() - layout.thumb.size.height.as_f32() / 2.0)
            .clamp(0.0, layout.thumb_travel);
    let bottom_distance = (f64::from(thumb_top) * layout.max_offset as f64
        / f64::from(layout.thumb_travel))
    .round()
    .clamp(0.0, layout.max_offset as f64) as usize;
    Some(layout.max_offset.saturating_sub(bottom_distance))
}

/// 映射 GPUI 的普通按钮到终端协议按钮；导航键不伪装成终端主/次键。
///
/// @author kongweiguang
fn terminal_mouse_button(button: MouseButton) -> Option<TerminalMouseButton> {
    match button {
        MouseButton::Left => Some(TerminalMouseButton::Left),
        MouseButton::Middle => Some(TerminalMouseButton::Middle),
        MouseButton::Right => Some(TerminalMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

/// 从同一指针上下文校验快照与固定输入 Entity，再提交 DEC 鼠标报告，避免旧 Canvas 按新会话路由。
///
/// @author kongweiguang
fn emit_bound_mouse_event(
    pointer: &TerminalPointerContext<'_>,
    button: TerminalMouseButton,
    action: TerminalMouseAction,
    button_down: bool,
    cell: ViewportCell,
    modifiers: gpui_kit::Modifiers,
    cx: &mut App,
) {
    let (Some(input_handler), Some(snapshot)) = (pointer.input_handler, pointer.snapshot) else {
        return;
    };
    if input_handler.read(cx).binding().session_id != snapshot.session_id {
        return;
    }

    let event = TerminalInputEvent::Mouse(TerminalMouseEvent {
        button,
        action,
        button_down,
        column: cell.column,
        row: cell.row,
        modifiers: InputModifiers {
            shift: modifiers.shift,
            alt: modifiers.alt,
            control: modifiers.control,
            super_key: modifiers.platform,
        },
    });
    let _ = input_handler.update(cx, |handler, cx| handler.emit_input_event(event, cx));
}

/// 对启用 DEC mouse tracking 的终端优先经固定 pane handler 转发，保留 TUI 原生右键协议。
/// 普通终端才执行用户配置的焦点、粘贴或菜单策略，避免菜单与终端选择交互争抢事件。
/// 上下文只借用原有快照与输入绑定；按下阶段只记录清理意图，避免异步清除与后续拖选的回执乱序。
///
/// @author kongweiguang
fn handle_mouse_down(
    pointer: &TerminalPointerContext<'_>,
    tools: &Entity<TerminalToolsView>,
    view: &Entity<RootView>,
    right_click_behavior: TerminalRightClickBehavior,
    event: &MouseDownEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let TerminalPointerContext {
        snapshot,
        bounds,
        metrics,
        input_handler,
        ..
    } = *pointer;
    let Some(snapshot) = snapshot else {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        return;
    };
    let binding =
        pointer_binding(pointer, cx).filter(|binding| binding.session_id == snapshot.session_id);
    tools.update(cx, |tools, tools_cx| {
        tools.cancel_canvas_interaction_for_other_binding(binding.as_ref(), tools_cx)
    });

    if event.button == MouseButton::Left
        && let (Some(binding), Some(bounds)) = (binding.as_ref(), cached_canvas_bounds(bounds))
        && let Some(layout) = terminal_scroll_control_layout(bounds, snapshot, metrics)
        && let Some(hit) = scroll_control_hit(layout, event.position)
    {
        match hit {
            TerminalScrollControlHit::ReturnToBottom => {
                let _ = view.update(cx, |root, cx| {
                    root.handle_terminal_viewport_scroll(binding.clone(), 0, cx)
                });
            }
            TerminalScrollControlHit::Thumb => {
                let _ = tools.update(cx, |tools, _| {
                    tools.begin_terminal_scrollbar_drag(
                        binding.clone(),
                        event.position.y.as_f32(),
                        snapshot.scrollback_offset,
                        layout.max_offset,
                        layout.thumb_travel,
                    )
                });
            }
            TerminalScrollControlHit::Track => {
                if let Some(offset) =
                    scrollbar_offset_for_track_click(layout, event.position.y.as_f32())
                {
                    let _ = view.update(cx, |root, cx| {
                        root.handle_terminal_viewport_scroll(binding.clone(), offset, cx)
                    });
                }
            }
        }
        cx.stop_propagation();
        return;
    }

    let click_action = right_click_action(right_click_behavior);
    let shift_forces_local_selection = event.button == MouseButton::Left && event.modifiers.shift;
    if input_handler.is_none()
        && (event.button == MouseButton::Left
            || (event.button == MouseButton::Right
                && click_action == TerminalRightClickAction::Menu))
    {
        let Some(binding) = binding.as_ref() else {
            cx.stop_propagation();
            return;
        };
        let focused = view.update(cx, |root, cx| {
            root.focus_terminal_history_target(binding, window, cx)
        });
        if !focused {
            cx.stop_propagation();
            return;
        }
    }
    if mouse_down_should_focus_target(
        event.button,
        snapshot.modes.mouse_tracking != MouseTrackingMode::None,
        click_action,
    ) && let Some(input_handler) = input_handler
    {
        let handler_binding = input_handler.read(cx).binding().clone();
        if binding.as_ref() != Some(&handler_binding) {
            cx.stop_propagation();
            return;
        }
        let focused = view.update(cx, |root, cx| {
            root.focus_terminal_canvas_target(
                &handler_binding,
                input_handler.clone(),
                tools.clone(),
                window,
                cx,
            )
        });
        if !focused {
            cx.stop_propagation();
            return;
        }
    }
    if snapshot.modes.mouse_tracking != MouseTrackingMode::None && !shift_forces_local_selection {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        let Some(button) = terminal_mouse_button(event.button) else {
            return;
        };
        let (Some(bounds), cell_width) = (cached_canvas_bounds(bounds), metrics.cell_width(window))
        else {
            return;
        };
        let Some(cell) = pointer_cell(snapshot, bounds, metrics, event.position, cell_width) else {
            return;
        };
        emit_bound_mouse_event(
            pointer,
            button,
            TerminalMouseAction::Press,
            true,
            cell,
            event.modifiers,
            cx,
        );
        cx.stop_propagation();
        return;
    }

    if event.button == MouseButton::Right {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        let action = click_action;
        let handled = binding.as_ref().is_some_and(|binding| {
            let handler_binding_matches = input_handler
                .is_none_or(|input_handler| input_handler.read(cx).binding() == binding);
            handler_binding_matches
                && view.update(cx, |root, cx| {
                    root.handle_terminal_canvas_right_click(
                        binding.clone(),
                        input_handler.cloned(),
                        tools.clone(),
                        action,
                        window,
                        cx,
                    )
                })
        });
        // 只有目标仍有效且设置为 Menu 时保留冒泡，让 GPUI Kit 打开该 pane 的 popup。
        if action != TerminalRightClickAction::Menu || !handled {
            cx.stop_propagation();
        }
        return;
    }

    if event.button != MouseButton::Left {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        return;
    }
    let Some(binding) = binding else {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        cx.stop_propagation();
        return;
    };
    let Some(selection_kind) = selection_kind_for_gesture(event.click_count, event.modifiers.alt)
    else {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        cx.stop_propagation();
        return;
    };
    let (Some(bounds), cell_width) = (cached_canvas_bounds(bounds), metrics.cell_width(window))
    else {
        return;
    };
    let Some(cell) = pointer_cell(snapshot, bounds, metrics, event.position, cell_width) else {
        return;
    };
    let Some((start, viewport)) = terminal_point_and_viewport_from_cell(snapshot, cell) else {
        return;
    };
    let plain_single_left_click = event.button == MouseButton::Left
        && event.click_count == 1
        && !event.modifiers.alt
        && !event.modifiers.shift
        && !event.modifiers.control
        && !event.modifiers.platform;
    let clear_selection_on_click = should_defer_selection_clear_on_mouse_down(
        plain_single_left_click,
        snapshot.selection.is_some(),
    );
    let _ = tools.update(cx, |tools, tools_cx| {
        tools.begin_canvas_selection_gesture(
            binding,
            start,
            cell,
            selection_kind,
            snapshot.revision,
            viewport,
            event.modifiers.secondary(),
            clear_selection_on_click,
            tools_cx,
        )
    });
    cx.stop_propagation();
}

/// release 命中在途滚动时延后到真实 ACK 对应的快照 revision 再映射端点；普通空白单击仍在 release 核验后才清旧选择。
///
/// @author kongweiguang
fn handle_mouse_up(
    pointer: &TerminalPointerContext<'_>,
    tools: &Entity<TerminalToolsView>,
    view: &Entity<RootView>,
    event: &MouseUpEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let TerminalPointerContext {
        snapshot,
        bounds,
        metrics,
        ..
    } = *pointer;
    let Some(snapshot) = snapshot else {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        return;
    };
    let Some(binding) =
        pointer_binding(pointer, cx).filter(|binding| binding.session_id == snapshot.session_id)
    else {
        tools.update(cx, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        return;
    };
    tools.update(cx, |tools, tools_cx| {
        tools.cancel_canvas_interaction_for_other_binding(Some(&binding), tools_cx)
    });

    if event.button == MouseButton::Left
        && let Some(offset) = tools
            .read(cx)
            .update_terminal_scrollbar_drag(&binding, event.position.y.as_f32())
    {
        let _ = view.update(cx, |root, cx| {
            root.handle_terminal_viewport_scroll(binding.clone(), offset, cx)
        });
        tools.update(cx, |tools, _| {
            tools.finish_terminal_scrollbar_drag(&binding)
        });
        cx.stop_propagation();
        return;
    }

    if event.button == MouseButton::Left
        && let Some(token) = tools.read(cx).active_canvas_selection_gesture(&binding)
    {
        let Some(bounds) = cached_canvas_bounds(bounds) else {
            tools.update(cx, |tools, tools_cx| {
                tools.cancel_canvas_selection_gesture(tools_cx)
            });
            return;
        };
        let cell_width = metrics.cell_width(window);
        let plain_unmodified_release = event.click_count == 1
            && !event.modifiers.shift
            && !event.modifiers.control
            && !event.modifiers.alt
            && !event.modifiers.platform;
        let completed = view.update(cx, |root, cx| {
            let Some(snapshot) = root.terminal_snapshot_for_binding(&binding).cloned() else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return None;
            };
            let Some(cell) = pointer_cell(&snapshot, bounds, metrics, event.position, cell_width)
            else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return None;
            };
            let pending_scroll = tools
                .read(cx)
                .canvas_selection_gesture_frame(&token)
                .and_then(|frame| frame.pending_scroll);
            if pending_scroll.is_some_and(|pending| !pending.is_acknowledged_by(snapshot.revision))
            {
                tools.update(cx, |tools, _| {
                    tools.defer_canvas_selection_release(
                        &token,
                        cell,
                        plain_unmodified_release,
                        event.modifiers.secondary(),
                        event.click_count,
                    )
                });
                return None;
            }
            let Some((end, viewport)) = terminal_point_and_viewport_from_cell(&snapshot, cell)
            else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return None;
            };
            let completed = tools.update(cx, |tools, cx| {
                tools.finish_canvas_selection_gesture(
                    &token,
                    end,
                    viewport,
                    plain_unmodified_release,
                    cx,
                )
            });
            completed.map(|completed| (completed, snapshot))
        });
        if let Some((completed, snapshot)) = completed
            && completed.selection.is_none()
            && allows_osc8_click(
                completed.open_link_on_click,
                event.modifiers.secondary(),
                event.click_count,
            )
            && let Some(link) = link_at_point(&snapshot.rows, completed.start)
        {
            let _ = tools.update(cx, |tools, cx| tools.emit_open_link(link.as_str(), cx));
        }
        cx.stop_propagation();
        return;
    }

    if snapshot.modes.mouse_tracking != MouseTrackingMode::None {
        let Some(button) = terminal_mouse_button(event.button) else {
            return;
        };
        let (Some(bounds), cell_width) = (cached_canvas_bounds(bounds), metrics.cell_width(window))
        else {
            return;
        };
        let Some(cell) = pointer_cell(snapshot, bounds, metrics, event.position, cell_width) else {
            return;
        };
        emit_bound_mouse_event(
            pointer,
            button,
            TerminalMouseAction::Release,
            false,
            cell,
            event.modifiers,
            cx,
        );
    }
}

/// 滚动 ACK 未到时只保存指针位置，避免旧 viewport 把选择端点映射到错误绝对行；其余 TUI move 仍校验 handler 和模式。
///
/// @author kongweiguang
fn handle_mouse_move(
    pointer: &TerminalPointerContext<'_>,
    tools: &Entity<TerminalToolsView>,
    view: &Entity<RootView>,
    event: &MouseMoveEvent,
    window: &mut Window,
    cx: &mut App,
) {
    let TerminalPointerContext {
        snapshot,
        bounds,
        metrics,
        ..
    } = *pointer;
    let Some(snapshot) = snapshot else { return };
    let binding =
        pointer_binding(pointer, cx).filter(|binding| binding.session_id == snapshot.session_id);
    tools.update(cx, |tools, tools_cx| {
        tools.cancel_canvas_interaction_for_other_binding(binding.as_ref(), tools_cx)
    });
    let Some(binding) = binding else { return };

    if event.pressed_button != Some(MouseButton::Left) {
        let active_gesture = tools.read(cx).active_canvas_selection_gesture(&binding);
        let release_is_waiting_for_scroll = active_gesture.as_ref().is_some_and(|token| {
            tools
                .read(cx)
                .canvas_selection_gesture_frame(token)
                .is_some_and(|frame| frame.pending_release.is_some())
        });
        if active_gesture.is_some() && !release_is_waiting_for_scroll {
            tools.update(cx, |tools, tools_cx| {
                tools.cancel_canvas_selection_gesture(tools_cx)
            });
        }
        tools.update(cx, |tools, _| {
            tools.finish_terminal_scrollbar_drag(&binding)
        });
    }

    if let Some(offset) = tools
        .read(cx)
        .update_terminal_scrollbar_drag(&binding, event.position.y.as_f32())
    {
        let _ = view.update(cx, |root, cx| {
            root.handle_terminal_viewport_scroll(binding.clone(), offset, cx)
        });
        cx.stop_propagation();
        return;
    }

    if let Some(token) = tools.read(cx).active_canvas_selection_gesture(&binding) {
        if tools
            .read(cx)
            .canvas_selection_gesture_frame(&token)
            .is_some_and(|frame| frame.pending_release.is_some())
        {
            cx.stop_propagation();
            return;
        }
        let Some(bounds) = cached_canvas_bounds(bounds) else {
            tools.update(cx, |tools, tools_cx| {
                tools.cancel_canvas_selection_gesture(tools_cx)
            });
            return;
        };
        let cell_width = metrics.cell_width(window);
        let now = Instant::now();
        let should_schedule = view.update(cx, |root, cx| {
            let Some(snapshot) = root.terminal_snapshot_for_binding(&binding).cloned() else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            };
            let Some(cell) = pointer_cell(&snapshot, bounds, metrics, event.position, cell_width)
            else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            };
            let edge_direction = selection_edge_direction(bounds, metrics, event.position);
            let pending_scroll = tools
                .read(cx)
                .canvas_selection_gesture_frame(&token)
                .and_then(|frame| frame.pending_scroll);
            let scroll_unresolved = pending_scroll
                .is_some_and(|pending| !pending.is_acknowledged_by(snapshot.revision));
            if scroll_unresolved
                && pending_scroll.is_some_and(|pending| {
                    now.saturating_duration_since(pending.requested_at)
                        >= SELECTION_SCROLL_ACK_TIMEOUT
                })
            {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            }
            tools.update(cx, |tools, cx| {
                let pointer_updated = if scroll_unresolved {
                    tools.update_canvas_selection_pointer(&token, cell, edge_direction)
                } else {
                    if pending_scroll.is_some() {
                        tools.clear_canvas_pending_scroll(&token);
                    }
                    if let Some((end, viewport)) =
                        terminal_point_and_viewport_from_cell(&snapshot, cell)
                    {
                        tools.update_canvas_selection_gesture(
                            &token,
                            end,
                            cell,
                            edge_direction,
                            viewport,
                            cx,
                        )
                    } else {
                        tools.cancel_canvas_selection_gesture(cx);
                        false
                    }
                };
                pointer_updated && tools.begin_canvas_autoscroll_schedule(&token)
            })
        });
        if should_schedule {
            schedule_selection_autoscroll(view.clone(), tools.clone(), token, window);
        }
        cx.stop_propagation();
        return;
    }

    if snapshot.modes.mouse_tracking == MouseTrackingMode::None {
        return;
    }
    let Some(bounds) = cached_canvas_bounds(bounds) else {
        return;
    };
    let cell_width = metrics.cell_width(window);
    let Some(cell) = pointer_cell(snapshot, bounds, metrics, event.position, cell_width) else {
        return;
    };
    let (button, button_down) = match event.pressed_button {
        Some(button) => {
            let Some(button) = terminal_mouse_button(button) else {
                return;
            };
            (button, true)
        }
        None => (TerminalMouseButton::Left, false),
    };
    emit_bound_mouse_event(
        pointer,
        button,
        TerminalMouseAction::Move,
        button_down,
        cell,
        event.modifiers,
        cx,
    );
}

/// 高刷显示器上也只保留一条动画链；每帧重新读 Root 的当前快照，不捕获旧 viewport rows。
///
/// @author kongweiguang
fn schedule_selection_autoscroll(
    view: Entity<RootView>,
    tools: Entity<TerminalToolsView>,
    token: TerminalSelectionGestureToken,
    window: &mut Window,
) {
    window.request_animation_frame();
    window.on_next_frame(move |window, app| {
        run_selection_autoscroll_frame(view, tools, token, window, app);
    });
}

/// 每条边缘滚动都等匹配请求 ACK 与最小快照 revision 后才映射端点；失焦、超时、release 或 owner 失效会终止该链。
///
/// @author kongweiguang
fn run_selection_autoscroll_frame(
    view: Entity<RootView>,
    tools: Entity<TerminalToolsView>,
    token: TerminalSelectionGestureToken,
    window: &mut Window,
    app: &mut App,
) {
    if !window.is_window_active() {
        tools.update(app, |tools, tools_cx| {
            tools.cancel_canvas_selection_gesture(tools_cx)
        });
        return;
    }
    let continue_scheduling = view.update(app, |root, cx| {
        let Some(frame) = tools.read(cx).canvas_selection_gesture_frame(&token) else {
            tools.update(cx, |tools, _| {
                tools.finish_canvas_autoscroll_schedule(&token)
            });
            return false;
        };
        if frame.edge_direction == 0 && frame.pending_scroll.is_none() {
            tools.update(cx, |tools, _| {
                tools.finish_canvas_autoscroll_schedule(&token)
            });
            return false;
        }
        let Some(snapshot) = root.terminal_snapshot_for_binding(token.binding()).cloned() else {
            tools.update(cx, |tools, tools_cx| {
                tools.cancel_canvas_selection_gesture(tools_cx)
            });
            return false;
        };
        let now = Instant::now();
        if let Some(pending) = frame.pending_scroll {
            if !pending.is_acknowledged_by(snapshot.revision) {
                if now.saturating_duration_since(pending.requested_at)
                    >= SELECTION_SCROLL_ACK_TIMEOUT
                {
                    // 丢失 ACK 时结束手势并保留最近已确认选择；不重复排队绝对滚动目标。
                    tools.update(cx, |tools, tools_cx| {
                        tools.cancel_canvas_selection_gesture(tools_cx)
                    });
                    return false;
                }
                return true;
            }
            tools.update(cx, |tools, _| tools.clear_canvas_pending_scroll(&token));
        }
        if let Some(release) = frame.pending_release {
            let Some((end, viewport)) =
                terminal_point_and_viewport_from_cell(&snapshot, release.cell)
            else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            };
            let completed = tools.update(cx, |tools, cx| {
                tools.finish_canvas_selection_gesture(
                    &token,
                    end,
                    viewport,
                    release.plain_unmodified_release,
                    cx,
                )
            });
            if let Some(completed) = completed
                && completed.selection.is_none()
                && allows_osc8_click(
                    completed.open_link_on_click,
                    release.modifier_is_held,
                    release.click_count,
                )
                && let Some(link) = link_at_point(&snapshot.rows, completed.start)
            {
                let _ = tools.update(cx, |tools, cx| tools.emit_open_link(link.as_str(), cx));
            }
            return false;
        }
        let Some((end, viewport)) = terminal_point_and_viewport_from_cell(&snapshot, frame.cell)
        else {
            tools.update(cx, |tools, tools_cx| {
                tools.cancel_canvas_selection_gesture(tools_cx)
            });
            return false;
        };
        let active = tools.update(cx, |tools, cx| {
            tools.update_canvas_selection_gesture(
                &token,
                end,
                frame.cell,
                frame.edge_direction,
                viewport,
                cx,
            )
        });
        if !active {
            tools.update(cx, |tools, _| {
                tools.finish_canvas_autoscroll_schedule(&token)
            });
            return false;
        }
        if frame.edge_direction == 0 {
            tools.update(cx, |tools, _| {
                tools.finish_canvas_autoscroll_schedule(&token)
            });
            return false;
        }
        if !tools.update(cx, |tools, _| {
            tools.should_autoscroll_canvas_gesture(&token, now)
        }) {
            return true;
        }
        let offset = if frame.edge_direction < 0 {
            snapshot
                .scrollback_offset
                .saturating_add(1)
                .min(snapshot.history_size)
        } else {
            snapshot.scrollback_offset.saturating_sub(1)
        };
        if offset != snapshot.scrollback_offset {
            let request_id = tools.update(cx, |tools, _| {
                tools.register_canvas_selection_scroll(
                    token.binding(),
                    token.gesture_id(),
                    offset,
                    now,
                )
            });
            let Some(request_id) = request_id else {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            };
            if !root.handle_terminal_selection_scroll(
                token.binding().clone(),
                offset,
                token.gesture_id(),
                request_id,
                cx,
            ) {
                tools.update(cx, |tools, tools_cx| {
                    tools.cancel_canvas_selection_gesture(tools_cx)
                });
                return false;
            }
        }
        true
    });
    if continue_scheduling {
        schedule_selection_autoscroll(view, tools, token, window);
    }
}

/// 依据同一帧网格补齐搜索、关键词和选择背景；沿用覆盖排序与整段矩形批次，不新增逐 cell 分配。
///
/// @author kongweiguang
fn collect_overlay_backgrounds(
    spans: &[CellOverlaySpan],
    row_origin: Point<Pixels>,
    grid: &TerminalPaintGrid<'_>,
    palette: &TerminalPalette,
    backgrounds: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    let TerminalPaintGrid {
        canvas_bounds,
        cell_width,
        line_height,
        inset,
        ..
    } = *grid;
    let right_edge = canvas_bounds.origin.x + canvas_bounds.size.width - inset;
    let bottom_edge = canvas_bounds.origin.y + canvas_bounds.size.height - inset;
    let mut spans = spans.to_vec();
    spans.sort_by_key(|span| (span.kind, style_sort_key(span.style)));
    for span in spans {
        let background = span.style.and_then(|style| style.background).or_else(|| {
            span.style
                .is_none()
                .then(|| default_overlay_colors(span.kind, palette).1)
        });
        let Some(background) = background else {
            continue;
        };
        let x = row_origin.x + cell_width * usize::from(span.start_column);
        let right = (row_origin.x + cell_width * usize::from(span.end_column)).min(right_edge);
        let bottom = (row_origin.y + line_height).min(bottom_edge);
        if right <= x || bottom <= row_origin.y {
            continue;
        }
        backgrounds.push((
            gpui_kit::bounds(
                point(x, row_origin.y),
                size(right - x, bottom - row_origin.y),
            ),
            terminal_rgb(background),
        ));
    }
}

/// 只为非默认或 inverse 后非透明背景绘制 rect；复用该帧网格裁剪，保留空格与宽字符尾列着色。
///
/// @author kongweiguang
fn collect_terminal_background(
    run: &TerminalRun,
    viewport_row: u16,
    visible_columns: u16,
    grid: &TerminalPaintGrid<'_>,
    palette: &TerminalPalette,
    backgrounds: &mut Vec<(Bounds<Pixels>, Hsla)>,
) {
    if run.start_column >= visible_columns {
        return;
    }
    let inverse = run.style.attributes.inverse;
    let background = if inverse {
        run.style.foreground
    } else {
        run.style.background
    };
    if !inverse && background == TerminalColor::DefaultBackground {
        return;
    }
    let columns = run.start_column..run.end_column.min(visible_columns);
    if let Some(clip) = cell_clip(grid, viewport_row, columns) {
        backgrounds.push((clip, terminal_rgb(palette.resolve(background))));
    }
}

/// 用该帧唯一网格将绘制范围夹在 inset、可见 cell 列和 Canvas 边界内，保持宽字与半列的原裁剪坐标。
///
/// @author kongweiguang
fn cell_clip(
    grid: &TerminalPaintGrid<'_>,
    row: u16,
    columns: Range<u16>,
) -> Option<Bounds<Pixels>> {
    let TerminalPaintGrid {
        canvas_bounds,
        metrics,
        cell_width,
        line_height,
        inset,
    } = *grid;
    if columns.end <= columns.start {
        return None;
    }
    let row_origin = metrics.cell_origin(canvas_bounds.origin, row, 0, cell_width);
    let left = metrics
        .cell_origin(canvas_bounds.origin, row, columns.start, cell_width)
        .x
        .max(canvas_bounds.origin.x + inset);
    let right = (metrics
        .cell_origin(canvas_bounds.origin, row, columns.end, cell_width)
        .x)
        .min(canvas_bounds.origin.x + canvas_bounds.size.width - inset);
    let top = row_origin.y.max(canvas_bounds.origin.y + inset);
    let bottom = (row_origin.y + line_height)
        .min(canvas_bounds.origin.y + canvas_bounds.size.height - inset);
    (right > left && bottom > top)
        .then(|| gpui_kit::bounds(point(left, top), size(right - left, bottom - top)))
}

/// 用文本哈希和 GPUI run 样式命中系统 layout cache，避免为未变化的字形重新 shape。
///
/// @author kongweiguang
fn shape_cached_line(
    window: &mut Window,
    text: &str,
    font_size: Pixels,
    runs: &[TextRun],
    force_width: Option<Pixels>,
) -> ShapedLine {
    let mut hasher = DefaultHasher::new();
    text.hash(&mut hasher);
    window.text_system().shape_line_by_hash(
        hasher.finish(),
        text.len(),
        font_size,
        runs,
        force_width,
        || SharedString::from(Arc::<str>::from(text)),
    )
}

/// 从同一终端样式和覆盖层创建 GPUI glyph run，隐藏属性最后应用以防高亮重新显示文本。
///
/// @author kongweiguang
fn text_run_for_segment(
    text_style: &TextStyle,
    byte_len: usize,
    terminal_style: kerminal_terminal::snapshot::TerminalStyle,
    paint: Option<CellOverlayPaint>,
    palette: &TerminalPalette,
) -> TextRun {
    let mut run = text_style.to_run(byte_len);
    apply_terminal_style(&mut run, terminal_style, palette);
    if let Some(paint) = paint {
        apply_overlay_paint(&mut run, paint, palette);
    }
    if terminal_style.attributes.hidden {
        hide_text(&mut run);
    }
    run
}

/// 合并连续 ASCII cell 的同色样式 run，让 GPUI 一次 shape 长文本并按等宽列数布局。
///
/// @author kongweiguang
fn append_text_segment(
    batch: &mut TextBatch,
    segment: &CellTextSegment,
    text_style: &TextStyle,
    terminal_style: kerminal_terminal::snapshot::TerminalStyle,
    palette: &TerminalPalette,
) {
    let length = segment.byte_range.len();
    if batch.last_paint == Some(segment.paint) {
        if let Some(last) = batch.text_runs.last_mut() {
            last.len += length;
        }
    } else {
        batch.text_runs.push(text_run_for_segment(
            text_style,
            length,
            terminal_style,
            segment.paint,
            palette,
        ));
        batch.last_paint = Some(segment.paint);
    }
    batch.byte_range.end = segment.byte_range.end;
    batch.columns.end = segment.column_range.end;
}

/// 结束连续 ASCII 批次并复用该帧网格；force_width 仍为单字形 cell advance，裁剪按整段列数计算且不复制文本。
///
/// @author kongweiguang
fn flush_ascii_batch(
    batch: Option<TextBatch>,
    source: &str,
    viewport_row: u16,
    grid: &TerminalPaintGrid<'_>,
    font_size: Pixels,
    window: &mut Window,
    painted_runs: &mut Vec<PaintedRun>,
) {
    let TerminalPaintGrid {
        canvas_bounds,
        metrics,
        cell_width,
        line_height,
        ..
    } = *grid;
    let Some(batch) = batch else {
        return;
    };
    let Some(clip) = cell_clip(grid, viewport_row, batch.columns.clone()) else {
        return;
    };
    let text = &source[batch.byte_range];
    let line = shape_cached_line(window, text, font_size, &batch.text_runs, Some(cell_width));
    let origin = metrics.cell_origin(
        canvas_bounds.origin,
        viewport_row,
        batch.columns.start,
        cell_width,
    );
    painted_runs.push(PaintedRun {
        origin,
        clip,
        line_height,
        line,
        #[cfg(feature = "acceptance-metrics")]
        echo_target: batch.echo_target,
    });
}

/// 组合文本整段带下划线，输入法当前候选选区额外反色显示以匹配系统 marked range。
///
/// @author kongweiguang
fn ime_preview_run(mut run: TextRun, foreground: Hsla, background: Option<Hsla>) -> TextRun {
    run.color = foreground;
    run.background_color = background;
    run.underline = Some(GpuiUnderlineStyle {
        thickness: px(1.0),
        color: Some(foreground),
        wavy: false,
    });
    run
}

/// 将 UTF-16 映射后的候选区再次收敛到 UTF-8 字符边界，阻止错误平台事件切开 grapheme 字节。
///
/// @author kongweiguang
fn utf8_safe_range(text: &str, range: Range<usize>) -> (usize, usize) {
    let floor_boundary = |mut index: usize| {
        index = index.min(text.len());
        while !text.is_char_boundary(index) {
            index -= 1;
        }
        index
    };
    let start = floor_boundary(range.start);
    let end = floor_boundary(range.end.max(range.start)).max(start);
    (start, end)
}

#[cfg(test)]
#[path = "../tests/unit/terminal_canvas_context_menu.rs"]
mod tests;

#[cfg(test)]
#[path = "../tests/unit/terminal_canvas_split_candidate_geometry.rs"]
mod split_candidate_geometry_tests;

#[cfg(all(test, feature = "acceptance-metrics"))]
#[path = "../tests/unit/terminal_canvas_echo.rs"]
mod terminal_echo_tests;
