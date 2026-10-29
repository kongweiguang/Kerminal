//! 应用色板、Kit 投影与终端独立性的回归。
//!
//! @author kongweiguang

use gpui_kit::{Hsla, Rgba, WindowAppearance};
use kerminal_application::preferences::{AppColorScheme, TerminalColorScheme, ThemeMode};

use super::{ThemeTokens, terminal_palette_for_mode};

/// 用线性 RGB 测量真实对比度，透明色先按指定表面合成。
///
/// @author kongweiguang
fn contrast(foreground: Hsla, background: Hsla, surface: Hsla) -> f32 {
    let surface = Rgba::from(surface);
    let background = surface.blend(Rgba::from(background));
    let foreground = background.blend(Rgba::from(foreground));
    let luminance = |color: Rgba| {
        let linear = |value: f32| {
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
    };
    let a = luminance(foreground);
    let b = luminance(background);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// 每套浅深外观均满足普通文字、强调文字、按钮和输入边框的最低对比度。
///
/// @author kongweiguang
#[test]
fn palettes_keep_text_and_control_contrast() {
    for scheme in AppColorScheme::ALL {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let theme = ThemeTokens::for_scheme(WindowAppearance::Dark, mode, scheme);
            for background in [
                theme.background,
                theme.sidebar,
                theme.panel,
                theme.secondary,
                theme.hover,
            ] {
                for foreground in [theme.foreground, theme.muted, theme.accent] {
                    let ratio = contrast(foreground, background, theme.panel);
                    assert!(ratio >= 4.5, "{scheme:?} {mode:?}: text contrast {ratio}");
                }
            }
            for background in [theme.primary, theme.primary_hover, theme.selected] {
                let foreground = if background == theme.selected {
                    theme.selected_foreground
                } else {
                    theme.primary_foreground
                };
                let ratio = contrast(foreground, background, theme.panel);
                assert!(
                    ratio >= 4.5,
                    "{scheme:?} {mode:?}: selection contrast {ratio}"
                );
            }
            let ratio = contrast(theme.strong_border(), theme.panel, theme.panel);
            assert!(
                ratio >= 3.0,
                "{scheme:?} {mode:?}: input border contrast {ratio}"
            );
            for background in [theme.accent, theme.surface_selected(), theme.panel] {
                let ratio = contrast(theme.foreground_on(background), background, theme.panel);
                assert!(
                    ratio >= 4.5,
                    "{scheme:?} {mode:?}: highlight contrast {ratio}"
                );
            }
        }
    }
}

/// 显式模式忽略系统明暗，系统模式只切换同一配色的浅深版本。
///
/// @author kongweiguang
#[test]
fn system_appearance_preserves_the_selected_palette() {
    for scheme in AppColorScheme::ALL {
        let dark = ThemeTokens::for_scheme(WindowAppearance::Light, ThemeMode::Dark, scheme);
        let light = ThemeTokens::for_scheme(WindowAppearance::Dark, ThemeMode::Light, scheme);
        assert_ne!(dark.background, light.background);
        assert_eq!(
            dark.background,
            ThemeTokens::for_scheme(WindowAppearance::Dark, ThemeMode::System, scheme).background
        );
        assert_eq!(
            dark.background,
            ThemeTokens::for_scheme(WindowAppearance::VibrantDark, ThemeMode::System, scheme)
                .background
        );
        assert_eq!(
            light.background,
            ThemeTokens::for_scheme(WindowAppearance::Light, ThemeMode::System, scheme).background
        );
        assert_eq!(
            light.background,
            ThemeTokens::for_scheme(WindowAppearance::VibrantLight, ThemeMode::System, scheme)
                .background
        );
        let workbench = dark.for_workbench();
        let tool = dark.for_tool_panel();
        let settings = dark.for_settings();
        assert_eq!(workbench.background, tool.background);
        assert_eq!(tool.accent, settings.accent);
        assert_eq!(tool.foreground, settings.foreground);
    }
}

/// 所有命名方案必须呈现不同的应用色板，避免设置选项只改变标签。
///
/// @author kongweiguang
#[test]
fn palette_families_have_distinct_visible_colors() {
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        for (index, scheme) in AppColorScheme::ALL.iter().enumerate() {
            let left = ThemeTokens::for_scheme(WindowAppearance::Dark, mode, *scheme);
            for other in AppColorScheme::ALL.iter().skip(index + 1) {
                let right = ThemeTokens::for_scheme(WindowAppearance::Dark, mode, *other);
                assert!(
                    left.background != right.background || left.accent != right.accent,
                    "{scheme:?} and {other:?} have identical visible colors"
                );
            }
        }
    }
}

/// 全局主题更新在系统明暗变化后保留配色，并同步 Kit、自绘区域和独立终端色盘。
///
/// @author kongweiguang
#[gpui_kit::test]
fn runtime_theme_switch_keeps_kit_and_views_in_sync(app: &mut gpui_kit::TestAppContext) {
    use gpui_kit::component::Theme as KitTheme;
    use gpui_kit::{Context, IntoElement, Render, Window, div};

    struct ThemeHarness;
    impl Render for ThemeHarness {
        /// 空视图只提供真实窗口和 Context，主题行为仍经过生产更新入口。
        ///
        /// @author kongweiguang
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    app.update(crate::init);
    let (view, visual) = app.add_window_view(|_, _| ThemeHarness);
    visual.update(|window, app| {
        view.update(app, |_, cx| {
            let terminal = terminal_palette_for_mode(
                WindowAppearance::Dark,
                ThemeMode::System,
                TerminalColorScheme::TokyoNight,
            );
            for scheme in AppColorScheme::ALL {
                super::apply_app_theme(ThemeMode::System, scheme, WindowAppearance::Dark, cx);
                let expected =
                    ThemeTokens::for_scheme(WindowAppearance::Dark, ThemeMode::System, scheme);
                let current = ThemeTokens::current(window, cx);
                assert_eq!(current.panel, expected.panel);
                assert_eq!(KitTheme::global(cx).colors.primary, expected.primary);
                assert_eq!(KitTheme::global(cx).colors.popover, expected.panel);
                assert_eq!(KitTheme::global(cx).colors.slider_bar, expected.primary);
                assert_eq!(KitTheme::global(cx).colors.caret, expected.accent);
                assert_eq!(KitTheme::global(cx).colors.button_danger, expected.error);
                assert_eq!(
                    terminal,
                    terminal_palette_for_mode(
                        WindowAppearance::Dark,
                        ThemeMode::System,
                        TerminalColorScheme::TokyoNight
                    )
                );

                super::apply_theme_mode(ThemeMode::System, WindowAppearance::Light, cx);
                let expected =
                    ThemeTokens::for_scheme(WindowAppearance::Light, ThemeMode::System, scheme);
                assert_eq!(ThemeTokens::current(window, cx).panel, expected.panel);
                assert_eq!(KitTheme::global(cx).colors.primary, expected.primary);
                let colors = KitTheme::global(cx).colors;
                for (foreground, backgrounds) in [
                    (
                        colors.danger_foreground,
                        [colors.danger, colors.danger_hover, colors.danger_active],
                    ),
                    (
                        colors.success_foreground,
                        [colors.success, colors.success_hover, colors.success_active],
                    ),
                    (
                        colors.warning_foreground,
                        [colors.warning, colors.warning_hover, colors.warning_active],
                    ),
                ] {
                    for background in backgrounds {
                        assert!(contrast(foreground, background, expected.panel) >= 4.5);
                    }
                }
            }
        });
    });
}

/// 品牌绿贯穿强调、按钮和选中态；深色强调降低饱和度，避免长滑块抢占终端内容的视觉层级。
///
/// @author kongweiguang
#[test]
fn kerminal_brand_uses_logo_green_in_both_appearances() {
    for mode in [ThemeMode::Dark, ThemeMode::Light] {
        let theme = ThemeTokens::for_scheme(WindowAppearance::Dark, mode, AppColorScheme::Kerminal);
        for color in [
            theme.accent,
            theme.primary,
            theme.primary_hover,
            theme.selected,
        ] {
            let rgb = Rgba::from(color);
            assert!(
                rgb.g > rgb.r && rgb.g > rgb.b,
                "Kerminal brand roles must remain green"
            );
        }
        if mode == ThemeMode::Dark {
            assert!(theme.accent.s < 0.5, "Dark emphasis should remain subdued");
        }
    }
}

/// 在默认材质合成后保留至少 8 级 sRGB 色阶差，防止侧栏与中央再次近同色。
/// 此为辅助区层次的产品下限；文字和关键控件的 WCAG 对比度由独立测试覆盖。
///
/// @author kongweiguang
#[test]
fn auxiliary_surfaces_remain_distinct_after_material_compositing() {
    for scheme in AppColorScheme::ALL {
        for mode in [ThemeMode::Light, ThemeMode::Dark] {
            let theme = ThemeTokens::for_scheme(WindowAppearance::Dark, mode, scheme);
            let center = Rgba::from(theme.background);
            assert_eq!(theme.tool_panel_surface, theme.sidebar);
            for surface in [theme.materials.chrome, theme.tool_panel_surface] {
                let actual = center.blend(Rgba::from(surface));
                let difference = (actual.r - center.r)
                    .abs()
                    .max((actual.g - center.g).abs())
                    .max((actual.b - center.b).abs());
                assert!(
                    difference >= 8.0 / 255.0,
                    "{scheme:?} {mode:?}: auxiliary surface merges into workspace ({difference})"
                );
            }
        }
    }
}
