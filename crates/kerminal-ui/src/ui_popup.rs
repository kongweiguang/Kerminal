//! main 右栏菜单的共享表面与交互态样式；条目内容和动作仍归各自功能模块维护。
//!
//! @author kongweiguang

use gpui_kit::{
    Hsla, Pixels, StyleRefinement,
    component::{Side, menu::PopupMenu},
    div,
    prelude::*,
    px,
};

use crate::{theme::ThemeTokens, ui_backdrop};

/// 下拉菜单使用右侧勾选，并让已选项和当前键盘/鼠标项共享 main 的强调态。
///
/// @author kongweiguang
pub(crate) fn dropdown_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    let active = active_item_style(theme);
    menu.check_side(Side::Right)
        .surface_style(surface_style(
            theme,
            theme.floating_surface,
            px(13.),
            px(20.),
        ))
        .items_style(items_style(px(4.)))
        .item_style(dropdown_item_style(theme))
        .item_checked_style(active.clone())
        .item_hover_style(active)
        .separator_style(separator_style(theme))
        .max_h(px(256.))
        .scrollable(true)
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// Agent 选择器保留 main 的宽度和滚动上限；152px 仅属于定位翻转阈值，不应制造空白菜单行。
///
/// @author kongweiguang
pub(crate) fn agent_launcher_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    dropdown_menu(menu, theme)
        .min_w(px(240.))
        .max_w(px(360.))
        .max_h(px(320.))
        .scrollable(true)
        .items_style(agent_items_style())
}

/// 右键菜单维持 main 的中性 hover；只有给定固定宽度的容器操作菜单才收窄到 176px。
///
/// @author kongweiguang
pub(crate) fn context_menu(
    menu: PopupMenu,
    theme: ThemeTokens,
    fixed_width: Option<Pixels>,
) -> PopupMenu {
    let menu = menu
        .check_side(Side::Right)
        .surface_style(surface_style(
            theme,
            theme.context_menu_surface(),
            px(13.),
            px(20.),
        ))
        .items_style(items_style(px(6.)))
        .item_style(context_item_style(theme))
        .item_hover_style(context_hover_style(theme))
        .separator_style(separator_style(theme))
        .render_surface(move |surface, _, _| ui_backdrop::context_menu_surface(surface, theme));
    if let Some(width) = fixed_width {
        menu.min_w(width).max_w(width)
    } else {
        menu
    }
}

/// 文件书签是含标题和嵌套删除按钮的自定义菜单，保持 main 的320px宽度及视口内边距。
///
/// @author kongweiguang
pub(crate) fn file_bookmark_menu(
    menu: PopupMenu,
    theme: ThemeTokens,
    viewport_width: Pixels,
) -> PopupMenu {
    let width = px((viewport_width.as_f32() - 16.).clamp(0., 320.));
    menu.min_w(width)
        .max_w(width)
        .surface_style(surface_style(
            theme,
            theme.floating_surface,
            px(13.),
            px(19.5),
        ))
        .items_style(items_style(px(6.)))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 文件视图菜单按 main 的160px宽度、32px行高和右侧勾选显示当前模式。
///
/// @author kongweiguang
pub(crate) fn file_view_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    menu.check_side(Side::Right)
        .min_w(px(160.))
        .max_w(px(160.))
        .surface_style(surface_style(
            theme,
            theme.floating_surface,
            px(16.),
            px(24.),
        ))
        .items_style(items_style(px(4.)))
        .item_style(file_view_item_style(theme))
        .item_checked_style(file_view_selected_style(theme))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 上传菜单用独立176px菜单规格，避免通用文件选择菜单扩大行高或增加勾选列。
///
/// @author kongweiguang
pub(crate) fn file_upload_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    menu.min_w(px(176.))
        .max_w(px(176.))
        .surface_style(surface_style(
            theme,
            theme.floating_surface,
            px(16.),
            px(24.),
        ))
        .items_style(items_style(px(6.)))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 菜单的玻璃背景、边界和文字层级集中在一个 refinement 中以避免覆盖焦点布局。
///
/// @author kongweiguang
fn surface_style(
    theme: ThemeTokens,
    background: Hsla,
    text_size: Pixels,
    line_height: Pixels,
) -> StyleRefinement {
    div()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(background)
        .text_color(theme.foreground)
        .text_size(text_size)
        .line_height(line_height)
        .shadow_lg()
        .style()
        .clone()
}

/// 统一菜单内边距和行间距，保留容器自有宽度与滚动行为。
///
/// @author kongweiguang
fn items_style(padding: Pixels) -> StyleRefinement {
    div().p(padding).gap_y(px(0.)).style().clone()
}

/// 普通选择菜单匹配 main 的最小行高、可读留白和右侧勾选空间。
///
/// @author kongweiguang
fn dropdown_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h_auto()
        .min_h(px(36.))
        .px(px(10.))
        .py(px(8.))
        .rounded(theme.control_radius)
        .gap_x(px(12.))
        .text_size(px(13.))
        .line_height(px(20.))
        .style()
        .clone()
}

/// Agent launcher 的菜单主体按图标 32px 与上下各 8px 组成 48px 稳定行。
///
/// @author kongweiguang
fn agent_items_style() -> StyleRefinement {
    div().p(px(6.)).gap_y(px(0.)).style().clone()
}

/// 右键命令采用 32px 行高与 8px 图标间距，避免工具菜单挤压长标签。
///
/// @author kongweiguang
fn context_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h_auto()
        .min_h(px(32.))
        .px(px(8.))
        .py_0()
        .rounded(theme.control_radius)
        .gap_x(px(8.))
        .text_size(px(13.))
        .line_height(px(20.))
        .style()
        .clone()
}

/// 视图选项固定32px行高并采用8px水平留白，匹配 main 的紧凑单选菜单。
///
/// @author kongweiguang
fn file_view_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h(px(32.))
        .px(px(8.))
        .py_0()
        .rounded(theme.control_radius)
        .gap_x(px(8.))
        .text_size(px(16.))
        .line_height(px(24.))
        .style()
        .clone()
}

/// 下拉菜单使用选中蓝底，保持选中值和键盘焦点在切换后仍可见。
///
/// @author kongweiguang
fn active_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .bg(theme.surface_selected())
        .text_color(theme.accent)
        .style()
        .clone()
}

/// 文件菜单悬停保持 main 的中性底色，让勾选值单独表达持久选择。
///
/// @author kongweiguang
fn file_hover_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .bg(theme.surface_hover())
        .text_color(theme.foreground)
        .style()
        .clone()
}

/// 视图模式选中态沿用主题选中色和前景色，避免被鼠标 hover 状态覆盖。
///
/// @author kongweiguang
fn file_view_selected_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .bg(theme.surface_selected())
        .text_color(theme.selected_foreground)
        .style()
        .clone()
}

/// 右键操作只使用浅/深主题中性的低透明 hover，不把普通命令表现为主操作。
///
/// @author kongweiguang
fn context_hover_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .bg(theme.context_menu_hover_surface())
        .text_color(theme.foreground)
        .style()
        .clone()
}

/// 分隔线沿用 main 的细边界和 4px 上下间距。
///
/// @author kongweiguang
fn separator_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h(px(1.))
        .mx(px(6.))
        .my(px(4.))
        .border_b(px(1.))
        .border_color(theme.border)
        .style()
        .clone()
}
