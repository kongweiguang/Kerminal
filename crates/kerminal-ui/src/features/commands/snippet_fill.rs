//! 右栏片段 Fill 只写入 Paste，Run 才提交 Enter；目标均由点击时捕获的 UI binding 固定。
//!
//! @author kongweiguang

use std::{future::Future, sync::Arc};

use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, IntoElement, Render, Subscription, Window,
    component::{
        Disableable as _, WindowExt as _,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
    },
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    ApplicationTaskExecutor, PublicErrorCode,
    commands::{SnippetRenderResult, SnippetRunHistory, direct_snippet_command},
    sessions::UiSessionBinding,
};
use kerminal_domain::commands::{
    SnippetCatalogOrigin, SnippetDefinition, SnippetPreferenceKey, SnippetRisk, SnippetUsageAction,
    SnippetUsageRecordRequest, SnippetVariableKind, SnippetVariableRenderStrategy,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroize;

use super::{CommandsView, SnippetTerminalFillCompleted, command_message};
use crate::theme::ThemeTokens;

/// 未知回执保留操作时的原终端身份，只有明确核对后才允许再次写入。
#[derive(Default)]
pub(super) struct TerminalFillState {
    pub(super) cancellation: Option<CancellationToken>,
    pub(super) unknown_target: Option<UiSessionBinding>,
}

/// Run 确认期间冻结正文、风险级别和完整目标代次；Drop 会清除展开后的敏感命令。
pub(super) struct PendingSnippetRun {
    command: String,
    sensitive: bool,
    key: Option<SnippetPreferenceKey>,
    title: String,
    target_label: String,
    requires_strong_confirmation: bool,
    binding: UiSessionBinding,
    snippet_generation: u64,
    session_view_generation: u64,
    snippet_editor_generation: u64,
}

impl PendingSnippetRun {
    /// 确认必须仍对应打开时的 pane binding、片段身份和 UI 代次，防止旧预览写入新目标。
    fn matches_current(&self, view: &CommandsView) -> bool {
        view.ui_session_binding.as_ref() == Some(&self.binding)
            && view.session_id.as_ref() == Some(&self.binding.session_id)
            && view.snippet_generation == self.snippet_generation
            && view.session_view_generation == self.session_view_generation
            && view.snippet_editor_generation == self.snippet_editor_generation
            && selected_preference_key(view) == self.key
    }
}

impl Drop for PendingSnippetRun {
    /// 确认取消、目标失效或提交后尽早清除冻结的命令副本。
    fn drop(&mut self) {
        self.command.zeroize();
    }
}

/// 用量回写需要 Tokio reactor，因此连 future 的创建也交给共享应用 executor；回写失败不能撤销已确认的终端动作。
///
/// @author kongweiguang
pub(super) async fn record_snippet_usage_best_effort<Build, Usage>(
    executor: ApplicationTaskExecutor,
    build: Build,
) where
    Build: FnOnce() -> Usage + Send + 'static,
    Usage: Future<Output = kerminal_application::AppResult<()>> + Send + 'static,
{
    let _ = executor.run(move |_| build()).await;
}

/// Run 确认窗持有局部输入状态，只有精确目标名匹配后才启用破坏性提交。
struct SnippetRunConfirmationView {
    parent: Entity<CommandsView>,
    title: String,
    target_label: String,
    command_preview: String,
    requires_strong_confirmation: bool,
    target_confirmation: Entity<InputState>,
    confirmation_matches: bool,
    _target_subscription: Subscription,
}

impl SnippetRunConfirmationView {
    /// 对话框独立持有确认输入，避免复用片段运行时变量或把确认文本写入草稿。
    fn new(
        parent: Entity<CommandsView>,
        title: String,
        target_label: String,
        command_preview: String,
        requires_strong_confirmation: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let target_confirmation = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(crate::locale::text("输入目标名称", "Enter the target name"))
        });
        let target_subscription = cx.subscribe_in(
            &target_confirmation,
            window,
            |view, _, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    view.confirmation_matches =
                        view.target_confirmation.read(cx).value() == view.target_label;
                    cx.notify();
                }
            },
        );
        Self {
            parent,
            title,
            target_label,
            command_preview,
            requires_strong_confirmation,
            target_confirmation,
            confirmation_matches: !requires_strong_confirmation,
            _target_subscription: target_subscription,
        }
    }
}

impl Render for SnippetRunConfirmationView {
    /// 展示固定目标与运行快照；强确认只在输入目标名完全匹配时开放提交。
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = ThemeTokens::current(window, cx);
        let title = self.title.clone();
        let target = self.target_label.clone();
        let confirm_disabled = !self.confirmation_matches;
        let command_lines = self
            .command_preview
            .split('\n')
            .map(|line| {
                div()
                    .w_full()
                    .whitespace_normal()
                    .child(line.strip_suffix('\r').unwrap_or(line).to_owned())
            })
            .collect::<Vec<_>>();
        let mut body = div()
            .id("snippet-run-confirmation-content")
            .w_full()
            .flex()
            .flex_col()
            .gap_3()
            .text_color(theme.foreground)
            .child(div().text_size(theme.text_sm).child(crate::locale::text(
                format!("片段：{title} · 目标：{target}"),
                format!("Snippet: {title} · Target: {target}"),
            )))
            .child(
                div()
                    .id("snippet-run-confirmation-preview")
                    .w_full()
                    .max_h(px(160.))
                    .overflow_y_scroll()
                    .rounded_sm()
                    .border_l_2()
                    .border_color(theme.warning)
                    .bg(theme.panel)
                    .px_3()
                    .py_2()
                    .font_family("monospace")
                    .text_size(theme.text_xs)
                    .children(command_lines),
            );
        if self.requires_strong_confirmation {
            body = body.child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(crate::locale::text(
                        format!("输入目标名称“{target}”以确认"),
                        format!("Enter the target name “{target}” to confirm"),
                    ))
                    .child(Input::new(&self.target_confirmation).w_full()),
            );
        }
        body.child(
            div()
                    .text_size(theme.text_xs)
                    .text_color(theme.muted)
                .child(crate::locale::text(
                    "命令将提交到显示的终端，请核对目标和参数。",
                    "This command will be submitted to the shown terminal. Check the target and arguments.",
                )),
        )
        .child(
            div()
                .w_full()
                .flex()
                .justify_end()
                .gap_2()
                .child(
                    Button::new("snippet-run-cancel")
                        .compact()
                        .label(crate::locale::text("取消", "Cancel"))
                        .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                            let parent = view.parent.clone();
                            parent.update(cx, |view, cx| {
                                view.cancel_snippet_run_confirmation(cx);
                            });
                            window.close_dialog(cx);
                        })),
                )
                .child(
                    Button::new("snippet-run-confirm")
                        .compact()
                        .when(self.requires_strong_confirmation, Button::danger)
                        .when(!self.requires_strong_confirmation, Button::primary)
                        .label(crate::locale::text("确认提交", "Confirm run"))
                        .disabled(confirm_disabled)
                        .on_click(cx.listener(|view, _: &ClickEvent, window, cx| {
                            let entered_target = view
                                .target_confirmation
                                .read(cx)
                                .value()
                                .to_string();
                            let parent = view.parent.clone();
                            let confirmed = parent.update(cx, |parent, cx| {
                                parent.confirm_snippet_run(&entered_target, cx)
                            });
                            if confirmed {
                                window.close_dialog(cx);
                            }
                        })),
                ),
        )
    }
}

impl TerminalFillState {
    /// 活跃操作和未知结果都必须阻止重放；人工检查仅清除未知目标状态。
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

    /// 右栏 Fill 沿用 main 对当前预览的单行粘贴；不要求 Shell 集成，原 binding 仍由 writer 原子复核。
    /// 成功后只发带点击时焦点的票据，Root 再决定是否可将焦点交还原终端。
    ///
    /// @author kongweiguang
    pub(super) fn fill_snippet_terminal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
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
        let (Some(binding), Some(service), Some(selected)) = (
            self.ui_session_binding.clone(),
            self.snippet_execution.clone(),
            self.selected_snippet.clone(),
        ) else {
            self.status = Some(command_message(
                "请先选择已保存的片段并绑定终端",
                "Select a saved snippet and bind a terminal first",
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
        let key = preference_key(&selected.definition.id, selected.revision.is_some());
        let Some(rendered) = render_selected_snippet_direct(self, cx) else {
            return;
        };
        self.snippet_fill_generation = self.snippet_fill_generation.wrapping_add(1).max(1);
        let request_generation = self.snippet_fill_generation;
        let session_view_generation = self.session_view_generation;
        let snippet_generation = self.snippet_generation;
        let editor_generation = self.snippet_editor_generation;
        let expected_focus = window.focused(cx);
        let snippet_editor_generation = self.snippet_editor_open.then_some(editor_generation);
        let cancellation = CancellationToken::new();
        self.snippet_terminal_fill.cancellation = Some(cancellation.clone());
        self.status = Some(command_message("正在填入终端…", "Filling terminal…"));
        let executor = self.executor.clone();
        let usage_commands = self.commands.clone();
        let fill_binding = binding.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    service
                        .fill_direct(rendered, fill_binding, operation_cancellation)
                        .await
                        .map(|_| ())
                })
                .await;
            let record_usage = result.is_ok();
            let _ = cx.update(|_, app| {
                let _ = this.update(app, |view, cx| {
                    view.snippet_terminal_fill.cancellation = None;
                    let context_matches = view.snippet_fill_generation == request_generation
                        && view.session_view_generation == session_view_generation
                        && view.snippet_generation == snippet_generation
                        && view.snippet_editor_generation == editor_generation
                        && view.ui_session_binding.as_ref() == Some(&binding);
                    match result {
                        Ok(()) if context_matches => {
                            view.status = Some(command_message(
                                "已填入终端，可继续编辑",
                                "Filled terminal input; you can keep editing",
                            ));
                            cx.emit(SnippetTerminalFillCompleted {
                                binding: binding.clone(),
                                snippet_fill_generation: request_generation,
                                session_view_generation,
                                expected_focus,
                                snippet_editor_generation,
                            });
                        }
                        Ok(()) => {}
                        Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                            view.snippet_terminal_fill.unknown_target = Some(binding.clone());
                            view.status = Some(command_message(
                                "填入结果未知，请检查原终端",
                                "Fill result is unknown; check the original terminal",
                            ));
                        }
                        Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                            if context_matches {
                                view.status = Some(command_message("填入已取消", "Fill cancelled"));
                            }
                        }
                        Err(error) if context_matches => {
                            view.status = Some(error.public_message().to_owned());
                        }
                        Err(_) => {}
                    }
                    cx.notify();
                });
            });
            if record_usage {
                let request = SnippetUsageRecordRequest {
                    receipt_id: Uuid::new_v4().to_string(),
                    key,
                    action: SnippetUsageAction::Insert,
                };
                record_snippet_usage_best_effort(executor, move || async move {
                    usage_commands
                        .record_snippet_usage(request)
                        .await
                        .map(|_| ())
                })
                .await;
            }
        })
        .detach();
        cx.notify();
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

    /// 右栏 Run 按 main 的风险、敏感值、耗时和多行规则确认，并冻结当前正文及目标快照。
    ///
    /// @author kongweiguang
    pub(super) fn request_snippet_run_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_terminal_fill_blocked()
            || self.pending_snippet_run.is_some()
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
        let Some(mut rendered) = render_selected_snippet_direct(self, cx) else {
            self.status = Some(command_message("请先选择片段", "Select a snippet first"));
            cx.notify();
            return;
        };
        if rendered.command.trim().is_empty() {
            if rendered.sensitive || self.selected_snippet_sensitive {
                rendered.command.zeroize();
            }
            self.status = Some(command_message(
                "命令片段内容为空",
                "Snippet command is empty",
            ));
            cx.notify();
            return;
        }

        let key = selected_preference_key(self);
        let title = self.snippet_title.read(cx).value().to_string();
        let target_label = self
            .session_target_label()
            .map(str::to_owned)
            .or_else(|| self.session_info.as_ref().map(super::session_title))
            .unwrap_or_else(|| command_message("当前终端", "Current terminal"));
        let has_legacy_raw = self.snippet_variables.iter().any(|field| {
            field.variable.render_strategy == SnippetVariableRenderStrategy::ValidatedRaw
        });
        let non_instant = selected_snippet_duration_requires_confirmation(self);
        let multiline = rendered.command.contains(['\r', '\n']);
        let (requires_confirmation, requires_strong_confirmation) = snippet_run_confirmation_policy(
            self.snippet_risk,
            rendered.sensitive || self.selected_snippet_sensitive,
            has_legacy_raw,
            non_instant,
            multiline,
        );
        if !requires_confirmation {
            self.start_snippet_run(rendered, key, binding, service, cx);
            return;
        }

        self.pending_snippet_run = Some(PendingSnippetRun {
            command: rendered.command,
            sensitive: rendered.sensitive || self.selected_snippet_sensitive,
            key,
            title: if title.trim().is_empty() {
                command_message("未命名片段", "Untitled snippet")
            } else {
                title
            },
            target_label: target_label.clone(),
            requires_strong_confirmation,
            binding,
            snippet_generation: self.snippet_generation,
            session_view_generation: self.session_view_generation,
            snippet_editor_generation: self.snippet_editor_generation,
        });
        let pending = self
            .pending_snippet_run
            .as_ref()
            .expect("pending run was set");
        let preview = if pending.sensitive {
            command_message("[敏感命令已隐藏]", "[Sensitive command hidden]")
        } else {
            pending.command.clone()
        };
        let parent = cx.entity().clone();
        let dialog_content = cx.new(|cx| {
            SnippetRunConfirmationView::new(
                parent.clone(),
                pending.title.clone(),
                pending.target_label.clone(),
                preview,
                pending.requires_strong_confirmation,
                window,
                cx,
            )
        });
        let dialog_theme = ThemeTokens::current(window, cx).for_tool_panel();
        window.open_dialog(cx, move |dialog, _, _| {
            let parent_for_close = parent.clone();
            dialog
                .backdrop_blur(dialog_theme.modal_blur_radius)
                .surface_backdrop_filter(
                    dialog_theme.floating_blur_radius,
                    dialog_theme.floating_saturation,
                )
                .bg(dialog_theme.floating_surface)
                .rounded(dialog_theme.radius_dialog)
                .title(crate::locale::text("确认运行命令", "Confirm command run"))
                .width(px(480.))
                .max_w(px(480.))
                .keyboard(true)
                .overlay_closable(true)
                .close_button(true)
                .on_close(move |_, _, app| {
                    parent_for_close.update(app, |view, cx| {
                        view.cancel_snippet_run_confirmation(cx);
                    });
                })
                .child(dialog_content.clone())
        });
        cx.notify();
    }

    /// Dialog 关闭时清除冻结计划；其 Drop 会擦除保留的命令正文。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_snippet_run_confirmation(&mut self, cx: &mut Context<Self>) {
        if self.pending_snippet_run.take().is_some() {
            cx.notify();
        }
    }

    /// 确认只提交仍匹配当前 binding 与片段代次的冻结计划；过期目标关闭并拒绝发送。
    ///
    /// @author kongweiguang
    fn confirm_snippet_run(&mut self, entered_target: &str, cx: &mut Context<Self>) -> bool {
        let Some(pending) = self.pending_snippet_run.take() else {
            return true;
        };
        if pending.requires_strong_confirmation && entered_target != pending.target_label {
            self.pending_snippet_run = Some(pending);
            self.status = Some(command_message(
                "输入的目标名称不匹配",
                "The entered target name does not match",
            ));
            cx.notify();
            return false;
        }
        if !pending.matches_current(self) {
            self.status = Some(command_message(
                "终端或片段已变化，请重新核对后运行",
                "The terminal or snippet changed; review it before running",
            ));
            cx.notify();
            return true;
        }
        let Some(service) = self.snippet_execution.clone() else {
            self.status = Some(command_message(
                "片段运行服务当前不可用",
                "Snippet execution is unavailable",
            ));
            cx.notify();
            return true;
        };
        let rendered = SnippetRenderResult {
            command: pending.command.clone(),
            sensitive: pending.sensitive,
        };
        self.start_snippet_run(
            rendered,
            pending.key.clone(),
            pending.binding.clone(),
            service,
            cx,
        );
        true
    }

    /// 运行在提交边界按 main 去除首尾空白，再按实际成功回执记用量；不改变共享直接发送用例的广播正文契约。
    ///
    /// @author kongweiguang
    fn start_snippet_run(
        &mut self,
        mut rendered: SnippetRenderResult,
        key: Option<SnippetPreferenceKey>,
        binding: UiSessionBinding,
        service: Arc<kerminal_application::commands::SnippetExecutionService>,
        cx: &mut Context<Self>,
    ) {
        if self.snippet_terminal_fill_blocked()
            || self.ui_session_binding.as_ref() != Some(&binding)
            || self.session_id.as_ref() != Some(&binding.session_id)
        {
            self.status = Some(command_message(
                "终端绑定已变化，请重新选择目标",
                "The terminal binding changed; select the target again",
            ));
            cx.notify();
            return;
        }
        let normalized_command = rendered.command.trim().to_owned();
        rendered.command.zeroize();
        rendered.command = normalized_command;
        let sensitive = rendered.sensitive;
        let record_history = self.record_command && !sensitive;
        self.snippet_fill_generation = self.snippet_fill_generation.wrapping_add(1).max(1);
        let request_generation = self.snippet_fill_generation;
        let session_view_generation = self.session_view_generation;
        let snippet_generation = self.snippet_generation;
        let editor_generation = self.snippet_editor_generation;
        let cancellation = CancellationToken::new();
        self.snippet_terminal_fill.cancellation = Some(cancellation.clone());
        self.status = Some(command_message("正在运行…", "Running…"));
        let executor = self.executor.clone();
        let usage_commands = self.commands.clone();
        let send_binding = binding.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, move |operation_cancellation| async move {
                    service
                        .send_direct(
                            rendered,
                            send_binding,
                            record_history,
                            operation_cancellation,
                        )
                        .await
                })
                .await;
            let record_usage = result.is_ok();
            let _ = this.update(cx, |view, cx| {
                view.snippet_terminal_fill.cancellation = None;
                let context_matches = view.snippet_fill_generation == request_generation
                    && view.session_view_generation == session_view_generation
                    && view.snippet_generation == snippet_generation
                    && view.snippet_editor_generation == editor_generation
                    && view.ui_session_binding.as_ref() == Some(&binding);
                match result {
                    Ok(SnippetRunHistory::Failed { .. }) if context_matches => {
                        view.status = Some(command_message(
                            "命令已提交，历史记录失败",
                            "Command submitted; history could not be saved",
                        ));
                    }
                    Ok(_) if context_matches => {
                        view.status = Some(command_message("命令已提交", "Command submitted"));
                    }
                    Ok(_) => {}
                    Err(error) if error.public_code() == PublicErrorCode::OutcomeUnknown => {
                        view.snippet_terminal_fill.unknown_target = Some(binding.clone());
                        view.status = Some(command_message(
                            "运行结果未知，请检查原终端",
                            "Run result is unknown; check the original terminal",
                        ));
                    }
                    Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                        if context_matches {
                            view.status = Some(command_message("运行已取消", "Run cancelled"));
                        }
                    }
                    Err(error) if context_matches => {
                        view.status = Some(error.public_message().to_owned());
                    }
                    Err(_) => {}
                }
                cx.notify();
            });
            if record_usage && let Some(key) = key {
                let request = SnippetUsageRecordRequest {
                    receipt_id: Uuid::new_v4().to_string(),
                    key,
                    action: SnippetUsageAction::Run,
                };
                record_snippet_usage_best_effort(executor, move || async move {
                    usage_commands
                        .record_snippet_usage(request)
                        .await
                        .map(|_| ())
                })
                .await;
            }
        })
        .detach();
        cx.notify();
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

    /// 取消尚未开始的终端操作；未知状态的核对按钮只解除重复提交门禁。
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
                        crate::locale::text("取消操作", "Cancel action")
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
                    "终端操作结果未知，请检查原终端",
                    "Terminal action result is unknown; check the original terminal",
                ))
                .child(
                    Button::new("snippet-fill-reviewed")
                        .ghost()
                        .label(crate::locale::text(
                            "已检查原终端，允许再次操作",
                            "I checked the original terminal; allow another action",
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

/// 按选中定义的来源与稳定 ID 构造使用偏好键，不从显示标题推断身份。
///
/// @author kongweiguang
fn preference_key(snippet_id: &str, user_defined: bool) -> SnippetPreferenceKey {
    SnippetPreferenceKey {
        origin: if user_defined {
            SnippetCatalogOrigin::User
        } else {
            SnippetCatalogOrigin::Builtin
        },
        snippet_id: snippet_id.to_owned(),
    }
}

/// 新建草稿没有可记录的目录键；已选定义按 revision 判断用户或内置来源。
///
/// @author kongweiguang
fn selected_preference_key(view: &CommandsView) -> Option<SnippetPreferenceKey> {
    view.selected_snippet
        .as_ref()
        .map(|selected| preference_key(&selected.definition.id, selected.revision.is_some()))
}

/// 缺少目录时长信息按需确认处理，避免未知运行时长静默跳过 main 的确认条件。
///
/// @author kongweiguang
fn selected_snippet_duration_requires_confirmation(view: &CommandsView) -> bool {
    let Some(key) = selected_preference_key(view) else {
        return true;
    };
    view.snippet_page_items
        .iter()
        .find(|item| item.id == key.snippet_id && item.origin == key.origin)
        .is_none_or(|item| item.duration != "instant")
}

/// main 仅让只读、非敏感、即时、单行且非 raw 模板免确认；破坏性风险另需键入目标名。
///
/// @author kongweiguang
fn snippet_run_confirmation_policy(
    risk: SnippetRisk,
    sensitive: bool,
    has_legacy_raw: bool,
    non_instant: bool,
    multiline: bool,
) -> (bool, bool) {
    let effective_risk = if has_legacy_raw && risk == SnippetRisk::Inspect {
        SnippetRisk::Change
    } else {
        risk
    };
    (
        sensitive || effective_risk != SnippetRisk::Inspect || non_instant || multiline,
        effective_risk == SnippetRisk::Destructive,
    )
}
