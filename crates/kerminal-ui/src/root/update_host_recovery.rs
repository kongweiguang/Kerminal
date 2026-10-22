//! Windows 更新后恢复主实例监听器失败时，Root 保留安装结果并提供安全退出状态。
//!
//! @author kongweiguang

use gpui_kit::{Context, SharedString};
use kerminal_application::updates::{DownloadedUpdateId, UpdateInstallResult};

use super::{RootHostLifecycle, RootView, updates::UpdateInstallRequest};

impl RootView {
    /// 仅接收匹配回执并独立保留原错；listener 恢复失败不覆盖 SDK 结果，也不开放同包重试或普通服务。
    ///
    /// @author kongweiguang
    pub fn report_update_listener_recovery_failure(
        &mut self,
        update_id: DownloadedUpdateId,
        version: &str,
        original_result: UpdateInstallResult,
        safe_message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if !pending_install_matches(self.pending_update_install.as_ref(), update_id, version) {
            return;
        }

        let safe_message = safe_message.into();
        self.host_lifecycle = RootHostLifecycle::ListenerRecoveryFailed;
        self.host_failure_message = Some(safe_message.clone());
        self.status = Some(safe_message.clone());
        self.updates_panel.update(cx, |panel, cx| {
            panel.report_host_listener_recovery_failure(
                update_id,
                original_result,
                safe_message.to_string(),
                cx,
            );
        });
        cx.notify();
    }
}

/// 绑定 listener 回执到原始更新身份，避免迟到状态修改另一版本的面板和 Root 生命周期。
fn pending_install_matches(
    pending: Option<&UpdateInstallRequest>,
    update_id: DownloadedUpdateId,
    version: &str,
) -> bool {
    pending.is_some_and(|request| request.update_id == update_id && request.version == version)
}

#[cfg(test)]
#[path = "../../tests/unit/root_update_host_recovery.rs"]
mod tests;
