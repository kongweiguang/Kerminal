//! 偏好页离开与草稿丢弃确认。
//!
//! @author kongweiguang

use super::{PreferencesEvent, PreferencesPanel};
use crate::features::preferences::{CloseRequest, ConfigDraft, DraftStatus};
use gpui_kit::{Context, Window};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CloseGateAction {
    Wait,
    ConfirmDiscard,
    Proceed,
}

/// 把 settings/Profile 草稿状态投影成唯一离开决策，普通关闭及受控工作区导航共用同一门禁。
///
/// @author kongweiguang
fn close_gate_action(
    settings: CloseRequest,
    profile: CloseRequest,
    staged_inputs: bool,
) -> CloseGateAction {
    if settings == CloseRequest::WaitForSave || profile == CloseRequest::WaitForSave {
        CloseGateAction::Wait
    } else if settings == CloseRequest::ConfirmDiscard
        || profile == CloseRequest::ConfirmDiscard
        || staged_inputs
    {
        CloseGateAction::ConfirmDiscard
    } else {
        CloseGateAction::Proceed
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum QuitDeferral {
    ProfileOperation,
    SyncOperation { cancellation_requested: bool },
    VaultOperation,
    Saving,
    OutcomeUnknown,
}

/// 将仍有副作用的事务和配置写入状态映射为明确的退出延后原因。
fn quit_deferral(
    profile_operation_running: bool,
    sync_busy: bool,
    sync_cancel_requested: bool,
    vault_busy: bool,
    settings_status: Option<DraftStatus>,
    profile_status: Option<DraftStatus>,
) -> Option<QuitDeferral> {
    if profile_operation_running {
        return Some(QuitDeferral::ProfileOperation);
    }
    if sync_busy {
        return Some(QuitDeferral::SyncOperation {
            cancellation_requested: sync_cancel_requested,
        });
    }
    if vault_busy {
        return Some(QuitDeferral::VaultOperation);
    }
    if [settings_status, profile_status].contains(&Some(DraftStatus::OutcomeUnknown)) {
        return Some(QuitDeferral::OutcomeUnknown);
    }
    if [settings_status, profile_status].contains(&Some(DraftStatus::Saving)) {
        return Some(QuitDeferral::Saving);
    }
    None
}

impl PreferencesPanel {
    /// 用户请求离开时同时检查 settings/Profile dirty 状态，不让根视图切换造成静默丢稿。
    ///
    /// @author kongweiguang
    pub fn request_close(&mut self, cx: &mut Context<Self>) {
        let settings_close = self
            .settings
            .as_mut()
            .map(ConfigDraft::request_close)
            .unwrap_or(CloseRequest::CloseNow);
        let profile_close = self
            .profile_draft
            .as_mut()
            .map(ConfigDraft::request_close)
            .unwrap_or(CloseRequest::CloseNow);
        let staged_inputs = self.has_unadded_inputs();
        match close_gate_action(settings_close, profile_close, staged_inputs) {
            CloseGateAction::Wait => {
                // 等待结果期间不保留导航意图，避免保存/未知结果读回后意外切页。
                self.pending_navigation = None;
                let outcome_unknown = self
                    .settings
                    .as_ref()
                    .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown)
                    || self
                        .profile_draft
                        .as_ref()
                        .is_some_and(|draft| draft.status() == DraftStatus::OutcomeUnknown);
                self.status = Some(if outcome_unknown {
                    crate::locale::text(
                        "写入结果尚未确认，请重新读取配置后再离开。",
                        "The write result is not confirmed. Read the configuration back before leaving.",
                    )
                    .to_string()
                } else {
                    crate::locale::text(
                        "保存完成后才能离开设置。",
                        "Wait for the save to finish before leaving Preferences.",
                    )
                    .to_string()
                });
            }
            CloseGateAction::ConfirmDiscard => self.close_confirmation = true,
            CloseGateAction::Proceed => self.emit_leave_event(cx),
        }
        cx.notify();
    }

    /// MCP 搜索入口先记录受控目标，再经过与关闭按钮相同的 dirty/save/unknown 检查。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn request_mcp_runtime_navigation(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if !self.mcp_runtime_available {
            self.status = Some(
                crate::locale::text(
                    "MCP 运行时尚未由应用装配。",
                    "The MCP runtime is not available in this build.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        }
        if self.close_confirmation {
            return;
        }
        self.pending_navigation = Some(super::PreferencesNavigationDestination::McpRuntime);
        self.request_close(cx);
    }

    /// 关于与更新只负责切换到已有工作区；沿用 close gate，避免导航绕过未保存草稿。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn request_updates_navigation(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.close_confirmation {
            return;
        }
        self.pending_navigation = Some(super::PreferencesNavigationDestination::Updates);
        self.request_close(cx);
    }

    /// 离开确认通过后才发受控导航事件，Root 切换现有工作区而不绕过设置草稿门禁。
    ///
    /// @author kongweiguang
    fn emit_leave_event(&mut self, cx: &mut Context<Self>) {
        match self.pending_navigation.take() {
            Some(super::PreferencesNavigationDestination::McpRuntime) => {
                cx.emit(PreferencesEvent::OpenMcpRuntimeRequested);
            }
            Some(super::PreferencesNavigationDestination::Updates) => {
                cx.emit(PreferencesEvent::OpenUpdatesRequested);
            }
            None => cx.emit(PreferencesEvent::CloseRequested),
        }
    }

    /// 继续编辑只关闭确认条并重置子草稿的 close marker，不清除用户输入。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn cancel_close(&mut self, cx: &mut Context<Self>) {
        self.close_confirmation = false;
        self.pending_navigation = None;
        if let Some(draft) = self.settings.as_mut()
            && draft.close_confirmation_open()
        {
            let _ = draft.cancel_close();
        }
        if let Some(draft) = self.profile_draft.as_mut()
            && draft.close_confirmation_open()
        {
            let _ = draft.cancel_close();
        }
        cx.notify();
    }

    /// 只有用户确认后才放弃两个页面草稿并通知 Root 恢复打开设置前的页面。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn discard_and_close(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.close_confirmation = false;
        if let Some(draft) = self.settings.as_mut() {
            if draft.close_confirmation_open() {
                let _ = draft.discard_and_close();
            } else {
                let _ = draft.discard_changes();
            }
        }
        if let Some(draft) = self.profile_draft.as_mut() {
            if draft.close_confirmation_open() {
                let _ = draft.discard_and_close();
            } else {
                let _ = draft.discard_changes();
            }
        }
        self.restore_form_inputs(window, cx);
        self.emit_leave_event(cx);
    }

    /// RootView 调用此守卫后只有无草稿返回 true；需丢稿时先由本面板显示确认。
    ///
    /// @author kongweiguang
    pub fn request_quit(&mut self, cx: &mut Context<Self>) -> bool {
        let settings_status = self.settings.as_ref().map(ConfigDraft::status);
        let profile_status = self.profile_draft.as_ref().map(ConfigDraft::status);
        let cancellation_requested = self
            .sync_cancellation
            .as_ref()
            .is_some_and(super::sync::SyncCancellationHandle::cancel_requested);
        if let Some(reason) = quit_deferral(
            self.profile_operation_running,
            self.sync_busy.is_some(),
            cancellation_requested,
            self.vault_busy,
            settings_status,
            profile_status,
        ) {
            let message = match reason {
                QuitDeferral::ProfileOperation => crate::locale::text(
                    "Profile 操作正在写入；等待结果后再退出。",
                    "A Profile change is still being written. Wait for its result before quitting.",
                ),
                QuitDeferral::SyncOperation {
                    cancellation_requested: true,
                } => crate::locale::text(
                    "已请求取消同步，正在等待 Git 清理回执；完成后请再次退出。",
                    "Sync cancellation was requested. Wait for Git cleanup, then try quitting again.",
                ),
                QuitDeferral::SyncOperation {
                    cancellation_requested: false,
                } => crate::locale::text(
                    "配置同步仍在执行；可在同步页取消或等待结果，完成后请再次退出。",
                    "Configuration sync is still running. Cancel it on the sync page or wait for its result, then try quitting again.",
                ),
                QuitDeferral::VaultOperation => crate::locale::text(
                    "Vault 操作正在执行且不能安全取消；等待结果后再退出。",
                    "A Vault operation is running and cannot be safely cancelled. Wait for its result before quitting.",
                ),
                QuitDeferral::Saving => crate::locale::text(
                    "配置正在保存；等待保存回执后再退出。",
                    "Configuration is being saved. Wait for the save receipt before quitting.",
                ),
                QuitDeferral::OutcomeUnknown => crate::locale::text(
                    "配置写入结果未知；必须先重新读取并核对，不能按普通忙碌状态退出。",
                    "A configuration write has an unknown result. Read it back and verify before quitting.",
                ),
            }
            .to_string();
            self.status = Some(message.clone());
            cx.emit(PreferencesEvent::QuitDeferred { message });
            cx.notify();
            return false;
        }
        let settings_close = self
            .settings
            .as_mut()
            .map(ConfigDraft::request_close)
            .unwrap_or(CloseRequest::CloseNow);
        let profile_close = self
            .profile_draft
            .as_mut()
            .map(ConfigDraft::request_close)
            .unwrap_or(CloseRequest::CloseNow);
        if settings_close == CloseRequest::WaitForSave || profile_close == CloseRequest::WaitForSave
        {
            let message = crate::locale::text(
                "保存回执或配置读回核对完成前不能退出。",
                "Wait for the save receipt or configuration readback before quitting.",
            )
            .to_string();
            self.status = Some(message.clone());
            cx.emit(PreferencesEvent::QuitDeferred { message });
            cx.notify();
            return false;
        }
        if settings_close == CloseRequest::ConfirmDiscard
            || profile_close == CloseRequest::ConfirmDiscard
            || self.has_unadded_inputs()
        {
            self.quit_confirmation = true;
            cx.notify();
            return false;
        }
        true
    }

    /// 用户确认丢弃偏好草稿后发出串行退出事件，具体进程退出仍由 RootView 负责。
    pub(in crate::features::preferences) fn confirm_quit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.quit_confirmation {
            return;
        }
        self.quit_confirmation = false;
        self.discard_settings_and_profile_drafts();
        self.restore_form_inputs(window, cx);
        cx.emit(PreferencesEvent::QuitConfirmed);
        cx.notify();
    }

    /// 取消退出保留表单和 opaque revision，RootView 不会继续关闭其它工作区。
    pub(in crate::features::preferences) fn cancel_quit(&mut self, cx: &mut Context<Self>) {
        self.quit_confirmation = false;
        self.cancel_draft_close_markers();
        cx.emit(PreferencesEvent::QuitCancelled);
        cx.notify();
    }

    /// 重用状态机回到编辑态，不在退出确认时复制一份 settings/profile 快照。
    fn discard_settings_and_profile_drafts(&mut self) {
        if let Some(draft) = self.settings.as_mut() {
            if draft.close_confirmation_open() {
                let _ = draft.discard_and_close();
            } else {
                let _ = draft.discard_changes();
            }
        }
        if let Some(draft) = self.profile_draft.as_mut() {
            if draft.close_confirmation_open() {
                let _ = draft.discard_and_close();
            } else {
                let _ = draft.discard_changes();
            }
        }
    }

    /// 同时清理两个草稿的确认标志，保留用户做出的任何编辑。
    ///
    /// @author kongweiguang
    fn cancel_draft_close_markers(&mut self) {
        if let Some(draft) = self.settings.as_mut()
            && draft.close_confirmation_open()
        {
            let _ = draft.cancel_close();
        }
        if let Some(draft) = self.profile_draft.as_mut()
            && draft.close_confirmation_open()
        {
            let _ = draft.cancel_close();
        }
    }

    /// 丢弃后同步输入和基线诊断，避免旧文本残留或原文件问题随关闭确认消失。
    fn restore_form_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(settings) = self.settings.as_ref().map(|draft| draft.value().clone()) {
            self.sync_settings_inputs(&settings, window, cx);
            self.rebuild_keyword_inputs(&settings.terminal.keyword_highlights.rules, window, cx);
        }
        if let Some(profile) = self
            .profile_draft
            .as_ref()
            .map(|draft| draft.value().clone())
        {
            self.sync_profile_inputs(&profile, window, cx);
        }
        self.custom_agent_name.clear();
        self.custom_agent_command.clear();
        self.new_keyword_pattern.clear();
        self.new_keyword_note.clear();
        super::set_input(&self.agent_launcher_inputs.name, String::new(), window, cx);
        super::set_input(
            &self.agent_launcher_inputs.command,
            String::new(),
            window,
            cx,
        );
        super::set_input(
            &self.terminal_behavior_inputs.new_keyword_pattern,
            String::new(),
            window,
            cx,
        );
        super::set_input(
            &self.terminal_behavior_inputs.new_keyword_note,
            String::new(),
            window,
            cx,
        );
        self.settings_input_errors.clear();
        self.profile_input_errors.clear();
        self.refresh_settings_issues();
        self.profile_issues.clear();
        self.status = None;
    }

    /// 临时 Agent/关键词输入在添加前也属于可丢弃草稿，关闭时不静默遗失。
    fn has_unadded_inputs(&self) -> bool {
        !self.custom_agent_name.trim().is_empty()
            || !self.custom_agent_command.trim().is_empty()
            || !self.new_keyword_pattern.trim().is_empty()
            || !self.new_keyword_note.trim().is_empty()
    }

    /// 设置模态的 X 统一执行 dirty-close 闭环，不在叶子层假定返回目标。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn close_button(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request_close(cx);
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_quit_gate.rs"]
mod tests;
