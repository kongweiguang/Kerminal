//! Root 所有终端 pane 共用的创建、首帧、取消与清理闭环。
//!
//! @author kongweiguang

#[cfg(feature = "acceptance-metrics")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::{future::Future, sync::Arc};

use gpui_kit::{AppContext as _, Context, SharedString};
use kerminal_application::{
    AppError,
    operations::OperationsService,
    platform::external_launch::ExternalTargetLease,
    sessions::{
        CreateSessionRequest, RenderSnapshotRequest, SessionChangeKind, SessionFilter,
        SessionService, SessionSubscription, TerminalPalette, UiSessionBinding,
    },
    workspace::{WorkspacePaneMode, WorkspaceSessionLoad, WorkspaceSessionSaveOutcome},
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget, TerminalSize},
};
use kerminal_terminal::snapshot::RenderSnapshot;
use tokio_util::sync::CancellationToken;

use super::{
    RootView, WorkspaceTerminalOpenKind,
    workspace_runtime::{OrphanCleanupInput, RuntimePanePhase, WorkspaceRuntime},
};
use crate::features::workspace_controller::{
    WorkspacePersistenceState, WorkspaceRestoreFailure, WorkspaceRestoreItemId,
};
use crate::features::workspace_tabs::append_shared_sftp_workbench_projection;

/// 工作区 CAS 回执供 Root 的退出门控使用，未知提交与回读确认必须保持不同结果。
pub(super) enum WorkspaceSnapshotSaveCompletion {
    NoChanges,
    Saved,
    Conflict,
    ReadbackConfirmedUncommitted,
    OutcomeUnknown,
    Busy,
    Failed(SharedString),
}

/// 创建结果保留 subscription 与首帧；Root 接管后 watcher 才开始消费更新。
struct PreparedWorkspaceSession {
    session: SessionInfo,
    snapshot: RenderSnapshot,
    subscription: SessionSubscription,
}

/// 初始化失败时显式携带未能关闭的 SessionInfo，防止失去清理重试入口。
enum WorkspaceSessionOpenResult {
    Ready(PreparedWorkspaceSession),
    Failed {
        message: SharedString,
        cleanup_pending: Option<(SessionInfo, Option<RenderSnapshot>)>,
    },
}

/// 一次 pane 会话打开请求必须把资源 owner、恢复代次与取消令牌作为同一身份传递。
/// @author kongweiguang
pub(super) struct WorkspaceTerminalOpenRequest {
    pub(super) tab_id: TabId,
    pub(super) pane_id: PaneId,
    pub(super) kind: WorkspaceTerminalOpenKind,
    pub(super) size: TerminalSize,
    pub(super) lease: Option<ExternalTargetLease>,
    pub(super) transient: bool,
    pub(super) generation: u64,
    pub(super) cancellation: CancellationToken,
}

/// 未发布本机容器 exec 初始化失败时的 owner 交接输入；借用请求身份避免复制异步 session 内容。
struct PendingLocalContainerCleanup<'a> {
    tab_id: &'a TabId,
    pane_id: &'a PaneId,
    target: &'a kerminal_domain::operations::ContainerTarget,
    session: &'a SessionInfo,
    snapshot: Option<&'a RenderSnapshot>,
    external_lease: Option<ExternalTargetLease>,
    transient: bool,
    status: SharedString,
}

/// 颜色和滚动缓冲是同一会话显示配置，和目标创建/资源身份分开传递。
/// @author kongweiguang
struct TerminalSessionPresentation {
    palette: TerminalPalette,
    scrollback: usize,
}

/// 生命周期查询共用输入绑定请求序号，使较早启动的查询回执无法改写较新的 pane 状态。
///
/// @author kongweiguang
fn next_lifecycle_context_request_generation(generation: &mut u64) -> u64 {
    *generation = generation.wrapping_add(1).max(1);
    *generation
}

/// Ready and later lifecycle observations share one ticket registry so either result can invalidate the other.
///
/// @author kongweiguang
fn register_lifecycle_context_request(
    generation: &mut u64,
    requests: &mut std::collections::HashMap<SessionId, (u64, SessionOwner)>,
    session_id: SessionId,
    owner: SessionOwner,
) -> u64 {
    let request_generation = next_lifecycle_context_request_generation(generation);
    requests.insert(session_id, (request_generation, owner));
    request_generation
}

/// A restored pane may install a handler, but only an exact fresh marker and installed binding may launch echo probes.
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn terminal_echo_probe_may_start(fresh_probe_pending: bool, exact_binding_installed: bool) -> bool {
    fresh_probe_pending && exact_binding_installed
}

/// 生命周期查询回执只有仍占有 session 请求槽位时才可继续处理；所有结果分支都先经过此门。
///
/// @author kongweiguang
fn lifecycle_context_request_is_current(
    current: Option<&(u64, SessionOwner)>,
    request_generation: u64,
    owner: &SessionOwner,
) -> bool {
    current.is_some_and(|(generation, current_owner)| {
        *generation == request_generation && current_owner == owner
    })
}

/// 首次创建 session 时可能尚无服务观察，允许其 context；已观察过的 pane 则拒绝旧代次。
///
/// @author kongweiguang
fn lifecycle_context_generation_is_current(
    context_generation: u64,
    observed_generation: Option<u64>,
) -> bool {
    observed_generation.is_none_or(|generation| context_generation >= generation)
}

/// 自然退出和可恢复失败仍持有可读历史；关闭或重连阶段不得沿用旧视口绑定。
///
/// @author kongweiguang
pub(super) fn lifecycle_retains_terminal_history(lifecycle: SessionLifecycle) -> bool {
    matches!(
        lifecycle,
        SessionLifecycle::Exited | SessionLifecycle::Failed
    )
}

impl RootView {
    /// Ready 不重放创建前已发布的 Running lifecycle，因此可见首帧接管后显式查 context；后台恢复 pane 等待焦点路径按需绑定。
    /// lifecycle 仍查询隐藏 pane 以退休历史；只有所属 tab 可见时才安装 Running handler，且每个回执都经过身份与 generation 门禁。
    ///
    /// @author kongweiguang
    ///
    /// Fresh probe 只在可见 pane 的精确 binding 真正安装后触发；若 Ready 入场或回执时 pane 已隐藏，
    /// 则消费 fresh 标记并记录固定拒绝原因，不延迟到用户之后激活 pane 时注入探针。普通输入绑定仍由焦点路径按需恢复。
    /// 此入口只协调 UI pane，从 tab/pane 派生 owner，避免调用方传入互相矛盾的身份。
    fn request_terminal_input_context_reconcile(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        session_id: SessionId,
        require_visible_tab: bool,
        #[cfg(feature = "acceptance-metrics")] fresh_probe_pending: Arc<AtomicBool>,
        cx: &mut Context<Self>,
    ) {
        let owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        if self.workspace_runtime.session_for_pane(&pane_id) != Some(&session_id)
            || self.session_command_owner(&session_id).as_ref() != Some(&owner)
        {
            return;
        }
        if require_visible_tab && !self.workspace_tab_is_visible(&tab_id) {
            #[cfg(feature = "acceptance-metrics")]
            if fresh_probe_pending.swap(false, Ordering::AcqRel) {
                crate::acceptance_metrics::terminal_echo_probe_registry()
                    .record_admission_rejection(
                        crate::acceptance_metrics::TerminalEchoAdmissionRejection::FreshPaneHidden,
                    );
            }
            return;
        }
        let request_generation = register_lifecycle_context_request(
            &mut self.pane_input_binding_request_generation,
            &mut self.pane_input_binding_requests,
            session_id.clone(),
            owner.clone(),
        );
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let window_handle = self.window_handle;
        let root_entity = cx.entity();
        cx.spawn(async move |_this, cx| {
            let lookup_session_id = session_id.clone();
            let lookup_owner = owner.clone();
            let input_context = executor
                .run(move |_| async move {
                    let listed = sessions
                        .list(SessionFilter {
                            owner: Some(lookup_owner),
                            target: None,
                        })
                        .await?;
                    let Some(info) = listed
                        .into_iter()
                        .find(|info| info.session_id == lookup_session_id)
                    else {
                        return Ok(None);
                    };
                    let lifecycle = info.lifecycle;
                    if lifecycle != SessionLifecycle::Running {
                        return Ok(Some((lifecycle, None)));
                    }
                    sessions
                        .terminal_command_context(lookup_session_id)
                        .await
                        .map(|context| Some((lifecycle, Some(context))))
                })
                .await;
            let _ = cx.update_window(window_handle, move |_, window, app| {
                root_entity.update(app, |root, cx| {
                    if root.workspace_runtime.session_for_pane(&pane_id) != Some(&session_id)
                        || root.session_command_owner(&session_id).as_ref() != Some(&owner)
                        || !lifecycle_context_request_is_current(
                            root.pane_input_binding_requests.get(&session_id),
                            request_generation,
                            &owner,
                        )
                    {
                        return;
                    }
                    root.pane_input_binding_requests.remove(&session_id);
                    if require_visible_tab && !root.workspace_tab_is_visible(&tab_id) {
                        #[cfg(feature = "acceptance-metrics")]
                        if fresh_probe_pending.swap(false, Ordering::AcqRel) {
                            crate::acceptance_metrics::terminal_echo_probe_registry()
                                .record_admission_rejection(
                                    crate::acceptance_metrics::TerminalEchoAdmissionRejection::FreshPaneHidden,
                                );
                        }
                        return;
                    }
                    if root
                        .session_restart_projection(&pane_id)
                        .is_some_and(|projection| projection.busy)
                    {
                        root.invalidate_pane_input_handler(&session_id, cx);
                        return;
                    }
                    if let Ok(Some((_, Some(context)))) = input_context.as_ref()
                        && !lifecycle_context_generation_is_current(
                            context.session_generation,
                            root.session_restart_projection(&pane_id)
                                .map(|projection| projection.binding.session_generation),
                        )
                    {
                        // 迟到的旧 generation 不得撤销或覆盖当前 pane 的输入绑定。
                        return;
                    }
                    match input_context {
                        Ok(Some((lifecycle, None)))
                            if lifecycle_retains_terminal_history(lifecycle) =>
                        {
                            root.retire_pane_input_handler_for_history(&session_id, window, cx);
                        }
                        Ok(Some((_, Some(context))))
                            if context.session_id == session_id && context.owner == owner =>
                        {
                            let binding = UiSessionBinding {
                                session_id: context.session_id.clone(),
                                owner: context.owner.clone(),
                                session_generation: context.session_generation,
                            };
                            if root.workspace_tab_is_visible(&tab_id) {
                                root.install_pane_input_handler(
                                    binding.clone(),
                                    context,
                                    window,
                                    cx,
                                );
                                #[cfg(feature = "acceptance-metrics")]
                                if terminal_echo_probe_may_start(
                                    fresh_probe_pending.load(Ordering::Acquire),
                                    root.pane_input_handlers.get(&session_id).is_some_and(
                                        |registration| registration.binding == binding,
                                    ),
                                ) {
                                    root.maybe_start_terminal_echo_probes(&binding, cx);
                                    fresh_probe_pending.store(false, Ordering::Release);
                                }
                            } else if root
                                .pane_input_handlers
                                .get(&session_id)
                                .is_some_and(|registration| registration.binding != binding)
                            {
                                // Hidden pane must not gain a new writer or retain a handler for an older generation.
                                root.invalidate_pane_input_handler(&session_id, cx);
                            }
                        }
                        Ok(Some((_, Some(_)))) => {
                            root.invalidate_pane_input_handler(&session_id, cx);
                            root.status = Some("终端重连后输入绑定发生变化，已停用旧输入。".into());
                            cx.notify();
                        }
                        Ok(Some(_)) | Ok(None) => {
                            root.invalidate_pane_input_handler(&session_id, cx);
                        }
                        Err(error) => {
                            root.invalidate_pane_input_handler(&session_id, cx);
                            root.status = Some(error.public_message().into());
                            cx.notify();
                        }
                    }
                    root.refresh_proxy_panel(cx);
                    root.refresh_session_reconnect_state(tab_id, pane_id, cx);
                });
            });
        })
        .detach();
    }

    /// 按预留身份接收资源回执；首帧 identity 未核实时不绑定 pane，避免真实 session 留在未发布布局。
    /// 收尾后仍登记 owner，但不再从迟到 Ready 派发订阅或代理、SFTP 等普通任务。
    /// Closed 必须立即撤销右栏绑定，不能依赖中央终端当前是否可见。
    /// 初始 Ready 只为当前显示 tab 安装输入 handler；后台恢复 pane 留给后续显式焦点路径按需绑定。
    /// lifecycle 仍处理隐藏 pane 的退出与历史退休，Running 新 binding 仅在所属 tab 显示时安装。
    /// 验收探针仅由精确匹配的 fresh pane generation 或新建 split 输入 binding 首次成功安装触发，恢复和重连不启动探针。
    /// acceptance-metrics watch task 若早于异步输入绑定启动，会在每次收到变化后重试读取 generation；首次成功后固定该代。
    /// 新 tab 的持久化失败需将 Ready session 转交 orphan cleanup，避免 pane 不可见后遗留运行资源。
    ///
    /// @author kongweiguang
    pub(super) fn open_workspace_terminal_pane(
        &mut self,
        request: WorkspaceTerminalOpenRequest,
        cx: &mut Context<RootView>,
    ) -> Result<(), SharedString> {
        let WorkspaceTerminalOpenRequest {
            tab_id,
            pane_id,
            kind,
            size,
            lease,
            transient,
            generation,
            cancellation,
        } = request;
        if !self.workspace_restore_is_current(&pane_id, generation) {
            return Err(crate::locale::text(
                "终端 pane 恢复请求已过期。",
                "The terminal pane restore request is no longer current.",
            ));
        }
        let runtime = self
            .workspace_runtime
            .pane(&pane_id)
            .ok_or_else(|| SharedString::from("终端 pane 预留已失效。"))?;
        if runtime.tab_id != tab_id
            || !matches!(
                runtime.phase,
                RuntimePanePhase::RestorePending | RuntimePanePhase::RestoreFailed
            )
        {
            return Err(crate::locale::text(
                "终端 pane 当前不能启动新的会话。",
                "This terminal pane cannot start another session in its current state.",
            ));
        }
        let split_operation_id = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .and_then(|operation| match operation {
                crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                    operation_id,
                    tab_id: pending_tab_id,
                    pane,
                    ..
                } if pending_tab_id == &tab_id && pane.id == pane_id => Some(operation_id.clone()),
                crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                    ..
                }
                | crate::features::workspace_controller::WorkspaceResourceOperation::ClosePane {
                    ..
                }
                | crate::features::workspace_controller::WorkspaceResourceOperation::CloseTab {
                    ..
                } => None,
            })
            .filter(|operation_id| {
                self.workspace_resource_operations
                    .get(&pane_id)
                    .is_some_and(|entry| entry.operation_id() == operation_id.as_str())
            });

        let orphan_lease = lease.clone();
        self.workspace_runtime
            .set_resource_owner(&pane_id, lease, transient)
            .map_err(|_| SharedString::from("终端 pane 资源 owner 无法登记。"))?;
        self.status = Some(crate::locale::text(
            "正在启动终端会话…",
            "Starting terminal session…",
        ));
        self.refresh_workspace_views(cx);
        cx.notify();

        let services = self.services.clone();
        let executor = services.executor.clone();
        let sessions = services.sessions.clone();
        let lifecycle_executor = services.executor.clone();
        let lifecycle_sessions = services.sessions.clone();
        let operations = services.operations.clone();
        let direct_ssh_without_initial_command = matches!(
            &kind,
            WorkspaceTerminalOpenKind::Direct(SessionTarget::Ssh(target))
                if target.command.is_none()
        );
        let palette = self.current_terminal_palette();
        let scrollback = self.terminal_scrollback;
        let restore_cancel = cancellation.clone();
        let is_new_tab_creation = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                !controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == &tab_id)
            });
        #[cfg(feature = "acceptance-metrics")]
        let fresh_terminal_probe_pending = {
            let is_fresh_reserved_generation =
                self.pending_fresh_terminal_probes.get(&pane_id).copied() == Some(generation);
            if is_fresh_reserved_generation {
                self.pending_fresh_terminal_probes.remove(&pane_id);
            }
            Arc::new(AtomicBool::new(
                is_fresh_reserved_generation || split_operation_id.is_some(),
            ))
        };
        let unpublished_transient_local_container_target =
            unpublished_transient_local_container_target(&kind, transient, is_new_tab_creation);
        let active_tab_at_open = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.clone());
        let center_panel_at_open = self.center_panel;
        let window_handle = self.window_handle;
        let create_tab_id = tab_id.clone();
        let create_pane_id = pane_id.clone();

        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    if task_cancellation.is_cancelled() || restore_cancel.is_cancelled() {
                        return Ok(WorkspaceSessionOpenResult::Failed {
                            message: crate::locale::text(
                                "终端 pane 恢复已取消。",
                                "Terminal pane restore was cancelled.",
                            ),
                            cleanup_pending: None,
                        });
                    }
                    let create_cancellation = task_cancellation.child_token();
                    let cancellation_for_prepare = create_cancellation.clone();
                    let mut prepare = Box::pin(prepare_workspace_session(
                        sessions,
                        operations,
                        kind,
                        SessionOwner::Ui {
                            tab_id: create_tab_id,
                            pane_id: create_pane_id,
                        },
                        size,
                        TerminalSessionPresentation { palette, scrollback },
                        cancellation_for_prepare,
                    ));
                    let mut restore_cancelled = Box::pin(restore_cancel.cancelled());
                    let mut restore_cancel_applied = false;
                    let result = std::future::poll_fn(move |task_context| {
                        if !restore_cancel_applied
                            && restore_cancelled.as_mut().poll(task_context).is_ready()
                        {
                            restore_cancel_applied = true;
                            create_cancellation.cancel();
                        }
                        prepare.as_mut().poll(task_context)
                    })
                    .await;
                    Ok(result)
                })
                .await;

            let result = match result {
                Ok(result) => result,
                Err(error) => WorkspaceSessionOpenResult::Failed {
                    message: error.public_message().to_owned().into(),
                    cleanup_pending: None,
                },
            };

            // 用资源首次 Ready 回执时的 generation 捕获绑定；后续异步代理任务不能重查后误认重连为首次连接。
            let initial_proxy_binding = if direct_ssh_without_initial_command {
                if let WorkspaceSessionOpenResult::Ready(prepared) = &result {
                    let session_id = prepared.session.session_id.clone();
                    let owner = prepared.session.owner.clone();
                    let query_sessions = services.sessions.clone();
                    executor
                        .clone()
                        .run(move |_| async move {
                            query_sessions.terminal_command_context(session_id).await
                        })
                        .await
                        .ok()
                        .filter(|context| {
                            context.session_id == prepared.session.session_id
                                && context.owner == owner
                        })
                        .map(|context| UiSessionBinding {
                            session_id: context.session_id,
                            owner: context.owner,
                            session_generation: context.session_generation,
                        })
                } else {
                    None
                }
            } else {
                None
            };

            let mut completion = Some(result);
            let mut subscription = None;
            let mut cleanup_pending = None;
            let mut external_sftp_after_ready = None;
            let mut initial_proxy_after_ready = None;
            let accepted = this.update(cx, |root, cx| {
                if !root.workspace_restore_is_current(&pane_id, generation) {
                    return false;
                }

                let Some(open_result) = completion.take() else {
                    root.status = Some(crate::locale::text(
                        "终端启动结果缺失，保留 pane 供重试。",
                        "The terminal startup result is missing; the pane was kept for retry.",
                    ));
                    root.refresh_workspace_views(cx);
                    return true;
                };
                match open_result {
                    WorkspaceSessionOpenResult::Ready(prepared) => {
                        if is_new_tab_creation {
                            root.is_creating_session = false;
                        }
                        let session_id = prepared.session.session_id.clone();
                        let target = prepared.session.target.clone();
                        let session_size = prepared.session.size;
                        if prepared.snapshot.session_id != session_id
                            || prepared.snapshot.size != session_size
                        {
                            cleanup_pending = Some((
                                prepared.session,
                                Some(prepared.snapshot),
                                crate::locale::text(
                                    "终端首帧与 session 身份或尺寸不一致；资源保留供清理。",
                                    "The terminal's first frame does not match its session identity or size; the resource is retained for cleanup.",
                                ),
                            ));
                            return true;
                        }
                        if root.workspace_runtime.install_session(
                            &pane_id,
                            session_id.clone(),
                            target.clone(),
                            session_size,
                        ).is_err() {
                            cleanup_pending = Some((
                                prepared.session,
                                Some(prepared.snapshot),
                                crate::locale::text(
                                    "终端已启动，但 pane 已被另一会话占用。",
                                    "The terminal started, but the pane is occupied by another session.",
                                ),
                            ));
                            return true;
                        }
                        let accepted_snapshot = root.workspace_runtime.accept_initial_snapshot(
                            &pane_id,
                            &session_id,
                            prepared.snapshot.clone(),
                        );
                        if !matches!(accepted_snapshot, Ok(true)) {
                            let base_message = crate::locale::text(
                                "终端已启动，但首帧与 pane 状态不匹配；请重试关闭。",
                                "The terminal started, but its first frame did not match the pane; retry closing it.",
                            );
                            let phase_error = match root.workspace_runtime.begin_close(&pane_id) {
                                Ok(closing)
                                    if matches!(
                                        root.workspace_runtime
                                            .mark_close_failed(&pane_id, &closing),
                                        Ok(true)
                                    ) => None,
                                Ok(_) | Err(_) => Some(crate::locale::text(
                                    "pane 生命周期状态未能回到可重试关闭状态。",
                                    "The pane could not be returned to a retryable close state.",
                                )),
                            };
                            let message = phase_error.map_or_else(
                                || base_message.clone(),
                                |error| SharedString::from(format!("{base_message} {error}")),
                            );
                            if root
                                .workspace_runtime
                                .set_status(&pane_id, Some(message.clone()))
                                .is_err()
                            {
                                root.status = Some(crate::locale::text(
                                    "终端已启动，但首帧失败状态无法绑定到 pane。",
                                    "The terminal started but its first-frame failure could not be attached to the pane.",
                                ));
                            } else {
                                root.status = Some(message);
                            }
                            root.refresh_workspace_views(cx);
                            return true;
                        }

                        if root.workspace_runtime.set_status(&pane_id, None).is_err() {
                            root.status = Some(crate::locale::text(
                                "终端已就绪，但无法更新 pane 状态。",
                                "The terminal is ready, but pane state could not be updated.",
                            ));
                            return true;
                        }
                        let Some(_layout) = root.workspace_runtime.tab_layout(&tab_id).cloned() else {
                            let message = crate::locale::text(
                                "工作区缺少此 tab 的布局，终端保留为可关闭资源。",
                                "The workspace has no layout for this tab; the terminal remains available for cleanup.",
                            );
                            let close_state = match root.workspace_runtime.begin_close(&pane_id) {
                                Ok(closing)
                                    if matches!(
                                        root.workspace_runtime
                                            .mark_close_failed(&pane_id, &closing),
                                        Ok(true)
                                    ) => None,
                                Ok(_) | Err(_) => Some(crate::locale::text(
                                    "pane 生命周期状态未能回到可重试关闭状态。",
                                    "The pane could not be returned to a retryable close state.",
                                )),
                            };
                            let message = close_state.map_or_else(
                                || message.clone(),
                                |error| SharedString::from(format!("{message} {error}")),
                            );
                            if root
                                .workspace_runtime
                                .set_status(&pane_id, Some(message.clone()))
                                .is_err()
                            {
                                root.status = Some(crate::locale::text(
                                    "终端缺少布局且 pane 错误状态无法绑定。",
                                    "The terminal has no layout and its pane error state could not be recorded.",
                                ));
                            } else {
                                root.status = Some(message);
                            }
                            root.refresh_workspace_views(cx);
                            return true;
                        };

                        if let Some(controller) = root.workspace_controller.as_mut()
                            && controller
                                .restore_status(&WorkspaceRestoreItemId::Pane(pane_id.clone()))
                                .is_some()
                            && let Err(error) = controller.record_restore_result(
                                WorkspaceRestoreItemId::Pane(pane_id.clone()),
                                Ok(()),
                            )
                        {
                            root.status = Some(error.public_message().to_owned().into());
                        }
                        // SessionManager 已在返回 Ready 前登记此 session；停止后保留同一 UI owner，
                        // 由当前 close_all_until 或其重试回收，不再启动订阅、代理或 SFTP follow-up。
                        if root.host_shutdown_requested {
                            cx.notify();
                            return true;
                        }
                        let tab_already_persisted = root
                            .workspace_controller
                            .as_ref()
                            .is_some_and(|controller| {
                                controller
                                    .snapshot()
                                    .terminal_tabs
                                    .iter()
                                    .any(|tab| tab.id() == &tab_id)
                            });
                        let is_split_completion = root
                            .workspace_controller
                            .as_ref()
                            .and_then(|controller| controller.pending_resource_operation())
                            .is_some_and(|operation| {
                                matches!(operation,
                                    crate::features::workspace_controller::WorkspaceResourceOperation::CreatePane {
                                        tab_id: operation_tab,
                                        pane,
                                        ..
                                    } if operation_tab == &tab_id && pane.id == pane_id
                                )
                            });
                        let is_new_tab = !tab_already_persisted && !is_split_completion;
                        let activate_new_tab = is_new_tab
                            && root
                                .workspace_controller
                                .as_ref()
                                .and_then(|controller| controller.snapshot().active_tab_id.clone())
                                == active_tab_at_open
                            && root.center_panel == center_panel_at_open;
                        if let Some(previous) = root
                            .session_watch_cancellations
                            .insert(session_id.clone(), cancellation.clone())
                        {
                            previous.cancel();
                        }
                        if is_split_completion {
                            root.complete_workspace_split_after_open_ack(
                                &tab_id,
                                &pane_id,
                                generation,
                                cx,
                            );
                        } else {
                            root.persist_ready_workspace_pane(
                                &tab_id,
                                &pane_id,
                                activate_new_tab,
                                is_new_tab,
                                cx,
                            );
                        }
                        if root.status.as_ref().is_some_and(|status| {
                            status.as_ref() == "正在启动终端会话…"
                                || status.as_ref() == "Starting terminal session…"
                        }) {
                            root.status = None;
                        }
                        root.finish_workspace_restore(&pane_id, generation, cx);
                        root.refresh_session_reconnect_state(tab_id.clone(), pane_id.clone(), cx);
                        if is_new_tab
                            && direct_ssh_without_initial_command
                            && prepared.session.owner
                                == (SessionOwner::Ui {
                                    tab_id: tab_id.clone(),
                                    pane_id: pane_id.clone(),
                                })
                            && matches!(
                                &prepared.session.target,
                                SessionTarget::Ssh(target) if target.command.is_none()
                            )
                            && initial_proxy_binding
                                .as_ref()
                                .is_some_and(|binding| binding.session_id == session_id)
                            && root.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
                                pane.phase == RuntimePanePhase::Ready
                                    && pane.session_id.as_ref() == Some(&session_id)
                            })
                        {
                            initial_proxy_after_ready = initial_proxy_binding.clone();
                        }
                        if root
                            .workspace_runtime
                            .pane(&pane_id)
                            .is_some_and(|pane| pane.open_sftp_after_connect)
                        {
                            external_sftp_after_ready = Some(session_id.clone());
                        }
                        root.request_terminal_input_context_reconcile(
                            tab_id.clone(),
                            pane_id.clone(),
                            session_id.clone(),
                            true,
                            #[cfg(feature = "acceptance-metrics")]
                            fresh_terminal_probe_pending.clone(),
                            cx,
                        );
                        subscription = Some(prepared.subscription);
                        true
                    }
                    WorkspaceSessionOpenResult::Failed {
                        message,
                        cleanup_pending,
                    } => {
                        if is_new_tab_creation {
                            root.is_creating_session = false;
                        }
                        if let Some((session, snapshot)) = cleanup_pending {
                            let safe = crate::locale::text(
                                "终端初始化失败，资源关闭也未完成；请重试关闭。",
                                "Terminal initialization failed and cleanup is still pending; retry closing it.",
                            );
                            let message = SharedString::from(format!("{safe} {message}"));
                            if let Some(target) = unpublished_transient_local_container_target
                                .as_ref()
                                && retain_unpublished_transient_local_container_cleanup(
                                    root,
                                    PendingLocalContainerCleanup {
                                        tab_id: &tab_id,
                                        pane_id: &pane_id,
                                        target,
                                        session: &session,
                                        snapshot: snapshot.as_ref(),
                                        external_lease: orphan_lease.clone(),
                                        transient,
                                        status: message.clone(),
                                    },
                                    cx,
                                )
                            {
                                return true;
                            }
                            let session_id = session.session_id.clone();
                            let target = session.target.clone();
                            let session_size = session.size;
                            if root.workspace_runtime.retain_cleanup_session(
                                &pane_id,
                                session_id.clone(),
                                target.clone(),
                                session_size,
                                snapshot.clone(),
                                message.clone(),
                            ).is_err() {
                                let retained = root.workspace_runtime.retain_orphan_cleanup(
                                    OrphanCleanupInput {
                                        tab_id: tab_id.clone(),
                                        pane_id: pane_id.clone(),
                                        session_id,
                                        target,
                                        size: session_size,
                                        snapshot,
                                        status: message.clone(),
                                        external_lease: orphan_lease.clone(),
                                        transient,
                                    },
                                );
                                root.refresh_workspace_orphan_cleanup(cx);
                                root.status = Some(if retained.is_ok() {
                                    message.clone()
                                } else {
                                    crate::locale::text(
                                        "终端初始化失败且清理 owner 无法登记；工作区关闭不会提交。",
                                        "Terminal initialization failed and its cleanup owner could not be retained; workspace close will not commit.",
                                    )
                                });
                            } else {
                                root.status = Some(message.clone());
                            }
                            root.refresh_workspace_views(cx);
                        } else {
                            if let Some(target) = unpublished_transient_local_container_target
                                .as_ref()
                                .filter(|target| {
                                    can_discard_unpublished_transient_local_container_reservation(
                                        &root.workspace_runtime,
                                        &tab_id,
                                        &pane_id,
                                        target,
                                    )
                                })
                                && discard_unpublished_transient_local_container_reservation(
                                    &mut root.workspace_runtime,
                                    &tab_id,
                                    &pane_id,
                                    target,
                                )
                            {
                                root.cancel_workspace_restore(&pane_id);
                                root.status = Some(message);
                                root.refresh_workspace_views(cx);
                                cx.notify();
                                return true;
                            }
                            if root.workspace_runtime.mark_restore_failed(&pane_id).is_err()
                                || root.workspace_runtime.set_status(&pane_id, Some(message.clone())).is_err()
                            {
                                root.status = Some(crate::locale::text(
                                    "终端启动失败，pane 状态更新未完成。",
                                    "Terminal startup failed and the pane state could not be updated.",
                                ));
                            }
                            root.status = Some(message.clone());
                            if let Some(controller) = root.workspace_controller.as_mut()
                                && controller
                                    .restore_status(&WorkspaceRestoreItemId::Pane(pane_id.clone()))
                                    .is_some()
                                && let Err(error) = controller.record_restore_result(
                                    WorkspaceRestoreItemId::Pane(pane_id.clone()),
                                    Err(WorkspaceRestoreFailure::ResourceFailure),
                                )
                            {
                                root.status = Some(error.public_message().to_owned().into());
                            }
                            root.refresh_workspace_views(cx);
                        }
                        root.finish_workspace_restore(&pane_id, generation, cx);
                        false
                    }
                }
            });

            if accepted.as_ref().is_ok_and(|accepted| *accepted) {
                if let Some(binding) = initial_proxy_after_ready.take() {
                    let _ = this.update(cx, |root, cx| {
                        root.apply_proxy_policy_after_initial_ssh_ready(binding, cx);
                    });
                }
                if let Some(session_id) = external_sftp_after_ready.take() {
                    let root = this.clone();
                    let pane_id = pane_id.clone();
                    let _ = cx.update_window(window_handle, move |_, window, app| {
                        let _ = root.update(app, |root, cx| {
                            super::connections::open_external_sftp_after_connect(
                                root,
                                &pane_id,
                                &session_id,
                                generation,
                                window,
                                cx,
                            );
                        });
                    });
                }
            }

            if let Some(stale_result) = completion.take() {
                let stale_session = match stale_result {
                    WorkspaceSessionOpenResult::Ready(prepared) => Some((
                        prepared.session,
                        Some(prepared.snapshot),
                        crate::locale::text(
                            "恢复请求已过期；此终端待清理。",
                            "The restore request expired; this terminal requires cleanup.",
                        ),
                    )),
                    WorkspaceSessionOpenResult::Failed {
                        cleanup_pending: Some((session, snapshot)),
                        message,
                    } => Some((session, snapshot, message)),
                    WorkspaceSessionOpenResult::Failed { cleanup_pending: None, .. } => None,
                };
                if let Some((session, snapshot, message)) = stale_session {
                    let close_port = services.sessions.clone();
                    let close_id = session.session_id.clone();
                    let close_result = executor
                        .run(move |_| async move { close_port.close(close_id).await })
                        .await;
                    let cleanup_confirmed = close_result.is_ok();
                    let close_error = close_result.err();
                    let split_operation_id = split_operation_id.clone();
                    let split_tab_id = tab_id.clone();
                    let split_pane_id = pane_id.clone();
                    let _ = this.update(cx, |root, cx| {
                        if let Some(close_error) = close_error {
                            let full_message = SharedString::from(format!(
                                "{} {}: {}",
                                crate::locale::text(
                                    "会话关闭失败，资源仍由此 pane 保留：",
                                    "Session cleanup failed; the pane retains this resource:",
                                ),
                                message,
                                close_error.public_message(),
                            ));
                            let session_id = session.session_id.clone();
                            let target = session.target.clone();
                            let session_size = session.size;
                            // 代次失效意味着同一 PaneId 可能已进入新一轮恢复，迟到资源不绑定到新运行态。
                            let retained = root.workspace_runtime.retain_orphan_cleanup(
                                OrphanCleanupInput {
                                    tab_id: tab_id.clone(),
                                    pane_id: pane_id.clone(),
                                    session_id: session_id.clone(),
                                    target,
                                    size: session_size,
                                    snapshot: snapshot.clone(),
                                    status: full_message.clone(),
                                    external_lease: orphan_lease.clone(),
                                    transient,
                                },
                            );
                            root.refresh_workspace_orphan_cleanup(cx);
                            root.status = Some(if retained.is_ok() {
                                full_message.clone()
                            } else {
                                crate::locale::text(
                                    "过期会话关闭失败，无法保留清理 owner。",
                                    "A stale session could not be closed or retained for cleanup.",
                                )
                            });
                            root.refresh_workspace_views(cx);
                            cx.notify();
                        }
                        if let Some(operation_id) = split_operation_id.as_deref() {
                            if cleanup_confirmed {
                                root.schedule_cancelled_workspace_split_finalize(
                                    &split_tab_id,
                                    &split_pane_id,
                                    operation_id,
                                    cx,
                                );
                            }
                        } else {
                            root.complete_pending_workspace_close_after_restore_ack(&pane_id, cx);
                        }
                    });
                } else {
                    let split_operation_id = split_operation_id.clone();
                    let split_tab_id = tab_id.clone();
                    let split_pane_id = pane_id.clone();
                    let _ = this.update(cx, |root, cx| {
                        if let Some(operation_id) = split_operation_id.as_deref() {
                            root.schedule_cancelled_workspace_split_finalize(
                                &split_tab_id,
                                &split_pane_id,
                                operation_id,
                                cx,
                            );
                        } else {
                            root.complete_pending_workspace_close_after_restore_ack(&pane_id, cx);
                        }
                    });
                }
                return;
            }
            if !matches!(accepted, Ok(true)) || cleanup_pending.is_some() {
                if let Some((session, snapshot, message)) = cleanup_pending {
                    let _ = this.update(cx, |root, cx| {
                        if let Some(target) = unpublished_transient_local_container_target
                            .as_ref()
                            && retain_unpublished_transient_local_container_cleanup(
                                root,
                                PendingLocalContainerCleanup {
                                    tab_id: &tab_id,
                                    pane_id: &pane_id,
                                    target,
                                    session: &session,
                                    snapshot: snapshot.as_ref(),
                                    external_lease: orphan_lease.clone(),
                                    transient,
                                    status: message.clone(),
                                },
                                cx,
                            )
                        {
                            return;
                        }
                        let session_id = session.session_id.clone();
                        let target = session.target.clone();
                        let session_size = session.size;
                        let retained = root.workspace_runtime.retain_cleanup_session(
                            &pane_id,
                            session_id.clone(),
                            target.clone(),
                            session_size,
                            snapshot.clone(),
                            message.clone(),
                        );
                        root.status = Some(match retained {
                            Ok(()) => message,
                            Err(_) => {
                                let retained = root.workspace_runtime.retain_orphan_cleanup(
                                    OrphanCleanupInput {
                                        tab_id: tab_id.clone(),
                                        pane_id: pane_id.clone(),
                                        session_id,
                                        target,
                                        size: session_size,
                                        snapshot,
                                        status: message.clone(),
                                        external_lease: orphan_lease.clone(),
                                        transient,
                                    },
                                );
                                root.refresh_workspace_orphan_cleanup(cx);
                                if retained.is_ok() {
                                    message
                                } else {
                                    crate::locale::text(
                                        "终端已创建，但清理 owner 无法登记；关闭不会提交。",
                                        "The terminal was created, but its cleanup owner could not be retained; close will not commit.",
                                    )
                                }
                            }
                        });
                        root.refresh_workspace_views(cx);
                        cx.notify();
                    });
                }
                return;
            }

            let Some(mut updates) = subscription else {
                return;
            };
            let Some(watch_session_id) = this
                .update(cx, |root, _| {
                    root.workspace_runtime.session_for_pane(&pane_id).cloned()
                })
                .ok()
                .flatten()
            else {
                return;
            };
            #[cfg(feature = "acceptance-metrics")]
            let mut watch_session_generation = this
                .update(cx, |root, _| {
                    root.terminal_view_bindings
                        .get(&watch_session_id)
                        .map(|binding| binding.session_generation)
                })
                .ok()
                .flatten();
            loop {
                let change = match updates.recv(&cancellation).await {
                    Ok(Some(change)) => change,
                    Ok(None) | Err(_) => break,
                };
                // The initial read can race async handler installation; retry only while absent,
                // then stamp later events immediately after recv without another UI round trip.
                #[cfg(feature = "acceptance-metrics")]
                if watch_session_generation.is_none() {
                    watch_session_generation = this
                        .update(cx, |root, _| {
                            root.terminal_view_bindings
                                .get(&watch_session_id)
                                .map(|binding| binding.session_generation)
                        })
                        .ok()
                        .flatten();
                }
                #[cfg(feature = "acceptance-metrics")]
                if let Some(session_generation) = watch_session_generation {
                    crate::acceptance_metrics::terminal_echo_probe_registry()
                        .record_phase_for_session(
                            watch_session_id.as_str(),
                            session_generation,
                            crate::acceptance_metrics::TerminalEchoPhase::WatchReceived,
                            Some(change.revision),
                        );
                }
                if matches!(change.kind, SessionChangeKind::Lifecycle | SessionChangeKind::Closed)
                {
                    if change.kind == SessionChangeKind::Closed {
                        let pane_owner_request = this
                            .update(cx, |root, _| {
                                if root.workspace_runtime.session_for_pane(&pane_id)
                                    != Some(&watch_session_id)
                                {
                                    return None;
                                }
                                let owner = SessionOwner::Ui {
                                    tab_id: tab_id.clone(),
                                    pane_id: pane_id.clone(),
                                };
                                let request_generation = next_lifecycle_context_request_generation(
                                    &mut root.pane_input_binding_request_generation,
                                );
                                root.pane_input_binding_requests.insert(
                                    watch_session_id.clone(),
                                    (request_generation, owner),
                                );
                                Some(())
                            })
                            .ok()
                            .flatten();
                        if pane_owner_request.is_some() {
                            let _ = this.update(cx, |root, cx| {
                                if root.workspace_runtime.session_for_pane(&pane_id)
                                    == Some(&watch_session_id)
                                {
                                    root.invalidate_pane_input_handler(&watch_session_id, cx);
                                    root.record_closed_session_for_container_panel(
                                        &tab_id,
                                        &pane_id,
                                        &watch_session_id,
                                    );
                                    root.refresh_proxy_panel(cx);
                                    // 右栏可能仍在主机详情页可见，不能等待中央终端下一次 snapshot 才撤销旧绑定。
                                    root.refresh_operations_panel_targets(cx);
                                    cx.notify();
                                }
                            });
                        }
                    } else {
                        // Ready 与后续 lifecycle 共用同一查询/回执门，避免重连和退出规则分叉。
                        let _ = this.update(cx, |root, cx| {
                            root.request_terminal_input_context_reconcile(
                                tab_id.clone(),
                                pane_id.clone(),
                                watch_session_id.clone(),
                                false,
                                #[cfg(feature = "acceptance-metrics")]
                                fresh_terminal_probe_pending.clone(),
                                cx,
                            );
                        });
                    }
                    let owner = this
                        .update(cx, |root, _| {
                            root.container_exec_refresh_targets
                                .contains_key(&pane_id)
                                .then(|| SessionOwner::Ui {
                                    tab_id: tab_id.clone(),
                                    pane_id: pane_id.clone(),
                                })
                        })
                        .ok()
                        .flatten();
                    if let Some(owner) = owner {
                        let terminal_ended = if change.kind == SessionChangeKind::Closed {
                            true
                        } else {
                            let sessions = lifecycle_sessions.clone();
                            let filter = SessionFilter {
                                owner: Some(owner),
                                target: None,
                            };
                            let current = lifecycle_executor
                                .run(move |_task_cancellation| async move {
                                    sessions.list(filter).await
                                })
                                .await
                                .ok()
                                .and_then(|sessions| {
                                    sessions
                                        .into_iter()
                                        .find(|session| session.session_id == watch_session_id)
                                });
                            current.is_some_and(|session| {
                                matches!(
                                    session.lifecycle,
                                    SessionLifecycle::Exited
                                        | SessionLifecycle::Closed
                                        | SessionLifecycle::Failed
                                )
                            })
                        };
                        if terminal_ended {
                            let _ = this.update(cx, |root, cx| {
                                if root.workspace_runtime.session_for_pane(&pane_id)
                                    == Some(&watch_session_id)
                                {
                                    root.mark_container_exec_target_dirty(&pane_id, cx);
                                }
                            });
                        }
                    }
                }
                if matches!(change.kind, SessionChangeKind::Output | SessionChangeKind::Lifecycle) {
                    let source = this
                        .update(cx, |root, _| {
                            let visible = root.center_panel == super::CenterPanel::Terminal
                                && root.workspace_controller.as_ref().is_some_and(|controller| {
                                    controller.snapshot().active_tab_id.as_ref() == Some(&tab_id)
                                })
                                && super::preferences::interaction_policy(root)
                                    .is_some_and(|policy| policy.command_block_rail_visible(true));
                            (visible
                                && root.workspace_runtime.session_for_pane(&pane_id)
                                    == Some(&watch_session_id))
                            .then(|| root.workspace_tools.get(&watch_session_id).cloned())
                            .flatten()
                        })
                        .ok()
                        .flatten();
                    if let Some(source) = source {
                        let _ = this.update(cx, |root, cx| {
                            if root.workspace_runtime.session_for_pane(&pane_id)
                                == Some(&watch_session_id)
                            {
                                super::command_blocks::refresh_from_event(
                                    root,
                                    source,
                                    watch_session_id.clone(),
                                    cx,
                                );
                            }
                        });
                    }
                }
                let session_is_current = this
                    .update(cx, |root, _| {
                        root.workspace_runtime
                            .snapshot_session_is_current(&pane_id, &watch_session_id)
                    })
                    .unwrap_or(false);
                if session_is_current {
                    let _ = this.update(cx, |root, cx| {
                        root.request_workspace_pane_snapshot(
                            tab_id.clone(),
                            pane_id.clone(),
                            cx,
                        );
                    });
                } else {
                    break;
                }
            }
        })
        .detach();
        Ok(())
    }

    /// 更新只读投影且先安装组拓扑再安装标签，避免同批组变化撤销新 active 标签的测量请求。
    /// 无窗口的恢复回调不创建 GPUI Entity，真实会话和布局所有权仍留在 controller；隐藏搜索先撤销计时和提交门。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_workspace_views(&mut self, cx: &mut Context<RootView>) {
        self.reconcile_workspace_search_visibility(cx);
        let Some(controller) = self.workspace_controller.as_ref() else {
            let mut tab_projections = Vec::new();
            if self.sftp_workbench_tab_open {
                append_shared_sftp_workbench_projection(
                    &mut tab_projections,
                    &self.sftp_workbench_tab_id,
                    self.shared_sftp_workbench_is_selected(),
                );
            }
            self.workspace_tabs_view.update(cx, |tabs, cx| {
                tabs.set_groups(Vec::new(), cx);
                tabs.set_projection(tab_projections, cx);
            });
            self.refresh_agent_target_candidate(cx);
            self.refresh_context_panel_projection(cx);
            cx.notify();
            return;
        };

        let snapshot = controller.snapshot().clone();
        let pending_split = self.pending_workspace_split_preview(cx);
        let mut tab_projections = controller.tab_projections();
        if self.sftp_workbench_tab_open {
            while tab_projections
                .iter()
                .any(|tab| tab.id == self.sftp_workbench_tab_id)
            {
                self.sftp_workbench_tab_id = TabId::new();
            }
            append_shared_sftp_workbench_projection(
                &mut tab_projections,
                &self.sftp_workbench_tab_id,
                self.shared_sftp_workbench_is_selected(),
            );
        }
        let group_projections = controller.tab_group_projections();
        self.workspace_tabs_view.update(cx, |tabs, cx| {
            tabs.set_groups(group_projections, cx);
            tabs.set_projection(tab_projections, cx);
        });

        for tab in snapshot.terminal_tabs.iter() {
            let kerminal_application::workspace::WorkspaceSessionTab::Terminal {
                id: tab_id,
                layout,
                ..
            } = tab
            else {
                continue;
            };
            let Some((view_layout, focused, preview)) = Self::workspace_render_layout_projection(
                &snapshot,
                tab_id,
                layout,
                pending_split.as_ref(),
            ) else {
                continue;
            };
            if !view_layout.contains_pane(&focused) {
                continue;
            }
            let projections = self.workspace_pane_projections(&view_layout, cx);
            let candidate_interaction_locked = preview
                .as_ref()
                .is_some_and(|preview| self.workspace_split_candidate_interaction_locked(preview));
            let host_picker = self.split_target_picker_projection(tab_id, cx);
            if let Some(render) = self.workspace_renders.get(tab_id) {
                let source_bounds = preview.as_ref().and_then(|preview| {
                    render
                        .read(cx)
                        .measured_pane_bounds(&preview.source_pane_id, cx)
                });
                render.update(cx, |render, cx| {
                    render.set_projection(
                        super::workspace_render::WorkspaceRenderProjection {
                            layout: view_layout,
                            focused_pane: focused,
                            panes: projections,
                            candidate: super::workspace_render::SplitCandidateRenderContext {
                                preview,
                                source_bounds,
                                interaction_locked: candidate_interaction_locked,
                            },
                            host_picker,
                        },
                        cx,
                    );
                });
            }
        }
        self.refresh_agent_target_candidate(cx);
        self.refresh_context_panel_projection(cx);
        cx.notify();
    }

    /// CAS 保存唯一 typed snapshot；transient 资源先归一化，再通过同一 application service 落盘。
    pub(super) fn persist_workspace_snapshot(&mut self, cx: &mut Context<RootView>) {
        self.persist_workspace_snapshot_with_completion(|_, _, _| {}, cx);
    }

    /// 以唯一 CAS 保存口回报退出门控；保存期间新同步的快照在旧回执后仍为 Dirty 时再排一次 CAS。
    pub(super) fn persist_workspace_snapshot_with_completion(
        &mut self,
        completion: impl FnOnce(&mut RootView, WorkspaceSnapshotSaveCompletion, &mut Context<RootView>)
        + 'static,
        cx: &mut Context<RootView>,
    ) {
        let Some(workspace_service) = self.services.workspace_sessions.clone() else {
            completion(
                self,
                WorkspaceSnapshotSaveCompletion::Failed(crate::locale::text(
                    "工作区持久化服务不可用。",
                    "Workspace persistence is unavailable.",
                )),
                cx,
            );
            return;
        };
        let Some(controller) = self.workspace_controller.as_ref() else {
            completion(
                self,
                WorkspaceSnapshotSaveCompletion::Failed(crate::locale::text(
                    "工作区状态尚未完成初始化。",
                    "Workspace state has not been initialized.",
                )),
                cx,
            );
            return;
        };
        let snapshot = controller.snapshot().clone();
        let (transient_tabs, transient_panes) = self.workspace_runtime.transient_ids();
        let persisted_snapshot = match super::workspace_transient::without_transient_resources(
            &snapshot,
            &transient_tabs,
            &transient_panes,
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.status = Some(error.public_message().into());
                cx.notify();
                completion(
                    self,
                    WorkspaceSnapshotSaveCompletion::Failed(
                        error.public_message().to_owned().into(),
                    ),
                    cx,
                );
                return;
            }
        };
        let request = match self
            .workspace_controller
            .as_mut()
            .map(|controller| controller.begin_save_projected(persisted_snapshot))
        {
            Some(Ok(Some(request))) => request,
            Some(Ok(None)) => {
                completion(
                    self,
                    if self
                        .workspace_controller
                        .as_ref()
                        .is_some_and(|controller| controller.has_unsaved_or_pending_work())
                    {
                        WorkspaceSnapshotSaveCompletion::Busy
                    } else {
                        WorkspaceSnapshotSaveCompletion::NoChanges
                    },
                    cx,
                );
                return;
            }
            None => {
                completion(
                    self,
                    WorkspaceSnapshotSaveCompletion::Failed(crate::locale::text(
                        "工作区状态已在保存前关闭。",
                        "Workspace state became unavailable before saving.",
                    )),
                    cx,
                );
                return;
            }
            Some(Err(error)) => {
                self.status = Some(error.public_message().to_owned().into());
                cx.notify();
                completion(
                    self,
                    WorkspaceSnapshotSaveCompletion::Failed(
                        error.public_message().to_owned().into(),
                    ),
                    cx,
                );
                return;
            }
        };
        let executor = self.services.executor.clone();
        let request_id = request.request_id.clone();
        let base_document = request.base_document.clone();
        let expected_revision = request.expected_revision.clone();
        let base_snapshot = request.base_snapshot.clone();
        let desired_snapshot = request.desired_snapshot.clone();
        let current_desired = desired_snapshot.clone();
        cx.spawn(async move |this, cx| {
            let save_service = workspace_service.clone();
            let result = executor
                .run(move |task_cancel| async move {
                    save_service
                        .save_workspace_snapshot(
                            task_cancel,
                            &base_document,
                            expected_revision,
                            &base_snapshot,
                            &desired_snapshot,
                        )
                        .await
                })
                .await;
            let mut reread = None;
            if matches!(&result, Ok(WorkspaceSessionSaveOutcome::OutcomeUnknown)) {
                let reload = workspace_service.clone();
                let read = executor
                    .run(move |task_cancel| async move { reload.load(task_cancel).await })
                    .await;
                reread = Some(read);
            }
            let _ = this.update(cx, |root, cx| {
                let Some(controller) = root.workspace_controller.as_mut() else {
                    completion(
                        root,
                        WorkspaceSnapshotSaveCompletion::Failed(crate::locale::text(
                            "工作区状态在保存期间被卸载。",
                            "Workspace state was unloaded while its save was running.",
                        )),
                        cx,
                    );
                    return;
                };
                let completion_result = match result {
                    Ok(WorkspaceSessionSaveOutcome::OutcomeUnknown) => match reread {
                        Some(Ok(WorkspaceSessionLoad::Loaded { document, revision }))
                            if document.workspace_snapshot().ok().as_ref()
                                == Some(&current_desired) =>
                        {
                            let latest = WorkspaceSessionLoad::Loaded { document, revision };
                            let finish_result = controller.finish_save(
                                &request_id,
                                WorkspaceSessionSaveOutcome::OutcomeUnknown,
                            );
                            if let Err(error) = finish_result {
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            } else if let Err(error) =
                                controller.resolve_unknown_readback(latest.clone())
                            {
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            } else {
                                root.workspace_session_state = Some(latest);
                                if controller.has_unsaved_or_pending_work() {
                                    WorkspaceSnapshotSaveCompletion::Busy
                                } else {
                                    WorkspaceSnapshotSaveCompletion::Saved
                                }
                            }
                        }
                        Some(Ok(latest)) => {
                            let finish_result = controller.finish_save(
                                &request_id,
                                WorkspaceSessionSaveOutcome::OutcomeUnknown,
                            );
                            if let Err(error) = finish_result {
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            } else if let Err(error) =
                                controller.resolve_unknown_readback(latest.clone())
                            {
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            } else {
                                root.workspace_session_state = Some(latest);
                                match controller.persistence_state() {
                                    WorkspacePersistenceState::Conflict { .. } => {
                                        WorkspaceSnapshotSaveCompletion::Conflict
                                    }
                                    WorkspacePersistenceState::ReadbackConfirmedUncommitted {
                                        ..
                                    } => {
                                        WorkspaceSnapshotSaveCompletion::ReadbackConfirmedUncommitted
                                    }
                                    _ => WorkspaceSnapshotSaveCompletion::OutcomeUnknown,
                                }
                            }
                        }
                        Some(Err(error)) => {
                            let finish_result = controller.finish_save(
                                &request_id,
                                WorkspaceSessionSaveOutcome::OutcomeUnknown,
                            );
                            root.status = Some(crate::locale::text(
                                error.public_message(),
                                "Workspace save status is unknown and could not be read back.",
                            ));
                            match finish_result {
                                Ok(()) => WorkspaceSnapshotSaveCompletion::OutcomeUnknown,
                                Err(finish_error) => WorkspaceSnapshotSaveCompletion::Failed(
                                    finish_error.public_message().to_owned().into(),
                                ),
                            }
                        }
                        None => WorkspaceSnapshotSaveCompletion::OutcomeUnknown,
                    },
                    Ok(outcome) => {
                        let saved = matches!(&outcome, WorkspaceSessionSaveOutcome::Saved { .. });
                        let conflict =
                            matches!(&outcome, WorkspaceSessionSaveOutcome::Conflict { .. });
                        let latest = match &outcome {
                            WorkspaceSessionSaveOutcome::Conflict { latest } => Some(latest.clone()),
                            _ => None,
                        };
                        match controller.finish_save(&request_id, outcome) {
                            Ok(()) if saved => {
                                if controller.has_unsaved_or_pending_work() {
                                    WorkspaceSnapshotSaveCompletion::Busy
                                } else {
                                    WorkspaceSnapshotSaveCompletion::Saved
                                }
                            }
                            Ok(()) if conflict => {
                                root.workspace_session_state = latest;
                                WorkspaceSnapshotSaveCompletion::Conflict
                            }
                            Ok(()) => WorkspaceSnapshotSaveCompletion::OutcomeUnknown,
                            Err(error) => {
                                root.status = Some(error.public_message().to_owned().into());
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            }
                        }
                    }
                    Err(error) => {
                        match controller.finish_save_failure(&request_id) {
                            Ok(()) => {
                                root.status = Some(error.public_message().to_owned().into());
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    error.public_message().to_owned().into(),
                                )
                            }
                            Err(finish_error) => {
                                root.status = Some(finish_error.public_message().to_owned().into());
                                WorkspaceSnapshotSaveCompletion::Failed(
                                    finish_error.public_message().to_owned().into(),
                                )
                            }
                        }
                    }
                };

                // finish_save compares the full source snapshot and leaves later UI edits dirty.
                let retry_changed_snapshot = matches!(
                    &completion_result,
                    WorkspaceSnapshotSaveCompletion::Busy
                ) && root.workspace_controller.as_ref().is_some_and(|controller| {
                    matches!(
                        controller.persistence_state(),
                        WorkspacePersistenceState::Dirty
                    ) && controller.pending_resource_operation().is_none()
                });
                root.refresh_workspace_views(cx);
                cx.notify();
                completion(root, completion_result, cx);
                if retry_changed_snapshot
                    && root.workspace_controller.as_ref().is_some_and(|controller| {
                        matches!(
                            controller.persistence_state(),
                            WorkspacePersistenceState::Dirty
                        ) && controller.pending_resource_operation().is_none()
                    })
                {
                    root.persist_workspace_snapshot(cx);
                }
            });
        })
        .detach();
    }

    /// 恢复完成只清理对应的启动提示，不能覆盖保存失败或其它 pane 的错误状态。
    pub(super) fn finish_workspace_restore(
        &mut self,
        pane_id: &PaneId,
        generation: u64,
        cx: &mut Context<RootView>,
    ) {
        if !self.workspace_restore_is_current(pane_id, generation) {
            return;
        }
        self.refresh_workspace_views(cx);
        if self
            .workspace_runtime
            .pane(pane_id)
            .is_some_and(|pane| pane.phase == RuntimePanePhase::Ready)
            && self.status.as_ref().is_some_and(|status| {
                status.as_ref() == "正在启动终端会话…"
                    || status.as_ref() == "Starting terminal session…"
            })
        {
            self.status = None;
        }
        cx.notify();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_open_lifecycle.rs"]
mod lifecycle_context_tests;

/// 只为新建且不可持久恢复的本机容器 exec 预留返回清理特例，SSH 与普通 pane 仍走原有 owner 流程。
///
/// @author kongweiguang
pub(super) fn unpublished_transient_local_container_target(
    kind: &WorkspaceTerminalOpenKind,
    transient: bool,
    is_new_tab_creation: bool,
) -> Option<kerminal_domain::operations::ContainerTarget> {
    if !transient || !is_new_tab_creation {
        return None;
    }
    match kind {
        WorkspaceTerminalOpenKind::ContainerExec { target, .. } if target.host_id.is_none() => {
            Some(target.clone())
        }
        WorkspaceTerminalOpenKind::Direct(_)
        | WorkspaceTerminalOpenKind::ContainerExec { .. }
        | WorkspaceTerminalOpenKind::TmuxAttach { .. } => None,
    }
}

/// 先把未能关闭的本机容器 session 交给全局 orphan 清理 owner，再删除无 session 的独占预留。
/// 任何身份不符都退回原有 pane owner 流程；取消不会伪装成关闭成功，也不会重放 exec。
///
/// @author kongweiguang
fn retain_unpublished_transient_local_container_cleanup(
    root: &mut RootView,
    cleanup: PendingLocalContainerCleanup<'_>,
    cx: &mut Context<RootView>,
) -> bool {
    if !failed_local_container_session_matches(
        cleanup.session,
        cleanup.tab_id,
        cleanup.pane_id,
        cleanup.target,
    ) || !can_discard_unpublished_transient_local_container_reservation(
        &root.workspace_runtime,
        cleanup.tab_id,
        cleanup.pane_id,
        cleanup.target,
    ) {
        return false;
    }

    let retained = root
        .workspace_runtime
        .retain_orphan_cleanup(OrphanCleanupInput {
            tab_id: cleanup.tab_id.clone(),
            pane_id: cleanup.pane_id.clone(),
            session_id: cleanup.session.session_id.clone(),
            target: cleanup.session.target.clone(),
            size: cleanup.session.size,
            snapshot: cleanup.snapshot.cloned(),
            status: cleanup.status.clone(),
            external_lease: cleanup.external_lease,
            transient: cleanup.transient,
        });
    if retained.is_err() {
        return false;
    }

    let reservation_removed = discard_unpublished_transient_local_container_reservation(
        &mut root.workspace_runtime,
        cleanup.tab_id,
        cleanup.pane_id,
        cleanup.target,
    );
    root.cancel_workspace_restore(cleanup.pane_id);
    root.refresh_workspace_orphan_cleanup(cx);
    root.status = Some(if reservation_removed {
        cleanup.status
    } else {
        crate::locale::text(
            "清理 owner 已保留，但本机容器 pane 预留未能安全移除。",
            "The cleanup owner was retained, but the local container pane reservation could not be safely removed.",
        )
    });
    root.refresh_workspace_views(cx);
    cx.notify();
    true
}

/// 检查 runtime 是否仍只包含指定本机临时容器的新建空 pane，避免清理时误删已绑定资源或其它布局。
///
/// @author kongweiguang
pub(super) fn can_discard_unpublished_transient_local_container_reservation(
    runtime: &WorkspaceRuntime,
    tab_id: &TabId,
    pane_id: &PaneId,
    target: &kerminal_domain::operations::ContainerTarget,
) -> bool {
    let Some(pane) = runtime.pane(pane_id) else {
        return false;
    };
    let Some(layout) = runtime.tab_layout(tab_id) else {
        return false;
    };
    let layout_panes = layout.pane_ids();
    &pane.tab_id == tab_id
        && pane.transient
        && pane.persisted.mode == WorkspacePaneMode::Container
        && pane.persisted.target.is_none()
        && pane.persisted.remote_host_id.is_none()
        && target.host_id.is_none()
        && pane.persisted.container_id.as_deref() == Some(target.container_id.as_str())
        && pane.session_id.is_none()
        && pane.target.is_none()
        && matches!(
            pane.phase,
            RuntimePanePhase::RestorePending | RuntimePanePhase::RestoreFailed
        )
        && layout_panes.len() == 1
        && layout_panes.first() == Some(pane_id)
}

/// 仅在 owner 转移成功后移除精确的空预留，并一并清理其独占 tab 布局。
///
/// @author kongweiguang
pub(super) fn discard_unpublished_transient_local_container_reservation(
    runtime: &mut WorkspaceRuntime,
    tab_id: &TabId,
    pane_id: &PaneId,
    target: &kerminal_domain::operations::ContainerTarget,
) -> bool {
    if !can_discard_unpublished_transient_local_container_reservation(
        runtime, tab_id, pane_id, target,
    ) || !matches!(runtime.remove_empty_pane(pane_id), Ok(true))
    {
        return false;
    }
    runtime.remove_tab_layout(tab_id);
    true
}

/// 只允许与本机容器 exec 同一 tab、pane、session owner 和容器 ID 的回执转入 orphan 清理。
///
/// @author kongweiguang
pub(super) fn failed_local_container_session_matches(
    session: &SessionInfo,
    tab_id: &TabId,
    pane_id: &PaneId,
    target: &kerminal_domain::operations::ContainerTarget,
) -> bool {
    session.owner
        == (SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        })
        && matches!(
            &session.target,
            SessionTarget::Container(session_target)
                if session_target.host_id.as_ref() == target.host_id.as_ref()
                    && session_target.runtime == target.runtime
                    && session_target.container_id.as_str() == target.container_id.as_str()
        )
}

/// 通过已装配的 application service 执行 Direct、容器 exec 或 tmux attach；显示配置合并传递以限制参数散布。
/// @author kongweiguang
async fn prepare_workspace_session(
    sessions: Arc<dyn SessionService>,
    operations: Option<Arc<OperationsService>>,
    kind: WorkspaceTerminalOpenKind,
    owner: SessionOwner,
    size: TerminalSize,
    presentation: TerminalSessionPresentation,
    cancellation: CancellationToken,
) -> WorkspaceSessionOpenResult {
    let session = match kind {
        WorkspaceTerminalOpenKind::Direct(target) => {
            sessions
                .create_cancellable(
                    CreateSessionRequest {
                        target,
                        owner,
                        size,
                    },
                    cancellation.clone(),
                )
                .await
        }
        WorkspaceTerminalOpenKind::ContainerExec { target, command } => {
            let Some(operations) = operations else {
                return WorkspaceSessionOpenResult::Failed {
                    message: crate::locale::text(
                        "容器终端服务尚未配置。",
                        "Container terminal service is unavailable.",
                    ),
                    cleanup_pending: None,
                };
            };
            operations
                .containers
                .open_exec(target, command, owner, size, cancellation.clone())
                .await
        }
        WorkspaceTerminalOpenKind::TmuxAttach {
            target, session_id, ..
        } => {
            let Some(operations) = operations else {
                return WorkspaceSessionOpenResult::Failed {
                    message: crate::locale::text(
                        "tmux 终端服务尚未配置。",
                        "tmux terminal service is unavailable.",
                    ),
                    cleanup_pending: None,
                };
            };
            operations
                .tmux
                .attach_session(target, session_id, owner, size, cancellation.clone())
                .await
        }
    };
    let session = match session {
        Ok(session) => session,
        Err(error) => {
            return WorkspaceSessionOpenResult::Failed {
                message: error.public_message().to_owned().into(),
                cleanup_pending: None,
            };
        }
    };

    let session_id = session.session_id.clone();
    let setup = async {
        sessions
            .set_palette(session_id.clone(), presentation.palette)
            .await?;
        sessions
            .set_scrollback_limit(session_id.clone(), presentation.scrollback)
            .await?;
        let subscription = sessions.subscribe(session_id.clone()).await?;
        let snapshot = sessions
            .render_snapshot(session_id.clone(), RenderSnapshotRequest::default())
            .await?;
        Ok::<_, AppError>((subscription, snapshot))
    }
    .await;
    match setup {
        Ok((subscription, snapshot)) => {
            WorkspaceSessionOpenResult::Ready(PreparedWorkspaceSession {
                session,
                snapshot,
                subscription,
            })
        }
        Err(error) => {
            let message = SharedString::from(error.public_message());
            match sessions.close(session_id).await {
                Ok(()) => WorkspaceSessionOpenResult::Failed {
                    message,
                    cleanup_pending: None,
                },
                Err(cleanup_error) => {
                    let snapshot = sessions
                        .render_snapshot(
                            session.session_id.clone(),
                            RenderSnapshotRequest::default(),
                        )
                        .await
                        .ok();
                    let safe = crate::locale::text(
                        "终端初始化失败，资源关闭仍待重试。",
                        "Terminal initialization failed; resource cleanup still needs retry.",
                    );
                    WorkspaceSessionOpenResult::Failed {
                        message: SharedString::from(format!(
                            "{safe} {}",
                            cleanup_error.public_message()
                        )),
                        cleanup_pending: Some((session, snapshot)),
                    }
                }
            }
        }
    }
}
