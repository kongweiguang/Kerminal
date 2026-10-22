//! 外观控件的选择映射与仅依赖父容器的布局投影回归。
//!
//! @author kongweiguang

use super::{
    DENSITY_CHOICES, DENSITY_SEGMENT_HEIGHT_PX, DENSITY_SEGMENT_MIN_WIDTH_PX, InterfaceDensity,
    THEME_CHOICE_HEIGHT_PX, appearance_column, appearance_image_controls,
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
}

/// 视觉尺寸维持桌面紧凑密度，同时让密度控件仍有可操作的最小触控高度与宽度。
/// @author kongweiguang
#[test]
fn appearance_choices_stay_compact_and_operable() {
    assert!((56.0..=64.0).contains(&THEME_CHOICE_HEIGHT_PX));
    assert!((44.0..=48.0).contains(&DENSITY_SEGMENT_HEIGHT_PX));
    assert!((120.0..=160.0).contains(&DENSITY_SEGMENT_MIN_WIDTH_PX));
}

/// basis 为宽态两栏留足现有控件空间，窄态仍可缩到父容器宽度而不是横向撑出设置弹层。
/// @author kongweiguang
#[test]
fn appearance_columns_keep_a_wrap_basis_without_a_fixed_minimum_width() {
    use gpui_kit::{Styled, div, px};

    let mut column = appearance_column(div());
    let style = column.style();

    assert_eq!(style.flex_basis, Some(px(460.).into()));
    assert_eq!(style.flex_grow, Some(1.));
    assert_eq!(style.flex_shrink, Some(1.));
    assert_eq!(style.min_size.width, Some(px(0.).into()));
    assert_eq!(style.size.width, None);
}

/// 关闭背景不能运行图片控件构建闭包，也不留下空容器；原输入/滑块实体与草稿不归此 builder 管理。
///
/// @author kongweiguang
#[test]
fn disabled_background_omits_image_controls_without_building_them() {
    let builds = std::cell::Cell::new(0);
    let controls = appearance_image_controls(false, |controls| {
        builds.set(builds.get() + 1);
        controls
    });

    assert!(controls.is_none());
    assert_eq!(builds.get(), 0);
}

/// 开启后只构建一次并沿用纵向全宽间距，避免条件展开改变窄正文的收缩约束。
/// 此纯测不代替开关后的草稿保存和三主题实窗验证。
///
/// @author kongweiguang
#[test]
fn enabled_background_builds_one_full_width_image_group() {
    use gpui_kit::{FlexDirection, Styled, px};

    let builds = std::cell::Cell::new(0);
    let mut controls = appearance_image_controls(true, |controls| {
        builds.set(builds.get() + 1);
        controls
    })
    .expect("enabled image controls are present");
    let style = controls.style();

    assert_eq!(builds.get(), 1);
    assert_eq!(style.flex_direction, Some(FlexDirection::Column));
    assert_eq!(style.min_size.width, Some(px(0.).into()));
    assert!(style.size.width.is_some());
}
