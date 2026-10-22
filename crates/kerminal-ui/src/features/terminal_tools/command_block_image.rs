//! 将选中的命令块文本渲染成有界 PNG，供系统剪贴板使用。
//!
//! @author kongweiguang

use gpui_kit::{DevicePixels, Size, SvgRenderer, SvgSize};
use image::{ExtendedColorType, ImageEncoder, codecs::png::PngEncoder};

const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_RENDER_BYTES: usize = 12 * 1024;
const MAX_LINES: usize = 100;
const MAX_COLUMNS: usize = 192;
const MAX_WIDTH: u32 = 1600;
const MAX_HEIGHT: u32 = 2500;
const MAX_PIXELS: u64 = 4_000_000;
const MAX_PNG_BYTES: usize = 16 * 1024 * 1024;

/// 图像生成失败的稳定原因；不给 UI 泄漏渲染器内部诊断或输入片段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ImageExportError {
    Empty,
    SourceTooLarge,
    RenderFailed,
    PixelBudgetExceeded,
    InvalidPixelBuffer,
    PngEncodingFailed,
    EncodedImageTooLarge,
}

/// 使用 GPUI 的 SVG 字体解析和成熟 PNG 编码器处理所选文本，不把 SVG 内容直接写入剪贴板。
pub(crate) fn encode_command_block_png(
    text: &str,
    renderer: &SvgRenderer,
) -> Result<Vec<u8>, ImageExportError> {
    if text.is_empty() {
        return Err(ImageExportError::Empty);
    }
    if text.len() > MAX_SOURCE_BYTES {
        return Err(ImageExportError::SourceTooLarge);
    }

    let (document, width, height) = build_svg_document(text)?;
    let parsed = renderer
        .parse_svg(document.as_bytes())
        .map_err(|_| ImageExportError::RenderFailed)?;
    let render = renderer
        .render_parsed(
            &parsed,
            SvgSize::ExactSize(Size::new(
                DevicePixels(width as i32),
                DevicePixels(height as i32),
            )),
        )
        .map_err(|_| ImageExportError::RenderFailed)?;
    let size = render.size(0);
    let width = u32::try_from(size.width.0).map_err(|_| ImageExportError::InvalidPixelBuffer)?;
    let height = u32::try_from(size.height.0).map_err(|_| ImageExportError::InvalidPixelBuffer)?;
    let bgra = render
        .as_bytes(0)
        .ok_or(ImageExportError::InvalidPixelBuffer)?;
    let png = encode_bgra_png(bgra, width, height)?;
    if png.len() > MAX_PNG_BYTES {
        return Err(ImageExportError::EncodedImageTooLarge);
    }
    Ok(png)
}

/// 校验 GPUI BGRA 栅格长度和面积后按像素块交换通道，避免错尺寸或非整像素被静默接受。
///
/// @author kongweiguang
fn encode_bgra_png(bgra: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ImageExportError> {
    if width == 0 || height == 0 || u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(ImageExportError::PixelBudgetExceeded);
    }
    let expected_bytes = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or(ImageExportError::InvalidPixelBuffer)?;
    if bgra.len() != expected_bytes {
        return Err(ImageExportError::InvalidPixelBuffer);
    }

    let mut rgba = bgra.to_vec();
    let (pixels, remainder) = rgba.as_chunks_mut::<4>();
    if !remainder.is_empty() {
        return Err(ImageExportError::InvalidPixelBuffer);
    }
    for pixel in pixels {
        pixel.swap(0, 2);
    }
    let mut png = Vec::new();
    PngEncoder::new(&mut png)
        .write_image(&rgba, width, height, ExtendedColorType::Rgba8)
        .map_err(|_| ImageExportError::PngEncodingFailed)?;
    Ok(png)
}

/// 生成有固定像素上限的 SVG；文本只进入 XML text node，并逐字符转义。
fn build_svg_document(text: &str) -> Result<(String, u32, u32), ImageExportError> {
    if text.len() > MAX_SOURCE_BYTES {
        return Err(ImageExportError::SourceTooLarge);
    }
    let source_truncated = text.len() > MAX_RENDER_BYTES;
    let line_truncated = text.lines().count() > MAX_LINES;
    let has_truncation = source_truncated || line_truncated;
    let display = truncate_at_boundary(text, MAX_RENDER_BYTES);
    let mut lines = display
        .lines()
        .take(if has_truncation {
            MAX_LINES - 1
        } else {
            MAX_LINES
        })
        .map(|line| truncate_line_columns(line, MAX_COLUMNS))
        .collect::<Vec<_>>();
    if lines.is_empty() {
        lines.push(String::new());
    }
    if has_truncation {
        lines.push("… 内容已裁切 …".to_owned());
    }
    let (width, height) = dimensions_for_lines(&lines);
    let mut document = String::with_capacity(display.len().saturating_mul(2).saturating_add(512));
    document.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" viewBox=\"0 0 {width} {height}\"><rect width=\"100%\" height=\"100%\" rx=\"12\" fill=\"#111827\"/><g font-family=\"monospace\" font-size=\"14\" xml:space=\"preserve\">"
    ));
    for (index, line) in lines.iter().enumerate() {
        let escaped = escape_xml_text(line);
        let y = 24 + index * 22;
        let color = if index == 0 { "#93c5fd" } else { "#e5e7eb" };
        document.push_str(&format!(
            "<text x=\"16\" y=\"{y}\" fill=\"{color}\">{escaped}</text>"
        ));
    }
    document.push_str("</g></svg>");
    Ok((document, width, height))
}

/// 按可见终端字符宽度限行，避免超宽命令造成无限画布和超预算栅格。
fn truncate_line_columns(line: &str, max_columns: usize) -> String {
    use unicode_width::UnicodeWidthChar as _;

    let mut result = String::new();
    let mut columns = 0usize;
    for character in line.chars() {
        let width = character.width().unwrap_or(0);
        if columns.saturating_add(width) > max_columns {
            result.push('…');
            break;
        }
        result.push(character);
        columns += width;
    }
    result
}

/// 用 Unicode cell 宽度确定宽度，并限制最大栅格面积后返回尺寸。
fn dimensions_for_lines(lines: &[String]) -> (u32, u32) {
    use unicode_width::UnicodeWidthStr as _;

    let columns = lines
        .iter()
        .map(|line| line.width())
        .max()
        .unwrap_or(1)
        .clamp(1, MAX_COLUMNS);
    let width = (columns as u32 * 9 + 32).clamp(320, MAX_WIDTH);
    let height = (lines.len().max(1) as u32 * 22 + 32).clamp(64, MAX_HEIGHT);
    let capped_height = if u64::from(width) * u64::from(height) > MAX_PIXELS {
        (MAX_PIXELS / u64::from(width)).max(1) as u32
    } else {
        height
    };
    (width, capped_height.min(MAX_HEIGHT))
}

/// 在 Unicode 字符边界裁剪超长内容，不可能把 UTF-8 码点切成非法片段。
fn truncate_at_boundary(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// XML text node 使用最小转义并替换 XML 1.0 禁止字符，避免输出注入节点或生成无效文档。
/// @author kongweiguang
fn escape_xml_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\'' => escaped.push_str("&apos;"),
            '"' => escaped.push_str("&quot;"),
            '\0' => escaped.push('\u{fffd}'),
            character if character.is_control() => escaped.push(' '),
            character if !is_valid_xml_10_character(character) => escaped.push('\u{fffd}'),
            character => escaped.push(character),
        }
    }
    escaped
}

/// 按 XML 1.0 Char 产生式判断标量是否合法；Rust 字符已排除代理项，但仍可能包含 FFFE/FFFF。
/// @author kongweiguang
fn is_valid_xml_10_character(character: char) -> bool {
    let scalar = character as u32;
    matches!(scalar, 0x9 | 0xA | 0xD)
        || (0x20..=0xD7FF).contains(&scalar)
        || (0xE000..=0xFFFD).contains(&scalar)
        || (0x10000..=0x10FFFF).contains(&scalar)
}

#[cfg(test)]
#[path = "../../../tests/unit/terminal_command_block_image.rs"]
mod tests;
