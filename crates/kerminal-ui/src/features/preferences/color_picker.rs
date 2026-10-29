//! 设置中的原生色盘外观；颜色值与提交事件仍由共享 ColorPickerState 持有。
//!
//! @author kongweiguang

#[cfg(test)]
#[path = "../../../tests/unit/preferences_color_picker.rs"]
mod tests;

use std::{cell::Cell, rc::Rc};

use gpui_kit::{
    App, AppContext as _, Bounds, Context, Entity, Focusable as _, Hsla, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render, Rgba, Subscription,
    Window,
    base::ColorPicker as BaseColorPicker,
    canvas,
    component::{
        ActiveTheme as _, Sizable as _,
        button::{Button, ButtonVariants as _},
        color_picker::ColorPickerState,
        input::{Input, InputEvent, InputState},
        menu::{DropdownMenu as _, PopupMenuItem},
        popover::Popover,
    },
    div, hsla, linear_color_stop, linear_gradient,
    prelude::*,
    px, relative, rgb,
};

const POPUP_WIDTH: f32 = 234.;
const POPUP_HEIGHT: f32 = 252.;
const PLANE_HEIGHT: f32 = 138.;

/// Root 将平台取色器作为能力注入，视图本身不依赖基础设施实现。
pub(super) type ScreenColorPickerCallback =
    Rc<dyn Fn(Entity<ColorPickerState>, &mut Window, &mut App)>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PickerFormat {
    Rgb,
    Hsl,
    Hex,
}

impl PickerFormat {
    /// 固定三种与 Main 原生色盘相同的数字表示，并将未知索引收敛到安全默认值。
    fn from_tab(tab: usize) -> Self {
        match tab {
            1 => Self::Hsl,
            2 => Self::Hex,
            _ => Self::Rgb,
        }
    }

    /// 返回格式菜单使用的稳定索引，使展示模式继续复用原 picker 的轻量状态位。
    fn tab(self) -> usize {
        match self {
            Self::Rgb => 0,
            Self::Hsl => 1,
            Self::Hex => 2,
        }
    }

    /// 返回输入字段下方的通道标签，保持不同表示与数值顺序一致。
    fn labels(self) -> [&'static str; 3] {
        match self {
            Self::Rgb => ["R", "G", "B"],
            Self::Hsl => ["H", "S", "L"],
            Self::Hex => ["HEX", "", ""],
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DragTarget {
    Plane,
    Hue,
}

/// 设置专用色盘视图；业务颜色仍由调用方持有的 ColorPickerState 唯一写入。
pub(super) struct ColorPickerView {
    picker: Entity<ColorPickerState>,
    on_pick_screen_color: Option<ScreenColorPickerCallback>,
    numeric_inputs: Vec<Entity<InputState>>,
    focused_input: Option<usize>,
    drag_target: Rc<Cell<Option<DragTarget>>>,
    _subscriptions: Vec<Subscription>,
}

impl ColorPickerView {
    /// 固定数字输入实体和观察订阅，避免颜色拖动或格式切换时丢失输入焦点。
    pub(super) fn new(
        picker: Entity<ColorPickerState>,
        on_pick_screen_color: Option<ScreenColorPickerCallback>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let numeric_inputs = (0..3)
            .map(|_| {
                cx.new(|cx| {
                    InputState::new(window, cx)
                        .pattern(regex::Regex::new(r"^[0-9]{0,3}$").expect("fixed numeric pattern"))
                })
            })
            .collect::<Vec<_>>();
        let mut subscriptions = Vec::with_capacity(numeric_inputs.len() + 1);

        for (index, input) in numeric_inputs.iter().enumerate() {
            subscriptions.push(cx.subscribe_in(
                input,
                window,
                move |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Focus => this.focused_input = Some(index),
                    InputEvent::Blur => {
                        if this.focused_input == Some(index) {
                            this.focused_input = None;
                            this.sync_numeric_inputs(window, cx);
                        }
                    }
                    InputEvent::Change => {
                        this.focused_input = Some(index);
                        this.apply_numeric_input(
                            index,
                            input.read(cx).value().as_ref(),
                            window,
                            cx,
                        );
                    }
                    InputEvent::PressEnter { .. } => {}
                },
            ));
        }

        subscriptions.push(cx.observe_in(&picker, window, |this, _, window, cx| {
            this.sync_numeric_inputs(window, cx);
            cx.notify();
        }));

        let mut view = Self {
            picker,
            on_pick_screen_color,
            numeric_inputs,
            focused_input: None,
            drag_target: Rc::new(Cell::new(None)),
            _subscriptions: subscriptions,
        };
        view.sync_numeric_inputs(window, cx);
        view
    }

    /// 按当前表示刷新非聚焦字段；聚焦字段保留编辑中间态，失焦后再规范化。
    fn sync_numeric_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let color = self.current_color(cx);
        let format = self.format(cx);
        let values = numeric_values(color, format);

        for (index, value) in values.into_iter().enumerate() {
            if self.focused_input == Some(index) {
                continue;
            }
            let text = value.map_or_else(String::new, |value| value.to_string());
            let input = &self.numeric_inputs[index];
            if input.read(cx).value().as_ref() == text {
                continue;
            }
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
    }

    /// 解析一个完整且范围有效的通道值，忽略输入法、删除过程和超范围中间态。
    fn apply_numeric_input(
        &mut self,
        index: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Ok(value) = text.parse::<u16>() else {
            return;
        };
        let format = self.format(cx);
        let current = self.current_color(cx);
        let next = match format {
            PickerFormat::Rgb if value <= 255 => {
                let mut rgba = Rgba::from(current);
                let channel = value as f32 / 255.;
                match index {
                    0 => rgba.r = channel,
                    1 => rgba.g = channel,
                    2 => rgba.b = channel,
                    _ => return,
                }
                Hsla::from(rgba)
            }
            PickerFormat::Hsl => {
                let mut hue = current.h;
                let mut saturation = current.s;
                let mut lightness = current.l;
                match index {
                    0 if value <= 360 => hue = value as f32 / 360.,
                    1 if value <= 100 => saturation = value as f32 / 100.,
                    2 if value <= 100 => lightness = value as f32 / 100.,
                    _ => return,
                }
                hsla(hue, saturation, lightness, current.a)
            }
            _ => return,
        };
        self.update_color(next, window, cx);
    }

    /// 读取外部 picker 当前预览值；未赋值时用不透明红色保持色盘和字段可操作。
    fn current_color(&self, cx: &App) -> Hsla {
        self.picker
            .read(cx)
            .displayed_color()
            .unwrap_or_else(|| hsla(0., 1., 0.5, 1.))
    }

    /// 读取由同一 ColorPickerState 持有的表示模式，避免 renderer 维护第二个真值。
    fn format(&self, cx: &App) -> PickerFormat {
        PickerFormat::from_tab(self.picker.read(cx).active_tab())
    }

    /// 将交互结果写回原 picker，让现有颜色事件订阅和草稿保存链保持不变。
    fn update_color(&mut self, color: Hsla, window: &mut Window, cx: &mut Context<Self>) {
        if self.picker.read(cx).displayed_color() == Some(color) {
            return;
        }
        self.picker
            .update(cx, |picker, cx| picker.update_color(color, window, cx));
    }

    /// 按 HSV 平面位置更新颜色，并转回共享模型使用的 HSLA 表示。
    fn update_plane(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.current_color(cx);
        let hsv = hsla_to_hsv(current);
        let saturation = normalized(position.x, bounds.left(), bounds.right());
        let value = 1. - normalized(position.y, bounds.top(), bounds.bottom());
        self.update_color(hsv_to_hsla(hsv.0, saturation, value, 1.), window, cx);
    }

    /// 按色相条位置更新色相，同时保留当前 HSV 饱和度和亮度。
    fn update_hue(
        &mut self,
        bounds: Bounds<Pixels>,
        position: Point<Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let current = self.current_color(cx);
        let hsv = hsla_to_hsv(current);
        let hue = normalized(position.x, bounds.left(), bounds.right());
        self.update_color(hsv_to_hsla(hue, hsv.1, hsv.2, 1.), window, cx);
    }

    /// 绘制符合 Main 尺寸的固定色盘弹层，颜色、模式和外部关闭状态均来自共享状态。
    /// 系统色盘表面独立于工作台材料透明度，保证深浅主题下取色预览稳定。
    fn render_popup(&self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popup_background =
            crate::theme::ThemeTokens::current(window, cx).settings_native_color_picker_surface();
        let popup_border = cx.theme().border;
        let popup_foreground = cx.theme().foreground;
        let color = self.current_color(cx);
        let hsv = hsla_to_hsv(color);
        let hue_color = hsv_to_hsla(hsv.0, 1., 1., 1.);
        let plane_marker = div()
            .absolute()
            .left(relative(hsv.1))
            .top(relative(1. - hsv.2))
            .ml(px(-6.5))
            .mt(px(-6.5))
            .size(px(13.))
            .rounded_full()
            .border_2()
            .border_color(rgb(0xffffff))
            .shadow_sm();
        let plane_view = self.pointer_layer(DragTarget::Plane, cx);
        let plane = div()
            .id("settings-color-picker-plane")
            .relative()
            .w_full()
            .h(px(PLANE_HEIGHT))
            .flex_none()
            .overflow_hidden()
            .bg(hue_color)
            .child(div().absolute().inset_0().bg(linear_gradient(
                90.,
                linear_color_stop(rgb(0xffffff), 0.),
                linear_color_stop(rgb(0xffffff).opacity(0.), 1.),
            )))
            .child(div().absolute().inset_0().bg(linear_gradient(
                180.,
                linear_color_stop(rgb(0x000000).opacity(0.), 0.),
                linear_color_stop(rgb(0x000000), 1.),
            )))
            .child(plane_marker)
            .child(plane_view);

        div()
            .id("settings-color-picker-popup")
            .w(px(POPUP_WIDTH))
            .h(px(POPUP_HEIGHT))
            .flex()
            .flex_col()
            .overflow_hidden()
            .rounded_sm()
            .border_1()
            .border_color(popup_border)
            .bg(popup_background)
            .text_color(popup_foreground)
            .shadow_lg()
            .child(plane)
            .child(self.render_controls_row(color, window, cx))
            .child(self.render_value_fields(cx.entity().downgrade(), cx))
    }

    /// 注册平面或色相条的窗口级鼠标事件，使按住拖动越过指示器仍连续更新。
    fn pointer_layer(&self, target: DragTarget, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity().downgrade();
        let drag_target = self.drag_target.clone();

        canvas(
            |bounds, window, _| window.insert_hitbox(bounds, gpui_kit::HitboxBehavior::Normal),
            move |bounds, _, window, _| {
                let view_down = view.clone();
                let drag_target_down = drag_target.clone();
                window.on_mouse_event(move |event: &MouseDownEvent, phase, window, app| {
                    if !phase.bubble()
                        || event.button != MouseButton::Left
                        || !bounds_contains(bounds, event.position)
                    {
                        return;
                    }
                    drag_target_down.set(Some(target));
                    let _ = view_down.update(app, |this, cx| match target {
                        DragTarget::Plane => this.update_plane(bounds, event.position, window, cx),
                        DragTarget::Hue => this.update_hue(bounds, event.position, window, cx),
                    });
                });

                let view_move = view.clone();
                let drag_target_move = drag_target.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, app| {
                    if !phase.bubble()
                        || !event.dragging()
                        || drag_target_move.get() != Some(target)
                    {
                        return;
                    }
                    let _ = view_move.update(app, |this, cx| match target {
                        DragTarget::Plane => this.update_plane(bounds, event.position, window, cx),
                        DragTarget::Hue => this.update_hue(bounds, event.position, window, cx),
                    });
                });

                let drag_target_up = drag_target.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, _| {
                    if phase.bubble()
                        && event.button == MouseButton::Left
                        && drag_target_up.get() == Some(target)
                    {
                        drag_target_up.set(None);
                    }
                });
            },
        )
        .absolute()
        .inset_0()
    }

    /// 排列色相渐变、预览色和可选平台吸管；无采样能力时保留原生槽位但不画假按钮。
    fn render_controls_row(
        &self,
        color: Hsla,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let hsv = hsla_to_hsv(color);
        let hue_stops = [
            hsla(0., 1., 0.5, 1.),
            hsla(1. / 6., 1., 0.5, 1.),
            hsla(2. / 6., 1., 0.5, 1.),
            hsla(3. / 6., 1., 0.5, 1.),
            hsla(4. / 6., 1., 0.5, 1.),
            hsla(5. / 6., 1., 0.5, 1.),
            hsla(1., 1., 0.5, 1.),
        ];
        let hue_track = div()
            .id("settings-color-picker-hue")
            .relative()
            .flex()
            .flex_1()
            .h(px(12.))
            .rounded_sm()
            .overflow_hidden()
            .children(hue_stops.windows(2).map(|pair| {
                div().flex_1().h_full().bg(linear_gradient(
                    90.,
                    linear_color_stop(pair[0], 0.),
                    linear_color_stop(pair[1], 1.),
                ))
            }))
            .child(
                div()
                    .absolute()
                    .left(relative(hsv.0))
                    .top(px(-0.5))
                    .ml(px(-6.))
                    .size(px(13.))
                    .rounded_full()
                    .border_1()
                    .border_color(rgb(0x333333))
                    .bg(rgb(0xffffff)),
            )
            .child(self.pointer_layer(DragTarget::Hue, cx));
        let eyedropper = self
            .on_pick_screen_color
            .as_ref()
            .map(|callback| {
                let callback = callback.clone();
                let picker = self.picker.clone();
                Button::new(("settings-color-picker-eyedropper", self.picker.entity_id()))
                    .compact()
                    .ghost()
                    .icon(gpui_kit::assets::IconName::Pipette)
                    .w(px(18.))
                    .h(px(20.))
                    .flex_none()
                    .accessibility_label(crate::locale::text("屏幕取色", "Pick screen color"))
                    .on_click(move |_, window, app| {
                        callback(picker.clone(), window, app);
                    })
                    .into_any_element()
            })
            .unwrap_or_else(|| div().w(px(18.)).h(px(20.)).into_any_element());

        div()
            .w_full()
            .h(px(46.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(12.))
            .child(eyedropper)
            .child(
                div()
                    .size(px(32.))
                    .flex_none()
                    .rounded_full()
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(color),
            )
            .child(hue_track)
            .child(div().w(px(8.)).flex_none())
    }

    /// 渲染 Main 原生色盘中的三个数值框或单个 HEX 框，并保留原格式下拉菜单。
    fn render_value_fields(
        &self,
        view: gpui_kit::WeakEntity<Self>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let format = self.format(cx);
        let labels = format.labels();
        let fields = if format == PickerFormat::Hex {
            div()
                .id("settings-color-picker-hex-field")
                .flex_1()
                .min_w(px(0.))
                .child(gpui_kit::Styled::h(
                    Input::new(self.picker.read(cx).hex_input()).small(),
                    px(32.),
                ))
                .into_any_element()
        } else {
            div()
                .flex()
                .gap(px(6.))
                .children(self.numeric_inputs.iter().map(|input| {
                    gpui_kit::Styled::h(
                        Input::new(input)
                            .small()
                            .w(px(52.))
                            .text_align(gpui_kit::TextAlign::Center),
                        px(32.),
                    )
                }))
                .into_any_element()
        };
        let menu = self.render_format_menu(format, view);

        div()
            .w_full()
            .flex_1()
            .min_h(px(0.))
            .flex()
            .flex_col()
            .justify_start()
            .pt(px(0.))
            .child(
                div()
                    .w_full()
                    .h(px(38.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .pl(px(24.))
                    .pr(px(16.))
                    .child(fields)
                    .child(menu),
            )
            .child(
                div()
                    .w_full()
                    .h(px(25.))
                    .flex_none()
                    .flex()
                    .gap(px(6.))
                    .pl(px(24.))
                    .pr(px(16.))
                    .children(
                        labels
                            .into_iter()
                            .filter(|label| !label.is_empty())
                            .map(|label| {
                                let width = if format == PickerFormat::Hex {
                                    px(170.)
                                } else {
                                    px(52.)
                                };
                                div().w(width).text_size(px(12.)).text_center().child(label)
                            }),
                    )
                    .child(div().w(px(18.))),
            )
    }

    /// 使用独立的小下拉按钮选择 RGB、HSL 或 HEX，不增加 Main 中不存在的格式入口。
    fn render_format_menu(
        &self,
        format: PickerFormat,
        view: gpui_kit::WeakEntity<Self>,
    ) -> impl IntoElement {
        let picker = self.picker.clone();
        Button::new(("settings-color-picker-format", self.picker.entity_id()))
            .compact()
            .ghost()
            .icon(gpui_kit::assets::IconName::ChevronsUpDown)
            .w(px(18.))
            .h(px(28.))
            .flex_none()
            .accessibility_label(crate::locale::text("颜色格式", "Color format"))
            .dropdown_menu(move |mut menu, _, _| {
                for (value, label) in [
                    (PickerFormat::Rgb, "RGB"),
                    (PickerFormat::Hsl, "HSL"),
                    (PickerFormat::Hex, "HEX"),
                ]
                .into_iter()
                {
                    let tab = value.tab();
                    let picker = picker.clone();
                    let view = view.clone();
                    menu = menu.item(PopupMenuItem::new(label).checked(value == format).on_click(
                        move |_, window, app| {
                            picker.update(app, |picker, cx| {
                                picker.set_active_tab(tab, cx);
                            });
                            let _ = view.update(app, |this, cx| {
                                this.focused_input = None;
                                this.sync_numeric_inputs(window, cx);
                            });
                        },
                    ));
                }
                menu
            })
    }
}

impl Render for ColorPickerView {
    /// 保留共享 picker 的键盘、焦点与 open 状态，让自绘 UI 不另造弹层行为。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.picker
            .update(cx, |picker, cx| picker.sync_pending_value(window, cx));
        let picker_state = self.picker.read(cx);
        let open = picker_state.is_open();
        let color = picker_state.value();
        let focus_handle = self.picker.focus_handle(cx);
        let picker_for_root = self.picker.clone();
        let picker_for_popover = self.picker.clone();
        let id = self.picker.entity_id();

        BaseColorPicker::new(("settings-color-picker", id))
            .open(open)
            .track_focus(&focus_handle)
            .accessibility_label(crate::locale::text("颜色选择器", "Color picker"))
            .on_open_change(move |open, _, cx| {
                picker_for_root.update(cx, |picker, cx| picker.set_open(open, cx));
            })
            .child(
                Popover::new(("settings-color-picker-popover", id))
                    .open(open)
                    .appearance(false)
                    .overlay_closable(true)
                    .on_open_change(move |open, _, cx| {
                        picker_for_popover.update(cx, |picker, cx| picker.set_open(*open, cx));
                    })
                    .trigger(
                        Button::new(("settings-color-picker-trigger", id))
                            .compact()
                            .outline()
                            .w(px(32.))
                            .h(px(32.))
                            .accessibility_label(crate::locale::text("选择颜色", "Choose color"))
                            .child(
                                div()
                                    .size(px(20.))
                                    .rounded_sm()
                                    .bg(color.unwrap_or_else(|| hsla(0., 0., 0.5, 1.))),
                            ),
                    )
                    .when(open, |popover| popover.child(self.render_popup(window, cx))),
            )
    }
}

/// 将屏幕轴坐标夹到矩形范围并归一化，保证拖动越界时颜色仍有限且可预期。
fn normalized(value: Pixels, start: Pixels, end: Pixels) -> f32 {
    let extent = (end - start).as_f32();
    if extent <= f32::EPSILON {
        return 0.;
    }
    ((value - start).as_f32() / extent).clamp(0., 1.)
}

/// 判断桌面指针是否位于指定色盘平面或色相条的当前 hitbox 内。
fn bounds_contains(bounds: Bounds<Pixels>, position: Point<Pixels>) -> bool {
    position.x >= bounds.left()
        && position.x <= bounds.right()
        && position.y >= bounds.top()
        && position.y <= bounds.bottom()
}

/// 把 UI 使用的 HSLA 转成 HSV 平面坐标，并保留灰阶颜色的既有色相。
fn hsla_to_hsv(color: Hsla) -> (f32, f32, f32) {
    let rgba = Rgba::from(color);
    let max = rgba.r.max(rgba.g).max(rgba.b);
    let min = rgba.r.min(rgba.g).min(rgba.b);
    let delta = max - min;
    let hue = if delta <= f32::EPSILON {
        color.h.rem_euclid(1.)
    } else if max == rgba.r {
        ((rgba.g - rgba.b) / delta).rem_euclid(6.) / 6.
    } else if max == rgba.g {
        ((rgba.b - rgba.r) / delta + 2.) / 6.
    } else {
        ((rgba.r - rgba.g) / delta + 4.) / 6.
    };
    let saturation = if max <= f32::EPSILON { 0. } else { delta / max };
    (hue, saturation, max)
}

/// 把 HSV 平面值转换成共享 HSLA 值，供平面、色相条和 RGB 输入共用。
fn hsv_to_hsla(hue: f32, saturation: f32, value: f32, alpha: f32) -> Hsla {
    let value = value.clamp(0., 1.);
    let saturation = saturation.clamp(0., 1.);
    let lightness = value * (1. - saturation / 2.);
    let hsl_saturation = if lightness <= f32::EPSILON || lightness >= 1. - f32::EPSILON {
        0.
    } else {
        (value - lightness) / lightness.min(1. - lightness)
    };
    hsla(hue, hsl_saturation, lightness, alpha)
}

/// 返回三个文本框对应的整数值；HEX 使用共享 picker 自己的完整精度输入实体。
fn numeric_values(color: Hsla, format: PickerFormat) -> [Option<u16>; 3] {
    match format {
        PickerFormat::Rgb => {
            let rgba = Rgba::from(color);
            [
                Some((rgba.r * 255.).round() as u16),
                Some((rgba.g * 255.).round() as u16),
                Some((rgba.b * 255.).round() as u16),
            ]
        }
        PickerFormat::Hsl => [
            Some((color.h.rem_euclid(1.) * 360.).round() as u16),
            Some((color.s * 100.).round() as u16),
            Some((color.l * 100.).round() as u16),
        ],
        PickerFormat::Hex => [None, None, None],
    }
}
