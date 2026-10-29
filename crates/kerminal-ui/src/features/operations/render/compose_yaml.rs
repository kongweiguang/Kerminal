//! Compose YAML 页内只读预览的工具栏、路径选择和编辑器布局。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, SharedString,
    assets::IconName,
    component::{
        Disableable, Icon,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::operations::ComposeProjectTarget;

use crate::{theme::ThemeTokens, ui_button::icon_button};

use super::super::OperationsView;
use super::super::compose_yaml::compose_yaml_metadata_items;
use super::super::container_panel::ContainerPanelActionTicket;

/// 在当前 Compose 项目页内呈现只读 YAML；中间工作区按钮仍使用既有独立文件 Tab 流程。
///
/// @author kongweiguang
pub(super) fn render(
    view: &mut OperationsView,
    project: &ComposeProjectTarget,
    ticket: &ContainerPanelActionTicket,
    theme: ThemeTokens,
    cx: &mut Context<OperationsView>,
) -> AnyElement {
    view.compose_yaml.select_project(project);
    if project.config_files.is_empty() {
        return yaml_state_message(
            crate::locale::text(
                "未发现 Compose YAML 路径。",
                "No Compose YAML path was found.",
            ),
            theme,
        )
        .into_any_element();
    }

    let selected_path = view.compose_yaml.selected_path().map(str::to_owned);
    let loading = view.compose_yaml.is_loading();
    let preview = view.compose_yaml.preview().cloned();
    let error = view.compose_yaml.error().map(str::to_owned);
    let editor = view.compose_yaml.editor().cloned();
    let mut path_picker = div()
        .id("compose-yaml-path-picker")
        .w_full()
        .min_w_0()
        .flex()
        .gap_1()
        .overflow_x_scroll();
    if project.config_files.len() > 1 {
        for path in &project.config_files {
            let path_for_action = path.clone();
            let project_for_action = project.clone();
            let ticket_for_action = ticket.clone();
            let button = Button::new(format!("compose-yaml-path:{path}"))
                .compact()
                .label(path.clone())
                .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
                    if view.compact_layout {
                        view.request_compose_yaml_workspace(
                            &project_for_action,
                            &path_for_action,
                            &ticket_for_action,
                            cx,
                        );
                    } else {
                        view.request_compose_yaml_preview(
                            &project_for_action,
                            &path_for_action,
                            &ticket_for_action,
                            cx,
                        );
                    }
                }));
            path_picker = path_picker.child(if selected_path.as_deref() == Some(path.as_str()) {
                button.primary()
            } else {
                button.ghost()
            });
        }
    }

    let refresh_project = project.clone();
    let refresh_ticket = ticket.clone();
    let refresh = icon_button(
        "compose-yaml-refresh",
        IconName::RefreshCw,
        crate::locale::text("重新读取 Compose YAML", "Reload Compose YAML"),
    )
    .disabled(loading || selected_path.is_none())
    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
        if let Some(path) = view.compose_yaml.selected_path().map(str::to_owned) {
            view.request_compose_yaml_preview(&refresh_project, &path, &refresh_ticket, cx);
        }
    }));

    let copy_project = project.clone();
    let copy_ticket = ticket.clone();
    let copy_path = icon_button(
        "compose-yaml-copy-path",
        IconName::Copy,
        crate::locale::text("复制 Compose YAML 路径", "Copy Compose YAML path"),
    )
    .disabled(selected_path.is_none())
    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
        if let Some(path) = view.compose_yaml.selected_path().map(str::to_owned)
            && view.container_action_ticket_is_current(&copy_ticket, None)
            && matches!(
                view.container_host.page(),
                super::container_panel::ContainerHostPage::ComposeProject { target }
                    if target == &copy_project
            )
        {
            cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(path));
        }
    }));

    let open_project = project.clone();
    let open_ticket = ticket.clone();
    let open_workspace = icon_button(
        "compose-yaml-open-workspace",
        IconName::FileCode,
        crate::locale::text(
            "在中间工作区打开 Compose YAML",
            "Open Compose YAML in workspace",
        ),
    )
    .disabled(selected_path.is_none())
    .on_click(cx.listener(move |view, _: &ClickEvent, _, cx| {
        let Some(path) = view.compose_yaml.selected_path().map(str::to_owned) else {
            return;
        };
        if !view.container_action_ticket_is_current(&open_ticket, None) {
            return;
        }
        view.request_compose_config(
            open_project.host_id.clone(),
            path,
            Some(open_project.working_dir.clone()),
            &open_ticket,
            cx,
        );
    }));

    let toolbar = div()
        .id("compose-yaml-toolbar")
        .w_full()
        .min_w_0()
        .h(px(36.))
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .px_2()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.sidebar)
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Icon::new(IconName::FileCode)
                        .size(px(16.))
                        .text_color(theme.muted),
                )
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(theme.text_xs)
                        .text_color(theme.foreground)
                        .child(selected_path.clone().unwrap_or_else(|| {
                            crate::locale::text("未选择 Compose YAML", "No Compose YAML selected")
                                .to_string()
                        })),
                ),
        )
        .child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_1()
                .child(refresh)
                .child(copy_path)
                .child(open_workspace),
        );

    let editor_body = match (loading, error, editor) {
        (true, _, _) => yaml_state_message(
            crate::locale::text("正在读取 Compose YAML…", "Reading Compose YAML…"),
            theme,
        ),
        (false, Some(error), _) => yaml_error_message(&error, theme),
        (false, None, Some(editor)) if preview.as_ref().is_some_and(|preview| !preview.binary) => {
            div()
                .id("compose-yaml-editor-frame")
                .size_full()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .child(
                    gpui_kit::component::input::Editor::new(&editor)
                        .readonly(true)
                        .appearance(false)
                        .bordered(false)
                        .aria_label(crate::locale::text(
                            "Compose YAML 只读预览",
                            "Compose YAML read-only preview",
                        )),
                )
                .into_any_element()
        }
        (false, None, _) => yaml_state_message(
            selected_path.as_ref().map_or_else(
                || {
                    crate::locale::text(
                        "选择路径后预览宿主机文件。",
                        "Select a path to preview the host file.",
                    )
                },
                |path| SharedString::from(format!("# 选择路径后预览宿主机文件\n# {path}")),
            ),
            theme,
        ),
    };

    let metadata = preview
        .as_ref()
        .map(compose_yaml_metadata_items)
        .unwrap_or_default();
    let footer = yaml_footer(
        preview.as_ref().is_some_and(|preview| preview.truncated),
        &metadata,
        theme,
    );

    div()
        .id("compose-yaml-preview")
        .w_full()
        .flex_1()
        .min_w_0()
        .min_h_0()
        .flex()
        .flex_col()
        .gap_2()
        .child(toolbar)
        .when(project.config_files.len() > 1, |body| {
            body.child(path_picker)
        })
        .child(
            div()
                .id("compose-yaml-content")
                .w_full()
                .flex_1()
                .min_w_0()
                .min_h_0()
                .overflow_hidden()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.materials.chrome)
                .child(editor_body),
        )
        .child(footer)
        .into_any_element()
}

/// 用窄栏友好的居中状态承载首次读取和无内容提示，不增加新的页面操作。
///
/// @author kongweiguang
fn yaml_state_message(message: impl Into<SharedString>, theme: ThemeTokens) -> AnyElement {
    div()
        .id("compose-yaml-state-message")
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_3()
        .text_size(theme.text_xs)
        .text_color(theme.muted)
        .child(message.into())
        .into_any_element()
}

/// 错误和二进制状态说明失败原因，重试入口仍固定在 YAML 工具栏。
///
/// @author kongweiguang
fn yaml_error_message(message: &str, theme: ThemeTokens) -> AnyElement {
    div()
        .id("compose-yaml-error-message")
        .w_full()
        .min_w_0()
        .flex()
        .flex_col()
        .items_start()
        .justify_center()
        .gap_2()
        .p_4()
        .text_size(theme.text_xs)
        .text_color(theme.error)
        .child(crate::locale::text(
            "无法预览 Compose YAML",
            "Unable to preview Compose YAML",
        ))
        .child(div().min_w_0().child(message.to_owned()))
        .child(
            div()
                .min_w_0()
                .text_color(theme.muted)
                .child(crate::locale::text(
                    "请确认主机连接和文件路径有效，然后重试。",
                    "Check the host connection and file path, then retry.",
                )),
        )
        .into_any_element()
}

/// 元数据显示为轻量文本而不是交互控件，并在截断时保留主线的明确提示。
///
/// @author kongweiguang
fn yaml_footer(truncated: bool, metadata: &[String], theme: ThemeTokens) -> AnyElement {
    let mut footer = div()
        .id("compose-yaml-footer")
        .w_full()
        .min_w_0()
        .flex()
        .items_center()
        .justify_between()
        .gap_2()
        .h(px(20.));
    if truncated {
        footer = footer.child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_size(px(11.))
                .text_color(theme.warning)
                .child(crate::locale::text(
                    "文件超过预览上限，内容已截断。",
                    "The file exceeded the preview limit and was truncated.",
                )),
        );
    } else {
        footer = footer.child(div().min_w_0().flex_1());
    }
    let mut metadata_row = div()
        .id("compose-yaml-metadata")
        .flex_shrink_0()
        .flex()
        .items_center()
        .gap_1()
        .overflow_x_scroll();
    for (index, item) in metadata.iter().enumerate() {
        metadata_row = metadata_row.child(
            div()
                .id(format!("compose-yaml-metadata-{index}-{item}"))
                .flex_shrink_0()
                .px_1()
                .text_size(px(10.))
                .text_color(theme.muted)
                .child(item.clone()),
        );
    }
    footer.child(metadata_row).into_any_element()
}

/// 右栏路径选择打开中央只读 Tab，完整工作台仍原位预览；两条路径复用同一冻结项目与票据。
///
/// @author kongweiguang
pub(super) fn request_preview_for_selected_path(
    view: &mut OperationsView,
    project: &ComposeProjectTarget,
    ticket: &ContainerPanelActionTicket,
    cx: &mut Context<OperationsView>,
) {
    view.compose_yaml.select_project(project);
    if let Some(path) = view
        .compose_yaml
        .selected_path()
        .map(str::to_owned)
        .or_else(|| project.config_files.first().cloned())
    {
        if view.compact_layout {
            view.request_compose_yaml_workspace(project, &path, ticket, cx);
        } else {
            view.request_compose_yaml_preview(project, &path, ticket, cx);
        }
    }
}
