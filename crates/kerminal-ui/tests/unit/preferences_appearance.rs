//! 外观控件的选择映射与仅依赖父容器的布局投影回归。
//!
//! @author kongweiguang

use super::{
    BACKGROUND_FIT_CHOICES, BackgroundFit, DENSITY_CHOICES, InterfaceDensity,
    THEME_CHOICE_HEIGHT_PX, appearance_column,
};

/// 密度按钮的 ID、标签和写入枚举保持同序，避免显示选择与保存值发生错配。
/// @author kongweiguang
#[test]
fn density_choices_keep_stable_labels_and_values() {
    assert_eq!(
        DENSITY_CHOICES.map(|choice| choice.id),
        ["density-compact", "density-comfortable", "density-spacious"]
    );
    assert_eq!(
        DENSITY_CHOICES.map(|choice| choice.value),
        [
            InterfaceDensity::Compact,
            InterfaceDensity::Comfortable,
            InterfaceDensity::Spacious,
        ]
    );
    assert_eq!(
        DENSITY_CHOICES.map(|choice| choice.label_zh),
        ["紧凑", "舒适", "宽松"]
    );
    assert_eq!(
        DENSITY_CHOICES.map(|choice| choice.label_en),
        ["Compact", "Comfortable", "Spacious"]
    );
    assert_eq!(
        DENSITY_CHOICES.map(|choice| choice.description_zh),
        ["更紧凑，适合小屏。", "默认留白。", "留白更多。"]
    );
}

/// 主题卡片沿用 main 的 80px 最小高度；密度项高度由标签、说明和垂直留白共同决定。
/// @author kongweiguang
#[test]
fn appearance_theme_cards_match_main_minimum_height() {
    assert_eq!(THEME_CHOICE_HEIGHT_PX, 80.0);
}

/// 验证真实卡片含内边距和边框时的外框比例，避免只断言 builder 属性而漏掉 flex 分配偏差。
/// @author kongweiguang
#[gpui_kit::test]
fn appearance_columns_follow_main_fraction_without_fixed_widths(
    app: &mut gpui_kit::TestAppContext,
) {
    use gpui_kit::{Context, InteractiveElement as _, Render, Window, div, prelude::*, px};

    struct Columns(f32);

    impl Render for Columns {
        /// 复用生产卡片和列布局；只替换容器宽度，以便在无原生窗口的门禁中覆盖缩放后的几何。
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let theme =
                crate::theme::ThemeTokens::for_appearance(gpui_kit::WindowAppearance::Light);
            div()
                .w(px(self.0))
                .flex()
                .gap_4()
                .child(
                    appearance_column(super::section_panel(theme), 0.92)
                        .debug_selector(|| "appearance-left".into()),
                )
                .child(
                    appearance_column(super::section_panel(theme), 1.08)
                        .debug_selector(|| "appearance-right".into()),
                )
        }
    }

    app.update(crate::init);
    for width in [480., 680., 900.] {
        let (_, visual) = app.add_window_view(|_, _| Columns(width));
        visual.update(|window, cx| window.draw(cx).clear(cx));
        let left = visual
            .debug_bounds("appearance-left")
            .expect("left card laid out");
        let right = visual
            .debug_bounds("appearance-right")
            .expect("right card laid out");
        let gap = right.origin.x.as_f32() - left.right().as_f32();
        let available = width - gap;
        assert!((gap - 16.).abs() < 0.1, "main card gap: {gap}");
        assert!(
            (left.size.width.as_f32() - available * 0.46).abs() < 1.,
            "left fraction at {width}: {left:?}"
        );
        assert!(
            (right.size.width.as_f32() - available * 0.54).abs() < 1.,
            "right fraction at {width}: {right:?}"
        );
        assert!(
            (right.right().as_f32() - width).abs() < 0.1,
            "cards fit container at {width}"
        );
    }
}

/// 背景铺放选项与 main 的可见名称和值保持一致，避免存储枚举和卡片顺序错位。
/// @author kongweiguang
#[test]
fn background_fit_choices_keep_main_labels_and_order() {
    assert_eq!(
        BACKGROUND_FIT_CHOICES.map(|choice| choice.id),
        ["fit-cover", "fit-contain", "fit-tile"]
    );
    assert_eq!(
        BACKGROUND_FIT_CHOICES.map(|choice| choice.value),
        [
            BackgroundFit::Cover,
            BackgroundFit::Contain,
            BackgroundFit::Tile
        ]
    );
    assert_eq!(
        BACKGROUND_FIT_CHOICES.map(|choice| choice.label_zh),
        ["填充画布", "完整显示", "平铺纹理"]
    );
}
