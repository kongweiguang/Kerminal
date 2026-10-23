//! 右栏片段发送只投递一次 Paste+Enter；目标仍由点击时捕获的 UI binding 固定。
//!
//! @author kongweiguang

use gpui_kit::{
    Context, IntoElement, Window,
    component::button::{Button, ButtonVariants},
    div,
    prelude::*,
};
use kerminal_application::{
    PublicErrorCode,
    commands::{SnippetRenderResult, SnippetRunHistory, direct_snippet_command},
    sessions::UiSessionBinding,
};
use kerminal_domain::commands::{SnippetDefinition, SnippetVariableKind};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroize;

use super::{CommandsView, SnippetTerminalFillCompleted, command_message};
use crate::theme::ThemeTokens;

/// 未知回执保留发送时的原终端身份，只有明确核对后才允许再次发送。
#[derive(Default)]
pub(super) struct TerminalFillState {
    pub(super) cancellation: Option<CancellationToken>,
    pub(super) unknown_target: Option<UiSessionBinding>,
}

impl TerminalFillState {
    /// 活跃发送和未知结果都必须阻止重放；人工检查仅清除未知目标状态。
    ///
    /// @author kongweiguang
    pub(super) fn blocks_new_send(&self) -> bool {
        self.cancellation.is_some() || self.unknown_target.is_some()
    }
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

/// 从当前表单实体生成一次性渲染快照；选中定义只提供身份与默认元数据，不覆盖未保存正文。
/// 运行时输入在离开此 helper 前清零，敏感标志同时继承目录项和当前变量定义。
///
/// @author kongweiguang
pub(super) fn render_selected_snippet_direct(
    view: &mut CommandsView,
    cx: &mut Context<CommandsView>,
) -> Option<SnippetRenderResult> {
    let mut definition = view
        .selected_snippet
        .as_ref()
        .map(|snippet| snippet.definition.clone())
        .or_else(|| {
            view.snippet_is_new.then(|| SnippetDefinition {
                id: String::new(),
                title: String::new(),
                command: String::new(),
                description: None,
                tags: Vec::new(),
                sort_order: 0,
                scope: view.snippet_scope,
                category: None,
                risk: view.snippet_risk,
                default_action: view.snippet_default_action,
                variables: Vec::new(),
                context_bindings: Vec::new(),
                derived_from: None,
            })
        })?;
    let command = view
        .snippet_command
        .update(cx, |input, _| input.text().to_string());
    definition.command.zeroize();
    definition.command = command;
    definition.variables = view
        .snippet_variables
        .iter()
        .map(|field| {
            let mut variable = field.variable.clone();
            variable.name = field.name.update(cx, |input, _| input.value().to_string());
            variable
        })
        .collect();
    let mut values = view
        .snippet_variables
        .iter()
        .map(|field| {
            (
                field.name.update(cx, |input, _| input.value().to_string()),
                field.input.update(cx, |input, _| input.value().to_string()),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    let mut rendered = direct_snippet_command(&definition, &values);
    definition.command.zeroize();
    values.values_mut().for_each(String::zeroize);
    rendered.sensitive |= view.selected_snippet_sensitive
        || view.snippet_variables.iter().any(|field| {
            field.variable.sensitive || field.variable.kind == SnippetVariableKind::Secret
        });
    Some(rendered)
}

impl CommandsView {
    /// 未知写入结果必须由用户检查原终端后解除门禁，防止重复发送。
    ///
    /// @author kongweiguang
    pub(super) fn snippet_terminal_fill_blocked(&self) -> bool {
        self.snippet_terminal_fill.blocks_new_send()
    }

    /// 表单只在 writer 等待期间冻结，未知回执仍允许查看和修改片段内容。
    ///
    /// @author kongweiguang
    pub(super) fn snippet_terminal_fill_in_flight(&self) -> bool {
        self.snippet_terminal_fill.cancellation.is_some()
    }

    /// 在 writer 启动前取消本次请求；writer 启动后仍等待确定回执，不用取消信号推断未发送。
    ///
    /// @author kongweiguang
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

    /// 显式发送时只做必要的目标身份和并发门禁；不探测平台、shell 或 prompt，也不改变终端焦点。
    /// Unknown 保留在原目标上，用户核对前不允许重放；变量副本与敏感命令交由零化边界清理。
    ///
    /// @author kongweiguang
    pub(super) fn send_snippet_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.snippet_terminal_fill_blocked()
            || self.snippet_saving
            || self.confirm_snippet_delete
            || self.pending_snippet_transition.is_some()
        {
            self.status = Some(command_message(
                "请先完成或核对当前片段操作",
                "Finish or review the current snippet action first",
            ));
            cx.notify();
            return;
        }
        let (Some(binding), Some(service)) = (
            self.ui_session_binding.clone(),
            self.snippet_execution.clone(),
        ) else {
            self.status = Some(command_message(
                "请选择片段并绑定终端",
                "Select a snippet and bind a terminal",
            ));
            cx.notify();
            return;
        };
        if self.session_id.as_ref() != Some(&binding.session_id) {
            self.status = Some(command_message(
                "终端绑定已变化，请重新选择目标",
                "The terminal binding changed; select the target again",
            ));
            cx.notify();
            return;
        }
        let Some(rendered) = render_selected_snippet_direct(self, cx) else {
            self.status = Some(command_message("请先选择片段", "Select a snippet first"));
            cx.notify();
            return;
        };
        if rendered.command.trim().is_empty() {
            let mut rendered = rendered;
            if rendered.sensitive {
                rendered.command.zeroize();
            }
            self.status = Some(command_message(
                "命令片段内容为空",
                "Snippet command is empty",
            ));
            cx.notify();
            return;
        }
        self.snippet_fill_generation = self.snippet_fill_generation.wrapping_add(1).max(1);
        let request_generation = self.snippet_fill_generation;
        let session_view_generation = self.session_view_generation;
        let snippet_generation = self.snippet_generation;
        let editor_generation = self.snippet_editor_generation;
        let record_history = self.record_command && !rendered.sensitive;
        let cancellation = CancellationToken::new();
        self.snippet_terminal_fill.cancellation = Some(cancellation.clone());
        self.status = Some(command_message("正在发送…", "Sending…"));
        let executor = self.executor.clone();
        let send_binding = binding.clone();
        let send_cancellation = cancellation.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(
                    send_cancellation,
                    move |operation_cancellation| async move {
                        service
                            .send_direct(
                                rendered,
                                send_binding,
                                record_history,
                                operation_cancellation,
                            )
                            .await
                    },
                )
                .await;
            let _ = cx.update(|_, app| {
                let _ = this.update(app, |view, cx| {
                    view.snippet_terminal_fill.cancellation = None;
                    let context_matches = view.snippet_fill_generation == request_generation
                        && view.session_view_generation == session_view_generation
                        && view.snippet_generation == snippet_generation
                        && view.snippet_editor_generation == editor_generation
                        && view.ui_session_binding.as_ref() == Some(&binding);
                    match result {
                        Ok(SnippetRunHistory::Failed { .. }) => {
                            if context_matches {
                                view.status = Some(command_message(
                                    "已发送到终端，历史记录失败",
                                    "Sent to terminal; history could not be saved",
                                ));
                            }
                        }
                        Ok(_) => {
                            if context_matches {
                                view.status =
                                    Some(command_message("已发送到终端", "Sent to terminal"));
                            }
                        }
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            view.snippet_terminal_fill.unknown_target = Some(binding.clone());
                            view.status = Some(command_message(
                                "发送结果未知，请检查原终端",
                                "Send result is unknown; check the original terminal",
                            ));
                        }
                        Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                            if !context_matches {
                                cx.notify();
                                return;
                            }
                            view.status = Some(command_message("发送已取消", "Send cancelled"));
                        }
                        Err(error) if context_matches => {
                            view.status = Some(error.public_message().to_owned())
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消尚未开始的发送；未知状态的核对按钮只解除重发门禁。
    ///
    /// @author kongweiguang
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
                        crate::locale::text("等待中…", "Waiting…")
                    } else {
                        crate::locale::text("取消发送", "Cancel send")
                    })
                    .on_click(cx.listener(|view, _, _, cx| {
                        view.cancel_terminal_fill();
                        cx.notify();
                    })),
            );
        }
        if self.snippet_terminal_fill.unknown_target.is_some() {
            content = content
                .child(crate::locale::text(
                    "发送结果未知，请检查原终端",
                    "Send result is unknown; check the original terminal",
                ))
                .child(
                    Button::new("snippet-fill-reviewed")
                        .ghost()
                        .label(crate::locale::text(
                            "已检查原终端，允许再次发送",
                            "I checked the original terminal; allow another send",
                        ))
                        .on_click(cx.listener(|view, _, _, cx| {
                            view.snippet_terminal_fill.unknown_target = None;
                            view.status =
                                Some(command_message("已核对原终端", "Original terminal checked"));
                            cx.notify();
                        })),
                );
        }
        content
    }
}
