//! SFTP 工作区概览展示当前目录、同一目标的编辑状态和最近打开文件。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, InteractiveElement as _, IntoElement,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants as _},
        tooltip::Tooltip,
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::files::{FileEndpoint, FileEntryKind};

use super::super::{ActivateEditorFileRequested, FilesPane, OpenFileRequested};
use crate::{features::editor::EditorTabProjection, locale, theme::ThemeTokens};

/// 工作区始终围绕活动 Files endpoint 投影，编辑器指标只统计身份匹配的已开文件。
///
/// @author kongweiguang
pub(super) fn render_workspace_overview(
    pane: &mut FilesPane,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let side = pane.active_side.min(1);
    let panel = &pane.panels[side];
    let directory = panel.endpoint.clone();
    let generation = panel.generation;
    let current_path = workspace_endpoint_path(&directory);
    let current_path_tooltip = current_path.clone();
    let visible_entries = panel
        .listing
        .as_ref()
        .map(|listing| {
            listing
                .entries
                .iter()
                .filter(|entry| pane.show_hidden_files || !super::browser::is_hidden_entry(entry))
                .filter(|entry| panel.entry_filter.matches(entry.kind))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let directory_count = visible_entries
        .iter()
        .filter(|entry| entry.kind == FileEntryKind::Directory)
        .count();
    let file_count = visible_entries.len().saturating_sub(directory_count);
    let matching_tabs = pane
        .workspace_projection
        .as_ref()
        .map(|projection| {
            projection
                .tabs
                .iter()
                .filter(|tab| workspace_target_matches(&directory, &tab.endpoint))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let dirty_count = matching_tabs.iter().filter(|tab| tab.dirty).count();
    let recent_tabs = matching_tabs
        .iter()
        .rev()
        .take(5)
        .cloned()
        .collect::<Vec<_>>();
    let transfer_count = pane
        .transfer_rows
        .iter()
        .filter(|summary| {
            workspace_target_matches(&directory, &summary.request.source)
                || workspace_target_matches(&directory, &summary.request.destination)
        })
        .count();
    let selected_file = panel
        .selected
        .as_ref()
        .filter(|entry| entry.kind == FileEntryKind::File)
        .map(|entry| entry.endpoint.clone());
    let recent_rows = recent_tabs
        .into_iter()
        .enumerate()
        .map(|(index, tab)| render_workspace_recent_file(tab, index, theme, cx))
        .collect::<Vec<_>>();
    let open_directory = directory.clone();

    div()
        .id("files-workspace-overview")
        .flex_1()
        .min_h(px(0.))
        .flex()
        .flex_col()
        .gap_3()
        .overflow_y_scroll()
        .px_3()
        .py_3()
        .text_size(theme.text_sm)
        .text_color(theme.foreground)
        .child(
            div()
                .pb_3()
                .border_b_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_size(theme.text_xs)
                        .font_weight(gpui_kit::FontWeight::MEDIUM)
                        .text_color(theme.subtle)
                        .child(locale::text("当前根目录", "Current root directory")),
                )
                .child(
                    div()
                        .id("files-workspace-root-path")
                        .mt_1()
                        .w_full()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .font_family("monospace")
                        .text_color(theme.foreground)
                        .tooltip(move |window, cx| {
                            Tooltip::new(current_path_tooltip.clone()).build(window, cx)
                        })
                        .child(current_path),
                ),
        )
        .child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(workspace_metric_row(
                    locale::text("目录", "Folders"),
                    directory_count,
                    false,
                    theme,
                    "files-workspace-folders",
                ))
                .child(workspace_metric_row(
                    locale::text("文件", "Files"),
                    file_count,
                    false,
                    theme,
                    "files-workspace-files",
                ))
                .child(workspace_metric_row(
                    locale::text("已打开", "Open"),
                    matching_tabs.len(),
                    false,
                    theme,
                    "files-workspace-open",
                ))
                .child(workspace_metric_row(
                    locale::text("未保存", "Unsaved"),
                    dirty_count,
                    dirty_count > 0,
                    theme,
                    "files-workspace-dirty",
                ))
                .child(workspace_metric_row(
                    locale::text("传输", "Transfers"),
                    transfer_count,
                    false,
                    theme,
                    "files-workspace-transfers",
                )),
        )
        .when(panel.loading, |overview| {
            overview.child(
                div()
                    .text_size(theme.text_xs)
                    .text_color(theme.subtle)
                    .child(locale::text("正在读取目录…", "Reading directory…")),
            )
        })
        .child(
            div()
                .flex()
                .flex_col()
                .border_b_1()
                .border_color(theme.border)
                .pb_3()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .flex_1()
                                .text_size(theme.text_xs)
                                .font_weight(gpui_kit::FontWeight::MEDIUM)
                                .text_color(theme.subtle)
                                .child(locale::text("最近打开", "Recently opened")),
                        )
                        .child(
                            div()
                                .px_2()
                                .py_1()
                                .rounded_sm()
                                .border_1()
                                .border_color(theme.border)
                                .font_family("monospace")
                                .text_size(theme.text_xs)
                                .text_color(theme.subtle)
                                .child(recent_rows.len().to_string()),
                        ),
                )
                .child(
                    div()
                        .mt_1()
                        .text_size(theme.text_xs)
                        .text_color(theme.subtle)
                        .child(locale::text(
                            "右栏负责导航和文件操作；正文在中间工作区打开。",
                            "The right rail handles navigation and files; content opens in the center workspace.",
                        )),
                )
                .when(recent_rows.is_empty(), |section| {
                    section.child(
                        div()
                            .mt_3()
                            .px_3()
                            .py_4()
                            .border_1()
                            .border_color(theme.border)
                            .text_center()
                            .text_size(theme.text_xs)
                            .text_color(theme.subtle)
                            .child(locale::text(
                                "还没有从当前目标打开文件。",
                                "No files have been opened from this target yet.",
                            )),
                    )
                })
                .children(recent_rows),
        )
        .child(
            Button::new("files-workspace-open-selected")
                .compact()
                .ghost()
                .w_full()
                .label(locale::text(
                    "在中间打开选中文件",
                    "Open selected file in center",
                ))
                .disabled(selected_file.is_none())
                .on_click(cx.listener(move |pane, _: &ClickEvent, _, cx| {
                    if pane.browser_action_snapshot_is_current(side, generation, &open_directory)
                        && let Some(endpoint) = selected_file.as_ref()
                    {
                        cx.emit(OpenFileRequested {
                            endpoint: endpoint.clone(),
                        });
                    }
                })),
        )
        .into_any_element()
}

/// 显示稳定宽度的指标块，脏文件数使用主题 warning 色而不自造独立状态色。
///
/// @author kongweiguang
fn workspace_metric_row(
    label: gpui_kit::SharedString,
    value: usize,
    warning: bool,
    theme: ThemeTokens,
    id: &'static str,
) -> impl IntoElement {
    div()
        .id(id)
        .w_full()
        .min_h(px(48.))
        .px_3()
        .py_2()
        .flex()
        .items_center()
        .gap_2()
        .border_1()
        .border_color(theme.border)
        .bg(theme.materials.panel)
        .child(
            div()
                .flex_1()
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(label),
        )
        .child(
            div()
                .font_family("monospace")
                .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                .text_color(if warning {
                    theme.warning
                } else {
                    theme.foreground
                })
                .child(value.to_string()),
        )
}

/// 最近文件行只发送 Root 可校验的 endpoint 激活事件，不持有编辑器实体或正文。
///
/// @author kongweiguang
fn render_workspace_recent_file(
    tab: EditorTabProjection,
    index: usize,
    theme: ThemeTokens,
    cx: &mut Context<FilesPane>,
) -> AnyElement {
    let endpoint = tab.endpoint.clone();
    let title = tab.title.clone();
    let path = workspace_endpoint_path(&tab.endpoint);
    let path_tooltip = path.clone();
    div()
        .id(format!("files-workspace-recent-{index}"))
        .w_full()
        .min_h(px(44.))
        .mt_1()
        .px_2()
        .py_2()
        .flex()
        .items_center()
        .gap_2()
        .hover(|style| style.bg(theme.hover))
        .cursor_pointer()
        .role(gpui_kit::Role::ListItem)
        .aria_label(format!("{} · {}", title, path))
        .tooltip(move |window, cx| Tooltip::new(path_tooltip.clone()).build(window, cx))
        .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
            cx.emit(ActivateEditorFileRequested {
                endpoint: endpoint.clone(),
            });
        }))
        .child(
            div()
                .w(px(8.))
                .h(px(8.))
                .flex_shrink_0()
                .rounded_full()
                .bg(if tab.dirty {
                    theme.warning
                } else if tab.loading {
                    theme.accent
                } else {
                    theme.success
                }),
        )
        .child(
            div().flex_1().min_w(px(0.)).overflow_hidden().child(
                div()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(theme.text_xs)
                    .font_weight(gpui_kit::FontWeight::MEDIUM)
                    .text_color(theme.foreground)
                    .child(title),
            ),
        )
        .child(
            div()
                .max_w(px(124.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_family("monospace")
                .text_size(theme.text_xs)
                .text_color(theme.subtle)
                .child(path),
        )
        .into_any_element()
}

/// 比较 endpoint 的工作区身份而忽略具体路径，使同一 SSH 主机或容器共享文件摘要。
///
/// @author kongweiguang
fn workspace_target_matches(left: &FileEndpoint, right: &FileEndpoint) -> bool {
    match (left, right) {
        (FileEndpoint::Local { .. }, FileEndpoint::Local { .. }) => true,
        (
            FileEndpoint::Remote { host_id: left, .. },
            FileEndpoint::Remote { host_id: right, .. },
        ) => left == right,
        (
            FileEndpoint::Container {
                host_id: left_host,
                runtime: left_runtime,
                container_id: left_container,
                ..
            },
            FileEndpoint::Container {
                host_id: right_host,
                runtime: right_runtime,
                container_id: right_container,
                ..
            },
        ) => {
            left_host == right_host
                && left_runtime == right_runtime
                && left_container == right_container
        }
        _ => false,
    }
}

/// 统一展示本机和远端路径；Local 使用平台路径显示，远端仍使用 POSIX 路径。
///
/// @author kongweiguang
fn workspace_endpoint_path(endpoint: &FileEndpoint) -> String {
    match endpoint {
        FileEndpoint::Local { path } => path.display().to_string(),
        FileEndpoint::Remote { path, .. } | FileEndpoint::Container { path, .. } => {
            path.as_str().to_owned()
        }
    }
}
