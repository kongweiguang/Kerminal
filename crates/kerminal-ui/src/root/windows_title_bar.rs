//! Windows 工作区标题栏；系统控件 hit area 与可交互工作区内容互不覆盖。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, Hsla, IntoElement, StatefulInteractiveElement as _, Window, WindowControlArea,
    component::{Icon, IconName, Sizable as _},
    div,
    prelude::{InteractiveElement as _, ParentElement as _, Styled as _},
    px,
};

use crate::theme::ThemeTokens;

const TITLE_BAR_HEIGHT: f32 = 36.0;
const WINDOW_CONTROL_WIDTH: f32 = 34.0;
const TITLE_BAR_DRAG_WIDTH: f32 = 64.0;

/// 标题栏与 36px Tab 行统一高度；把 64px 保底拖动区和窗口按钮分别交给系统，Tab 内容保持可交互。
///
/// @author kongweiguang
pub(super) fn windows_workbench_title_bar(
    content: AnyElement,
    background: Hsla,
    theme: ThemeTokens,
    window: &Window,
) -> AnyElement {
    div()
        .id("windows-workbench-title-bar")
        .flex()
        .flex_row()
        .items_center()
        .h(px(TITLE_BAR_HEIGHT))
        .w_full()
        .flex_shrink_0()
        .border_b_1()
        .border_color(theme.border)
        .bg(background)
        .child(content)
        .child(
            div()
                .id("windows-workbench-title-bar-drag-space")
                .debug_selector(|| "windows-workbench-title-bar-drag-space".to_owned())
                .w(px(TITLE_BAR_DRAG_WIDTH))
                .h_full()
                .flex_shrink_0()
                .window_control_area(WindowControlArea::Drag),
        )
        .child(windows_window_controls(window, theme))
        .into_any_element()
}

/// 按平台能力只生成可用系统按钮；读屏名称留在控件上，窗口操作继续交给 Windows 原生 hit area。
///
/// @author kongweiguang
fn windows_window_controls(window: &Window, theme: ThemeTokens) -> AnyElement {
    let supported = window.window_controls();
    let mut controls = div()
        .id("windows-workbench-window-controls")
        .flex()
        .flex_row()
        .items_center()
        .flex_shrink_0()
        .h_full();

    if supported.minimize {
        controls = controls.child(windows_window_control(
            "windows-workbench-minimize",
            WindowControlArea::Min,
            IconName::WindowMinimize,
            crate::locale::text("最小化窗口", "Minimize window"),
            false,
            theme,
        ));
    }
    if supported.maximize {
        let (area, icon, label) = if window.is_maximized() {
            (
                WindowControlArea::Max,
                IconName::WindowRestore,
                crate::locale::text("还原窗口", "Restore window"),
            )
        } else {
            (
                WindowControlArea::Max,
                IconName::WindowMaximize,
                crate::locale::text("最大化窗口", "Maximize window"),
            )
        };
        controls = controls.child(windows_window_control(
            "windows-workbench-maximize",
            area,
            icon,
            label,
            false,
            theme,
        ));
    }

    controls
        .child(windows_window_control(
            "windows-workbench-close",
            WindowControlArea::Close,
            IconName::WindowClose,
            crate::locale::text("关闭窗口", "Close window"),
            true,
            theme,
        ))
        .into_any_element()
}

/// 为单个系统按钮保留独立 hit area 并提供无障碍名称，避免与可交互 Tab 内容抢占命中或失去读屏标识。
///
/// @author kongweiguang
fn windows_window_control(
    id: &'static str,
    area: WindowControlArea,
    icon: IconName,
    accessibility_label: gpui_kit::SharedString,
    is_close: bool,
    theme: ThemeTokens,
) -> AnyElement {
    let hover_foreground = if is_close {
        theme.error
    } else {
        theme.foreground
    };

    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .w(px(WINDOW_CONTROL_WIDTH))
        .h(px(TITLE_BAR_HEIGHT))
        .flex()
        .flex_shrink_0()
        .items_center()
        .justify_center()
        .text_color(theme.foreground)
        .role(gpui_kit::Role::Button)
        .aria_label(accessibility_label)
        .window_control_area(area)
        .hover(|style| style.bg(theme.hover).text_color(hover_foreground))
        .active(|style| style.bg(theme.selected).text_color(hover_foreground))
        .child(Icon::new(icon).small())
        .into_any_element()
}
