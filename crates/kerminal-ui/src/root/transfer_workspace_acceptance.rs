//! 双端传输原生验收的只读 Root 桥接。
//!
//! @author kongweiguang

use gpui_kit::{Context, Entity};
use kerminal_application::workspace::WorkspaceSessionTab;
use kerminal_domain::{
    files::{FileEndpoint, TransferSummary},
    identity::HostId,
};

use crate::features::files::FilesPane;

use super::{CenterPanel, QuitIntent, RootView, workspace_files::FilesWorkspaceSelection};

impl RootView {
    /// 冷重启验收必须等正常退出门禁确认工作区保存，失败时保留原错误而不是提前结束事件循环。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_quit_ready(&self) -> Result<bool, String> {
        if let Some(error) = &self.quit_preparation_failure {
            return Err(error.to_string());
        }
        Ok(self.quit_ready_emitted
            && self.host_shutdown_requested
            && self.quit_ready_intent == Some(QuitIntent::Exit))
    }

    /// 只查询当前真实展示的中央文件实体，不把后台标签误认为活动工作台。
    ///
    /// @author kongweiguang
    fn transfer_workspace_acceptance_pane(&self) -> Option<Entity<FilesPane>> {
        if self.center_panel != CenterPanel::Files {
            return None;
        }
        match &self.files_workspace_selection {
            FilesWorkspaceSelection::Workbench => self.files_pane.clone(),
            FilesWorkspaceSelection::TransferTab(tab_id) => self.workspace_files.files_pane(tab_id),
        }
    }

    /// 读取真实中央面板的目标和就绪状态，等待时不触发任何导航或连接操作。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_endpoint(
        &self,
        side: usize,
        cx: &Context<Self>,
    ) -> Option<(Option<FileEndpoint>, bool, Option<String>)> {
        self.transfer_workspace_acceptance_pane()?
            .read(cx)
            .transfer_workspace_acceptance_endpoint(side)
    }

    /// 读取生产列表当前选择，验证真实行点击确实绑定到预期项目。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_selected_names(
        &self,
        side: usize,
        cx: &Context<Self>,
    ) -> Option<Vec<String>> {
        Some(
            self.transfer_workspace_acceptance_pane()?
                .read(cx)
                .transfer_workspace_acceptance_selected_names(side),
        )
    }

    /// 查询当前中央视图的搜索词，不改变输入焦点或替自动化写入草稿。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_query(
        &self,
        side: usize,
        cx: &Context<Self>,
    ) -> Option<String> {
        self.transfer_workspace_acceptance_pane()?
            .read(cx)
            .transfer_workspace_acceptance_query(side)
    }

    /// 读取当前中央面板的侧内标签身份，用于验证重复主机标签的真实切换。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_host_tabs(
        &self,
        side: usize,
        cx: &Context<Self>,
    ) -> Option<Vec<(String, Option<HostId>, bool)>> {
        Some(
            self.transfer_workspace_acceptance_pane()?
                .read(cx)
                .transfer_workspace_acceptance_host_tabs(side),
        )
    }

    /// 读取生产传输队列以确认窗口动作形成的真实任务和终态。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_rows(
        &self,
        cx: &Context<Self>,
    ) -> Option<Vec<TransferSummary>> {
        Some(
            self.transfer_workspace_acceptance_pane()?
                .read(cx)
                .transfer_workspace_acceptance_rows(),
        )
    }

    /// 仅读回当前独立传输标签的持久化描述，不能用共享临时页冒充可恢复标签。
    ///
    /// @author kongweiguang
    pub fn transfer_workspace_acceptance_tab(&self) -> Option<WorkspaceSessionTab> {
        let FilesWorkspaceSelection::TransferTab(tab_id) = &self.files_workspace_selection else {
            return None;
        };
        self.workspace_controller
            .as_ref()?
            .snapshot()
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
            .cloned()
    }
}
