//! Compose YAML 页内只读预览的路径选择和编辑器布局。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, SharedString,
    component::button::{Button, ButtonVariants},
    div,
    prelude::*,
    px,
};
use kerminal_domain::operations::ComposeProjectTarget;

use crate::theme::ThemeTokens;

use super::super::OperationsView;
use super::super::compose_yaml::compose_yaml_metadata_items;
use super::super::container_panel::ContainerPanelActionTicket;

/// 在项目页内预览 YAML；编辑器显式占满正文视口，使长文件由编辑器自身滚动。
/// 刷新不替换已有正文，也不影响中央工作区。
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
                    view.request_compose_yaml_preview(
                        &project_for_action,
                        &path_for_action,
                        &ticket_for_action,
                        cx,
                    );
                }));
            path_picker = path_picker.child(if selected_path.as_deref() == Some(path.as_str()) {
                button.primary()
            } else {
                button.ghost()
            });
        }
    }

    let editor_body = match (error, editor) {
        (Some(error), _) => yaml_error_message(&error, theme),
        (None, Some(editor)) if preview.as_ref().is_some_and(|preview| !preview.binary) => div()
            .id("compose-yaml-editor-frame")
            .debug_selector(|| "rr-compose-yaml-editor-frame".to_owned())
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(
                gpui_kit::component::input::Editor::new(&editor)
                    .h_full()
                    .w_full()
                    .readonly(true)
                    .appearance(false)
                    .bordered(false)
                    .aria_label(crate::locale::text(
                        "Compose YAML 只读预览",
                        "Compose YAML read-only preview",
                    )),
            )
            .into_any_element(),
        (None, _) => div().size_full().into_any_element(),
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
        .when(!view.compact_layout, |body| body.min_h(px(220.)))
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

/// 错误和二进制状态说明失败原因，重试入口位于项目页标题栏。
///
/// @author kongweiguang
fn yaml_error_message(message: &str, theme: ThemeTokens) -> AnyElement {
    div()
        .id("compose-yaml-error-message")
        .size_full()
        .min_w_0()
        .min_h_0()
        .overflow_y_scroll()
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

/// 所有布局都在当前项目页请求所选路径，复用冻结项目和票据隔离迟到回执。
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
        view.request_compose_yaml_preview(project, &path, ticket, cx);
    }
}
