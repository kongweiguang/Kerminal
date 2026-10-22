//! 原生工作区与终端画布共用的语义颜色令牌。
//!
//! @author kongweiguang

use std::borrow::BorrowMut;

use gpui_kit::component::{Theme as KitTheme, ThemeMode as KitThemeMode};
use gpui_kit::{AppContext as _, Context, Global, Hsla, Pixels, Window, WindowAppearance, px, rgb};
use kerminal_application::preferences::{SettingsSnapshot, TerminalColorScheme, ThemeMode};
use kerminal_terminal::snapshot::{TerminalColor, TerminalPalette, TerminalRgb};

#[derive(Clone, Copy)]
struct TerminalThemePalette {
    foreground: u32,
    background: u32,
    cursor: u32,
    ansi: [u32; 16],
}

/// 只保存可重算的原始外观系数，避免跨窗口或主题切换时累乘已投影颜色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundMaterialSettings {
    background_enabled: bool,
    image_path_configured: bool,
    background_opacity: u8,
    window_opacity: u8,
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
    fn project(theme: &ThemeTokens, settings: BackgroundMaterialSettings) -> Self {
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

/// 浅色、深色和跟随系统的界面都从同一组语义令牌取色。
#[derive(Clone, Copy)]
pub(crate) struct ThemeTokens {
    pub background: Hsla,
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub secondary: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub subtle: Hsla,
    pub accent: Hsla,
    pub selected: Hsla,
    pub selected_foreground: Hsla,
    pub hover: Hsla,
    pub border: Hsla,
    pub terminal_background: Hsla,
    pub error: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub radius_sm: Pixels,
    pub radius: Pixels,
    pub text_xs: Pixels,
    pub text_sm: Pixels,
    pub text_body: Pixels,
    pub text_heading: Pixels,
    pub text_title: Pixels,
    pub(crate) materials: BackgroundMaterials,
    dark: bool,
}

impl ThemeTokens {
    /// 从 Kit 当前明暗模式投影同一份产品色板，窗口参数仅为兼容既有调用约定。
    ///
    /// @author kongweiguang
    pub(crate) fn current<T: 'static>(_window: &Window, cx: &Context<T>) -> Self {
        let dark = cx.read_global::<KitTheme, _>(|theme, _| theme.mode == KitThemeMode::Dark);
        let settings = cx
            .try_global::<BackgroundMaterialSettings>()
            .copied()
            .unwrap_or_default();

        Self::for_dark_with_materials(dark, settings)
    }

    /// 保留子面板原有调用约定，并作为 System 模式与窗口外观的唯一映射。
    ///
    /// @author kongweiguang
    pub fn for_appearance(appearance: WindowAppearance) -> Self {
        Self::for_dark_with_materials(
            is_dark_appearance(appearance),
            BackgroundMaterialSettings::default(),
        )
    }

    /// 用户显式主题优先；System 才跟随窗口外观变化，令牌始终按浅深成对切换。
    ///
    /// @author kongweiguang
    pub fn for_mode(appearance: WindowAppearance, mode: ThemeMode) -> Self {
        match mode {
            ThemeMode::Dark => {
                Self::for_dark_with_materials(true, BackgroundMaterialSettings::default())
            }
            ThemeMode::Light => {
                Self::for_dark_with_materials(false, BackgroundMaterialSettings::default())
            }
            ThemeMode::System => Self::for_appearance(appearance),
        }
    }

    /// 主题和材料独立计算，使多窗口 appearance 与重复设置保存均得到确定结果；正文与常规辅助信息统一为 14px，12px 仅留给次级标签。
    ///
    /// @author kongweiguang
    fn for_dark_with_materials(dark: bool, settings: BackgroundMaterialSettings) -> Self {
        let mut theme = if dark {
            Self {
                background: rgb(0x101012).into(),
                sidebar: rgb(0x151518).into(),
                panel: rgb(0x1e1e20).into(),
                secondary: rgb(0x27272a).into(),
                foreground: rgb(0xf4f4f5).into(),
                muted: rgb(0xa1a1aa).into(),
                subtle: rgb(0x8b8b95).into(),
                accent: rgb(0x60a5fa).into(),
                selected: rgb(0x1e3a5f).into(),
                selected_foreground: rgb(0xdbeafe).into(),
                hover: rgb(0x303034).into(),
                // 边界只分组，不和终端内容抢对比度；交互 hover 保留更亮的独立令牌。
                border: rgb(0x29292e).into(),
                terminal_background: rgb(0x1e1e20).into(),
                error: rgb(0xf87171).into(),
                success: rgb(0x34d399).into(),
                warning: rgb(0xfbbf24).into(),
                radius_sm: px(6.),
                radius: px(12.),
                text_xs: px(12.),
                text_sm: px(14.),
                text_body: px(14.),
                text_heading: px(16.),
                text_title: px(20.),
                materials: BackgroundMaterials {
                    root_base: rgb(0x101012).into(),
                    shell: rgb(0x101012).into(),
                    chrome: rgb(0x151518).into(),
                    workspace: rgb(0x101012).into(),
                    panel: rgb(0x1e1e20).into(),
                    terminal_body_opacity: 1.0,
                    terminal_header: rgb(0x1e1e20).into(),
                    image_visible: false,
                    image_opacity: 0.0,
                    image_veil: 0.0,
                    compositing_is_opaque: true,
                },
                dark: true,
            }
        } else {
            Self {
                background: rgb(0xf7f8fa).into(),
                sidebar: rgb(0xeff1f4).into(),
                panel: rgb(0xffffff).into(),
                secondary: rgb(0xf1f5f9).into(),
                foreground: rgb(0x1f2937).into(),
                muted: rgb(0x64748b).into(),
                subtle: rgb(0x6b7280).into(),
                accent: rgb(0x0066cc).into(),
                selected: rgb(0xdbeafe).into(),
                selected_foreground: rgb(0x1e40af).into(),
                hover: rgb(0xe8edf3).into(),
                // 浅色边界与白色面板保持低对比，避免工作台被硬框线切碎。
                border: rgb(0xe3e6eb).into(),
                terminal_background: rgb(0xffffff).into(),
                error: rgb(0xb42318).into(),
                success: rgb(0x15803d).into(),
                warning: rgb(0xb45309).into(),
                radius_sm: px(6.),
                radius: px(12.),
                text_xs: px(12.),
                text_sm: px(14.),
                text_body: px(14.),
                text_heading: px(16.),
                text_title: px(20.),
                materials: BackgroundMaterials {
                    root_base: rgb(0xf7f8fa).into(),
                    shell: rgb(0xf7f8fa).into(),
                    chrome: rgb(0xeff1f4).into(),
                    workspace: rgb(0xf7f8fa).into(),
                    panel: rgb(0xffffff).into(),
                    terminal_body_opacity: 1.0,
                    terminal_header: rgb(0xffffff).into(),
                    image_visible: false,
                    image_opacity: 0.0,
                    image_veil: 0.0,
                    compositing_is_opaque: true,
                },
                dark: false,
            }
        };
        theme.materials = BackgroundMaterials::project(&theme, settings);
        theme
    }
}

/// 统一更新 Kit 组件和自绘视图的主题；系统模式先解析窗口外观，再应用产品色板。
pub(crate) fn apply_theme_mode<T: 'static>(
    mode: ThemeMode,
    appearance: WindowAppearance,
    cx: &mut Context<T>,
) {
    let kit_mode = match mode {
        ThemeMode::Light => KitThemeMode::Light,
        ThemeMode::Dark => KitThemeMode::Dark,
        ThemeMode::System => {
            if is_dark_appearance(appearance) {
                KitThemeMode::Dark
            } else {
                KitThemeMode::Light
            }
        }
    };
    KitTheme::change(kit_mode, None, cx.borrow_mut());
    let tokens = ThemeTokens::for_mode(appearance, mode);
    KitTheme::update(cx.borrow_mut(), |theme| {
        apply_kit_palette(theme, tokens);
    });
}

/// Kit 主按钮、选中与焦点沿用旧版蓝色强调语义，并为浅深模式分别选择足够清晰的填色和文字对比；Windows 普通界面延续旧 CSS 的 Segoe UI 优先级，终端字体仍独立控制。
/// 保持 Kit 的 16px rem 基准，使 text_sm/text_xs 解析为 14px/12px，避免 14px 基准将常用控件缩到 12.25px/10.5px；应用正文与终端字号仍由各自令牌和配置控制。
///
/// @author kongweiguang
fn apply_kit_palette(theme: &mut KitTheme, tokens: ThemeTokens) {
    let dark = theme.mode.is_dark();
    let (primary, primary_hover, primary_active, primary_foreground, sidebar_primary_foreground) =
        if dark {
            (
                rgb(0x2563eb).into(),
                rgb(0x1d4ed8).into(),
                rgb(0x1e40af).into(),
                rgb(0xf1f5f9).into(),
                rgb(0xffffff).into(),
            )
        } else {
            (
                rgb(0x0066cc).into(),
                rgb(0x005bb7).into(),
                rgb(0x004c99).into(),
                rgb(0xf8fafc).into(),
                rgb(0xffffff).into(),
            )
        };
    let colors = &mut theme.colors;
    colors.background = tokens.background;
    colors.foreground = tokens.foreground;
    colors.muted_foreground = tokens.muted;
    colors.border = tokens.border;
    colors.input = tokens.border;
    colors.sidebar = tokens.sidebar;
    colors.sidebar_foreground = tokens.foreground;
    colors.sidebar_border = tokens.border;
    colors.sidebar_accent = tokens.selected;
    colors.sidebar_accent_foreground = tokens.selected_foreground;
    colors.sidebar_primary = primary;
    colors.sidebar_primary_foreground = sidebar_primary_foreground;
    colors.popover = tokens.panel;
    colors.popover_foreground = tokens.foreground;
    colors.accent = tokens.selected;
    colors.accent_foreground = tokens.selected_foreground;
    colors.primary = primary;
    colors.primary_hover = primary_hover;
    colors.primary_active = primary_active;
    colors.primary_foreground = primary_foreground;
    colors.button = tokens.secondary;
    colors.button_hover = tokens.hover;
    colors.button_active = tokens.selected;
    colors.button_foreground = tokens.foreground;
    colors.button_primary = primary;
    colors.button_primary_hover = primary_hover;
    colors.button_primary_active = primary_active;
    colors.button_primary_foreground = colors.primary_foreground;
    colors.button_secondary = tokens.secondary;
    colors.button_secondary_hover = tokens.hover;
    colors.button_secondary_active = tokens.selected;
    colors.button_secondary_foreground = tokens.foreground;
    colors.secondary = tokens.secondary;
    colors.secondary_hover = tokens.hover;
    colors.secondary_active = tokens.selected;
    colors.secondary_foreground = tokens.foreground;
    colors.list = tokens.panel;
    colors.list_even = tokens.panel;
    colors.list_head = tokens.sidebar;
    colors.list_hover = tokens.hover;
    colors.list_active = tokens.selected;
    colors.list_active_border = tokens.accent;
    colors.tab = tokens.panel;
    colors.tab_bar = tokens.background;
    colors.tab_active = tokens.selected;
    colors.tab_active_foreground = tokens.selected_foreground;
    colors.tab_foreground = tokens.muted;
    colors.table = tokens.panel;
    colors.table_even = tokens.panel;
    colors.table_hover = tokens.hover;
    colors.table_active = tokens.selected;
    colors.table_active_border = tokens.accent;
    colors.table_head = tokens.sidebar;
    colors.table_head_foreground = tokens.muted;
    colors.table_foot = tokens.sidebar;
    colors.table_foot_foreground = tokens.muted;
    colors.table_row_border = tokens.border;
    colors.selection = tokens.selected.alpha(if dark { 0.52 } else { 0.7 });
    colors.ring = tokens.accent;
    colors.drag_border = tokens.accent;
    colors.drop_target = tokens.selected;
    colors.group_box = tokens.panel;
    colors.group_box_foreground = tokens.foreground;
    colors.muted = tokens.secondary;
    colors.skeleton = tokens.secondary;
    colors.switch = tokens.secondary;
    colors.switch_thumb = tokens.foreground;
    colors.scrollbar = tokens.background;
    colors.scrollbar_thumb = tokens.subtle;
    colors.scrollbar_thumb_hover = tokens.muted;
    colors.title_bar = tokens.background;
    colors.title_bar_border = tokens.border;
    colors.status_bar = tokens.panel;
    colors.status_bar_border = tokens.border;
    colors.window_border = tokens.border;
    colors.success = tokens.success;
    colors.warning = tokens.warning;
    colors.danger = tokens.error;
    colors.link = tokens.accent;
    colors.link_hover = tokens.accent;
    theme.radius = px(7.);
    theme.radius_lg = px(8.);
    theme.font_size = px(16.);
    #[cfg(target_os = "windows")]
    if theme.font_family != "Segoe UI" {
        // Windows 延续旧 CSS 的 Latin 字体优先级，并让 DirectWrite 为中文选择匹配字形；只在 Kit 重置为系统默认时覆盖。
        theme.font_family = "Segoe UI".into();
    }
    theme.focus_ring = true;
}

/// GPUI 在 macOS 的 Vibrant appearance 同样表达浅深色，需与普通 appearance 一致映射。
fn is_dark_appearance(appearance: WindowAppearance) -> bool {
    matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    )
}

/// 把兼容设置中的色盘和实际明暗外观统一转换成 engine palette，供绘制与 OSC 查询共用。
pub(crate) fn terminal_palette_for_mode(
    appearance: WindowAppearance,
    mode: ThemeMode,
    scheme: TerminalColorScheme,
) -> TerminalPalette {
    let dark = match mode {
        ThemeMode::Dark => true,
        ThemeMode::Light => false,
        ThemeMode::System => is_dark_appearance(appearance),
    };
    let palette = match (scheme, dark) {
        (TerminalColorScheme::Kerminal, false) => TerminalThemePalette {
            foreground: 0x1d1d1f,
            background: 0xf7f7fa,
            cursor: 0x1d1d1f,
            ansi: [
                0x1d1d1f, 0xd70015, 0x248a3d, 0xb25000, 0x0a84ff, 0xaf52de, 0x007aff, 0xf2f2f7,
                0x8e8e93, 0xff453a, 0x34c759, 0xff9f0a, 0x409cff, 0xbf5af2, 0x32ade6, 0xffffff,
            ],
        },
        (TerminalColorScheme::Kerminal, true) => TerminalThemePalette {
            foreground: 0xe4e4e7,
            background: 0x1f1f21,
            cursor: 0xf8fafc,
            ansi: [
                0x18181b, 0xf87171, 0x4ade80, 0xfacc15, 0x60a5fa, 0xc084fc, 0x22d3ee, 0xe4e4e7,
                0x71717a, 0xfca5a5, 0x86efac, 0xfde68a, 0x93c5fd, 0xd8b4fe, 0x67e8f9, 0xfafafa,
            ],
        },
        (TerminalColorScheme::TokyoNight, false) => TerminalThemePalette {
            foreground: 0x343b58,
            background: 0xe1e2e7,
            cursor: 0x343b58,
            ansi: [
                0x343b58, 0x8c4351, 0x485e30, 0x8f5e15, 0x34548a, 0x5a4a78, 0x166775, 0xd5d6db,
                0x9699a3, 0x8c4351, 0x485e30, 0x8f5e15, 0x166775, 0x5a4a78, 0x0f4b6e, 0xffffff,
            ],
        },
        (TerminalColorScheme::TokyoNight, true) => TerminalThemePalette {
            foreground: 0xc0caf5,
            background: 0x1a1b26,
            cursor: 0xc0caf5,
            ansi: [
                0x15161e, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7aa2f7, 0xbb9af7, 0x2ac3de, 0xa9b1d6,
                0x565f89, 0xf7768e, 0x9ece6a, 0xe0af68, 0x7dcfff, 0xbb9af7, 0x7dcfff, 0xc0caf5,
            ],
        },
        (TerminalColorScheme::Solarized, false) => TerminalThemePalette {
            foreground: 0x657b83,
            background: 0xfdf6e3,
            cursor: 0x586e75,
            ansi: [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x839496, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        },
        (TerminalColorScheme::Solarized, true) => TerminalThemePalette {
            foreground: 0x839496,
            background: 0x002b36,
            cursor: 0x93a1a1,
            ansi: [
                0x073642, 0xdc322f, 0x859900, 0xb58900, 0x268bd2, 0xd33682, 0x2aa198, 0xeee8d5,
                0x586e75, 0xcb4b16, 0x586e75, 0x657b83, 0x839496, 0x6c71c4, 0x93a1a1, 0xfdf6e3,
            ],
        },
        (TerminalColorScheme::Github, false) => TerminalThemePalette {
            foreground: 0x24292f,
            background: 0xffffff,
            cursor: 0x24292f,
            ansi: [
                0x24292f, 0xcf222e, 0x1a7f37, 0x9a6700, 0x0969da, 0x8250df, 0x1b7c83, 0xf6f8fa,
                0x6e7781, 0xcf222e, 0x1a7f37, 0x9a6700, 0x218bff, 0xa475f9, 0x1b7c83, 0xffffff,
            ],
        },
        (TerminalColorScheme::Github, true) => TerminalThemePalette {
            foreground: 0xc9d1d9,
            background: 0x0d1117,
            cursor: 0xf0f6fc,
            ansi: [
                0x484f58, 0xff7b72, 0x3fb950, 0xd29922, 0x58a6ff, 0xbc8cff, 0x39c5cf, 0xb1bac4,
                0x6e7681, 0xffa198, 0x7ee787, 0xd29922, 0x79c0ff, 0xd2a8ff, 0x56d4dd, 0xf0f6fc,
            ],
        },
    };
    TerminalPalette::from_ansi(
        rgb_from_hex(palette.foreground),
        rgb_from_hex(palette.background),
        rgb_from_hex(palette.cursor),
        palette.ansi.map(rgb_from_hex),
    )
}

/// 让终端面板底色直接来自同一份 engine palette，避免设置 scheme 后外沿与文本背景不一致。
pub(crate) fn terminal_background_color(palette: TerminalPalette) -> Hsla {
    let color = palette.resolve(TerminalColor::DefaultBackground);
    gpui_kit::rgb(u32::from(color.red) << 16 | u32::from(color.green) << 8 | u32::from(color.blue))
        .into()
}

/// 把已校验的 RGB 字面量转换为纯终端模型颜色，不引入 UI 色彩表示。
fn rgb_from_hex(hex: u32) -> TerminalRgb {
    TerminalRgb::new(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

#[cfg(test)]
#[path = "../tests/unit/background_materials.rs"]
mod background_materials_tests;
