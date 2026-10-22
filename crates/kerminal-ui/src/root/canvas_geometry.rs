//! Root 以完整 binding 将终端 Canvas 几何变化接入运行态与历史视图 resize 队列。
//!
//! @author kongweiguang

use gpui_kit::{AnyElement, Context, div, prelude::*};
use kerminal_domain::{
    identity::{PaneId, TabId},
    sessions::{SessionLifecycle, SessionOwner, TerminalSize},
};

use crate::features::workspace_panes::{SplitCandidateMeasurement, SplitCandidateProbeContext};
use crate::terminal_canvas::{
    CanvasResizeRequest, binding_matches_current, clear_split_probe_failure,
    should_log_split_root_rejection, should_request_split_probe_retry,
    workspace_restore_size_probe, workspace_split_candidate_size_probe,
};

use super::session_command_pump::observation_proves_session_not_running;
use super::session_reconnect::SessionPanelObservation;
use super::{CenterPanel, RootView, RuntimePanePhase, logical_input_modal_blocked};
use crate::features::workspace_controller::{
    WorkspaceResourceOperation, WorkspaceRestoreItemId, WorkspaceRestoreStatus,
};

/// Root 对暂态候选门禁拒绝有限重绘并记录具体阶段，避免有效几何回执丢失后永久等待。
///
/// @author kongweiguang
fn retry_split_candidate_after_root_rejection(
    preview: &crate::features::workspace_panes::SplitCandidatePreview,
    layout_generation: u64,
    stage: &'static str,
    cx: &mut Context<RootView>,
) {
    if should_log_split_root_rejection(&preview.operation_id, layout_generation, stage) {
        tracing::warn!(
            target: "kerminal::split_measurement",
            operation_id = %preview.operation_id,
            layout_generation,
            stage,
            "split candidate passed canvas geometry but was rejected by Root admission"
        );
    }
    if should_request_split_probe_retry(&preview.operation_id, layout_generation) {
        cx.notify();
    }
}

impl RootView {
    /// 只为仍匹配当前 CreatePane owner 的 view-only pane 构造无输入测量 Canvas。
    ///
    /// @author kongweiguang
    pub(crate) fn workspace_split_candidate_probe_element(
        &mut self,
        probe: SplitCandidateProbeContext,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let preview_is_current = self
            .pending_workspace_split_preview(cx)
            .is_some_and(|(preview, _, _)| preview == probe.preview);
        let owner_is_unstarted = self
            .workspace_resource_operations
            .get(&probe.preview.pane_id)
            .is_some_and(|entry| {
                entry.operation_id() == probe.preview.operation_id
                    && !entry.split_measurement_started()
            });
        let pane_is_pending = self
            .workspace_runtime
            .pane(&probe.preview.pane_id)
            .is_some_and(|pane| {
                pane.tab_id == probe.preview.tab_id
                    && pane.phase == RuntimePanePhase::RestorePending
                    && pane.session_id.is_none()
            });
        let pane_has_orphan = self
            .workspace_runtime
            .orphan_cleanup_for_pane(&probe.preview.pane_id)
            .next()
            .is_some();
        if !preview_is_current || !owner_is_unstarted || !pane_is_pending || pane_has_orphan {
            return div().size_0().into_any_element();
        }
        workspace_split_candidate_size_probe(cx.weak_entity(), probe, self.terminal_metrics.clone())
            .into_any_element()
    }

    /// 只在 exact operation、candidate、活动渲染代次和 pending pane 全部匹配时启动一次既有 restore。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_workspace_split_size_measurement(
        &mut self,
        measurement: SplitCandidateMeasurement,
        cx: &mut Context<Self>,
    ) {
        let preview = measurement.preview;
        if self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
        {
            return;
        }
        let operation_matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .is_some_and(|operation| {
                matches!(
                    operation,
                    WorkspaceResourceOperation::CreatePane {
                        operation_id,
                        tab_id,
                        source_pane_id,
                        pane,
                    } if operation_id == &preview.operation_id
                        && tab_id == &preview.tab_id
                        && source_pane_id == &preview.source_pane_id
                        && pane.id == preview.pane_id
                )
            });
        let candidate_layout_matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_create_pane_candidate(&preview.operation_id))
            .is_some_and(|candidate| {
                candidate.active_tab_id.as_ref() == Some(&preview.tab_id)
                    && candidate.terminal_tabs.iter().any(|tab| {
                        tab.id() == &preview.tab_id
                            && tab.terminal_layout().is_some_and(|layout| {
                                layout.contains_pane(&preview.source_pane_id)
                                    && layout.contains_pane(&preview.pane_id)
                            })
                    })
                    && candidate
                        .terminal_panes
                        .iter()
                        .any(|pane| pane.id == preview.pane_id)
            });
        let active_tab_matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.snapshot().active_tab_id.as_ref())
            == Some(&preview.tab_id);
        let view_matches = self
            .workspace_renders
            .get(&preview.tab_id)
            .is_some_and(|render| {
                render
                    .read(cx)
                    .split_candidate_matches(&preview, measurement.layout_generation, cx)
            });
        let pane_is_pending = self
            .workspace_runtime
            .pane(&preview.pane_id)
            .is_some_and(|pane| {
                pane.tab_id == preview.tab_id
                    && pane.phase == RuntimePanePhase::RestorePending
                    && pane.session_id.is_none()
            });
        let pane_has_orphan = self
            .workspace_runtime
            .orphan_cleanup_for_pane(&preview.pane_id)
            .next()
            .is_some();
        if !operation_matches {
            return;
        }
        if !candidate_layout_matches
            || !active_tab_matches
            || !view_matches
            || !pane_is_pending
            || pane_has_orphan
        {
            let stage = if !candidate_layout_matches {
                "candidate_layout"
            } else if !active_tab_matches {
                "active_tab"
            } else if !view_matches {
                "render_generation"
            } else if pane_has_orphan {
                "orphan_owner"
            } else {
                "pending_pane"
            };
            retry_split_candidate_after_root_rejection(
                &preview,
                measurement.layout_generation,
                stage,
                cx,
            );
            return;
        }
        let Some(persisted) = self
            .workspace_runtime
            .pane(&preview.pane_id)
            .filter(|pane| pane.persisted.id == preview.pane_id)
            .map(|pane| pane.persisted.clone())
        else {
            retry_split_candidate_after_root_rejection(
                &preview,
                measurement.layout_generation,
                "persisted_pane",
                cx,
            );
            return;
        };
        let Some(owner) = self.workspace_resource_operations.get_mut(&preview.pane_id) else {
            return;
        };
        if owner.operation_id() != preview.operation_id
            || owner.cancel_requested()
            || !owner
                .try_admit_split_measurement(&preview.operation_id, measurement.layout_generation)
        {
            return;
        }
        // 准入和尺寸写入共用刚核对的运行阶段；此同步段不能插入异步操作，以免目标核对后发生换代。
        if self
            .workspace_runtime
            .set_pending_size(&preview.pane_id, measurement.size)
            .is_err()
        {
            let reset = owner.reset_split_measurement_for_retry(&preview.operation_id);
            self.status = Some(crate::locale::text(
                if reset {
                    "分屏尺寸未能写入运行态；候选仍未启动，可取消后重新分屏。"
                } else {
                    "分屏尺寸写入失败且操作代次无法回滚；请检查工作区状态。"
                },
                if reset {
                    "The split size could not be written to runtime state; the candidate was not started and can be cancelled before trying again."
                } else {
                    "The split size write failed and its operation generation could not be rolled back; inspect the workspace state."
                },
            ));
            cx.notify();
            return;
        }
        clear_split_probe_failure(&preview.operation_id, measurement.layout_generation);
        self.restore_workspace_pane(preview.tab_id, persisted, cx);
    }

    /// 仅给 controller 标记为待恢复的持久 pane 测量首帧；新建 pane 继续使用原尺寸策略。
    ///
    /// @author kongweiguang
    pub(crate) fn workspace_restore_size_probe_element(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let item_id = WorkspaceRestoreItemId::Pane(pane_id.clone());
        let is_pending_restore = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.restore_status(&item_id) == Some(WorkspaceRestoreStatus::Pending)
                    && controller.snapshot().active_tab_id.as_ref() == Some(&tab_id)
            });
        let pane_is_pending = self.workspace_runtime.pane(&pane_id).is_some_and(|pane| {
            pane.tab_id == tab_id
                && pane.phase == RuntimePanePhase::RestorePending
                && pane.session_id.is_none()
        });
        if self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || !is_pending_restore
            || !pane_is_pending
        {
            return div().size_0().into_any_element();
        }
        workspace_restore_size_probe(
            cx.weak_entity(),
            tab_id,
            pane_id.clone(),
            self.workspace_restore_generations.get(&pane_id).copied(),
            self.terminal_metrics.clone(),
        )
        .into_any_element()
    }

    /// 仅接受当前活动且未被关闭操作持有的 pane 测量；即使同标签其它 session 正等 close ACK，也不能复活待关闭占位。
    /// 更新 pending size 后仍复用既有 resolver、代次和启动管线。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_workspace_restore_size_measurement(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        measured_generation: Option<u64>,
        size: TerminalSize,
        cx: &mut Context<Self>,
    ) {
        if self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
        {
            return;
        }
        let item_id = WorkspaceRestoreItemId::Pane(pane_id.clone());
        let is_pending_restore = self
            .workspace_controller
            .as_ref()
            .is_some_and(|controller| {
                controller.restore_status(&item_id) == Some(WorkspaceRestoreStatus::Pending)
                    && controller.snapshot().active_tab_id.as_ref() == Some(&tab_id)
                    && !restore_probe_is_owned_by_close(
                        controller.pending_resource_operation(),
                        &tab_id,
                        &pane_id,
                    )
            });
        if !is_pending_restore
            || !restore_probe_generation_matches(
                measured_generation,
                self.workspace_restore_generations.get(&pane_id).copied(),
            )
        {
            return;
        }
        let Some((current_size, persisted)) =
            self.workspace_runtime.pane(&pane_id).and_then(|pane| {
                (pane.tab_id == tab_id
                    && pane.session_id.is_none()
                    && pane.phase == RuntimePanePhase::RestorePending)
                    .then(|| (pane.size, pane.persisted.clone()))
            })
        else {
            return;
        };
        if current_size != size
            && self
                .workspace_runtime
                .set_pending_size(&pane_id, size)
                .is_err()
        {
            return;
        }
        if measured_generation
            .is_some_and(|generation| self.workspace_restore_is_current(&pane_id, generation))
        {
            return;
        }
        self.restore_workspace_pane(tab_id, persisted, cx);
        // 下一轮 prepaint 捕获新代次；旧回调因代次不符而失效。
        cx.notify();
    }

    /// 运行态 resize 要求活动输入 handler；Exited/Failed 历史重排使用保留的 view binding 并进入同一 FIFO。
    /// 尺寸变化先取消未完成拖选 token，避免 engine resize 后旧 UI 端点重建错误锚点。
    ///
    /// @author kongweiguang
    pub(crate) fn handle_terminal_canvas_resize(
        &mut self,
        request: CanvasResizeRequest,
        cx: &mut Context<Self>,
    ) {
        let binding = &request.binding;
        let is_current_registration = self
            .pane_input_handlers
            .get(&binding.session_id)
            .is_some_and(|registration| binding_matches_current(&request, &registration.binding));
        let is_current_view = self.terminal_view_bindings.get(&binding.session_id) == Some(binding);

        match &binding.owner {
            SessionOwner::Ui { tab_id, pane_id } => {
                if !is_current_view {
                    return;
                }
                let is_ready_pane = self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                    pane.phase == RuntimePanePhase::Ready
                        && pane.tab_id == *tab_id
                        && pane.session_id.as_ref() == Some(&binding.session_id)
                });
                if !is_ready_pane {
                    return;
                }
                let Some(pane) = self.workspace_runtime.pane(pane_id) else {
                    return;
                };
                let observation = pane.target.as_ref().and_then(|target| {
                    self.session_reconnect.panel_observation(
                        pane_id,
                        tab_id,
                        &binding.session_id,
                        target,
                    )
                });
                if history_resize_binding_is_current(
                    binding,
                    self.terminal_view_bindings.get(&binding.session_id),
                    observation.as_ref(),
                ) {
                    let is_new_size = self
                        .workspace_runtime
                        .history_view_resize_needs_enqueue(pane_id, request.size)
                        .unwrap_or(false)
                        && self.failed_session_resizes.get(&binding.session_id)
                            != Some(&request.size);
                    if is_new_size {
                        self.cancel_terminal_drag_for_session(&binding.session_id, cx);
                    }
                    self.enqueue_history_view_resize(binding.clone(), request.size, cx);
                    return;
                }
                if observation_proves_session_not_running(binding, observation.as_ref())
                    || !is_current_registration
                {
                    return;
                }
                let is_new_size = self
                    .workspace_runtime
                    .resize_needs_enqueue(pane_id, request.size)
                    .unwrap_or(false)
                    && self.failed_session_resizes.get(&binding.session_id) != Some(&request.size);
                if is_new_size {
                    self.cancel_terminal_drag_for_session(&binding.session_id, cx);
                }
                self.resize_workspace_pane_from_canvas(pane_id.clone(), request.size, cx);
            }
            SessionOwner::Agent { agent_session_id } => {
                if !is_current_registration
                    || !is_current_view
                    || self.selected_agent_session_id.as_ref() != Some(agent_session_id)
                    || self.agent_terminal_session_id.as_ref() != Some(&binding.session_id)
                    || self.agent_terminal_size == Some(request.size)
                {
                    return;
                }
                if self.agent_terminal_size != Some(request.size)
                    && self.failed_session_resizes.get(&binding.session_id) != Some(&request.size)
                {
                    self.cancel_terminal_drag_for_session(&binding.session_id, cx);
                }
                self.resize_agent_terminal_from_canvas(
                    binding.session_id.clone(),
                    request.size,
                    cx,
                );
            }
            SessionOwner::Mcp | SessionOwner::Service { .. } => {}
        }
    }
}

/// 只让精确 view binding 的 Exited/Failed UI pane 走历史重排，拒绝结束过渡态和旧代次。
///
/// @author kongweiguang
fn history_resize_binding_is_current(
    binding: &kerminal_application::sessions::UiSessionBinding,
    view_binding: Option<&kerminal_application::sessions::UiSessionBinding>,
    observation: Option<&SessionPanelObservation>,
) -> bool {
    matches!(&binding.owner, SessionOwner::Ui { .. })
        && view_binding == Some(binding)
        && observation.is_some_and(|observation| {
            observation.binding == *binding
                && matches!(
                    observation.lifecycle,
                    SessionLifecycle::Exited | SessionLifecycle::Failed
                )
        })
}

/// 关闭候选提交前仍保留旧快照；必须按同 tab/pane 的 close owner 拒绝已排队的 deferred probe。
/// 不阻断其它 pane 或 CreatePane，避免把整个工作区变成全局恢复门禁。
///
/// @author kongweiguang
fn restore_probe_is_owned_by_close(
    operation: Option<&WorkspaceResourceOperation>,
    tab_id: &TabId,
    pane_id: &PaneId,
) -> bool {
    match operation {
        Some(WorkspaceResourceOperation::ClosePane {
            tab_id: closing_tab,
            pane_id: closing_pane,
            ..
        }) => closing_tab == tab_id && closing_pane == pane_id,
        Some(WorkspaceResourceOperation::CloseTab {
            tab_id: closing_tab,
            pane_ids,
            ..
        }) => closing_tab == tab_id && pane_ids.contains(pane_id),
        Some(WorkspaceResourceOperation::CreatePane { .. }) | None => false,
    }
}

/// 精确匹配恢复尝试代次，拒绝重试或关闭后迟到的首帧回调。
///
/// @author kongweiguang
fn restore_probe_generation_matches(measured: Option<u64>, current: Option<u64>) -> bool {
    measured == current
}

#[cfg(test)]
#[path = "../../tests/unit/root_canvas_geometry.rs"]
mod tests;
