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
use crate::theme::TerminalSelectionStyle;

/// 将终端样式映射到文字属性，网格背景由独立 cell quad 绘制以填满空白列。
///
/// @author kongweiguang
pub(super) fn apply_terminal_style(
    run: &mut TextRun,
    style: TerminalStyle,
    palette: &TerminalPalette,
) {
    run.color = terminal_rgb(effective_terminal_foreground(style, palette));
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

/// 提取 cell 在 inverse 与 dim 之后的实际前景，供普通文字和选区兜底共用同一判断。
///
/// 选区需要知道用户真正看到的前景色，而不是 style.foreground 的原始语义；这样
/// inverse cell 在选区中仍保持反色语义，dim 文本也不会被误判为高对比。
///
/// @author kongweiguang
pub(super) fn effective_terminal_foreground(
    style: TerminalStyle,
    palette: &TerminalPalette,
) -> TerminalRgb {
    let foreground = if style.attributes.inverse {
        style.background
    } else {
        style.foreground
    };
    resolve_foreground(foreground, style.attributes.dim, palette)
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

/// 应用搜索、关键词或选择样式；显式主题色优先，否则只读取对应覆盖类型的默认色。
///
/// 选区背景已由连续轮廓统一绘制，这里只决定文字颜色且不重复写入 background，
/// 避免每个 glyph 再画一块与轮廓错位的矩形。
///
/// @author kongweiguang
pub(super) fn apply_overlay_paint(
    run: &mut TextRun,
    paint: CellOverlayPaint,
    palette: &TerminalPalette,
    effective_foreground: TerminalRgb,
    selection: TerminalSelectionStyle,
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

    if paint.kind == TerminalOverlayKind::Selection {
        run.color = terminal_rgb(selection.foreground_for(effective_foreground));
        run.background_color = None;
        return;
    }

    if let Some((foreground, background)) = highlight_colors(paint.kind, palette) {
        run.color = terminal_rgb(foreground);
        run.background_color = Some(terminal_rgb(background));
    }
}

/// 搜索与关键词仍复用终端 palette；选区由连续几何单独绘制，因此不提供通用覆盖色。
///
/// @author kongweiguang
pub(super) fn highlight_colors(
    kind: TerminalOverlayKind,
    palette: &TerminalPalette,
) -> Option<(TerminalRgb, TerminalRgb)> {
    match kind {
        TerminalOverlayKind::Keyword | TerminalOverlayKind::Search => Some((
            palette.resolve(TerminalColor::DefaultForeground),
            palette.resolve(TerminalColor::Ansi(3)),
        )),
        TerminalOverlayKind::ActiveSearch => Some((
            palette.resolve(TerminalColor::DefaultBackground),
            palette.resolve(TerminalColor::Ansi(3)),
        )),
        TerminalOverlayKind::Selection => None,
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
