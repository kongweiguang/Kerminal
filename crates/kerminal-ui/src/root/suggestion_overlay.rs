//! 终端建议的当前帧锚点和视觉让位策略。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, AvailableSpace, Bounds, ContentMask, Entity, Hsla, IntoElement, Pixels, Point,
    ShapedLine, SharedString, Size, TextAlign, Window, canvas, div, point, prelude::*, px, size,
};
use kerminal_terminal::snapshot::{RenderSnapshot, TerminalCursor};

use super::command_suggestions::TerminalCommandSuggestionMenu;
use crate::terminal_metrics::TerminalMetrics;

/// 滚动历史、选区或隐藏光标时不可从屏幕坐标推断当前输入位置。
pub(super) fn anchor_available(snapshot: &RenderSnapshot, has_selection: bool) -> bool {
    !has_selection
        && snapshot.selection.is_none()
        && snapshot.scrollback_offset == 0
        && !snapshot.modes.alternate_screen
        && snapshot.cursor.visible
        && usize::from(snapshot.cursor.row) < snapshot.rows.len()
        && snapshot.cursor.column < snapshot.size.columns()
}

/// 只检查光标后是否已有可见字符，以便让位给 Shell 自身的预测；绝不据此恢复命令正文。
pub(super) fn has_native_prediction(snapshot: &RenderSnapshot) -> bool {
    snapshot
        .rows
        .iter()
        .find(|row| row.viewport_row == snapshot.cursor.row)
        .is_some_and(|row| {
            row.runs
                .iter()
                .filter(|run| !run.style.attributes.hidden)
                .any(|run| {
                    run.cell_spans.iter().any(|span| {
                        span.column_range.end > snapshot.cursor.column
                            && run.text.get(span.byte_range.clone()).is_none_or(|text| {
                                text.chars().any(|character| !character.is_whitespace())
                            })
                    })
                })
        })
}

/// 保存实际提交给文字绘制器的参数，独立于只供观测的 UI 节点布局。
struct InlineGhostLine {
    origin: Point<Pixels>,
    clip: Bounds<Pixels>,
    line_height: Pixels,
    line: ShapedLine,
}

impl InlineGhostLine {
    /// ASCII 使用正文相同的 cell advance；宽字形自然 shaping，行高和原点都不经过 UI 像素取整。
    fn new(
        pane: Bounds<Pixels>,
        cursor: TerminalCursor,
        metrics: &TerminalMetrics,
        text: SharedString,
        color: Hsla,
        window: &mut Window,
    ) -> Self {
        let cell_width = metrics.cell_width(window);
        let (origin, clip) = inline_ghost_geometry(pane, cursor, metrics, cell_width);
        let mut style = metrics.text_style(window);
        style.color = color;
        let force_width = text.is_ascii().then_some(cell_width);
        let run = style.to_run(text.len());
        let line =
            window
                .text_system()
                .shape_line(text, px(metrics.font_size), &[run], force_width);
        Self {
            origin,
            clip,
            line_height: px(metrics.line_height),
            line,
        }
    }
}

/// 提示直接走正文的 ShapedLine 绘制路径；普通 Text 元素会取整行高和布局坐标，导致相邻文字上下错位。
/// 观测节点只标记裁剪区域，不能参与字形定位；提示始终单行裁剪，也不占用终端布局或输入焦点。
pub(super) fn inline_ghost(
    selector: SharedString,
    text: SharedString,
    cursor: TerminalCursor,
    metrics: TerminalMetrics,
    color: Hsla,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let ghost = InlineGhostLine::new(bounds, cursor, &metrics, text, color, window);
            let clip = ghost.clip;
            let mut marker = div()
                .id(selector.clone())
                .debug_selector(move || selector.to_string())
                .w(clip.size.width)
                .h(clip.size.height)
                .into_any_element();
            marker.layout_as_root(
                Size {
                    width: AvailableSpace::Definite(clip.size.width),
                    height: AvailableSpace::Definite(clip.size.height),
                },
                window,
                cx,
            );
            marker.prepaint_at(clip.origin, window, cx);
            (marker, ghost)
        },
        |_, (mut marker, ghost), window, cx| {
            marker.paint(window, cx);
            window.with_content_mask(Some(ContentMask { bounds: ghost.clip }), |window| {
                if let Err(error) = ghost.line.paint(
                    ghost.origin,
                    ghost.line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                ) {
                    tracing::warn!(%error, "failed to paint terminal inline suggestion");
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// 光标原点保留正文的完整精度；仅裁剪区域受 pane 边界约束，禁止为了贴边而移动文字基线。
fn inline_ghost_geometry(
    pane: Bounds<Pixels>,
    cursor: TerminalCursor,
    metrics: &TerminalMetrics,
    cell_width: Pixels,
) -> (Point<Pixels>, Bounds<Pixels>) {
    let origin = metrics.cell_origin(pane.origin, cursor.row, cursor.column, cell_width);
    let clip = Bounds::new(
        origin,
        size(
            (pane.right() - origin.x).max(px(0.)),
            px(metrics.line_height),
        ),
    )
    .intersect(&pane);
    (origin, clip)
}

/// 在终端 pane 内按真实光标放置菜单，空间不足时翻到上方并限制在 pane 内。
/// 覆盖层显式固定左上原点，不能沿用终端画布之后的静态位置，否则整层会偏移到 pane 下方。
pub(super) fn anchored_menu(
    menu: Entity<TerminalCommandSuggestionMenu>,
    cursor: TerminalCursor,
    metrics: TerminalMetrics,
) -> impl IntoElement {
    canvas(
        move |bounds, window, cx| {
            let mut content: AnyElement = div()
                .w(bounds.size.width.min(px(440.)))
                .max_h(bounds.size.height)
                .overflow_hidden()
                .child(menu)
                .into_any_element();
            let content_size = content.layout_as_root(
                Size {
                    width: AvailableSpace::Definite(bounds.size.width),
                    height: AvailableSpace::Definite(bounds.size.height),
                },
                window,
                cx,
            );
            let cell_width = metrics.cell_width(window);
            let cursor_origin =
                metrics.cell_origin(bounds.origin, cursor.row, cursor.column, cell_width);
            let position =
                menu_position(bounds, cursor_origin, px(metrics.line_height), content_size);
            content.prepaint_at(position, window, cx);
            content
        },
        |_, mut content, window, cx| content.paint(window, cx),
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

/// 几何计算使用当前帧测量值，确保缩放、分屏与窗口变化后不会复用过期坐标。
fn menu_position(
    pane: Bounds<Pixels>,
    cursor: Point<Pixels>,
    line_height: Pixels,
    menu: Size<Pixels>,
) -> Point<Pixels> {
    let left = pane.origin.x.as_f32();
    let top = pane.origin.y.as_f32();
    let right = left + pane.size.width.as_f32();
    let bottom = top + pane.size.height.as_f32();
    let width = menu.width.as_f32().min(pane.size.width.as_f32()).max(0.);
    let height = menu.height.as_f32().min(pane.size.height.as_f32()).max(0.);
    let x = cursor.x.as_f32().clamp(left, (right - width).max(left));
    let below = cursor.y.as_f32() + line_height.as_f32();
    let y = if below + height <= bottom {
        below
    } else {
        cursor.y.as_f32() - height
    }
    .clamp(top, (bottom - height).max(top));
    point(px(x), px(y))
}

#[cfg(test)]
#[path = "../../tests/unit/root_suggestion_overlay.rs"]
mod tests;
