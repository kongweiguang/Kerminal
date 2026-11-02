//! 双端传输原生验收的只读投影；所有变更仍经生产窗口输入完成。
//!
//! @author kongweiguang

use kerminal_domain::{
    files::{FileEndpoint, TransferSummary},
    identity::HostId,
};

use super::FilesPane;

impl FilesPane {
    /// 返回真实已选目标与目录就绪状态，避免把未选目标的内部占位当作本机。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_workspace_acceptance_endpoint(
        &self,
        side: usize,
    ) -> Option<(Option<FileEndpoint>, bool, Option<String>)> {
        let panel = self.panels.get(side)?;
        if self.compact_layout || !self.host_picker.enabled {
            return None;
        }
        let endpoint = (!self.host_picker.target_unselected[side]).then(|| panel.endpoint.clone());
        let ready = endpoint.as_ref().is_some_and(|endpoint| {
            !self.host_picker.target_pending[side]
                && !panel.loading
                && panel.requested_endpoint.is_none()
                && panel
                    .listing
                    .as_ref()
                    .is_some_and(|listing| &listing.directory == endpoint)
        });
        Some((endpoint, ready, panel.directory_error.clone()))
    }

    /// 仅回读当前面板的文件选择，避免自动化直接调用选择器或传输提交逻辑。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_workspace_acceptance_selected_names(&self, side: usize) -> Vec<String> {
        self.selected_entries(side)
            .into_iter()
            .map(|entry| entry.name)
            .collect()
    }

    /// 回读真实搜索输入的受控值，证明键盘事件被工作台接收。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_workspace_acceptance_query(&self, side: usize) -> Option<String> {
        self.host_picker.query.get(side).cloned()
    }

    /// 回读侧内稳定标签身份及活动项，确保同一主机的多个标签不会按 HostId 合并。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_workspace_acceptance_host_tabs(
        &self,
        side: usize,
    ) -> Vec<(String, Option<HostId>, bool)> {
        let Some(tabs) = self.host_picker.host_tabs.sides.get(side) else {
            return Vec::new();
        };
        let active = self.host_picker.host_tabs.active[side];
        tabs.iter()
            .map(|tab| {
                (
                    tab.key(),
                    tab.target.host_id().cloned(),
                    active == Some(tab.id),
                )
            })
            .collect()
    }

    /// 返回已由生产订阅接收的队列快照，不创建第二个传输服务。
    ///
    /// @author kongweiguang
    pub(crate) fn transfer_workspace_acceptance_rows(&self) -> Vec<TransferSummary> {
        self.transfer_rows.clone()
    }
}
