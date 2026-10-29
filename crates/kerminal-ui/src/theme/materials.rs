//! 应用色板与背景设置的幂等材料投影。
//!
//! @author kongweiguang

use gpui_kit::{Global, Hsla};
use kerminal_application::preferences::SettingsSnapshot;

use super::ThemeTokens;

/// 只保存可重算的原始外观系数，避免跨窗口或主题切换时累乘已投影颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundMaterialSettings {
    pub(super) background_enabled: bool,
    pub(super) image_path_configured: bool,
    pub(super) background_opacity: u8,
    pub(super) window_opacity: u8,
}

impl Global for BackgroundMaterialSettings {}

impl BackgroundMaterialSettings {
    /// 从完整设置快照提取投影所需布尔值和百分比，不把本机路径放入全局状态。
    ///
    /// @author kongweiguang
    pub(crate) fn from_snapshot(snapshot: &SettingsSnapshot) -> Self {
        Self {
            background_enabled: snapshot.background_enabled,
            image_path_configured: !snapshot.background_image_path.trim().is_empty(),
            background_opacity: snapshot.background_opacity.min(100),
            window_opacity: snapshot.window_opacity.clamp(35, 100),
        }
    }
}

impl Default for BackgroundMaterialSettings {
    /// 在首次读取配置前使用完整不透明、无壁纸的稳定默认值。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            background_enabled: false,
            image_path_configured: false,
            background_opacity: 100,
            window_opacity: 100,
        }
    }
}

/// 由不透明主题色板和原始设置即时计算的表面材料；文本与交互色不参与透明混合。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BackgroundMaterials {
    pub(crate) root_base: Hsla,
    pub(crate) shell: Hsla,
    pub(crate) chrome: Hsla,
    pub(crate) workspace: Hsla,
    pub(crate) panel: Hsla,
    pub(crate) terminal_body_opacity: f32,
    pub(crate) terminal_header: Hsla,
    pub(crate) image_visible: bool,
    pub(crate) image_opacity: f32,
    pub(crate) image_veil: f32,
    /// 仅当全视口 root_base 本身 alpha 为 1 时成立，不依赖图片加载或其它表面。
    pub(crate) compositing_is_opaque: bool,
}

impl BackgroundMaterials {
    /// 从不透明基色一次性派生 root、工作区、导航与终端表面，重复保存结果保持幂等。
    ///
    /// @author kongweiguang
    pub(super) fn project(theme: &ThemeTokens, settings: BackgroundMaterialSettings) -> Self {
        let window_opacity = f32::from(settings.window_opacity.clamp(35, 100)) / 100.0;
        let image_strength = if settings.background_enabled && settings.image_path_configured {
            f32::from(settings.background_opacity.min(100)) / 100.0
        } else {
            0.0
        };
        let image_visible = image_strength > 0.0;
        let transparency_depth = 1.0 - window_opacity;
        let (chrome_base, terminal_base, min_chrome, min_terminal, base_veil) = if theme.dark {
            (0.72, 0.72, 0.36, 0.62, 0.30)
        } else {
            (0.76, 0.76, 0.40, 0.66, 0.42)
        };
        let chrome_opacity = clamp_alpha(
            if image_visible {
                (if theme.dark { 0.62 } else { 0.66 })
                    - transparency_depth * 0.06
                    - image_strength * 0.26
            } else {
                chrome_base - transparency_depth * 0.08
            },
            min_chrome,
            0.82,
        );
        let workspace_opacity = if image_visible {
            clamp_alpha(0.12 - image_strength * 0.10, 0.035, 0.12)
        } else {
            chrome_opacity
        };
        let terminal_surface_opacity = clamp_alpha(
            if image_visible {
                0.14 - image_strength * 0.12
            } else {
                terminal_base - transparency_depth * 0.08
            },
            if image_visible { 0.04 } else { min_terminal },
            0.84,
        );
        let terminal_header_opacity = if image_visible {
            clamp_alpha(0.28 - image_strength * 0.08, 0.20, 0.28)
        } else {
            clamp_alpha(
                terminal_surface_opacity + 0.05,
                if theme.dark { 0.68 } else { 0.70 },
                0.88,
            )
        };
        let image_veil = if image_visible {
            clamp_alpha(1.0 - image_strength * (1.0 - base_veil), base_veil, 1.0)
        } else {
            0.0
        };
        let root_base = theme.background.opacity(window_opacity);
        let workspace = theme.background.opacity(window_opacity * workspace_opacity);

        Self {
            root_base,
            // 页面容器不能复用 root_base 的不透明底色，否则会遮住其下的品牌背景图。
            shell: workspace,
            chrome: theme.sidebar.opacity(window_opacity * chrome_opacity),
            workspace,
            panel: theme.panel.opacity(window_opacity * chrome_opacity),
            terminal_body_opacity: window_opacity * terminal_surface_opacity,
            terminal_header: theme
                .terminal_background
                .opacity(window_opacity * terminal_header_opacity),
            image_visible,
            // 旧版以颜色遮罩承载滑块，不直接淡化整张图片；透明度滑块折算到 veil。
            image_opacity: if image_visible { 1.0 } else { 0.0 },
            image_veil,
            compositing_is_opaque: root_base.a >= 1.0,
        }
    }
}

/// 把旧版表面透明度公式的结果限幅，保护百分比边界与主题切换输入。
///
/// @author kongweiguang
fn clamp_alpha(value: f32, min: f32, max: f32) -> f32 {
    value.clamp(min, max)
}
