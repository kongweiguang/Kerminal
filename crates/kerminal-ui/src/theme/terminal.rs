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

/// 选区必须清楚可见，但不能强到抢走文字注意力；区间来自实测偏淡与过亮两侧的折中。
const SELECTION_MIN_CONTRAST: f32 = 1.35;
const SELECTION_MAX_CONTRAST: f32 = 2.0;
/// 二分目标取窗口内侧，给 sRGB 取整和色盘差异留出上下余量。
const SELECTION_TARGET_CONTRAST: f32 = 1.45;
/// 前景参考色达不到该对比时无法稳定命中目标，改用黑白端点重新计算。
const SELECTION_REFERENCE_MIN_CONTRAST: f32 = 1.45;
/// 文本优先：原始前景达到 3:1 就保留 ANSI 色。
const SELECTION_FOREGROUND_MIN_CONTRAST: f32 = 3.0;
/// 兜底前景需要达到正文级 4.5:1，避免选区内的低对比色文字消失。
const SELECTION_FOREGROUND_FALLBACK_CONTRAST: f32 = 4.5;
const WHITE: TerminalRgb = TerminalRgb::new(0xff, 0xff, 0xff);
const BLACK: TerminalRgb = TerminalRgb::new(0x00, 0x00, 0x00);

impl TerminalSelectionStyle {
    /// 文本优先：只有前景与选区底对比不足 3:1 才换兜底色，避免正常彩色文本被统一成白或黑。
    ///
    /// @author kongweiguang
    pub(crate) fn foreground_for(&self, effective: TerminalRgb) -> TerminalRgb {
        if contrast_ratio(effective, self.background) >= SELECTION_FOREGROUND_MIN_CONTRAST {
            effective
        } else {
            self.foreground_fallback
        }
    }
}

/// 从终端 palette 派生选区外观：底色看终端自身明暗，不跟随 UI ThemeMode 或 cursor。
///
/// 用户可以让界面保持深色却选择浅色终端方案；只用 UI 主题判断深浅会把选区画到相反的
/// 明暗方向。选区底色采用不透明实色，一次填充连续轮廓，避免与图片背景和 ANSI 底色
/// 做透明度合成后难以保证文字可读性。参考色优先取 palette 前景；前景与底色的对比
/// 不足 1.45 时无法命中目标，改用按底色亮度选出的白/黑端点，避免与 cursor 色彩绑定。
///
/// @author kongweiguang
pub(crate) fn terminal_selection_style(palette: &TerminalPalette) -> TerminalSelectionStyle {
    let background = palette.background;
    let dark = relative_luminance(background) < 0.5;
    // 深色底混入更多参考色保证可见，浅色底比例更低以免选区像刺眼的实心色块。
    let initial = if dark { 0.16 } else { 0.12 };
    let foreground_contrast = contrast_ratio(palette.foreground, background);
    let mut selection_background = mix_srgb(
        background,
        palette.foreground,
        bounded_selection_ratio(background, palette.foreground, initial),
    );

    // OSC 10 可把前景调到与底色仅 1.32–1.44 的对比，这时再混前景也到不了 1.45；
    // 改用底色远端的白/黑端点，保证选区本身始终可见。
    if foreground_contrast < SELECTION_REFERENCE_MIN_CONTRAST
        || contrast_ratio(selection_background, background) < SELECTION_MIN_CONTRAST
    {
        let reference = selection_endpoint(background);
        selection_background = mix_srgb(
            background,
            reference,
            bounded_selection_ratio(background, reference, initial),
        );
    }

    // 极端输入的兜底：若首选端点仍不足下限，比较另一端点，取实际对比更高者。
    if contrast_ratio(selection_background, background) < SELECTION_MIN_CONTRAST {
        let alternative = opposite_endpoint(selection_endpoint(background));
        let alternative_background = mix_srgb(
            background,
            alternative,
            bounded_selection_ratio(background, alternative, initial),
        );
        if contrast_ratio(alternative_background, background)
            > contrast_ratio(selection_background, background)
        {
            selection_background = alternative_background;
        }
    }

    let foreground_fallback = selection_foreground_fallback(palette, selection_background);
    TerminalSelectionStyle {
        background: selection_background,
        foreground_fallback,
    }
}

/// 按底色亮度选择远端黑白端点；只用于退化前景，不读取 cursor。
///
/// @author kongweiguang
fn selection_endpoint(background: TerminalRgb) -> TerminalRgb {
    if relative_luminance(background) < 0.5 {
        WHITE
    } else {
        BLACK
    }
}

/// 返回黑白中的另一端；仅在首选端点仍异常时尝试。
///
/// @author kongweiguang
fn opposite_endpoint(endpoint: TerminalRgb) -> TerminalRgb {
    if endpoint == WHITE { BLACK } else { WHITE }
}

/// 把初始混入比例调进可辨认但不抢眼的对比度区间，循环上限固定为 8 次。
///
/// 对比度随混入比例单调上升，因此取区间内侧 1.45 作为二分目标；命中时越过 1.35 下限，
/// 且离 2.0 上限仍有明显余量。参考色本身达不到下限时返回最大混合，调用方会改用
/// 黑白端点，避免死循环或伪造一个不存在的颜色。
///
/// @author kongweiguang
fn bounded_selection_ratio(background: TerminalRgb, reference: TerminalRgb, initial: f32) -> f32 {
    let contrast_at = |ratio| contrast_ratio(mix_srgb(background, reference, ratio), background);
    if (SELECTION_MIN_CONTRAST..=SELECTION_MAX_CONTRAST).contains(&contrast_at(initial)) {
        return initial;
    }
    let mut low = 0.0_f32;
    let mut high = 1.0_f32;
    for _ in 0..8 {
        let middle = (low + high) * 0.5;
        if contrast_at(middle) >= SELECTION_TARGET_CONTRAST {
            high = middle;
        } else {
            low = middle;
        }
    }
    high
}

/// 从 palette 前景向黑白中对比更高的一端混合，最多 8 次；仍未达标时直接返回该端点。
///
/// 先比较白、黑与选区底的实际对比再选目标，避免只用亮度 0.5 判断时把中灰选区推向
/// 对比更低的一侧。8 次混合可能不足以收敛到 4.5:1；此时返回端点本身，任一同色背景
/// 与黑白之一至少 4.58:1，因此最终兜底可读。内置浅深方案通常在混合阶段就达标，但
/// 这里不声称所有中间混合步骤都满足阈值。
///
/// @author kongweiguang
fn selection_foreground_fallback(
    palette: &TerminalPalette,
    selection_background: TerminalRgb,
) -> TerminalRgb {
    let mut foreground = palette.foreground;
    if contrast_ratio(foreground, selection_background) >= SELECTION_FOREGROUND_FALLBACK_CONTRAST {
        return foreground;
    }
    let target = if contrast_ratio(WHITE, selection_background)
        >= contrast_ratio(BLACK, selection_background)
    {
        WHITE
    } else {
        BLACK
    };
    for _ in 0..8 {
        foreground = mix_srgb(foreground, target, 0.2);
        if contrast_ratio(foreground, selection_background)
            >= SELECTION_FOREGROUND_FALLBACK_CONTRAST
        {
            return foreground;
        }
    }
    target
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
