//! 主机弹层在缩放和小窗口下的尺寸边界。
//!
//! @author kongweiguang

use super::*;

/// 正常窗口保持设计尺寸，窗口缩小时保留外边距且不会产生负正文尺寸。
/// @author kongweiguang
#[test]
fn modal_is_clamped_to_viewport_at_multiple_scales() {
    for scale in [1.0, 1.25, 1.5, 2.0] {
        let base = px(16.0 * scale);
        for viewport in [
            size(px(1920.0), px(1080.0)),
            size(px(640.0), px(480.0)),
            size(px(16.0), px(16.0)),
        ] {
            let actual = host_editor_dimensions(viewport, base);
            assert!(actual.width >= px(0.0) && actual.height >= px(0.0));
            assert!(actual.width <= (viewport.width - base * 2.0).max(px(0.0)));
            assert!(actual.height <= (viewport.height - base * 2.0).max(px(0.0)));
            assert!(actual.width <= base * EDITOR_WIDTH_REM);
            assert!(actual.height <= base * EDITOR_HEIGHT_REM);
        }
    }
}
