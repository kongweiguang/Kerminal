//! 标题栏导航共用的细分隔线，避免相同层级使用不同高度。
//!
//! @author kongweiguang

use gpui_kit::{AnyElement, Hsla, IntoElement, Styled, div, px, rems};

/// 分隔线只占固定细槽；高度随界面字号缩放，宽度保留物理细线，状态切换不改变相邻命中区。
pub(crate) fn navigation_separator(color: Hsla) -> AnyElement {
    div()
        .w(px(1.))
        .h(rems(1.125))
        .flex_none()
        .bg(color)
        .into_any_element()
}
