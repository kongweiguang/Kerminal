//! 应用色板的静态颜色数据；终端 ANSI 色盘由独立模块管理。
//!
//! @author kongweiguang

use gpui_kit::{Hsla, Rgba, rgb};
use kerminal_application::preferences::AppColorScheme;

/// 各区域共享的实色角色；透明材料从这些基色投影，避免切换时累乘透明度。
#[derive(Clone, Copy)]
pub(super) struct AppPalette {
    pub background: Hsla,
    pub sidebar: Hsla,
    pub panel: Hsla,
    pub secondary: Hsla,
    pub hover: Hsla,
    pub foreground: Hsla,
    pub muted: Hsla,
    pub accent: Hsla,
    pub primary: Hsla,
    pub primary_hover: Hsla,
    pub primary_foreground: Hsla,
    pub dark_foreground: Hsla,
    pub border: Hsla,
    pub selected: Hsla,
    pub scrim: Hsla,
    pub success: Hsla,
    pub warning: Hsla,
    pub error: Hsla,
}

/// 十二套浅深色板只记录产品角色，不将外部产品的编辑器或终端主题直接当成界面契约。
#[derive(Clone, Copy)]
struct PaletteHex {
    background: u32,
    sidebar: u32,
    panel: u32,
    secondary: u32,
    hover: u32,
    foreground: u32,
    muted: u32,
    accent: u32,
    primary: u32,
    primary_hover: u32,
    border: u32,
    selected: u32,
}

/// 颜色锚点参考 Kerminal 图标、JetBrains New UI、GMark 和 VS Code Modern；文字与按钮色按对比度适配。
/// Kerminal 从图标绿色派生柔和强调色，减少长滑块与多处选中态同时出现时的视觉竞争。
/// 浅色及白字按钮使用同色系深绿；图标本身保留亮绿，键盘焦点仍使用可读的强调色。
/// 两侧辅助区保留足够的中性明暗差，避免材质叠加后与中央工作区近同色。
/// VS Code 来源：microsoft/vscode extensions/theme-defaults/themes/{dark,light}_modern.json。
///
/// @author kongweiguang
pub(super) fn palette(scheme: AppColorScheme, dark: bool) -> AppPalette {
    let colors = match (scheme, dark) {
        (AppColorScheme::Kerminal, true) => PaletteHex {
            background: 0x101012,
            sidebar: 0x242528,
            panel: 0x1c1c1e,
            secondary: 0x27272a,
            hover: 0x303033,
            foreground: 0xf4f4f5,
            muted: 0xa1a1aa,
            accent: 0x8fc4a5,
            primary: 0x167344,
            primary_hover: 0x1b7f4b,
            border: 0x36363a,
            selected: 0x263b30,
        },
        (AppColorScheme::Kerminal, false) => PaletteHex {
            background: 0xf5f5f7,
            sidebar: 0xe9ebee,
            panel: 0xffffff,
            secondary: 0xf4f4f5,
            hover: 0xe8e8eb,
            foreground: 0x18181b,
            muted: 0x595960,
            accent: 0x116b3c,
            primary: 0x167344,
            primary_hover: 0x1b7f4b,
            border: 0xd4d4d8,
            selected: 0xdcf5e6,
        },
        (AppColorScheme::JetBrains, true) => PaletteHex {
            background: 0x1e1f22,
            sidebar: 0x2b2d30,
            panel: 0x2b2d30,
            secondary: 0x393b40,
            hover: 0x43454a,
            foreground: 0xdfe1e5,
            muted: 0xaeb3bc,
            accent: 0x99bbff,
            primary: 0x315fcc,
            primary_hover: 0x396bd2,
            border: 0x4e5157,
            selected: 0x253b5b,
        },
        (AppColorScheme::JetBrains, false) => PaletteHex {
            background: 0xf7f8fa,
            sidebar: 0xe7e9ee,
            panel: 0xffffff,
            secondary: 0xebecf0,
            hover: 0xe1e3e8,
            foreground: 0x1e1f22,
            muted: 0x50545a,
            accent: 0x255bbd,
            primary: 0x245ac2,
            primary_hover: 0x306ace,
            border: 0xc7cbd1,
            selected: 0xdbe8ff,
        },
        (AppColorScheme::Fleet, true) => PaletteHex {
            background: 0x090909,
            sidebar: 0x18191b,
            panel: 0x202123,
            secondary: 0x28292b,
            hover: 0x363739,
            foreground: 0xe0e1e4,
            muted: 0xaeb3bb,
            accent: 0xb5afff,
            primary: 0x594dc8,
            primary_hover: 0x685bdb,
            border: 0x4d5055,
            selected: 0x393359,
        },
        (AppColorScheme::Fleet, false) => PaletteHex {
            background: 0xf2f2f2,
            sidebar: 0xe5e6e9,
            panel: 0xffffff,
            secondary: 0xf1f1f4,
            hover: 0xe7e7ec,
            foreground: 0x181818,
            muted: 0x555555,
            accent: 0x5648be,
            primary: 0x594cc1,
            primary_hover: 0x695bd2,
            border: 0xcdcdd4,
            selected: 0xe8e5fb,
        },
        (AppColorScheme::Xcode, true) => PaletteHex {
            background: 0x1c1d2a,
            sidebar: 0x292a30,
            panel: 0x292a30,
            secondary: 0x303139,
            hover: 0x3b3c45,
            foreground: 0xffffff,
            muted: 0xb9bac2,
            accent: 0x78b8ff,
            primary: 0x1166c2,
            primary_hover: 0x1974d1,
            border: 0x565862,
            selected: 0x283f5c,
        },
        (AppColorScheme::Xcode, false) => PaletteHex {
            background: 0xf5f5f5,
            sidebar: 0xe6e9ee,
            panel: 0xffffff,
            secondary: 0xf0f1f3,
            hover: 0xe5e6e9,
            foreground: 0x1d1d1f,
            muted: 0x57575e,
            accent: 0x005bb7,
            primary: 0x005bbb,
            primary_hover: 0x0d69c7,
            border: 0xc9c9cf,
            selected: 0xdcecff,
        },
        (AppColorScheme::Claude, true) => PaletteHex {
            background: 0x141413,
            sidebar: 0x242320,
            panel: 0x1a1918,
            secondary: 0x262624,
            hover: 0x343431,
            foreground: 0xfaf9f5,
            muted: 0xb9b7ae,
            accent: 0xf49a7c,
            primary: 0xa2462b,
            primary_hover: 0xb25033,
            border: 0x53534e,
            selected: 0x493027,
        },
        (AppColorScheme::Claude, false) => PaletteHex {
            background: 0xf5f4ed,
            sidebar: 0xe8e6de,
            panel: 0xfaf9f5,
            secondary: 0xebeae3,
            hover: 0xe1e0d9,
            foreground: 0x141413,
            muted: 0x595851,
            accent: 0xa13f24,
            primary: 0xa13f24,
            primary_hover: 0xb14c2d,
            border: 0xc9c7bc,
            selected: 0xf4ded5,
        },
        (AppColorScheme::VsCode, true) => PaletteHex {
            background: 0x1f1f1f,
            sidebar: 0x2b2b2b,
            panel: 0x202020,
            secondary: 0x313131,
            hover: 0x383838,
            foreground: 0xcccccc,
            muted: 0xadadad,
            accent: 0x4daafc,
            primary: 0x0078d4,
            primary_hover: 0x026ec1,
            border: 0x3c3c3c,
            selected: 0x264778,
        },
        (AppColorScheme::VsCode, false) => PaletteHex {
            background: 0xffffff,
            sidebar: 0xe9e9e9,
            panel: 0xffffff,
            secondary: 0xf2f2f2,
            hover: 0xe5e5e5,
            foreground: 0x3b3b3b,
            muted: 0x545454,
            accent: 0x005fb8,
            primary: 0x005fb8,
            primary_hover: 0x0258a8,
            border: 0xcecece,
            selected: 0xd6e8fa,
        },
    };
    AppPalette {
        background: rgb(colors.background).into(),
        sidebar: rgb(colors.sidebar).into(),
        panel: rgb(colors.panel).into(),
        secondary: rgb(colors.secondary).into(),
        hover: rgb(colors.hover).into(),
        foreground: rgb(colors.foreground).into(),
        muted: rgb(colors.muted).into(),
        accent: rgb(colors.accent).into(),
        primary: rgb(colors.primary).into(),
        primary_hover: rgb(colors.primary_hover).into(),
        primary_foreground: rgb(0xffffff).into(),
        dark_foreground: rgb(0x000000).into(),
        border: rgb(colors.border).into(),
        selected: rgb(colors.selected).into(),
        scrim: rgb(if dark { 0x000000 } else { 0x111113 }).into(),
        success: rgb(if dark { 0x6ee7a0 } else { 0x16743a }).into(),
        warning: rgb(if dark { 0xf9cf64 } else { 0x925009 }).into(),
        error: rgb(if dark { 0xff9999 } else { 0xb42318 }).into(),
    }
}

/// 先在真实面板上合成半透明高亮，再按 WCAG 相对亮度选择可读前景。
///
/// @author kongweiguang
pub(super) fn contrasting_foreground(
    surface: Hsla,
    background: Hsla,
    dark_foreground: Hsla,
    light_foreground: Hsla,
) -> Hsla {
    let surface = Rgba::from(surface);
    let background = surface.blend(Rgba::from(background));
    let dark_ratio = contrast_ratio(Rgba::from(dark_foreground), background);
    let light_ratio = contrast_ratio(Rgba::from(light_foreground), background);
    if dark_ratio >= light_ratio {
        dark_foreground
    } else {
        light_foreground
    }
}

/// 计算两种不透明颜色的对比度；不能用 HSL 亮度代替线性 RGB 亮度。
///
/// @author kongweiguang
fn contrast_ratio(first: Rgba, second: Rgba) -> f32 {
    let first = relative_luminance(first);
    let second = relative_luminance(second);
    (first.max(second) + 0.05) / (first.min(second) + 0.05)
}

/// 将 sRGB 组件转换为线性光，再按 WCAG 权重计算亮度。
///
/// @author kongweiguang
fn relative_luminance(color: Rgba) -> f32 {
    let linear = |value: f32| {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}
