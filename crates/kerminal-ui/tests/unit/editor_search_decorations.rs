//! 原生编辑器查找字形装饰回归。
//!
//! @author kongweiguang

use gpui_kit::WindowAppearance;

use super::{SearchDecorationStyles, search_decorations};
use crate::theme::ThemeTokens;

/// CJK 命中继续使用 UTF-8 半开字节范围，活动项与其它匹配应用不同主题色。
#[test]
fn decorations_keep_utf8_ranges_and_distinguish_active_match() {
    let content = "终端终端";
    let ranges = vec![0..6, 6..content.len()];
    let styles =
        SearchDecorationStyles::from_theme(ThemeTokens::for_appearance(WindowAppearance::Dark));
    let decorations = search_decorations(&ranges, Some(&ranges[1]), styles);

    assert_eq!(content.len(), 12);
    assert_eq!(decorations.len(), 2);
    assert_eq!(decorations[0].range, ranges[0]);
    assert_eq!(decorations[1].range, ranges[1]);
    assert_ne!(decorations[0].style, decorations[1].style);
    assert!(decorations[0].style.background_color.is_some());
    assert!(decorations[1].style.background_color.is_some());
}

/// 普通与活动命中沿用 ThemeTokens 的浅深配对，避开固定单主题色造成的不可读。
#[test]
fn paired_themes_keep_search_styles_distinct_and_theme_aware() {
    let light =
        SearchDecorationStyles::from_theme(ThemeTokens::for_appearance(WindowAppearance::Light));
    let dark =
        SearchDecorationStyles::from_theme(ThemeTokens::for_appearance(WindowAppearance::Dark));

    assert_ne!(light.normal.background_color, light.active.background_color);
    assert_ne!(dark.normal.background_color, dark.active.background_color);
    assert_ne!(light.normal.background_color, dark.normal.background_color);
    assert_ne!(light.active.background_color, dark.active.background_color);
}
