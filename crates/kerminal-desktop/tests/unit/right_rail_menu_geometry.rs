//! 分数 DPI 菜单边界的实际回执回归。
//!
//! @author kongweiguang

use super::*;
use gpui_pre::{point, px, size};

/// 真实 150% DPI 回执包含三分之一逻辑像素，不能因 intersect 舍入失败而永远等待。
#[test]
fn fractional_popup_inside_viewport_is_accepted_without_allowing_visible_overflow() {
    let viewport = Bounds {
        origin: point(px(0.), px(0.)),
        size: size(px(1020.), px(720.)),
    };
    let popup = Bounds {
        origin: point(px(694.), px(357.33334)),
        size: size(px(240.), px(208.)),
    };
    assert!(bounds_inside_viewport(popup, viewport));
    let outside = Bounds {
        origin: point(px(780.5), px(357.33334)),
        ..popup
    };
    assert!(!bounds_inside_viewport(outside, viewport));
}
