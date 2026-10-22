//! 文件编辑器的异步读取、revision 保存及应用服务协调。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, Window,
    component::input::{EditorState, InputEvent},
};
use kerminal_application::{AppError, PublicErrorCode};
use kerminal_domain::files::FileEndpoint;
use kerminal_domain::files::editor::FileEditorSession;
use kerminal_domain::files::editor::SaveResolution;
use tokio_util::sync::CancellationToken;

use super::language::language_for_file_name;
use super::workspace::{EditorStatus, FileEditorWorkspace, PendingEditorSave, transition_message};
use super::{
    EditorSaveFailureAction, EditorSaveSuccessAction, editor_save_failure_action,
    editor_save_success_action,
};

/// 保存失败后的 UI 决策与读取关闭意图保持同一回执边界。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SaveFailureDecision {
    action: EditorSaveFailureAction,
    arm_close_after_readback: bool,
    cancel_followup_intents: bool,
}

/// 保存成功但用户已请求取消时保留标签，避免迟到的成功回执触发关闭。
fn save_success_action(
    cancel_requested: bool,
    close_after_success: bool,
    quit_after_success: bool,
    quit_flow: bool,
    dirty_after_success: bool,
) -> EditorSaveSuccessAction {
    if cancel_requested {
        EditorSaveSuccessAction::KeepOpen
    } else {
        editor_save_success_action(
            close_after_success,
            quit_after_success,
            quit_flow,
            dirty_after_success,
        )
    }
}

/// 取消请求优先撤销关闭后续动作，未知提交只有未取消的显式关闭才绑定读回。
fn save_failure_decision(
    cancel_requested: bool,
    conflict: bool,
    outcome_unknown: bool,
    close_after_success: bool,
    quit_after_success: bool,
    quit_flow: bool,
) -> SaveFailureDecision {
    SaveFailureDecision {
        action: if cancel_requested {
            EditorSaveFailureAction::PreserveWorkspace
        } else {
            editor_save_failure_action(conflict, close_after_success, quit_after_success, quit_flow)
        },
        arm_close_after_readback: outcome_unknown && close_after_success && !cancel_requested,
        cancel_followup_intents: cancel_requested,
    }
}

impl FileEditorWorkspace {
    /// 重试首读或执行用户确认的读回；加载及有效只读状态发布给 Files，关闭意图仍只绑定该 generation。
    ///
    /// @author kongweiguang
    pub(super) fn start_load(
        &mut self,
        endpoint: FileEndpoint,
        bind_close_after_readback: bool,
        cx: &mut Context<Self>,
    ) {
        let generation = self.reserve_load_generation();
        if self.active_endpoint.as_ref() == Some(&endpoint) {
            self.clear_search_projection(cx);
        }
        let mut close_intent_bound = false;
        if let Some(tab) = self.tabs.iter_mut().find(|tab| tab.endpoint == endpoint) {
            tab.loading = true;
            tab.load_generation = generation;
            tab.error = None;
            if bind_close_after_readback {
                close_intent_bound = tab.close_after_readback.bind_to_load(generation);
            }
            if let Some(editor) = tab.editor.clone() {
                editor.update(cx, |state, cx| state.set_readonly(true, cx));
            }
        } else {
            return;
        }
        if close_intent_bound
            && let Some(pending) = self
                .pending_workspace_close
                .as_mut()
                .filter(|pending| pending.endpoint == endpoint)
        {
            pending.load_generation = Some(generation);
        }
        self.spawn_load(endpoint, generation, cx);
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
    }

    /// 为每次加载分配单调变化的本地令牌，迟到结果不能覆盖更新后的文本缓冲。
    pub(super) fn reserve_load_generation(&mut self) -> u64 {
        let generation = self.next_load_generation;
        self.next_load_generation = self.next_load_generation.wrapping_add(1).max(1);
        generation
    }

    /// 网络和磁盘读取交给 application runtime，结果回到创建它的窗口；关闭 tab 或窗口后安全忽略迟到结果。
    ///
    /// @author kongweiguang
    pub(super) fn spawn_load(
        &mut self,
        endpoint: FileEndpoint,
        generation: u64,
        cx: &mut Context<Self>,
    ) {
        let service = self.service.clone();
        let executor = self.executor.clone();
        let window_handle = self.window_handle;
        let task_endpoint = endpoint.clone();
        let request_endpoint = endpoint.clone();
        let task = cx.spawn(async move |this, cx| {
            let result =
                executor
                    .run(|cancellation| async move {
                        service.open(request_endpoint, cancellation).await
                    })
                    .await;
            let _ = cx.update_window(window_handle, |_, window, app| {
                let _ = this.update(app, |workspace, cx| {
                    workspace.finish_load(task_endpoint, generation, result, window, cx);
                });
            });
        });
        if let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.endpoint == endpoint && tab.load_generation == generation)
        {
            tab.load_task = Some(task);
        }
    }

    /// 仅接收当前 generation；失败读回清关闭意图并为原 Root close owner 回报 false。
    ///
    /// @author kongweiguang
    fn finish_load(
        &mut self,
        endpoint: FileEndpoint,
        generation: u64,
        result: Result<FileEditorSession, AppError>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.endpoint == endpoint && tab.load_generation == generation)
        else {
            return;
        };

        let close_after_readback = match result {
            Ok(session) => {
                let initial_content = session.saved_content().to_owned();
                let readonly = super::workspace::effective_readonly(
                    session.is_readonly(),
                    self.tabs[index].workspace_readonly,
                );
                let editor = match self.tabs[index].editor.clone() {
                    Some(editor) => {
                        let language = language_for_file_name(&self.tabs[index].title);
                        let mut decorations = self.tabs[index].search_decorations.clone();
                        editor.update(cx, |state, cx| {
                            state.set_value(initial_content, window, cx);
                            state.set_readonly(readonly, cx);
                            if let Some(language) = language {
                                state.set_highlighter(language, cx);
                            }
                            if decorations.is_none() {
                                decorations =
                                    Some(state.create_decorations_collection(Vec::new(), cx));
                            }
                        });
                        self.tabs[index].search_decorations = decorations;
                        editor
                    }
                    None => {
                        let language =
                            language_for_file_name(&self.tabs[index].title).map(str::to_owned);
                        let mut decorations = None;
                        let editor = cx.new(|cx| {
                            let mut state = EditorState::new(window, cx)
                                .default_value(initial_content)
                                .searchable(false)
                                // 框架原生菜单走平台 NativeMenu；右键菜单由外层 GPUI Kit PopupMenu 统一绘制主题。
                                .context_menu(false);
                            if let Some(language) = language {
                                state = state.language(language);
                            }
                            decorations = Some(state.create_decorations_collection(Vec::new(), cx));
                            state
                        });
                        editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
                        let subscribed_endpoint = endpoint.clone();
                        let subscription =
                            cx.subscribe(&editor, move |workspace, state, event, cx| {
                                if matches!(event, InputEvent::Change) {
                                    let content = state.read(cx).text().to_string();
                                    workspace.update_dirty(&subscribed_endpoint, content, cx);
                                    workspace.refresh_search_document(&subscribed_endpoint, cx);
                                    cx.notify();
                                }
                            });
                        self.tabs[index].editor_subscription = Some(subscription);
                        self.tabs[index].search_decorations = decorations;
                        editor
                    }
                };

                let tab = &mut self.tabs[index];
                tab.session = Some(session);
                tab.editor = Some(editor);
                tab.loading = false;
                tab.document_generation = tab.document_generation.wrapping_add(1).max(1);
                tab.dirty = false;
                tab.error = None;
                let close_after_readback = tab.close_after_readback.finish_load(generation, true);
                self.status = None;
                let query = self.current_search_query(cx);
                self.sync_search_query(query, cx);
                close_after_readback
            }
            Err(error) => {
                let message = public_error_message(&error);
                let tab = &mut self.tabs[index];
                tab.loading = false;
                tab.error = Some(message.clone());
                tab.close_after_readback.finish_load(generation, false);
                if let (Some(editor), Some(session)) = (&tab.editor, &tab.session) {
                    let readonly = super::workspace::effective_readonly(
                        session.is_readonly(),
                        tab.workspace_readonly,
                    );
                    editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
                }
                self.status = Some(EditorStatus::Message(message));
                if self.active_endpoint.as_ref() == Some(&endpoint) && self.search_visible() {
                    let query = self.current_search_query(cx);
                    self.sync_search_query(query, cx);
                }
                false
            }
        };

        if close_after_readback {
            self.remove_tab(&endpoint, cx);
        } else {
            self.resolve_workspace_close(&endpoint, Some(generation), false, cx);
        }

        self.publish_workspace_projection_if_changed(cx);

        if self.quit_flow {
            self.advance_quit(cx);
        }
        cx.notify();
    }

    /// 编辑器变化仅重算对应 tab 的 dirty 投影，且只在布尔脏状态改变时广播，不复制正文。
    ///
    /// @author kongweiguang
    pub(super) fn update_dirty(
        &mut self,
        endpoint: &FileEndpoint,
        content: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.endpoint == endpoint) {
            tab.dirty = tab
                .session
                .as_ref()
                .is_some_and(|session| session.is_dirty(&content));
        }
        self.publish_workspace_projection_if_changed(cx);
    }

    /// 保存当前文件时只提交编辑组件的快照，领域聚合负责 revision 与 dirty 基线。
    pub(super) fn save_active(
        &mut self,
        resolution: SaveResolution,
        close_after_success: bool,
        quit_after_success: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(endpoint) = self.active_endpoint.clone() else {
            return;
        };
        self.save_endpoint(
            endpoint,
            resolution,
            close_after_success,
            quit_after_success,
            cx,
        );
    }

    /// 保存快照沿用 revision CAS；取消只触发业务 token，结果 owner 等待实际文件收据。
    ///
    /// @author kongweiguang
    pub(super) fn save_endpoint(
        &mut self,
        endpoint: FileEndpoint,
        resolution: SaveResolution,
        close_after_success: bool,
        quit_after_success: bool,
        cx: &mut Context<Self>,
    ) {
        let save_generation = self.reserve_save_generation();
        let Some(tab) = self
            .tabs
            .iter_mut()
            .find(|tab| tab.endpoint == endpoint && !tab.loading)
        else {
            return;
        };
        let Some(editor) = tab.editor.as_ref() else {
            return;
        };
        let content = editor.update(cx, |state, _| state.text().to_string());
        let Some(session) = tab.session.as_mut() else {
            return;
        };
        let command = match session.begin_save(&content, resolution) {
            Ok(command) => command,
            Err(error) => {
                self.status = Some(EditorStatus::Message(transition_message(error)));
                cx.notify();
                return;
            }
        };
        tab.error = None;
        let cancellation = CancellationToken::new();
        tab.save_operation = Some(PendingEditorSave::new(
            endpoint.clone(),
            save_generation,
            cancellation.clone(),
        ));
        self.status = Some(EditorStatus::Message(
            crate::locale::text("正在保存…", "Saving…").to_string(),
        ));

        let service = self.service.clone();
        let executor = self.executor.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(cancellation, |operation_cancellation| async move {
                    service.save(command, operation_cancellation).await
                })
                .await;
            let _ = this.update(cx, |workspace, cx| {
                workspace.finish_save(
                    endpoint,
                    save_generation,
                    result,
                    close_after_success,
                    quit_after_success,
                    cx,
                );
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消只传播到当前文件的业务 token；等待真实写入收据期间继续保持 Saving 状态。
    ///
    /// @author kongweiguang
    pub(super) fn request_save_cancellation(
        &mut self,
        endpoint: &FileEndpoint,
        cx: &mut Context<Self>,
    ) {
        let requested = {
            let Some(tab) = self.tabs.iter_mut().find(|tab| &tab.endpoint == endpoint) else {
                return;
            };
            if !tab.session.as_ref().is_some_and(|session| {
                session.phase() == kerminal_domain::files::editor::EditorPhase::Saving
            }) {
                return;
            }
            tab.save_operation
                .as_mut()
                .is_some_and(PendingEditorSave::request_cancel)
        };
        if requested {
            self.status = Some(EditorStatus::Message(
                crate::locale::text(
                    "已请求取消保存；正在等待文件操作返回真实结果。",
                    "Save cancellation was requested; waiting for the file operation's actual result.",
                )
                .to_string(),
            ));
            cx.notify();
        }
    }

    /// 只消费相同文件和保存代次的实际回执；取消后的结果也必须经过统一后续动作决策。
    ///
    /// @author kongweiguang
    fn finish_save(
        &mut self,
        endpoint: FileEndpoint,
        save_generation: u64,
        result: Result<kerminal_domain::files::WriteTextResult, AppError>,
        close_after_success: bool,
        quit_after_success: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(index) = self.tabs.iter().position(|tab| tab.endpoint == endpoint) else {
            return;
        };
        if !self.tabs[index]
            .save_operation
            .as_ref()
            .is_some_and(|operation| operation.matches(&endpoint, save_generation))
        {
            return;
        }
        let cancel_requested = self.tabs[index]
            .save_operation
            .as_ref()
            .is_some_and(PendingEditorSave::cancel_requested);
        let load_generation = self.tabs[index].load_generation;
        self.tabs[index].save_operation = None;

        match result {
            Ok(write_result) => {
                let dirty_after_success = {
                    let tab = &mut self.tabs[index];
                    let Some(session) = tab.session.as_mut() else {
                        return;
                    };
                    if session.save_succeeded(write_result).is_err() {
                        tab.error = Some(
                            crate::locale::text(
                                "保存状态异常，请重新加载后继续",
                                "The save state is inconsistent. Reload the file before continuing.",
                            )
                            .to_string(),
                        );
                        tab.dirty = true;
                        self.status = tab.error.clone().map(EditorStatus::Message);
                        let failure = save_failure_decision(
                            cancel_requested,
                            false,
                            false,
                            close_after_success,
                            quit_after_success,
                            self.quit_flow,
                        );
                        if failure.cancel_followup_intents {
                            self.cancel_save_followup_intents(
                                &endpoint,
                                load_generation,
                                quit_after_success,
                                cx,
                            );
                        } else {
                            if failure.action == EditorSaveFailureAction::CancelQuitIntent {
                                self.quit_flow = false;
                                self.confirmation = None;
                                cx.emit(super::workspace::FileEditorWorkspaceEvent::QuitCancelled);
                            }
                            self.resolve_workspace_close(
                                &endpoint,
                                Some(load_generation),
                                false,
                                cx,
                            );
                        }
                        self.publish_workspace_projection_if_changed(cx);
                        cx.notify();
                        return;
                    }
                    let content = tab
                        .editor
                        .as_ref()
                        .map(|editor| editor.update(cx, |state, _| state.text().to_string()))
                        .unwrap_or_default();
                    tab.dirty = session.is_dirty(&content);
                    tab.error = None;
                    tab.dirty
                };
                self.status = Some(if cancel_requested {
                    EditorStatus::Message(crate::locale::text(
                        "保存已完成；取消请求未能撤回已完成的写入，文件保持打开。",
                        "The save completed before cancellation could take effect; the file remains open.",
                    )
                    .to_string())
                } else {
                    EditorStatus::SaveCompleted
                });
                if cancel_requested {
                    self.cancel_save_followup_intents(
                        &endpoint,
                        load_generation,
                        quit_after_success,
                        cx,
                    );
                }
                match save_success_action(
                    cancel_requested,
                    close_after_success,
                    quit_after_success,
                    self.quit_flow,
                    dirty_after_success,
                ) {
                    EditorSaveSuccessAction::KeepOpen => {}
                    EditorSaveSuccessAction::ConfirmClose => {
                        self.confirmation = Some(super::workspace::EditorConfirmation::Close(
                            endpoint.clone(),
                        ));
                        self.status = Some(EditorStatus::Message(
                            crate::locale::text(
                                "保存的版本已写入；保存期间的新修改仍未保存。",
                                "The saved version was written. Edits made while saving are still unsaved.",
                            )
                            .to_string(),
                        ));
                    }
                    EditorSaveSuccessAction::ConfirmQuit => {
                        self.quit_flow = true;
                        self.advance_quit(cx);
                    }
                    EditorSaveSuccessAction::Close => self.remove_tab(&endpoint, cx),
                    EditorSaveSuccessAction::CloseAndQuit => {
                        self.remove_tab(&endpoint, cx);
                        self.advance_quit(cx);
                    }
                    EditorSaveSuccessAction::ContinueQuit => self.advance_quit(cx),
                }
            }
            Err(error) => {
                let conflict = error.public_code() == PublicErrorCode::Conflict;
                let outcome_unknown = error.public_code() == PublicErrorCode::OutcomeUnknown;
                let failure = save_failure_decision(
                    cancel_requested,
                    conflict,
                    outcome_unknown,
                    close_after_success,
                    quit_after_success,
                    self.quit_flow,
                );
                let failure_action = failure.action;
                let message =
                    if cancel_requested && error.public_code() == PublicErrorCode::Cancelled {
                        crate::locale::text(
                            "保存已安全取消；编辑内容仍未保存。",
                            "The save was safely cancelled. Your editor contents remain unsaved.",
                        )
                        .to_string()
                    } else {
                        public_error_message(&error)
                    };
                let tab = &mut self.tabs[index];
                if let Some(session) = tab.session.as_mut() {
                    if outcome_unknown {
                        let _ = session.save_outcome_unknown();
                    } else {
                        let _ = session.save_failed(conflict);
                    }
                    let content = tab
                        .editor
                        .as_ref()
                        .map(|editor| editor.update(cx, |state, _| state.text().to_string()))
                        .unwrap_or_default();
                    tab.dirty = session.is_dirty(&content);
                }
                if failure.arm_close_after_readback {
                    tab.close_after_readback.arm();
                }
                tab.error = Some(message);
                self.status = tab.error.clone().map(EditorStatus::Message);
                match failure_action {
                    EditorSaveFailureAction::ConfirmOverwrite => {
                        self.confirmation = Some(super::workspace::EditorConfirmation::Overwrite {
                            endpoint: endpoint.clone(),
                            close_after_success,
                            quit_after_success: quit_after_success || self.quit_flow,
                        });
                    }
                    EditorSaveFailureAction::CancelQuitIntent => {
                        self.confirmation = None;
                        self.quit_flow = false;
                        cx.emit(super::workspace::FileEditorWorkspaceEvent::QuitCancelled);
                    }
                    EditorSaveFailureAction::PreserveWorkspace => {}
                }
                if failure.cancel_followup_intents {
                    self.cancel_save_followup_intents(
                        &endpoint,
                        load_generation,
                        quit_after_success,
                        cx,
                    );
                } else if failure_action == EditorSaveFailureAction::PreserveWorkspace
                    && !outcome_unknown
                {
                    self.resolve_workspace_close(&endpoint, Some(load_generation), false, cx);
                }
            }
        }
        self.publish_workspace_projection_if_changed(cx);
        cx.notify();
    }

    /// 保存取消已收到真实终态后撤销同一关闭/退出后续动作，不能自动关闭刚保存的文件。
    fn cancel_save_followup_intents(
        &mut self,
        endpoint: &FileEndpoint,
        load_generation: u64,
        quit_after_success: bool,
        cx: &mut Context<Self>,
    ) {
        if quit_after_success || self.quit_flow {
            self.quit_flow = false;
            self.confirmation = None;
            cx.emit(super::workspace::FileEditorWorkspaceEvent::QuitCancelled);
        }
        self.resolve_workspace_close(endpoint, Some(load_generation), false, cx);
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/editor_save_receipt.rs"]
mod save_receipt_tests;

/// 将稳定错误类别映射为双语 UI 文案，同时沿用应用层已脱敏的中文摘要。
fn public_error_message(error: &AppError) -> String {
    let english = match error.public_code() {
        PublicErrorCode::Invalid => "The file request is invalid.",
        PublicErrorCode::NotFound => "The file is unavailable or no longer exists.",
        PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
            "This file operation is not supported."
        }
        PublicErrorCode::Conflict => {
            "The file changed. Reload it or explicitly confirm an overwrite."
        }
        PublicErrorCode::Cancelled => "The file operation was cancelled.",
        PublicErrorCode::Infrastructure => {
            "The file operation failed. Your editor contents were kept; you can retry."
        }
        PublicErrorCode::OutcomeUnknown => {
            "The save result is unknown. Reload the file before continuing; the save will not be retried automatically."
        }
        PublicErrorCode::AuthenticationFailed => "File access authentication failed.",
        PublicErrorCode::HostKeyUnknown => "The host key has not been trusted yet.",
        PublicErrorCode::HostKeyChanged => "The host key changed. The connection was blocked.",
        PublicErrorCode::HostKeyTrustMismatch => {
            "The host key confirmation did not match. Trust was not saved."
        }
        PublicErrorCode::Timeout => "The file operation timed out.",
        PublicErrorCode::RemoteExit => "The remote file operation exited unexpectedly.",
        PublicErrorCode::ConfigurationCorrupt => {
            "Configuration data is damaged and needs recovery."
        }
    };
    crate::locale::text(error.public_message(), english).to_string()
}
