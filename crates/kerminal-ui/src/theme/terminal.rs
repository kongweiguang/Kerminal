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
