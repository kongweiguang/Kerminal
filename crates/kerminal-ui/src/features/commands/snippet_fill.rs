//! 右栏片段填入只投递一次 Paste；异步准备与最终写入之间再次核对 UI 草稿。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, IntoElement, Window,
    component::button::{Button, ButtonVariants},
    div,
    prelude::*,
};
use kerminal_application::{
    PublicErrorCode, commands::SnippetRunPreparation, sessions::UiSessionBinding,
};
use tokio_util::sync::CancellationToken;

use super::{
    CommandsView, SnippetCatalogOrigin, SnippetPreferenceKey, SnippetTerminalFillCompleted,
    configuration_watch,
};
use crate::theme::ThemeTokens;

/// 未知回执保留原终端身份，只有明确核对才能再次填入，避免重复粘贴。
#[derive(Default)]
pub(super) struct TerminalFillState {
    cancellation: Option<CancellationToken>,
    unknown_target: Option<UiSessionBinding>,
}

/// 延迟聚焦只接受仍对应当前 Fill 请求、右栏视图及编辑弹层代次和完整 UI 绑定的成功票据。
///
/// @author kongweiguang
pub(super) fn snippet_fill_completion_matches_current(
    event: &SnippetTerminalFillCompleted,
    binding: Option<&UiSessionBinding>,
    snippet_fill_generation: u64,
    session_view_generation: u64,
    snippet_editor_generation: Option<u64>,
) -> bool {
    event.snippet_fill_generation == snippet_fill_generation
        && event.session_view_generation == session_view_generation
        && binding == Some(&event.binding)
        && event.snippet_editor_generation == snippet_editor_generation
}

impl gpui_kit::EventEmitter<SnippetTerminalFillCompleted> for CommandsView {}

impl CommandsView {
    /// 填入与运行互斥；未知结果必须由用户检查原终端后解除门禁。
    pub(super) fn snippet_terminal_fill_blocked(&self) -> bool {
        self.snippet_terminal_fill.cancellation.is_some()
            || self.snippet_terminal_fill.unknown_target.is_some()
    }

    /// 切目标或销毁视图只请求协作取消，已有 writer 仍等待实际回执，不能推断未写入。
    pub(super) fn cancel_terminal_fill(&mut self) {
        if let Some(cancellation) = self.snippet_terminal_fill.cancellation.as_ref() {
            cancellation.cancel();
        }
    }

    /// Root 延迟聚焦前重新确认成功回执仍属于当前请求和绑定，避免异步回调抢占新目标。
    ///
    /// @author kongweiguang
    pub(crate) fn snippet_terminal_fill_completion_is_current(
        &self,
        event: &SnippetTerminalFillCompleted,
    ) -> bool {
        snippet_fill_completion_matches_current(
            event,
            self.ui_session_binding.as_ref(),
            self.snippet_fill_generation,
            self.session_view_generation,
            self.snippet_editor_open
                .then_some(self.snippet_editor_generation),
        )
    }

    /// UI 先冻结已展示定义和变量，准备成功后核对原草稿，最终输入仍由应用层原子检查绑定和 context。
    pub(super) fn fill_snippet_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_terminal_fill_blocked()
            || self.snippet_run.blocks_navigation()
            || self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
        {
            self.status = Some("请先完成或核对当前片段操作".to_owned());
            cx.notify();
            return;
        }
        let (Some(snippet), Some(binding), Some(service)) = (
            self.selected_snippet.clone(),
            self.ui_session_binding.clone(),
            self.snippet_execution.clone(),
        ) else {
            self.status = Some("请先选择片段并绑定可用终端".to_owned());
            cx.notify();
            return;
        };
        let draft = match self.collect_snippet_draft_values(cx) {
            Ok(draft)
                if configuration_watch::snippet_definition_draft_matches_saved(
                    &snippet.definition,
                    &draft,
                ) =>
            {
                draft
            }
            _ => {
                self.status = Some("请先保存片段定义；运行变量会保留".to_owned());
                cx.notify();
                return;
            }
        };
        self.snippet_fill_generation = self.snippet_fill_generation.wrapping_add(1).max(1);
        let guard = self.snippet_fill_guard(self.snippet_fill_generation, draft, cx);
        let request = SnippetRunPreparation {
            key: SnippetPreferenceKey {
                origin: if snippet.revision.is_some() {
                    SnippetCatalogOrigin::User
                } else {
                    SnippetCatalogOrigin::Builtin
                },
                snippet_id: snippet.definition.id.clone(),
            },
            binding: binding.clone(),
            variables: guard.variable_values.clone(),
            record_history: false,
        };
        let cancellation = CancellationToken::new();
        self.snippet_terminal_fill.cancellation = Some(cancellation.clone());
        self.status = Some("正在核对片段与终端…".to_owned());
        let expected_focus = window.focused(cx);
        let snippet_editor_generation = self
            .snippet_editor_open
            .then_some(self.snippet_editor_generation);
        let executor = self.executor.clone();
        let prepare_service = service.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    prepare_service
                        .prepare_fill(request, snippet.definition, snippet.revision)
                        .await
                })
                .await;
            let proceed = cx
                .update(|_, app| {
                    this.update(app, |view, cx| {
                        let current_draft = view.collect_snippet_draft_values(cx);
                        let current = current_draft.ok().map(|draft| {
                            view.snippet_fill_guard(guard.request_generation, draft, cx)
                        });
                        if cancellation.is_cancelled()
                            || current.as_ref().is_none_or(|current| {
                                !guard.same_request_context(current)
                                    || !guard.drafts_unchanged(current)
                            })
                        {
                            view.snippet_terminal_fill.cancellation = None;
                            view.status =
                                Some("填入已取消或草稿、目标已变化；当前输入已保留".to_owned());
                            cx.notify();
                            return false;
                        }
                        true
                    })
                    .unwrap_or(false)
                })
                .unwrap_or(false);
            if !proceed {
                return;
            }
            let result = match result {
                Ok(plan) => {
                    executor
                        .run(move |_| async move { service.fill(plan, cancellation).await })
                        .await
                }
                Err(error) => Err(error),
            };
            let _ = cx.update(|_, app| {
                let _ = this.update(app, |view, cx| {
                    view.snippet_terminal_fill.cancellation = None;
                    let context_matches = view.snippet_fill_generation == guard.request_generation
                        && view.session_view_generation == guard.session_view_generation
                        && view.ui_session_binding == guard.binding;
                    // 已开始写入时取消不能撤回 Paste；目标切换后仍须交代原终端的成功回执。
                    if !context_matches
                        && !result.as_ref().is_err_and(|error| {
                            error.public_code() == PublicErrorCode::OutcomeUnknown
                        })
                    {
                        if result.is_ok() {
                            view.status = Some(format!(
                                "已填入原终端 {}（代次 {}），尚未执行；请检查原终端输入。",
                                binding.session_id, binding.session_generation
                            ));
                        }
                        cx.notify();
                        return;
                    }
                    match result {
                        Ok(_) => {
                            view.status =
                                Some("已填入终端，尚未执行；可继续编辑后按回车".to_owned());
                            cx.emit(SnippetTerminalFillCompleted {
                                binding: binding.clone(),
                                snippet_fill_generation: guard.request_generation,
                                session_view_generation: guard.session_view_generation,
                                expected_focus,
                                snippet_editor_generation,
                            });
                        }
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            view.snippet_terminal_fill.unknown_target = Some(binding.clone());
                            view.status =
                                Some("填入结果未知，请检查原终端后核对；不会自动重放".to_owned());
                        }
                        Err(error) => view.status = Some(error.to_string()),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消等待 writer 回执；未知状态的核对按钮只解除门禁，不把未知记录改报成功。
    pub(super) fn render_snippet_fill_feedback(
        &self,
        theme: ThemeTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut content = div()
            .w_full()
            .flex()
            .flex_col()
            .gap_2()
            .text_color(theme.muted);
        if let Some(cancellation) = self.snippet_terminal_fill.cancellation.as_ref() {
            content = content.child(
                Button::new("snippet-fill-cancel")
                    .ghost()
                    .label(if cancellation.is_cancelled() {
                        "正在等待填入回执…"
                    } else {
                        "取消填入"
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.cancel_terminal_fill();
                        cx.notify();
                    })),
            );
        }
        if let Some(target) = self.snippet_terminal_fill.unknown_target.as_ref() {
            content = content
                .child(format!(
                    "请核对原终端 {}（代次 {}）的输入，避免重复填入。",
                    target.session_id, target.session_generation
                ))
                .child(
                    Button::new("snippet-fill-reviewed")
                        .ghost()
                        .label("已核对原终端")
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.snippet_terminal_fill.unknown_target = None;
                            view.status = Some("已记录核对；需要时可再次填入".to_owned());
                            cx.notify();
                        })),
                );
        }
        content
    }
}
