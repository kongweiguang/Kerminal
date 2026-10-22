//! 编辑器工作区只读概览；激活请求回到 Root，由 Root 校验 editor tab 仍存在。
//!
//! @author kongweiguang

use std::rc::Rc;

use crate::{locale, theme::ThemeTokens};
use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement,
    base::v_virtual_list,
    component::{IconName, tooltip::Tooltip},
    div,
    prelude::*,
    px, size,
};

use super::super::{ActivateEditorFileRequested, FilesPane};
use crate::features::editor::EditorTabProjection;

/// 绘制只读 editor tab 快照，不将 GPUI editor entity 或文件正文跨 feature 暴露。
///
/// @author kongweiguang
pub(super) fn render_workspace_overview(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let Some(projection) = pane.workspace_projection.as_ref() else {
        return workspace_message(
            IconName::LoaderCircle,
            locale::text("正在读取编辑器工作区", "Reading editor workspace"),
            locale::text(
                "正在同步已打开文件与编辑状态。",
                "Syncing open files and editor state.",
            ),
            theme,
        );
    };

    if projection.tabs.is_empty() {
        return workspace_message(
            IconName::FileText,
            locale::text("没有已打开的文件", "No files are open"),
            locale::text(
                "从文件列表打开普通文件后，会在这里显示编辑状态。",
                "Open a regular file from the browser to see its editor state here.",
            ),
            theme,
        );
    }

    let tabs = Rc::new(
        projection
            .tabs
            .iter()
            .cloned()
            .map(|tab| {
                let target_detail = pane.target_names.endpoint_detail(&tab.endpoint);
                (tab, target_detail)
            })
            .collect::<Vec<_>>(),
    );
    let sizes = Rc::new(vec![size(px(1.), px(42.)); tabs.len()]);
    let scroll = pane.workspace_scroll.clone();
    div()
        .id("files-workspace-overview")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.materials.panel)
        .child(
            div()
                .h(px(32.))
                .w_full()
                .px_3()
                .flex()
                .items_center()
                .gap_2()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.materials.chrome)
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .child(IconName::FileText)
                .child(locale::text("编辑器工作区", "Editor workspace"))
                .child(div().flex_1())
                .child(locale::text(
                    format!("{} 个文件", tabs.len()),
                    format!("{} files", tabs.len()),
                )),
        )
        .child(
            div()
                .id("files-workspace-tabs")
                .flex_1()
                .min_h(px(0.))
                .bg(theme.materials.workspace)
                .child(
                    v_virtual_list(
                        cx.entity(),
                        "files-workspace-tabs-list",
                        sizes,
                        move |_, range, _, cx| {
                            range
                                .map(|index| {
                                    render_workspace_tab(
                                        tabs[index].0.clone(),
                                        tabs[index].1.clone(),
                                        index,
                                        theme,
                                        cx,
                                    )
                                    .into_any_element()
                                })
                                .collect::<Vec<_>>()
                        },
                    )
                    .track_scroll(&scroll),
                ),
        )
        .into_any_element()
}

/// 将单个 editor 状态投影成拥有式可访问行，避免虚拟列表返回临时 Context 借用。
///
/// @author kongweiguang
fn render_workspace_tab(
    tab: EditorTabProjection,
    target_detail: String,
    index: usize,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let endpoint = tab.endpoint.clone();
    let active = tab.active;
    let title = tab.title.clone();
    let path = target_detail;
    let mut row = div()
        .id(format!("files-workspace-tab-{index}"))
        .w_full()
        .h(px(42.))
        .px_3()
        .flex()
        .items_center()
        .gap_2()
        .border_b_1()
        .border_color(theme.border)
        .bg(if active {
            theme.selected
        } else {
            theme.background.opacity(0.)
        })
        .hover(|style| style.bg(theme.hover))
        .cursor_pointer()
        .tab_index(0)
        .role(gpui_kit::Role::ListItem)
        .aria_label(format!("{} · {}", title, path))
        .aria_selected(active)
        .tooltip(move |window, cx| Tooltip::new(path.clone()).build(window, cx))
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.emit(ActivateEditorFileRequested {
                endpoint: endpoint.clone(),
            });
        }))
        .child(div().text_color(theme.accent).child(IconName::FileText))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .text_ellipsis()
                .text_size(theme.text_body)
                .text_color(theme.foreground)
                .child(tab.title),
        );

    for (label, visible, color) in [
        (
            locale::text("正在读取", "Loading"),
            tab.loading,
            theme.accent,
        ),
        (locale::text("已修改", "Modified"), tab.dirty, theme.warning),
        (
            locale::text("只读", "Read only"),
            tab.readonly,
            theme.subtle,
        ),
    ] {
        if visible {
            row = row.child(
                div()
                    .flex_shrink_0()
                    .text_size(theme.text_xs)
                    .text_color(color)
                    .child(label),
            );
        }
    }

    row.into_any_element()
}

/// 空快照与加载快照采用相同的主题空态，避免把未收到 Root 响应误显示成空工作区。
///
/// @author kongweiguang
fn workspace_message(
    icon: IconName,
    title: gpui_kit::SharedString,
    detail: gpui_kit::SharedString,
    theme: ThemeTokens,
) -> AnyElement {
    div()
        .id("files-workspace-empty")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.materials.panel)
        .child(div().text_color(theme.accent).child(icon))
        .child(
            div()
                .text_size(theme.text_sm)
                .text_color(theme.foreground)
                .child(title),
        )
        .child(
            div()
                .max_w(px(420.))
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(detail),
        )
        .into_any_element()
}
