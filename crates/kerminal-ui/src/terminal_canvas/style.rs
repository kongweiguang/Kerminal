//! 终端文本属性、前景调暗和覆盖层颜色的 GPUI 投影。
//!
//! @author kongweiguang

use gpui_kit::{
    FontStyle, FontWeight, Hsla, StrikethroughStyle, TextRun, UnderlineStyle as GpuiUnderlineStyle,
    px,
};
use kerminal_terminal::snapshot::{
    TerminalColor, TerminalPalette, TerminalRgb, TerminalStyle,
    UnderlineStyle as TerminalUnderlineStyle,
};

use super::overlay::{CellOverlayPaint, TerminalOverlayKind};

/// 将终端样式映射到文字属性，网格背景由独立 cell quad 绘制以填满空白列。
///
/// @author kongweiguang
pub(super) fn apply_terminal_style(
    run: &mut TextRun,
    style: TerminalStyle,
    palette: &TerminalPalette,
) {
    let (foreground, _) = if style.attributes.inverse {
        (style.background, style.foreground)
    } else {
        (style.foreground, style.background)
    };
    run.color = terminal_rgb(resolve_foreground(
        foreground,
        style.attributes.dim,
        palette,
    ));
    run.background_color = None;
    if style.attributes.bold {
        run.font.weight = FontWeight::BOLD;
    }
    if style.attributes.italic {
        run.font.style = FontStyle::Italic;
    }
    run.underline = match style.attributes.underline {
        TerminalUnderlineStyle::None => None,
        TerminalUnderlineStyle::Curly => Some(GpuiUnderlineStyle {
            thickness: px(1.0),
            color: style
                .underline_color
                .map(|color| terminal_rgb(palette.resolve(color))),
            wavy: true,
        }),
        TerminalUnderlineStyle::Double => Some(GpuiUnderlineStyle {
            thickness: px(2.0),
            color: style
                .underline_color
                .map(|color| terminal_rgb(palette.resolve(color))),
            wavy: false,
        }),
        TerminalUnderlineStyle::Single
        | TerminalUnderlineStyle::Dotted
        | TerminalUnderlineStyle::Dashed => Some(GpuiUnderlineStyle {
            thickness: px(1.0),
            color: style
                .underline_color
                .map(|color| terminal_rgb(palette.resolve(color))),
            wavy: false,
        }),
    };
    run.strikethrough = style
        .attributes
        .strikethrough
        .then_some(StrikethroughStyle {
            thickness: px(1.0),
            color: Some(run.color),
        });
    if style.attributes.hidden {
        hide_text(run);
    }
}

/// 根据 cell 的颜色语义处理 dim；已 dim 的 ANSI 颜色不会被重复压暗。
///
/// @author kongweiguang
fn resolve_foreground(color: TerminalColor, dim: bool, palette: &TerminalPalette) -> TerminalRgb {
    if !dim {
        return palette.resolve(color);
    }
    match color {
        TerminalColor::DefaultForeground | TerminalColor::BrightForeground => {
            palette.resolve(TerminalColor::DimForeground)
        }
        TerminalColor::Ansi(index @ 0..=7) => palette.resolve(TerminalColor::DimAnsi(index)),
        TerminalColor::DimAnsi(_) | TerminalColor::DimForeground => palette.resolve(color),
        TerminalColor::DefaultBackground
        | TerminalColor::Cursor
        | TerminalColor::Ansi(_)
        | TerminalColor::Indexed(_)
        | TerminalColor::Rgb { .. } => dim_rgb(palette.resolve(color)),
    }
}

/// 令 hidden 属性只隐藏 glyph 与装饰，不抹去终端背景或其他 cell overlay。
///
/// @author kongweiguang
pub(super) fn hide_text(run: &mut TextRun) {
    run.color.a = 0.0;
    run.underline = None;
    run.strikethrough = None;
}

/// 应用搜索、选择或主题化关键词样式；仅覆盖调用方显式配置的颜色分量。
///
/// @author kongweiguang
pub(super) fn apply_overlay_paint(
    run: &mut TextRun,
    paint: CellOverlayPaint,
    palette: &TerminalPalette,
) {
    if let Some(style) = paint.style {
        if let Some(foreground) = style.foreground {
            run.color = terminal_rgb(foreground);
        }
        if let Some(background) = style.background {
            run.background_color = Some(terminal_rgb(background));
        }
        return;
    }

    let (foreground, background) = default_overlay_colors(paint.kind, palette);
    run.color = terminal_rgb(foreground);
    run.background_color = Some(terminal_rgb(background));
}

/// 选择/搜索默认色复用终端 palette，主题关键词颜色由 overlay 显式提供。
///
/// @author kongweiguang
pub(super) fn default_overlay_colors(
    kind: TerminalOverlayKind,
    palette: &TerminalPalette,
) -> (TerminalRgb, TerminalRgb) {
    match kind {
        TerminalOverlayKind::Keyword | TerminalOverlayKind::Search => (
            palette.resolve(TerminalColor::DefaultForeground),
            palette.resolve(TerminalColor::Ansi(3)),
        ),
        TerminalOverlayKind::ActiveSearch => (
            palette.resolve(TerminalColor::DefaultBackground),
            palette.resolve(TerminalColor::Ansi(3)),
        ),
        TerminalOverlayKind::Selection => (
            palette.resolve(TerminalColor::DefaultBackground),
            palette.resolve(TerminalColor::Cursor),
        ),
    }
}

/// 按 RGB 线性压低不带 palette 语义的 Indexed/RGB 前景，保持 dim 的可预测结果。
///
/// @author kongweiguang
fn dim_rgb(color: TerminalRgb) -> TerminalRgb {
    TerminalRgb::new(
        ((u16::from(color.red) * 2) / 3) as u8,
        ((u16::from(color.green) * 2) / 3) as u8,
        ((u16::from(color.blue) * 2) / 3) as u8,
    )
}

/// 把终端 RGB 转成 GPUI 色值，palette 的解析已在纯终端层完成。
///
/// @author kongweiguang
pub(super) fn terminal_rgb(color: TerminalRgb) -> Hsla {
    gpui_kit::rgb(u32::from(color.red) << 16 | u32::from(color.green) << 8 | u32::from(color.blue))
        .into()
}
