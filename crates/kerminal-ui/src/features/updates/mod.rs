//! 更新面板只提交 application 更新用例，并把安装与重启意图交还工作台根视图。
//!
//! @author kongweiguang

use std::sync::Arc;

use gpui_kit::{Context, EventEmitter};
use kerminal_application::{
    AppError, ApplicationTaskExecutor, PublicErrorCode,
    platform::{DesktopNotificationEvent, DesktopNotificationService, NotificationDisposition},
    updates::{
        DownloadedUpdate, DownloadedUpdateId, UpdateAvailability, UpdateCandidate,
        UpdateInstallDisposition, UpdateInstallResult, UpdateProgress, UpdateRecoveryId,
        UpdateRecoveryRecord, UpdateRecoveryState, UpdateService,
    },
};
use tokio_util::sync::CancellationToken;

mod about;
mod render;

/// 需要退出保护的安装和重启动作作为类型化意图交给 Root 处理。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpdatesEvent {
    InstallRequested {
        update_id: DownloadedUpdateId,
        version: String,
    },
    RetryInstallRequested {
        update_id: DownloadedUpdateId,
        version: String,
    },
    ExitAfterUpdateFailure {
        update_id: DownloadedUpdateId,
    },
}

#[derive(Clone, Debug)]
enum UpdatePanelState {
    Idle,
    Checking,
    NoUpdate,
    Available(UpdateCandidate),
    Downloading(UpdateCandidate),
    ReadyToInstall(DownloadedUpdate),
    AwaitingInstallGuard(DownloadedUpdate),
    Installing(DownloadedUpdate),
    Discarding(DownloadedUpdate),
    RestartPending(DownloadedUpdate),
    RestartFailed(DownloadedUpdate),
    HostRecoveryFailed {
        update: DownloadedUpdate,
        original_result: UpdateInstallResult,
        safe_message: String,
    },
    StoppedUpdateFailed {
        update: DownloadedUpdate,
        can_retry: bool,
    },
}

/// 仅保存服务返回的候选与已验签包句柄；网络、签名和安装逻辑都留在 application/infra。
pub struct UpdatesPanel {
    service: Option<UpdateService>,
    notifications: Option<Arc<DesktopNotificationService>>,
    executor: ApplicationTaskExecutor,
    state: UpdatePanelState,
    progress: UpdateProgress,
    status: Option<String>,
    status_is_error: bool,
    notification_error: Option<String>,
    pending_notification: Option<(u64, kerminal_application::updates::UpdateCandidateId)>,
    active_cancel: Option<CancellationToken>,
    cancel_requested: bool,
    check_generation: u64,
    active_check_generation: Option<u64>,
    recovery_records: Vec<UpdateRecoveryRecord>,
    recovery_loaded: bool,
    recovery_loading: bool,
    recovery_error: Option<String>,
    recovery_status: Option<String>,
    recovery_confirmation: Option<UpdateRecoveryId>,
    recovery_acknowledging: Option<UpdateRecoveryId>,
    about_license_expanded: bool,
}

impl EventEmitter<UpdatesEvent> for UpdatesPanel {}

impl UpdatesPanel {
    /// 只接收组合根提供的真实服务；None 明确呈现未配置状态且绝不伪造“已是最新版”。
    pub fn new(service: Option<UpdateService>, executor: ApplicationTaskExecutor) -> Self {
        Self::with_notifications(service, executor, None)
    }

    /// 接收组合根共享的通知 service，避免更新面板另建通知策略或绕过用户偏好。
    pub fn with_notifications(
        service: Option<UpdateService>,
        executor: ApplicationTaskExecutor,
        notifications: Option<Arc<DesktopNotificationService>>,
    ) -> Self {
        let (status, status_is_error) = match service.as_ref().map(UpdateService::availability) {
            Some(UpdateAvailability::Configured) => (None, false),
            Some(UpdateAvailability::Unavailable { reason }) => {
                (Some(localized_unavailable(reason)), true)
            }
            None => (
                Some(localized(
                    "更新服务尚未装配，恢复记录也无法读取。",
                    "The update service is not composed, so recovery records cannot be read.",
                )),
                true,
            ),
        };
        let recovery_loaded = service.is_none();
        Self {
            service,
            notifications,
            executor,
            state: UpdatePanelState::Idle,
            progress: UpdateProgress::default(),
            status,
            status_is_error,
            notification_error: None,
            pending_notification: None,
            active_cancel: None,
            cancel_requested: false,
            check_generation: 0,
            active_check_generation: None,
            recovery_records: Vec::new(),
            recovery_loaded,
            recovery_loading: false,
            recovery_error: None,
            recovery_status: None,
            recovery_confirmation: None,
            recovery_acknowledging: None,
            about_license_expanded: false,
        }
    }

    /// 首次进入面板时异步读取恢复记录；磁盘校验始终经 application executor 执行。
    pub fn load_recoveries_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.is_host_terminal() || self.recovery_loaded || self.recovery_loading {
            return;
        }
        let Some(service) = &self.service else {
            return;
        };
        let service = service.clone();
        let executor = self.executor.clone();
        self.recovery_loading = true;
        self.recovery_error = None;

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.recoveries().await })
                .await;
            let _ = this.update(cx, |panel, cx| panel.finish_recovery_load(result, cx));
        })
        .detach();
        cx.notify();
    }

    /// 读取失败时保留错误并提供显式重试，不把损坏或不可读恢复目录当作空列表。
    pub fn retry_recovery_load(&mut self, cx: &mut Context<Self>) {
        if self.is_host_terminal() || self.recovery_loading {
            return;
        }
        self.recovery_loaded = false;
        self.load_recoveries_if_needed(cx);
    }

    /// 选择单条恢复材料后再显示二次确认，防止列表点击直接删除已验签 artifact。
    pub fn request_recovery_acknowledgement(
        &mut self,
        recovery_id: UpdateRecoveryId,
        cx: &mut Context<Self>,
    ) {
        if self.is_host_terminal()
            || self.recovery_acknowledging.is_some()
            || !self
                .recovery_records
                .iter()
                .any(|record| record.id == recovery_id)
        {
            return;
        }
        self.recovery_confirmation = Some(recovery_id);
        self.recovery_status = None;
        cx.notify();
    }

    /// 用户取消二次确认时不触碰持久化记录或其 artifact。
    pub fn cancel_recovery_acknowledgement(
        &mut self,
        recovery_id: UpdateRecoveryId,
        cx: &mut Context<Self>,
    ) {
        if !self.is_host_terminal()
            && self.recovery_confirmation == Some(recovery_id)
            && self.recovery_acknowledging.is_none()
        {
            self.recovery_confirmation = None;
            cx.notify();
        }
    }

    /// 只有二次确认选中的恢复身份可进入 application 清理用例，操作不调用安装器。
    pub fn acknowledge_recovery(&mut self, recovery_id: UpdateRecoveryId, cx: &mut Context<Self>) {
        if self.is_host_terminal() {
            return;
        }
        if !can_acknowledge_recovery(
            self.recovery_confirmation,
            self.recovery_acknowledging,
            &self.recovery_records,
            recovery_id,
            self.is_host_terminal(),
        ) {
            return;
        }
        let Some(service) = &self.service else {
            return;
        };
        let service = service.clone();
        let executor = self.executor.clone();
        self.recovery_acknowledging = Some(recovery_id);
        self.recovery_status = Some(localized(
            "正在清理已确认的恢复材料…",
            "Clearing the confirmed recovery material…",
        ));

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.acknowledge_recovery(recovery_id).await })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_recovery_acknowledgement(recovery_id, result, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 只更新持久化读取结果；失败记录仍可见为错误并允许再次读取。
    fn finish_recovery_load(
        &mut self,
        result: kerminal_application::AppResult<Vec<UpdateRecoveryRecord>>,
        cx: &mut Context<Self>,
    ) {
        self.recovery_loading = false;
        match result {
            Ok(records) => {
                self.recovery_records = records;
                self.recovery_loaded = true;
                self.recovery_error = None;
            }
            Err(error) => {
                self.recovery_loaded = false;
                self.recovery_error = Some(localized_error(&error));
            }
        }
        cx.notify();
    }

    /// 成功清理只移除所选卡片；失败保留记录、确认上下文和可读错误供用户处理。
    fn finish_recovery_acknowledgement(
        &mut self,
        recovery_id: UpdateRecoveryId,
        result: kerminal_application::AppResult<()>,
        cx: &mut Context<Self>,
    ) {
        self.recovery_acknowledging = None;
        match result {
            Ok(()) => {
                self.recovery_records
                    .retain(|record| record.id != recovery_id);
                self.recovery_confirmation = None;
                self.recovery_status = Some(localized(
                    "已清理所选恢复记录；没有重放安装。",
                    "The selected recovery record was cleared; no installation was replayed.",
                ));
                self.recovery_error = None;
            }
            Err(error) => {
                self.recovery_status = Some(localized_error(&error));
            }
        }
        cx.notify();
    }

    /// 每轮检查分配独立代次并沿用共享执行器，避免取消或迟到结果触发重复通知。
    pub fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        let Some(service) = &self.service else {
            return;
        };
        if service.availability() != UpdateAvailability::Configured {
            return;
        }
        if self.is_busy() {
            return;
        }

        let service = service.clone();
        let executor = self.executor.clone();
        let user_cancel = CancellationToken::new();
        let completion_cancel = user_cancel.clone();
        self.check_generation = self.check_generation.wrapping_add(1);
        let check_generation = self.check_generation;
        self.active_check_generation = Some(check_generation);
        self.active_cancel = Some(user_cancel.clone());
        self.cancel_requested = false;
        self.notification_error = None;
        self.pending_notification = None;
        self.status = Some(localized("正在检查更新…", "Checking for updates…"));
        self.status_is_error = false;
        self.state = UpdatePanelState::Checking;

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |application_cancel| async move {
                    service
                        .check_with_cancellation(user_cancel, application_cancel)
                        .await
                })
                .await;
            let was_cancelled = completion_cancel.is_cancelled();
            let _ = this.update(cx, |panel, cx| {
                panel.finish_check(result, was_cancelled, check_generation, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 开始下载一个明确候选；进度由 application subscription 合并，UI 不依赖 Tokio 通道。
    pub fn download_update(&mut self, cx: &mut Context<Self>) {
        let Some(service) = &self.service else {
            return;
        };
        if service.availability() != UpdateAvailability::Configured {
            return;
        }
        let UpdatePanelState::Available(candidate) = &self.state else {
            return;
        };

        let candidate = candidate.clone();
        let service = service.clone();
        let executor = self.executor.clone();
        let user_cancel = CancellationToken::new();
        let operation_cancel = user_cancel.clone();
        let completion_cancel = user_cancel.clone();
        let (reporter, subscription) = UpdateService::progress_channel();
        self.active_cancel = Some(user_cancel.clone());
        self.cancel_requested = false;
        self.progress = UpdateProgress::default();
        self.status = Some(localized(
            "正在下载并验证更新包…",
            "Downloading and verifying the update…",
        ));
        self.status_is_error = false;
        self.state = UpdatePanelState::Downloading(candidate.clone());

        let progress_executor = executor.clone();
        let candidate_id = candidate.id;
        cx.spawn(async move |this, cx| {
            loop {
                let subscription = subscription.clone();
                let next = progress_executor
                    .run(move |cancel| async move { Ok(subscription.next(cancel).await) })
                    .await;
                let Ok(Some(progress)) = next else {
                    break;
                };
                let _ = this.update(cx, |panel, cx| {
                    panel.set_download_progress(candidate_id, progress, cx);
                });
            }
        })
        .detach();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |application_cancel| async move {
                    service
                        .download_with_cancellation(
                            candidate.id,
                            operation_cancel,
                            application_cancel,
                            reporter,
                        )
                        .await
                })
                .await;
            let was_cancelled = completion_cancel.is_cancelled();
            let _ = this.update(cx, |panel, cx| {
                panel.finish_download(candidate, result, was_cancelled, cx);
            });
        })
        .detach();
        cx.notify();
    }

    /// 取消检查或下载并保留运行态直到适配器完成有界收尾，避免 UI 报已取消而后台仍无人管理。
    pub fn cancel_active_operation(&mut self, cx: &mut Context<Self>) {
        let Some(cancel) = &self.active_cancel else {
            return;
        };
        if self.cancel_requested {
            return;
        }
        cancel.cancel();
        self.cancel_requested = true;
        self.status = Some(localized(
            "正在取消；等待当前请求收尾并释放资源…",
            "Cancelling; waiting for the current request to clean up…",
        ));
        self.status_is_error = false;
        cx.notify();
    }

    /// 安装必须先向 Root 请求 dirty-editor 退出保护，不在面板内直接调用平台安装器。
    pub fn request_install(&mut self, cx: &mut Context<Self>) {
        let UpdatePanelState::ReadyToInstall(update) = &self.state else {
            return;
        };
        let update = update.clone();
        self.state = UpdatePanelState::AwaitingInstallGuard(update.clone());
        self.status = Some(localized(
            "正在等待工作区检查未保存内容…",
            "Waiting for the workspace unsaved-changes check…",
        ));
        self.status_is_error = false;
        cx.emit(UpdatesEvent::InstallRequested {
            update_id: update.id,
            version: update.version,
        });
        cx.notify();
    }

    /// 用户在 Root 脏状态保护中取消时退回可安装状态，保留已验签句柄供后续处理。
    pub fn cancel_install_guard(&mut self, update_id: DownloadedUpdateId, cx: &mut Context<Self>) {
        let UpdatePanelState::AwaitingInstallGuard(update) = &self.state else {
            return;
        };
        if update.id != update_id {
            return;
        }
        self.state = UpdatePanelState::ReadyToInstall(update.clone());
        self.status = Some(localized(
            "安装请求已取消，已下载的更新包仍可稍后处理。",
            "Installation was cancelled. The downloaded update is still available.",
        ));
        self.status_is_error = false;
        cx.notify();
    }

    /// 仅在 Root 完成编辑器保护和 workspace 保存后进入 desktop 安装状态，不调用安装器。
    pub fn begin_host_install(
        &mut self,
        update_id: DownloadedUpdateId,
        cx: &mut Context<Self>,
    ) -> bool {
        let UpdatePanelState::AwaitingInstallGuard(update) = &self.state else {
            return false;
        };
        if update.id != update_id {
            return false;
        }
        let update = update.clone();
        if self.service.is_none() {
            self.state = UpdatePanelState::ReadyToInstall(update);
            self.status = Some(localized(
                "更新服务当前不可用。",
                "The update service is currently unavailable.",
            ));
            self.status_is_error = true;
            cx.notify();
            return false;
        }
        self.state = UpdatePanelState::Installing(update.clone());
        self.status = Some(localized(
            "已通过退出保护，正在关闭会话并准备安装…",
            "Exit checks passed. Closing sessions and preparing installation…",
        ));
        self.status_is_error = false;
        cx.notify();
        true
    }

    /// desktop 完成 quiesce、安装及必要重启后回报结果；失败状态明确标记后台服务已停止。
    pub fn finish_host_install(
        &mut self,
        update_id: DownloadedUpdateId,
        result: UpdateInstallResult,
        cx: &mut Context<Self>,
    ) {
        if !listener_recovery_failure_matches(&self.state, update_id) {
            return;
        }
        let UpdatePanelState::Installing(update) = &self.state else {
            return;
        };
        let update = update.clone();
        let can_retry = result.can_retry();
        match result {
            UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired) => {
                self.state = UpdatePanelState::RestartPending(update);
                self.status = Some(localized("应用正在重新启动。", "Kerminal is restarting."));
                self.status_is_error = false;
            }
            UpdateInstallResult::NotStarted(error) | UpdateInstallResult::FailedNoEffect(error) => {
                self.state = UpdatePanelState::StoppedUpdateFailed { update, can_retry };
                self.status = Some(format!(
                    "{} {}",
                    localized_error(&error),
                    localized(
                        "应用后台服务已停止；已验签安装包仍保留，可安全重试或退出后手动恢复。",
                        "Application services are stopped. The verified package is retained; retry safely or exit for manual recovery."
                    )
                ));
                self.status_is_error = true;
            }
            UpdateInstallResult::OutcomeUnknown(error) => {
                self.state = UpdatePanelState::StoppedUpdateFailed {
                    update,
                    can_retry: false,
                };
                self.status = Some(format!(
                    "{} {}",
                    localized_error(&error),
                    localized(
                        "应用后台服务已停止；更新结果未知，已保留诊断材料。请退出并人工核实安装状态，不要重放此更新。",
                        "Application services are stopped. The update result is unknown and diagnostic material is retained. Exit and verify the installation manually; do not replay this update."
                    )
                ));
                self.status_is_error = true;
            }
        }
        cx.notify();
    }

    /// 安装已完成但新进程启动失败时绑定原下载身份，禁止误报成安装失败或安全重试。
    pub fn report_host_restart_failure(
        &mut self,
        update_id: DownloadedUpdateId,
        safe_message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        if !restart_failure_matches(&self.state, update_id) {
            return;
        }
        let UpdatePanelState::RestartPending(update) = &self.state else {
            return;
        };
        self.state = UpdatePanelState::RestartFailed(update.clone());
        self.status = Some(format!(
            "{} {}",
            safe_message.into(),
            localized(
                "更新已安装，请手动重新打开应用。",
                "The update is installed. Please reopen the application manually."
            )
        ));
        self.status_is_error = true;
        cx.notify();
    }

    /// Windows 安装器保持原进程后若 listener 无法恢复，保留安装回执并关闭同句柄重试入口。
    pub fn report_host_listener_recovery_failure(
        &mut self,
        update_id: DownloadedUpdateId,
        original_result: UpdateInstallResult,
        safe_message: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        let UpdatePanelState::Installing(update) = &self.state else {
            return;
        };
        if update.id != update_id {
            return;
        }
        let update = update.clone();
        self.finish_host_install(update_id, original_result.clone(), cx);
        self.state = UpdatePanelState::HostRecoveryFailed {
            update,
            original_result,
            safe_message: safe_message.into(),
        };
        self.status = Some(localized(
            "单实例监听器未能恢复。请退出并人工重新打开应用。",
            "The single-instance listener could not be restored. Exit and reopen the application manually.",
        ));
        self.status_is_error = true;
        cx.notify();
    }

    /// 只允许对端口证明无副作用的失败复用原句柄；重试直达已quiesce的桌面宿主。
    pub fn retry_host_install(&mut self, cx: &mut Context<Self>) {
        if !can_retry_host_install(&self.state) {
            return;
        }
        let UpdatePanelState::StoppedUpdateFailed { update, .. } = &self.state else {
            return;
        };
        let update = update.clone();
        self.state = UpdatePanelState::Installing(update.clone());
        self.status = Some(localized(
            "后台服务已停止，正在请求安全重试…",
            "Application services are stopped. Requesting a safe retry…",
        ));
        self.status_is_error = false;
        cx.emit(UpdatesEvent::RetryInstallRequested {
            update_id: update.id,
            version: update.version,
        });
        cx.notify();
    }

    /// 结果未知时只允许退出人工处理，避免用户误把不确定提交再次发送给安装器。
    pub fn exit_after_update_failure(&mut self, cx: &mut Context<Self>) {
        if !can_exit_after_install_failure(&self.state) {
            return;
        }
        let update = match &self.state {
            UpdatePanelState::StoppedUpdateFailed { update, .. }
            | UpdatePanelState::HostRecoveryFailed { update, .. } => update,
            _ => return,
        };
        cx.emit(UpdatesEvent::ExitAfterUpdateFailure {
            update_id: update.id,
        });
        cx.notify();
    }

    /// 释放已验签暂存包；失败时保留可见状态并允许用户重试丢弃或重新请求安装。
    pub fn discard_download(&mut self, cx: &mut Context<Self>) {
        let UpdatePanelState::ReadyToInstall(update) = &self.state else {
            return;
        };
        let update = update.clone();
        let Some(service) = &self.service else {
            return;
        };
        let service = service.clone();
        let executor = self.executor.clone();
        self.state = UpdatePanelState::Discarding(update.clone());
        self.status = Some(localized(
            "正在释放暂存更新包…",
            "Discarding the staged update…",
        ));
        self.status_is_error = false;

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { service.discard(update.id).await })
                .await;
            let _ = this.update(cx, |panel, cx| panel.finish_discard(update, result, cx));
        })
        .detach();
        cx.notify();
    }

    /// 提供 busy 检查以防 GPUI 连续点击重复提交同一个 updater transition。
    fn is_busy(&self) -> bool {
        matches!(
            &self.state,
            UpdatePanelState::Checking
                | UpdatePanelState::Downloading(_)
                | UpdatePanelState::AwaitingInstallGuard(_)
                | UpdatePanelState::Installing(_)
                | UpdatePanelState::Discarding(_)
        )
    }

    /// desktop 已接管安装或停止后台服务后禁止再次提交需要 application executor 的操作。
    fn is_host_terminal(&self) -> bool {
        matches!(
            &self.state,
            UpdatePanelState::RestartPending(_)
                | UpdatePanelState::RestartFailed(_)
                | UpdatePanelState::HostRecoveryFailed { .. }
                | UpdatePanelState::StoppedUpdateFailed { .. }
        )
    }

    /// 只消费匹配代次一次；通知发送失败单独记录，不回滚成功候选或改写检查结果。
    fn finish_check(
        &mut self,
        result: kerminal_application::AppResult<Option<UpdateCandidate>>,
        was_cancelled: bool,
        check_generation: u64,
        cx: &mut Context<Self>,
    ) {
        if !take_check_operation(
            &mut self.active_check_generation,
            &self.state,
            check_generation,
        ) {
            return;
        }

        let notification_candidate = notification_candidate_for_check(&result, was_cancelled);
        self.active_cancel = None;
        self.cancel_requested = false;
        match result {
            Ok(Some(candidate)) => {
                self.state = UpdatePanelState::Available(candidate);
                self.status = Some(localized("发现可用更新。", "An update is available."));
                self.status_is_error = false;
            }
            Ok(None) => {
                self.state = UpdatePanelState::NoUpdate;
                self.status = Some(localized("当前已是最新版本。", "Kerminal is up to date."));
                self.status_is_error = false;
            }
            Err(error) if was_cancelled && error.public_code() == PublicErrorCode::Cancelled => {
                self.state = UpdatePanelState::Idle;
                self.status = Some(localized("更新检查已取消。", "Update check cancelled."));
                self.status_is_error = false;
            }
            Err(error) => {
                self.state = UpdatePanelState::Idle;
                self.status = Some(localized_error(&error));
                self.status_is_error = true;
            }
        }
        if let Some(candidate) = notification_candidate {
            self.send_update_available_notification(candidate, check_generation, cx);
        }
        cx.notify();
    }

    /// 将通知投递到应用共享 runtime；OS adapter 的失败只写单独诊断，不改候选或检查结果。
    fn send_update_available_notification(
        &mut self,
        candidate: UpdateCandidate,
        check_generation: u64,
        cx: &mut Context<Self>,
    ) {
        let Some(notifications) = self.notifications.clone() else {
            return;
        };
        let executor = self.executor.clone();
        let candidate_id = candidate.id;
        self.pending_notification = Some((check_generation, candidate_id));
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move {
                    notify_update_available(&notifications, &candidate.version).await
                })
                .await;
            let _ = this.update(cx, |panel, cx| {
                panel.finish_update_notification(check_generation, candidate_id, result, cx);
            });
        })
        .detach();
    }

    /// 保留通知失败的安全公开文案，同时确保迟到通知结果不影响另一条候选。
    fn finish_update_notification(
        &mut self,
        check_generation: u64,
        candidate_id: kerminal_application::updates::UpdateCandidateId,
        result: kerminal_application::AppResult<NotificationDisposition>,
        cx: &mut Context<Self>,
    ) {
        let pending = self.pending_notification;
        if pending != Some((check_generation, candidate_id)) {
            return;
        }
        self.pending_notification = None;
        if let Some(message) =
            update_notification_diagnostic(pending, check_generation, candidate_id, &result)
        {
            self.notification_error = Some(format!(
                "{}：{message}",
                localized(
                    "系统通知发送失败，更新仍可在此面板查看",
                    "System notification failed; the update remains available in this panel"
                )
            ));
            cx.notify();
        }
    }

    /// 只更新仍对应同一候选的下载进度，忽略过期订阅通知。
    fn set_download_progress(
        &mut self,
        candidate_id: kerminal_application::updates::UpdateCandidateId,
        progress: UpdateProgress,
        cx: &mut Context<Self>,
    ) {
        if matches!(&self.state, UpdatePanelState::Downloading(candidate) if candidate.id == candidate_id)
        {
            self.progress = progress;
            cx.notify();
        }
    }

    /// 下载完成后只把经过 application/infra 验签确认的 DownloadedUpdate 暴露给安装与丢弃按钮。
    fn finish_download(
        &mut self,
        candidate: UpdateCandidate,
        result: kerminal_application::AppResult<DownloadedUpdate>,
        was_cancelled: bool,
        cx: &mut Context<Self>,
    ) {
        self.active_cancel = None;
        self.cancel_requested = false;
        self.progress = UpdateProgress::default();
        match result {
            Ok(update) => {
                self.state = UpdatePanelState::ReadyToInstall(update);
                self.status = Some(localized(
                    "更新包已下载并通过签名验证。",
                    "The update was downloaded and its signature verified.",
                ));
                self.status_is_error = false;
            }
            Err(error) if was_cancelled && error.public_code() == PublicErrorCode::Cancelled => {
                self.state = UpdatePanelState::Available(candidate);
                self.status = Some(localized(
                    "下载已取消，可以重新下载。",
                    "Download cancelled. You can try again.",
                ));
                self.status_is_error = false;
            }
            Err(error) => {
                self.state = UpdatePanelState::Available(candidate);
                self.status = Some(localized_error(&error));
                self.status_is_error = true;
            }
        }
        cx.notify();
    }

    /// 只有释放成功才清除包句柄；失败结果保留可重试引用。
    fn finish_discard(
        &mut self,
        update: DownloadedUpdate,
        result: kerminal_application::AppResult<()>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(()) => {
                self.state = UpdatePanelState::Idle;
                self.status = Some(localized(
                    "暂存更新包已释放。",
                    "The staged update was discarded.",
                ));
                self.status_is_error = false;
            }
            Err(error) => {
                self.state = UpdatePanelState::ReadyToInstall(update);
                self.status = Some(localized_error(&error));
                self.status_is_error = true;
            }
        }
        cx.notify();
    }
}

impl Drop for UpdatesPanel {
    /// 面板关闭时只协作取消检查或下载；共享 executor 仍负责监督后台任务收尾。
    fn drop(&mut self) {
        if let Some(cancel) = &self.active_cancel {
            cancel.cancel();
        }
    }
}

/// 通过工作台共用 locale 选择更新界面的双语文案，不在 feature 内探测系统语言。
fn localized(zh_cn: &str, en_us: &str) -> String {
    crate::locale::text(zh_cn, en_us).to_string()
}

/// 把 adapter 的固定不可用原因映射成当前界面的双语提示，不把源地址或密钥写进状态。
fn localized_unavailable(reason: &'static str) -> String {
    match reason {
        "此构建尚未配置更新源和签名公钥" => localized(
            "此构建尚未配置更新源和签名公钥；恢复记录仍可查看。",
            "This build has no update feed or signing key; recovery records remain available.",
        ),
        "当前安装形态不支持应用内自动更新" => localized(
            "当前安装形态不支持应用内更新；可通过系统包管理器更新。",
            "In-app updates are not supported for this installation type.",
        ),
        "当前系统无法确定更新目标" => localized(
            "当前系统无法确定更新目标；恢复记录仍可查看。",
            "The updater cannot determine this platform target; recovery records remain available.",
        ),
        _ => localized(
            "当前更新能力不可用；恢复记录仍可查看。",
            "Updates are currently unavailable; recovery records remain available.",
        ),
    }
}

/// 让恢复状态的文案明确要求人工核实，不暗示 Unknown 可安全重试或自动恢复。
fn recovery_state_label(state: UpdateRecoveryState) -> String {
    match state {
        UpdateRecoveryState::PendingConfirmation => {
            localized("等待人工核实", "Awaiting manual review")
        }
        UpdateRecoveryState::OutcomeUnknown => {
            localized("安装结果未知", "Installation result unknown")
        }
        UpdateRecoveryState::VersionMatchedAwaitingConfirmation => localized(
            "版本已匹配，仍需确认",
            "Version matches; confirmation required",
        ),
    }
}

/// 只允许当前 checking 代次完成一次；迟到或重复回调不能再发出通知任务。
fn take_check_operation(
    active_generation: &mut Option<u64>,
    state: &UpdatePanelState,
    requested_generation: u64,
) -> bool {
    if *active_generation != Some(requested_generation)
        || !matches!(state, UpdatePanelState::Checking)
    {
        return false;
    }
    *active_generation = None;
    true
}

/// 只有未取消且真实返回候选的检查才可产生一次更新提醒。
fn notification_candidate_for_check(
    result: &kerminal_application::AppResult<Option<UpdateCandidate>>,
    was_cancelled: bool,
) -> Option<UpdateCandidate> {
    if was_cancelled {
        return None;
    }
    result.as_ref().ok()?.as_ref().cloned()
}

/// 使用通知 service 自己的窗口状态和策略，不在 UI 复制设置过滤或阻塞系统调用。
async fn notify_update_available(
    notifications: &DesktopNotificationService,
    version: &str,
) -> kerminal_application::AppResult<NotificationDisposition> {
    let visibility = notifications.window_visibility()?;
    notifications
        .notify(
            DesktopNotificationEvent::UpdateAvailable {
                version: version.to_owned(),
                current_version: None,
                importance: None,
            },
            visibility,
        )
        .await
}

/// 只为仍显示的同一候选保留通知错误；检查结果和候选状态保持只读不变。
fn update_notification_diagnostic(
    pending: Option<(u64, kerminal_application::updates::UpdateCandidateId)>,
    requested_generation: u64,
    candidate_id: kerminal_application::updates::UpdateCandidateId,
    result: &kerminal_application::AppResult<NotificationDisposition>,
) -> Option<String> {
    if pending != Some((requested_generation, candidate_id)) {
        return None;
    }
    result
        .as_ref()
        .err()
        .map(|error| error.public_message().to_owned())
}

/// 清理入口只接受当前二次确认身份，且不允许并发清理或对已消失记录重放请求。
fn can_acknowledge_recovery(
    confirmation: Option<UpdateRecoveryId>,
    acknowledging: Option<UpdateRecoveryId>,
    records: &[UpdateRecoveryRecord],
    requested: UpdateRecoveryId,
    host_terminal: bool,
) -> bool {
    !host_terminal
        && confirmation == Some(requested)
        && acknowledging.is_none()
        && records.iter().any(|record| record.id == requested)
}

/// 只接收当前正在重启的下载身份，忽略宿主迟到或重复的旧安装回执。
fn restart_failure_matches(state: &UpdatePanelState, requested: DownloadedUpdateId) -> bool {
    matches!(state, UpdatePanelState::RestartPending(update) if update.id == requested)
}

/// listener 失效回执只允许覆盖同一正在处理的安装，不接收迟到或无关回执。
fn listener_recovery_failure_matches(
    state: &UpdatePanelState,
    requested: DownloadedUpdateId,
) -> bool {
    matches!(state, UpdatePanelState::Installing(update) if update.id == requested)
}

/// 只对明确无副作用安装失败开放同一已验签句柄的重试。
fn can_retry_host_install(state: &UpdatePanelState) -> bool {
    matches!(
        state,
        UpdatePanelState::StoppedUpdateFailed {
            can_retry: true,
            ..
        }
    )
}

/// Root 退出门禁只应从失败状态接受退出请求，其他状态不暴露失败恢复动作。
fn can_exit_after_install_failure(state: &UpdatePanelState) -> bool {
    matches!(
        state,
        UpdatePanelState::StoppedUpdateFailed { .. } | UpdatePanelState::HostRecoveryFailed { .. }
    )
}

/// 把稳定错误类别映射成更新场景的双语安全提示，不展示底层 URL 或路径。
fn localized_error(error: &AppError) -> String {
    let (zh_cn, en_us) = match error.public_code() {
        PublicErrorCode::Invalid => ("更新请求无效。", "The update request is invalid."),
        PublicErrorCode::NotFound => (
            "更新包已不存在，请重新检查。",
            "The update package is no longer available. Check again.",
        ),
        PublicErrorCode::Unsupported => (
            "当前更新方式不受支持。",
            "This update method is not supported.",
        ),
        PublicErrorCode::Conflict => (
            "更新状态已变化，请重新检查。",
            "The update state changed. Check again.",
        ),
        PublicErrorCode::Cancelled => ("更新操作已取消。", "The update operation was cancelled."),
        PublicErrorCode::Timeout => (
            "更新操作超时，请重试。",
            "The update operation timed out. Try again.",
        ),
        _ => (
            "更新操作失败，请重试。",
            "The update operation failed. Try again.",
        ),
    };
    localized(zh_cn, en_us)
}

/// 在 listener 恢复失败后保留原安装分类，避免安全提示遮住安装器的实际回执。
fn install_result_summary(result: &UpdateInstallResult) -> String {
    match result {
        UpdateInstallResult::Installed(_) => localized(
            "安装器报告更新已安装。",
            "The installer reported the update as installed.",
        ),
        UpdateInstallResult::NotStarted(error) => format!(
            "{} {}",
            localized("安装未启动。", "Installation did not start."),
            localized_error(error)
        ),
        UpdateInstallResult::FailedNoEffect(error) => format!(
            "{} {}",
            localized(
                "安装失败且未更改目标文件。",
                "Installation failed without changing the target."
            ),
            localized_error(error)
        ),
        UpdateInstallResult::OutcomeUnknown(error) => format!(
            "{} {}",
            localized("安装结果未知。", "The installation result is unknown."),
            localized_error(error)
        ),
    }
}

/// 把字节数投影为界面摘要，不依赖包大小一定可由服务端提供。
fn format_progress(progress: UpdateProgress) -> String {
    match progress.total_bytes {
        Some(total) if total > 0 => {
            let percent = progress
                .received_bytes
                .saturating_mul(100)
                .checked_div(total)
                .unwrap_or(0)
                .min(100);
            format!(
                "{percent}% · {} / {}",
                format_bytes(progress.received_bytes),
                format_bytes(total)
            )
        }
        _ => format!(
            "{} {}",
            localized("已接收", "Received"),
            format_bytes(progress.received_bytes)
        ),
    }
}

/// 使用人类可读的二进制单位显示进度，避免在界面暴露不必要的原始 byte 数。
fn format_bytes(value: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    match value {
        value if value >= GIB => format!("{:.1} GiB", value as f64 / GIB as f64),
        value if value >= MIB => format!("{:.1} MiB", value as f64 / MIB as f64),
        value if value >= KIB => format!("{:.1} KiB", value as f64 / KIB as f64),
        value => format!("{value} B"),
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/updates.rs"]
mod tests;
