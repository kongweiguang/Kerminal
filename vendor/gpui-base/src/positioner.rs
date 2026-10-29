//! Shared popup positioning.
//!
//! Every anchored surface in Base resolves its position here so that flipping,
//! alignment, and viewport clamping cannot drift apart between popups,
//! tooltips, and menus.
//! @author kongweiguang

use gpui::{
    Anchor, AnyElement, App, Bounds, Decorations, Display, Edges, Element, GlobalElementId,
    Half as _, HitboxBehavior, InspectorElementId, IntoElement, LayoutId, ParentElement, Pixels,
    Point, Position, Size, Style, Window, point, px,
};

use crate::Placement;
use std::{cell::Cell, rc::Rc};

/// Alignment of a popup along the side it is placed on.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Align {
    /// Align with the trigger's leading edge.
    Start,
    /// Center on the trigger.
    #[default]
    Center,
    /// Align with the trigger's trailing edge.
    End,
}

/// How a popup derives its position.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Strategy {
    /// Place `anchor`'s corner of the popup at `position`, then clamp into the
    /// viewport. This reproduces GPUI's `anchored` corner behavior and does not
    /// flip to the opposite side.
    Corner {
        anchor: Anchor,
        position: Point<Pixels>,
    },
    /// Center the popup at the requested point, clamping its bounds to the viewport margin.
    Center { position: Point<Pixels> },
    /// Place the popup on `placement`'s side of the trigger, flipping to the
    /// opposite side when it does not fit, then clamp into the viewport.
    Side {
        trigger_bounds: Bounds<Pixels>,
        placement: Option<Placement>,
        align: Align,
        offset: Pixels,
    },
}

/// The bounds a popup resolved to, and the side it ended up on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResolvedPosition {
    /// Final popup bounds in window coordinates.
    pub bounds: Bounds<Pixels>,
    /// The side the popup was placed on. `None` for corner and center positioning,
    /// which have no notion of a side.
    pub placement: Option<Placement>,
}

/// An unstyled element that positions its children as an anchored popup.
///
/// This owns measurement, side selection, alignment, and viewport clamping. It
/// installs no presentation of its own and adds no wrapper element around its
/// children.
pub struct Positioner {
    strategy: Strategy,
    corner_position: Option<Rc<Cell<Point<Pixels>>>>,
    side_trigger_bounds: Option<Rc<Cell<Bounds<Pixels>>>>,
    minimum_available_space: Option<Pixels>,
    on_position: Option<Box<dyn Fn(ResolvedPosition)>>,
    margin: Pixels,
    occlude: bool,
    children: Vec<AnyElement>,
}

#[doc(hidden)]
pub struct PositionerState {
    child_layout_ids: Vec<LayoutId>,
}

impl Positioner {
    /// Places the popup on a side of `trigger_bounds`, flipping when needed.
    ///
    /// 以触发器测量值创建 side 策略；触发器会在布局期变化时由跟踪单元格刷新。
    pub fn side(trigger_bounds: Bounds<Pixels>) -> Self {
        Self {
            strategy: Strategy::Side {
                trigger_bounds,
                placement: None,
                align: Align::Center,
                offset: px(0.),
            },
            corner_position: None,
            side_trigger_bounds: None,
            minimum_available_space: None,
            on_position: None,
            margin: px(4.),
            occlude: false,
            children: Vec::new(),
        }
    }

    /// Places `anchor`'s corner of the popup at `position`.
    ///
    /// This is the corner-anchoring path used by triggers that were written
    /// against GPUI's `anchored` element. It clamps into the viewport without
    /// changing the requested anchor.
    ///
    /// 该路径保持 legacy corner 语义，不与 side 翻转混用。
    pub fn corner(anchor: Anchor, position: Point<Pixels>) -> Self {
        Self {
            strategy: Strategy::Corner { anchor, position },
            corner_position: None,
            side_trigger_bounds: None,
            minimum_available_space: None,
            on_position: None,
            margin: px(4.),
            occlude: false,
            children: Vec::new(),
        }
    }

    /// Centers the measured popup on a point in window coordinates.
    ///
    /// The child is measured each frame so its actual size, rather than a
    /// guessed size, determines the popup origin.
    pub fn center(position: Point<Pixels>) -> Self {
        Self {
            strategy: Strategy::Center { position },
            corner_position: None,
            side_trigger_bounds: None,
            minimum_available_space: None,
            on_position: None,
            margin: px(4.),
            occlude: false,
            children: Vec::new(),
        }
    }

    /// Updates the requested corner or center point; side positioning is unchanged.
    ///
    /// This is useful when an animation moves an already-composed popup.
    pub fn position(mut self, position: Point<Pixels>) -> Self {
        match &mut self.strategy {
            Strategy::Corner {
                position: current, ..
            }
            | Strategy::Center { position: current } => *current = position,
            Strategy::Side { .. } => {}
        }
        self
    }

    /// Sets the preferred side. Only meaningful for [`Positioner::side`].
    pub fn placement(mut self, placement: Placement) -> Self {
        if let Strategy::Side {
            placement: slot, ..
        } = &mut self.strategy
        {
            *slot = Some(placement);
        }
        self
    }

    /// 保留边缘菜单的最小可用空间规则，同时让常规弹层继续按内容高度翻转。
    pub fn minimum_available_space(mut self, height: Pixels) -> Self {
        self.minimum_available_space = Some(height.max(px(0.)));
        self
    }

    /// Sets the alignment along the chosen side. Only meaningful for
    /// [`Positioner::side`].
    pub fn align(mut self, align: Align) -> Self {
        if let Strategy::Side { align: slot, .. } = &mut self.strategy {
            *slot = align;
        }
        self
    }

    /// Sets the gap between the trigger and the popup. Only meaningful for
    /// [`Positioner::side`].
    pub fn offset(mut self, offset: Pixels) -> Self {
        if let Strategy::Side { offset: slot, .. } = &mut self.strategy {
            *slot = offset;
        }
        self
    }

    // Read after trigger prepaint so an open popup follows a moving trigger.
    pub(crate) fn tracked_corner_position(mut self, position: Rc<Cell<Point<Pixels>>>) -> Self {
        self.corner_position = Some(position);
        self
    }

    /// 使用触发器本帧的测量值解析 side 策略，避免 resize 或重排后读取上一帧位置。
    pub(crate) fn tracked_side_trigger_bounds(
        mut self,
        trigger_bounds: Rc<Cell<Bounds<Pixels>>>,
    ) -> Self {
        self.side_trigger_bounds = Some(trigger_bounds);
        self
    }

    /// Observe resolved geometry before children prepaint.
    pub fn on_position(mut self, callback: impl Fn(ResolvedPosition) + 'static) -> Self {
        self.on_position = Some(Box::new(callback));
        self
    }

    /// Blocks the mouse over the positioned popup.
    ///
    /// Off by default, because a tooltip that swallowed the pointer would
    /// un-hover the very trigger keeping it open. An interactive surface — a
    /// popover, a menu, a dropdown — wants it on: what the surface covers is
    /// the surface's, not the panel underneath.
    pub fn occlude(mut self) -> Self {
        self.occlude = true;
        self
    }

    /// Sets the minimum distance kept between the popup and the viewport edge.
    pub fn margin(mut self, margin: Pixels) -> Self {
        self.margin = margin;
        self
    }
}

/// The part of the window's viewport that is frame rather than content, per
/// side.
///
/// A window drawn with client-side decorations pads its content by the client
/// inset to make room for a shadow, except along an edge that is tiled against
/// the screen, where the frame draws no shadow and the content runs to the
/// viewport edge. `Window::client_inset` is the one value on every side (the
/// platform needs it stable across tiling changes to size the window), so the
/// tiling is what says where it actually applies. A server-decorated window
/// has no frame of its own.
pub(crate) fn frame_insets(decorations: Decorations, client_inset: Pixels) -> Edges<Pixels> {
    match decorations {
        Decorations::Server => Edges::default(),
        Decorations::Client { tiling } => Edges {
            top: if tiling.top { px(0.) } else { client_inset },
            right: if tiling.right { px(0.) } else { client_inset },
            bottom: if tiling.bottom { px(0.) } else { client_inset },
            left: if tiling.left { px(0.) } else { client_inset },
        },
    }
}

/// Resolves the bounds of a popup of `popup_size`.
///
/// Side placement picks the preferred side when the popup fits, otherwise the
/// opposite side, otherwise whichever side has more room. The result is always
/// clamped into the viewport, keeping `margin` from each edge. Center placement
/// derives its origin from the measured popup size before applying that clamp.
fn resolve(
    strategy: Strategy,
    popup_size: Size<Pixels>,
    viewport_size: Size<Pixels>,
    margin: Edges<Pixels>,
    minimum_available_space: Option<Pixels>,
) -> ResolvedPosition {
    match strategy {
        Strategy::Corner { anchor, position } => ResolvedPosition {
            bounds: clamp(
                Bounds::from_anchor_and_size(anchor, position, popup_size),
                viewport_size,
                margin,
            ),
            placement: None,
        },
        Strategy::Center { position } => {
            let origin = point(
                position.x - popup_size.width / 2.,
                position.y - popup_size.height / 2.,
            );
            ResolvedPosition {
                bounds: clamp(Bounds::new(origin, popup_size), viewport_size, margin),
                placement: None,
            }
        }
        Strategy::Side {
            trigger_bounds,
            placement,
            align,
            offset,
        } => {
            let placement = resolve_placement(
                trigger_bounds,
                popup_size,
                viewport_size,
                margin,
                placement,
                offset,
                minimum_available_space,
            );
            let origin = side_origin(trigger_bounds, popup_size, placement, align, offset);
            ResolvedPosition {
                bounds: clamp(Bounds::new(origin, popup_size), viewport_size, margin),
                placement: Some(placement),
            }
        }
    }
}

/// 使用单一边缘选择规则，令受约束弹层的尺寸计算和实际落边保持一致。
fn resolve_placement(
    trigger_bounds: Bounds<Pixels>,
    popup_size: Size<Pixels>,
    viewport_size: Size<Pixels>,
    margin: Edges<Pixels>,
    preferred: Option<Placement>,
    offset: Pixels,
    minimum_available_space: Option<Pixels>,
) -> Placement {
    let right_limit = (viewport_size.width - margin.right).max(margin.left);
    let bottom_limit = (viewport_size.height - margin.bottom).max(margin.top);
    let available_left = (trigger_bounds.left() - margin.left).max(px(0.));
    let available_right = (right_limit - trigger_bounds.right()).max(px(0.));
    let available_above = (trigger_bounds.top() - margin.top).max(px(0.));
    let available_below = (bottom_limit - trigger_bounds.bottom()).max(px(0.));

    if let (Some(minimum), Some(preferred @ (Placement::Top | Placement::Bottom))) =
        (minimum_available_space, preferred)
    {
        if let Some((placement, _)) = vertical_side_space(
            trigger_bounds,
            viewport_size,
            margin,
            preferred,
            offset,
            minimum,
        ) {
            return placement;
        }
    }

    match preferred {
        Some(Placement::Right) if popup_size.width <= available_right => Placement::Right,
        Some(Placement::Right) if popup_size.width <= available_left => Placement::Left,
        Some(Placement::Right) if available_right >= available_left => Placement::Right,
        Some(Placement::Right) => Placement::Left,
        Some(Placement::Left) if popup_size.width <= available_left => Placement::Left,
        Some(Placement::Left) if popup_size.width <= available_right => Placement::Right,
        Some(Placement::Left) if available_left >= available_right => Placement::Left,
        Some(Placement::Left) => Placement::Right,
        Some(Placement::Bottom) if popup_size.height <= available_below => Placement::Bottom,
        Some(Placement::Bottom) if popup_size.height <= available_above => Placement::Top,
        Some(Placement::Bottom) if available_below >= available_above => Placement::Bottom,
        Some(Placement::Bottom) => Placement::Top,
        Some(Placement::Top) | None if popup_size.height <= available_above => Placement::Top,
        Some(Placement::Top) | None if popup_size.height <= available_below => Placement::Bottom,
        Some(Placement::Top) | None if available_below >= available_above => Placement::Bottom,
        Some(Placement::Top) | None => Placement::Top,
    }
}

/// 根据 main 的垂直翻转阈值返回选定边的可用高度，供首帧布局提前限高。
pub(crate) fn vertical_side_space(
    trigger_bounds: Bounds<Pixels>,
    viewport_size: Size<Pixels>,
    margin: Edges<Pixels>,
    preferred: Placement,
    offset: Pixels,
    minimum_available_space: Pixels,
) -> Option<(Placement, Pixels)> {
    if !matches!(preferred, Placement::Top | Placement::Bottom) {
        return None;
    }

    let bottom_limit = (viewport_size.height - margin.bottom).max(margin.top);
    let above = (trigger_bounds.top() - margin.top - offset).max(px(0.));
    let below = (bottom_limit - trigger_bounds.bottom() - offset).max(px(0.));
    let (placement, available) = match preferred {
        Placement::Bottom if below >= minimum_available_space || below >= above => {
            (Placement::Bottom, below)
        }
        Placement::Bottom => (Placement::Top, above),
        Placement::Top if above >= minimum_available_space || above >= below => {
            (Placement::Top, above)
        }
        Placement::Top => (Placement::Bottom, below),
        Placement::Left | Placement::Right => return None,
    };

    Some((placement, available))
}

fn side_origin(
    trigger_bounds: Bounds<Pixels>,
    popup_size: Size<Pixels>,
    placement: Placement,
    align: Align,
    offset: Pixels,
) -> Point<Pixels> {
    let aligned_x = match align {
        Align::Start => trigger_bounds.left(),
        Align::Center => trigger_bounds.center().x - popup_size.width.half(),
        Align::End => trigger_bounds.right() - popup_size.width,
    };
    let aligned_y = match align {
        Align::Start => trigger_bounds.top(),
        Align::Center => trigger_bounds.center().y - popup_size.height.half(),
        Align::End => trigger_bounds.bottom() - popup_size.height,
    };

    match placement {
        Placement::Top => point(aligned_x, trigger_bounds.top() - popup_size.height - offset),
        Placement::Bottom => point(aligned_x, trigger_bounds.bottom() + offset),
        Placement::Left => point(trigger_bounds.left() - popup_size.width - offset, aligned_y),
        Placement::Right => point(trigger_bounds.right() + offset, aligned_y),
    }
}

fn clamp(
    mut bounds: Bounds<Pixels>,
    viewport_size: Size<Pixels>,
    margin: Edges<Pixels>,
) -> Bounds<Pixels> {
    let right_limit = (viewport_size.width - margin.right).max(margin.left);
    let bottom_limit = (viewport_size.height - margin.bottom).max(margin.top);

    if bounds.right() > right_limit {
        bounds.origin.x -= bounds.right() - right_limit;
    }
    if bounds.left() < margin.left {
        bounds.origin.x = margin.left;
    }
    if bounds.bottom() > bottom_limit {
        bounds.origin.y -= bounds.bottom() - bottom_limit;
    }
    if bounds.top() < margin.top {
        bounds.origin.y = margin.top;
    }

    bounds
}

impl ParentElement for Positioner {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl Element for Positioner {
    type RequestLayoutState = PositionerState;
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let child_layout_ids = self
            .children
            .iter_mut()
            .map(|child| child.request_layout(window, cx))
            .collect::<Vec<_>>();
        let layout_id = window.request_layout(
            Style {
                position: Position::Absolute,
                display: Display::Flex,
                ..Style::default()
            },
            child_layout_ids.iter().copied(),
            cx,
        );

        (layout_id, PositionerState { child_layout_ids })
    }

    /// 在当前帧刷新可跟踪定位策略，避免触发器重排后弹层沿用上一帧 bounds。
    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // 父布局重排后，side 策略必须使用同一帧重新测得的触发器 bounds。
        if request_layout.child_layout_ids.is_empty() {
            return;
        }

        let mut child_min = point(Pixels::MAX, Pixels::MAX);
        let mut child_max = Point::default();
        for child_layout_id in &request_layout.child_layout_ids {
            let child_bounds = window.layout_bounds(*child_layout_id);
            child_min = child_min.min(&child_bounds.origin);
            child_max = child_max.max(&child_bounds.bottom_right());
        }

        let popup_size = (child_max - child_min).into();
        let frame = frame_insets(
            window.window_decorations(),
            window.client_inset().unwrap_or(px(0.)),
        );
        let mut strategy = self.strategy;
        if let (Strategy::Corner { position, .. }, Some(tracked)) =
            (&mut strategy, &self.corner_position)
        {
            *position = tracked.get();
        }
        if let (Strategy::Side { trigger_bounds, .. }, Some(tracked)) =
            (&mut strategy, &self.side_trigger_bounds)
        {
            *trigger_bounds = tracked.get();
        }
        let position = resolve(
            strategy,
            popup_size,
            window.viewport_size(),
            frame.map(|inset| *inset + self.margin),
            self.minimum_available_space,
        );
        if let Some(callback) = &self.on_position {
            callback(position);
        }
        // Ahead of the children so it blocks what is behind the popup without
        // blocking the popup's own content.
        if self.occlude {
            window.insert_hitbox(position.bounds, HitboxBehavior::BlockMouse);
        }

        let offset = position.bounds.origin - bounds.origin;
        let offset = point(offset.x.round(), offset.y.round());

        window.with_element_offset(offset, |window| {
            for child in &mut self.children {
                child.prepaint(window, cx);
            }
        });
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        _: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for child in &mut self.children {
            child.paint(window, cx);
        }
    }
}

impl IntoElement for Positioner {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Tiling;

    const MARGIN: Pixels = px(4.);

    fn trigger(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), Size::new(px(w), px(h)))
    }

    fn viewport() -> Size<Pixels> {
        Size::new(px(500.), px(400.))
    }

    /// Existing side placement tests share a constructor with unconstrained legacy behavior.
    fn side(
        trigger_bounds: Bounds<Pixels>,
        placement: Option<Placement>,
        align: Align,
        popup: Size<Pixels>,
    ) -> ResolvedPosition {
        resolve(
            Strategy::Side {
                trigger_bounds,
                placement,
                align,
                offset: px(0.),
            },
            popup,
            viewport(),
            Edges::all(MARGIN),
            None,
        )
    }

    /// The selector's compact rule prioritizes a usable preferred side, then the side with more room.
    fn vertical_space(
        trigger_bounds: Bounds<Pixels>,
        viewport: Size<Pixels>,
        preferred: Placement,
    ) -> (Placement, Pixels) {
        vertical_side_space(
            trigger_bounds,
            viewport,
            Edges::all(px(8.)),
            preferred,
            px(6.),
            px(152.),
        )
        .unwrap()
    }

    /// The picker stays below when the usable area meets its minimum threshold.
    #[test]
    fn vertical_space_keeps_bottom_when_it_meets_the_minimum() {
        assert_eq!(
            vertical_space(trigger(200., 100., 40., 24.), viewport(), Placement::Bottom),
            (Placement::Bottom, px(262.))
        );
    }

    /// The picker flips above when the bottom space is smaller and the top has more room.
    #[test]
    fn vertical_space_flips_above_when_bottom_is_too_small() {
        assert_eq!(
            vertical_space(trigger(200., 240., 40., 24.), viewport(), Placement::Bottom),
            (Placement::Top, px(226.))
        );
    }

    /// Short windows still prefer the larger side even when neither side reaches the minimum.
    #[test]
    fn vertical_space_uses_the_larger_side_below_the_minimum() {
        assert_eq!(
            vertical_space(
                trigger(80., 40., 40., 24.),
                Size::new(px(200.), px(200.)),
                Placement::Bottom,
            ),
            (Placement::Bottom, px(122.))
        );
    }

    #[test]
    fn prefers_the_requested_side_when_it_fits() {
        let position = side(
            trigger(200., 200., 40., 20.),
            Some(Placement::Top),
            Align::Center,
            Size::new(px(80.), px(30.)),
        );

        assert_eq!(position.placement, Some(Placement::Top));
        assert_eq!(position.bounds.bottom(), px(200.));
    }

    #[test]
    fn flips_to_the_opposite_side_when_the_preferred_side_does_not_fit() {
        let position = side(
            trigger(200., 10., 40., 20.),
            Some(Placement::Top),
            Align::Center,
            Size::new(px(80.), px(60.)),
        );

        assert_eq!(position.placement, Some(Placement::Bottom));
        assert_eq!(position.bounds.top(), px(30.));
    }

    #[test]
    fn clamps_into_the_viewport_while_keeping_the_flipped_side() {
        let position = side(
            trigger(480., 200., 40., 20.),
            Some(Placement::Bottom),
            Align::Center,
            Size::new(px(120.), px(30.)),
        );

        assert_eq!(position.placement, Some(Placement::Bottom));
        assert_eq!(position.bounds.right(), viewport().width - MARGIN);
    }

    #[test]
    fn alignment_selects_the_leading_center_or_trailing_edge() {
        let trigger_bounds = trigger(200., 200., 100., 20.);
        let popup = Size::new(px(40.), px(30.));

        let start = side(trigger_bounds, Some(Placement::Bottom), Align::Start, popup);
        let center = side(
            trigger_bounds,
            Some(Placement::Bottom),
            Align::Center,
            popup,
        );
        let end = side(trigger_bounds, Some(Placement::Bottom), Align::End, popup);

        assert_eq!(start.bounds.left(), px(200.));
        assert_eq!(center.bounds.left(), px(230.));
        assert_eq!(end.bounds.left(), px(260.));
    }

    #[test]
    fn side_offset_adds_a_gap_between_trigger_and_popup() {
        let position = resolve(
            Strategy::Side {
                trigger_bounds: trigger(200., 200., 40., 20.),
                placement: Some(Placement::Bottom),
                align: Align::Center,
                offset: px(8.),
            },
            Size::new(px(40.), px(30.)),
            viewport(),
            Edges::all(MARGIN),
            None,
        );

        assert_eq!(position.bounds.top(), px(228.));
    }

    #[test]
    fn corner_positioning_places_the_named_corner_and_never_reports_a_side() {
        let position = resolve(
            Strategy::Corner {
                anchor: Anchor::TopLeft,
                position: point(px(100.), px(100.)),
            },
            Size::new(px(40.), px(30.)),
            viewport(),
            Edges::all(MARGIN),
            None,
        );

        assert_eq!(position.placement, None);
        assert_eq!(position.bounds.origin, point(px(100.), px(100.)));
    }

    #[test]
    fn corner_positioning_clamps_but_does_not_flip() {
        let position = resolve(
            Strategy::Corner {
                anchor: Anchor::TopLeft,
                position: point(px(480.), px(390.)),
            },
            Size::new(px(40.), px(30.)),
            viewport(),
            Edges::all(MARGIN),
            None,
        );

        assert_eq!(position.placement, None);
        assert_eq!(position.bounds.right(), viewport().width - MARGIN);
        assert_eq!(position.bounds.bottom(), viewport().height - MARGIN);
    }

    /// Center placement uses each frame's measured child size to derive its origin.
    #[test]
    fn center_positioning_uses_the_measured_popup_size() {
        let center = point(px(250.), px(200.));
        for (popup_size, expected_origin) in [
            (Size::new(px(80.), px(60.)), point(px(210.), px(170.))),
            (Size::new(px(120.), px(100.)), point(px(190.), px(150.))),
        ] {
            let position = resolve(
                Strategy::Center { position: center },
                popup_size,
                viewport(),
                Edges::all(MARGIN),
                None,
            );

            assert_eq!(position.placement, None);
            assert_eq!(position.bounds.origin, expected_origin);
            assert_eq!(
                position.bounds.origin + point(popup_size.width / 2., popup_size.height / 2.),
                center
            );
        }
    }

    /// A center point near an edge is clamped without changing popup size or inventing a side.
    #[test]
    fn center_positioning_respects_the_viewport_margin() {
        let popup_size = Size::new(px(80.), px(60.));
        let position = resolve(
            Strategy::Center {
                position: point(px(0.), px(0.)),
            },
            popup_size,
            viewport(),
            Edges::all(MARGIN),
            None,
        );

        assert_eq!(position.placement, None);
        assert_eq!(position.bounds.origin, point(MARGIN, MARGIN));
        assert_eq!(position.bounds.size, popup_size);
    }

    #[test]
    fn corner_position_can_be_updated_without_rebuilding_the_positioner() {
        let positioner = Positioner::corner(Anchor::TopLeft, point(px(10.), px(20.)))
            .position(point(px(30.), px(40.)));

        assert!(matches!(
            positioner.strategy,
            Strategy::Corner {
                position,
                ..
            } if position == point(px(30.), px(40.))
        ));
    }

    /// Center animation updates retain center-point semantics instead of switching to an origin.
    #[test]
    fn center_position_can_be_updated_without_rebuilding_the_positioner() {
        let positioner =
            Positioner::center(point(px(10.), px(20.))).position(point(px(30.), px(40.)));

        assert!(matches!(
            positioner.strategy,
            Strategy::Center { position } if position == point(px(30.), px(40.))
        ));
    }

    /// A tiled edge draws no shadow, so a popup may run right up to the
    /// margin there; an untiled edge keeps the whole client inset as well.
    #[test]
    fn frame_insets_apply_the_client_inset_only_on_untiled_edges() {
        let inset = px(20.);

        assert_eq!(
            frame_insets(Decorations::Server, inset),
            Edges::default(),
            "a server-decorated window has no frame to keep clear of"
        );
        assert_eq!(
            frame_insets(
                Decorations::Client {
                    tiling: Tiling::tiled()
                },
                inset
            ),
            Edges::default(),
            "a window tiled on every side draws no shadow padding"
        );
        assert_eq!(
            frame_insets(
                Decorations::Client {
                    tiling: Tiling {
                        top: false,
                        left: true,
                        right: false,
                        bottom: true,
                    }
                },
                inset
            ),
            Edges {
                top: inset,
                right: inset,
                bottom: px(0.),
                left: px(0.),
            }
        );
    }

    /// The bug this guards: a menu opened from a trigger near the right edge
    /// of a tiled window was pushed inward by the client inset although no
    /// shadow was drawn there, so it no longer lined up with its trigger.
    #[test]
    fn clamping_keeps_only_the_margin_on_a_tiled_edge() {
        let popup = Size::new(px(120.), px(30.));
        // A menu aligned to a trigger that ends 10px short of the right edge.
        let corner = Strategy::Corner {
            anchor: Anchor::TopRight,
            position: point(viewport().width - px(10.), px(100.)),
        };
        let margin = Edges::all(MARGIN);
        let inset = px(20.);

        let tiled = resolve(corner, popup, viewport(), margin, None);
        assert_eq!(tiled.bounds.right(), viewport().width - px(10.));

        let untiled = resolve(
            corner,
            popup,
            viewport(),
            margin.map(|margin| *margin + inset),
            None,
        );
        assert_eq!(untiled.bounds.right(), viewport().width - MARGIN - inset);
    }

    // Migrated from the tooltip module when its private positioning logic was
    // merged into this one. They passed unchanged across that move, which is
    // what proves the merge preserved tooltip placement behavior.
    const WINDOW_MARGIN: Pixels = px(4.);

    fn tooltip_placement(
        trigger_bounds: Bounds<Pixels>,
        popup: Size<Pixels>,
        viewport: Size<Pixels>,
        margin: Pixels,
        placement: Option<Placement>,
    ) -> (Bounds<Pixels>, Placement) {
        let resolved = resolve(
            Strategy::Side {
                trigger_bounds,
                placement,
                align: Align::Center,
                offset: px(0.),
            },
            popup,
            viewport,
            Edges::all(margin),
            None,
        );
        (resolved.bounds, resolved.placement.unwrap())
    }

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), Size::new(px(width), px(height)))
    }

    fn size_px(width: f32, height: f32) -> Size<Pixels> {
        Size::new(px(width), px(height))
    }

    #[test]
    fn prefers_above_when_space_allows() {
        let trigger = bounds(100., 80., 80., 24.);
        let position = tooltip_placement(
            trigger,
            size_px(120., 30.),
            size_px(300., 200.),
            WINDOW_MARGIN,
            None,
        );
        assert_eq!(position.1, Placement::Top);
        assert_eq!(position.0.origin, point(px(80.), px(50.)));
    }
    #[test]
    fn flips_and_clamps_on_each_axis() {
        let top = tooltip_placement(
            bounds(24., 4., 120., 32.),
            size_px(240., 32.),
            size_px(520., 260.),
            WINDOW_MARGIN,
            None,
        );
        assert_eq!(top.1, Placement::Bottom);

        let right = tooltip_placement(
            bounds(260., 60., 32., 32.),
            size_px(120., 30.),
            size_px(300., 200.),
            WINDOW_MARGIN,
            Some(Placement::Right),
        );
        assert_eq!(right.1, Placement::Left);

        let left_edge = tooltip_placement(
            bounds(4., 80., 24., 24.),
            size_px(120., 30.),
            size_px(300., 200.),
            WINDOW_MARGIN,
            None,
        );
        assert_eq!(left_edge.0.left(), WINDOW_MARGIN);
    }
    #[test]
    fn places_tooltip_to_the_right() {
        let trigger = bounds(20., 60., 32., 32.);
        let position = tooltip_placement(
            trigger,
            size_px(120., 30.),
            size_px(300., 200.),
            WINDOW_MARGIN,
            Some(Placement::Right),
        );
        assert_eq!(position.1, Placement::Right);
        assert_eq!(position.0.left(), trigger.right());
        assert_eq!(position.0.center().y, trigger.center().y);
    }
    #[test]
    fn right_placement_clamps_vertical_edges() {
        let trigger = bounds(20., 2., 32., 20.);
        let position = tooltip_placement(
            trigger,
            size_px(120., 40.),
            size_px(300., 200.),
            WINDOW_MARGIN,
            Some(Placement::Right),
        );
        assert_eq!(position.1, Placement::Right);
        assert_eq!(position.0.top(), WINDOW_MARGIN);
        assert_eq!(position.0.left(), trigger.right());
    }
    #[test]
    fn uses_larger_side_when_neither_vertical_side_fits() {
        let position = tooltip_placement(
            bounds(120., 20., 40., 20.),
            size_px(160., 120.),
            size_px(300., 100.),
            WINDOW_MARGIN,
            None,
        );
        assert_eq!(position.1, Placement::Bottom);
        assert_eq!(position.0.top(), WINDOW_MARGIN);
        assert_eq!(position.0.left(), px(60.));
    }
}
