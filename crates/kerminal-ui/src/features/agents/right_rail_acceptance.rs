//! Agent 右栏原生验收所需的生产动作薄桥。
//!
//! @author kongweiguang

use super::*;

/// 限制验收快照为面板可见状态，避免把启动命令或 workspace 内容带入报告。
#[derive(Clone, Debug)]
pub(crate) struct AgentAcceptancePanelSnapshot {
    pub busy: bool,
    pub loading: bool,
    pub sessions_verified: bool,
    pub status: Option<String>,
    pub selected: Option<AgentSessionView>,
    pub sessions: Vec<AgentSessionView>,
}

impl AgentsPanel {
    /// 只在本轮 typed 配置已刷新到 launcher catalog 后继续验收，加载和失败状态分别保留。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_launcher_ready(
        &self,
        launcher_id: &str,
    ) -> Result<bool, String> {
        if let Some(error) = self.launcher_load_error.as_ref() {
            return Err(error.clone());
        }
        if let Some(error) = self.launcher_selection_error.as_ref() {
            return Err(error.clone());
        }
        if self.launcher_catalog_loading || self.launcher_selection_busy {
            return Ok(false);
        }
        let Some(catalog) = self.launcher_catalog.as_ref() else {
            return Ok(false);
        };
        let expected_key = format!("custom:{launcher_id}");
        Ok(self
            .launcher_key
            .as_ref()
            .is_some_and(|key| key.as_str() == expected_key)
            && catalog.selected_key.as_str() == expected_key
            && catalog
                .options
                .iter()
                .any(|option| option.key.as_str() == expected_key))
    }

    /// 把配置或 validator 失败归入既有 launcher 错误状态，使等待方得到可读错误并停止后续 CLI。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_record_setup_error(
        &mut self,
        message: String,
        cx: &mut Context<Self>,
    ) {
        self.launcher_load_error = Some(message.clone());
        self.status = Some(message);
        self.launcher_catalog_loading = false;
        cx.notify();
    }

    /// 配置写入完成后通过生产“新建对话”动作清掉任何从 fixture 历史恢复的旧选择。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_show_new_chat(&mut self, cx: &mut Context<Self>) -> bool {
        if self.busy || self.launcher_selection_busy {
            return false;
        }
        self.show_new_chat(cx);
        self.selected_agent.is_none()
    }

    /// 仅以已配置的本轮 custom key 调用正式新建动作，拒绝重复开始或回退 builtin。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_create_agent(
        &mut self,
        launcher_id: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy
            || self.loading
            || !self.sessions_verified
            || self.right_rail_acceptance_launcher_ready(launcher_id) != Ok(true)
        {
            return false;
        }
        self.create_agent(cx);
        self.busy
    }

    /// 继续仅针对仍 Running 的精确 Agent ID；复用历史行的目标绑定与恢复资格判断。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_continue_agent(
        &mut self,
        agent_session_id: &AgentSessionId,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy || self.loading || !self.sessions_verified {
            return false;
        }
        let Some(view) = self
            .sessions
            .iter()
            .find(|view| &view.session.agent_session_id == agent_session_id)
        else {
            return false;
        };
        if !render::has_running_agent_terminal(view) {
            return false;
        }
        let id = view.session.agent_session_id.clone();
        let expected_title = view.session.title.clone();
        self.continue_agent_from_history(
            id,
            expected_title,
            self.history_mode,
            self.scope.clone(),
            self.candidate
                .as_ref()
                .map(|candidate| candidate.binding.clone()),
            cx,
        );
        true
    }

    /// 改名复用原历史 CAS 草稿和输入校验，目标与标题 revision 始终固定在选中的会话身份上。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_rename_agent(
        &mut self,
        agent_session_id: &AgentSessionId,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy || !can_save_agent_session_title(&title) {
            return false;
        }
        let Some(view) = self
            .sessions
            .iter()
            .find(|view| &view.session.agent_session_id == agent_session_id)
        else {
            return false;
        };
        let id = view.session.agent_session_id.clone();
        let expected_title = view.session.title.clone();
        let candidate = self
            .candidate
            .as_ref()
            .map(|candidate| candidate.binding.clone());
        self.begin_rename_agent(
            AgentRenameTarget {
                id: id.clone(),
                expected_title: expected_title.clone(),
            },
            self.history_mode,
            self.scope.clone(),
            candidate,
            window,
            cx,
        );
        if self.rename_target.as_ref() != Some(&(id, expected_title)) {
            return false;
        }
        self.rename_title
            .update(cx, |input, cx| input.set_value(title, window, cx));
        self.save_agent_title(cx);
        self.busy
    }

    /// 首次打开生产 Kit 确认框，后续仅在同一 owner 仍有效时提交生产确认动作。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_archive_agent(
        &mut self,
        agent_session_id: &AgentSessionId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy || self.loading || !self.sessions_verified {
            return false;
        }
        if let Some(owner) = self.archive_confirmation_owner.clone() {
            if &owner.id != agent_session_id {
                return false;
            }
            return self.confirm_archive_confirmation(&owner, window.window_handle(), cx);
        }
        let Some(view) = self.sessions.iter().find(|view| {
            &view.session.agent_session_id == agent_session_id
                && view.session.status != AgentSessionStatus::Archived
        }) else {
            return false;
        };
        self.open_archive_confirmation(
            AgentArchiveConfirmationOwner {
                id: view.session.agent_session_id.clone(),
                expected_title: view.session.title.clone(),
                mode: self.history_mode,
                scope: self.scope.clone(),
                candidate: self
                    .candidate
                    .as_ref()
                    .map(|candidate| candidate.binding.clone()),
                generation: 0,
            },
            window,
            cx,
        );
        false
    }

    /// 仅在指定会话的生产确认 owner 已挂载且归档尚未开始时报告 ready。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_archive_confirmation_ready(
        &self,
        agent_session_id: &AgentSessionId,
    ) -> bool {
        !self.busy
            && self
                .archive_confirmation_owner
                .as_ref()
                .is_some_and(|owner| &owner.id == agent_session_id)
    }

    /// 由 desktop harness 读取生产 projection 和操作状态，驱动器不访问 AgentPanel 私有字段。
    ///
    /// @author kongweiguang
    pub(crate) fn right_rail_acceptance_snapshot(&self) -> AgentAcceptancePanelSnapshot {
        AgentAcceptancePanelSnapshot {
            busy: self.busy,
            loading: self.loading,
            sessions_verified: self.sessions_verified,
            status: self.status.clone(),
            selected: self.selected_view().cloned(),
            sessions: self.sessions.clone(),
        }
    }
}
