//! 背景材料投影边界测试。
//!
//! @author kongweiguang

use crate::theme::{BackgroundMaterialSettings, BackgroundMaterials, ThemeTokens};
use gpui_kit::WindowAppearance;
use kerminal_application::preferences::{AppColorScheme, ThemeMode};

impl ThemeTokens {
    /// 旧有组件测试采用默认色板，生产入口始终使用完整的配色身份。
    ///
    /// @author kongweiguang
    pub(crate) fn for_appearance(appearance: WindowAppearance) -> Self {
        Self::for_scheme(appearance, ThemeMode::System, AppColorScheme::Kerminal)
    }

    /// 保持既有材料测试的默认配色语义，避免为测试向生产模块加入闲置入口。
    ///
    /// @author kongweiguang
    pub(crate) fn for_mode(appearance: WindowAppearance, mode: ThemeMode) -> Self {
        Self::for_scheme(appearance, mode, AppColorScheme::Kerminal)
    }

    /// 从基色投影材料，测试不能复用已降低透明度的表面。
    ///
    /// @author kongweiguang
    fn for_dark_with_materials(dark: bool, settings: BackgroundMaterialSettings) -> Self {
        Self::for_palette_with_materials(dark, AppColorScheme::Kerminal, settings)
    }
}

/// 有壁纸时只让全视口底色保持不透明，工作区材料必须透出图片。
#[test]
fn wallpaper_materials_keep_only_the_root_base_opaque() {
    let theme = ThemeTokens::for_dark_with_materials(
        true,
        BackgroundMaterialSettings {
            background_enabled: true,
            image_path_configured: true,
            background_opacity: 100,
            window_opacity: 100,
        },
    );
    let materials = theme.materials;

    assert!(materials.compositing_is_opaque);
    assert_eq!(materials.root_base.a, 1.0);
    assert!(materials.shell.a < 1.0);
    assert!(materials.chrome.a < 1.0);
    assert!(materials.workspace.a < 1.0);
    assert!(materials.terminal_body_opacity < 1.0);
    assert!(materials.image_visible);
    assert_eq!(materials.image_opacity, 1.0);
}

/// 不透明 root base 不能掩盖窗口透明度设置请求的桌面合成能力。
#[test]
fn reduced_window_opacity_marks_compositing_as_translucent() {
    let theme = ThemeTokens::for_dark_with_materials(
        false,
        BackgroundMaterialSettings {
            background_enabled: false,
            image_path_configured: false,
            background_opacity: 100,
            window_opacity: 74,
        },
    );

    assert!(!theme.materials.compositing_is_opaque);
    assert!(theme.materials.root_base.a < 1.0);
    assert_eq!(theme.materials.image_opacity, 0.0);
}

/// 多次应用同一设置从不透明色板重新投影，避免浅深切换和保存后 alpha 累乘。
#[test]
fn material_projection_is_idempotent_from_base_palette() {
    let settings = BackgroundMaterialSettings {
        background_enabled: true,
        image_path_configured: true,
        background_opacity: 65,
        window_opacity: 88,
    };
    let theme = ThemeTokens::for_mode(WindowAppearance::Dark, ThemeMode::Dark);
    let first = BackgroundMaterials::project(&theme, settings);
    let second = BackgroundMaterials::project(&theme, settings);

    assert_eq!(first, second);
    assert!(first.shell.a < 1.0);
    assert!(first.image_veil > 0.0);
}

/// 嵌套内容容器不能累加宿主的透明材质；浮层、字段与正文颜色仍须保持可读。
///
/// @author kongweiguang
#[test]
fn tool_content_leaves_surface_compositing_to_its_host() {
    for scheme in AppColorScheme::ALL {
        for dark in [false, true] {
            for (image, opacity) in [(false, 100), (false, 35), (true, 100), (true, 74)] {
                let base = ThemeTokens::for_palette_with_materials(
                    dark,
                    scheme,
                    BackgroundMaterialSettings {
                        background_enabled: image,
                        image_path_configured: image,
                        background_opacity: 100,
                        window_opacity: opacity,
                    },
                );
                let content = base.for_tool_panel();
                let repeated = content.for_tool_panel();
                assert!(base.materials.chrome.a > 0.);
                assert!(base.materials.chrome.a < 1.);
                assert_eq!(content.tool_panel_surface.a, 0.);
                assert_eq!(content.materials.shell.a, 0.);
                assert_eq!(content.materials.chrome.a, 0.);
                assert_eq!(content.materials, repeated.materials);
                assert_eq!(content.panel, base.panel);
                assert_eq!(content.code_surface(), base.panel);
                assert_eq!(content.content_surface(), repeated.content_surface());
                if image || opacity < 100 {
                    assert!(content.content_surface().a < 0.3);
                } else {
                    assert_eq!(content.content_surface(), base.panel);
                }
                assert_eq!(content.foreground, base.foreground);
                assert_eq!(content.field_surface(), base.field_surface());
                assert_eq!(content.floating_surface, base.floating_surface);
            }
        }
    }
}

/// 工作台局部配色不能重复降低材质透明度，也不能因跟随系统主题而污染原始令牌。
///
/// @author kongweiguang
#[test]
fn workbench_palette_preserves_material_alpha_in_all_theme_modes() {
    for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            let base = ThemeTokens::for_mode(appearance, mode);
            let first = base.for_workbench();
            let repeated = first.for_workbench();
            assert_eq!(first.materials, repeated.materials);
            assert_eq!(first.sidebar, repeated.sidebar);
            assert_eq!(first.materials.chrome.a, base.materials.chrome.a);
            assert_eq!(first.materials.workspace.a, base.materials.workspace.a);
            assert_eq!(first.materials.panel.a, base.materials.panel.a);
            assert_eq!(first.dark, base.dark);
            assert_ne!(first.foreground, first.sidebar);
        }
    }
}

/// 无背景滤镜的平台必须遮住底层正文；同一共享令牌同时约束菜单、弹框和自由工具窗。
///
/// @author kongweiguang
#[test]
fn floating_surfaces_keep_content_readable_without_a_backdrop_filter() {
    for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
        for mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
            let theme = ThemeTokens::for_mode(appearance, mode);
            assert_eq!(theme.floating_surface, theme.tool_panel_floating_surface);
            assert_eq!(theme.floating_surface, theme.context_menu_surface());
            #[cfg(target_os = "windows")]
            assert!((0.9..1.0).contains(&theme.floating_surface.a));
            #[cfg(not(target_os = "windows"))]
            assert_eq!(theme.floating_surface.a, 1.0);
        }
    }
}
