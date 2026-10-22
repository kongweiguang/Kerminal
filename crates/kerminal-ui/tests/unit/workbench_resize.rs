//! 局部反馈几何边界回归，不创建窗口或终端进程。
//!
//! @author kongweiguang

use super::*;

/// 指针移出槽或窗口过短时，反馈仍限定在原槽范围，避免负尺寸和光效跳到其它栏。
#[test]
fn glow_stays_on_original_axis_at_edges_and_in_short_panels() {
    let bounds = Bounds::new(point(px(100.), px(20.)), size(px(1.), px(40.)));
    for pointer in [point(px(900.), px(-50.)), point(px(-50.), px(900.))] {
        let glow = glow_bounds(bounds, Axis::Horizontal, pointer);
        assert_eq!(glow.center().x, bounds.center().x);
        assert!(glow.top() >= bounds.top());
        assert!(glow.bottom() <= bounds.bottom());
        assert_eq!(glow.size.height, px(40.));
    }
    let horizontal = Bounds::new(point(px(20.), px(100.)), size(px(40.), px(1.)));
    let glow = glow_bounds(horizontal, Axis::Vertical, point(px(0.), px(0.)));
    assert_eq!(glow.center().y, horizontal.center().y);
    assert_eq!(glow.size.width, px(40.));
}

/// 正常高度的反馈只覆盖指针附近，防止重新退化为整条亮线。
#[test]
fn glow_is_local_to_pointer_in_full_height_panel() {
    let bounds = Bounds::new(point(px(200.), px(0.)), size(px(1.), px(1000.)));
    let glow = glow_bounds(bounds, Axis::Horizontal, point(px(600.), px(450.)));
    assert_eq!(glow.center().y, px(450.));
    assert_eq!(glow.size.height, px(144.));
    assert_eq!(glow.center().x, bounds.center().x);
}
