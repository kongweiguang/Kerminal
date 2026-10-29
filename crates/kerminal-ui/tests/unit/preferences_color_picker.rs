//! 设置颜色选择器的 HSV 映射、输入事件和 retained state 回归。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc};

use gpui_kit::{
    AppContext as _, Entity, Focusable as _, Hsla, Rgba, TestAppContext, VisualTestContext,
    component::{
        Colorize as _,
        color_picker::{ColorPickerEvent, ColorPickerState},
    },
    hsla,
};

use super::{ColorPickerView, PickerFormat, hsla_to_hsv, hsv_to_hsla, numeric_values};

/// 颜色写回配置前会经过 HSLA 表示；这些样本锁住字节舍入和半透明 alpha 的往返稳定性。
///
/// @author kongweiguang
#[test]
fn hex_roundtrip_preserves_rgb_bytes_and_alpha() {
    for hex in ["#123456", "#A0B1C280", "#0102037F"] {
        let roundtrip = Hsla::parse_hex(hex).expect("valid HEX parses").to_hex();
        assert_eq!(roundtrip, hex);
    }
}

/// 中文和 emoji 可作为输入法中间态；按 ASCII HEX 拒绝，且不能在 UTF-8 字节边界 panic。
///
/// @author kongweiguang
#[test]
fn hex_parser_rejects_unicode_at_six_and_eight_byte_lengths() {
    assert!(Hsla::parse_hex("###123456").is_err());
    for invalid in ["#中ab", "#😀abc"] {
        assert!(Hsla::parse_hex(invalid).is_err(), "input={invalid}");
    }
}

/// 用生产 retained view 挂载共享 picker，并打开真实 Popover 以供输入事件命中。
fn mount_picker(
    app: &mut TestAppContext,
) -> (
    Entity<ColorPickerState>,
    Entity<ColorPickerView>,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let mounted_picker = Rc::new(RefCell::new(None));
    let mounted_view = Rc::new(RefCell::new(None));
    let picker_output = mounted_picker.clone();
    let view_output = mounted_view.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let picker =
            cx.new(|cx| ColorPickerState::new(window, cx).default_value(hsla(0., 1., 0.5, 1.)));
        let view = cx.new(|cx| ColorPickerView::new(picker.clone(), None, window, cx));
        picker.update(cx, |picker, cx| picker.set_open(true, cx));
        *picker_output.borrow_mut() = Some(picker);
        *view_output.borrow_mut() = Some(view.clone());
        gpui_kit::base::Root::new(view, window, cx)
    });
    let picker = mounted_picker
        .borrow_mut()
        .take()
        .expect("ColorPickerState mounted");
    let view = mounted_view
        .borrow_mut()
        .take()
        .expect("ColorPickerView mounted");
    draw(visual);
    (picker, view, visual)
}

/// 等待 retained 状态更新并绘制两帧，让实体观察与输入焦点完成同步。
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
}

/// 通过实际键盘输入替换受控字段内容，不直接调用业务颜色更新方法。
fn type_into_input(
    visual: &mut VisualTestContext,
    input: &Entity<gpui_kit::component::input::InputState>,
    text: &str,
) {
    visual.update(|window, app| {
        let focus = input.read(app).focus_handle(app);
        window.focus(&focus, app);
        window.draw(app).clear(app);
    });
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input(text);
    draw(visual);
}

/// 读取 retained 数值字段的当前文本，用于比较模式切换和外部颜色变化后的投影。
fn numeric_input_texts(
    visual: &mut VisualTestContext,
    view: &Entity<ColorPickerView>,
) -> Vec<String> {
    visual.update(|_, app| {
        view.read(app)
            .numeric_inputs
            .iter()
            .map(|input| input.read(app).value().to_string())
            .collect()
    })
}

/// 让一项控制格式状态，并等待 ColorPickerView 的实体观察刷新未聚焦字段。
fn set_format(
    visual: &mut VisualTestContext,
    picker: &Entity<ColorPickerState>,
    format: PickerFormat,
) {
    visual.update(|window, app| {
        window.blur(app);
        picker.update(app, |picker, cx| picker.set_active_tab(format.tab(), cx));
    });
    draw(visual);
}

/// 用 RGB 数据检查 HSV 平面四角及递增色相顺序，锁住坐标轴方向和非镜像转换。
#[test]
fn hsv_plane_corners_and_hue_order_match_color_wheel() {
    let corners = [
        (0., 1., [1., 1., 1.]),
        (1., 1., [1., 0., 0.]),
        (0., 0., [0., 0., 0.]),
        (1., 0., [0., 0., 0.]),
    ];
    for (saturation, value, expected) in corners {
        let color = hsv_to_hsla(0., saturation, value, 1.);
        let rgba = Rgba::from(color);
        for (actual, expected) in [rgba.r, rgba.g, rgba.b].into_iter().zip(expected) {
            assert!((actual - expected).abs() < 0.002);
        }
        let (_, _, actual_value) = hsla_to_hsv(color);
        assert!((actual_value - value).abs() < 0.002);
    }

    let wheel = [
        (0., [1., 0., 0.]),
        (1. / 6., [1., 1., 0.]),
        (2. / 6., [0., 1., 0.]),
        (3. / 6., [0., 1., 1.]),
        (4. / 6., [0., 0., 1.]),
        (5. / 6., [1., 0., 1.]),
    ];
    for (hue, expected) in wheel {
        let rgba = Rgba::from(hsv_to_hsla(hue, 1., 1., 1.));
        for (actual, expected) in [rgba.r, rgba.g, rgba.b].into_iter().zip(expected) {
            assert!((actual - expected).abs() < 0.002, "hue={hue}");
        }
    }
}

/// 真实 RGB、HSL 和 HEX 输入都写回原 ColorPickerState，并沿共享事件发出提交颜色。
#[gpui_kit::test]
fn real_rgb_hsl_and_hex_input_update_shared_picker_and_emit_change(app: &mut TestAppContext) {
    let (picker, view, visual) = mount_picker(app);
    let changes = Rc::new(RefCell::new(Vec::<Option<Hsla>>::new()));
    let change_output = changes.clone();
    let _subscription = visual.update(|_, app| {
        app.subscribe(&picker, move |_, event: &ColorPickerEvent, _| {
            let ColorPickerEvent::Change(color) = event;
            change_output.borrow_mut().push(*color);
        })
    });

    let red_input = visual.update(|_, app| view.read(app).numeric_inputs[0].clone());
    type_into_input(visual, &red_input, "17");
    let rgb_color =
        visual.update(|_, app| picker.read(app).value().expect("RGB input commits a color"));
    let rgb = Rgba::from(rgb_color);
    assert!((rgb.r - 17. / 255.).abs() < 0.002);
    assert!(rgb.g.abs() < 0.002);
    assert!(rgb.b.abs() < 0.002);
    assert_eq!(changes.borrow().last().copied(), Some(Some(rgb_color)));

    set_format(visual, &picker, PickerFormat::Hsl);
    let saturation_input = visual.update(|_, app| view.read(app).numeric_inputs[1].clone());
    type_into_input(visual, &saturation_input, "50");
    let hsl_color =
        visual.update(|_, app| picker.read(app).value().expect("HSL input commits a color"));
    assert!((hsl_color.s - 0.5).abs() < 0.002);
    assert_eq!(changes.borrow().last().copied(), Some(Some(hsl_color)));

    set_format(visual, &picker, PickerFormat::Hex);
    let hex_input = visual.update(|_, app| picker.read(app).hex_input().clone());
    type_into_input(visual, &hex_input, "#123456");
    visual.simulate_keystrokes("enter");
    draw(visual);
    let hex_color = visual.update(|_, app| {
        picker
            .read(app)
            .value()
            .expect("Enter commits a valid HEX input")
    });
    assert_eq!(hex_color.to_hex(), "#123456");
    assert_eq!(changes.borrow().last().copied(), Some(Some(hex_color)));
    assert!(!visual.update(|_, app| picker.read(app).is_open()));
}

/// 切换 RGB/HSL 并由 picker 外部更新颜色时，所有未聚焦字段都跟随共享值刷新。
#[gpui_kit::test]
fn format_changes_and_external_color_updates_refresh_unfocused_inputs(app: &mut TestAppContext) {
    let (picker, view, visual) = mount_picker(app);
    let initial_hex =
        visual.update(|_, app| picker.read(app).hex_input().read(app).value().to_string());
    assert_eq!(initial_hex, "#FF0000");

    set_format(visual, &picker, PickerFormat::Hsl);
    let hsl_color = visual.update(|_, app| picker.read(app).value().unwrap());
    assert_eq!(
        numeric_input_texts(visual, &view),
        numeric_values(hsl_color, PickerFormat::Hsl)
            .map(|value| value.map_or_else(String::new, |value| value.to_string()))
            .to_vec()
    );

    let external_color = hsla(0.5, 0.6, 0.4, 1.);
    visual.update(|window, app| {
        picker.update(app, |picker, cx| {
            picker.update_color(external_color, window, cx)
        });
    });
    draw(visual);
    assert_eq!(
        numeric_input_texts(visual, &view),
        numeric_values(external_color, PickerFormat::Hsl)
            .map(|value| value.map_or_else(String::new, |value| value.to_string()))
            .to_vec()
    );

    set_format(visual, &picker, PickerFormat::Rgb);
    assert_eq!(
        numeric_input_texts(visual, &view),
        numeric_values(external_color, PickerFormat::Rgb)
            .map(|value| value.map_or_else(String::new, |value| value.to_string()))
            .to_vec()
    );
}
