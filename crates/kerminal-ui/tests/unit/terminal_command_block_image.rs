//! 命令块图片导出边界测试；测试实现留在 src 之外。
//!
//! @author kongweiguang

use super::*;

/// SVG 文本必须转义 XML 分隔符，按既定规则清理控制字符，并保持可读文本。
#[test]
fn svg_text_escapes_markup_and_terminal_controls() {
    let escaped = escape_xml_text("<script a=\"&\">\0\u{1b}中文");

    assert_eq!(escaped, "&lt;script a=&quot;&amp;&quot;&gt;� 中文");
}

/// XML 1.0 禁止的非字符不能穿过转义层进入 SVG 文档。
#[test]
fn svg_text_replaces_xml_10_forbidden_noncharacters() {
    let escaped = escape_xml_text("a\u{fffe}\u{ffff}b");

    assert_eq!(escaped, "a��b");
    assert!(escaped.chars().all(is_valid_xml_10_character));
}

/// 图片预览裁切必须落在 UTF-8 边界，并保留可见裁切标记。
///
/// @author kongweiguang
#[test]
fn svg_preview_truncates_at_utf8_boundary_and_marks_omission() {
    let long_text = "中".repeat(MAX_RENDER_BYTES / 3 + 2);
    let (svg, _, _) = build_svg_document(&long_text).expect("SVG document is bounded");

    assert!(svg.contains("内容已裁切"));
    assert!(svg.len() < MAX_RENDER_BYTES * 3);
    assert!(svg.contains("中"));
}

/// 非 ASCII 字符按终端显示列数裁切，宽字符不会使输出图像越过像素预算。
#[test]
fn dimensions_and_line_width_are_bounded_for_wide_text() {
    let line = truncate_line_columns(&"界🙂".repeat(200), MAX_COLUMNS);
    let (width, height) = dimensions_for_lines(&[line]);

    assert!(width <= MAX_WIDTH);
    assert!(height <= MAX_HEIGHT);
    assert!(u64::from(width) * u64::from(height) <= MAX_PIXELS);
}

/// BGRA 到 PNG 的通道转换经真实解码验证，防止复制后红蓝反转。
#[test]
fn bgra_encoder_produces_decodable_png_with_correct_channels() {
    let png = encode_bgra_png(&[0, 0, 255, 255], 1, 1).expect("one pixel encodes");

    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let decoded = image::load_from_memory_with_format(&png, image::ImageFormat::Png)
        .expect("PNG can be decoded")
        .to_rgba8();
    assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
}

/// 尺寸和源 buffer 不一致时拒绝编码，不能生成伪成功的坏图。
#[test]
fn bgra_encoder_rejects_invalid_buffer_and_excessive_dimensions() {
    assert_eq!(
        encode_bgra_png(&[0, 0, 0, 255], 2, 1),
        Err(ImageExportError::InvalidPixelBuffer)
    );
    assert_eq!(
        encode_bgra_png(&[], MAX_WIDTH, MAX_HEIGHT + 1),
        Err(ImageExportError::PixelBudgetExceeded)
    );
}

/// 文档只能从有限文本构造，空内容和超大原文都明确拒绝。
#[test]
fn svg_document_rejects_empty_and_oversized_input() {
    assert_eq!(
        encode_command_block_png("", &gpui_kit::SvgRenderer::new(std::sync::Arc::new(()))),
        Err(ImageExportError::Empty)
    );
    assert_eq!(
        build_svg_document(&"x".repeat(MAX_SOURCE_BYTES + 1)),
        Err(ImageExportError::SourceTooLarge)
    );
}
