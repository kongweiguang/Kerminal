//! 只组合共享几何与层级，不接管业务状态、滚动容器或事件。
//!
//! @author kongweiguang

use gpui_kit::{Div, div, prelude::*};

use crate::theme::ThemeTokens;

/// 面板头保留内容自然高度，长标题或多行工具仍能展开；动作槽由调用方保持不收缩。
pub(crate) fn panel_header(theme: ThemeTokens) -> Div {
    div()
        .flex()
        .items_center()
        .min_w_0()
        .flex_shrink_0()
        .px(theme.metrics.panel_padding)
        .py(theme.metrics.gap / 2.)
        .gap(theme.metrics.gap)
        .border_b_1()
        .border_color(theme.border)
        .text_size(theme.text_sm)
        .text_color(theme.foreground)
}

/// 正文只提供一致内边距，最小尺寸允许父级收缩，滚动和草稿仍归原模块维护。
pub(crate) fn panel_content(theme: ThemeTokens) -> Div {
    div()
        .flex()
        .flex_col()
        .min_w_0()
        .min_h_0()
        .p(theme.metrics.panel_padding)
        .gap(theme.metrics.gap)
        .text_size(theme.text_body)
        .text_color(theme.foreground)
}

/// 表单动作按取消到提交排列；窄窗口及文字放大时允许换行，不以裁切隐藏恢复入口。
pub(crate) fn dialog_actions(theme: ThemeTokens) -> Div {
    div()
        .flex()
        .flex_wrap()
        .items_center()
        .justify_end()
        .min_w_0()
        .flex_shrink_0()
        .gap(theme.metrics.gap)
}

#[cfg(test)]
#[path = "../tests/unit/ui_surface.rs"]
mod tests;
