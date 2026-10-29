//! 配置仓库状态与显式同步动作的 UI 协调。
//!
//! @author kongweiguang

use gpui_kit::Context;
use kerminal_application::{PublicErrorCode, configuration_sync::ConfigurationSyncOutcome};

use super::PreferencesPanel;

impl PreferencesPanel {
    /// 状态读取只查询本地工作树；远端引用必须由用户显式 Fetch 更新。
    pub(in crate::features::preferences) fn refresh_configuration_sync(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        if self.sync_loading || self.connectivity.sync_running || self.connectivity.key_saving {
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

    /// 同步与密钥保存共享忙碌门禁，避免两个工作区写操作并行触碰同一批文件。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn run_workspace_sync(&mut self, cx: &mut Context<Self>) {
        if self.connectivity.sync_running
            || self.sync_loading
            || self.connectivity.key_loading
            || self.connectivity.key_saving
        {
            return;
        }
        if self.sync_outcome_unknown {
            self.verify_workspace_sync(cx);
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

        self.connectivity.sync_running = true;
        self.connectivity.sync_run_result = None;
        self.sync_error = None;
        self.sync_result = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(|cancellation| async move { service.synchronize(cancellation).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.connectivity.sync_running = false;
                match result {
                    Ok(result) => {
                        panel.sync_outcome_unknown =
                            result.outcome == ConfigurationSyncOutcome::OutcomeUnknown;
                        panel.sync_result = result.stages.last().cloned();
                        panel.connectivity.sync_run_result = Some(result);
                    }
                    Err(error) => {
                        if error.public_code() == PublicErrorCode::OutcomeUnknown {
                            panel.sync_outcome_unknown = true;
                        }
                        panel.sync_error = Some(super::localized_error(&error));
                    }
                }
                cx.notify();
                let workspace_outcome_unknown = panel.sync_outcome_unknown;
                panel.connectivity.resume_pending_documents(
                    panel.vault_keys.clone(),
                    panel.configuration_sync.clone(),
                    workspace_outcome_unknown,
                    cx,
                );
                if !panel.sync_outcome_unknown {
                    panel.refresh_configuration_sync(cx);
                }
            });
        })
        .detach();
    }

    /// 结果未知时先更新远端引用并回读领先/落后状态，未取得完整回执前不重放同步写操作。
    fn verify_workspace_sync(&mut self, cx: &mut Context<Self>) {
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

        self.connectivity.sync_running = true;
        self.sync_error = None;
        let executor = self.executor.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |cancellation| async move {
                    let fetch = service.fetch(cancellation).await?;
                    let status = if matches!(
                        fetch.outcome,
                        ConfigurationSyncOutcome::Succeeded | ConfigurationSyncOutcome::NoChanges
                    ) {
                        Some(service.status().await?)
                    } else {
                        None
                    };
                    Ok((fetch, status))
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.connectivity.sync_running = false;
                match result {
                    Ok((fetch, Some(status))) => {
                        let summary = crate::locale::text(
                            format!(
                                "远端状态已刷新，本地领先 {}、落后 {}；核对后可重新同步。",
                                status.ahead.map_or_else(|| "未知".to_owned(), |count| count.to_string()),
                                status.behind.map_or_else(|| "未知".to_owned(), |count| count.to_string()),
                            ),
                            format!(
                                "Remote status refreshed: {} ahead and {} behind. Review it before syncing again.",
                                status.ahead.map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
                                status.behind.map_or_else(|| "unknown".to_owned(), |count| count.to_string()),
                            ),
                        ).to_string();
                        panel.sync_status = Some(status);
                        panel.sync_outcome_unknown = false;
                        panel.sync_result = None;
                        panel.connectivity.sync_run_result =
                            Some(kerminal_application::configuration_sync::ConfigurationSyncRunResult {
                                outcome: fetch.outcome,
                                summary,
                                stages: vec![fetch],
                            });
                        panel.sync_error = None;
                    }
                    Ok((fetch, None)) => {
                        panel.connectivity.sync_run_result =
                            Some(kerminal_application::configuration_sync::ConfigurationSyncRunResult {
                                outcome: fetch.outcome,
                                summary: fetch.summary.clone(),
                                stages: vec![fetch],
                            });
                    }
                    Err(error) => panel.sync_error = Some(super::localized_error(&error)),
                }
                cx.notify();
                let workspace_outcome_unknown = panel.sync_outcome_unknown;
                panel.connectivity.resume_pending_documents(
                    panel.vault_keys.clone(),
                    panel.configuration_sync.clone(),
                    workspace_outcome_unknown,
                    cx,
                );
            });
        })
        .detach();
    }
}

/// 首次建立面板时读取配置仓库与外部协议状态；仅空白工作区按既有用例初始化 Vault key。
///
/// @author kongweiguang
impl PreferencesPanel {
    /// 首次读取由密钥文档流程串行完成 readiness 检查和空白 workspace bootstrap，避免旧状态回包覆盖新 key。
    pub(super) fn load_integrations(&mut self, cx: &mut Context<Self>) {
        self.refresh_configuration_sync(cx);
        self.refresh_external_protocol(cx);
        let loading_documents = self.connectivity.load_documents(
            self.vault_keys.clone(),
            self.configuration_sync.clone(),
            false,
            false,
            cx,
        );
        if !loading_documents {
            self.refresh_vault_availability(cx);
        }
        self.connectivity.refresh_mcp(cx);
    }

    /// 每次重新打开设置时刷新运行态与工作区快照；在途或未知写入不会覆盖密钥草稿。
    ///
    /// Root 在 modal session 重置导航后调用此方法；密钥回读遇到在途操作会延后到回执后。
    ///
    /// @author kongweiguang
    pub(in crate::features::preferences) fn refresh_integrations_on_modal_open(
        &mut self,
        cx: &mut Context<Self>,
    ) {
        self.refresh_external_protocol(cx);
        self.refresh_configuration_sync(cx);
        self.connectivity.refresh_mcp(cx);
        let workspace_outcome_unknown = self.sync_outcome_unknown;
        let loading_documents = self.connectivity.load_documents(
            self.vault_keys.clone(),
            self.configuration_sync.clone(),
            true,
            workspace_outcome_unknown,
            cx,
        );
        if !loading_documents {
            self.refresh_vault_availability(cx);
        }
    }
}
