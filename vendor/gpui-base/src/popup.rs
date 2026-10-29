// @author kongweiguang

use std::{cell::Cell, rc::Rc};

use gpui::{
    Anchor, AnyElement, App, Bounds, Div, ElementId, InteractiveElement, Interactivity,
    IntoElement, ParentElement, Pixels, Point, RenderOnce, StatefulInteractiveElement,
    StyleRefinement, Styled, Window, canvas, deferred, div, point, px,
};

use crate::{
    Align, Placement, Positioner, ResolvedPosition, StyledExt as _,
    positioner::{frame_insets, vertical_side_space},
};

/// Distance kept between a popup and the window edge.
const WINDOW_MARGIN: Pixels = px(8.);
/// Horizontal room reserved for the popup to stay clear of both viewport edges.
const TRIGGER_WIDTH_VIEWPORT_INSET: Pixels = px(16.);

/// Deferred paint priority for interactive surfaces that must appear above dialogs.
pub const POPUP_PRIORITY: usize = 100;

#[derive(Default)]
struct PopupAnchorState {
    bounds: Bounds<Pixels>,
    viewport_width: Option<Pixels>,
    captured: bool,
}

/// An unstyled trigger and anchored popup host.
///
/// `Popup` owns trigger measurement, anchor-point calculation, first-frame
/// synchronization, deferred rendering, and window-edge snapping. Callers own
/// open state, interaction, popup content, appearance, and motion.
#[derive(IntoElement)]
pub struct Popup {
    id: ElementId,
    base: gpui::Stateful<Div>,
    style: StyleRefinement,
    anchor: Anchor,
    offset: Pixels,
    auto_flip: bool,
    trigger_width: Option<(Pixels, Pixels)>,
    available_height: Option<(Pixels, Pixels)>,
    on_position: Option<Box<dyn Fn(ResolvedPosition, Bounds<Pixels>)>>,
    trigger: AnyElement,
    content: Option<AnyElement>,
}

impl Popup {
    /// 初始化保持 corner 定位；side 翻转只由调用方显式启用。
    pub fn new(id: impl Into<ElementId>, trigger: impl IntoElement) -> Self {
        let id = id.into();
        Self {
            base: div().id(id.clone()),
            id,
            style: StyleRefinement::default(),
            anchor: Anchor::TopLeft,
            offset: px(0.),
            auto_flip: false,
            trigger_width: None,
            available_height: None,
            on_position: None,
            trigger: trigger.into_any_element(),
            content: None,
        }
    }

    pub fn anchor(mut self, anchor: impl Into<Anchor>) -> Self {
        self.anchor = anchor.into();
        self
    }

    /// Gap from the trigger along the anchor's outward direction, zero by default.
    pub fn offset(mut self, offset: Pixels) -> Self {
        self.offset = offset;
        self
    }

    /// 在首选边空间不足时翻转到触发器另一侧，并保留 anchor 的对齐方式。
    pub fn auto_flip(mut self) -> Self {
        self.auto_flip = true;
        self
    }

    /// Constrain content width to the trigger width clamped to `[min, max]`, then the viewport minus 16px.
    pub fn trigger_width(mut self, min: Pixels, max: Pixels) -> Self {
        self.trigger_width = Some((min, max));
        self
    }

    /// 在垂直锚点菜单中按触发器所在边计算首帧最大高度，避免内容先溢出再调整。
    pub fn available_height(mut self, minimum_space: Pixels, max_height: Pixels) -> Self {
        self.auto_flip = true;
        self.available_height = Some((minimum_space.max(px(0.)), max_height.max(px(0.))));
        self
    }

    /// Observe resolved popup and trigger bounds before content prepaint.
    pub fn on_position(
        mut self,
        callback: impl Fn(ResolvedPosition, Bounds<Pixels>) + 'static,
    ) -> Self {
        self.on_position = Some(Box::new(callback));
        self
    }

    pub fn content(mut self, content: impl IntoElement) -> Self {
        self.content = Some(content.into_any_element());
        self
    }

    pub fn resolved_corner(anchor: Anchor, trigger_bounds: Bounds<Pixels>) -> Point<Pixels> {
        match anchor {
            Anchor::TopLeft => trigger_bounds.origin,
            Anchor::TopCenter => trigger_bounds.top_center(),
            Anchor::TopRight => trigger_bounds.top_right(),
            Anchor::BottomLeft => Point {
                x: trigger_bounds.origin.x,
                y: trigger_bounds.origin.y - trigger_bounds.size.height,
            },
            Anchor::BottomCenter => Point {
                x: trigger_bounds.top_center().x,
                y: trigger_bounds.origin.y - trigger_bounds.size.height,
            },
            Anchor::BottomRight => Point {
                x: trigger_bounds.top_right().x,
                y: trigger_bounds.origin.y - trigger_bounds.size.height,
            },
            Anchor::LeftCenter | Anchor::RightCenter => trigger_bounds.origin,
        }
    }
}

/// Match the popup's anchor to the opposite edge of the measured trigger.
fn anchor_position(anchor: Anchor, trigger: Bounds<Pixels>, offset: Pixels) -> Point<Pixels> {
    match anchor {
        Anchor::TopLeft => trigger.bottom_left() + point(px(0.), offset),
        Anchor::TopCenter => trigger.bottom_center() + point(px(0.), offset),
        Anchor::TopRight => trigger.bottom_right() + point(px(0.), offset),
        Anchor::BottomLeft => trigger.origin - point(px(0.), offset),
        Anchor::BottomCenter => trigger.top_center() - point(px(0.), offset),
        Anchor::BottomRight => trigger.top_right() - point(px(0.), offset),
        Anchor::LeftCenter => trigger.right_center() + point(offset, px(0.)),
        Anchor::RightCenter => trigger.left_center() - point(offset, px(0.)),
    }
}

/// 将 legacy anchor 解释为 side 定位首选边和沿边对齐，不改变默认 corner 路径。
fn side_preferences(anchor: Anchor) -> (Placement, Align) {
    match anchor {
        Anchor::TopLeft => (Placement::Bottom, Align::Start),
        Anchor::TopCenter => (Placement::Bottom, Align::Center),
        Anchor::TopRight => (Placement::Bottom, Align::End),
        Anchor::BottomLeft => (Placement::Top, Align::Start),
        Anchor::BottomCenter => (Placement::Top, Align::Center),
        Anchor::BottomRight => (Placement::Top, Align::End),
        Anchor::LeftCenter => (Placement::Right, Align::Center),
        Anchor::RightCenter => (Placement::Left, Align::Center),
    }
}

/// Resolve the opt-in trigger width while letting the viewport cap win for narrow windows.
fn resolve_trigger_width(
    trigger_width: Pixels,
    viewport_width: Pixels,
    min_width: Pixels,
    max_width: Pixels,
) -> Pixels {
    let (min_width, max_width) = (min_width.min(max_width), min_width.max(max_width));
    let preferred = trigger_width.max(min_width).min(max_width);
    preferred.min((viewport_width - TRIGGER_WIDTH_VIEWPORT_INSET).max(px(0.)))
}

impl Styled for Popup {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl InteractiveElement for Popup {
    fn interactivity(&mut self) -> &mut Interactivity {
        self.base.interactivity()
    }
}

impl StatefulInteractiveElement for Popup {}

impl RenderOnce for Popup {
    /// 在默认角定位和显式 side 翻转间分流，并在首帧按共享边缘规则限制长菜单。
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let state =
            window.use_keyed_state((self.id, "anchor"), cx, |_, _| PopupAnchorState::default());
        let anchor = self.anchor;
        let auto_flip = self.auto_flip;
        let track_trigger_width = self.trigger_width.is_some() && self.content.is_some();
        let available_height = self.available_height;
        let trigger_bounds = Rc::new(Cell::new(state.read(cx).bounds));
        let offset = self.offset;
        let position = Rc::new(Cell::new(anchor_position(
            anchor,
            state.read(cx).bounds,
            offset,
        )));

        let root = self
            .base
            .child(self.trigger)
            .child(
                canvas(
                    {
                        let state = state.clone();
                        let position = position.clone();
                        let trigger_bounds = trigger_bounds.clone();
                        let track_trigger_width = track_trigger_width;
                        move |bounds, window, cx| {
                            trigger_bounds.set(bounds);
                            position.set(anchor_position(anchor, bounds, offset));
                            let viewport_width = window.viewport_size().width;
                            let (first, geometry_changed) = state.update(cx, |state, _| {
                                let first = !state.captured;
                                let geometry_changed = state.bounds != bounds
                                    || state.viewport_width != Some(viewport_width);
                                state.bounds = bounds;
                                state.viewport_width = Some(viewport_width);
                                state.captured = true;
                                (first, geometry_changed)
                            });
                            if first || (track_trigger_width && geometry_changed) {
                                window.request_animation_frame();
                            }
                        }
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .size_full()
                .top_0()
                .left_0(),
            )
            .refine_style(&self.style);

        let Some(content) = self.content else {
            return root;
        };
        if !state.read(cx).captured {
            return root;
        }

        let content = if let Some((min, max)) = self.trigger_width {
            let width = resolve_trigger_width(
                state.read(cx).bounds.size.width,
                window.viewport_size().width,
                min,
                max,
            );
            div().flex_col().w(width).child(content).into_any_element()
        } else {
            content
        };

        let minimum_available_space = if auto_flip {
            available_height.and_then(|(minimum_space, max_height)| {
                let frame = frame_insets(
                    window.window_decorations(),
                    window.client_inset().unwrap_or(px(0.)),
                );
                let margin = frame.map(|inset| *inset + WINDOW_MARGIN);
                let (preferred, _) = side_preferences(anchor);
                let (_, available) = vertical_side_space(
                    trigger_bounds.get(),
                    window.viewport_size(),
                    margin,
                    preferred,
                    offset,
                    minimum_space,
                )?;
                let height = available.min(max_height);
                Some((minimum_space, height))
            })
        } else {
            None
        };
        let content = if let Some((_, height)) = minimum_available_space {
            div()
                .flex_col()
                .max_h(height)
                .child(content)
                .into_any_element()
        } else {
            content
        };

        let positioner = if auto_flip {
            let (placement, align) = side_preferences(anchor);
            let positioner = Positioner::side(trigger_bounds.get())
                .placement(placement)
                .align(align)
                .offset(offset)
                .tracked_side_trigger_bounds(trigger_bounds.clone());
            if let Some((minimum, _)) = minimum_available_space {
                positioner.minimum_available_space(minimum)
            } else {
                positioner
            }
        } else {
            Positioner::corner(anchor, position.get()).tracked_corner_position(position)
        };
        let positioner = if let Some(callback) = self.on_position {
            positioner.on_position(move |position| callback(position, trigger_bounds.get()))
        } else {
            positioner
        };
        root.child(
            deferred(
                positioner
                    .margin(WINDOW_MARGIN)
                    // The host blocks the mouse, so no caller has to remember:
                    // what a popup covers belongs to the popup.
                    .occlude()
                    .child(content),
            )
            .with_priority(POPUP_PRIORITY),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, Render, px};

    /// Height constraints opt into side placement and clamp invalid sizes at the behavior boundary.
    #[test]
    fn available_height_enables_side_placement_and_clamps_sizes() {
        let popup = Popup::new("height", div()).available_height(px(-1.), px(320.));

        assert!(popup.auto_flip);
        assert_eq!(popup.available_height, Some((px(0.), px(320.))));
    }

    #[test]
    fn resolved_corner_preserves_existing_anchor_math() {
        let bounds = Bounds {
            origin: Point::new(px(100.), px(100.)),
            size: gpui::Size::new(px(200.), px(50.)),
        };
        assert_eq!(
            Popup::resolved_corner(Anchor::TopCenter, bounds),
            Point::new(px(200.), px(100.))
        );
        assert_eq!(
            Popup::resolved_corner(Anchor::BottomRight, bounds),
            Point::new(px(300.), px(50.))
        );
    }

    /// 锚点映射必须先保留原始 preferred side 与沿边对齐，再交由 side resolver 决定是否翻转。
    #[test]
    fn side_preferences_preserve_anchor_side_and_alignment() {
        assert_eq!(
            side_preferences(Anchor::TopLeft),
            (Placement::Bottom, Align::Start)
        );
        assert_eq!(
            side_preferences(Anchor::TopCenter),
            (Placement::Bottom, Align::Center)
        );
        assert_eq!(
            side_preferences(Anchor::TopRight),
            (Placement::Bottom, Align::End)
        );
        assert_eq!(
            side_preferences(Anchor::BottomLeft),
            (Placement::Top, Align::Start)
        );
        assert_eq!(
            side_preferences(Anchor::BottomCenter),
            (Placement::Top, Align::Center)
        );
        assert_eq!(
            side_preferences(Anchor::BottomRight),
            (Placement::Top, Align::End)
        );
        assert_eq!(
            side_preferences(Anchor::LeftCenter),
            (Placement::Right, Align::Center)
        );
        assert_eq!(
            side_preferences(Anchor::RightCenter),
            (Placement::Left, Align::Center)
        );
    }

    /// Width follows the trigger within its configured range, while the viewport always wins.
    #[test]
    fn trigger_width_clamps_trigger_and_viewport_sizes() {
        assert_eq!(
            resolve_trigger_width(px(80.), px(800.), px(240.), px(360.)),
            px(240.)
        );
        assert_eq!(
            resolve_trigger_width(px(300.), px(800.), px(240.), px(360.)),
            px(300.)
        );
        assert_eq!(
            resolve_trigger_width(px(500.), px(800.), px(240.), px(360.)),
            px(360.)
        );
        assert_eq!(
            resolve_trigger_width(px(300.), px(280.), px(240.), px(360.)),
            px(264.)
        );
        assert_eq!(
            resolve_trigger_width(px(300.), px(800.), px(360.), px(240.)),
            px(300.)
        );
    }

    struct Harness;

    impl Render for Harness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            Popup::new(
                "popup",
                div()
                    .debug_selector(|| "popup-trigger".into())
                    .size(px(100.)),
            )
            .content(
                div()
                    .debug_selector(|| "popup-content".into())
                    .size(px(20.)),
            )
        }
    }

    struct AutoFlipHarness {
        origin: Point<Pixels>,
        trigger_width: Pixels,
        resolved: Rc<Cell<Option<Placement>>>,
    }

    impl Render for AutoFlipHarness {
        /// 固定触发器起点以覆盖边缘翻转，并允许测试在布局重排后移动同一实例。
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let resolved = self.resolved.clone();
            div().size_full().child(
                div()
                    .absolute()
                    .left(self.origin.x)
                    .top(self.origin.y)
                    .child(
                        Popup::new(
                            "auto-flip-popup",
                            div()
                                .debug_selector(|| "auto-flip-trigger".into())
                                .w(self.trigger_width)
                                .h(px(40.)),
                        )
                        .anchor(Anchor::BottomLeft)
                        .auto_flip()
                        .trigger_width(px(240.), px(360.))
                        .on_position(move |position, _| resolved.set(position.placement))
                        .content(
                            div()
                                .debug_selector(|| "auto-flip-content".into())
                                .w_full()
                                .h(px(20.)),
                        ),
                    ),
            )
        }
    }

    /// A caller that styles its own surface — a hover card, a dropdown — does
    /// not have to remember to block the mouse. The host does it, so the panel
    /// a popup covers stops reacting to a pointer that is over the popup.
    struct OcclusionHarness {
        background_hovered: Rc<Cell<bool>>,
        content_hovered: Rc<Cell<bool>>,
    }

    impl Render for OcclusionHarness {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let background = self.background_hovered.clone();
            let content = self.content_hovered.clone();
            div()
                .relative()
                .size(px(200.))
                .child(
                    div()
                        .id("background")
                        .absolute()
                        .size_full()
                        .on_mouse_move(move |_, _, _| background.set(true)),
                )
                .child(
                    Popup::new("popup", div().size(px(100.))).content(
                        div()
                            .id("content")
                            .size(px(40.))
                            .on_mouse_move(move |_, _, _| content.set(true)),
                    ),
                )
        }
    }

    #[gpui::test]
    fn the_popup_surface_blocks_the_panel_it_covers(cx: &mut gpui::TestAppContext) {
        let background_hovered = Rc::new(Cell::new(false));
        let content_hovered = Rc::new(Cell::new(false));
        let (_, window) = cx.add_window_view({
            let background_hovered = background_hovered.clone();
            let content_hovered = content_hovered.clone();
            move |_, _| OcclusionHarness {
                background_hovered,
                content_hovered,
            }
        });
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));

        window.simulate_mouse_move(
            gpui::point(px(20.), px(110.)),
            None,
            gpui::Modifiers::default(),
        );
        assert!(!background_hovered.get());
        // The surface blocks what is behind it, not its own content: the
        // hitbox goes in ahead of the children, never over them.
        assert!(content_hovered.get());

        // The same pointer outside the surface still reaches the panel, so the
        // assertion above is about occlusion and not a dead listener.
        window.simulate_mouse_move(
            gpui::point(px(150.), px(180.)),
            None,
            gpui::Modifiers::default(),
        );
        assert!(background_hovered.get());
    }

    #[gpui::test]
    fn trigger_capture_enables_deferred_content_on_the_next_frame(cx: &mut gpui::TestAppContext) {
        let (_, window) = cx.add_window_view(|_, _| Harness);
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));

        assert_eq!(
            window.debug_bounds("popup-trigger").unwrap().size,
            gpui::Size::new(px(100.), px(100.))
        );
        assert_eq!(
            window.debug_bounds("popup-content").unwrap().size,
            gpui::Size::new(px(20.), px(20.))
        );
    }

    /// 验证边缘翻转使用真实可用空间，且 resize/reflow 后定位读取当前帧 bounds。
    #[gpui::test]
    fn auto_flip_uses_available_space_and_tracks_trigger_reflow(cx: &mut gpui::TestAppContext) {
        let resolved = Rc::new(Cell::new(None));
        let (view, window) = cx.add_window_view({
            let resolved = resolved.clone();
            move |_, _| AutoFlipHarness {
                origin: point(px(100.), px(8.)),
                trigger_width: px(40.),
                resolved,
            }
        });
        window.update(|window, cx| window.draw(cx).clear(cx));
        window.update(|window, cx| window.draw(cx).clear(cx));

        assert_eq!(resolved.get(), Some(Placement::Bottom));
        let trigger = window.debug_bounds("auto-flip-trigger").unwrap();
        let content = window.debug_bounds("auto-flip-content").unwrap();
        assert!(content.top() >= trigger.bottom());
        assert_eq!(content.size.width, px(240.));

        window.update(|window, cx| {
            view.update(cx, |view, cx| {
                view.origin = point(px(120.), px(300.));
                view.trigger_width = px(300.);
                cx.notify();
            });
            window.draw(cx).clear(cx);
        });
        window.update(|window, cx| window.draw(cx).clear(cx));

        assert_eq!(resolved.get(), Some(Placement::Top));
        let content = window.debug_bounds("auto-flip-content").unwrap();
        assert_eq!(content.size.width, px(300.));
    }
}
