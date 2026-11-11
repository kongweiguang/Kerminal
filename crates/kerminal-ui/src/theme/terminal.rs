//! 终端配色独立于应用色板，保持 ANSI、光标与默认颜色的原有语义。
//!
//! @author kongweiguang

use gpui_kit::{Hsla, WindowAppearance};
use kerminal_application::preferences::{TerminalColorScheme, ThemeMode};
use kerminal_terminal::snapshot::{TerminalColor, TerminalPalette, TerminalRgb};

use super::is_dark_appearance;

/// 保留终端兼容色值，避免应用外观切换改变正在运行的终端色盘。
#[derive(Clone, Copy)]
struct TerminalThemePalette {
    foreground: u32,
    background: u32,
    cursor: u32,
    ansi: [u32; 16],
}

/// 把兼容设置中的色盘和实际明暗外观统一转换成 engine palette，供绘制与 OSC 查询共用。
///
/// @author kongweiguang
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
///
/// @author kongweiguang
pub(crate) fn terminal_background_color(palette: TerminalPalette) -> Hsla {
    let color = palette.resolve(TerminalColor::DefaultBackground);
    gpui_kit::rgb(u32::from(color.red) << 16 | u32::from(color.green) << 8 | u32::from(color.blue))
        .into()
}

/// 选区底色与前景兜底只在 UI 内派生，终端引擎、持久化与 MCP 都不需要理解选择外观。
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TerminalSelectionStyle {
    /// 选区使用不透明实色；透明叠色会和任意 ANSI cell 背景组合出不可预测的最终对比度。
    pub(crate) background: TerminalRgb,
    /// 原始 ANSI 前景在选区底上不足 3:1 时改用的可读前景。
    pub(crate) foreground_fallback: TerminalRgb,
}

impl TerminalSelectionStyle {
    /// 文本优先：只有前景与选区底对比不足 3:1 才换兜底色，避免正常彩色文本被统一成白或黑。
    ///
    /// @author kongweiguang
    pub(crate) fn foreground_for(&self, effective: TerminalRgb) -> TerminalRgb {
        if contrast_ratio(effective, self.background) >= 3.0 {
            effective
        } else {
            self.foreground_fallback
        }
    }
}

/// 从终端 palette 派生选区外观：底色看终端自身明暗，不跟随 UI ThemeMode。
///
/// 用户可以让界面保持深色却选择浅色终端方案；只用 UI 主题判断深浅会把选区画到相反的
/// 明暗方向。选区底色采用不透明实色，一次填充连续轮廓，避免与图片背景和 ANSI 底色
/// 做透明度合成后难以保证文字可读性。
///
/// @author kongweiguang
pub(crate) fn terminal_selection_style(palette: &TerminalPalette) -> TerminalSelectionStyle {
    let background = palette.background;
    let dark = relative_luminance(background) < 0.5;
    // 退化色盘的前景几乎与底色同色时，cursor 仍能提供比噪声更可靠的参考方向；
    // 它只作为混合参考，不让选区直接等于光标色。
    let reference = if contrast_ratio(palette.foreground, background) >= 1.3 {
        palette.foreground
    } else {
        palette.cursor
    };
    // 深色底混入更多参考色保证可见，浅色底比例更低以免选区像刺眼的实心色块。
    let initial = if dark { 0.16 } else { 0.12 };
    let ratio = bounded_selection_ratio(background, reference, initial);
    let selection_background = mix_srgb(background, reference, ratio);
    let foreground_fallback = selection_foreground_fallback(palette, selection_background);
    TerminalSelectionStyle {
        background: selection_background,
        foreground_fallback,
    }
}

/// 把初始混入比例调进可辨认但不抢眼的对比度区间，循环上限固定为 8 次。
///
/// 对比度随混入比例单调上升，因此取区间中部 1.5 作为二分目标：命中时必然越过 1.25 下限，
/// 且离 2.0 上限仍有明显余量。参考色本身达不到下限时只能返回最大混合，避免死循环或
/// 伪造一个不存在的颜色。
///
/// @author kongweiguang
fn bounded_selection_ratio(background: TerminalRgb, reference: TerminalRgb, initial: f32) -> f32 {
    let contrast_at = |ratio| contrast_ratio(mix_srgb(background, reference, ratio), background);
    if (1.25..=2.0).contains(&contrast_at(initial)) {
        return initial;
    }
    let mut low = 0.0_f32;
    let mut high = 1.0_f32;
    for _ in 0..8 {
        let middle = (low + high) * 0.5;
        if contrast_at(middle) >= 1.5 {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

/// 从 palette 前景向远离选区底的方向退到白或黑，保证兜底文字仍有 4.5:1 可读性。
///
/// 固定步长最多推进 8 次；浅色选区底向黑、深色选区底向白，避免兜底文字和选区底同向
/// 混合后又回到低对比度。palette 前景已经达标时直接沿用，保持原有配色习惯。
///
/// @author kongweiguang
fn selection_foreground_fallback(
    palette: &TerminalPalette,
    selection_background: TerminalRgb,
) -> TerminalRgb {
    let mut foreground = palette.foreground;
    if contrast_ratio(foreground, selection_background) >= 4.5 {
        return foreground;
    }
    let target = if relative_luminance(selection_background) < 0.5 {
        TerminalRgb::new(0xff, 0xff, 0xff)
    } else {
        TerminalRgb::new(0x00, 0x00, 0x00)
    };
    for _ in 0..8 {
        foreground = mix_srgb(foreground, target, 0.2);
        if contrast_ratio(foreground, selection_background) >= 4.5 {
            break;
        }
    }
    foreground
}

/// 按 WCAG 相对亮度把选区明暗判断绑定到真实感知亮度，而不是简单比较通道均值。
///
/// @author kongweiguang
pub(crate) fn relative_luminance(color: TerminalRgb) -> f32 {
    let linear = |channel: u8| {
        let value = f32::from(channel) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.red) + 0.7152 * linear(color.green) + 0.0722 * linear(color.blue)
}

/// 用 WCAG 对比度衡量选区与底色、文字与选区底的关系，测试与生产共用同一公式。
///
/// @author kongweiguang
pub(crate) fn contrast_ratio(left: TerminalRgb, right: TerminalRgb) -> f32 {
    let left = relative_luminance(left);
    let right = relative_luminance(right);
    (left.max(right) + 0.05) / (left.min(right) + 0.05)
}

/// 在 sRGB 通道上做线性混合并四舍五入回 8 位色，保持与终端 palette 的离散色值一致。
///
/// @author kongweiguang
fn mix_srgb(from: TerminalRgb, to: TerminalRgb, ratio: f32) -> TerminalRgb {
    let ratio = ratio.clamp(0.0, 1.0);
    let channel = |from: u8, to: u8| {
        (f32::from(from) * (1.0 - ratio) + f32::from(to) * ratio)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    TerminalRgb::new(
        channel(from.red, to.red),
        channel(from.green, to.green),
        channel(from.blue, to.blue),
    )
}

/// 把已校验的 RGB 字面量转换为纯终端模型颜色，不引入 UI 色彩表示。
///
/// @author kongweiguang
fn rgb_from_hex(hex: u32) -> TerminalRgb {
    TerminalRgb::new(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}
