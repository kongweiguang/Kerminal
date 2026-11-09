//! 截图门禁的纯像素回归；不把构造数据当作原生画面验收。
//!
//! @author kongweiguang

use super::*;

/// 填充真实夹具的两块网页颜色，可选择加入宿主标记或遮罩。
fn frame(scale: u16, overlay: bool) -> Vec<u8> {
    let mut pixels = Vec::new();
    for (color, count) in [([0x56u8, 0x34, 0x12], 2100), ([0x78u8, 0xcf, 0x25], 600)] {
        for _ in 0..count {
            pixels.extend(color.map(|value| ((u16::from(value) * scale + 50) / 100) as u8));
            pixels.push(255);
        }
    }
    if overlay {
        for _ in 0..120 {
            pixels.extend_from_slice(&[0x91, 0x3a, 0xe6, 255]);
        }
    }
    pixels
}

/// 同一帧必须包含网页细节和当前阶段需要的宿主浮层。
#[test]
fn requires_both_page_details_and_stage_specific_overlay() {
    assert!(count_fixture_pixels(&frame(100, false), false).accepted(false));
    assert!(!count_fixture_pixels(&frame(100, false), false).accepted(true));
    assert!(count_fixture_pixels(&frame(100, true), false).accepted(true));
    assert!(!count_fixture_pixels(&frame(100, true), false).accepted(false));
}

/// 对话框只接受已知遮罩，不允许黑屏和不相关衰减色伪装为页面。
#[test]
fn permits_only_known_modal_dimming_and_rejects_blank() {
    assert!(count_fixture_pixels(&frame(80, true), true).accepted(true));
    assert!(count_fixture_pixels(&frame(95, true), true).accepted(true));
    assert!(!count_fixture_pixels(&frame(35, true), true).accepted(true));
    assert!(!count_fixture_pixels(&vec![0; 20_000], true).accepted(true));
}
