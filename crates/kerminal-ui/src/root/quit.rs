//! Root 退出与更新重启事务；桌面组合根仍负责实际资源关闭。
//!
//! @author kongweiguang

use gpui_kit::{
    AnyElement, ClickEvent, Context, IntoElement, SharedString, Window,
    component::{Disableable as _, button::Button},
    div,
    prelude::*,
    px,
};
use kerminal_application::{
    AppError,
    updates::{DownloadedUpdateId, UpdateInstallDisposition, UpdateInstallResult},
};

use super::updates::{GuardRequestResult, UpdateInstallRequest};
use super::{RootView, terminal_open};

/// 桌面退出或更新重启的意图；RootView 统一完成草稿和编辑文件的脏状态保护。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QuitIntent {
    Exit,
    RestartForUpdate { version: String },
}

/// 根窗口仅在全部用户草稿与文件编辑器门禁完成后向桌面组合根报告。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RootViewEvent {
    QuitReady {
        intent: QuitIntent,
    },
    InstallReady {
        update_id: DownloadedUpdateId,
        version: String,
    },
    RetryInstallReady {
        update_id: DownloadedUpdateId,
        version: String,
    },
}

/// 根窗口的服务生命周期；进入停止失败后只允许查询、清理和安全重试。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootHostLifecycle {
    Running,
    Stopping,
    StopFailed,
    StoppedUpdateFailed,
    OutcomeUnknown,
    RestartFailed,
    ListenerRecoveryFailed,
    Stopped,
}

/// 只有尚未派发收尾的 Running 可以从普通回执继续门禁；Stopping 由原 workspace CAS owner 驱动。
///
/// @author kongweiguang
pub(super) fn quit_preparation_is_running(
    lifecycle: RootHostLifecycle,
    host_shutdown_requested: bool,
) -> bool {
    lifecycle == RootHostLifecycle::Running && !host_shutdown_requested
}

/// Stopping 在宿主派发前表示原 CAS 仍拥有准备流程，任何公共 emit 入口都只能等待其回执。
///
/// @author kongweiguang
fn workspace_quit_preparation_in_flight(
    lifecycle: RootHostLifecycle,
    host_shutdown_requested: bool,
) -> bool {
    lifecycle == RootHostLifecycle::Stopping && !host_shutdown_requested
}

/// 只投影既有生命周期；收尾请求事实与失败正文不构成第二套业务状态机。
struct HostShutdownPresentation {
    message: SharedString,
    managed_tasks: usize,
    cleanup_note: SharedString,
    retry_exit: bool,
}

/// 已派发的宿主收尾可能部分完成，退出重试必须复用既有路径，不能恢复普通服务。
///
/// @author kongweiguang
fn can_retry_exit(lifecycle: RootHostLifecycle) -> bool {
    matches!(
        lifecycle,
        RootHostLifecycle::StopFailed
            | RootHostLifecycle::StoppedUpdateFailed
            | RootHostLifecycle::OutcomeUnknown
            | RootHostLifecycle::RestartFailed
            | RootHostLifecycle::ListenerRecoveryFailed
    )
}

/// 仅投影既有确定失败的原更新身份；真正重试仍交面板和 Root 的双重 typed 门禁判断。
///
/// @author kongweiguang
fn retryable_update_request(
    lifecycle: RootHostLifecycle,
    request: Option<&UpdateInstallRequest>,
) -> Option<&UpdateInstallRequest> {
    request.filter(|_| {
        matches!(
            lifecycle,
            RootHostLifecycle::StopFailed | RootHostLifecycle::StoppedUpdateFailed
        )
    })
}

/// Stopping 在 CAS 准备阶段仍可取消，只有实际派发收尾后才替换工作台，且普通 status 永不参与原错投影。
///
/// @author kongweiguang
fn host_shutdown_presentation(
    requested: bool,
    lifecycle: RootHostLifecycle,
    failure: Option<&SharedString>,
    managed_tasks: usize,
) -> Option<HostShutdownPresentation> {
    if !requested {
        return None;
    }
    let retry_exit = can_retry_exit(lifecycle);
    let message = failure.cloned().unwrap_or_else(|| {
        if retry_exit {
            crate::locale::text(
                "应用服务已停止或未能完成收尾。请重试退出后重新打开应用。",
                "Application services have stopped or cleanup is incomplete. Retry quitting, then reopen the application.",
            )
        } else {
            crate::locale::text(
                "正在收尾应用服务，请等待退出完成。",
                "Application services are stopping. Please wait for cleanup to finish.",
            )
        }
    });
    Some(HostShutdownPresentation {
        message,
        managed_tasks,
        cleanup_note: crate::locale::text(
            "任务归零不代表宿主资源已全部清理；仍需等待宿主确认会话、监听器和服务收尾。",
            "Zero managed tasks does not prove host resources are fully cleaned; wait for the host to confirm sessions, listeners, and services.",
        ),
        retry_exit,
    })
}

/// 已交给 host 或明确准备失败的退出不能再隐藏；普通运行和用户取消仍使用原托盘策略。
///
/// @author kongweiguang
fn window_close_policy(
    requested: bool,
    tray_owner_installed: bool,
    preparation_blocked: bool,
) -> kerminal_application::platform::window_state::ClosePolicy {
    use kerminal_application::platform::window_state::{ClosePolicy, close_policy};
    if requested || preparation_blocked {
        ClosePolicy::RequestQuit
    } else {
        close_policy(tray_owner_installed)
    }
}

/// 准备失败只阻止隐藏并保留原错，不表示 host 已停止，也不持有可被迟到回执重放的退出意图。
/// 成功结果清空提示，实际派发仍须走原有确认和宿主门禁。
///
/// @author kongweiguang
fn workspace_quit_preparation_failure(
    result: &terminal_open::WorkspaceSnapshotSaveCompletion,
) -> Option<SharedString> {
    Some(match result {
        terminal_open::WorkspaceSnapshotSaveCompletion::Conflict => crate::locale::text(
            "工作区与另一窗口的修改冲突，退出或更新已取消。",
            "Workspace changes conflict with another window; exit or update was cancelled.",
        ),
        terminal_open::WorkspaceSnapshotSaveCompletion::ReadbackConfirmedUncommitted => {
            crate::locale::text(
                "工作区写入尚未提交，退出或更新已取消。",
                "Workspace changes were not committed; exit or update was cancelled.",
            )
        }
        terminal_open::WorkspaceSnapshotSaveCompletion::OutcomeUnknown => crate::locale::text(
            "无法确认工作区保存结果，退出或更新已取消。",
            "The workspace save result is unknown; exit or update was cancelled.",
        ),
        terminal_open::WorkspaceSnapshotSaveCompletion::Busy => crate::locale::text(
            "工作区仍有操作未完成，退出或更新已取消。",
            "Workspace operations are still pending; exit or update was cancelled.",
        ),
        terminal_open::WorkspaceSnapshotSaveCompletion::Failed(message) => message.clone(),
        terminal_open::WorkspaceSnapshotSaveCompletion::NoChanges
        | terminal_open::WorkspaceSnapshotSaveCompletion::Saved => return None,
    })
}

impl RootView {
    /// 拒绝本次退出而不是取消原写入：草稿、CAS 与未知结果仍由连接面板持有，处理后须由用户重新退出。
    /// 不创建自动续跑的退出事务，防止迟到保存回执突然关闭用户正在操作的窗口。
    ///
    /// @author kongweiguang
    fn cancel_quit_for_connection_changes(&mut self, cx: &mut Context<Self>) -> bool {
        let reason = self
            .connections_panel
            .as_ref()
            .and_then(|panel| panel.read(cx).quit_block_reason(cx));
        let Some(reason) = reason else {
            return false;
        };
        self.pending_quit_intent = None;
        self.status = Some(reason.into());
        cx.notify();
        true
    }

    /// 布局真正在途时保留退出意图等收据；主机写入或草稿须先处理，不能以关闭窗口丢弃结果核对所有权。
    /// 已提交收尾的失败终态只走既有退出重试，不重新调用已停服务。
    ///
    /// @author kongweiguang
    pub fn request_quit(&mut self, intent: QuitIntent, cx: &mut Context<Self>) -> bool {
        if self.custom_agent_manager.is_some() {
            self.request_close_custom_agent_manager(cx);
            self.status = Some(
                if self
                    .custom_agent_manager
                    .as_ref()
                    .is_some_and(|manager| manager.read(cx).is_busy())
                {
                    "请等待自定义 Agent 保存或读回完成，再退出"
                } else {
                    "请先处理自定义 Agent 草稿，再退出"
                }
                .into(),
            );
            cx.notify();
            return false;
        }
        if self.quit_ready_emitted || self.pending_quit_intent.is_some() {
            return false;
        }
        if self.host_lifecycle != RootHostLifecycle::Running {
            if can_retry_exit(self.host_lifecycle) && intent == QuitIntent::Exit {
                self.pending_quit_intent = Some(QuitIntent::Exit);
                self.emit_quit_ready(cx);
                return true;
            }
            return false;
        }
        self.failed_quit_intent = None;
        if self.cancel_quit_for_connection_changes(cx) {
            return false;
        }
        self.pending_quit_intent = Some(intent);
        let preferences = self.preferences_panel.clone();
        if preferences.read(cx).is_workbench_layout_save_in_flight() {
            cx.notify();
            return false;
        }
        if !preferences.update(cx, |panel, cx| panel.request_quit(cx)) {
            return false;
        }
        self.continue_quit_after_preferences(cx)
    }

    /// 失败保留不可逆的收尾事实和独立原错，普通任务迟到状态不能掩盖重试退出入口。
    ///
    /// @author kongweiguang
    pub fn report_quit_failure(
        &mut self,
        intent: QuitIntent,
        safe_message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if !self.quit_ready_emitted || self.quit_ready_intent.as_ref() != Some(&intent) {
            return;
        }
        self.quit_ready_emitted = false;
        self.quit_ready_intent = None;
        self.pending_quit_intent = None;
        self.failed_quit_intent = Some(intent);
        self.host_lifecycle = RootHostLifecycle::StopFailed;
        let safe_message = safe_message.into();
        self.host_failure_message = Some(safe_message.clone());
        self.status = Some(safe_message);
        cx.notify();
    }

    /// 给 desktop 一个只读生命周期视图，避免它从状态栏文字推断服务是否仍可用。
    pub fn host_lifecycle(&self) -> RootHostLifecycle {
        self.host_lifecycle
    }

    /// StopFailed 后重试同一收尾意图时保留请求事实，不重新开放普通服务调用。
    ///
    /// @author kongweiguang
    pub fn retry_host_shutdown(&mut self, cx: &mut Context<Self>) -> bool {
        if self.host_lifecycle != RootHostLifecycle::StopFailed {
            return false;
        }
        if let Some(update) = self.pending_update_install.clone() {
            self.host_lifecycle = RootHostLifecycle::Stopping;
            self.host_shutdown_requested = true;
            cx.emit(RootViewEvent::InstallReady {
                update_id: update.update_id,
                version: update.version,
            });
            cx.notify();
            return true;
        }
        let Some(intent) = self.failed_quit_intent.clone() else {
            return false;
        };
        self.pending_quit_intent = Some(intent);
        self.emit_quit_ready(cx);
        true
    }

    /// 安装前收尾失败保留同一验签句柄及独立原错，Retry 重新收尾而非直接安装。
    ///
    /// @author kongweiguang
    pub fn report_update_quiesce_failure(
        &mut self,
        update_id: DownloadedUpdateId,
        version: &str,
        safe_message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = self.pending_update_install.as_ref() else {
            return;
        };
        if request.update_id != update_id || request.version != version {
            return;
        }
        let safe_message = safe_message.into();
        self.host_lifecycle = RootHostLifecycle::StopFailed;
        self.failed_quit_intent = Some(QuitIntent::RestartForUpdate {
            version: request.version.clone(),
        });
        self.status = Some(safe_message.clone());
        self.host_failure_message = Some(safe_message);
        self.quit_ready_emitted = false;
        self.quit_ready_intent = None;
        let error = AppError::infrastructure("更新安装前的应用服务收尾未完成");
        self.updates_panel.update(cx, |panel, cx| {
            panel.finish_host_install(update_id, UpdateInstallResult::NotStarted(error), cx);
        });
        cx.notify();
    }

    /// 安装回执只更新匹配句柄；已停止和结果未知仍保留独立原因，绝不投影为可继续工作。
    ///
    /// @author kongweiguang
    pub fn complete_update_install(
        &mut self,
        update_id: DownloadedUpdateId,
        result: UpdateInstallResult,
        cx: &mut Context<Self>,
    ) {
        if !self
            .pending_update_install
            .as_ref()
            .is_some_and(|request| request.update_id == update_id)
        {
            return;
        }
        self.host_failure_message = match &result {
            UpdateInstallResult::Installed(_) => None,
            UpdateInstallResult::NotStarted(error)
            | UpdateInstallResult::FailedNoEffect(error)
            | UpdateInstallResult::OutcomeUnknown(error) => {
                Some(error.public_message().to_owned().into())
            }
        };
        self.host_lifecycle = match &result {
            UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired) => {
                RootHostLifecycle::Stopping
            }
            UpdateInstallResult::NotStarted(_) | UpdateInstallResult::FailedNoEffect(_) => {
                RootHostLifecycle::StoppedUpdateFailed
            }
            UpdateInstallResult::OutcomeUnknown(_) => RootHostLifecycle::OutcomeUnknown,
        };
        self.updates_panel.update(cx, |panel, cx| {
            panel.finish_host_install(update_id, result, cx)
        });
        cx.notify();
    }

    /// 新进程启动失败保留确定事实和独立诊断，普通任务回执不能覆盖手动恢复指引。
    ///
    /// @author kongweiguang
    pub fn report_update_restart_failure(
        &mut self,
        update_id: DownloadedUpdateId,
        version: &str,
        safe_message: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) {
        if !self
            .pending_update_install
            .as_ref()
            .is_some_and(|request| request.update_id == update_id && request.version == version)
        {
            return;
        }
        self.host_lifecycle = RootHostLifecycle::RestartFailed;
        let safe_message = safe_message.into();
        self.host_failure_message = Some(safe_message.clone());
        self.status = Some(safe_message.clone());
        self.updates_panel.update(cx, |panel, cx| {
            panel.report_host_restart_failure(update_id, safe_message.to_string(), cx);
        });
        cx.notify();
    }

    /// 更新意图通过原门禁；实际重试派发时保留收尾事实，不让更新失败窗口重回普通工作台。
    ///
    /// @author kongweiguang
    pub(super) fn handle_updates_event(
        &mut self,
        event: &crate::features::updates::UpdatesEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            crate::features::updates::UpdatesEvent::InstallRequested { update_id, version } => {
                if self.host_lifecycle != RootHostLifecycle::Running {
                    self.updates_panel
                        .update(cx, |panel, cx| panel.cancel_install_guard(*update_id, cx));
                    return;
                }
                match self.update_install_guard.begin(*update_id, version.clone()) {
                    GuardRequestResult::Started => {
                        self.pending_update_install = self.update_install_guard.pending().cloned();
                        let _ = self.request_quit(
                            QuitIntent::RestartForUpdate {
                                version: version.clone(),
                            },
                            cx,
                        );
                    }
                    GuardRequestResult::AlreadyPending => {
                        // The settings panel may have been waiting on a running save/profile operation.
                        let _ = self.request_quit(
                            QuitIntent::RestartForUpdate {
                                version: version.clone(),
                            },
                            cx,
                        );
                    }
                    GuardRequestResult::Busy => {
                        self.updates_panel
                            .update(cx, |panel, cx| panel.cancel_install_guard(*update_id, cx));
                        self.status = Some(crate::locale::text(
                            "另一个更新请求正在处理，请稍后再试。",
                            "Another update request is already being handled. Try again shortly.",
                        ));
                    }
                }
            }
            crate::features::updates::UpdatesEvent::RetryInstallRequested {
                update_id,
                version,
            } => {
                let Some(request) = self.pending_update_install.as_ref() else {
                    return;
                };
                if request.update_id != *update_id || request.version != *version {
                    return;
                }
                match self.host_lifecycle {
                    RootHostLifecycle::StopFailed => {
                        self.host_lifecycle = RootHostLifecycle::Stopping;
                        self.host_shutdown_requested = true;
                        cx.emit(RootViewEvent::InstallReady {
                            update_id: *update_id,
                            version: version.clone(),
                        });
                    }
                    RootHostLifecycle::StoppedUpdateFailed => {
                        self.host_lifecycle = RootHostLifecycle::Stopping;
                        self.host_shutdown_requested = true;
                        cx.emit(RootViewEvent::RetryInstallReady {
                            update_id: *update_id,
                            version: version.clone(),
                        });
                    }
                    _ => return,
                }
            }
            crate::features::updates::UpdatesEvent::ExitAfterUpdateFailure { update_id } => {
                if self
                    .pending_update_install
                    .as_ref()
                    .is_none_or(|request| request.update_id != *update_id)
                    || !matches!(
                        self.host_lifecycle,
                        RootHostLifecycle::StopFailed
                            | RootHostLifecycle::StoppedUpdateFailed
                            | RootHostLifecycle::OutcomeUnknown
                            | RootHostLifecycle::RestartFailed
                            | RootHostLifecycle::ListenerRecoveryFailed
                    )
                {
                    return;
                }
                self.pending_quit_intent = Some(QuitIntent::Exit);
                self.emit_quit_ready(cx);
            }
        }
        cx.notify();
        let _ = window;
    }

    /// 取消只适用于宿主派发前；迟到确认不能清除已进入收尾的更新身份或将应用重新标为 Running。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_pending_update_install(&mut self, cx: &mut Context<Self>) {
        if self.host_shutdown_requested {
            return;
        }
        let Some(request) = self.pending_update_install.take() else {
            return;
        };
        self.update_install_guard.cancel(request.update_id);
        self.updates_panel.update(cx, |panel, cx| {
            panel.cancel_install_guard(request.update_id, cx)
        });
        self.pending_quit_intent = None;
        if self.host_lifecycle == RootHostLifecycle::Stopping && !self.quit_ready_emitted {
            self.host_lifecycle = RootHostLifecycle::Running;
        }
    }

    /// 只有 Running 准备阶段可在设置门禁通过后 flush 布局；等待确认期间产生的主机草稿也须重新检查。
    /// 无法发送的失败/只读/冲突队列中止本次退出并保留更新请求，用户核对后再次退出。
    ///
    /// @author kongweiguang
    pub(super) fn continue_quit_after_preferences(&mut self, cx: &mut Context<Self>) -> bool {
        if !quit_preparation_is_running(self.host_lifecycle, self.host_shutdown_requested) {
            return false;
        }
        if self.cancel_quit_for_connection_changes(cx) {
            return false;
        }
        let preferences = self.preferences_panel.clone();
        preferences.update(cx, |panel, cx| panel.flush_pending_workbench_layout(cx));
        let owner_has_pending = preferences.read(cx).has_pending_workbench_layout();
        if !super::workbench::workbench_layout_receipts_complete(
            &self.workbench_layout_intent,
            owner_has_pending,
        ) {
            if owner_has_pending && !preferences.read(cx).is_workbench_layout_save_in_flight() {
                self.pending_quit_intent = None;
                self.workbench_layout_save_error = Some(crate::locale::text(
                    "布局尚未保存；请通过状态栏在设置中读回、核对或明确重试后再次退出。",
                    "Layout is unsaved. Use the status bar to reload, verify or explicitly retry, then quit again.",
                ));
            }
            cx.notify();
            return false;
        }
        let Some(editor) = self.file_editor_workspace.as_ref() else {
            self.emit_quit_ready(cx);
            return true;
        };
        if editor.update(cx, |workspace, cx| workspace.request_quit(cx)) {
            self.emit_quit_ready(cx);
            true
        } else {
            false
        }
    }

    /// 编辑器等迟到确认不能在 Stopping 抢走原 CAS 意图；Running 仍复查主机所有权并等布局末收据。
    /// 停止失败后的重试不再调用已停止的服务。
    ///
    /// @author kongweiguang
    pub(super) fn emit_quit_ready(&mut self, cx: &mut Context<Self>) {
        if workspace_quit_preparation_in_flight(self.host_lifecycle, self.host_shutdown_requested) {
            return;
        }
        if self.host_lifecycle == RootHostLifecycle::Running
            && self.pending_quit_intent.is_some()
            && self.cancel_quit_for_connection_changes(cx)
        {
            return;
        }
        if self.host_lifecycle == RootHostLifecycle::Running
            && self.pending_quit_intent.is_some()
            && !super::workbench::workbench_layout_receipts_complete(
                &self.workbench_layout_intent,
                self.preferences_panel
                    .read(cx)
                    .has_pending_workbench_layout(),
            )
        {
            let _ = self.continue_quit_after_preferences(cx);
            return;
        }
        let Some(intent) = self.pending_quit_intent.take() else {
            return;
        };
        let update_request = match &intent {
            QuitIntent::RestartForUpdate { version } => self
                .pending_update_install
                .clone()
                .filter(|request| &request.version == version),
            QuitIntent::Exit => None,
        };
        if matches!(&intent, QuitIntent::RestartForUpdate { .. }) && update_request.is_none() {
            self.pending_quit_intent = None;
            self.host_lifecycle = RootHostLifecycle::Running;
            self.status = Some(crate::locale::text(
                "更新请求已失效，请重新发起更新。",
                "The update request is no longer valid. Start the update again.",
            ));
            self.cancel_pending_update_install(cx);
            cx.notify();
            return;
        }

        // Desktop retries after a failed/partial shutdown must not call application services again.
        if self.host_lifecycle != RootHostLifecycle::Running {
            self.emit_quit_ready_event(intent, cx);
            return;
        }
        // CAS 准备期间保留原退出意图；布局失败/明确取消可以撤销它，迟到成功不能复活退出。
        self.pending_quit_intent = Some(intent.clone());
        if self.services.workspace_sessions.is_none() {
            self.finish_quit_after_workspace_save(
                intent,
                terminal_open::WorkspaceSnapshotSaveCompletion::NoChanges,
                cx,
            );
            return;
        }
        if self.workspace_load_pending {
            self.finish_quit_after_workspace_save(
                intent,
                terminal_open::WorkspaceSnapshotSaveCompletion::Busy,
                cx,
            );
            return;
        }

        self.host_lifecycle = RootHostLifecycle::Stopping;
        self.persist_workspace_snapshot_with_completion(
            move |root, result, cx| root.finish_quit_after_workspace_save(intent, result, cx),
            cx,
        );
    }

    /// workspace CAS 不冻结鼠标回执；提交宿主前复查主机草稿与布局，避免迟到成功绕过用户写入所有权。
    /// 只在尚未派发宿主收尾时修改生命周期，不能将部分停止的应用伪装成 Running。
    ///
    /// @author kongweiguang
    fn finish_quit_after_workspace_save(
        &mut self,
        intent: QuitIntent,
        result: terminal_open::WorkspaceSnapshotSaveCompletion,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested {
            return;
        }
        if self.pending_quit_intent.as_ref() != Some(&intent) {
            // 本次 CAS 仅属准备阶段；取消后不以迟到成功重新开始 host 收尾。
            self.host_lifecycle = RootHostLifecycle::Running;
            cx.notify();
            return;
        }
        match result {
            terminal_open::WorkspaceSnapshotSaveCompletion::NoChanges
            | terminal_open::WorkspaceSnapshotSaveCompletion::Saved => {
                if self.cancel_quit_for_connection_changes(cx) {
                    self.host_lifecycle = RootHostLifecycle::Running;
                    return;
                }
                if !super::workbench::workbench_layout_receipts_complete(
                    &self.workbench_layout_intent,
                    self.preferences_panel
                        .read(cx)
                        .has_pending_workbench_layout(),
                ) {
                    self.host_lifecycle = RootHostLifecycle::Running;
                    self.pending_quit_intent = None;
                    // 真实失败已取消本次退出；Ready/watch 回执不能代替用户重新发起。
                    if self.workbench_layout_save_error.is_none() {
                        let _ = self.request_quit(intent, cx);
                    }
                    cx.notify();
                    return;
                }
                self.pending_quit_intent = None;
                if let QuitIntent::RestartForUpdate { version } = &intent {
                    let Some(request) = self
                        .pending_update_install
                        .clone()
                        .filter(|request| &request.version == version)
                    else {
                        self.cancel_pending_update_install(cx);
                        self.status = Some(crate::locale::text(
                            "更新请求已失效，请重新发起更新。",
                            "The update request is no longer valid. Start the update again.",
                        ));
                        cx.notify();
                        return;
                    };
                    let Some(approved) = self.update_install_guard.approve(request.update_id)
                    else {
                        self.cancel_pending_update_install(cx);
                        return;
                    };
                    if approved != request {
                        self.cancel_pending_update_install(cx);
                        return;
                    }
                    let started = self.updates_panel.update(cx, |panel, cx| {
                        panel.begin_host_install(request.update_id, cx)
                    });
                    if !started {
                        self.pending_update_install = Some(request.clone());
                        self.cancel_pending_update_install(cx);
                        self.status = Some(crate::locale::text(
                            "更新面板已无法接管安装，请重新发起更新。",
                            "The update panel could not take over installation. Start the update again.",
                        ));
                        cx.notify();
                        return;
                    }
                    self.pending_update_install = Some(request.clone());
                    self.quit_ready_emitted = false;
                    self.quit_ready_intent = None;
                    self.host_lifecycle = RootHostLifecycle::Stopping;
                    self.host_shutdown_requested = true;
                    self.quit_preparation_failure = None;
                    cx.emit(RootViewEvent::InstallReady {
                        update_id: request.update_id,
                        version: request.version,
                    });
                    cx.notify();
                    return;
                }
                self.emit_quit_ready_event(intent, cx);
            }
            result => {
                let is_update = matches!(&intent, QuitIntent::RestartForUpdate { .. });
                let message = workspace_quit_preparation_failure(&result)
                    .expect("failure completion must carry a preparation error");
                self.pending_quit_intent = None;
                self.quit_ready_emitted = false;
                self.quit_ready_intent = None;
                self.failed_quit_intent = None;
                self.host_lifecycle = RootHostLifecycle::Running;
                if is_update {
                    self.cancel_pending_update_install(cx);
                }
                self.quit_preparation_failure = Some(message.clone());
                self.status = Some(message);
                cx.notify();
            }
        }
    }

    /// 派发时单调记录已交给宿主的事实；即使收尾失败，也不能重新使用已取消的服务。
    ///
    /// @author kongweiguang
    fn emit_quit_ready_event(&mut self, intent: QuitIntent, cx: &mut Context<Self>) {
        self.quit_preparation_failure = None;
        self.host_lifecycle = RootHostLifecycle::Stopping;
        self.host_shutdown_requested = true;
        self.quit_ready_emitted = true;
        self.quit_ready_intent = Some(intent.clone());
        cx.emit(RootViewEvent::QuitReady { intent });
    }

    /// 丢稿决定只在宿主派发前继续门禁；迟到的取消回执不能撤销已经发生的资源收尾。
    ///
    /// @author kongweiguang
    pub(super) fn handle_preferences_quit_result(
        &mut self,
        confirmed: bool,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested {
            return;
        }
        if !confirmed {
            self.cancel_pending_update_install(cx);
            self.pending_quit_intent = None;
            self.quit_preparation_failure = None;
            return;
        }
        let _ = self.continue_quit_after_preferences(cx);
    }

    /// 实际收尾或准备失败后的 X 只走显式退出重试；失败原错不能因隐藏到托盘而失去入口。
    ///
    /// @author kongweiguang
    pub(super) fn handle_window_close_request(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let policy = window_close_policy(
            self.host_shutdown_requested,
            self.tray_owner_installed,
            self.quit_preparation_failure.is_some(),
        );
        if matches!(
            policy,
            kerminal_application::platform::window_state::ClosePolicy::HideToTray
        ) && window.supports_visibility_control()
        {
            if window.set_window_visibility(false).is_ok() {
                return;
            }
            self.report_host_status("窗口无法隐藏到托盘，正在检查是否可以安全退出", cx);
        }
        let _ = self.request_quit(QuitIntent::Exit, cx);
    }

    /// 准备失败保留普通工作台以处理原关闭/保存错误；仅显式重试或继续使用可改变退出意图。
    /// 提示不挂模态、不重用 host lifecycle，避免失败前就停止服务或迟到回执自动退出。
    ///
    /// @author kongweiguang
    pub(super) fn render_quit_preparation_failure(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let message = self.quit_preparation_failure.clone()?;
        let theme = crate::theme::ThemeTokens::current(window, cx);
        Some(
            div()
                .id("quit-preparation-failure")
                .bg(theme.materials.panel)
                .text_color(theme.foreground)
                .border_b_1()
                .border_color(theme.border)
                .w_full()
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .child(div().flex_1().text_color(theme.error).child(message))
                .child(
                    Button::new("quit-preparation-retry")
                        .compact()
                        .label(crate::locale::text("重试退出", "Retry quitting"))
                        .disabled(self.pending_quit_intent.is_some())
                        .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
                            root.request_quit(QuitIntent::Exit, cx);
                        })),
                )
                .child(
                    Button::new("quit-preparation-cancel")
                        .compact()
                        .label(crate::locale::text("继续使用", "Keep working"))
                        .disabled(self.pending_quit_intent.is_some())
                        .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
                            root.quit_preparation_failure = None;
                            cx.notify();
                        })),
                )
                .into_any_element(),
        )
    }

    /// 停止视图不挂载会发起普通任务的子 Entity；原 owner 留在 Root，资源回执仍由原订阅处理。
    ///
    /// @author kongweiguang
    pub(super) fn render_host_shutdown(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let presentation = host_shutdown_presentation(
            self.host_shutdown_requested,
            self.host_lifecycle,
            self.host_failure_message.as_ref(),
            self.application_task_snapshot.active_tasks,
        )?;
        if !self.host_shutdown_view_shown {
            window.focus(&self.focus_handle, cx);
            self.keybinding_runtime.clear_pending();
            self.host_shutdown_view_shown = true;
        }
        let theme = crate::theme::ThemeTokens::current(window, cx);
        let title_content = div()
            .flex_1()
            .min_w(px(0.))
            .h_full()
            .px_3()
            .flex()
            .items_center()
            .text_color(theme.foreground)
            .child("Kerminal")
            .into_any_element();
        #[cfg(target_os = "windows")]
        let title_bar = super::windows_title_bar::windows_workbench_title_bar(
            title_content,
            theme.materials.chrome,
            theme,
            window,
        );
        #[cfg(not(target_os = "windows"))]
        let title_bar = {
            let root = cx.entity();
            gpui_kit::component::TitleBar::new()
                .h(px(36.))
                .bg(theme.materials.chrome)
                .on_close_window(move |_: &ClickEvent, window, app| {
                    root.update(app, |root, cx| root.handle_window_close_request(window, cx));
                })
                .child(title_content)
                .into_any_element()
        };
        let retry_exit = presentation.retry_exit;
        let mut actions = div().flex().gap_3().child(
            Button::new("host-shutdown-retry-exit")
                .label(crate::locale::text("重试退出", "Retry quitting"))
                .tab_stop(true)
                .disabled(!retry_exit)
                .loading(!retry_exit)
                .on_click(cx.listener(|root, _: &ClickEvent, _, cx| {
                    root.request_quit(QuitIntent::Exit, cx);
                })),
        );
        if let Some(request) =
            retryable_update_request(self.host_lifecycle, self.pending_update_install.as_ref())
                .cloned()
        {
            actions = actions.child(
                Button::new("host-shutdown-retry-update")
                    .label(crate::locale::text("重试更新", "Retry update"))
                    .tab_stop(true)
                    .on_click(cx.listener(move |root, _: &ClickEvent, _, cx| {
                        if retryable_update_request(
                            root.host_lifecycle,
                            root.pending_update_install.as_ref(),
                        ) != Some(&request)
                        {
                            return;
                        }
                        root.updates_panel
                            .update(cx, |panel, cx| panel.retry_host_install(cx));
                    })),
            );
        }
        Some(
            div()
                .id("host-shutdown-view")
                .size_full()
                .track_focus(&self.focus_handle)
                .key_context(crate::keybindings::KeybindingContext::App.as_gpui_identifier())
                .on_key_down(cx.listener(|root, event: &gpui_kit::KeyDownEvent, _, cx| {
                    if event.prefer_character_input {
                        root.keybinding_runtime.clear_pending();
                        return;
                    }
                    use super::keybindings::{KeybindingDispatch, KeybindingFocusScope};
                    match root.keybinding_runtime.dispatch(
                        &event.keystroke,
                        KeybindingFocusScope::App,
                        false,
                        false,
                        event.is_held,
                    ) {
                        KeybindingDispatch::Action(
                            crate::keybindings::KeybindingAction::ApplicationQuit,
                        ) => {
                            root.request_quit(QuitIntent::Exit, cx);
                            cx.stop_propagation();
                        }
                        KeybindingDispatch::Pending => cx.stop_propagation(),
                        _ => {}
                    }
                }))
                .flex()
                .flex_col()
                .bg(theme.materials.panel)
                .text_color(theme.foreground)
                .child(title_bar)
                .child(
                    div()
                        .id("host-shutdown-notice")
                        .flex_1()
                        .p_4()
                        .flex()
                        .flex_col()
                        .justify_center()
                        .items_center()
                        .gap_3()
                        .child(
                            div()
                                .text_color(if retry_exit { theme.error } else { theme.muted })
                                .child(presentation.message),
                        )
                        .child(div().text_color(theme.muted).child(format!(
                            "{}: {}",
                            crate::locale::text("未返回的受管任务", "Managed tasks still running",),
                            presentation.managed_tasks,
                        )))
                        .child(
                            div()
                                .max_w(px(620.))
                                .text_color(theme.muted)
                                .child(presentation.cleanup_note),
                        )
                        .child(actions),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_quit_recovery.rs"]
mod tests;
