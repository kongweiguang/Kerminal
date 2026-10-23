//! 目录行展开的安全预览与操作；业务状态继续由 CommandsView 唯一持有。
//!
//! @author kongweiguang

use gpui_kit::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, StatefulInteractiveElement as _,
    assets::IconName,
    component::{
        Disableable as _,
        button::{Button, ButtonVariants},
    },
    div,
    prelude::*,
    px,
};
use kerminal_domain::commands::{SnippetDefaultAction, SnippetPreferenceKey};

use crate::theme::ThemeTokens;

use super::super::super::{CommandsView, snippet_browser::SnippetInlinePreview};

impl CommandsView {
    /// 预览从表单实体读取模板，与发送共享同一未保存正文，避免编辑器关闭后展示旧定义。
    /// 宽工作台仍保留独立的草稿填入入口；展开预览本身不执行片段。
    ///
    /// @author kongweiguang
    pub(super) fn render_snippet_inline_preview(
        &mut self,
        key: &SnippetPreferenceKey,
        stale: bool,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<gpui_kit::AnyElement> {
        if stale {
            return None;
        }

        let state = self.snippet_inline_preview.clone();
        match state {
            SnippetInlinePreview::Collapsed => None,
            SnippetInlinePreview::Loading(loading_key) if loading_key == *key => Some(
                div()
                    .id(format!(
                        "snippet-preview-loading-{}-{}",
                        preview_origin(key),
                        key.snippet_id
                    ))
                    .w_full()
                    .px_3()
                    .pb_2()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                    .child(crate::locale::text(
                        "正在读取片段详情…",
                        "Loading snippet details…",
                    ))
                    .into_any_element(),
            ),
            SnippetInlinePreview::Failed {
                key: failed_key,
                message,
            } if failed_key == *key => Some(
                div()
                    .id(format!(
                        "snippet-preview-failed-{}-{}",
                        preview_origin(key),
                        key.snippet_id
                    ))
                    .w_full()
                    .px_3()
                    .pb_2()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(theme.text_xs)
                            .text_color(theme.error)
                            .child(message),
                    )
                    .child(
                        Button::new(format!(
                            "snippet-preview-retry-{}-{}",
                            preview_origin(key),
                            key.snippet_id
                        ))
                        .compact()
                        .secondary()
                        .label(crate::locale::text("重试", "Retry"))
                        .on_click(cx.listener({
                            let key = key.clone();
                            move |view, _: &ClickEvent, window, cx| {
                                view.retry_snippet_preview(key.clone(), window, cx);
                            }
                        })),
                    )
                    .into_any_element(),
            ),
            SnippetInlinePreview::Ready(ready_key) if ready_key == *key => {
                if !self.selected_snippet_matches_key(key) {
                    return Some(
                        div()
                            .id("snippet-preview-selection-mismatch")
                            .w_full()
                            .px_3()
                            .pb_2()
                            .text_size(theme.text_xs)
                            .text_color(theme.warning)
                            .child(crate::locale::text(
                                "详情来源已变化。请收起并重新展开此条目。",
                                "The detail source changed. Collapse and reopen this entry.",
                            ))
                            .into_any_element(),
                    );
                }

                let command = self
                    .snippet_command
                    .update(cx, |input, _| input.text().to_string());
                let command_label = crate::locale::text("命令模板", "Command template");
                let copy_label = crate::locale::text("复制片段模板", "Copy snippet template");
                let variables = self.render_snippet_runtime_variables(theme, cx);
                let fill_primary = self.snippet_default_action == SnippetDefaultAction::Insert;
                let run_primary = self.snippet_default_action == SnippetDefaultAction::Run;
                let fill_blocked = self.snippet_terminal_fill_blocked();
                let has_target = self.ui_session_binding.is_some() && self.session_id.is_some();
                let can_edit = !self.snippet_saving
                    && !self.confirm_snippet_delete
                    && self.pending_snippet_transition.is_none()
                    && !self.snippet_terminal_fill_in_flight();
                let can_fill = !self.snippet_saving
                    && self.pending_snippet_transition.is_none()
                    && !self.snippet_terminal_fill_in_flight();
                let can_send = has_target
                    && self.snippet_execution.is_some()
                    && !fill_blocked
                    && !self.snippet_saving
                    && !self.confirm_snippet_delete
                    && self.pending_snippet_transition.is_none();
                let variable_body = (!variables.is_empty()).then(|| {
                    div()
                        .id("snippet-preview-runtime-variables")
                        .w_full()
                        .max_h(px(144.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .children(variables)
                });

                Some(
                    div()
                        .id(format!(
                            "snippet-preview-ready-{}-{}",
                            preview_origin(key),
                            key.snippet_id
                        ))
                        .w_full()
                        .min_w(px(0.))
                        .px_3()
                        .pb_3()
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(
                            div()
                                .w_full()
                                .min_w(px(0.))
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_size(theme.text_xs)
                                        .text_color(theme.muted)
                                        .child(command_label),
                                )
                                .child(
                                    div()
                                        .id("snippet-preview-command-template")
                                        .w_full()
                                        .max_h(px(88.))
                                        .overflow_x_scroll()
                                        .overflow_y_scroll()
                                        .p_3()
                                        .rounded_md()
                                        .bg(theme.materials.panel)
                                        .text_size(theme.text_xs)
                                        .text_color(theme.foreground)
                                        .child(command),
                                ),
                        )
                        .when_some(variable_body, |panel, variables| panel.child(variables))
                        .child(
                            div()
                                .w_full()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .justify_between()
                                .gap_3()
                                .border_t_1()
                                .border_color(theme.border)
                                .pt_2()
                                .child(
                                    Button::new(format!(
                                        "snippet-preview-copy-{}-{}",
                                        preview_origin(key),
                                        key.snippet_id
                                    ))
                                    .compact()
                                    .ghost()
                                    .w(px(32.))
                                    .h(px(32.))
                                    .icon(IconName::Copy)
                                    .accessibility_label(copy_label.clone())
                                    .tooltip(copy_label)
                                    .on_click(cx.listener({
                                        let key = key.clone();
                                        move |view, _: &ClickEvent, _, cx| {
                                            view.copy_snippet_inline_preview(key.clone(), cx);
                                        }
                                    })),
                                )
                                .when(
                                    key.origin
                                        == kerminal_domain::commands::SnippetCatalogOrigin::Builtin,
                                    |actions| {
                                        actions.child(
                                            Button::new(format!(
                                                "snippet-preview-save-to-mine-{}",
                                                key.snippet_id
                                            ))
                                            .compact()
                                            .ghost()
                                            .w(px(32.))
                                            .h(px(32.))
                                            .icon(IconName::Plus)
                                            .accessibility_label(crate::locale::text(
                                                "保存到我的片段",
                                                "Save to my snippets",
                                            ))
                                            .tooltip(crate::locale::text(
                                                "保存到我的片段",
                                                "Save to my snippets",
                                            ))
                                            .disabled(!can_edit)
                                            .on_click(cx.listener(
                                                |view, _: &ClickEvent, window, cx| {
                                                    view.save_builtin_snippet_as_user(window, cx);
                                                },
                                            )),
                                        )
                                    },
                                )
                                .when(
                                    key.origin
                                        == kerminal_domain::commands::SnippetCatalogOrigin::User,
                                    |actions| {
                                        actions
                                            .child(
                                                Button::new(format!(
                                                    "snippet-preview-edit-{}-{}",
                                                    preview_origin(key),
                                                    key.snippet_id
                                                ))
                                                .compact()
                                                .h(px(32.))
                                                .secondary()
                                                .label(crate::locale::text("编辑", "Edit"))
                                                .disabled(!can_edit)
                                                .on_click(cx.listener({
                                                    let key = key.clone();
                                                    move |view, _: &ClickEvent, window, cx| {
                                                        view.open_snippet_key(
                                                            key.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                    }
                                                })),
                                            )
                                            .child(
                                                Button::new(format!(
                                                    "snippet-preview-delete-{}-{}",
                                                    preview_origin(key),
                                                    key.snippet_id
                                                ))
                                                .compact()
                                                .danger()
                                                .w(px(32.))
                                                .h(px(32.))
                                                .icon(IconName::Trash)
                                                .accessibility_label(crate::locale::text(
                                                    "删除我的片段",
                                                    "Delete my snippet",
                                                ))
                                                .tooltip(crate::locale::text(
                                                    "删除我的片段",
                                                    "Delete my snippet",
                                                ))
                                                .disabled(!can_edit)
                                                .on_click(cx.listener({
                                                    let key = key.clone();
                                                    move |view, _: &ClickEvent, window, cx| {
                                                        view.open_snippet_key(
                                                            key.clone(),
                                                            window,
                                                            cx,
                                                        );
                                                        if view.snippet_editor_open() {
                                                            view.delete_snippet(window, cx);
                                                        }
                                                    }
                                                })),
                                            )
                                    },
                                )
                                .when(!self.compact_layout, |actions| {
                                    actions.child(
                                        Button::new(format!(
                                            "snippet-preview-fill-{}-{}",
                                            preview_origin(key),
                                            key.snippet_id
                                        ))
                                        .compact()
                                        .h(px(32.))
                                        .when(fill_primary, Button::primary)
                                        .when(!fill_primary, Button::secondary)
                                        .label(crate::locale::text("填入草稿", "Fill draft"))
                                        .tooltip(crate::locale::text(
                                            "把展开后的命令填入工作台草稿",
                                            "Put the rendered command in the workbench draft",
                                        ))
                                        .disabled(!can_fill)
                                        .on_click(
                                            cx.listener(|view, _: &ClickEvent, window, cx| {
                                                view.fill_snippet(window, cx);
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    Button::new(format!(
                                        "snippet-preview-send-{}-{}",
                                        preview_origin(key),
                                        key.snippet_id
                                    ))
                                    .compact()
                                    .h(px(32.))
                                    .when(run_primary, Button::primary)
                                    .when(!run_primary, Button::secondary)
                                    .label(crate::locale::text("发送", "Send"))
                                    .tooltip(crate::locale::text(
                                        "发送到当前终端",
                                        "Send to the current terminal",
                                    ))
                                    .disabled(!can_send)
                                    .on_click(cx.listener(
                                        |view, _: &ClickEvent, window, cx| {
                                            view.send_snippet_terminal(window, cx);
                                        },
                                    )),
                                ),
                        )
                        .into_any_element(),
                )
            }
            SnippetInlinePreview::Loading(_)
            | SnippetInlinePreview::Ready(_)
            | SnippetInlinePreview::Failed { .. } => None,
        }
    }
}

/// 元素 ID 仅区分预览宿主；业务身份仍由完整 origin+snippet_id 键验证。
///
/// @author kongweiguang
fn preview_origin(key: &SnippetPreferenceKey) -> &'static str {
    match key.origin {
        kerminal_domain::commands::SnippetCatalogOrigin::Builtin => "builtin",
        kerminal_domain::commands::SnippetCatalogOrigin::User => "user",
    }
}
