//! Hidden-window observations for host selection and terminal creation workflows.
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use gpui_kit::Context;
use kerminal_application::workspace::WorkspaceTargetRef;
use kerminal_domain::{
    agents::AgentTargetBinding,
    identity::{HostId, PaneId, SessionId, TabId},
    sessions::SessionTarget,
};

use super::{CenterPanel, RootView, workspace_runtime::RuntimePanePhase};

impl RootView {
    /// 等主机、Profile 和工作区快照都已就绪后才允许驱动控件，避免验收点击落在加载中间态。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_ready(&self, host_id: &HostId, cx: &Context<Self>) -> bool {
        !self.workspace_load_pending
            && self.settings_snapshot.is_some()
            && self.workspace_controller.is_some()
            && self.host_navigation.as_ref().is_some_and(|navigation| {
                let navigation = navigation.read(cx);
                navigation.display_name_for(host_id).is_some()
                    && navigation.display_default_profile_name().is_some()
            })
    }

    /// 同时核对目录选择与终端中心未被替换，避免单击选中时偷偷导航到详情。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_host_selected(
        &self,
        host_id: &HostId,
        _cx: &Context<Self>,
    ) -> bool {
        self.center_panel == CenterPanel::Terminal
            && self.selected_host_id.as_ref() == Some(host_id)
    }

    /// 暴露模态遮挡状态，使隐藏窗口验收能区分事件选择成功与中心内容被模态替代。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_connections_modal_open(&self) -> bool {
        self.connections_modal_open
    }

    /// 核对中心详情实体是否已挂载，避免把缺失服务误诊为详情 selector 不可见。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_connections_panel_present(&self) -> bool {
        self.connections_panel.is_some()
    }

    /// 本地目录单击清除远端选择，但保留中央终端及其会话。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_local_profile_selected(&self) -> bool {
        self.center_panel == CenterPanel::Terminal && self.selected_host_id.is_none()
    }

    /// 复制当前候选绑定作为稳定观测值，验证主机和 Profile 导航不重绑活动 Agent 目标。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_agent_target_key(&self) -> Option<AgentTargetBinding> {
        self.agent_target_candidate
            .0
            .as_ref()
            .map(|candidate| candidate.binding.clone())
    }

    /// 同时观测创建门禁、请求归属和可见状态，避免把尚未开始的请求误判为已经回滚。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_terminal_creation_state(&self) -> (bool, bool, Option<String>) {
        (
            self.is_creating_session,
            self.pending_terminal_creation_owner.is_some(),
            self.status.as_ref().map(ToString::to_string),
        )
    }

    /// 只投影本地 pane 并按稳定 ID 去重，使桌面验收能分辨独立 Tab、Session 和失败状态。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_local_state(&self) -> (usize, usize, Vec<String>, Vec<String>) {
        let mut tab_ids = BTreeSet::<TabId>::new();
        let mut ready_session_ids = BTreeSet::<SessionId>::new();
        let mut failed_pane_ids = BTreeSet::<PaneId>::new();
        let mut local_pane_count = 0;

        for (pane_id, pane) in self.workspace_runtime.panes() {
            let is_local = matches!(
                pane.persisted.target.as_ref(),
                Some(WorkspaceTargetRef::Local { .. })
            ) || matches!(pane.target.as_ref(), Some(SessionTarget::Local(_)));
            if !is_local {
                continue;
            }

            local_pane_count += 1;
            tab_ids.insert(pane.tab_id.clone());
            match pane.phase {
                RuntimePanePhase::Ready => {
                    if let Some(session_id) = pane.session_id.as_ref() {
                        ready_session_ids.insert(session_id.clone());
                    }
                }
                RuntimePanePhase::RestoreFailed => {
                    failed_pane_ids.insert(pane_id.clone());
                }
                RuntimePanePhase::RestorePending
                | RuntimePanePhase::Closing
                | RuntimePanePhase::CloseFailed => {}
            }
        }

        (
            tab_ids.len(),
            local_pane_count,
            ready_session_ids
                .into_iter()
                .map(|session_id| session_id.to_string())
                .collect(),
            failed_pane_ids
                .into_iter()
                .map(|pane_id| pane_id.to_string())
                .collect(),
        )
    }

    /// 只返回活动 Tab 中已 Ready 且有 Session 的本地 pane，避免分屏点击命中恢复中目标。
    ///
    /// @author kongweiguang
    pub fn workbench_acceptance_active_local_pane_id(&self) -> Option<String> {
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        let active_tab_id = snapshot.active_tab_id.as_ref()?;
        let pane_id = snapshot.focused_pane_id.as_ref()?;
        let pane = self.workspace_runtime.pane(pane_id)?;
        let is_local = matches!(
            pane.persisted.target.as_ref(),
            Some(WorkspaceTargetRef::Local { .. })
        ) || matches!(pane.target.as_ref(), Some(SessionTarget::Local(_)));

        (is_local
            && pane.tab_id == *active_tab_id
            && pane.phase == RuntimePanePhase::Ready
            && pane.session_id.is_some())
        .then(|| pane_id.to_string())
    }
}
