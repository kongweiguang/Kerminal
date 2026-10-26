//! 通用工具动作的轻量呈现，业务状态与执行仍由原视图持有。
//!
//! @author kongweiguang

use gpui_kit::{
    ElementId, SharedString,
    component::{
        Icon, Sizable as _,
        button::{Button, ButtonVariants as _},
    },
    prelude::*,
};

/// 仅统一熟悉动作的尺寸与可访问名称；返回原 Button，避免复制事件、忙碌或焦点状态。
/// 提示由调用方按对象范围附加，立即卸载的触发器仍可沿用元素生命周期提示。
pub(crate) fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: impl Into<SharedString>,
) -> Button {
    let icon: Icon = icon.into();
    Button::new(id)
        .small()
        .ghost()
        .icon(icon)
        .flex_shrink_0()
        .accessibility_label(label)
        .tab_stop(true)
}
