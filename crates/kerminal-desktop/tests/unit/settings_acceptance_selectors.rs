//! 验收选择器必须识别 Kit 的真实输入身份且保持容器作用域。
//!
//! @author kongweiguang

use super::{Bounds, ElementId, bounds_inside_viewport, is_scoped_input_path, point, px, size};

/// 普通输入与数值输入的实体编号不同，但都必须在目标容器内被识别。
///
/// @author kongweiguang
#[test]
fn scoped_input_accepts_named_and_entity_identifiers() {
    let parent = ElementId::from("terminal-font-size");
    assert!(is_scoped_input_path(
        &[parent.clone(), ElementId::from("input")],
        &parent
    ));
    assert!(is_scoped_input_path(
        &[parent.clone(), ElementId::from(("input", 42_u64))],
        &parent
    ));
}

/// 同页另一字段或步进按钮不能误作当前字段的可编辑输入。
///
/// @author kongweiguang
#[test]
fn scoped_input_rejects_other_fields_and_step_buttons() {
    let parent = ElementId::from("terminal-font-size");
    assert!(!is_scoped_input_path(
        &[
            ElementId::from("terminal-line-height"),
            ElementId::from(("input", 42_u64))
        ],
        &parent
    ));
    assert!(!is_scoped_input_path(
        &[parent.clone(), ElementId::from("increment")],
        &parent
    ));
}

/// 真实 150% 缩放坐标曾产生小于千分之一像素的边界差，不能让定位滚轮持续空转。
///
/// @author kongweiguang
#[test]
fn viewport_accepts_fractional_scale_rounding() {
    let viewport = Bounds::new(
        point(px(243.333_33), px(455.333_34)),
        size(px(519.333_3), px(103.999_97)),
    );
    let field = Bounds::new(
        point(px(255.333_33), px(527.333_4)),
        size(px(495.333_34), px(32.)),
    );
    assert!(bounds_inside_viewport(field, viewport));
}

/// 舍入容差不能放宽完整可见要求，四边真实裁切、过高控件和空区域都必须拒绝。
///
/// @author kongweiguang
#[test]
fn viewport_rejects_real_clipping_and_empty_regions() {
    let viewport = Bounds::new(point(px(0.), px(0.)), size(px(100.), px(64.)));
    for origin in [
        point(px(-0.01), px(0.)),
        point(px(0.01), px(0.)),
        point(px(0.), px(-0.01)),
        point(px(0.), px(0.01)),
    ] {
        assert!(!bounds_inside_viewport(
            Bounds::new(origin, viewport.size),
            viewport
        ));
    }
    let short = Bounds::new(viewport.origin, size(px(100.), px(39.5)));
    assert!(!bounds_inside_viewport(viewport, short));
    let empty = Bounds::new(viewport.origin, size(px(100.), px(0.)));
    assert!(!bounds_inside_viewport(viewport, empty));
    assert!(!bounds_inside_viewport(empty, viewport));
}
