//! 分屏卡片表面样式回归：激活态不得重新铺出整块蒙版。
//!
//! @author kongweiguang

use gpui_kit::WindowAppearance;
use kerminal_application::preferences::{AppColorScheme, ThemeMode};

use super::PaneSurfaceStyle;
use crate::theme::ThemeTokens;

/// 浅色和深色下激活/非激活都使用同一工作区材质，激活只多一圈 1px 淡强调描边。
#[test]
fn active_pane_keeps_workspace_material_and_adds_subtle_border() {
    for appearance in [WindowAppearance::Light, WindowAppearance::Dark] {
        let theme =
            ThemeTokens::for_scheme(appearance, ThemeMode::System, AppColorScheme::Kerminal);
        let active = PaneSurfaceStyle::resolve(theme, true);
        let inactive = PaneSurfaceStyle::resolve(theme, false);

        assert_eq!(active.background, theme.materials.workspace);
        assert_eq!(inactive.background, theme.materials.workspace);
        assert_eq!(active.background, inactive.background);
        assert_eq!(active.border, theme.accent.opacity(0.55));
        assert_eq!(inactive.border, theme.border);
        assert_ne!(active.border, inactive.border);
        // 1px 外框由 render_pane 的 border_1() 固定，样式解析不再携带可漂移的线宽。
    }
}

/// 壁纸或透明窗口下工作区材质本身带透明度，激活态不能改用更不透明的 panel 材质形成蒙版。
#[test]
fn translucent_workspace_stays_shared_when_a_pane_is_active() {
    let mut theme = ThemeTokens::for_scheme(
        WindowAppearance::Dark,
        ThemeMode::Dark,
        AppColorScheme::Kerminal,
    );
    theme.materials.workspace = theme.materials.workspace.opacity(0.42);
    theme.materials.panel = theme.materials.panel.opacity(0.9);

    let active = PaneSurfaceStyle::resolve(theme, true);
    let inactive = PaneSurfaceStyle::resolve(theme, false);

    assert_eq!(active.background, theme.materials.workspace);
    assert_eq!(inactive.background, theme.materials.workspace);
    assert_ne!(active.background, theme.materials.panel);
    assert!(active.background.a < 1.0);
}
