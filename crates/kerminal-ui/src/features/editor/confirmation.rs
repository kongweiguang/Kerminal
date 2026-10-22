//! 脏文件关闭、冲突覆盖和重载确认视图。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement,
    component::button::{Button, ButtonVariants as _},
    div,
    prelude::*,
};

use crate::theme::ThemeTokens;

use super::workspace::{EditorConfirmation, FileEditorWorkspace, endpoint_title};

impl FileEditorWorkspace {
    /// 仅把原 Entity 持有的确认内容交给根遮罩；不重新解析活动标签或复制保存状态机。
    /// @author kongweiguang
    pub(crate) fn render_root_confirmation(
        &mut self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let confirmation = self.confirmation().cloned()?;
        Some(
            self.render_confirmation(confirmation, theme, cx)
                .into_any_element(),
        )
    }

    /// 确认说明与可换行按钮分开；根层统一限制短矮视口，按钮仍绑定原 endpoint 与动作语义。
    /// @author kongweiguang
    pub(super) fn render_confirmation(
        &mut self,
        confirmation: EditorConfirmation,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        match confirmation {
            EditorConfirmation::Close(endpoint) => {
                let save_endpoint = endpoint.clone();
                let discard_endpoint = endpoint.clone();
                div()
                    .id("file-editor-confirm-close")
                    .w_full()
                    .px_3()
                    .py_3()
                    .min_w(gpui_kit::px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.panel)
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .child(crate::locale::text(
                                "有未保存修改。保存后关闭、丢弃修改或取消关闭。",
                                "There are unsaved changes. Save before closing, discard the changes, or cancel.",
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("file-editor-save-close")
                                    .compact()
                                    .flex_shrink_0()
                                    .primary()
                                    .label(crate::locale::text("保存并关闭", "Save and close"))
                                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                                        workspace.confirm_save_and_close(save_endpoint.clone(), false, cx);
                                    })),
                            )
                            .child(
                                Button::new("file-editor-discard-close")
                                    .compact()
                                    .flex_shrink_0()
                                    .label(crate::locale::text("丢弃修改", "Discard changes"))
                                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                                        workspace.confirm_discard_and_close(
                                            discard_endpoint.clone(),
                                            false,
                                            cx,
                                        );
                                    })),
                            )
                            .child(cancel_button(cx)),
                    )
            }
            EditorConfirmation::Quit(endpoint) => {
                let save_endpoint = endpoint.clone();
                let discard_endpoint = endpoint.clone();
                div()
                    .id("file-editor-confirm-quit")
                    .w_full()
                    .px_3()
                    .py_3()
                    .min_w(gpui_kit::px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.panel)
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .child(crate::locale::text(
                                format!("{} 有未保存修改。", endpoint_title(&endpoint)),
                                format!("{} has unsaved changes.", endpoint_title(&endpoint)),
                            )),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("file-editor-save-continue-quit")
                                    .compact()
                                    .flex_shrink_0()
                                    .primary()
                                    .label(crate::locale::text("保存并继续退出", "Save and quit"))
                                    .on_click(cx.listener(
                                        move |workspace, _: &ClickEvent, _, cx| {
                                            workspace.confirm_save_and_close(
                                                save_endpoint.clone(),
                                                true,
                                                cx,
                                            );
                                        },
                                    )),
                            )
                            .child(
                                Button::new("file-editor-discard-continue-quit")
                                    .compact()
                                    .flex_shrink_0()
                                    .label(crate::locale::text(
                                        "放弃并继续退出",
                                        "Discard and quit",
                                    ))
                                    .on_click(cx.listener(
                                        move |workspace, _: &ClickEvent, _, cx| {
                                            workspace.confirm_discard_and_close(
                                                discard_endpoint.clone(),
                                                true,
                                                cx,
                                            );
                                        },
                                    )),
                            )
                            .child(cancel_button(cx)),
                    )
            }
            EditorConfirmation::Overwrite {
                endpoint,
                close_after_success,
                quit_after_success,
            } => div()
                .id("file-editor-confirm-overwrite")
                .w_full()
                .px_3()
                .py_3()
                .min_w(gpui_kit::px(0.))
                .flex()
                .flex_col()
                .gap_2()
                .bg(theme.panel)
                .child(
                    div()
                        .w_full()
                        .min_w(gpui_kit::px(0.))
                        .child(crate::locale::text(
                            "确认覆盖会替换远端文件当前内容。",
                            "Confirming overwrite replaces the current contents of the remote file.",
                        )),
                )
                .child(
                    div()
                        .w_full()
                        .min_w(gpui_kit::px(0.))
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap_2()
                        .child(
                            Button::new("file-editor-overwrite-confirm")
                                .compact()
                                .flex_shrink_0()
                                .primary()
                                .label(crate::locale::text("覆盖远端文件", "Overwrite remote file"))
                                .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                                    workspace.confirm_overwrite(
                                        endpoint.clone(),
                                        close_after_success,
                                        quit_after_success,
                                        cx,
                                    );
                                })),
                        )
                        .child(cancel_button(cx)),
                ),
            EditorConfirmation::Reload {
                endpoint,
                continue_quit,
                close_after_readback,
            } => {
                let description = if continue_quit {
                    crate::locale::text(
                        "保存结果待确认。重新读取文件后继续退出；本次保存不会自动重试。",
                        "The save result is unknown. Read the file back before quitting; the save will not be retried.",
                    )
                } else if close_after_readback {
                    crate::locale::text(
                        "先读回文件确认当前内容；读取成功后关闭此标签，读取失败会保留原内容。",
                        "Read the file back first. This tab closes only after a successful read; a failed read keeps the current contents.",
                    )
                } else {
                    crate::locale::text(
                        "重新加载将丢弃本地未保存内容，并读取远端最新版本。",
                        "Reloading discards local unsaved changes and reads the latest remote version.",
                    )
                };
                let label = if continue_quit {
                    crate::locale::text("读取并继续退出", "Read back and quit")
                } else if close_after_readback {
                    crate::locale::text("读回并关闭文件", "Read back and close file")
                } else {
                    crate::locale::text("丢弃本地修改并重载", "Discard local changes and reload")
                };
                div()
                    .id("file-editor-confirm-reload")
                    .w_full()
                    .px_3()
                    .py_3()
                    .min_w(gpui_kit::px(0.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .bg(theme.panel)
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .child(description),
                    )
                    .child(
                        div()
                            .w_full()
                            .min_w(gpui_kit::px(0.))
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("file-editor-reload-confirm")
                                    .compact()
                                    .flex_shrink_0()
                                    .primary()
                                    .label(label)
                                    .on_click(cx.listener(move |workspace, _: &ClickEvent, _, cx| {
                                        workspace.confirm_reload(
                                            endpoint.clone(),
                                            continue_quit,
                                            close_after_readback,
                                            cx,
                                        );
                                    })),
                            )
                            .child(cancel_button(cx)),
                    )
            }
        }
    }
}

/// 给所有确认框使用一致的取消按钮，避免隐藏确认后继续执行旧选择。
fn cancel_button(cx: &mut Context<FileEditorWorkspace>) -> Button {
    Button::new("file-editor-confirm-cancel")
        .compact()
        .flex_shrink_0()
        .label(crate::locale::text("取消", "Cancel"))
        .on_click(cx.listener(|workspace, _: &ClickEvent, _, cx| {
            workspace.cancel_confirmation(cx);
        }))
}
