//! 临时外部 SSH 的认证回执与同 pane 密码重试；不持久化目标或凭据。
//!
//! @author kongweiguang

use gpui_kit::{Context, SharedString, Window};
use kerminal_application::{PublicErrorCode, platform::external_launch::ExternalRoutedTarget};
use kerminal_domain::{
    identity::{HostId, PaneId},
    sessions::SessionTarget,
};

use super::{
    CenterPanel, RootView, WorkspaceTerminalOpenKind, terminal_open::WorkspaceTerminalOpenRequest,
    workspace_runtime::RuntimePanePhase,
};

/// 仅初次连接或用户明确提交密码的尝试持有目标；回执消费它，普通重连不获得提示资格。
pub(super) struct ExternalSshAttempt {
    pub(super) target: ExternalRoutedTarget,
    pub(super) generation: u64,
}

/// 密码提示只需冻结目标身份和 pane 代次；秘密与 lease 由既有 ConnectionsPanel 管理。
pub(super) struct ExternalPasswordRetryTicket {
    host_id: HostId,
    pane_id: PaneId,
    generation: u64,
}

impl RootView {
    /// 显式取消只使同一临时主机的重试票据失效；密码提交后的 host-key 预检不是取消信号。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_external_password_retry(&mut self, host_id: &HostId) {
        if self
            .pending_external_password_retry
            .as_ref()
            .is_some_and(|ticket| &ticket.host_id == host_id)
        {
            self.pending_external_password_retry = None;
        }
    }

    /// 只有同代获准的认证尝试失败且资源已收回时才提供密码输入；其它失败保留原分类和重试路径。
    ///
    /// @author kongweiguang
    pub(super) fn offer_external_password_retry(
        &mut self,
        pane_id: &PaneId,
        generation: u64,
        code: PublicErrorCode,
        cleanup_pending: bool,
        cx: &mut Context<Self>,
    ) {
        if !self
            .external_ssh_attempts
            .get(pane_id)
            .is_some_and(|attempt| attempt.generation == generation)
        {
            return;
        }
        let Some(attempt) = self.external_ssh_attempts.remove(pane_id) else {
            return;
        };
        if !should_offer_password_retry(
            code,
            cleanup_pending,
            attempt.target.password_retry_available,
            self.host_shutdown_requested,
        ) {
            return;
        }
        let Some(panel) = self.connections_panel.clone() else {
            return;
        };
        if panel.read(cx).root_confirmation_active() {
            self.external_ssh_attempts.insert(pane_id.clone(), attempt);
            if !self
                .pending_external_password_offers
                .contains(&(pane_id.clone(), generation))
            {
                self.pending_external_password_offers
                    .push_back((pane_id.clone(), generation));
            }
            self.status = Some(crate::locale::text(
                "外部 SSH 认证失败；完成当前提示后将继续请求此连接的密码。",
                "External SSH authentication failed. The password prompt will continue after the current prompt.",
            ));
            return;
        }
        let ticket = ExternalPasswordRetryTicket {
            host_id: attempt.target.host_id().clone(),
            pane_id: pane_id.clone(),
            generation,
        };
        let accepted = panel.update(cx, |panel, cx| {
            panel.request_external_password_retry(attempt.target.clone(), cx)
        });
        if accepted {
            self.pending_external_password_retry = Some(ticket);
        }
    }

    /// 多个外部连接失败时串行使用已有模态；旧 pane 关闭或换代即释放排队目标。
    ///
    /// @author kongweiguang
    pub(super) fn resume_queued_external_password_offer(&mut self, cx: &mut Context<Self>) {
        if self.host_shutdown_requested
            || self
                .connections_panel
                .as_ref()
                .is_none_or(|panel| panel.read(cx).root_confirmation_active())
        {
            return;
        }
        while let Some((pane_id, generation)) = self.pending_external_password_offers.pop_front() {
            if self.workspace_restore_is_current(&pane_id, generation)
                && self.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
                    pane.phase == RuntimePanePhase::RestoreFailed && pane.session_id.is_none()
                })
            {
                self.offer_external_password_retry(
                    &pane_id,
                    generation,
                    PublicErrorCode::AuthenticationFailed,
                    false,
                    cx,
                );
                break;
            }
            if self
                .external_ssh_attempts
                .get(&pane_id)
                .is_some_and(|attempt| attempt.generation == generation)
            {
                self.external_ssh_attempts.remove(&pane_id);
            }
        }
    }

    /// 提交密码后只重试原失败 pane；原 pane 已关闭、重连或换代时消费回执并显示取消，绝不新建其它目标。
    ///
    /// @author kongweiguang
    pub(super) fn resume_external_password_retry(
        &mut self,
        target: &ExternalRoutedTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<bool> {
        let ticket = self.pending_external_password_retry.as_ref()?;
        if &ticket.host_id != target.host_id() {
            return None;
        }
        let ticket = self.pending_external_password_retry.take()?;
        let current = self.workspace_restore_is_current(&ticket.pane_id, ticket.generation)
            && !self.host_shutdown_requested;
        let pane = self.workspace_runtime.pane(&ticket.pane_id).filter(|pane| {
            current
                && pane.phase == RuntimePanePhase::RestoreFailed
                && pane.session_id.is_none()
                && pane.transient
                && matches!(pane.target.as_ref(), Some(SessionTarget::Ssh(ssh)) if ssh.host_id == ticket.host_id)
        });
        let Some((tab_id, size, session_target)) =
            pane.and_then(|pane| Some((pane.tab_id.clone(), pane.size, pane.target.clone()?)))
        else {
            self.status = Some(crate::locale::text(
                "原外部终端已关闭或状态已变化，本次密码重试已取消。",
                "The original external terminal was closed or changed. This password retry was cancelled.",
            ));
            cx.notify();
            return Some(true);
        };
        let lease = match target.lease.retain() {
            Ok(lease) => lease,
            Err(error) => {
                self.status = Some(SharedString::from(error.public_message()));
                cx.notify();
                return Some(true);
            }
        };
        let (generation, cancellation) = self.begin_workspace_restore(&ticket.pane_id);
        // 当前 ticket 只能来自已验证的直连 SSH agent 回退；恢复内部提示资格不表示允许自动连接。
        let mut retry_target = target.clone();
        retry_target.password_retry_available = true;
        self.external_ssh_attempts.insert(
            ticket.pane_id.clone(),
            ExternalSshAttempt {
                target: retry_target,
                generation,
            },
        );
        let result = self.open_workspace_terminal_pane(
            WorkspaceTerminalOpenRequest {
                tab_id,
                pane_id: ticket.pane_id.clone(),
                kind: WorkspaceTerminalOpenKind::Direct(session_target),
                size,
                lease: Some(lease),
                transient: true,
                generation,
                cancellation,
            },
            cx,
        );
        if let Err(error) = result {
            self.external_ssh_attempts.remove(&ticket.pane_id);
            self.status = Some(error);
        } else {
            self.set_center_panel(CenterPanel::Terminal, window, cx);
        }
        cx.notify();
        Some(true)
    }
}

/// 根据稳定错误码决定是否请求交互；网络失败、清理未完成与显式私钥均不能隐式改变认证方式。
///
/// @author kongweiguang
fn should_offer_password_retry(
    code: PublicErrorCode,
    cleanup_pending: bool,
    retry_available: bool,
    stopping: bool,
) -> bool {
    code == PublicErrorCode::AuthenticationFailed
        && !cleanup_pending
        && retry_available
        && !stopping
}

#[cfg(test)]
#[path = "../../tests/unit/root_external_auth.rs"]
mod tests;
