//! 将应用语义颜色投影到 GPUI Kit 的控件和基础主题。
//!
//! @author kongweiguang

use gpui_kit::{component::Theme as KitTheme, px};

use super::ThemeTokens;

/// Kit 控件含派生按钮状态均从产品语义色板投影，避免已解析的默认按钮色残留在新主题中。
/// Windows 普通界面延续旧 CSS 的 Segoe UI 优先级，终端字体仍独立控制。
/// 保持 Kit 的 16px rem 基准，使 text_sm/text_xs 解析为 14px/12px，避免 14px 基准将常用控件缩到 12.25px/10.5px；应用正文与终端字号仍由各自令牌和配置控制。
/// 统一关闭外扩聚焦光晕，覆盖普通、密码、多行、数字与组合输入；沿用 Kit 的聚焦细边框，兼顾紧凑布局和键盘焦点辨识。
///
/// @author kongweiguang
pub(super) fn apply_kit_palette(theme: &mut KitTheme, tokens: ThemeTokens) {
    let dark = theme.mode.is_dark();
    let primary = tokens.primary;
    let primary_hover = tokens.primary_hover;
    let primary_active = primary_hover;
    let primary_foreground = tokens.primary_foreground;
    let sidebar_primary_foreground = primary_foreground;
    let colors = &mut theme.colors;
    colors.background = tokens.background;
    colors.foreground = tokens.foreground;
    colors.muted_foreground = tokens.muted;
    colors.border = tokens.border;
    colors.input = tokens.border;
    colors.sidebar = tokens.sidebar;
    colors.sidebar_foreground = tokens.foreground;
    colors.sidebar_border = tokens.border;
    colors.sidebar_accent = tokens.selected;
    colors.sidebar_accent_foreground = tokens.selected_foreground;
    colors.sidebar_primary = primary;
    colors.sidebar_primary_foreground = sidebar_primary_foreground;
    colors.popover = tokens.panel;
    colors.popover_foreground = tokens.foreground;
    colors.accent = tokens.selected;
    colors.accent_foreground = tokens.selected_foreground;
    colors.primary = primary;
    colors.primary_hover = primary_hover;
    colors.primary_active = primary_active;
    colors.primary_foreground = primary_foreground;
    colors.button = tokens.secondary;
    colors.button_hover = tokens.hover;
    colors.button_active = tokens.selected;
    colors.button_foreground = tokens.foreground;
    colors.button_primary = primary;
    colors.button_primary_hover = primary_hover;
    colors.button_primary_active = primary_active;
    colors.button_primary_foreground = colors.primary_foreground;
    colors.button_secondary = tokens.secondary;
    colors.button_secondary_hover = tokens.hover;
    colors.button_secondary_active = tokens.selected;
    colors.button_secondary_foreground = tokens.foreground;
    colors.description_list_label = tokens.secondary;
    colors.description_list_label_foreground = tokens.muted;
    colors.caret = tokens.accent;
    colors.progress_bar = tokens.primary;
    colors.slider_bar = tokens.primary;
    colors.slider_thumb = tokens.primary_foreground;
    colors.secondary = tokens.secondary;
    colors.secondary_hover = tokens.hover;
    colors.secondary_active = tokens.selected;
    colors.secondary_foreground = tokens.foreground;
    colors.list = tokens.panel;
    colors.list_even = tokens.panel;
    colors.list_head = tokens.sidebar;
    colors.list_hover = tokens.hover;
    colors.list_active = tokens.selected;
    colors.list_active_border = tokens.accent;
    colors.tab = tokens.panel;
    colors.tab_bar = tokens.background;
    colors.tab_bar_segmented = tokens.secondary;
    colors.tab_active = tokens.selected;
    colors.tab_active_foreground = tokens.selected_foreground;
    colors.tab_foreground = tokens.muted;
    colors.table = tokens.panel;
    colors.table_even = tokens.panel;
    colors.table_hover = tokens.hover;
    colors.table_active = tokens.selected;
    colors.table_active_border = tokens.accent;
    colors.table_head = tokens.sidebar;
    colors.table_head_foreground = tokens.muted;
    colors.table_foot = tokens.sidebar;
    colors.table_foot_foreground = tokens.muted;
    colors.table_row_border = tokens.border;
    colors.selection = tokens.selected.alpha(if dark { 0.52 } else { 0.7 });
    colors.ring = tokens.accent;
    colors.drag_border = tokens.accent;
    colors.drop_target = tokens.selected;
    colors.group_box = tokens.panel;
    colors.group_box_foreground = tokens.foreground;
    colors.muted = tokens.secondary;
    colors.skeleton = tokens.secondary;
    colors.switch = tokens.secondary;
    // 开关拇指与强调按钮的前景一致，不使用普通正文色。
    colors.switch_thumb = tokens.primary_foreground;
    colors.overlay = tokens.dialog_scrim;
    colors.scrollbar = tokens.background;
    colors.scrollbar_thumb = tokens.subtle;
    colors.scrollbar_thumb_hover = tokens.muted;
    colors.title_bar = tokens.background;
    colors.title_bar_border = tokens.border;
    colors.status_bar = tokens.panel;
    colors.status_bar_border = tokens.border;
    colors.window_border = tokens.border;
    colors.success = tokens.success;
    colors.success_hover = tokens.success.opacity(0.95);
    colors.success_active = tokens.success.opacity(0.9);
    colors.success_foreground = tokens.foreground_on(tokens.success);
    colors.button_success = colors.success;
    colors.button_success_hover = colors.success_hover;
    colors.button_success_active = colors.success_active;
    colors.button_success_foreground = colors.success_foreground;
    colors.warning = tokens.warning;
    colors.warning_hover = tokens.warning.opacity(0.95);
    colors.warning_active = tokens.warning.opacity(0.9);
    colors.warning_foreground = tokens.foreground_on(tokens.warning);
    colors.button_warning = colors.warning;
    colors.button_warning_hover = colors.warning_hover;
    colors.button_warning_active = colors.warning_active;
    colors.button_warning_foreground = colors.warning_foreground;
    colors.danger = tokens.error;
    colors.danger_hover = tokens.error.opacity(0.95);
    colors.danger_active = tokens.error.opacity(0.9);
    colors.danger_foreground = tokens.foreground_on(tokens.error);
    colors.button_danger = colors.danger;
    colors.button_danger_hover = colors.danger_hover;
    colors.button_danger_active = colors.danger_active;
    colors.button_danger_foreground = colors.danger_foreground;
    colors.info = tokens.primary;
    colors.info_hover = tokens.primary_hover;
    colors.info_active = tokens.primary_hover;
    colors.info_foreground = tokens.primary_foreground;
    colors.button_info = colors.info;
    colors.button_info_hover = colors.info_hover;
    colors.button_info_active = colors.info_active;
    colors.button_info_foreground = colors.info_foreground;
    colors.link = tokens.accent;
    colors.link_hover = tokens.accent;
    colors.link_active = tokens.accent;
    theme.radius = px(7.);
    theme.radius_lg = px(8.);
    theme.font_size = px(16.);
    #[cfg(target_os = "windows")]
    if theme.font_family != "Segoe UI" {
        // Windows 延续旧 CSS 的 Latin 字体优先级，并让 DirectWrite 为中文选择匹配字形；只在 Kit 重置为系统默认时覆盖。
        theme.font_family = "Segoe UI".into();
    }
    theme.focus_ring = false;
}
