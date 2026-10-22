//! 工作台分隔槽只负责绘制反馈，命中、拖动、尺寸限制与持久化仍由 Kit 持有。
//!
//! @author kongweiguang

use std::rc::Rc;

use gpui_kit::{
    Axis, Bounds, BoxShadow, Hsla, IntoElement as _, MouseMoveEvent, Pixels, Point, Window,
    base::{ResizeHandleRenderer, ResizeHandleState},
    canvas, div, fill, linear_color_stop, linear_gradient, point,
    prelude::*,
    px, size,
};

/// 空闲返回透明元素而非 None，避免 Kit 回退到常驻直线；不另建鼠标状态或修改原拖动命中区。
/// 焦点默认行为由 Kit 基于真实顶层 hitbox 处理，避免这里的绘制器猜测遮挡关系。
/// 光晕按普通树顺序绘制，保留父层裁剪，让菜单和弹层自然覆盖分隔槽。
///
/// @author kongweiguang
pub(super) fn appearance(accent: Hsla) -> ResizeHandleRenderer {
    Rc::new(move |handle, _, _| {
        if handle.state() == ResizeHandleState::Idle {
            return Some(div().size_full().into_any_element());
        }
        let axis = handle.axis();
        let pressed = handle.is_active();
        Some(
            canvas(
                |_, _, _| (),
                move |bounds, (), window, _| {
                    paint_glow(bounds, axis, accent, pressed, window);
                    // Kit 只在 hover 状态变化时重绘；局部光晕还需随槽内鼠标移动更新位置。
                    window.on_mouse_event(|_: &MouseMoveEvent, phase, window, _| {
                        if phase.bubble() {
                            window.refresh();
                        }
                    });
                },
            )
            .size_full()
            .into_any_element(),
        )
    })
}

/// 将反馈限制在指针附近，短窗口和边缘位置均裁到槽内；跨出命中带的拖动仍沿原轴显示。
fn glow_bounds(bounds: Bounds<Pixels>, axis: Axis, pointer: Point<Pixels>) -> Bounds<Pixels> {
    let half_length = px(72.);
    match axis {
        Axis::Horizontal => {
            let center = pointer.y.clamp(bounds.top(), bounds.bottom());
            let top = (center - half_length).max(bounds.top());
            let bottom = (center + half_length).min(bounds.bottom());
            Bounds::new(
                point(bounds.center().x - px(1.), top),
                size(px(2.), bottom - top),
            )
        }
        Axis::Vertical => {
            let center = pointer.x.clamp(bounds.left(), bounds.right());
            let left = (center - half_length).max(bounds.left());
            let right = (center + half_length).min(bounds.right());
            Bounds::new(
                point(left, bounds.center().y - px(1.)),
                size(right - left, px(2.)),
            )
        }
    }
}

/// 用原生阴影和两段渐变形成局部柔光；只在交互帧绘制，不使用定时器或全高高亮。
fn paint_glow(
    bounds: Bounds<Pixels>,
    axis: Axis,
    accent: Hsla,
    pressed: bool,
    window: &mut Window,
) {
    let glow = glow_bounds(bounds, axis, window.mouse_position());
    let alpha = if pressed { 0.65 } else { 0.42 };
    window.paint_drop_shadows(
        glow,
        px(1.).into(),
        &[BoxShadow::new(px(0.), px(0.), accent.opacity(alpha))
            .blur_radius(px(18.))
            .spread_radius(px(3.))],
    );
    let transparent = linear_color_stop(accent.opacity(0.), 0.);
    let strong = linear_color_stop(accent.opacity(if pressed { 1. } else { 0.8 }), 1.);
    let (first, second, angle) = match axis {
        Axis::Horizontal => {
            let half = glow.size.height / 2.;
            (
                Bounds::new(glow.origin, size(glow.size.width, half)),
                Bounds::new(
                    glow.origin + point(px(0.), half),
                    size(glow.size.width, half),
                ),
                180.,
            )
        }
        Axis::Vertical => {
            let half = glow.size.width / 2.;
            (
                Bounds::new(glow.origin, size(half, glow.size.height)),
                Bounds::new(
                    glow.origin + point(half, px(0.)),
                    size(half, glow.size.height),
                ),
                90.,
            )
        }
    };
    window.paint_quad(fill(first, linear_gradient(angle, transparent, strong)));
    window.paint_quad(fill(
        second,
        linear_gradient(angle + 180., transparent, strong),
    ));
}

#[cfg(test)]
#[path = "../../tests/unit/workbench_resize.rs"]
mod tests;
