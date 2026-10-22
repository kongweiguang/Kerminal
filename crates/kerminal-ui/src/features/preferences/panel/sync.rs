//! 配置仓库状态与显式同步动作的 UI 协调。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{
    AppError, PublicErrorCode,
    configuration_sync::{
        ConfigurationSyncOperationKind, ConfigurationSyncOperationResult, ConfigurationSyncOutcome,
        ConfigurationSyncService,
    },
};
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::PreferencesPanel;

#[derive(Clone, Default)]
pub(super) struct SyncCancellationHandle {
    state: Arc<Mutex<SyncCancellationState>>,
}

#[derive(Default)]
struct SyncCancellationState {
    token: Option<CancellationToken>,
    requested: bool,
}

impl SyncCancellationHandle {
    /// 记录 executor 交给 application 用例的令牌；若用户先点取消则立即转发请求。
    fn install(&self, token: CancellationToken) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.requested {
            token.cancel();
        }
        state.token = Some(token);
    }

    /// 只请求 application 协作取消并等待原用例清理回执，不中断 Git 子进程或丢弃结果。
    pub(super) fn request_cancel(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.requested {
            return false;
        }
        state.requested = true;
        if let Some(token) = state.token.as_ref() {
            token.cancel();
        }
        true
    }

    /// UI 通过令牌状态显示“正在等待清理”，不会把取消请求冒充为已完成。
    pub(super) fn cancel_requested(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .requested
    }
}

impl PreferencesPanel {
    /// 状态读取只查询本地工作树；远端引用必须由用户显式 Fetch 更新。
    pub(in crate::features::preferences) fn refresh_configuration_sync(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.sync_loading || self.sync_busy.is_some() {
            return;
        }
        let Some(service) = self.configuration_sync.clone() else {
            self.sync_error = Some(
                crate::locale::text(
                    "配置同步服务未由应用装配。",
                    "The configuration sync service is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        self.sync_loading = true;
        self.sync_error = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|_| async move { service.status().await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.sync_loading = false;
                match result {
                    Ok(status) => {
                        panel.sync_status = Some(status);
                        panel.sync_error = None;
                    }
                    Err(error) => panel.sync_error = Some(super::localized_error(&error)),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 启动一个显式 Git 操作，并保留本地提交、远端推送与未知结果的区别。
    pub(in crate::features::preferences) fn run_configuration_sync(
        &mut self,
        kind: ConfigurationSyncOperationKind,
        cx: &mut Context<Self>,
    ) {
        if self.sync_loading || self.sync_busy.is_some() {
            return;
        }
        if self.sync_outcome_unknown && kind != ConfigurationSyncOperationKind::Fetch {
            self.sync_error = Some(crate::locale::text(
                "上次操作结果未知，请先 Fetch 并检查仓库状态。",
                "The previous result is unknown. Fetch and inspect the repository before continuing.",
            ).to_string());
            cx.notify();
            return;
        }
        let Some(service) = self.configuration_sync.clone() else {
            self.sync_error = Some(
                crate::locale::text(
                    "配置同步服务未由应用装配。",
                    "The configuration sync service is unavailable.",
                )
                .to_string(),
            );
            cx.notify();
            return;
        };

        self.sync_busy = Some(kind);
        self.sync_error = None;
        self.sync_result = None;
        let cancellation_handle = SyncCancellationHandle::default();
        self.sync_cancellation = Some(cancellation_handle.clone());
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| {
                    cancellation_handle.install(cancellation.clone());
                    async move { execute_sync_action(service, kind, cancellation).await }
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.sync_busy = None;
                panel.sync_cancellation = None;
                match result {
                    Ok(operation) => {
                        panel.sync_outcome_unknown =
                            operation.outcome == ConfigurationSyncOutcome::OutcomeUnknown;
                        if kind == ConfigurationSyncOperationKind::Fetch
                            && matches!(
                                operation.outcome,
                                ConfigurationSyncOutcome::Succeeded
                                    | ConfigurationSyncOutcome::NoChanges
                            )
                        {
                            panel.sync_outcome_unknown = false;
                        }
                        panel.sync_result = Some(operation);
                    }
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            panel.sync_outcome_unknown = true;
                        }
                        panel.sync_error = Some(super::localized_error(&error));
                    }
                }
                cx.notify();
                if !panel.sync_outcome_unknown {
                    panel.refresh_configuration_sync(cx);
                }
            });
        })
        .detach();
    }

    /// 取消按钮仅发出协作信号，UI 仍等待 application 返回清理或未知结果回执。
    pub(in crate::features::preferences) fn cancel_configuration_sync(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        let requested = self
            .sync_cancellation
            .as_ref()
            .is_some_and(SyncCancellationHandle::request_cancel);
        if requested {
            self.sync_error = Some(crate::locale::text(
                "已请求取消，正在等待 Git 清理回执；完成前不能启动另一项同步。",
                "Cancellation requested. Waiting for Git cleanup before another sync can start.",
            ).to_string());
        }
        cx.notify();
    }

    /// 只向同步视图投影取消请求状态，令牌本身仍由面板用例私有持有。
    pub(in crate::features::preferences) fn sync_cancel_requested(&self) -> bool {
        self.sync_cancellation
            .as_ref()
            .is_some_and(SyncCancellationHandle::cancel_requested)
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/preferences_sync_cancellation.rs"]
mod tests;

/// 将唯一的同步操作 enum 映射到 application 用例，取消令牌随网络/进程操作传播。
async fn execute_sync_action(
    service: ConfigurationSyncService,
    kind: ConfigurationSyncOperationKind,
    cancellation: CancellationToken,
) -> Result<ConfigurationSyncOperationResult, AppError> {
    match kind {
        ConfigurationSyncOperationKind::Fetch => service.fetch(cancellation).await,
        ConfigurationSyncOperationKind::Pull => service.pull(cancellation).await,
        ConfigurationSyncOperationKind::Commit => service.commit(cancellation).await,
        ConfigurationSyncOperationKind::Push => service.push(cancellation).await,
        ConfigurationSyncOperationKind::ContinueRebase => {
            service.continue_rebase(cancellation).await
        }
        ConfigurationSyncOperationKind::AbortRebase => service.abort_rebase(cancellation).await,
    }
}

/// 首次建立面板时读取配置仓库、Vault readiness 和外部协议状态，不触发变更操作。
///
/// @author kongweiguang
impl PreferencesPanel {
    pub(super) fn load_integrations(&mut self, cx: &mut Context<Self>) {
        self.refresh_configuration_sync(cx);
        self.refresh_vault_availability(cx);
        self.refresh_external_protocol(cx);
    }
}
