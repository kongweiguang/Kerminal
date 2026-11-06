//! main 右栏菜单的共享表面与交互态样式；条目内容和动作仍归各自功能模块维护。
//!
//! @author kongweiguang

use gpui_kit::{
    Hsla, Pixels, StyleRefinement,
    base::StyledExt as _,
    component::{Side, menu::PopupMenu},
    div,
    prelude::*,
    px,
};

use crate::{theme::ThemeTokens, ui_backdrop};

#[cfg(test)]
#[path = "../tests/unit/context_menu_dispatch.rs"]
mod context_menu_dispatch;

/// 下拉菜单通过勾选和选中底色表达持久选择，悬停保持中性，键盘行为仍由 Kit 管理。
///
/// @author kongweiguang
pub(crate) fn dropdown_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    let active = active_item_style(theme);
    menu.check_side(Side::Right)
        .surface_style(surface_style(theme, theme.floating_surface))
        .items_style(items_style(theme.metrics.gap / 2.))
        .item_style(dropdown_item_style(theme))
        .item_checked_style(active.clone())
        .item_hover_style(file_hover_style(theme))
        .separator_style(separator_style(theme))
        .max_h(px(256.))
        .scrollable(true)
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// Agent 选择器保留 main 的宽度和滚动上限；条目左边距与触发器图标对齐，不重复计入内容自绘图标。
///
/// @author kongweiguang
pub(crate) fn agent_launcher_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    dropdown_menu(menu, theme)
        .min_w(px(240.))
        .max_w(px(360.))
        .max_h(px(320.))
        .scrollable(true)
        .items_style(agent_items_style(theme))
        .item_style(
            div()
                .refine_style(&dropdown_item_style(theme))
                .px(px(5.))
                .style()
                .clone(),
        )
}

/// 右键命令复用共享字号、行高和中性悬停，保留调用方的对象范围与宽度约束。
///
/// @author kongweiguang
pub(crate) fn context_menu(
    menu: PopupMenu,
    theme: ThemeTokens,
    fixed_width: Option<Pixels>,
) -> PopupMenu {
    let menu = menu
        .check_side(Side::Right)
        .surface_style(surface_style(theme, theme.context_menu_surface()))
        .items_style(items_style(theme.metrics.gap / 2.))
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

/// 书签保留标题和嵌套删除动作，文字放大时宽度同步增长，但不越过当前视口。
///
/// @author kongweiguang
pub(crate) fn file_bookmark_menu(
    menu: PopupMenu,
    theme: ThemeTokens,
    viewport_width: Pixels,
) -> PopupMenu {
    let scale = theme.text_body.as_f32() / 14.;
    let width = px((viewport_width.as_f32() - 16.).clamp(0., 320. * scale));
    menu.min_w(width)
        .max_w(width)
        .surface_style(surface_style(theme, theme.floating_surface))
        .items_style(items_style(theme.metrics.gap / 2.))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 文件视图使用普通菜单几何和右侧勾选，避免同一区域的选项突然放大字号。
///
/// @author kongweiguang
pub(crate) fn file_view_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    let width = px(160. * theme.text_body.as_f32() / 14.);
    menu.check_side(Side::Right)
        .min_w(width)
        .max_w(width)
        .surface_style(surface_style(theme, theme.floating_surface))
        .items_style(items_style(theme.metrics.gap / 2.))
        .item_style(file_view_item_style(theme))
        .item_checked_style(file_view_selected_style(theme))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 上传动作共享普通菜单的文字与命中区，不增加无意义的持久勾选列。
///
/// @author kongweiguang
pub(crate) fn file_upload_menu(menu: PopupMenu, theme: ThemeTokens) -> PopupMenu {
    let width = px(176. * theme.text_body.as_f32() / 14.);
    menu.min_w(width)
        .max_w(width)
        .surface_style(surface_style(theme, theme.floating_surface))
        .items_style(items_style(theme.metrics.gap / 2.))
        .item_style(context_item_style(theme))
        .item_hover_style(file_hover_style(theme))
        .render_surface(move |surface, _, _| {
            ui_backdrop::floating_surface(surface, theme, theme.radius)
        })
}

/// 菜单的玻璃背景、边界和文字层级集中在一个 refinement 中以避免覆盖焦点布局。
///
/// @author kongweiguang
fn surface_style(theme: ThemeTokens, background: Hsla) -> StyleRefinement {
    div()
        .overflow_hidden()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .bg(background)
        .text_color(theme.foreground)
        .text_size(theme.text_body)
        .line_height(theme.text_body * 1.5)
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

/// 选择和命令菜单共享密度行高，允许多行内容自然增长，不挤压右侧勾选空间。
///
/// @author kongweiguang
fn dropdown_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h_auto()
        .min_h(theme.metrics.row_height)
        .px(theme.metrics.gap)
        .py(theme.metrics.gap / 2.)
        .rounded(theme.control_radius)
        .gap_x(theme.metrics.gap)
        .text_size(theme.text_body)
        .line_height(theme.text_body * 1.5)
        .style()
        .clone()
}

/// Agent 菜单仅保留图标复合内容需要的空间，外围留白跟随其它菜单的密度。
///
/// @author kongweiguang
fn agent_items_style(theme: ThemeTokens) -> StyleRefinement {
    items_style(theme.metrics.gap / 2.)
}

/// 命令项采用共享密度和自适应行高，文字放大后保留完整命中区。
///
/// @author kongweiguang
fn context_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .h_auto()
        .min_h(theme.metrics.row_height)
        .px(theme.metrics.gap)
        .py(theme.metrics.gap / 2.)
        .rounded(theme.control_radius)
        .gap_x(theme.metrics.gap)
        .text_size(theme.text_body)
        .line_height(theme.text_body * 1.5)
        .style()
        .clone()
}

/// 文件视图项复用普通命令行几何，避免固定行高裁掉放大的文字。
///
/// @author kongweiguang
fn file_view_item_style(theme: ThemeTokens) -> StyleRefinement {
    context_item_style(theme)
}

/// 选中项复用当前主题语义，不以品牌色文字替代菜单的勾选与键盘焦点。
///
/// @author kongweiguang
fn active_item_style(theme: ThemeTokens) -> StyleRefinement {
    div()
        .bg(theme.surface_selected())
        .text_color(theme.selected_foreground)
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
