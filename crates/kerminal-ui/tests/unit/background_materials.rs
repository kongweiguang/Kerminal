//! 背景材料投影边界测试。
//!
//! @author kongweiguang

use crate::theme::{BackgroundMaterialSettings, BackgroundMaterials, ThemeTokens};
use gpui_kit::WindowAppearance;
use kerminal_application::preferences::ThemeMode;

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
