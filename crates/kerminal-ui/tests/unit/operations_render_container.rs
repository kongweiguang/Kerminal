//! 固定容器右栏概览文本布局的纯函数回归测试。
//!
//! @author kongweiguang

use super::add_container_detail_soft_breaks;

/// 长镜像和路径只增加零宽断点；实际换行交给可用宽度，复制仍可用原值。
///
/// @author kongweiguang
#[test]
fn container_detail_soft_breaks_preserve_long_ascii_values() {
    let original = "registry.example.com/team-with-a-long-name/container-image:release-2026";
    let display = add_container_detail_soft_breaks(original);

    assert_eq!(display.replace('\u{200b}', ""), original);
    assert!(display.contains('\u{200b}'));
    assert!(!display.contains('\n'));
}

/// 长 Unicode 值不依赖英文分隔符，并且在组合字符的 grapheme 边界提供软断点。
///
/// @author kongweiguang
#[test]
fn container_detail_soft_breaks_keep_unicode_graphemes_intact() {
    let original = "e\u{301}".repeat(25);
    let display = add_container_detail_soft_breaks(&original);

    assert_eq!(display.replace('\u{200b}', ""), original);
    assert!(display.contains('\u{200b}'));
    assert!(!display.contains('\n'));
    assert!(
        display
            .split('\u{200b}')
            .all(|chunk| chunk.ends_with('\u{301}'))
    );
}
