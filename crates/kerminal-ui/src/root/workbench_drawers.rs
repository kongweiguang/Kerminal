//! 紧凑工作台的抽屉绘制；焦点门禁和内容 Entity 生命周期仍由 Root 负责。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, Entity, FocusHandle, IntoElement,
    assets::IconName,
    component::{
        FocusTrapElement as _,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};

use kerminal_application::preferences::ToolRailToolId;

use crate::{features::connections::HostNavigation, theme::ThemeTokens};

use super::{RootView, workbench::tool_rail_label};

/// 左右留白作为同一布局快照传入，不读取或修改用户保存的停靠宽度。
pub(super) struct ToolDrawerLayout {
    pub left: f32,
    pub right: f32,
    pub width: f32,
}

/// 主机抽屉避开工具 Rail；关闭使用无折叠箭头的面板图标，并核对主机 owner。
/// Escape 仍让搜索和菜单先消费，避免抽屉抢走子控件的键盘交互。
///
/// @author kongweiguang
pub(super) fn render_compact_host_drawer(
    host_navigation: Option<Entity<HostNavigation>>,
    drawer_width: f32,
    right_inset: f32,
    theme: ThemeTokens,
    panel_background: gpui_kit::Hsla,
    focus_handle: &FocusHandle,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let close_label = crate::locale::text("关闭主机列表", "Close host list");
    let navigation = host_navigation.map_or_else(
        || {
            div()
                .flex_1()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "主机导航尚未配置。",
                    "Host navigation is not configured.",
                ))
                .into_any_element()
        },
        |navigation| {
            div()
                .id("workbench-host-drawer-navigation")
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(navigation)
                .into_any_element()
        },
    );

    div()
        .id("workbench-compact-host-drawer-overlay")
        .absolute()
        .top_0()
        .bottom_0()
        .left_0()
        .right(px(right_inset))
        .occlude()
        .flex()
        .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.22))
        .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
            root.dismiss_compact_drawer(None, window, cx);
        }))
        .child(
            div()
                .id("workbench-compact-host-drawer-surface")
                .on_key_down(cx.listener(|root, event, window, cx| {
                    root.handle_compact_drawer_key_down(None, event, window, cx);
                }))
                .w(px(drawer_width))
                .h_full()
                .occlude()
                .flex()
                .flex_col()
                .overflow_hidden()
                .border_1()
                .border_color(theme.border)
                .bg(panel_background)
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("workbench-compact-host-drawer-header")
                        .w_full()
                        .h(px(40.))
                        .flex_shrink_0()
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_color(theme.foreground)
                        .child(crate::locale::text("主机", "Hosts"))
                        .child(
                            Button::new("workbench-close-compact-host-drawer")
                                .compact()
                                .ghost()
                                .icon(IconName::PanelLeft)
                                .accessibility_label(close_label.clone())
                                .tooltip(close_label)
                                .tab_stop(true)
                                .on_click(cx.listener(|root, _: &ClickEvent, window, cx| {
                                    root.dismiss_compact_drawer(None, window, cx);
                                })),
                        ),
                )
                .child(navigation)
                .focus_trap("workbench-compact-host-drawer-focus-trap", focus_handle),
        )
        .into_any_element()
}

/// 抽屉以原 ToolId 约束 X、遮罩和 Escape 的关闭 owner；只接冒泡按键，Agent PTY 和子输入优先消费。
/// 宽度夹紧到可用区域并贴右边缘，不把窄窗工具误画成占满中央区的弹框；用户保存的宽度不改写。
///
/// @author kongweiguang
pub(super) fn render_compact_tool_drawer(
    content: AnyElement,
    tool: ToolRailToolId,
    layout: ToolDrawerLayout,
    theme: ThemeTokens,
    panel_background: gpui_kit::Hsla,
    focus_handle: &FocusHandle,
    cx: &mut Context<RootView>,
) -> AnyElement {
    let close_label = crate::locale::text("关闭工具面板", "Close tool panel");
    div()
        .id("workbench-compact-tool-drawer-overlay")
        .absolute()
        .top_0()
        .bottom_0()
        .left(px(layout.left))
        .right(px(layout.right))
        .occlude()
        .flex()
        .justify_end()
        .bg(gpui_kit::hsla(0.0, 0.0, 0.0, 0.22))
        .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
            root.dismiss_compact_drawer(Some(tool), window, cx);
        }))
        .child(
            div()
                .id("workbench-compact-tool-drawer-surface")
                .on_key_down(cx.listener(move |root, event, window, cx| {
                    root.handle_compact_drawer_key_down(Some(tool), event, window, cx);
                }))
                .w(px(layout.width))
                .h_full()
                .min_w(px(0.))
                .min_h(px(0.))
                .occlude()
                .flex()
                .flex_col()
                .overflow_hidden()
                .border_l_1()
                .border_color(theme.border)
                .bg(panel_background)
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .id("workbench-compact-tool-drawer-header")
                        .w_full()
                        .h(px(40.))
                        .flex_shrink_0()
                        .px_3()
                        .flex()
                        .items_center()
                        .justify_between()
                        .border_b_1()
                        .border_color(theme.border)
                        .text_color(theme.foreground)
                        .child(tool_rail_label(tool))
                        .child(
                            Button::new("workbench-close-compact-tool-drawer")
                                .compact()
                                .ghost()
                                .icon(IconName::X)
                                .accessibility_label(close_label.clone())
                                .tooltip(close_label)
                                .tab_stop(true)
                                .on_click(cx.listener(move |root, _: &ClickEvent, window, cx| {
                                    root.dismiss_compact_drawer(Some(tool), window, cx);
                                })),
                        ),
                )
                .child(content)
                .focus_trap("workbench-compact-tool-drawer-focus-trap", focus_handle),
        )
        .into_any_element()
}

/// 抽屉借用现有 Dock 宽度且不超出真实剩余视口，非有限测量不进入绘制或用户布局偏好。
///
/// @author kongweiguang
pub(super) fn tool_drawer_width(
    viewport_width: f32,
    left_inset: f32,
    right_inset: f32,
    preferred_width: f32,
) -> f32 {
    let available = (viewport_width - left_inset - right_inset).max(0.0);
    let preferred = if preferred_width.is_finite() {
        preferred_width.clamp(260.0, 680.0)
    } else {
        320.0
    };
    if available.is_finite() {
        preferred.min(available)
    } else {
        0.0
    }
}
