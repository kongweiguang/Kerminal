//! 恢复持久化工作区，并把已建立的终端资源接入 Root 运行态。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, ffi::OsString, path::PathBuf};

use gpui_kit::{AppContext as _, Context, SharedString};
use kerminal_application::{
    AppError, AppResult,
    connections::management::HostManagementService,
    preferences::ProfileService,
    workspace::{
        WorkspaceContainerRuntime, WorkspacePaneMode, WorkspaceSessionLoad, WorkspaceSessionPane,
        WorkspaceSessionTab, WorkspaceTargetRef, WorkspaceTargetUnavailableReason,
        WorkspaceTmuxBinding,
    },
};
use kerminal_domain::{
    connections::RemoteProtocol,
    files::ContainerRuntime,
    identity::{PaneId, TabId},
    operations::{ContainerTarget, TmuxTarget},
    sessions::{
        ContainerSessionTarget, LocalSessionTarget, SessionTarget, SshInitialCommand,
        SshSessionTarget, TelnetSessionTarget, TerminalSize,
    },
    workspace::TerminalLayoutNode,
};
use tokio_util::sync::CancellationToken;

use super::{
    CenterPanel, RootView, WorkspaceTerminalOpenKind,
    workspace_runtime::{RuntimePanePhase, WorkspaceRuntime},
};
use crate::features::workspace_controller::{
    WorkspaceController, WorkspaceControllerEffect, WorkspacePersistenceState,
    WorkspaceRestoreFailure, WorkspaceRestoreItemId,
};

/// 供 operations/UI owner 使用的资源预留回执；ID 与恢复代次在 Root 内一次生成。
pub(super) struct WorkspaceTerminalReservation {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub pane: WorkspaceSessionPane,
    pub layout: TerminalLayoutNode,
    pub size: TerminalSize,
    pub generation: u64,
    pub cancellation: CancellationToken,
    pub transient: bool,
}

impl RootView {
    /// 先登记稳定 pane 身份并返回唯一代次，避免并行功能各自维护临时 session registry。
    pub(super) fn reserve_new_workspace_terminal_tab(
        &mut self,
        mut pane: WorkspaceSessionPane,
        title: String,
        size: TerminalSize,
        transient: bool,
        cx: &mut Context<RootView>,
    ) -> Result<WorkspaceTerminalReservation, SharedString> {
        if self.workspace_load_pending {
            return Err(crate::locale::text(
                "正在读取工作区，请稍后再创建终端。",
                "The workspace is still loading. Try creating a terminal again shortly.",
            ));
        }
        let Some(controller) = self.workspace_controller.as_ref() else {
            return Err(crate::locale::text(
                "工作区处于只读恢复状态；请先解决配置问题或显式迁移后再创建终端。",
                "The workspace is in read-only recovery. Resolve or explicitly migrate it before creating a terminal.",
            ));
        };
        if controller.pending_resource_operation().is_some()
            || matches!(
                controller.persistence_state(),
                WorkspacePersistenceState::Conflict { .. }
                    | WorkspacePersistenceState::OutcomeUnknown { .. }
                    | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
            )
        {
            return Err(crate::locale::text(
                "工作区有未完成的资源或保存冲突；请先处理后再创建终端。",
                "The workspace has a pending resource change or save conflict. Resolve it before creating a terminal.",
            ));
        }

        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        pane.id = pane_id.clone();
        pane.title = title;
        let layout = self.services.workspace.new_terminal_layout(pane_id.clone());
        self.services
            .workspace
            .validate_layout(&layout)
            .map_err(|error| SharedString::from(error.public_message().to_owned()))?;
        self.workspace_runtime
            .register_pending_pane(tab_id.clone(), pane.clone())
            .map_err(|_| SharedString::from("终端 pane 身份已被占用。"))?;
        self.workspace_runtime
            .register_tab_layout(tab_id.clone(), layout.clone());
        let (generation, cancellation) = self.begin_workspace_restore(&pane_id);
        self.refresh_workspace_views(cx);
        Ok(WorkspaceTerminalReservation {
            tab_id,
            pane_id,
            pane,
            layout,
            size,
            generation,
            cancellation,
            transient,
        })
    }
}

/// 首次载入时投影持久活动文件标签；只在既有用户导航/焦点代次仍为初值时使用默认终端页。
/// 这避免异步恢复覆盖启动期间的显式导航；资源仍由原 TabId 的文件恢复流程拥有，不创建会话或猜目标。
///
/// @author kongweiguang
fn initial_workspace_file_center_panel(
    initial_install: bool,
    pane_focus_intent_generation: u64,
    current_center_panel: CenterPanel,
    active_tab: Option<&WorkspaceSessionTab>,
) -> Option<CenterPanel> {
    if !initial_install
        || pane_focus_intent_generation != 0
        || current_center_panel != CenterPanel::Terminal
    {
        return None;
    }

    match active_tab? {
        WorkspaceSessionTab::SftpTransfer { .. } => Some(CenterPanel::Files),
        WorkspaceSessionTab::WorkspaceFile { .. } => Some(CenterPanel::Editor),
        WorkspaceSessionTab::Terminal { .. } | WorkspaceSessionTab::Unsupported { .. } => None,
    }
}

/// 从缺失或已加载文档建立同一套 typed reducer/runtime；无效或不支持版本保持只读。
impl RootView {
    /// 保留仓储分类并只为有效 v3 创建恢复状态；活跃 pane 首次获得真实画布尺寸后才启动恢复 PTY。
    /// 活动文件标签仅恢复既有中央投影，启动期间的显式导航优先，不在此另建文件资源或改绑目标。
    ///
    /// @author kongweiguang
    pub(super) fn install_workspace_load(
        &mut self,
        load: WorkspaceSessionLoad,
        cx: &mut Context<RootView>,
    ) {
        let previous_load = self.workspace_session_state.clone();
        self.workspace_session_error = None;
        self.workspace_session_state = Some(load.clone());

        let controller = match load {
            WorkspaceSessionLoad::Missing => WorkspaceController::from_missing(),
            WorkspaceSessionLoad::Loaded { document, revision } => {
                WorkspaceController::from_loaded(document, revision)
            }
            WorkspaceSessionLoad::Invalid { .. } | WorkspaceSessionLoad::Unsupported { .. } => {
                self.workspace_controller = None;
                self.workspace_runtime = WorkspaceRuntime::default();
                self.refresh_workspace_views(cx);
                return;
            }
        };

        let controller = match controller {
            Ok(controller) => controller,
            Err(error) => {
                self.workspace_controller = None;
                self.workspace_runtime = WorkspaceRuntime::default();
                self.workspace_session_error = Some(error.public_message().into());
                self.refresh_workspace_views(cx);
                return;
            }
        };
        let runtime = match WorkspaceRuntime::from_snapshot(controller.snapshot()) {
            Ok(runtime) => runtime,
            Err(_) => {
                self.workspace_controller = None;
                self.workspace_runtime = WorkspaceRuntime::default();
                self.workspace_session_error =
                    Some("工作区快照无法建立运行态；原文件未修改。".into());
                self.refresh_workspace_views(cx);
                return;
            }
        };

        if let Err(error) = self.install_workspace_file_snapshot(controller.snapshot()) {
            self.workspace_session_state = previous_load;
            self.workspace_session_error = Some(error.public_message().into());
            self.refresh_workspace_views(cx);
            return;
        }

        let restored_file_center_panel = controller
            .snapshot()
            .active_tab_id
            .as_ref()
            .and_then(|active_id| {
                controller
                    .snapshot()
                    .terminal_tabs
                    .iter()
                    .find(|tab| tab.id() == active_id)
            })
            .and_then(|tab| {
                initial_workspace_file_center_panel(
                    self.workspace_load_pending,
                    self.pane_focus_intent_generation,
                    self.center_panel,
                    Some(tab),
                )
            });

        self.workspace_controller = Some(controller);
        self.workspace_runtime = runtime;
        if let Some(panel) = restored_file_center_panel {
            self.center_panel = panel;
            if panel == CenterPanel::Files
                && let Some(tab_id) = self
                    .workspace_controller
                    .as_ref()
                    .and_then(|controller| controller.snapshot().active_tab_id.clone())
            {
                self.files_workspace_selection =
                    super::workspace_files::FilesWorkspaceSelection::TransferTab(tab_id);
            }
        }
        self.refresh_workspace_views(cx);

        // 只有活动 tab 的 Restoring 画布首帧测量会进入恢复；隐藏 pane 保持无资源的 RestorePending。
    }

    /// 仅在首次工作区载入后且窗口仍由启动焦点拥有时提交既有 pane 焦点意图；不创建或重连 session。
    ///
    /// 延至下一帧检查窗口和 Root 状态，让载入期间发生的导航、模态、退出或失焦自然否决自动聚焦。
    ///
    /// @author kongweiguang
    pub(super) fn request_startup_workspace_focus(
        &mut self,
        initial_install: bool,
        cx: &mut Context<RootView>,
    ) {
        if !initial_install {
            return;
        }
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, _app| {
                window.on_next_frame(move |window, app| {
                    let _ = this.update(app, |root, cx| {
                        let focus = window.focused(cx);
                        let modal_open = root.preferences_modal_open
                            || root.connections_modal_open
                            || root.connection_prompt_modal_open
                            || root.snippet_editor_modal_open
                            || root.root_confirmation_owner.is_some()
                            || root.split_target_picker.read(cx).is_open()
                            || window.has_active_prompt()
                            || root.pending_quit_intent.is_some()
                            || root.quit_ready_intent.is_some()
                            || root.host_shutdown_requested
                            || root.host_lifecycle != super::RootHostLifecycle::Running;
                        let gate = StartupWorkspaceFocusGate {
                            initial_install,
                            window_active: window.is_window_active(),
                            terminal_center: root.center_panel == super::CenterPanel::Terminal,
                            startup_focus_owned: root.pane_focus_intent_generation == 0
                                && focus
                                    .as_ref()
                                    .is_none_or(|focused| focused == &root.focus_handle),
                            modal_open,
                            focus_intent_pending: root.pending_pane_focus_intent.is_some(),
                        };
                        let target = root.workspace_controller.as_ref().and_then(|controller| {
                            let snapshot = controller.snapshot();
                            let active_tab_id = snapshot.active_tab_id.as_ref()?;
                            let active_tab = snapshot
                                .terminal_tabs
                                .iter()
                                .find(|tab| tab.id() == active_tab_id)?;
                            let WorkspaceSessionTab::Terminal { id, layout, .. } = active_tab
                            else {
                                return None;
                            };
                            startup_workspace_terminal_focus_target(
                                gate,
                                Some((id, layout)),
                                snapshot.focused_pane_id.as_ref(),
                            )
                        });
                        if let Some((tab_id, pane_id)) = target {
                            root.request_workspace_pane_focus(tab_id, pane_id, window, cx);
                        }
                    });
                });
            });
        })
        .detach();
    }

    /// 仅在尚未提交宿主收尾时解析目标；PTY 尺寸在解析完成后读取最新测量值，避免捕获隐藏 pane 的默认网格。
    /// 首次预留的 Profile 解析会推进恢复代次，须精确转交既有聚焦意图；历史恢复不新增或重建焦点请求。
    /// 验收 probe 标记也只随完全匹配的 fresh reservation generation 前移，迟到的普通恢复不能继承它。
    ///
    /// @author kongweiguang
    pub(super) fn restore_workspace_pane(
        &mut self,
        tab_id: TabId,
        pane: WorkspaceSessionPane,
        cx: &mut Context<RootView>,
    ) {
        if self.host_shutdown_requested {
            return;
        }
        let pane_id = pane.id.clone();
        let is_registered = self
            .workspace_runtime
            .pane(&pane_id)
            .is_some_and(|runtime| runtime.tab_id == tab_id && runtime.persisted == pane);
        if !is_registered {
            return;
        }

        let reserved_generation = self
            .workspace_restore_generations
            .get(&pane_id)
            .copied()
            .filter(|generation| self.workspace_restore_is_current(&pane_id, *generation))
            .filter(|_| {
                self.workspace_controller
                    .as_ref()
                    .is_some_and(|controller| {
                        !controller
                            .snapshot()
                            .terminal_tabs
                            .iter()
                            .any(|tab| tab.id() == &tab_id)
                    })
            });
        #[cfg(feature = "acceptance-metrics")]
        let fresh_probe_reserved_generation = self
            .pending_fresh_terminal_probes
            .get(&pane_id)
            .copied()
            .filter(|pending_generation| {
                self.workspace_restore_generations.get(&pane_id) == Some(pending_generation)
            });
        let (generation, cancellation) = self.begin_workspace_restore(&pane_id);
        #[cfg(feature = "acceptance-metrics")]
        if let Some(reserved_generation) = fresh_probe_reserved_generation {
            // Profile resolution creates a new restore generation synchronously; transfer only the exact reservation marker.
            if self.pending_fresh_terminal_probes.get(&pane_id) == Some(&reserved_generation) {
                self.pending_fresh_terminal_probes
                    .insert(pane_id.clone(), generation);
            }
        }
        if let Some(reserved_generation) = reserved_generation {
            super::pane_input_runtime::advance_new_workspace_pane_focus_restore_generation(
                self.pending_pane_focus_intent.as_mut(),
                &tab_id,
                &pane_id,
                reserved_generation,
                generation,
            );
        }
        let services = self.services.clone();
        let executor = services.executor.clone();
        let resolve_cancellation = cancellation.clone();

        cx.spawn(async move |this, cx| {
            let profiles = services.profiles.clone();
            let hosts = services.hosts.clone();
            let pane_for_task = pane.clone();
            let resolved = executor
                .run(move |application_cancellation| async move {
                    let resolve = resolve_cancellation.run_until_cancelled(
                        resolve_workspace_open_target(&profiles, hosts.as_ref(), &pane_for_task),
                    );
                    Ok::<_, AppError>(
                        match application_cancellation.run_until_cancelled(resolve).await {
                            Some(Some(result)) => result,
                            Some(None) | None => Err((
                                WorkspaceRestoreFailure::Cancelled,
                                AppError::cancelled("工作区目标解析已取消"),
                            )),
                        },
                    )
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.workspace_restore_is_current(&pane_id, generation) {
                    root.complete_pending_workspace_close_after_restore_ack(&pane_id, cx);
                    return;
                }
                // 解析尚未创建 PTY；停止后只把当前回执记为取消，不再进入资源创建管线。
                let resolved = if root.host_shutdown_requested && matches!(&resolved, Ok(Ok(_))) {
                    Ok(Err((
                        WorkspaceRestoreFailure::Cancelled,
                        AppError::cancelled("应用已提交收尾，工作区目标不再启动"),
                    )))
                } else {
                    resolved
                };
                match resolved {
                    Ok(Ok((kind, runtime_target))) => {
                        if root
                            .workspace_runtime
                            .set_restore_target(&pane_id, runtime_target.clone())
                            .is_err()
                        {
                            root.mark_workspace_pane_restore_failed(
                                &pane_id,
                                generation,
                                WorkspaceRestoreFailure::ResourceFailure,
                                Some(runtime_target),
                                "pane 恢复状态已变化".into(),
                                cx,
                            );
                            return;
                        }
                        let Some(initial_size) = root
                            .workspace_runtime
                            .pane(&pane_id)
                            .map(|runtime| runtime.size)
                        else {
                            return;
                        };
                        if let Err(error) = root.open_workspace_terminal_pane(
                            super::terminal_open::WorkspaceTerminalOpenRequest {
                                tab_id,
                                pane_id: pane_id.clone(),
                                kind,
                                size: initial_size,
                                lease: None,
                                transient: false,
                                generation,
                                cancellation: cancellation.clone(),
                            },
                            cx,
                        ) {
                            let restore_target = root
                                .workspace_runtime
                                .pane(&pane_id)
                                .and_then(|runtime| runtime.target.clone());
                            root.mark_workspace_pane_restore_failed(
                                &pane_id,
                                generation,
                                WorkspaceRestoreFailure::ResourceFailure,
                                restore_target,
                                error,
                                cx,
                            );
                        }
                    }
                    Ok(Err((failure, error))) => {
                        root.mark_workspace_pane_restore_failed(
                            &pane_id,
                            generation,
                            failure,
                            None,
                            error.public_message().to_owned().into(),
                            cx,
                        );
                    }
                    Err(error) => {
                        root.mark_workspace_pane_restore_failed(
                            &pane_id,
                            generation,
                            failure_for_error(&error),
                            None,
                            error.public_message().to_owned().into(),
                            cx,
                        );
                    }
                }
            });
        })
        .detach();
    }

    /// 复用中央工作区投影判断 tab 可见性；split layout 的叶 pane 共享 tab 状态，不自行猜 active pane。
    /// 该判定也供终端输入绑定启动使用，后台恢复不应仅因 session Ready 就创建隐藏 writer。
    ///
    /// @author kongweiguang
    pub(super) fn workspace_tab_is_visible(&self, tab_id: &TabId) -> bool {
        self.center_panel == CenterPanel::Terminal
            && self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
                == Some(tab_id)
    }

    /// 中央区返回终端时为当前递归布局中的 panes 补读；最多只刷新实际可见的一组终端。
    ///
    /// @author kongweiguang
    pub(super) fn request_visible_workspace_snapshots(&mut self, cx: &mut Context<RootView>) {
        if self.center_panel != CenterPanel::Terminal {
            return;
        }
        let Some((tab_id, pane_ids)) = self.workspace_controller.as_ref().and_then(|controller| {
            let snapshot = controller.snapshot();
            let tab_id = snapshot.active_tab_id.clone()?;
            let tab = snapshot
                .terminal_tabs
                .iter()
                .find(|tab| tab.id() == &tab_id)?;
            Some((tab_id, tab.terminal_layout()?.pane_ids()))
        }) else {
            return;
        };

        for pane_id in pane_ids {
            if !self.workspace_runtime.snapshot_needs_refresh(&pane_id) {
                continue;
            }
            self.request_workspace_pane_snapshot(tab_id.clone(), pane_id, cx);
        }
    }

    /// 对指定 pane 读取引擎当前视口；绑定尚未安装时保留 dirty，由安装回调补读且不回写 offset。
    /// 尺寸失效或主/备用屏等拓扑变化后的有效快照才重查搜索；普通输出不重查，避免搜索与快照互相触发。
    ///
    /// @author kongweiguang
    pub(super) fn request_workspace_pane_snapshot(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        cx: &mut Context<RootView>,
    ) {
        let binding = self
            .workspace_runtime
            .pane(&pane_id)
            .and_then(|pane| pane.session_id.as_ref())
            .and_then(|session_id| self.terminal_view_bindings.get(session_id))
            .cloned();
        if !self
            .workspace_runtime
            .prepare_bound_snapshot(&tab_id, &pane_id, binding.as_ref())
        {
            return;
        }
        let Some(binding) = binding else {
            return;
        };
        if !self.workspace_tab_is_visible(&tab_id) {
            let _ = self.workspace_runtime.defer_snapshot(&pane_id);
            return;
        }
        let (session_id, token, request) = match self.workspace_runtime.begin_snapshot(&pane_id) {
            Ok(Some(request)) => request,
            Ok(None) => return,
            Err(_) => return,
        };
        #[cfg(feature = "acceptance-metrics")]
        crate::acceptance_metrics::terminal_echo_probe_registry().record_phase_for_session(
            binding.session_id.as_str(),
            binding.session_generation,
            crate::acceptance_metrics::TerminalEchoPhase::SnapshotRequested,
            None,
        );
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let callback_binding = binding.clone();
        cx.spawn(async move |this, cx| {
            let result = executor
                .run(move |_| async move { sessions.render_snapshot_ui(binding, request).await })
                .await;
            #[cfg(feature = "acceptance-metrics")]
            if let Ok(snapshot) = &result {
                crate::acceptance_metrics::terminal_echo_probe_registry().record_phase_for_session(
                    callback_binding.session_id.as_str(),
                    callback_binding.session_generation,
                    crate::acceptance_metrics::TerminalEchoPhase::SnapshotReturned,
                    Some(snapshot.revision),
                );
            }
            let _ = this.update(cx, |root, cx| match result {
                Ok(snapshot) => {
                    if !root.workspace_tab_is_visible(&tab_id) {
                        let _ = root.workspace_runtime.finish_snapshot_request(
                            &pane_id,
                            &session_id,
                            token,
                            false,
                            false,
                        );
                        return;
                    }
                    #[cfg(feature = "acceptance-metrics")]
                    let snapshot_revision = snapshot.revision;
                    let changes_grid_topology =
                        root.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
                            pane.snapshot.as_ref().is_none_or(|current| {
                                current.size != snapshot.size
                                    || current.grid_epoch != snapshot.grid_epoch
                            })
                        });
                    let accepted = root
                        .workspace_runtime
                        .accept_snapshot(&pane_id, &session_id, token, snapshot)
                        .unwrap_or(false);
                    #[cfg(feature = "acceptance-metrics")]
                    crate::acceptance_metrics::terminal_echo_probe_registry()
                        .record_phase_for_session(
                            callback_binding.session_id.as_str(),
                            callback_binding.session_generation,
                            if accepted {
                                crate::acceptance_metrics::TerminalEchoPhase::SnapshotAccepted
                            } else {
                                crate::acceptance_metrics::TerminalEchoPhase::SnapshotRejected
                            },
                            Some(snapshot_revision),
                        );
                    let follow_up = root
                        .workspace_runtime
                        .finish_snapshot_request(&pane_id, &session_id, token, accepted, true)
                        .unwrap_or(false);
                    if accepted {
                        root.refresh_workspace_views(cx);
                        if changes_grid_topology {
                            root.refresh_open_terminal_search_for_view_revision(&session_id, cx);
                        }
                    }
                    if follow_up {
                        root.request_workspace_pane_snapshot(tab_id.clone(), pane_id.clone(), cx);
                    }
                }
                Err(error) => {
                    if !root
                        .workspace_runtime
                        .mark_snapshot_failed(&pane_id, &session_id, token)
                        .unwrap_or(false)
                    {
                        return;
                    }
                    let binding_replaced = root
                        .terminal_view_bindings
                        .get(&session_id)
                        .is_some_and(|current| {
                            current.session_id == callback_binding.session_id
                                && current.owner == callback_binding.owner
                                && current.session_generation != callback_binding.session_generation
                        });
                    let follow_up = root
                        .workspace_runtime
                        .finish_snapshot_request(
                            &pane_id,
                            &session_id,
                            token,
                            false,
                            binding_replaced,
                        )
                        .unwrap_or(false);
                    if follow_up && root.workspace_tab_is_visible(&tab_id) {
                        root.request_workspace_pane_snapshot(tab_id.clone(), pane_id.clone(), cx);
                    } else if root.workspace_tab_is_visible(&tab_id) {
                        root.status = Some(error.public_message().into());
                        root.refresh_workspace_views(cx);
                    }
                }
            });
        })
        .detach();
    }

    /// 资源首帧就绪后同步完整工作区；本机临时容器用精确运行态身份通过 UI snapshot 校验，随后仍由 CAS 投影剔除。
    /// 激活决策由调用方按创建时基线提供。
    /// 分屏的目标和标题属于 pane；共享 tab 元数据不参与子 pane 身份判断。
    /// 新建 tab 的硬同步失败会转入现有 orphan close 重试 owner；延迟资源操作仍由原 pane 持有。
    ///
    /// @author kongweiguang
    pub(super) fn persist_ready_workspace_pane(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        activate_new_tab: bool,
        cleanup_new_tab_on_sync_failure: bool,
        cx: &mut Context<RootView>,
    ) {
        if self.workspace_controller.is_none() {
            self.status = Some(crate::locale::text(
                "终端已就绪，但工作区处于只读恢复状态；资源仍由运行态保留。",
                "The terminal is ready, but the workspace is in read-only recovery; the runtime still owns the resource.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        }

        let Some(runtime_pane) = self.workspace_runtime.pane(pane_id) else {
            self.status = Some(crate::locale::text(
                "终端已启动，但没有对应的工作区 pane 运行态。",
                "The terminal started without a matching workspace pane runtime.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        };
        let Some(layout) = self.workspace_runtime.tab_layout(tab_id).cloned() else {
            self.status = Some(crate::locale::text(
                "终端已启动，但工作区完整布局缺失。",
                "The terminal started, but its complete workspace layout is missing.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        };
        let pane = runtime_pane.persisted.clone();
        let Some(session_id) = runtime_pane.session_id.clone() else {
            self.status = Some(crate::locale::text(
                "终端尚未绑定到就绪会话。",
                "The terminal is not bound to a ready session yet.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        };
        let runtime_is_ready = &runtime_pane.tab_id == tab_id
            && &pane.id == pane_id
            && runtime_pane.phase == RuntimePanePhase::Ready
            && runtime_pane.session_id.as_ref() == Some(&session_id)
            && runtime_pane.target.is_some()
            && runtime_pane
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.session_id == session_id);
        if !runtime_is_ready
            || !layout.contains_pane(pane_id)
            || self.services.workspace.validate_layout(&layout).is_err()
        {
            self.status = Some(crate::locale::text(
                "终端 pane 尚未完成资源接入或布局校验。",
                "The terminal pane has not completed resource installation or layout validation.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        }

        // 本机容器没有可持久 WorkspaceTargetRef；只在 runtime 的 transient 与完整 Container target
        // 同时吻合 persisted container id 时借用现有 pane owner 流程，不合成可恢复的本机 shell 目标。
        let transient_local_container_id = if runtime_pane.transient
            && pane.mode == WorkspacePaneMode::Container
            && pane.target.is_none()
            && pane.remote_host_id.is_none()
        {
            runtime_pane
                .target
                .as_ref()
                .and_then(|target| match target {
                    SessionTarget::Container(container)
                        if container.host_id.is_none()
                            && pane.container_id.as_deref()
                                == Some(container.container_id.as_str()) =>
                    {
                        Some(container.container_id.clone())
                    }
                    _ => None,
                })
        } else {
            None
        };

        let workspace_tab = WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: pane.title.clone(),
            machine_id: pane.machine_id.clone(),
            layout: layout.clone(),
        };
        let sync_result = {
            let Some(controller) = self.workspace_controller.as_mut() else {
                self.status = Some(crate::locale::text(
                    "工作区状态已变化；终端资源仍由运行态保留。",
                    "The workspace state changed; the runtime still owns the terminal resource.",
                ));
                self.refresh_workspace_views(cx);
                cx.notify();
                return;
            };
            let mut snapshot = controller.snapshot().clone();
            let existing_tab = snapshot
                .terminal_tabs
                .iter()
                .find(|existing| existing.id() == tab_id)
                .cloned();
            let existing_pane = snapshot
                .terminal_panes
                .iter()
                .find(|existing| &existing.id == pane_id)
                .cloned();
            match (existing_tab, existing_pane) {
                (Some(existing_tab), Some(existing_pane))
                    if ready_workspace_pane_matches_snapshot(
                        tab_id,
                        pane_id,
                        &pane,
                        &existing_tab,
                        &existing_pane,
                        &layout,
                    ) =>
                {
                    Ok(WorkspaceControllerEffect::Unchanged)
                }
                (Some(_), _) | (_, Some(_)) => Err(AppError::conflict(
                    "工作区中已存在不同内容的标签或分屏 ID。",
                )),
                (None, None) => {
                    let active_tab_before_insert = snapshot.active_tab_id.clone();
                    snapshot.terminal_tabs.push(workspace_tab);
                    snapshot.terminal_panes.push(pane.clone());
                    let selected_tab_id = activate_new_tab
                        .then(|| tab_id.clone())
                        .or(active_tab_before_insert)
                        .filter(|active_id| {
                            snapshot
                                .terminal_tabs
                                .iter()
                                .any(|candidate| candidate.id() == active_id)
                        })
                        .or_else(|| Some(tab_id.clone()));
                    snapshot.active_tab_id = selected_tab_id.clone();
                    if selected_tab_id.as_ref() == Some(tab_id) {
                        snapshot.focused_pane_id = Some(pane.id.clone());
                        snapshot.selected_machine_id = Some(pane.machine_id.clone());
                    } else {
                        let active_layout = selected_tab_id.as_ref().and_then(|active_id| {
                            snapshot
                                .terminal_tabs
                                .iter()
                                .find(|candidate| candidate.id() == active_id)
                                .and_then(WorkspaceSessionTab::terminal_layout)
                        });
                        if !snapshot.focused_pane_id.as_ref().is_some_and(|focused| {
                            active_layout.is_some_and(|layout| layout.contains_pane(focused))
                        }) {
                            snapshot.focused_pane_id = None;
                        }
                    }
                    if let Some(container_id) = transient_local_container_id.as_deref() {
                        controller.synchronize_snapshot_for_transient_local_container(
                            snapshot,
                            tab_id,
                            pane_id,
                            container_id,
                        )
                    } else {
                        controller.synchronize_snapshot(snapshot)
                    }
                }
            }
        };
        let should_persist = matches!(&sync_result, Ok(WorkspaceControllerEffect::Changed { .. }));
        match sync_result {
            Ok(WorkspaceControllerEffect::Changed { .. }) => {
                if activate_new_tab {
                    self.center_panel = super::CenterPanel::Terminal;
                }
                self.refresh_workspace_views(cx);
            }
            Ok(WorkspaceControllerEffect::Unchanged) => {
                self.refresh_workspace_views(cx);
            }
            Ok(WorkspaceControllerEffect::ResourceOperation(_)) => {
                self.status = Some(crate::locale::text(
                    "工作区正在处理其它资源变更；终端资源已保留，待变更完成后再同步。",
                    "The workspace is processing another resource change. The terminal remains owned by the runtime until synchronization can continue.",
                ));
                self.refresh_workspace_views(cx);
            }
            Err(error) => {
                if cleanup_new_tab_on_sync_failure {
                    let failure_status = crate::locale::text(
                        format!(
                            "终端未能写入工作区：{}。正在回收本次会话。",
                            error.public_message()
                        ),
                        format!(
                            "The terminal could not be written to the workspace: {}. Its session is being closed.",
                            error.public_message()
                        ),
                    );
                    match self.workspace_runtime.move_ready_pane_to_orphan_cleanup(
                        pane_id,
                        crate::locale::text(
                            "工作区写入失败，等待关闭确认。",
                            "Workspace persistence failed; waiting for close confirmation.",
                        ),
                    ) {
                        Ok(session_id) => {
                            self.handle_workspace_orphan_cleanup_event(
                                super::workspace_orphan_cleanup::WorkspaceOrphanCleanupEvent::Retry(
                                    session_id,
                                ),
                                cx,
                            );
                            self.status = Some(failure_status);
                        }
                        Err(_) => {
                            self.status = Some(crate::locale::text(
                                format!(
                                    "{} 会话资源仍由运行态保留，未能安全启动自动回收。",
                                    error.public_message()
                                ),
                                format!(
                                    "{} The session remains runtime-owned; automatic cleanup could not safely start.",
                                    error.public_message()
                                ),
                            ));
                        }
                    }
                } else {
                    self.status = Some(error.public_message().into());
                }
                self.refresh_workspace_views(cx);
            }
        }
        if should_persist && self.services.workspace_sessions.is_some() {
            self.persist_workspace_snapshot(cx);
        }
        self.refresh_proxy_panel(cx);
        cx.notify();
    }
}

/// 判断已恢复 tab 中的 pane 是否仍是当前运行态所绑定的精确快照成员。
///
/// 同一终端 tab 可以包含不同主机的分屏，因此 tab 级标题和 machine_id 不能用于
/// 判断子 pane；这里保留 pane 持久行精确比较，并核对 tab 身份、完整布局与成员关系。
///
/// @author kongweiguang
fn ready_workspace_pane_matches_snapshot(
    tab_id: &TabId,
    pane_id: &PaneId,
    pane: &WorkspaceSessionPane,
    existing_tab: &WorkspaceSessionTab,
    existing_pane: &WorkspaceSessionPane,
    layout: &TerminalLayoutNode,
) -> bool {
    pane.id == *pane_id
        && existing_pane == pane
        && matches!(
            existing_tab,
            WorkspaceSessionTab::Terminal {
                id: existing_tab_id,
                layout: existing_layout,
                ..
            } if existing_tab_id == tab_id
                && existing_layout == layout
                && existing_layout.contains_pane(pane_id)
        )
}

/// 记录目标解析或 Root open 同步拒绝；运行态仍保留原 pane 以便用户重试。
impl RootView {
    /// 当前失败仍更新 pane/controller 账目；停止后不刷新会触发普通任务的子视图，也不覆盖宿主原错。
    ///
    /// @author kongweiguang
    fn mark_workspace_pane_restore_failed(
        &mut self,
        pane_id: &PaneId,
        generation: u64,
        failure: WorkspaceRestoreFailure,
        target: Option<SessionTarget>,
        message: SharedString,
        cx: &mut Context<RootView>,
    ) {
        if !self.workspace_restore_is_current(pane_id, generation) {
            return;
        }
        let visible_message = record_workspace_restore_failure(
            &mut self.workspace_runtime,
            self.workspace_controller.as_mut(),
            pane_id,
            failure,
            target,
            message,
        );
        if self.host_shutdown_requested {
            cx.notify();
        } else {
            self.status = Some(visible_message);
            self.finish_workspace_restore(pane_id, generation, cx);
        }
    }
}

/// 把恢复回执记入真实运行态与 controller，和是否显示普通错误解耦，停止期间也不能跳过资源账目。
///
/// @author kongweiguang
fn record_workspace_restore_failure(
    runtime: &mut WorkspaceRuntime,
    controller: Option<&mut WorkspaceController>,
    pane_id: &PaneId,
    failure: WorkspaceRestoreFailure,
    target: Option<SessionTarget>,
    message: SharedString,
) -> SharedString {
    let mut visible_message = message;
    if let Some(target) = target
        && runtime.set_restore_target(pane_id, target).is_err()
    {
        visible_message = SharedString::from(format!(
            "{} {}",
            visible_message,
            crate::locale::text(
                "原连接目标无法保留供重试。",
                "The original target could not be retained for retry.",
            )
        ));
    }
    if runtime.mark_restore_failed(pane_id).is_err() {
        visible_message = SharedString::from(format!(
            "{} {}",
            visible_message,
            crate::locale::text(
                "pane 状态已变化，恢复失败未能写入运行态。",
                "The pane changed state before the restore failure could be recorded.",
            )
        ));
    }
    if let Some(controller) = controller
        && let Err(error) = controller
            .record_restore_result(WorkspaceRestoreItemId::Pane(pane_id.clone()), Err(failure))
    {
        visible_message =
            SharedString::from(format!("{} {}", visible_message, error.public_message()));
    }
    visible_message
}

/// 将应用错误压缩为工作区恢复状态机的稳定失败类别。
fn failure_for_error(error: &AppError) -> WorkspaceRestoreFailure {
    if error.public_code() == kerminal_application::PublicErrorCode::Cancelled {
        WorkspaceRestoreFailure::Cancelled
    } else {
        WorkspaceRestoreFailure::ResourceFailure
    }
}

/// 将冻结的 pane target 映射到唯一的终端打开策略，并保留运行态实际连接目标。
async fn resolve_workspace_open_target(
    profiles: &ProfileService,
    hosts: Option<&HostManagementService>,
    pane: &WorkspaceSessionPane,
) -> Result<(WorkspaceTerminalOpenKind, SessionTarget), (WorkspaceRestoreFailure, AppError)> {
    if pane.mode == WorkspacePaneMode::Preview {
        return Err((
            WorkspaceRestoreFailure::UnavailableTarget(
                WorkspaceTargetUnavailableReason::UnknownKind,
            ),
            AppError::unsupported("预览 pane 不能恢复为交互式终端"),
        ));
    }

    if let Some(WorkspaceTargetRef::DockerContainer {
        host_id,
        container_id,
        runtime,
        workdir,
        ..
    }) = pane.target.as_ref()
    {
        if pane.tmux_binding.is_some() {
            return Err((
                WorkspaceRestoreFailure::UnavailableTarget(
                    WorkspaceTargetUnavailableReason::UnknownKind,
                ),
                AppError::unsupported("容器终端不能恢复为 tmux attach"),
            ));
        }
        let runtime = match runtime {
            WorkspaceContainerRuntime::Docker => ContainerRuntime::Docker,
            WorkspaceContainerRuntime::Podman => ContainerRuntime::Podman,
        };
        let command = container_exec_command(pane, workdir.as_deref())
            .map_err(|error| (failure_for_error(&error), error))?;
        let target = ContainerTarget {
            host_id: Some(host_id.clone()),
            runtime,
            container_id: container_id.clone(),
        };
        let runtime_target = SessionTarget::Container(ContainerSessionTarget {
            host_id: Some(host_id.clone()),
            runtime,
            container_id: container_id.clone(),
            command: command.iter().map(OsString::from).collect(),
        });
        return Ok((
            WorkspaceTerminalOpenKind::ContainerExec { target, command },
            runtime_target,
        ));
    }

    let target = resolve_workspace_target(profiles, hosts, pane).await?;
    if let Some(binding) = pane.tmux_binding.as_ref() {
        let (host_id, profile_id) = match pane.target.as_ref() {
            Some(WorkspaceTargetRef::Local { profile_id }) => (
                None,
                profile_id.as_ref().or(pane.profile_id.as_ref()).cloned(),
            ),
            Some(WorkspaceTargetRef::Ssh { host_id }) => (Some(host_id.clone()), None),
            _ => {
                return Err((
                    WorkspaceRestoreFailure::UnavailableTarget(
                        WorkspaceTargetUnavailableReason::UnknownKind,
                    ),
                    AppError::unsupported("tmux 只能附加到已保存的本地或 SSH 目标"),
                ));
            }
        };
        let tmux_target = TmuxTarget {
            host_id,
            profile_id,
            socket_name: binding.socket_name.clone(),
            socket_path: binding.socket_path.clone(),
            tmux_path: None,
        };
        if !tmux_binding_matches_target(pane, binding, &tmux_target) {
            return Err((
                WorkspaceRestoreFailure::UnavailableTarget(
                    WorkspaceTargetUnavailableReason::Malformed,
                ),
                AppError::invalid("tmux 工作区绑定与终端目标不一致"),
            ));
        }
        return Ok((
            WorkspaceTerminalOpenKind::TmuxAttach {
                target: tmux_target,
                session_id: binding.session_id.clone(),
            },
            target,
        ));
    }

    Ok((WorkspaceTerminalOpenKind::Direct(target.clone()), target))
}

/// 按冻结 v3 中的 profile/host/socket 字段匹配目标，避免把含 `|` 的合法标识当成分隔符。
///
/// `target_ref` 是需要原样保留的旧外部标识；目标身份由 pane 与 binding 的结构化字段表达。
fn tmux_binding_matches_target(
    pane: &WorkspaceSessionPane,
    binding: &WorkspaceTmuxBinding,
    target: &TmuxTarget,
) -> bool {
    if target.validate().is_err() {
        return false;
    }
    let base_matches = match pane.target.as_ref() {
        Some(WorkspaceTargetRef::Local { profile_id }) => {
            target.host_id.is_none()
                && target.profile_id.as_deref()
                    == profile_id.as_deref().or(pane.profile_id.as_deref())
        }
        Some(WorkspaceTargetRef::Ssh { host_id }) => {
            target.host_id.as_ref() == Some(host_id) && target.profile_id.is_none()
        }
        _ => false,
    };
    base_matches
        && binding.socket_name.as_deref() == target.socket_name.as_deref()
        && binding.socket_path.as_deref() == target.socket_path.as_deref()
}

/// 只从 workspace 的 argv 组装 container exec；workdir 通过 POSIX 单引号转义应用。
fn container_exec_command(
    pane: &WorkspaceSessionPane,
    workdir: Option<&str>,
) -> AppResult<Vec<String>> {
    let mut command = pane
        .shell
        .iter()
        .chain(pane.args.iter().flatten())
        .cloned()
        .collect::<Vec<_>>();
    if let Some(workdir) = workdir {
        let command_line = if command.is_empty() {
            "exec sh".to_owned()
        } else {
            let quoted = command
                .iter()
                .map(|part| shell_quote(part))
                .collect::<Vec<_>>()
                .join(" ");
            format!("exec {quoted}")
        };
        command = vec![
            "sh".to_owned(),
            "-lc".to_owned(),
            format!("cd -- {} && {command_line}", shell_quote(workdir)),
        ];
    }
    if command.is_empty() {
        command.push("sh".to_owned());
    }
    if command
        .iter()
        .any(|argument| argument.chars().any(char::is_control))
    {
        return Err(AppError::invalid("容器启动参数包含控制字符"));
    }
    Ok(command)
}

/// 按 v3 类型化 target 恢复连接，明确缺失 profile/host 时失败而不切换其它默认目标。
async fn resolve_workspace_target(
    profiles: &ProfileService,
    hosts: Option<&HostManagementService>,
    pane: &WorkspaceSessionPane,
) -> Result<SessionTarget, (WorkspaceRestoreFailure, AppError)> {
    let Some(target_ref) = pane.target.as_ref() else {
        return Err((
            WorkspaceRestoreFailure::MissingTarget,
            AppError::not_found("工作区终端目标"),
        ));
    };
    match target_ref {
        WorkspaceTargetRef::Unavailable { reason, .. } => Err((
            WorkspaceRestoreFailure::UnavailableTarget(*reason),
            AppError::unsupported("工作区保存的终端目标当前不可恢复"),
        )),
        WorkspaceTargetRef::Local { profile_id } => {
            let profile_id = profile_id.as_deref().or(pane.profile_id.as_deref());
            let mut target = if let Some(profile_id) = profile_id {
                profiles
                    .local_session_target(profile_id)
                    .await
                    .map_err(|error| (failure_for_error(&error), error))?
                    .ok_or_else(|| {
                        (
                            WorkspaceRestoreFailure::MissingTarget,
                            AppError::not_found("工作区引用的本地 Profile"),
                        )
                    })?
            } else if let Some(shell) = pane.shell.as_deref() {
                local_target_from_pane(pane, shell)
                    .map_err(|error| (failure_for_error(&error), error))?
            } else {
                profiles
                    .default_local_session_target()
                    .await
                    .map_err(|error| (failure_for_error(&error), error))?
            };
            if let Some(working_directory) = pane.current_cwd.as_ref().or(pane.cwd.as_ref()) {
                target.working_directory = Some(PathBuf::from(working_directory));
            }
            Ok(SessionTarget::Local(target))
        }
        WorkspaceTargetRef::Ssh { host_id } => Ok(SessionTarget::Ssh(SshSessionTarget {
            host_id: host_id.clone(),
            working_directory: pane
                .current_cwd
                .as_ref()
                .or(pane.cwd.as_ref())
                .map(PathBuf::from),
            command: pane.remote_command.clone().map(SshInitialCommand::Shell),
        })),
        WorkspaceTargetRef::Telnet { host_id } => {
            let host = load_workspace_host(hosts, host_id).await?;
            if host.protocol != RemoteProtocol::Telnet {
                return Err((
                    WorkspaceRestoreFailure::UnavailableTarget(
                        WorkspaceTargetUnavailableReason::UnknownKind,
                    ),
                    AppError::invalid("工作区 Telnet 目标与主机协议不匹配"),
                ));
            }
            Ok(SessionTarget::Telnet(TelnetSessionTarget {
                host: host.endpoint.host,
                port: host.endpoint.port,
            }))
        }
        WorkspaceTargetRef::Serial { host_id } => {
            let host = load_workspace_host(hosts, host_id).await?;
            if host.protocol != RemoteProtocol::Serial {
                return Err((
                    WorkspaceRestoreFailure::UnavailableTarget(
                        WorkspaceTargetUnavailableReason::UnknownKind,
                    ),
                    AppError::invalid("工作区 Serial 目标与主机协议不匹配"),
                ));
            }
            let serial = host.serial.ok_or_else(|| {
                (
                    WorkspaceRestoreFailure::MissingTarget,
                    AppError::not_found("工作区引用的串口配置"),
                )
            })?;
            Ok(SessionTarget::Serial(serial))
        }
        WorkspaceTargetRef::DockerContainer { .. } => Err((
            WorkspaceRestoreFailure::UnavailableTarget(
                WorkspaceTargetUnavailableReason::UnknownKind,
            ),
            AppError::unsupported("容器终端必须通过容器 exec 用例恢复"),
        )),
    }
}

/// 按 HostId 精确读取连接配置；缺少连接服务或条目时不扫描/选择其它主机。
async fn load_workspace_host(
    hosts: Option<&HostManagementService>,
    host_id: &kerminal_domain::identity::HostId,
) -> Result<
    kerminal_application::connections::management::ManagedHost,
    (WorkspaceRestoreFailure, AppError),
> {
    let Some(hosts) = hosts else {
        return Err((
            WorkspaceRestoreFailure::MissingTarget,
            AppError::unsupported("主机目录服务尚未配置"),
        ));
    };
    hosts
        .load_host(host_id)
        .await
        .map_err(|error| (failure_for_error(&error), error))?
        .ok_or_else(|| {
            (
                WorkspaceRestoreFailure::MissingTarget,
                AppError::not_found("工作区引用的主机"),
            )
        })
}

/// 只有 local target 没有 profile 引用时，才用 v3 pane 内明示的完整启动参数重建。
fn local_target_from_pane(
    pane: &WorkspaceSessionPane,
    shell: &str,
) -> AppResult<LocalSessionTarget> {
    let program = OsString::from(shell);
    let args = pane
        .args
        .as_ref()
        .into_iter()
        .flatten()
        .map(OsString::from)
        .collect();
    let environment = pane
        .env
        .as_ref()
        .map(|values| {
            values
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let working_directory = pane
        .current_cwd
        .as_ref()
        .or(pane.cwd.as_ref())
        .map(PathBuf::from);
    Ok(LocalSessionTarget {
        program,
        args,
        working_directory,
        environment,
    })
}

/// 将输入作为 POSIX shell 单引号参数，避免 container workdir 变成可执行 shell 片段。
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// 启动恢复聚焦只在首次加载、前台终端仍由 Root 启动画面拥有时准入；不代表恢复 pane 本身自动抢焦点。
///
/// 采样时已有用户导航、模态或更具体的 pane 意图就不新建启动请求；后续 ACK 仍由既有焦点意图校验。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StartupWorkspaceFocusGate {
    pub(super) initial_install: bool,
    pub(super) window_active: bool,
    pub(super) terminal_center: bool,
    pub(super) startup_focus_owned: bool,
    pub(super) modal_open: bool,
    pub(super) focus_intent_pending: bool,
}

/// 把启动准入与确切活动终端目标合并为纯决策，后续仍由既有 binding/焦点意图 ACK 完成交接。
///
/// `active_terminal` 只由加载后的活动 tab 经 Terminal variant 校验取得；缺失 pane 焦点按显式激活语义回退到该布局首 pane。
///
/// @author kongweiguang
pub(super) fn startup_workspace_terminal_focus_target(
    gate: StartupWorkspaceFocusGate,
    active_terminal: Option<(&TabId, &TerminalLayoutNode)>,
    focused_pane: Option<&PaneId>,
) -> Option<(TabId, PaneId)> {
    if !gate.initial_install
        || !gate.window_active
        || !gate.terminal_center
        || !gate.startup_focus_owned
        || gate.modal_open
        || gate.focus_intent_pending
    {
        return None;
    }

    let (tab_id, layout) = active_terminal?;
    let pane_id = focused_pane
        .cloned()
        .filter(|pane_id| layout.contains_pane(pane_id))
        .or_else(|| layout.pane_ids().into_iter().next())?;
    Some((tab_id.clone(), pane_id))
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_sessions.rs"]
mod tests;
