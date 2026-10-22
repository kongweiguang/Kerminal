//! 工作区标签与真实资源关闭协调；仅在资源回执后提交 reducer 候选。
//!
//! @author kongweiguang

use gpui_kit::{
    AppContext as _, Context, FocusHandle, SharedString, Window, component::WindowExt as _,
};
use kerminal_application::platform::external_launch::ExternalTargetLease;
use kerminal_application::sessions::{
    DEFAULT_SESSION_CLOSE_BUDGET, SessionDeadline, UiSessionBinding,
};
use kerminal_application::workspace::WorkspaceSessionTab;
use kerminal_domain::{
    files::FileEndpoint,
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionOwner, SessionTarget, TerminalSize},
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::snapshot::RenderSnapshot;
use std::{collections::HashMap, future::Future, time::Duration};

use super::{
    CenterPanel, PendingPaneFocusIntent, RootView, keybinding_focus, keybindings,
    logical_input_modal_blocked,
    session_queue::QueueDropSummary,
    workspace_orphan_cleanup::OrphanCleanupCompletion,
    workspace_runtime::{OrphanCleanupInput, RuntimePanePhase, WorkspaceRuntime},
};
use crate::features::{
    workspace_controller::{
        WorkspaceController, WorkspaceControllerEffect, WorkspaceResourceOperation,
        WorkspaceRestoreFailure, WorkspaceRestoreItemId, WorkspaceRestoreStatus,
    },
    workspace_panes::SplitCandidatePreview,
    workspace_tabs::{
        WorkspaceCloseBatchIntent, WorkspaceTabCloseConfirmationFocusPhase,
        WorkspaceTabCloseConfirmationFocusTarget, WorkspaceTabEvent,
        partition_shared_sftp_workbench_close_batch, workspace_tab_event_targets,
    },
};

/// 只有 Activate 被 reducer 接受时才允许 Root 同步可见 pane；拒绝、未就绪和资源操作都必须闭合导航门禁。
///
/// @author kongweiguang
pub(super) fn workspace_tab_activation_accepted<E>(
    event: &WorkspaceTabEvent,
    result: Option<&Result<WorkspaceControllerEffect, E>>,
) -> bool {
    matches!(event, WorkspaceTabEvent::Activate(_))
        && matches!(
            result,
            Some(Ok(
                WorkspaceControllerEffect::Unchanged | WorkspaceControllerEffect::Changed { .. }
            ))
        )
}

/// 分屏创建占用 reducer 唯一资源槽时保留原始关闭目标，避免并发事件覆盖用户意图。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceDeferredCloseIntent {
    Pane { tab_id: TabId, pane_id: PaneId },
    Tab { tab_id: TabId },
    Batch(WorkspaceCloseBatchIntent),
}

/// 重复事件保持幂等；较晚的不同目标不得替换最先保存的关闭意图。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceDeferredCloseRequest {
    Stored,
    Duplicate,
    AlreadyPending,
}

impl WorkspaceDeferredCloseIntent {
    /// 只把创建源、候选 pane 或其所属 Tab 的关闭关联到 split，避免误取消其它工作区操作。
    ///
    /// @author kongweiguang
    pub(super) fn targets_split(
        &self,
        split_tab_id: &TabId,
        source_pane_id: &PaneId,
        candidate_pane_id: &PaneId,
    ) -> bool {
        match self {
            Self::Pane { tab_id, pane_id } => {
                tab_id == split_tab_id
                    && (pane_id == source_pane_id || pane_id == candidate_pane_id)
            }
            Self::Tab { tab_id } => tab_id == split_tab_id,
            Self::Batch(intent) => intent.tab_ids().contains(split_tab_id),
        }
    }
}

/// 只有关闭目标确实是已提交布局中的唯一 pane 才进入关闭 Tab 确认门禁。
/// 候选分屏尚未提交到该树，不能被它的 pane 数量误认成最后一个原有 pane。
///
/// @author kongweiguang
pub(super) fn is_last_committed_workspace_pane(
    layout: Option<&TerminalLayoutNode>,
    pane_id: &PaneId,
) -> bool {
    layout.is_some_and(|layout| {
        let pane_ids = layout.pane_ids();
        pane_ids.len() == 1 && pane_ids.first() == Some(pane_id)
    })
}

/// 关闭意图重放时，已取消的候选 pane 已从 runtime 移除；将该精确 stale close 视为已满足。
///
/// @author kongweiguang
pub(super) fn is_removed_uncommitted_workspace_pane(
    layout: Option<&TerminalLayoutNode>,
    runtime_has_pane: bool,
    pane_id: &PaneId,
) -> bool {
    !runtime_has_pane && !layout.is_some_and(|layout| layout.contains_pane(pane_id))
}

/// 在既有 pane→操作映射项中合并准入、取消和关闭焦点意图，不另建队列或状态机。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct WorkspaceResourceOperationEntry {
    operation_id: String,
    cancel_requested: bool,
    deferred_close: Option<WorkspaceDeferredCloseIntent>,
    split_measured_layout_generation: Option<u64>,
    close_focus: Option<CloseSurvivorFocusIntent>,
    close_tab_focus: Option<CloseTabFocusIntent>,
}

impl WorkspaceResourceOperationEntry {
    /// 新 reducer 操作从未测量、未取消且无焦点票据状态开始，避免继承前一代 pane 的清理状态。
    ///
    /// @author kongweiguang
    pub(super) fn new(operation_id: String) -> Self {
        Self {
            operation_id,
            cancel_requested: false,
            deferred_close: None,
            split_measured_layout_generation: None,
            close_focus: None,
            close_tab_focus: None,
        }
    }

    /// 只读暴露操作身份，供 controller 与异步回执做精确匹配。
    ///
    /// @author kongweiguang
    pub(super) fn operation_id(&self) -> &str {
        &self.operation_id
    }

    /// 区分关闭到达前是否已有真实布局测量准入，以决定撤销占位或等待资源 ACK。
    ///
    /// @author kongweiguang
    pub(super) fn split_measurement_started(&self) -> bool {
        self.split_measured_layout_generation.is_some()
    }

    /// 仅允许精确且未取消的操作记录一次测量准入，迟到事件不能复活已取消 split。
    ///
    /// @author kongweiguang
    pub(super) fn try_admit_split_measurement(
        &mut self,
        operation_id: &str,
        layout_generation: u64,
    ) -> bool {
        if self.operation_id != operation_id
            || self.cancel_requested
            || self.split_measured_layout_generation.is_some()
        {
            return false;
        }
        self.split_measured_layout_generation = Some(layout_generation);
        true
    }

    /// 只重置同一失败操作的测量标记；调用方须先确认无 session、orphan 与取消意图。
    ///
    /// @author kongweiguang
    pub(super) fn reset_split_measurement_for_retry(&mut self, operation_id: &str) -> bool {
        if self.operation_id != operation_id
            || self.cancel_requested
            || self.deferred_close.is_some()
        {
            return false;
        }
        self.split_measured_layout_generation = None;
        true
    }

    /// 在同一 entry 中关闭测量门并保留首个关闭目标，避免第二份 map 与主状态分叉。
    ///
    /// @author kongweiguang
    pub(super) fn defer_close(
        &mut self,
        intent: WorkspaceDeferredCloseIntent,
    ) -> WorkspaceDeferredCloseRequest {
        match self.deferred_close.as_ref() {
            Some(existing) if existing == &intent => WorkspaceDeferredCloseRequest::Duplicate,
            Some(_) => WorkspaceDeferredCloseRequest::AlreadyPending,
            None => {
                self.cancel_requested = true;
                self.deferred_close = Some(intent);
                WorkspaceDeferredCloseRequest::Stored
            }
        }
    }

    /// 同一 split owner 首次接收关闭时一起保存 pane 与 tab 的原焦点票据，重复请求不能升级票据。
    ///
    /// @author kongweiguang
    fn defer_close_with_focus_tickets(
        &mut self,
        intent: WorkspaceDeferredCloseIntent,
        close_focus: Option<CloseSurvivorFocusIntent>,
        close_tab_focus: Option<CloseTabFocusIntent>,
    ) -> WorkspaceDeferredCloseRequest {
        let request = self.defer_close(intent);
        if request == WorkspaceDeferredCloseRequest::Stored {
            self.close_focus = close_focus;
            self.close_tab_focus = close_tab_focus;
        }
        request
    }

    /// Restore 已有 Close operation 时仅借用同一 pane→entry 保留首个来源，不改变取消、资源 ACK 或测量状态。
    ///
    /// @author kongweiguang
    fn retain_restore_close_focus(
        &mut self,
        operation_id: &str,
        close_focus: Option<CloseSurvivorFocusIntent>,
    ) -> bool {
        if self.operation_id != operation_id
            || self.cancel_requested
            || self.deferred_close.is_some()
            || self.close_focus.is_some()
            || close_focus.is_none()
        {
            return false;
        }
        self.close_focus = close_focus;
        true
    }

    /// Restore 等待期间为 terminal tab 关闭保留独立导航票据，不与 pane survivor 焦点资料混用。
    ///
    /// @author kongweiguang
    fn retain_restore_close_tab_focus(
        &mut self,
        operation_id: &str,
        close_tab_focus: Option<CloseTabFocusIntent>,
    ) -> bool {
        if self.operation_id != operation_id
            || self.cancel_requested
            || self.deferred_close.is_some()
            || self.close_tab_focus.is_some()
            || close_tab_focus.is_none()
        {
            return false;
        }
        self.close_tab_focus = close_tab_focus;
        true
    }

    /// 暴露取消事实但不转移其所有权，避免 UI 再维护独立取消表。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_requested(&self) -> bool {
        self.cancel_requested
    }

    /// 一次性取得 deferred close 的原始目标及两类独立焦点票据，防止 ACK 回放时重捕获较新的用户焦点。
    ///
    /// @author kongweiguang
    fn take_deferred_close_after_cleanup_with_focus(
        &mut self,
        operation_id: &str,
        cleanup_confirmed: bool,
    ) -> Option<(
        WorkspaceDeferredCloseIntent,
        Option<CloseSurvivorFocusIntent>,
        Option<CloseTabFocusIntent>,
    )> {
        if self.operation_id != operation_id || !self.cancel_requested || !cleanup_confirmed {
            return None;
        }
        self.deferred_close
            .take()
            .map(|intent| (intent, self.close_focus.take(), self.close_tab_focus.take()))
    }
}

#[derive(Clone)]
enum WorkspaceCloseResource {
    Pane(PaneId),
    Orphan { generation: u64 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RestoreCloseAction {
    WaitForAcknowledgement,
    CloseSession,
    RemoveEmptyPane,
    Reject,
}

/// 只有已准入的恢复才等待资源回执；未显示且从未准入的占位没有可等待的任务。
/// 已取消的准入仍算在途，不能从 token 不再 current 推断资源已经收尾。
///
/// @author kongweiguang
fn restore_close_action(
    phase: RuntimePanePhase,
    has_session: bool,
    has_restore_admission: bool,
) -> RestoreCloseAction {
    match phase {
        RuntimePanePhase::RestorePending if !has_session && !has_restore_admission => {
            RestoreCloseAction::RemoveEmptyPane
        }
        RuntimePanePhase::RestorePending => RestoreCloseAction::WaitForAcknowledgement,
        RuntimePanePhase::Ready | RuntimePanePhase::CloseFailed if has_session => {
            RestoreCloseAction::CloseSession
        }
        RuntimePanePhase::RestoreFailed if !has_session => RestoreCloseAction::RemoveEmptyPane,
        RuntimePanePhase::Closing => RestoreCloseAction::Reject,
        _ if has_session => RestoreCloseAction::Reject,
        _ => RestoreCloseAction::RemoveEmptyPane,
    }
}

/// 关闭撤销的是尚未准入的恢复意图，不是伪造资源 ACK；两份既有投影必须同时变为可重试失败。
/// 混合标签的真实 close 若失败，此占位不会因取消墓碑而永远等待不存在的任务。
///
/// @author kongweiguang
fn cancel_unadmitted_restore_for_close(
    runtime: &mut WorkspaceRuntime,
    controller: &mut WorkspaceController,
    pane_id: &PaneId,
) -> bool {
    let item_id = WorkspaceRestoreItemId::Pane(pane_id.clone());
    if !runtime.pane(pane_id).is_some_and(|pane| {
        pane.phase == RuntimePanePhase::RestorePending && pane.session_id.is_none()
    }) || controller.restore_status(&item_id) != Some(WorkspaceRestoreStatus::Pending)
    {
        return false;
    }
    if runtime.mark_restore_failed(pane_id).is_err() {
        return false;
    }
    controller
        .record_restore_result(item_id, Err(WorkspaceRestoreFailure::Cancelled))
        .is_ok()
}

#[derive(Clone)]
struct SessionCloseRequest {
    resource: WorkspaceCloseResource,
    session_id: SessionId,
    owner: CleanupOwnerSnapshot,
}

/// Async close 回执可能迟到；它自带原始资源资料，不能从已经换代的 pane 猜 owner。
#[derive(Clone)]
struct CleanupOwnerSnapshot {
    tab_id: TabId,
    pane_id: PaneId,
    target: Option<SessionTarget>,
    size: TerminalSize,
    snapshot: Option<RenderSnapshot>,
    external_lease: Option<ExternalTargetLease>,
    transient: bool,
}

/// 只携带本次关闭前的用户焦点意图和完整幸存 binding；持有旧焦点句柄只用于核对，不延长 session 生命周期。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CloseSurvivorFocusIntent {
    focus: PendingPaneFocusIntent,
    binding: UiSessionBinding,
}

/// 活动 terminal tab 关闭只保留导航/焦点代次与可比较句柄，不持有 session 或重连所有权。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CloseTabFocusIntent {
    tab_id: TabId,
    navigation_generation: u64,
    pane_focus_generation: u64,
    expected_focus: Option<FocusHandle>,
    closing_pane_focus: Option<FocusHandle>,
    closing_search_focus: Option<FocusHandle>,
}

/// 确认框前的焦点快照按请求代次保存；`None` 也表示已捕获但不具备交接资格。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingCloseTabConfirmationFocus {
    request_id: u64,
    tab_id: TabId,
    focus: Option<CloseTabFocusIntent>,
    confirmed: bool,
}

/// 批量关闭确认前的焦点快照只对原始固定目标集和请求代次有效。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingCloseBatchConfirmationFocus {
    request_id: u64,
    intent: WorkspaceCloseBatchIntent,
    target_tab_id: Option<TabId>,
    focus: Option<CloseTabFocusIntent>,
    confirmed: bool,
}

impl PendingCloseBatchConfirmationFocus {
    /// 即使捕获时没有合格终端焦点，也保留请求身份，防止 ACK 或重试时按新焦点补造票据。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        request_id: u64,
        intent: WorkspaceCloseBatchIntent,
        target_tab_id: Option<TabId>,
        focus: Option<CloseTabFocusIntent>,
    ) -> Self {
        let target_tab_id = target_tab_id.filter(|tab_id| intent.tab_ids().contains(tab_id));
        let focus = target_tab_id
            .as_ref()
            .and(focus)
            .filter(|focus| Some(&focus.tab_id) == target_tab_id.as_ref());
        Self {
            request_id,
            intent,
            target_tab_id,
            focus,
            confirmed: false,
        }
    }

    /// 只有相同代次和固定 intent 的弹框回执可以改变请求状态。
    ///
    /// @author kongweiguang
    pub(super) fn confirm(&mut self, request_id: u64, intent: &WorkspaceCloseBatchIntent) -> bool {
        if self.request_id != request_id || self.intent != *intent {
            return false;
        }
        self.confirmed = true;
        true
    }

    /// 取消仅撤销当前精确请求，迟到取消不会触及更新的批次。
    ///
    /// @author kongweiguang
    pub(super) fn cancel(&self, request_id: u64, intent: &WorkspaceCloseBatchIntent) -> bool {
        self.request_id == request_id && self.intent == *intent
    }

    /// 批次事件只消费一次确认票据；`None` 焦点仍保留以防失败重试重新捕获。
    ///
    /// @author kongweiguang
    pub(super) fn take_confirmed(
        &mut self,
        request_id: u64,
        intent: &WorkspaceCloseBatchIntent,
    ) -> Option<(Option<TabId>, Option<CloseTabFocusIntent>)> {
        if !self.confirmed || self.request_id != request_id || self.intent != *intent {
            return None;
        }
        self.confirmed = false;
        Some((self.target_tab_id.take(), self.focus.take()))
    }
}

/// 跨越所有 CloseTab ACK 保留同一焦点来源；只在固定批次完整成功后允许 Root 交接。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PendingCloseBatchFocus {
    intent: WorkspaceCloseBatchIntent,
    target_tab_id: Option<TabId>,
    focus: Option<CloseTabFocusIntent>,
    target_closed: bool,
    consumed: bool,
}

impl PendingCloseBatchFocus {
    /// 过滤临时工作台后若原活动 tab 已不在目标集中，就撤销其焦点资格但仍保留批次身份。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        intent: WorkspaceCloseBatchIntent,
        target_tab_id: Option<TabId>,
        focus: Option<CloseTabFocusIntent>,
    ) -> Self {
        let target_tab_id = target_tab_id.filter(|tab_id| intent.tab_ids().contains(tab_id));
        let focus = target_tab_id
            .as_ref()
            .and(focus)
            .filter(|focus| Some(&focus.tab_id) == target_tab_id.as_ref());
        Self {
            intent,
            target_tab_id,
            focus,
            target_closed: false,
            consumed: false,
        }
    }

    /// 失败及重试保持初始来源；只记录目标 tab 在本固定批次中真实成功提交。
    ///
    /// @author kongweiguang
    pub(super) fn note_tab_closed(&mut self, tab_id: &TabId) {
        if self.target_tab_id.as_ref() == Some(tab_id) {
            self.target_closed = true;
        }
    }

    /// 整批 ACK 完成时一次性消费原票据；目标未成功关闭或 intent 换代都不产生交接。
    ///
    /// @author kongweiguang
    pub(super) fn take_after_successful_batch(
        &mut self,
        intent: &WorkspaceCloseBatchIntent,
    ) -> Option<Option<CloseTabFocusIntent>> {
        if self.intent != *intent || self.consumed {
            return None;
        }
        self.consumed = true;
        Some(self.target_closed.then(|| self.focus.take()).flatten())
    }

    /// 同一失败批次只能复用首次捕获的来源，菜单重试的新焦点不能替换票据。
    ///
    /// @author kongweiguang
    fn matches_intent(&self, intent: &WorkspaceCloseBatchIntent) -> bool {
        self.intent == *intent
    }
}

impl PendingCloseTabConfirmationFocus {
    /// 新确认覆盖已取消或过期请求，焦点票据只对这一稳定 TabId 和代次有效。
    ///
    /// @author kongweiguang
    pub(super) fn new(request_id: u64, tab_id: TabId, focus: Option<CloseTabFocusIntent>) -> Self {
        Self {
            request_id,
            tab_id,
            focus,
            confirmed: false,
        }
    }

    /// 只有同一请求的确认回调能授权后续 Close 事件消费预弹层焦点。
    ///
    /// @author kongweiguang
    pub(super) fn confirm(&mut self, request_id: u64, tab_id: &TabId) -> bool {
        if self.request_id != request_id || self.tab_id != *tab_id {
            return false;
        }
        self.confirmed = true;
        true
    }

    /// 取消只清除同一确认请求，迟到的取消不能擦掉较新的票据。
    ///
    /// @author kongweiguang
    pub(super) fn cancel(&self, request_id: u64, tab_id: &TabId) -> bool {
        self.request_id == request_id && self.tab_id == *tab_id
    }

    /// Close 只消费已确认的完全匹配票据；内层 None 禁止 modal 中途重新捕获焦点。
    ///
    /// @author kongweiguang
    pub(super) fn take_confirmed(
        &mut self,
        request_id: u64,
        tab_id: &TabId,
    ) -> Option<Option<CloseTabFocusIntent>> {
        if !self.confirmed || self.request_id != request_id || self.tab_id != *tab_id {
            return None;
        }
        self.confirmed = false;
        Some(self.focus.take())
    }
}

/// Restore ACK 只移走精确 Close owner 的原始焦点资料；split 的取消状态和其它代次 entry 均保持原样。
///
/// @author kongweiguang
fn take_restore_close_focus(
    owners: &mut HashMap<PaneId, WorkspaceResourceOperationEntry>,
    pane_id: &PaneId,
    operation_id: &str,
) -> Option<CloseSurvivorFocusIntent> {
    let owner = owners.get(pane_id)?;
    if owner.operation_id() != operation_id
        || owner.cancel_requested
        || owner.deferred_close.is_some()
        || owner.close_focus.is_none()
    {
        return None;
    }
    owners.remove(pane_id)?.close_focus
}

/// Restore ACK 只消费同一 CloseTab owner 的首个焦点票据；失败、取消及旧操作代次不移除 entry。
///
/// @author kongweiguang
fn take_restore_close_tab_focus(
    owners: &mut HashMap<PaneId, WorkspaceResourceOperationEntry>,
    pane_id: &PaneId,
    operation_id: &str,
) -> Option<CloseTabFocusIntent> {
    let owner = owners.get(pane_id)?;
    if owner.operation_id() != operation_id
        || owner.cancel_requested
        || owner.deferred_close.is_some()
        || owner.close_tab_focus.is_none()
    {
        return None;
    }
    owners.remove(pane_id)?.close_tab_focus
}

/// 与 reducer 保持相同的 close 后 focused pane：关闭非当前 pane 不改变原焦点，绝不在回执时重新择邻。
///
/// @author kongweiguang
fn close_survivor_pane(
    layout: &TerminalLayoutNode,
    closing: &PaneId,
    focused: &PaneId,
) -> Option<PaneId> {
    if !layout.contains_pane(closing) || !layout.contains_pane(focused) {
        return None;
    }
    if focused == closing {
        layout.pane_ids().into_iter().find(|pane| pane != closing)
    } else {
        Some(focused.clone())
    }
}

/// 关闭回执不能选择新目标；用户导航、真实焦点、恢复代次和 session owner 任一改变都使原交接失效。
///
/// @author kongweiguang
fn close_survivor_focus_is_current(
    expected_generation: u64,
    current_generation: u64,
    focus_allowed_and_unchanged: bool,
    expected_binding: &UiSessionBinding,
    current_binding: Option<&UiSessionBinding>,
    expected_restore_generation: Option<u64>,
    current_restore_generation: Option<u64>,
) -> bool {
    expected_generation == current_generation
        && focus_allowed_and_unchanged
        && current_binding == Some(expected_binding)
        && expected_restore_generation == current_restore_generation
}

/// CloseTab ACK 只能沿仍未换代的原活动 tab 意图交接，焦点边界由窗口检查结果明确传入。
///
/// @author kongweiguang
fn close_tab_focus_is_current(
    intent: &CloseTabFocusIntent,
    closing_tab_id: &TabId,
    current_navigation_generation: u64,
    current_pane_focus_generation: u64,
    focus_still_owned: bool,
) -> bool {
    intent.tab_id == *closing_tab_id
        && intent.navigation_generation == current_navigation_generation
        && intent.pane_focus_generation == current_pane_focus_generation
        && focus_still_owned
}

/// 搜索关闭票据只允许原查询框、其退役后留下的无焦点态、原 pane 或 Root 继续完成交接。
/// GPUI 在精确 FocusHandle 对应的节点退出渲染树后会把 dispatch path 回退到 App；仅对票据中的原搜索/pane 句柄接受该回退。
/// Modal、其它 Editor 和新代次仍由此所有权门及调用方的导航/焦点代次门拒绝。
///
/// @author kongweiguang
fn close_tab_focus_owner_is_current<T: Eq>(
    current_focus: Option<&T>,
    expected_focus: Option<&T>,
    closing_pane_focus: Option<&T>,
    closing_search_focus: Option<&T>,
    root_focus: &T,
    focus_scope: keybindings::KeybindingFocusScope,
) -> bool {
    if focus_scope == keybindings::KeybindingFocusScope::Modal {
        return false;
    }

    if let Some(search_focus) = closing_search_focus {
        return match current_focus {
            None => true,
            Some(current) if current == search_focus => matches!(
                focus_scope,
                keybindings::KeybindingFocusScope::Editor
                    | keybindings::KeybindingFocusScope::Terminal
                    | keybindings::KeybindingFocusScope::Workspace
                    | keybindings::KeybindingFocusScope::App
            ),
            Some(current) if current == root_focus => matches!(
                focus_scope,
                keybindings::KeybindingFocusScope::App
                    | keybindings::KeybindingFocusScope::Workspace
                    | keybindings::KeybindingFocusScope::Terminal
            ),
            Some(current) if closing_pane_focus == Some(current) => matches!(
                focus_scope,
                keybindings::KeybindingFocusScope::Terminal
                    | keybindings::KeybindingFocusScope::Workspace
            ),
            Some(_) => false,
        };
    }

    current_focus.is_none_or(|current| {
        let is_original_pane_focus = closing_pane_focus == Some(current);
        let focus_scope_is_owned = matches!(
            focus_scope,
            keybindings::KeybindingFocusScope::Terminal
                | keybindings::KeybindingFocusScope::Workspace
        ) || (is_original_pane_focus
            && focus_scope == keybindings::KeybindingFocusScope::App);
        (expected_focus == Some(current) || is_original_pane_focus || root_focus == current)
            && focus_scope_is_owned
    })
}

/// 按同一个绝对截止时间逐个关闭资源，并保留每项独立回执，确保一项失败不会跳过后续 owner。
async fn close_sessions_until<F, Fut>(
    requests: Vec<SessionCloseRequest>,
    deadline: SessionDeadline,
    mut close_one: F,
) -> Vec<(SessionCloseRequest, Result<(), String>)>
where
    F: FnMut(SessionId, SessionDeadline) -> Fut,
    Fut: Future<Output = Result<(), String>>,
{
    let mut results = Vec::with_capacity(requests.len());
    for request in requests {
        let result = close_one(request.session_id.clone(), deadline).await;
        results.push((request, result));
    }
    results
}

impl RootView {
    /// 只记录待关闭输入 owner 的物理焦点，或该关闭按钮原生键盘 click 证明的精确控件焦点。
    /// 固定 reducer close 后的 focused survivor；未绑定 pane 不自动创建或改绑，ACK 也不得重新择邻。
    ///
    /// @author kongweiguang
    fn capture_close_survivor_focus(
        &mut self,
        operation: &WorkspaceResourceOperation,
        keyboard_close_focus: Option<&FocusHandle>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<CloseSurvivorFocusIntent> {
        let WorkspaceResourceOperation::ClosePane {
            tab_id, pane_id, ..
        } = operation
        else {
            return None;
        };
        self.capture_close_survivor_focus_for_pane(
            tab_id,
            pane_id,
            keyboard_close_focus,
            window,
            cx,
        )
    }

    /// Split 仍占资源槽时没有 Close operation，故以原始 pane 身份捕获同一交接资料；ACK 重放不能调用此入口。
    /// 仅精确的未提交 preview pane 可借用现有候选布局定位幸存者，普通关闭仍只依据已提交布局。
    ///
    /// @author kongweiguang
    fn capture_close_survivor_focus_for_pane(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        keyboard_close_focus: Option<&FocusHandle>,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<CloseSurvivorFocusIntent> {
        if !window.is_window_active()
            || window.has_active_prompt()
            || self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || self.focused_agent_terminal
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
        {
            return None;
        }
        if let Some(close_focus) = keyboard_close_focus {
            if !close_focus.is_focused(window) {
                return None;
            }
        } else {
            let closing_binding = self.logical_focused_pane_binding(cx)?;
            if closing_binding.owner
                != (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: pane_id.clone(),
                })
                || !self.pane_input_binding_is_focused(&closing_binding, window, cx)
            {
                return None;
            }
        }
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        if snapshot.active_tab_id.as_ref() != Some(tab_id) {
            return None;
        }
        let committed_layout = snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)?
            .terminal_layout()?;
        let preview = self.pending_workspace_split_preview(cx);
        let layout = preview
            .as_ref()
            .filter(|(preview, _, _)| {
                preview.tab_id == *tab_id
                    && preview.pane_id == *pane_id
                    && snapshot
                        .focused_pane_id
                        .as_ref()
                        .is_some_and(|focused| committed_layout.contains_pane(focused))
            })
            .map_or(committed_layout, |(_, layout, _)| layout);
        let survivor_pane =
            close_survivor_pane(layout, pane_id, snapshot.focused_pane_id.as_ref()?)?;
        let survivor = self.workspace_runtime.pane(&survivor_pane)?;
        if survivor.phase != RuntimePanePhase::Ready || survivor.tab_id != *tab_id {
            return None;
        }
        let session_id = survivor.session_id.as_ref()?;
        let binding = self.pane_input_handlers.get(session_id)?.binding.clone();
        if binding.session_id != *session_id
            || binding.owner
                != (SessionOwner::Ui {
                    tab_id: tab_id.clone(),
                    pane_id: survivor_pane.clone(),
                })
        {
            return None;
        }
        let expected_focus = window.focused(cx)?;
        self.clear_pending_pane_focus();
        Some(CloseSurvivorFocusIntent {
            focus: PendingPaneFocusIntent {
                generation: self.pane_focus_intent_generation,
                tab_id: tab_id.clone(),
                pane_id: survivor_pane.clone(),
                session_id: Some(binding.session_id.clone()),
                restore_generation: self
                    .workspace_restore_generations
                    .get(&survivor_pane)
                    .copied(),
                expected_focus: Some(expected_focus),
            },
            binding,
        })
    }

    /// 只在原 close 操作成功提交后排窗口交接；用户切焦点、模态或退出准备都优先于晚 ACK。
    /// 完整 binding 与恢复代次仍相同时复用原焦点入口，不直发按键、不绕过关闭或退出门禁。
    ///
    /// @author kongweiguang
    fn complete_close_survivor_focus(
        &mut self,
        handoff: Option<CloseSurvivorFocusIntent>,
        cx: &mut Context<Self>,
    ) {
        let Some(handoff) = handoff else {
            return;
        };
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    let current_binding = root.logical_focused_pane_binding(cx);
                    let focus_allowed_and_unchanged = window.is_window_active()
                        && !window.has_active_prompt()
                        && !root.host_shutdown_requested
                        && root.center_panel == CenterPanel::Terminal
                        && !root.connection_prompt_modal_open
                        && !root.split_target_picker.read(cx).is_open()
                        && root
                            .workspace_controller
                            .as_ref()
                            .is_some_and(|controller| {
                                let snapshot = controller.snapshot();
                                snapshot.active_tab_id.as_ref() == Some(&handoff.focus.tab_id)
                                    && snapshot.focused_pane_id.as_ref()
                                        == Some(&handoff.focus.pane_id)
                            })
                        && !logical_input_modal_blocked(
                            root.preferences_modal_open,
                            root.connections_modal_open,
                            root.snippet_editor_modal_open,
                            root.root_confirmation_owner.is_some(),
                            root.pending_quit_intent.is_some() || root.quit_ready_intent.is_some(),
                        )
                        && window.focused(cx) == handoff.focus.expected_focus;
                    if close_survivor_focus_is_current(
                        handoff.focus.generation,
                        root.pane_focus_intent_generation,
                        focus_allowed_and_unchanged,
                        &handoff.binding,
                        current_binding.as_ref(),
                        handoff.focus.restore_generation,
                        root.workspace_restore_generations
                            .get(&handoff.focus.pane_id)
                            .copied(),
                    ) {
                        root.request_workspace_pane_focus(
                            handoff.focus.tab_id,
                            handoff.focus.pane_id,
                            window,
                            cx,
                        );
                    }
                });
            });
        })
        .detach();
    }

    /// 仅在原活动 terminal tab 的关闭提交后交接；使用导航代次和原焦点句柄拒绝晚 ACK 抢占新目标。
    /// 有剩余 terminal pane 时保留 reducer 选中的 pane；若下一活动标签是文件/传输标签，复用统一激活入口切换面板。
    /// 工作区确实为空时只把快捷键焦点交还 Root，避免关闭 ACK 抢占稍后产生的其它焦点。
    ///
    /// @author kongweiguang
    fn complete_close_tab_focus(
        &mut self,
        handoff: Option<CloseTabFocusIntent>,
        cx: &mut Context<Self>,
    ) {
        let Some(handoff) = handoff else {
            return;
        };
        let window_handle = self.window_handle;
        cx.spawn(async move |this, cx| {
            // Dialog callbacks emit Close before GPUI releases the dialog focus scope. Defer once
            // so its normal dismissal can run; if a dialog is still active next frame, refuse the
            // handoff instead of retaining a retry task or taking focus from a newer modal.
            cx.background_executor()
                .timer(Duration::from_millis(16))
                .await;
            let _ = cx.update_window(window_handle, move |_, window, app| {
                if !window.is_window_active() || window.has_active_dialog(app) {
                    return;
                }
                let _ = this.update(app, |root, cx| {
                    let current_focus = window.focused(cx);
                    let root_modal_open = window.has_active_prompt()
                        || root.connection_prompt_modal_open
                        || root.split_target_picker.read(cx).is_open()
                        || root.preferences_modal_open
                        || root.connections_modal_open
                        || root.snippet_editor_modal_open
                        || root.root_confirmation_owner.is_some()
                        || root.pending_quit_intent.is_some()
                        || root.quit_ready_intent.is_some();
                    let focus_scope = keybinding_focus::classify_focus_scope(
                        &window.context_stack(),
                        root_modal_open,
                    );
                    let focus_still_owned = close_tab_focus_owner_is_current(
                        current_focus.as_ref(),
                        handoff.expected_focus.as_ref(),
                        handoff.closing_pane_focus.as_ref(),
                        handoff.closing_search_focus.as_ref(),
                        &root.focus_handle,
                        focus_scope,
                    );
                    if !window.is_window_active()
                        || root_modal_open
                        || root.host_shutdown_requested
                        || root.center_panel != CenterPanel::Terminal
                        || root.focused_agent_terminal
                        || logical_input_modal_blocked(
                            root.preferences_modal_open,
                            root.connections_modal_open,
                            root.snippet_editor_modal_open,
                            root.root_confirmation_owner.is_some(),
                            root.pending_quit_intent.is_some() || root.quit_ready_intent.is_some(),
                        )
                        || !close_tab_focus_is_current(
                            &handoff,
                            &handoff.tab_id,
                            root.surface_navigation_generation,
                            root.pane_focus_intent_generation,
                            focus_still_owned,
                        )
                        || root
                            .workspace_controller
                            .as_ref()
                            .is_some_and(|controller| {
                                controller.pending_close_batch_remaining_count().is_some()
                            })
                    {
                        return;
                    }

                    let Some((active_tab_id, next_pane, workspace_is_empty)) =
                        root.workspace_controller.as_ref().map(|controller| {
                            let snapshot = controller.snapshot();
                            let next_pane = snapshot.active_tab_id.as_ref().and_then(|tab_id| {
                                snapshot
                                    .terminal_tabs
                                    .iter()
                                    .find(|tab| tab.id() == tab_id)
                                    .and_then(WorkspaceSessionTab::terminal_layout)
                                    .and_then(|layout| {
                                        snapshot
                                            .focused_pane_id
                                            .as_ref()
                                            .filter(|pane_id| layout.contains_pane(pane_id))
                                            .cloned()
                                            .or_else(|| layout.pane_ids().into_iter().next())
                                    })
                            });
                            (
                                snapshot.active_tab_id.clone(),
                                next_pane,
                                snapshot.active_tab_id.is_none()
                                    && snapshot.terminal_tabs.is_empty(),
                            )
                        })
                    else {
                        return;
                    };
                    if active_tab_id.as_ref() == Some(&handoff.tab_id) {
                        return;
                    }
                    if let Some(tab_id) = active_tab_id {
                        if let Some(pane_id) = next_pane {
                            root.request_workspace_pane_focus(tab_id, pane_id, window, cx);
                        } else {
                            // 非终端活动标签没有 pane；统一入口负责切换 Editor/Files 并加载对应文档或传输视图。
                            root.activate_workspace_tab(&tab_id, window, cx);
                        }
                    } else if workspace_is_empty {
                        root.clear_pending_pane_focus();
                        root.set_reported_focus_session(None, cx);
                        window.focus(&root.focus_handle, cx);
                    }
                });
            });
        })
        .detach();
    }

    /// Root 内的快捷键和最后 pane 关闭路径已持有 Root 可变借用；同步捕获焦点后抑制子视图 hook，
    /// 避免确认请求回调重入 Root，同时保留 UI 菜单路径的同一关闭确认和请求代次语义。
    ///
    /// @author kongweiguang
    pub(super) fn request_workspace_tab_close_with_confirmation(
        &mut self,
        tab_id: TabId,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let focus = self.capture_close_tab_focus_for_tab(&tab_id, window, cx);
        if let Some(request_id) = self.workspace_tabs_view.update(cx, |tabs, tabs_cx| {
            tabs.request_close_tab_from_root_with_prepared_focus(tab_id.clone(), window, tabs_cx)
        }) {
            self.pending_close_tab_confirmation_focus = Some(
                PendingCloseTabConfirmationFocus::new(request_id, tab_id, focus),
            );
        }
    }

    /// 只为确认前仍活动且无真实 Dialog/Prompt 的 terminal tab 保存焦点来源；可变 Window 让真实 toolkit dialog 参与门禁。
    /// 精确属于待关闭 session 的搜索输入可携带自身句柄，ACK 仍须通过导航与焦点代次门禁。
    ///
    /// @author kongweiguang
    fn capture_close_tab_focus(
        &self,
        operation: &WorkspaceResourceOperation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<CloseTabFocusIntent> {
        let WorkspaceResourceOperation::CloseTab { tab_id, tab, .. } = operation else {
            return None;
        };
        if !matches!(tab.as_ref(), WorkspaceSessionTab::Terminal { .. }) {
            return None;
        }
        self.capture_close_tab_focus_for_tab(tab_id, window, cx)
    }

    /// Deferred CloseTab 尚未有 reducer operation 时，仍按同一窗口和当前 active-tab 证据捕获原始焦点代次；可变 Window 用于 dialog 门禁。
    ///
    /// @author kongweiguang
    pub(super) fn capture_close_tab_focus_for_tab(
        &self,
        tab_id: &TabId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<CloseTabFocusIntent> {
        if !window.is_window_active()
            || window.has_active_dialog(cx)
            || window.has_active_prompt()
            || self.host_shutdown_requested
            || self.center_panel != CenterPanel::Terminal
            || self.focused_agent_terminal
            || self.connection_prompt_modal_open
            || self.split_target_picker.read(cx).is_open()
            || logical_input_modal_blocked(
                self.preferences_modal_open,
                self.connections_modal_open,
                self.snippet_editor_modal_open,
                self.root_confirmation_owner.is_some(),
                self.pending_quit_intent.is_some() || self.quit_ready_intent.is_some(),
            )
        {
            return None;
        }
        let focus_scope = keybinding_focus::classify_focus_scope(&window.context_stack(), false);
        let expected_focus = window.focused(cx);
        let snapshot = self.workspace_controller.as_ref()?.snapshot();
        if snapshot.active_tab_id.as_ref() != Some(tab_id) {
            return None;
        }
        let closing_tab = snapshot
            .terminal_tabs
            .iter()
            .find(|candidate| candidate.id() == tab_id)?;
        if !matches!(closing_tab, WorkspaceSessionTab::Terminal { .. }) {
            return None;
        }
        let closing_search_focus = expected_focus.as_ref().and_then(|current_focus| {
            self.focused_terminal_search_focus_for_tab(tab_id, closing_tab, current_focus, cx)
        });
        if matches!(
            focus_scope,
            keybindings::KeybindingFocusScope::Editor | keybindings::KeybindingFocusScope::App
        ) && expected_focus.is_some()
            && closing_search_focus.is_none()
        {
            return None;
        }
        if closing_search_focus.is_some()
            && focus_scope != keybindings::KeybindingFocusScope::Editor
        {
            return None;
        }
        let closing_pane_focus = closing_tab
            .terminal_layout()
            .and_then(|layout| {
                snapshot
                    .focused_pane_id
                    .as_ref()
                    .filter(|pane_id| layout.contains_pane(pane_id))
            })
            .and_then(|pane_id| {
                let pane = self.workspace_runtime.pane(pane_id)?;
                if pane.tab_id != *tab_id {
                    return None;
                }
                let session_id = pane.session_id.as_ref()?;
                let registration = self.pane_input_handlers.get(session_id)?;
                (registration.binding.session_id == *session_id
                    && registration.binding.owner
                        == (SessionOwner::Ui {
                            tab_id: tab_id.clone(),
                            pane_id: pane_id.clone(),
                        }))
                .then(|| registration.entity.read(cx).focus_handle())
            });
        Some(CloseTabFocusIntent {
            tab_id: tab_id.clone(),
            navigation_generation: self.surface_navigation_generation,
            pane_focus_generation: self.pane_focus_intent_generation,
            expected_focus,
            closing_pane_focus,
            closing_search_focus,
        })
    }

    /// 只识别当前聚焦且属于待关闭 tab 的终端搜索框；退出态 binding 由 Root 保留供历史视图使用。
    /// 用查询 Entity 的确切 FocusHandle 做 ACK 比对，避免把其它 Editor 焦点当作关闭意图。
    ///
    /// @author kongweiguang
    fn focused_terminal_search_focus_for_tab(
        &self,
        tab_id: &TabId,
        closing_tab: &WorkspaceSessionTab,
        current_focus: &FocusHandle,
        cx: &Context<Self>,
    ) -> Option<FocusHandle> {
        let layout = closing_tab.terminal_layout()?;
        for pane_id in layout.pane_ids() {
            let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
                continue;
            };
            if pane.tab_id != *tab_id {
                continue;
            }
            let Some(session_id) = pane.session_id.as_ref() else {
                continue;
            };
            let Some(binding) = self.terminal_view_bindings.get(session_id) else {
                continue;
            };
            if binding.session_id != *session_id
                || binding.owner
                    != (SessionOwner::Ui {
                        tab_id: tab_id.clone(),
                        pane_id: pane_id.clone(),
                    })
            {
                continue;
            }
            let Some(tools) = self.workspace_tools.get(session_id) else {
                continue;
            };
            let Some(search) = tools.read(cx).search_overlay() else {
                continue;
            };
            let search_focus = search.read(cx).query_focus_handle(cx);
            if &search_focus == current_focus {
                return Some(search_focus);
            }
        }
        None
    }

    /// 有真实窗口的用户关闭才可携带焦点意图；可变 Window 供真实 dialog 门禁使用，后台 ACK 不重捕获。
    /// 后台 ACK 重入不凭当前选择重新捕获用户意图，仍走无焦点参数的资源入口。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_resource_operation_from_window(
        &mut self,
        operation: WorkspaceResourceOperation,
        keyboard_close_focus: Option<FocusHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.handle_workspace_resource_operation_from_window_with_tab_focus(
            operation,
            keyboard_close_focus,
            None,
            window,
            cx,
        );
    }

    /// 已确认弹层传入 `Some(ticket)` 时使用弹层前快照；可变 Window 供 dialog 门禁使用，`Some(None)` 禁止迟到补捕获。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_resource_operation_from_window_with_tab_focus(
        &mut self,
        operation: WorkspaceResourceOperation,
        keyboard_close_focus: Option<FocusHandle>,
        tab_close_focus: Option<Option<CloseTabFocusIntent>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let handoff = self.capture_close_survivor_focus(
            &operation,
            keyboard_close_focus.as_ref(),
            window,
            cx,
        );
        let tab_handoff =
            tab_close_focus.unwrap_or_else(|| self.capture_close_tab_focus(&operation, window, cx));
        self.handle_workspace_resource_operation_with_focus(operation, handoff, tab_handoff, cx);
    }

    /// 捕获/确认/取消都用稳定 TabId 和请求代次相关联；可变 Window 检查真实 dialog，Requested 在弹层打开前同步运行。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_tab_close_confirmation_focus(
        &mut self,
        phase: WorkspaceTabCloseConfirmationFocusPhase,
        target: WorkspaceTabCloseConfirmationFocusTarget,
        request_id: u64,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match phase {
            WorkspaceTabCloseConfirmationFocusPhase::Requested => match target {
                WorkspaceTabCloseConfirmationFocusTarget::Tab(tab_id) => {
                    let focus = self.capture_close_tab_focus_for_tab(&tab_id, window, cx);
                    self.pending_close_tab_confirmation_focus = Some(
                        PendingCloseTabConfirmationFocus::new(request_id, tab_id, focus),
                    );
                }
                WorkspaceTabCloseConfirmationFocusTarget::Batch(intent) => {
                    let (target_tab_id, focus) =
                        self.capture_close_batch_focus_for_intent(&intent, window, cx);
                    self.pending_close_batch_confirmation_focus =
                        Some(PendingCloseBatchConfirmationFocus::new(
                            request_id,
                            intent,
                            target_tab_id,
                            focus,
                        ));
                }
            },
            WorkspaceTabCloseConfirmationFocusPhase::Confirmed => match target {
                WorkspaceTabCloseConfirmationFocusTarget::Tab(tab_id) => {
                    if let Some(pending) = self.pending_close_tab_confirmation_focus.as_mut() {
                        let _ = pending.confirm(request_id, &tab_id);
                    }
                }
                WorkspaceTabCloseConfirmationFocusTarget::Batch(intent) => {
                    if let Some(pending) = self.pending_close_batch_confirmation_focus.as_mut() {
                        let _ = pending.confirm(request_id, &intent);
                    }
                }
            },
            WorkspaceTabCloseConfirmationFocusPhase::Cancelled => match target {
                WorkspaceTabCloseConfirmationFocusTarget::Tab(tab_id) => {
                    if self
                        .pending_close_tab_confirmation_focus
                        .as_ref()
                        .is_some_and(|pending| pending.cancel(request_id, &tab_id))
                    {
                        self.pending_close_tab_confirmation_focus = None;
                    }
                }
                WorkspaceTabCloseConfirmationFocusTarget::Batch(intent) => {
                    if self
                        .pending_close_batch_confirmation_focus
                        .as_ref()
                        .is_some_and(|pending| pending.cancel(request_id, &intent))
                    {
                        self.pending_close_batch_confirmation_focus = None;
                    }
                }
            },
        }
    }

    /// 批次只捕获仍活动且处于固定目标快照中的 terminal tab；可变 Window 用于真实 dialog 门禁。
    ///
    /// @author kongweiguang
    fn capture_close_batch_focus_for_intent(
        &self,
        intent: &WorkspaceCloseBatchIntent,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> (Option<TabId>, Option<CloseTabFocusIntent>) {
        let Some(snapshot) = self
            .workspace_controller
            .as_ref()
            .map(WorkspaceController::snapshot)
        else {
            return (None, None);
        };
        let Some(target_tab_id) = snapshot
            .active_tab_id
            .as_ref()
            .filter(|tab_id| intent.tab_ids().contains(tab_id))
            .filter(|tab_id| {
                snapshot.terminal_tabs.iter().any(|tab| {
                    tab.id() == *tab_id && matches!(tab, WorkspaceSessionTab::Terminal { .. })
                })
            })
            .cloned()
        else {
            return (None, None);
        };
        let focus = self.capture_close_tab_focus_for_tab(&target_tab_id, window, cx);
        (Some(target_tab_id), focus)
    }

    /// Confirmed Close consumes only its own pre-modal ticket; mismatch leaves newer requests untouched.
    ///
    /// @author kongweiguang
    fn take_confirmed_close_tab_confirmation_focus(
        &mut self,
        tab_id: &TabId,
    ) -> Option<Option<CloseTabFocusIntent>> {
        let pending = self.pending_close_tab_confirmation_focus.as_mut()?;
        let request_id = pending.request_id;
        let focus = pending.take_confirmed(request_id, tab_id)?;
        self.pending_close_tab_confirmation_focus = None;
        Some(focus)
    }

    /// CloseBatch 仅消费与原始固定目标集匹配的一次确认，不让迟到弹层读取当前活动标签。
    ///
    /// @author kongweiguang
    fn take_confirmed_close_batch_confirmation_focus(
        &mut self,
        intent: &WorkspaceCloseBatchIntent,
    ) -> Option<(Option<TabId>, Option<CloseTabFocusIntent>)> {
        let pending = self.pending_close_batch_confirmation_focus.as_mut()?;
        let request_id = pending.request_id;
        let focus = pending.take_confirmed(request_id, intent)?;
        self.pending_close_batch_confirmation_focus = None;
        Some(focus)
    }

    /// begin close 后撤销 live 输入；只保留自然退出时冻结的 exact view binding，供失败 ACK 恢复只读历史。
    /// 不向即将退役的会话补发 blur，也不释放 session cleanup owner。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_workspace_session_input_for_close(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<RootView>,
    ) -> QueueDropSummary {
        if self.reported_focus_session.as_ref() == Some(session_id) {
            self.reported_focus_session = None;
        }
        let retired_history_binding = self
            .workspace_runtime
            .pane_for_session(session_id)
            .and_then(|pane_id| {
                self.terminal_view_bindings
                    .get(session_id)
                    .filter(|binding| {
                        self.workspace_runtime
                            .retired_history_binding_matches(pane_id, binding)
                    })
                    .cloned()
            });
        self.invalidate_pane_input_handler(session_id, cx);
        if let Some(binding) = retired_history_binding {
            // Preserve only the exact naturally retired view ticket; runtime phase gates it until close ACK.
            self.terminal_view_bindings
                .insert(session_id.clone(), binding);
        }
        self.session_commands.cancel_pending(session_id)
    }

    /// Root 单独接管共享工作台临时 Tab，持久标签与资源 ACK 仍由 controller 更新和关闭。
    /// 返回值只表示 Activate 已接受：持久标签须获 reducer 的 Changed/Unchanged，临时工作台须仍有效；其它路径一律拒绝 Root 导航。
    /// CloseTab 只在窗口入口捕获原活动 tab 的焦点代次，后台 ACK 不按届时的 active tab 重建意图。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_tab_event(
        &mut self,
        event: &WorkspaceTabEvent,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        let confirmed_batch_focus = match event {
            WorkspaceTabEvent::CloseBatch(intent) => {
                self.take_confirmed_close_batch_confirmation_focus(intent)
            }
            _ => None,
        };
        let batch_was_pending_at_event_start = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_close_batch_remaining_count())
            .is_some();
        let workbench_tab_id = self.sftp_workbench_tab_id.clone();
        let targets_workbench = workspace_tab_event_targets(event, &workbench_tab_id);
        if targets_workbench && !self.sftp_workbench_tab_open {
            self.status = Some(crate::locale::text(
                "SFTP 工作台标签已关闭，此操作已忽略。",
                "The SFTP workbench tab is closed; this action was ignored.",
            ));
            cx.notify();
            return false;
        }
        if targets_workbench {
            match event {
                WorkspaceTabEvent::Activate(_) => {
                    return self.open_shared_sftp_workbench(window, cx);
                }
                WorkspaceTabEvent::Close(_) => {
                    self.close_shared_sftp_workbench(window, cx);
                    return false;
                }
                WorkspaceTabEvent::CloseBatch(_) => {}
                _ => {
                    self.status = Some(crate::locale::text(
                        "SFTP 工作台只支持激活或关闭，不能重命名、分组或拖动。",
                        "The SFTP workbench can only be activated or closed; it cannot be renamed, grouped, or dragged.",
                    ));
                    cx.notify();
                    return false;
                }
            }
        }
        if let WorkspaceTabEvent::FileAction { tab_id, action } = event {
            self.handle_workspace_file_tab_action(tab_id, *action, window, cx);
            return false;
        }
        let mut effective_event = event.clone();
        let mut close_workbench_after_batch = false;
        let mut close_after_batch_targets = None;
        if let WorkspaceTabEvent::CloseBatch(intent) = event
            && targets_workbench
        {
            let Some((includes_workbench, filtered_intent)) =
                partition_shared_sftp_workbench_close_batch(intent, &workbench_tab_id)
            else {
                self.status = Some(crate::locale::text(
                    "SFTP 工作台不能作为批量关闭的保留锚点或分组成员。",
                    "The SFTP workbench cannot be a batch-close anchor or group member.",
                ));
                cx.notify();
                return false;
            };
            close_workbench_after_batch = includes_workbench;
            if filtered_intent.tab_ids().is_empty() {
                self.close_shared_sftp_workbench(window, cx);
                return false;
            }
            close_after_batch_targets = Some(filtered_intent.tab_ids().to_vec());
            effective_event = WorkspaceTabEvent::CloseBatch(filtered_intent);
        } else if matches!(event, WorkspaceTabEvent::CloseBatch(_)) {
            self.sftp_workbench_close_after_batch = None;
        }
        let mut close_batch_focus_override = match (&effective_event, confirmed_batch_focus) {
            (WorkspaceTabEvent::CloseBatch(intent), Some((target_tab_id, focus))) => Some(
                PendingCloseBatchFocus::new(intent.clone(), target_tab_id, focus),
            ),
            _ => None,
        };
        let tab_close_focus_override = match &effective_event {
            WorkspaceTabEvent::Close(tab_id) => {
                self.take_confirmed_close_tab_confirmation_focus(tab_id)
            }
            _ => None,
        };
        let deferred_intent = match event {
            WorkspaceTabEvent::Close(tab_id) => Some(WorkspaceDeferredCloseIntent::Tab {
                tab_id: tab_id.clone(),
            }),
            WorkspaceTabEvent::CloseBatch(intent) => {
                Some(WorkspaceDeferredCloseIntent::Batch(intent.clone()))
            }
            _ => None,
        };
        if let Some(intent) = deferred_intent
            && self.defer_workspace_split_close_intent(
                intent,
                true,
                None,
                tab_close_focus_override.clone(),
                window,
                cx,
            )
        {
            if let Some(ticket) = close_batch_focus_override.take() {
                self.retain_close_batch_focus(ticket, !batch_was_pending_at_event_start);
            }
            return false;
        }
        if close_workbench_after_batch {
            self.pending_sftp_navigation = None;
            self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        }
        if let WorkspaceTabEvent::CloseBatch(intent) = &effective_event {
            let retry = self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.retry_pending_close_batch(intent));
            if let Some(operation) = retry {
                if close_workbench_after_batch {
                    self.sftp_workbench_close_after_batch = close_after_batch_targets.clone();
                }
                self.handle_workspace_resource_operation(operation, cx);
                return false;
            }
        }
        if let WorkspaceTabEvent::Close(tab_id) = &effective_event {
            let retry = self
                .workspace_controller
                .as_ref()
                .and_then(|controller| controller.retry_pending_resource_operation())
                .filter(|operation| match operation {
                    WorkspaceResourceOperation::CloseTab {
                        tab_id: pending_tab,
                        ..
                    }
                    | WorkspaceResourceOperation::ClosePane {
                        tab_id: pending_tab,
                        ..
                    } => pending_tab == tab_id,
                    WorkspaceResourceOperation::CreatePane { .. } => false,
                });
            if let Some(operation) = retry {
                self.handle_workspace_resource_operation_from_window_with_tab_focus(
                    operation,
                    None,
                    tab_close_focus_override.clone(),
                    window,
                    cx,
                );
                return false;
            }
        }
        let refresh_commands_presentation =
            matches!(&effective_event, WorkspaceTabEvent::RenameTab { .. });
        let capture_tab_close_focus = matches!(&effective_event, WorkspaceTabEvent::Close(_));
        let result = self
            .workspace_controller
            .as_mut()
            .map(|controller| controller.apply_tab_event(effective_event));
        let activation_accepted = workspace_tab_activation_accepted(event, result.as_ref());
        if activation_accepted {
            // 只有持久标签切换已提交后，才撤销旧关闭批次留下的临时视图意图。
            self.sftp_workbench_close_after_batch = None;
        }
        match result {
            Some(Ok(WorkspaceControllerEffect::Unchanged)) => {}
            Some(Ok(WorkspaceControllerEffect::Changed { .. })) => {
                self.refresh_workspace_views(cx);
                if refresh_commands_presentation {
                    self.refresh_commands_target_presentation(cx);
                }
                self.persist_workspace_snapshot(cx);
            }
            Some(Ok(WorkspaceControllerEffect::ResourceOperation(operation))) => {
                if close_workbench_after_batch {
                    self.sftp_workbench_close_after_batch = close_after_batch_targets;
                }
                if let Some(ticket) = close_batch_focus_override.take() {
                    self.retain_close_batch_focus(ticket, !batch_was_pending_at_event_start);
                }
                if capture_tab_close_focus {
                    self.handle_workspace_resource_operation_from_window_with_tab_focus(
                        operation,
                        None,
                        tab_close_focus_override,
                        window,
                        cx,
                    );
                } else {
                    self.handle_workspace_resource_operation(operation, cx);
                }
            }
            Some(Err(error)) => {
                self.status = Some(error.public_message().to_owned().into());
                cx.notify();
            }
            None => {
                self.status = Some(crate::locale::text(
                    "工作区尚未就绪，无法更改标签。",
                    "The workspace is not ready to change tabs.",
                ));
                cx.notify();
            }
        }
        activation_accepted
    }

    /// 关闭只移除共享工作台视图并推进导航代次；当前传输仍由 FilesPane 持有，不触碰运行任务。
    ///
    /// @author kongweiguang
    pub(super) fn close_shared_sftp_workbench(
        &mut self,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        if !self.sftp_workbench_tab_open {
            return;
        }
        let was_selected = self.shared_sftp_workbench_is_selected();
        self.sftp_workbench_tab_open = false;
        self.sftp_workbench_close_after_batch = None;
        self.pending_sftp_navigation = None;
        self.surface_navigation_generation = self.surface_navigation_generation.wrapping_add(1);
        if was_selected {
            let fallback_tab_id = self.workspace_controller.as_ref().and_then(|controller| {
                let snapshot = controller.snapshot();
                let active_tab_id = snapshot.active_tab_id.as_ref()?;
                snapshot
                    .terminal_tabs
                    .iter()
                    .any(|tab| tab.id() == active_tab_id)
                    .then(|| active_tab_id.clone())
            });
            if let Some(tab_id) = fallback_tab_id {
                self.activate_workspace_tab(&tab_id, window, cx);
            } else {
                self.set_center_panel(CenterPanel::Terminal, window, cx);
            }
        }
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 批量关闭的临时标签延后至全部持久目标提交成功，避免失败回执顺带丢掉当前工作台。
    ///
    /// @author kongweiguang
    fn close_shared_sftp_workbench_after_successful_batch(&mut self, cx: &mut Context<RootView>) {
        let Some(targets) = self.sftp_workbench_close_after_batch.as_ref() else {
            return;
        };
        let Some(controller) = self.workspace_controller.as_ref() else {
            return;
        };
        let snapshot = controller.snapshot();
        let targets_remain = targets
            .iter()
            .any(|target| snapshot.terminal_tabs.iter().any(|tab| tab.id() == target));
        if controller.pending_close_batch_remaining_count().is_some() || targets_remain {
            return;
        }
        self.sftp_workbench_close_after_batch = None;
        if !self.sftp_workbench_tab_open {
            return;
        }
        let workbench_tab_id = self.sftp_workbench_tab_id.clone();
        self.workspace_tabs_view.update(cx, |_, cx| {
            cx.emit(WorkspaceTabEvent::Close(workbench_tab_id));
            cx.notify();
        });
    }

    /// 分屏创建走唯一 open ACK，关闭则等待文件门禁或共享 deadline 下的资源回执后提交；controller 是标签状态唯一来源。
    /// Orphan 回执独立于布局操作代次，并与清理面板共用唯一资源退役入口。
    ///
    /// @author kongweiguang
    pub(super) fn handle_workspace_resource_operation(
        &mut self,
        operation: WorkspaceResourceOperation,
        cx: &mut Context<RootView>,
    ) {
        self.handle_workspace_resource_operation_with_focus(operation, None, None, cx);
    }

    /// 两类焦点票据随同一次资源任务持有；等待恢复 ACK 时借用同一 entry，只有原 operation 成功提交才可交接。
    /// 失败或未知清理继续保留原 owner，旧回执不能从当前选择重新取得焦点意图。
    ///
    /// @author kongweiguang
    fn handle_workspace_resource_operation_with_focus(
        &mut self,
        operation: WorkspaceResourceOperation,
        close_focus: Option<CloseSurvivorFocusIntent>,
        close_tab_focus: Option<CloseTabFocusIntent>,
        cx: &mut Context<RootView>,
    ) {
        if let WorkspaceResourceOperation::CloseTab {
            operation_id, tab, ..
        } = &operation
            && !matches!(tab.as_ref(), WorkspaceSessionTab::Terminal { .. })
        {
            self.begin_workspace_file_tab_close(operation_id.clone(), tab.as_ref().clone(), cx);
            return;
        }
        let (operation_id, tab_id, pane_ids, remove_tab_layout_on_success) = match operation {
            WorkspaceResourceOperation::ClosePane {
                operation_id,
                tab_id,
                pane_id,
            } => (operation_id, tab_id, vec![pane_id], false),
            WorkspaceResourceOperation::CloseTab {
                operation_id,
                tab_id,
                pane_ids,
                ..
            } => (operation_id, tab_id, pane_ids, true),
            create @ WorkspaceResourceOperation::CreatePane { .. } => {
                self.begin_workspace_split(create, cx);
                return;
            }
        };

        let restore_pending = pane_ids
            .iter()
            .filter(|pane_id| {
                self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
                    restore_close_action(
                        pane.phase,
                        pane.session_id.is_some(),
                        self.has_recorded_workspace_restore_admission(pane_id),
                    ) == RestoreCloseAction::WaitForAcknowledgement
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        if !restore_pending.is_empty() {
            // 只有 ClosePane 可携带 survivor 交接；沿既有 entry 保留来源，后续恢复 ACK 不再读取当前焦点。
            if pane_ids.len() == 1 && close_focus.is_some() {
                self.workspace_resource_operations
                    .entry(pane_ids[0].clone())
                    .or_insert_with(|| WorkspaceResourceOperationEntry::new(operation_id.clone()))
                    .retain_restore_close_focus(&operation_id, close_focus);
            }
            if remove_tab_layout_on_success
                && close_tab_focus.is_some()
                && let Some(pane_id) = pane_ids.first()
            {
                self.workspace_resource_operations
                    .entry(pane_id.clone())
                    .or_insert_with(|| WorkspaceResourceOperationEntry::new(operation_id.clone()))
                    .retain_restore_close_tab_focus(&operation_id, close_tab_focus);
            }
            for pane_id in &restore_pending {
                self.cancel_workspace_restore(pane_id);
            }
            self.status = Some(crate::locale::text(
                "正在取消终端启动；收到资源回执后继续关闭。",
                "Terminal startup is being cancelled. Close will continue after its resource receipt arrives.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return;
        }

        // 恢复 ACK 或用户重试只消费同一 Close owner 的原始来源；不删除其它代次/仍属 split 的 entry。
        let retained_focus = pane_ids.first().and_then(|pane_id| {
            if remove_tab_layout_on_success {
                None
            } else {
                take_restore_close_focus(
                    &mut self.workspace_resource_operations,
                    pane_id,
                    &operation_id,
                )
            }
        });
        let close_focus = retained_focus.or(close_focus);
        let retained_tab_focus = pane_ids.first().and_then(|pane_id| {
            if remove_tab_layout_on_success {
                take_restore_close_tab_focus(
                    &mut self.workspace_resource_operations,
                    pane_id,
                    &operation_id,
                )
            } else {
                None
            }
        });
        let close_tab_focus = retained_tab_focus.or(close_tab_focus);

        let mut sessions_to_close = Vec::new();
        for pane_id in &pane_ids {
            let Some((phase, session_id, owner)) =
                self.workspace_runtime.pane(pane_id).map(|pane| {
                    (
                        pane.phase,
                        pane.session_id.clone(),
                        CleanupOwnerSnapshot {
                            tab_id: pane.tab_id.clone(),
                            pane_id: pane_id.clone(),
                            target: pane.target.clone(),
                            size: pane.size,
                            snapshot: pane.snapshot.clone(),
                            external_lease: pane.external_lease.clone(),
                            transient: pane.transient,
                        },
                    )
                })
            else {
                continue;
            };
            match restore_close_action(
                phase,
                session_id.is_some(),
                self.has_recorded_workspace_restore_admission(pane_id),
            ) {
                RestoreCloseAction::RemoveEmptyPane => {
                    // Invalidate prepaint callbacks before any sibling session close can yield.
                    self.cancel_workspace_restore(pane_id);
                    if phase == RuntimePanePhase::RestorePending {
                        let cancelled = match self.workspace_controller.as_mut() {
                            Some(controller) => cancel_unadmitted_restore_for_close(
                                &mut self.workspace_runtime,
                                controller,
                                pane_id,
                            ),
                            None => false,
                        };
                        if !cancelled {
                            self.complete_workspace_resource_failure(
                                &operation_id,
                                crate::locale::text(
                                    "未启动 pane 的取消状态无法核对，关闭未提交。",
                                    "The unstarted pane cancellation could not be verified; close was not committed.",
                                ),
                                cx,
                            );
                            return;
                        }
                    }
                    continue;
                }
                RestoreCloseAction::WaitForAcknowledgement => {
                    self.cancel_workspace_restore(pane_id);
                    self.status = Some(crate::locale::text(
                        "正在取消终端启动；收到资源回执后继续关闭。",
                        "Terminal startup is being cancelled. Close will continue after its resource receipt arrives.",
                    ));
                    cx.notify();
                    return;
                }
                RestoreCloseAction::Reject => {
                    self.complete_workspace_resource_failure(
                        &operation_id,
                        crate::locale::text(
                            "终端正在执行其它生命周期操作，关闭未提交。",
                            "A terminal is already in another lifecycle operation; close was not committed.",
                        ),
                        cx,
                    );
                    return;
                }
                RestoreCloseAction::CloseSession => {}
            }
            let Some(session_id) = session_id else {
                continue;
            };
            match self.workspace_runtime.begin_close(pane_id) {
                Ok(current) if current == session_id => {
                    sessions_to_close.push(SessionCloseRequest {
                        resource: WorkspaceCloseResource::Pane(pane_id.clone()),
                        session_id,
                        owner,
                    });
                }
                _ => {
                    let mut rollback_failed = false;
                    for started in &sessions_to_close {
                        if let WorkspaceCloseResource::Pane(started_pane) = &started.resource
                            && !matches!(
                                self.workspace_runtime
                                    .mark_close_failed(started_pane, &started.session_id),
                                Ok(true)
                            )
                        {
                            rollback_failed = true;
                        }
                    }
                    self.complete_workspace_resource_failure(
                        &operation_id,
                        if rollback_failed {
                            crate::locale::text(
                                "终端正在执行其它生命周期操作，关闭未提交且部分 pane 回滚状态失败。",
                                "A terminal is in another lifecycle operation; close was not committed and some pane rollback state failed.",
                            )
                        } else {
                            crate::locale::text(
                                "终端正在执行其它生命周期操作，关闭未提交。",
                                "A terminal is already in another lifecycle operation; close was not committed.",
                            )
                        },
                        cx,
                    );
                    return;
                }
            }
        }

        let orphan_ids = if remove_tab_layout_on_success {
            self.workspace_runtime
                .orphan_cleanup_for_tab(&tab_id)
                .map(|(session_id, _)| session_id.clone())
                .collect::<Vec<_>>()
        } else {
            pane_ids
                .iter()
                .flat_map(|pane_id| {
                    self.workspace_runtime
                        .orphan_cleanup_for_pane(pane_id)
                        .map(|(session_id, _)| session_id.clone())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        for session_id in orphan_ids {
            if !sessions_to_close
                .iter()
                .any(|request: &SessionCloseRequest| request.session_id == session_id)
            {
                let Some(orphan) = self.workspace_runtime.orphan_cleanup(&session_id) else {
                    continue;
                };
                let owner = CleanupOwnerSnapshot {
                    tab_id: orphan.tab_id.clone(),
                    pane_id: orphan.pane_id.clone(),
                    target: Some(orphan.target.clone()),
                    size: orphan.size,
                    snapshot: orphan.snapshot.clone(),
                    external_lease: orphan.external_lease.clone(),
                    transient: orphan.transient,
                };
                let generation = match self.workspace_runtime.begin_orphan_cleanup(
                    &session_id,
                    crate::locale::text(
                        "正在关闭脱离工作区布局的终端资源。",
                        "Closing a terminal resource detached from its workspace layout.",
                    ),
                ) {
                    Ok(generation) => generation,
                    // 全局清理面板或另一项关闭已持有此 SessionId，避免重复 close。
                    Err(_) => continue,
                };
                sessions_to_close.push(SessionCloseRequest {
                    resource: WorkspaceCloseResource::Orphan { generation },
                    session_id,
                    owner,
                });
            }
        }

        let mut cancelled_work = QueueDropSummary::default();
        for request in &sessions_to_close {
            cancelled_work
                .merge(self.cancel_workspace_session_input_for_close(&request.session_id, cx));
            if let Some(watcher) = self.session_watch_cancellations.remove(&request.session_id) {
                watcher.cancel();
            }
            self.terminal_tools_runtime
                .cancel_session(&request.session_id);
        }
        self.refresh_workspace_orphan_cleanup(cx);

        if sessions_to_close.is_empty() {
            for pane_id in &pane_ids {
                if self
                    .workspace_runtime
                    .pane(pane_id)
                    .is_some_and(|pane| pane.session_id.is_none())
                    && self.workspace_runtime.remove_empty_pane(pane_id).is_err()
                {
                    self.complete_workspace_resource_failure(
                        &operation_id,
                        crate::locale::text(
                            "空 pane 仍由异步资源操作占用，关闭未提交。",
                            "An empty pane is still owned by an asynchronous resource operation; close was not committed.",
                        ),
                        cx,
                    );
                    return;
                }
            }
            let result = self
                .workspace_controller
                .as_mut()
                .map(|controller| controller.complete_resource_operation(&operation_id, Ok(())));
            match result {
                Some(Ok(_)) => {
                    self.update_runtime_layout_after_close(&tab_id, remove_tab_layout_on_success);
                    if remove_tab_layout_on_success {
                        self.note_close_batch_tab_success(&tab_id);
                    }
                    self.refresh_workspace_views(cx);
                    self.refresh_workspace_orphan_cleanup(cx);
                    self.complete_close_survivor_focus(close_focus, cx);
                    self.complete_close_tab_focus(close_tab_focus, cx);
                    self.persist_workspace_snapshot(cx);
                    self.advance_workspace_tab_close_batch(cx);
                }
                Some(Err(error)) => self.status = Some(error.public_message().to_owned().into()),
                None => {}
            }
            cx.notify();
            return;
        }

        let services = self.services.clone();
        let executor = services.executor.clone();
        let session_service = services.sessions.clone();
        let close_deadline = SessionDeadline::now() + DEFAULT_SESSION_CLOSE_BUDGET;
        let completion_targets = sessions_to_close.clone();
        let operation_id_for_task = operation_id.clone();
        let cancelled_work_for_task = cancelled_work;
        cx.spawn(async move |this, cx| {
            let close_results = executor
                .run(move |_cancellation| async move {
                    Ok(close_sessions_until(
                        sessions_to_close,
                        close_deadline,
                        move |session_id, deadline| {
                            let sessions = session_service.clone();
                            async move {
                                sessions
                                    .close_until(session_id, deadline)
                                    .await
                                    .map_err(|error| error.public_message().to_owned())
                            }
                        },
                    )
                    .await)
                })
                .await;
            let close_results = match close_results {
                Ok(results) => results,
                Err(error) => completion_targets
                    .into_iter()
                    .map(|request| {
                        (request, Err(error.public_message().to_owned()))
                    })
                    .collect(),
            };
            let _ = this.update(cx, |root, cx| {
                let current_operation = root
                    .workspace_controller
                    .as_ref()
                    .and_then(|controller| controller.pending_resource_operation())
                    .is_some_and(|pending| pending.operation_id() == operation_id_for_task);
                if !current_operation {
                    // Controller 操作可能已换代，但孤立资源 close ACK 仍须按 owner generation 收敛。
                    for (request, result) in close_results {
                        let WorkspaceCloseResource::Orphan { generation } = request.resource
                        else {
                            continue;
                        };
                        root.apply_workspace_orphan_cleanup_result(
                            &request.session_id,
                            generation,
                            result,
                            cx,
                        );
                    }
                    root.refresh_workspace_orphan_cleanup(cx);
                    cx.notify();
                    return;
                }
                let mut failures: Vec<String> = Vec::new();
                for (request, result) in close_results {
                    let session_id = request.session_id;
                    match result {
                        Ok(()) => {
                            let removed = match request.resource {
                                WorkspaceCloseResource::Pane(pane_id) => {
                                    let removed = matches!(
                                        root.workspace_runtime
                                            .remove_closed_session(&pane_id, &session_id),
                                        Ok(true)
                                    );
                                    if removed {
                                        root.mark_container_exec_target_dirty(&pane_id, cx);
                                    }
                                    removed
                                }
                                WorkspaceCloseResource::Orphan { generation } => {
                                    match root.apply_workspace_orphan_cleanup_result(
                                        &session_id,
                                        generation,
                                        Ok(()),
                                        cx,
                                    ) {
                                        OrphanCleanupCompletion::Removed => continue,
                                        OrphanCleanupCompletion::Retryable
                                        | OrphanCleanupCompletion::Stale => false,
                                    }
                                }
                            };
                            if removed {
                                if let Some(tools) = root.workspace_tools.remove(&session_id) {
                                    root.terminal_tools_runtime.cancel_source(tools.entity_id());
                                }
                                root.workspace_tool_subscriptions.remove(&session_id);
                            } else {
                                failures.push(crate::locale::text(
                                    "终端已关闭，但 pane 运行态无法清理。",
                                    "The terminal closed, but its pane runtime could not be cleared.",
                                ).to_string());
                            }
                        }
                        Err(message) => {
                            let visible = SharedString::from(message);
                            match request.resource {
                                WorkspaceCloseResource::Pane(pane_id) => {
                                    let pane_retains_session = root
                                        .workspace_runtime
                                        .pane(&pane_id)
                                        .is_some_and(|pane| {
                                            pane.session_id.as_ref() == Some(&session_id)
                                        });
                                    if pane_retains_session {
                                        let marked = root
                                            .workspace_runtime
                                            .mark_close_failed(&pane_id, &session_id);
                                        let status_set = root
                                            .workspace_runtime
                                            .set_status(&pane_id, Some(visible.clone()));
                                        if !matches!(marked, Ok(true)) || status_set.is_err() {
                                            root.status = Some(crate::locale::text(
                                                "终端关闭失败，原 pane 仍保留会话，但重试状态更新未完成。",
                                                "Terminal close failed and the original pane retains its session, but retry state could not be fully updated.",
                                            ));
                                        }
                                    } else if let Some(target) = request.owner.target.clone() {
                                        let retained = root.workspace_runtime.retain_orphan_cleanup(
                                            OrphanCleanupInput {
                                                tab_id: request.owner.tab_id.clone(),
                                                pane_id: request.owner.pane_id.clone(),
                                                session_id: session_id.clone(),
                                                target,
                                                size: request.owner.size,
                                                snapshot: request.owner.snapshot.clone(),
                                                status: visible.clone(),
                                                external_lease: request.owner.external_lease.clone(),
                                                transient: request.owner.transient,
                                            },
                                        );
                                        let cleanup_owner_still_exists = root
                                            .workspace_runtime
                                            .orphan_cleanup(&session_id)
                                            .is_some()
                                            || root.workspace_runtime.panes().any(
                                                |(_, pane)| {
                                                    pane.session_id.as_ref() == Some(&session_id)
                                                },
                                            );
                                        if retained.is_err() && !cleanup_owner_still_exists {
                                            failures.push(crate::locale::text(
                                                "终端关闭失败且原资源 owner 无法恢复；关闭操作未提交。",
                                                "Terminal close failed and its original resource owner could not be restored; the workspace close was not committed.",
                                            ).to_string());
                                        }
                                    } else {
                                        failures.push(crate::locale::text(
                                            "终端关闭失败，原 pane 缺少可恢复的目标信息。",
                                            "Terminal close failed and its pane has no target metadata for cleanup recovery.",
                                        ).to_string());
                                    }
                                }
                                WorkspaceCloseResource::Orphan { generation } => {
                                    if root.apply_workspace_orphan_cleanup_result(
                                        &session_id,
                                        generation,
                                        Err(visible.to_string()),
                                        cx,
                                    ) == OrphanCleanupCompletion::Stale
                                    {
                                        failures.push(crate::locale::text(
                                            "终端关闭失败，原清理 owner 已变化；资源仍保留待核对。",
                                            "Terminal close failed after its cleanup owner changed; the resource remains pending review.",
                                        ).to_string());
                                    }
                                }
                            }
                            failures.push(visible.to_string());
                        }
                    }
                }
                if !failures.is_empty() {
                    let operation_result = root
                        .workspace_controller
                        .as_mut()
                        .map(|controller| {
                            controller.complete_resource_operation(
                                &operation_id_for_task,
                                Err(WorkspaceRestoreFailure::ResourceFailure),
                            )
                        });
                    if let Some(Err(error)) = operation_result {
                        failures.push(error.public_message().to_owned());
                    }
                    let mut message = failures.join(" · ");
                    if cancelled_work_for_task.commands > 0 {
                        message.push_str(&format!(
                            "；关闭期间取消了 {} 项未发送工作（输入 {} 条，{} 字节）。",
                            cancelled_work_for_task.commands,
                            cancelled_work_for_task.input_events,
                            cancelled_work_for_task.input_bytes,
                        ));
                    }
                    if let Some(remaining) = root
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| controller.pending_close_batch_remaining_count())
                    {
                        message.push_str(&crate::locale::text(
                            format!(
                                " 批量关闭已停止，仍有 {remaining} 个目标保留；再次执行同一关闭操作可重试剩余目标。"
                            ),
                            format!(
                                " Batch close stopped with {remaining} targets retained; run the same close action again to retry the remaining targets."
                            ),
                        ));
                    }
                    root.status = Some(SharedString::from(message));
                    root.refresh_workspace_views(cx);
                    root.refresh_workspace_orphan_cleanup(cx);
                    cx.notify();
                    return;
                }

                let mut removal_error = None;
                for pane_id in &pane_ids {
                    if root.workspace_runtime.pane(pane_id).is_some_and(|pane| pane.session_id.is_none()) {
                        root.cancel_workspace_restore(pane_id);
                        if root.workspace_runtime.remove_empty_pane(pane_id).is_err() {
                            removal_error = Some(crate::locale::text(
                                "空 pane 无法从运行态清理。",
                                "An empty pane could not be removed from runtime state.",
                            ));
                            break;
                        }
                    }
                }
                if let Some(error) = removal_error {
                    root.complete_workspace_resource_failure(&operation_id_for_task, error, cx);
                    return;
                }

                let result = root
                    .workspace_controller
                    .as_mut()
                    .map(|controller| {
                        controller.complete_resource_operation(&operation_id_for_task, Ok(()))
                    });
                match result {
                    Some(Ok(_)) => {
                        root.update_runtime_layout_after_close(
                            &tab_id,
                            remove_tab_layout_on_success,
                        );
                        if remove_tab_layout_on_success {
                            root.note_close_batch_tab_success(&tab_id);
                        }
                        root.status = (cancelled_work_for_task.commands > 0).then(|| {
                            SharedString::from(format!(
                                "终端已关闭；取消了 {} 项未发送工作（输入 {} 条，{} 字节）。",
                                cancelled_work_for_task.commands,
                                cancelled_work_for_task.input_events,
                                cancelled_work_for_task.input_bytes,
                            ))
                        });
                        root.refresh_workspace_views(cx);
                        root.refresh_workspace_orphan_cleanup(cx);
                        root.complete_close_survivor_focus(close_focus, cx);
                        root.complete_close_tab_focus(close_tab_focus, cx);
                        root.persist_workspace_snapshot(cx);
                        root.advance_workspace_tab_close_batch(cx);
                    }
                    Some(Err(error)) => {
                        root.status = Some(error.public_message().to_owned().into());
                        root.refresh_workspace_views(cx);
                    }
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 文件标签先通过共享编辑器脏状态门禁；Entity 更新无同步错误结果，关闭决定由事件回报。
    ///
    /// @author kongweiguang
    fn begin_workspace_file_tab_close(
        &mut self,
        operation_id: String,
        tab: WorkspaceSessionTab,
        cx: &mut Context<RootView>,
    ) {
        let tab_id = tab.id().clone();
        let endpoint = self.workspace_files.file_endpoint(&tab_id);
        let needs_editor_guard = matches!(&tab, WorkspaceSessionTab::WorkspaceFile { .. })
            && endpoint.as_ref().is_some_and(|endpoint| {
                !self
                    .workspace_files
                    .has_other_workspace_file_endpoint(&tab_id, endpoint)
            });
        if needs_editor_guard
            && let (Some(endpoint), Some(editor)) = (endpoint, self.file_editor_workspace.clone())
        {
            editor.update(cx, |workspace, cx| {
                workspace.request_workspace_close(endpoint.clone(), operation_id.clone(), cx);
            });
            return;
        }
        self.complete_nonterminal_workspace_tab_close(operation_id, tab_id, cx);
    }

    /// 编辑器只回报具体文档的用户决定；Root 校验仍属当前关闭操作后才提交 workspace。
    pub(super) fn handle_workspace_file_close_result(
        &mut self,
        operation_id: String,
        endpoint: FileEndpoint,
        closed: bool,
        cx: &mut Context<RootView>,
    ) {
        let pending = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .cloned();
        let Some(WorkspaceResourceOperation::CloseTab {
            operation_id: pending_id,
            tab,
            ..
        }) = pending
        else {
            return;
        };
        if pending_id != operation_id
            || !matches!(tab.as_ref(), WorkspaceSessionTab::WorkspaceFile { .. })
            || self.workspace_files.file_endpoint(tab.id()).as_ref() != Some(&endpoint)
        {
            return;
        }
        if !closed {
            self.complete_workspace_resource_failure(
                &operation_id,
                crate::locale::text(
                    "文件标签关闭已取消或未完成；工作区仍保留原标签。",
                    "File tab close was cancelled or did not complete; the workspace tab was kept.",
                ),
                cx,
            );
            return;
        }
        self.complete_nonterminal_workspace_tab_close(operation_id, tab.id().clone(), cx);
    }

    /// generation 在取消后仍存在，是 RestorePending 等待旧 ACK 的保守证据；裸占位只有两份记录都缺席才未准入。
    ///
    /// @author kongweiguang
    fn has_recorded_workspace_restore_admission(&self, pane_id: &PaneId) -> bool {
        self.workspace_restore_generations.contains_key(pane_id)
            || self.workspace_restore_cancellations.contains_key(pane_id)
    }

    /// 只撤销尚未准入真实 restore 的候选分屏；精确身份、无会话 owner 和无孤儿资源是安全前提。
    ///
    /// @author kongweiguang
    pub(super) fn cancel_unadmitted_workspace_split(
        &mut self,
        preview: &SplitCandidatePreview,
        cx: &mut Context<RootView>,
    ) -> bool {
        let preview_is_current = self
            .pending_workspace_split_preview(cx)
            .is_some_and(|(current, _, _)| current == *preview);
        if !preview_is_current {
            return false;
        }
        let Some(owner) = self.workspace_resource_operations.get(&preview.pane_id) else {
            return false;
        };
        if owner.operation_id() != preview.operation_id {
            return false;
        }
        if owner.split_measurement_started() {
            self.status = Some(crate::locale::text(
                "分屏恢复已经开始，不能撤销正在等待的资源操作。",
                "Split restoration has started and its in-flight resource operation cannot be cancelled.",
            ));
            cx.notify();
            return true;
        }
        if owner.cancel_requested() || owner.deferred_close.is_some() {
            self.status = Some(crate::locale::text(
                "分屏已由关闭操作接管，请等待清理回执。",
                "A close operation already owns this split; wait for its cleanup receipt.",
            ));
            cx.notify();
            return true;
        }
        let pane_is_empty_candidate =
            self.workspace_runtime
                .pane(&preview.pane_id)
                .is_some_and(|pane| {
                    pane.tab_id == preview.tab_id
                        && pane.phase == RuntimePanePhase::RestorePending
                        && pane.session_id.is_none()
                });
        let has_session_owner = self
            .workspace_runtime
            .session_for_pane(&preview.pane_id)
            .is_some();
        let has_orphan_owner = self
            .workspace_runtime
            .orphan_cleanup_for_pane(&preview.pane_id)
            .next()
            .is_some();
        if !pane_is_empty_candidate || has_session_owner || has_orphan_owner {
            self.status = Some(crate::locale::text(
                "分屏资源已进入恢复或清理阶段，不能直接取消；请等待状态回执。",
                "Split resources are restoring or cleaning up and cannot be discarded; wait for their status receipt.",
            ));
            cx.notify();
            return true;
        }

        // 未准入时没有异步 restore owner；先退役代次，再清理空占位和 reducer 候选。
        self.cancel_workspace_restore(&preview.pane_id);
        if !matches!(
            self.workspace_runtime.remove_empty_pane(&preview.pane_id),
            Ok(true)
        ) {
            self.status = Some(crate::locale::text(
                "候选分屏仍由运行态占用，尚未取消；可重试取消或检查工作区状态。",
                "The candidate pane is still owned by runtime state and was not cancelled; retry or inspect the workspace state.",
            ));
            cx.notify();
            return true;
        }
        let discarded = self
            .workspace_controller
            .as_mut()
            .is_some_and(|controller| controller.discard_pending_resource_operation());
        if !discarded {
            self.status = Some(crate::locale::text(
                "分屏候选已清理，但 controller 操作代次变化；请刷新工作区核对。",
                "The candidate pane was cleared, but the controller operation changed; refresh the workspace to reconcile it.",
            ));
            self.refresh_workspace_views(cx);
            cx.notify();
            return true;
        }
        self.workspace_resource_operations.remove(&preview.pane_id);
        self.status = Some(crate::locale::text(
            "已取消新分屏，原终端保持不变。",
            "The new split was cancelled; the original terminal is unchanged.",
        ));
        self.refresh_workspace_views(cx);
        self.refresh_workspace_orphan_cleanup(cx);
        cx.notify();
        true
    }

    /// Split 独占 reducer 资源槽时先保留关闭意图与原始焦点代次；仅首个请求捕获，重复请求不能升级或替换来源。
    /// 未测量候选可立即撤销，已准入创建必须等待 ACK；焦点资料不参与资源取消判断。
    ///
    /// @author kongweiguang
    pub(super) fn defer_workspace_split_close_intent(
        &mut self,
        intent: WorkspaceDeferredCloseIntent,
        close_confirmed: bool,
        keyboard_close_focus: Option<&FocusHandle>,
        tab_close_focus_override: Option<Option<CloseTabFocusIntent>>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        let pending = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .and_then(|operation| match operation {
                WorkspaceResourceOperation::CreatePane {
                    operation_id,
                    tab_id,
                    source_pane_id,
                    pane,
                    ..
                } => Some((
                    operation_id.clone(),
                    tab_id.clone(),
                    source_pane_id.clone(),
                    pane.id.clone(),
                )),
                WorkspaceResourceOperation::ClosePane { .. }
                | WorkspaceResourceOperation::CloseTab { .. } => None,
            });
        let Some((operation_id, split_tab_id, source_pane_id, split_pane_id)) = pending else {
            return false;
        };
        if !intent.targets_split(&split_tab_id, &source_pane_id, &split_pane_id) {
            self.status = Some(crate::locale::text(
                "分屏正在创建；此关闭尚未执行，也没有取消分屏，请稍后重试。",
                "A split is being created. This close was not applied and did not cancel it; retry when it finishes.",
            ));
            cx.notify();
            return true;
        }
        let Some(entry) = self.workspace_resource_operations.get(&split_pane_id) else {
            self.status = Some(crate::locale::text(
                "分屏操作记录无法核对；资源未取消，请稍后重试关闭。",
                "The split operation record could not be verified. Resources were not cancelled; retry close later.",
            ));
            cx.notify();
            return true;
        };
        if entry.operation_id() != operation_id.as_str() {
            self.status = Some(crate::locale::text(
                "分屏操作代次无法核对；资源未取消，请稍后重试关闭。",
                "The split operation generation could not be verified. Resources were not cancelled; retry close later.",
            ));
            cx.notify();
            return true;
        }
        if !close_confirmed {
            if entry.cancel_requested() {
                self.status = Some(crate::locale::text(
                    "已有关闭操作正在等待分屏清理；请先完成该操作。",
                    "A close is already waiting for split cleanup; finish it before requesting another close.",
                ));
                cx.notify();
                return true;
            }
            return false;
        }
        let capture_first_source = entry.deferred_close.is_none();
        let close_focus = match &intent {
            WorkspaceDeferredCloseIntent::Pane { tab_id, pane_id } if capture_first_source => self
                .capture_close_survivor_focus_for_pane(
                    tab_id,
                    pane_id,
                    keyboard_close_focus,
                    window,
                    cx,
                ),
            _ => None,
        };
        let close_tab_focus = match &intent {
            WorkspaceDeferredCloseIntent::Tab { tab_id } if capture_first_source => {
                tab_close_focus_override
                    .unwrap_or_else(|| self.capture_close_tab_focus_for_tab(tab_id, window, cx))
            }
            _ => None,
        };
        let Some(entry) = self.workspace_resource_operations.get_mut(&split_pane_id) else {
            return false;
        };
        let measurement_started = entry.split_measurement_started();
        match entry.defer_close_with_focus_tickets(intent, close_focus, close_tab_focus) {
            WorkspaceDeferredCloseRequest::AlreadyPending => {
                self.status = Some(crate::locale::text(
                    "已有一个关闭操作正在等待分屏清理；请先完成该操作。",
                    "A close request is already waiting for split cleanup; finish that request first.",
                ));
                cx.notify();
            }
            WorkspaceDeferredCloseRequest::Duplicate => {}
            WorkspaceDeferredCloseRequest::Stored if measurement_started => {
                self.cancel_workspace_restore(&split_pane_id);
                self.status = Some(crate::locale::text(
                    "正在取消分屏创建；收到资源清理回执后继续关闭。",
                    "Cancelling split creation; close will continue after its cleanup receipt.",
                ));
                self.refresh_workspace_views(cx);
                cx.notify();
            }
            WorkspaceDeferredCloseRequest::Stored => {
                // 尚未进入测量门，因此没有 open task 持有资源或可能返回迟到 ACK。
                self.cancel_workspace_restore(&split_pane_id);
                self.finalize_cancelled_workspace_split_after_cleanup(
                    &split_tab_id,
                    &split_pane_id,
                    &operation_id,
                    window,
                    cx,
                );
            }
        }
        true
    }

    /// 仅退役身份精确且无 session/orphan owner 的已取消候选，再单次重放原关闭意图与焦点票据。
    ///
    /// @author kongweiguang
    fn finalize_cancelled_workspace_split_after_cleanup(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        operation_id: &str,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let pending_matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .is_some_and(|operation| {
                matches!(
                    operation,
                    WorkspaceResourceOperation::CreatePane {
                        operation_id: pending_id,
                        tab_id: pending_tab,
                        pane,
                        ..
                    } if pending_id.as_str() == operation_id
                        && pending_tab == tab_id
                        && pane.id == *pane_id
                )
            });
        let Some(entry) = self.workspace_resource_operations.get(pane_id) else {
            return;
        };
        if !pending_matches || entry.operation_id() != operation_id || !entry.cancel_requested() {
            return;
        }
        if self.workspace_runtime.session_for_pane(pane_id).is_some()
            || self
                .workspace_runtime
                .orphan_cleanup_for_pane(pane_id)
                .next()
                .is_some()
        {
            self.status = Some(crate::locale::text(
                "分屏资源仍待清理；原关闭操作会在核对完成后继续。",
                "Split resources still need cleanup; the original close will continue after confirmation.",
            ));
            cx.notify();
            return;
        }
        if self.workspace_runtime.pane(pane_id).is_some()
            && !matches!(self.workspace_runtime.remove_empty_pane(pane_id), Ok(true))
        {
            self.status = Some(crate::locale::text(
                "分屏取消已收到回执，但空 pane 尚未能安全移除。",
                "Split cancellation was acknowledged, but its empty pane could not be safely removed.",
            ));
            cx.notify();
            return;
        }
        if !self
            .workspace_controller
            .as_mut()
            .is_some_and(|controller| controller.discard_pending_resource_operation())
        {
            self.status = Some(crate::locale::text(
                "分屏取消回执与当前 controller 操作不匹配；资源记录仍保留。",
                "The split cancellation receipt did not match the current controller operation; its record was retained.",
            ));
            cx.notify();
            return;
        }
        let deferred_close = self
            .workspace_resource_operations
            .get_mut(pane_id)
            .and_then(|entry| {
                entry.take_deferred_close_after_cleanup_with_focus(operation_id, true)
            });
        self.workspace_resource_operations.remove(pane_id);
        if self.status.as_ref().is_some_and(|status| {
            matches!(
                status.as_ref(),
                "正在创建分屏终端…"
                    | "Creating split terminal…"
                    | "正在取消分屏创建；收到资源清理回执后继续关闭。"
                    | "Cancelling split creation; close will continue after its cleanup receipt."
            )
        }) {
            self.status = None;
        }
        self.refresh_workspace_views(cx);
        if let Some((intent, close_focus, close_tab_focus)) = deferred_close {
            self.replay_deferred_workspace_close(intent, close_focus, close_tab_focus, window, cx);
        }
        cx.notify();
    }

    /// Tab/批量与最后一个 pane 仍走原保护入口；回放直接携带首个请求的票据，禁止 ACK 时重新捕获焦点。
    ///
    /// @author kongweiguang
    fn replay_deferred_workspace_close(
        &mut self,
        intent: WorkspaceDeferredCloseIntent,
        close_focus: Option<CloseSurvivorFocusIntent>,
        close_tab_focus: Option<CloseTabFocusIntent>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        match intent {
            WorkspaceDeferredCloseIntent::Pane { tab_id, pane_id } => {
                let layout = self.workspace_controller.as_ref().and_then(|controller| {
                    controller
                        .snapshot()
                        .terminal_tabs
                        .iter()
                        .find(|tab| tab.id() == &tab_id)
                        .and_then(WorkspaceSessionTab::terminal_layout)
                });
                if !is_last_committed_workspace_pane(layout, &pane_id) {
                    if is_removed_uncommitted_workspace_pane(
                        layout,
                        self.workspace_runtime.pane(&pane_id).is_some(),
                        &pane_id,
                    ) {
                        // 精确 split 取消 ACK 已移除此 preview；只消费先前来源，不能在此按当前焦点重新捕获。
                        self.complete_close_survivor_focus(close_focus, cx);
                        return;
                    }
                    let result = self.workspace_controller.as_mut().map(|controller| {
                        controller.apply_pane_event(
                            &tab_id,
                            crate::features::workspace_panes::WorkspacePaneEvent::Close(pane_id),
                        )
                    });
                    match result {
                        Some(Ok(WorkspaceControllerEffect::ResourceOperation(operation))) => {
                            self.handle_workspace_resource_operation_with_focus(
                                operation,
                                close_focus,
                                None,
                                cx,
                            );
                        }
                        Some(Err(error)) => {
                            self.status = Some(error.public_message().to_owned().into());
                            cx.notify();
                        }
                        _ => {}
                    }
                    return;
                }
                self.handle_workspace_render_event(
                    super::workspace_render::WorkspaceRenderEvent::Pane {
                        tab_id,
                        event: crate::features::workspace_panes::WorkspacePaneEvent::Close(pane_id),
                    },
                    window,
                    cx,
                );
            }
            WorkspaceDeferredCloseIntent::Tab { tab_id } => {
                let result = self
                    .workspace_controller
                    .as_mut()
                    .map(|controller| controller.apply_tab_event(WorkspaceTabEvent::Close(tab_id)));
                match result {
                    Some(Ok(WorkspaceControllerEffect::ResourceOperation(operation))) => {
                        self.handle_workspace_resource_operation_with_focus(
                            operation,
                            None,
                            close_tab_focus,
                            cx,
                        );
                    }
                    Some(Ok(WorkspaceControllerEffect::Changed { .. })) => {
                        self.refresh_workspace_views(cx);
                        self.persist_workspace_snapshot(cx);
                    }
                    Some(Ok(WorkspaceControllerEffect::Unchanged)) => {}
                    Some(Err(error)) => {
                        self.status = Some(error.public_message().to_owned().into());
                        cx.notify();
                    }
                    None => {}
                }
            }
            WorkspaceDeferredCloseIntent::Batch(intent) => {
                self.handle_workspace_tab_event(&WorkspaceTabEvent::CloseBatch(intent), window, cx);
            }
        }
    }

    /// 将精确取消回执排入窗口下一轮更新；身份复核与 entry 移除共同保证重复回调不重放。
    ///
    /// @author kongweiguang
    pub(super) fn schedule_cancelled_workspace_split_finalize(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        operation_id: &str,
        cx: &mut Context<RootView>,
    ) {
        let matches = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .is_some_and(|operation| {
                matches!(
                    operation,
                    WorkspaceResourceOperation::CreatePane {
                        operation_id: pending_id,
                        tab_id: pending_tab,
                        pane,
                        ..
                    } if pending_id.as_str() == operation_id
                        && pending_tab == tab_id
                        && pane.id == *pane_id
                )
            });
        let Some(entry) = self.workspace_resource_operations.get(pane_id) else {
            return;
        };
        if !matches || entry.operation_id() != operation_id || !entry.cancel_requested() {
            return;
        }
        if self.workspace_runtime.session_for_pane(pane_id).is_some()
            || self
                .workspace_runtime
                .orphan_cleanup_for_pane(pane_id)
                .next()
                .is_some()
        {
            return;
        }
        let window_handle = self.window_handle;
        let tab_id = tab_id.clone();
        let pane_id = pane_id.clone();
        let operation_id = operation_id.to_owned();
        cx.spawn(async move |this, cx| {
            let _ = cx.update_window(window_handle, move |_, window, app| {
                let _ = this.update(app, |root, cx| {
                    root.finalize_cancelled_workspace_split_after_cleanup(
                        &tab_id,
                        &pane_id,
                        &operation_id,
                        window,
                        cx,
                    );
                });
            });
        })
        .detach();
    }

    /// 所有恢复任务都返回后才续跑被取消的关闭；仍有 orphan 时保留操作供用户重试清理。
    pub(super) fn complete_pending_workspace_close_after_restore_ack(
        &mut self,
        pane_id: &PaneId,
        cx: &mut Context<RootView>,
    ) {
        let operation = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_resource_operation())
            .filter(|operation| match operation {
                WorkspaceResourceOperation::ClosePane {
                    pane_id: closing, ..
                } => closing == pane_id,
                WorkspaceResourceOperation::CloseTab { pane_ids, .. } => pane_ids.contains(pane_id),
                WorkspaceResourceOperation::CreatePane { .. } => false,
            })
            .cloned();
        let Some(operation) = operation else {
            return;
        };
        let pane_ids = match &operation {
            WorkspaceResourceOperation::ClosePane { pane_id, .. } => vec![pane_id.clone()],
            WorkspaceResourceOperation::CloseTab { pane_ids, .. } => pane_ids.clone(),
            WorkspaceResourceOperation::CreatePane { .. } => return,
        };
        if let Some(pane) = self.workspace_runtime.pane(pane_id)
            && pane.phase == RuntimePanePhase::RestorePending
            && pane.session_id.is_none()
        {
            let marked = self.workspace_runtime.mark_restore_failed(pane_id);
            let status_set = self.workspace_runtime.set_status(
                pane_id,
                Some(crate::locale::text(
                    "启动已取消，可以安全关闭此 pane。",
                    "Startup was cancelled; this pane can now be closed safely.",
                )),
            );
            if marked.is_err() || status_set.is_err() {
                self.complete_workspace_resource_failure(
                    operation.operation_id(),
                    crate::locale::text(
                        "启动取消已收到回执，但 pane 状态更新失败；工作区关闭保持未提交。",
                        "Startup cancellation was acknowledged, but pane state could not be updated; workspace close remains uncommitted.",
                    ),
                    cx,
                );
                return;
            }
        }
        if pane_ids.iter().any(|id| {
            self.workspace_runtime
                .pane(id)
                .is_some_and(|pane| pane.phase == RuntimePanePhase::RestorePending)
        }) {
            return;
        }
        let orphan_pending = match &operation {
            WorkspaceResourceOperation::ClosePane { pane_id, .. } => self
                .workspace_runtime
                .orphan_cleanup_for_pane(pane_id)
                .next()
                .is_some(),
            WorkspaceResourceOperation::CloseTab { tab_id, .. } => self
                .workspace_runtime
                .orphan_cleanup_for_tab(tab_id)
                .next()
                .is_some(),
            WorkspaceResourceOperation::CreatePane { .. } => false,
        };
        if orphan_pending {
            self.complete_workspace_resource_failure(
                operation.operation_id(),
                crate::locale::text(
                    "已创建的终端仍待清理；重试关闭可继续清理，不会重新创建会话。",
                    "A created terminal still needs cleanup. Retry close to continue cleanup; no new session will be created.",
                ),
                cx,
            );
            return;
        }
        self.handle_workspace_resource_operation(operation, cx);
    }

    /// 非终端标签经 editor guard 后完成同一个 controller operation，再释放独立面板实体；不维护第二份 active-tab 状态。
    fn complete_nonterminal_workspace_tab_close(
        &mut self,
        operation_id: String,
        tab_id: TabId,
        cx: &mut Context<RootView>,
    ) {
        let result = self
            .workspace_controller
            .as_mut()
            .map(|controller| controller.complete_resource_operation(&operation_id, Ok(())));
        match result {
            Some(Ok(_)) => {
                self.remove_workspace_file_tab_after_close(&tab_id);
                self.update_runtime_layout_after_close(&tab_id, true);
                self.note_close_batch_tab_success(&tab_id);
                self.status = None;
                self.refresh_workspace_views(cx);
                self.persist_workspace_snapshot(cx);
                self.advance_workspace_tab_close_batch(cx);
            }
            Some(Err(error)) => self.status = Some(error.public_message().into()),
            None => return,
        }
        cx.notify();
    }

    /// 资源关闭失败时保留 controller 的失败候选，供用户显式重试。
    fn complete_workspace_resource_failure(
        &mut self,
        operation_id: &str,
        message: SharedString,
        cx: &mut Context<RootView>,
    ) {
        let completion_error = self.workspace_controller.as_mut().and_then(|controller| {
            controller
                .complete_resource_operation(
                    operation_id,
                    Err(WorkspaceRestoreFailure::ResourceFailure),
                )
                .err()
        });
        let mut visible_message = match completion_error {
            Some(error) => SharedString::from(format!("{} {}", message, error.public_message())),
            None => message,
        };
        if let Some(remaining) = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_close_batch_remaining_count())
        {
            visible_message = SharedString::from(format!(
                "{visible_message}{}",
                crate::locale::text(
                    format!(
                        " 批量关闭已停止，仍有 {remaining} 个目标保留；再次执行同一关闭操作可重试剩余目标。"
                    ),
                    format!(
                        " Batch close stopped with {remaining} targets retained; run the same close action again to retry the remaining targets."
                    ),
                )
            ));
        }
        self.status = Some(visible_message);
        self.refresh_workspace_views(cx);
        cx.notify();
    }

    /// 仅在当前 tab 获得成功资源 ACK 后启动固定快照中的下一项；直接派发避免弱实体回投丢失关闭意图。
    /// 批次成功后才异步请求移除临时 SFTP 工作台，失败回执不会改变当前视图。
    ///
    /// @author kongweiguang
    fn advance_workspace_tab_close_batch(&mut self, cx: &mut Context<RootView>) {
        let batch_was_pending = self
            .workspace_controller
            .as_ref()
            .and_then(|controller| controller.pending_close_batch_remaining_count())
            .is_some();
        let result = self
            .workspace_controller
            .as_mut()
            .map(|controller| controller.advance_close_batch());
        match result {
            Some(Ok(Some(operation))) => {
                self.handle_workspace_resource_operation(operation, cx);
            }
            Some(Ok(None)) => {
                self.close_shared_sftp_workbench_after_successful_batch(cx);
                let batch_finished = batch_was_pending
                    && self
                        .workspace_controller
                        .as_ref()
                        .and_then(|controller| controller.pending_close_batch_remaining_count())
                        .is_none();
                if batch_finished && let Some(pending) = self.pending_close_batch_focus.as_mut() {
                    let intent = pending.intent.clone();
                    let focus = pending.take_after_successful_batch(&intent);
                    self.pending_close_batch_focus = None;
                    if let Some(focus) = focus {
                        self.complete_close_tab_focus(focus, cx);
                    }
                }
            }
            None => {}
            Some(Err(error)) => {
                self.status = Some(error.public_message().to_owned().into());
                self.refresh_workspace_views(cx);
            }
        }
    }

    /// 批量关闭只保留首个有效焦点快照，重试不得用当前菜单/标签焦点替换它。
    ///
    /// @author kongweiguang
    fn retain_close_batch_focus(
        &mut self,
        ticket: PendingCloseBatchFocus,
        may_replace_previous: bool,
    ) {
        if self
            .pending_close_batch_focus
            .as_ref()
            .is_some_and(|pending| pending.matches_intent(&ticket.intent))
        {
            return;
        }
        if !may_replace_previous {
            return;
        }
        let deferred_close_conflict = self.workspace_resource_operations.values().any(|entry| {
            entry.deferred_close.as_ref().is_some_and(|intent| {
                intent != &WorkspaceDeferredCloseIntent::Batch(ticket.intent.clone())
            })
        });
        if deferred_close_conflict {
            return;
        }
        self.pending_close_batch_focus = Some(ticket);
    }

    /// 只有成功提交 CloseTab 的资源 ACK 才能标记批次中的焦点目标已真正移除。
    ///
    /// @author kongweiguang
    fn note_close_batch_tab_success(&mut self, tab_id: &TabId) {
        if let Some(pending) = self.pending_close_batch_focus.as_mut() {
            pending.note_tab_closed(tab_id);
        }
    }

    /// 关闭提交后按 reducer 的真实布局同步运行态 pane 布局索引。
    fn update_runtime_layout_after_close(&mut self, tab_id: &TabId, remove_tab: bool) {
        if remove_tab {
            self.workspace_runtime.remove_tab_layout(tab_id);
            return;
        }
        let layout = self.workspace_controller.as_ref().and_then(|controller| {
            controller
                .snapshot()
                .terminal_tabs
                .iter()
                .find_map(|tab| match tab {
                    kerminal_application::workspace::WorkspaceSessionTab::Terminal {
                        id,
                        layout,
                        ..
                    } if id == tab_id => Some(layout.clone()),
                    _ => None,
                })
        });
        if let Some(layout) = layout {
            self.workspace_runtime
                .register_tab_layout(tab_id.clone(), layout);
        } else {
            self.workspace_runtime.remove_tab_layout(tab_id);
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_close.rs"]
mod tests;
