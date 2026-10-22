//! Root 的更新退出保护请求协调；外部对话框回执必须匹配原始下载句柄。
//!
//! @author kongweiguang

use kerminal_application::updates::DownloadedUpdateId;

/// 在设置、编辑器和 workspace 保存门禁期间保持不变的安装身份。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct UpdateInstallRequest {
    pub update_id: DownloadedUpdateId,
    pub version: String,
}

/// Root 已有保护流程中的请求接纳结果，避免重复点击或旧请求覆盖新句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GuardRequestResult {
    Started,
    AlreadyPending,
    Busy,
}

/// 单个根窗口一次只保护一个更新安装；桌面安装生命周期仍由 composition root 持有。
#[derive(Default)]
pub(crate) struct UpdateInstallGuard {
    pending: Option<UpdateInstallRequest>,
}

impl UpdateInstallGuard {
    /// 接纳首个安装请求；相同句柄视作重复提交，不同句柄在现有保护结束前拒绝。
    pub(crate) fn begin(
        &mut self,
        update_id: DownloadedUpdateId,
        version: String,
    ) -> GuardRequestResult {
        let request = UpdateInstallRequest { update_id, version };
        match &self.pending {
            None => {
                self.pending = Some(request);
                GuardRequestResult::Started
            }
            Some(pending) if pending == &request => GuardRequestResult::AlreadyPending,
            Some(_) => GuardRequestResult::Busy,
        }
    }

    /// 只消费与当前请求匹配的成功回执，防止迟到对话框启动另一个版本的更新。
    pub(crate) fn approve(
        &mut self,
        update_id: DownloadedUpdateId,
    ) -> Option<UpdateInstallRequest> {
        self.take_matching(update_id)
    }

    /// 取消时只清理对应句柄；调用方据此让面板恢复 Ready 而不影响新请求。
    pub(crate) fn cancel(&mut self, update_id: DownloadedUpdateId) -> Option<UpdateInstallRequest> {
        self.take_matching(update_id)
    }

    /// 读取当前待保护身份，便于 Root 将设置、编辑器和保存回执绑定到同一个请求。
    pub(crate) fn pending(&self) -> Option<&UpdateInstallRequest> {
        self.pending.as_ref()
    }

    /// 不匹配时保留活动请求；仅精确身份可解除门禁状态。
    fn take_matching(&mut self, update_id: DownloadedUpdateId) -> Option<UpdateInstallRequest> {
        if self
            .pending
            .as_ref()
            .is_some_and(|request| request.update_id == update_id)
        {
            return self.pending.take();
        }
        None
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_updates.rs"]
mod tests;
