//! 工作区 v3 恢复、交互 reducer 与持久化状态协调；不持有 GPUI 或会话资源。
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use gpui_kit::SharedString;
use kerminal_application::{
    AppError, AppResult,
    workspace::{
        WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode,
        WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionLoad,
        WorkspaceSessionPane, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
        WorkspaceSessionSnapshot, WorkspaceSessionTab, WorkspaceTabGroup, WorkspaceTabGroupColor,
        WorkspaceTabGroupId, WorkspaceTargetRef, WorkspaceTargetUnavailableReason,
    },
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use super::{
    workspace_panes::WorkspacePaneEvent,
    workspace_tabs::{
        GroupTabMoveDirection, TabDropPlacement, WorkspaceCloseBatchIntent, WorkspaceTabEvent,
        WorkspaceTabGroupProjection, WorkspaceTabKind, WorkspaceTabProjection,
    },
};

/// Root 恢复 workspace 时的每个独立业务资源；pane/tab 身份保留，运行时 SessionId 不进入快照。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceRestoreItem {
    TerminalPane {
        tab_id: TabId,
        pane: Box<WorkspaceSessionPane>,
    },
    SftpTransfer {
        tab: Box<WorkspaceSessionTab>,
    },
    WorkspaceFile {
        tab: Box<WorkspaceSessionTab>,
    },
    UnsupportedTab {
        tab: Box<WorkspaceSessionTab>,
    },
}

/// 恢复资源的稳定键允许 Root 在异步完成后只更新对应 pane 或 tab 的状态。
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum WorkspaceRestoreItemId {
    Pane(PaneId),
    Tab(TabId),
}

/// 恢复错误采用稳定分类；连接细节由 Root 按现有安全错误策略单独显示。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRestoreFailure {
    MissingTarget,
    UnavailableTarget(WorkspaceTargetUnavailableReason),
    UnsupportedTab,
    Cancelled,
    ResourceFailure,
}

/// 按资源显示创建中、就绪或可重试失败，不把临时连接状态写回旧 session。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceRestoreStatus {
    Pending,
    Ready,
    Failed(WorkspaceRestoreFailure),
}

/// Root 执行资源创建/关闭后回传结果；只有成功回传才提交候选布局。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceResourceOperation {
    CreatePane {
        operation_id: String,
        tab_id: TabId,
        source_pane_id: PaneId,
        pane: Box<WorkspaceSessionPane>,
    },
    ClosePane {
        operation_id: String,
        tab_id: TabId,
        pane_id: PaneId,
    },
    CloseTab {
        operation_id: String,
        tab_id: TabId,
        tab: Box<WorkspaceSessionTab>,
        pane_ids: Vec<PaneId>,
    },
}

/// Root 对本机临时容器给出的窄同步凭据；tab/pane/container 必须与候选快照完整一致。
#[derive(Clone, Copy)]
struct TransientLocalContainerBinding<'a> {
    tab_id: &'a TabId,
    pane_id: &'a PaneId,
    container_id: &'a str,
}

/// 分屏选择器只持有可显示的目标摘要与 typed ID，不复制认证、路由或 Profile 启动正文。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSplitTargetOption {
    pub target: WorkspaceTargetRef,
    pub title: String,
    pub detail: Option<String>,
    pub group_id: Option<String>,
}

impl WorkspaceResourceOperation {
    /// 所有异步资源结果都以唯一操作 ID 关联，防止迟到完成提交另一项候选工作区。
    pub fn operation_id(&self) -> &str {
        match self {
            Self::CreatePane { operation_id, .. }
            | Self::ClosePane { operation_id, .. }
            | Self::CloseTab { operation_id, .. } => operation_id,
        }
    }
}

/// Reducer 的结果区分本地快照变更和必须由 Root 先完成的真实资源操作。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspaceControllerEffect {
    Unchanged,
    Changed { generation: u64 },
    ResourceOperation(WorkspaceResourceOperation),
}

/// 一次保存携带完整 CAS 基线和不可变候选，Root 可安全交给 application service 异步执行。
#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceSaveRequest {
    pub request_id: String,
    pub generation: u64,
    pub base_document: WorkspaceSessionDocument,
    pub expected_revision: WorkspaceSessionExpectedRevision,
    pub base_snapshot: WorkspaceSessionSnapshot,
    /// 保存开始时的完整 UI 状态；回执只在 UI 仍等于此快照时才能清除后续变更。
    pub source_snapshot: WorkspaceSessionSnapshot,
    pub desired_snapshot: WorkspaceSessionSnapshot,
    pub candidate_document: WorkspaceSessionDocument,
}

/// 保存状态显式区分冲突和结果未知，避免以重试掩盖文件可能已提交的事实。
#[derive(Clone, Debug, PartialEq)]
pub enum WorkspacePersistenceState {
    Clean,
    Dirty,
    Saving { request_id: String, generation: u64 },
    Conflict { latest: Box<WorkspaceSessionLoad> },
    OutcomeUnknown { request: Box<WorkspaceSaveRequest> },
    ReadbackConfirmedUncommitted { latest: Box<WorkspaceSessionLoad> },
    Failed,
}

/// 纯工作区状态机；Root 保留运行时 session/target 映射并执行所有连接副作用。
pub struct WorkspaceController {
    base_document: WorkspaceSessionDocument,
    base_revision: WorkspaceSessionExpectedRevision,
    base_snapshot: WorkspaceSessionSnapshot,
    snapshot: WorkspaceSessionSnapshot,
    /// 最近一次与持久化基线对应的完整 UI 状态，允许 transient 仍留在界面而不被误判为 dirty。
    clean_working_snapshot: WorkspaceSessionSnapshot,
    generation: u64,
    save_state: WorkspacePersistenceState,
    in_flight_save: Option<WorkspaceSaveRequest>,
    pending_resource: Option<PendingResourceChange>,
    close_batch: Option<PendingTabCloseBatch>,
    restore_status: BTreeMap<WorkspaceRestoreItemId, WorkspaceRestoreStatus>,
}

/// 资源副作用完成前暂存候选；失败保留操作本身供显式重试或放弃。
struct PendingResourceChange {
    operation: WorkspaceResourceOperation,
    candidate: WorkspaceSessionSnapshot,
    failed: Option<WorkspaceRestoreFailure>,
}

/// 批量关闭只保存点击时的目标和下一项索引，不复制工作区 tab 或资源 owner。
#[derive(Clone, Debug, Eq, PartialEq)]
struct PendingTabCloseBatch {
    intent: WorkspaceCloseBatchIntent,
    next_index: usize,
}

impl WorkspaceController {
    /// 从已验证的 v3 文档创建基线；controller 不重新读取磁盘或猜测缺失目标。
    pub fn from_loaded(
        document: WorkspaceSessionDocument,
        revision: WorkspaceSessionRevision,
    ) -> AppResult<Self> {
        Self::from_baseline(
            document,
            WorkspaceSessionExpectedRevision::Revision(revision),
        )
    }

    /// 缺失状态使用明确的 no-clobber revision，不将首次创建伪装成已存在文档。
    pub fn from_missing() -> AppResult<Self> {
        Self::from_baseline(
            WorkspaceSessionDocument::empty_v3(),
            WorkspaceSessionExpectedRevision::Missing,
        )
    }

    /// 两种 load 状态共享初始化路径，确保 snapshot 与 CAS 基线来自同一已验证文档。
    fn from_baseline(
        document: WorkspaceSessionDocument,
        base_revision: WorkspaceSessionExpectedRevision,
    ) -> AppResult<Self> {
        let snapshot = document.workspace_snapshot()?;
        let restore_status = initial_restore_status(&snapshot);
        Ok(Self {
            base_document: document,
            base_revision,
            base_snapshot: snapshot.clone(),
            snapshot: snapshot.clone(),
            clean_working_snapshot: snapshot,
            generation: 0,
            save_state: WorkspacePersistenceState::Clean,
            in_flight_save: None,
            pending_resource: None,
            close_batch: None,
            restore_status,
        })
    }

    /// 返回只读 typed 快照供 Root 建立每个 pane 的独立运行时资源。
    pub fn snapshot(&self) -> &WorkspaceSessionSnapshot {
        &self.snapshot
    }

    /// 用快照生成 tabs 组件纯投影，非终端 tab 不伪造 layout 或 pane。
    pub fn tab_projections(&self) -> Vec<WorkspaceTabProjection> {
        ordered_tab_indices(&self.snapshot)
            .into_iter()
            .filter_map(|index| self.snapshot.terminal_tabs.get(index))
            .map(|tab| {
                let (kind, layout) = match tab {
                    WorkspaceSessionTab::Terminal { layout, .. } => {
                        (WorkspaceTabKind::Terminal, Some(layout.clone()))
                    }
                    WorkspaceSessionTab::SftpTransfer { .. } => {
                        (WorkspaceTabKind::SftpTransfer, None)
                    }
                    WorkspaceSessionTab::WorkspaceFile { .. } => {
                        (WorkspaceTabKind::WorkspaceFile, None)
                    }
                    WorkspaceSessionTab::Unsupported { kind, .. } => (
                        WorkspaceTabKind::Unsupported(SharedString::from(kind.clone())),
                        None,
                    ),
                };
                WorkspaceTabProjection {
                    id: tab.id().clone(),
                    title: SharedString::from(tab.title().to_owned()),
                    group_id: tab
                        .tab_group_id()
                        .map(|group_id| group_id.as_str().to_owned()),
                    active: self.snapshot.active_tab_id.as_ref() == Some(tab.id()),
                    kind,
                    layout,
                }
            })
            .collect()
    }

    /// 返回完整标签组目录，包括尚未分配标签的组，供 UI 生成可靠拖放目标。
    pub fn tab_group_projections(&self) -> Vec<WorkspaceTabGroupProjection> {
        ordered_tab_group_ids(&self.snapshot)
            .into_iter()
            .enumerate()
            .filter_map(|(order, id)| {
                self.snapshot
                    .tab_groups
                    .get(&id)
                    .map(|group| WorkspaceTabGroupProjection {
                        id: id.as_str().to_owned(),
                        title: SharedString::from(group.title.clone()),
                        color: group.color.clone(),
                        collapsed: group.collapsed,
                        order,
                    })
            })
            .collect()
    }

    /// 返回全部恢复意图；Root 必须按 ID 各自创建，不复用一个 live session 给多个 pane。
    pub fn restore_plan(&self) -> Vec<WorkspaceRestoreItem> {
        let panes = self
            .snapshot
            .terminal_panes
            .iter()
            .map(|pane| (pane.id.clone(), pane.clone()))
            .collect::<BTreeMap<_, _>>();
        let mut items = Vec::new();
        for tab in &self.snapshot.terminal_tabs {
            match tab {
                WorkspaceSessionTab::Terminal { id, layout, .. } => {
                    for pane_id in layout.pane_ids() {
                        if let Some(pane) = panes.get(&pane_id) {
                            items.push(WorkspaceRestoreItem::TerminalPane {
                                tab_id: id.clone(),
                                pane: Box::new(pane.clone()),
                            });
                        }
                    }
                }
                WorkspaceSessionTab::SftpTransfer { .. } => {
                    items.push(WorkspaceRestoreItem::SftpTransfer {
                        tab: Box::new(tab.clone()),
                    });
                }
                WorkspaceSessionTab::WorkspaceFile { .. } => {
                    items.push(WorkspaceRestoreItem::WorkspaceFile {
                        tab: Box::new(tab.clone()),
                    });
                }
                WorkspaceSessionTab::Unsupported { .. } => {
                    items.push(WorkspaceRestoreItem::UnsupportedTab {
                        tab: Box::new(tab.clone()),
                    });
                }
            }
        }
        items
    }

    /// Root 回报每项实际恢复结果；失败保留稳定错误分类以支持重试和可见提示。
    pub fn record_restore_result(
        &mut self,
        item_id: WorkspaceRestoreItemId,
        result: Result<(), WorkspaceRestoreFailure>,
    ) -> AppResult<()> {
        let status = self
            .restore_status
            .get_mut(&item_id)
            .ok_or_else(|| AppError::not_found("工作区恢复项不存在"))?;
        *status = match result {
            Ok(()) => WorkspaceRestoreStatus::Ready,
            Err(failure) => WorkspaceRestoreStatus::Failed(failure),
        };
        Ok(())
    }

    /// 查询单个资源的恢复状态，不把临时连接情况写入 session 文件。
    pub fn restore_status(
        &self,
        item_id: &WorkspaceRestoreItemId,
    ) -> Option<WorkspaceRestoreStatus> {
        self.restore_status.get(item_id).copied()
    }

    /// 查询保存闭环状态供 Root 显示 dirty、保存中、冲突或待回读提示。
    pub fn persistence_state(&self) -> &WorkspacePersistenceState {
        &self.save_state
    }

    /// dirty、异步资源操作及未知提交都阻止普通退出快路清空工作区。
    pub fn has_unsaved_or_pending_work(&self) -> bool {
        self.snapshot != self.clean_working_snapshot
            || self.pending_resource.is_some()
            || self.close_batch.is_some()
            || self.in_flight_save.is_some()
            || matches!(
                &self.save_state,
                WorkspacePersistenceState::Conflict { .. }
                    | WorkspacePersistenceState::OutcomeUnknown { .. }
                    | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
            )
    }

    /// Root 仅在实际资源创建或变更成功后同步 typed 快照；未显式绑定的新增 pane 必须有可恢复目标。
    /// 在途 CAS 请求保持不可变，旧回执只确认它自己的 source snapshot；后到资源仍会留在 Dirty 状态。
    ///
    /// @author kongweiguang
    pub fn synchronize_snapshot(
        &mut self,
        snapshot: WorkspaceSessionSnapshot,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.synchronize_snapshot_inner(snapshot, None)
    }

    /// 只为 Root 已核实的本机临时容器放行目标为空的 UI snapshot；该 pane 仍由运行态 owner 绑定。
    ///
    /// @author kongweiguang
    pub(crate) fn synchronize_snapshot_for_transient_local_container(
        &mut self,
        snapshot: WorkspaceSessionSnapshot,
        tab_id: &TabId,
        pane_id: &PaneId,
        container_id: &str,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.synchronize_snapshot_inner(
            snapshot,
            Some(TransientLocalContainerBinding {
                tab_id,
                pane_id,
                container_id,
            }),
        )
    }

    /// 复用唯一快照 CAS/reducer 门禁，例外只绑定一次 Root 明确给出的本机 transient container。
    ///
    /// @author kongweiguang
    fn synchronize_snapshot_inner(
        &mut self,
        snapshot: WorkspaceSessionSnapshot,
        transient_local_container: Option<TransientLocalContainerBinding<'_>>,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.ensure_no_pending_resource()?;
        let state_is_saving = matches!(&self.save_state, WorkspacePersistenceState::Saving { .. });
        if state_is_saving != self.in_flight_save.is_some() {
            return Err(AppError::conflict("工作区保存状态与在途请求不一致"));
        }
        snapshot
            .validate()
            .map_err(|_| AppError::invalid("运行态工作区快照无法通过验证"))?;
        if snapshot == self.snapshot {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        validate_new_runtime_targets(&self.snapshot, &snapshot, transient_local_container)?;

        self.snapshot = snapshot;
        self.reconcile_restore_status_after_runtime_sync();
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 标签交互在纯快照内更新选择/顺序/分组；拖放落点由 UI 明确给出并由 reducer 按稳定身份提交。
    /// 末尾空白投放以一个 reducer 意图同时解组和追加，避免拆成两次保存。
    /// 关闭操作仍先等待 Root 释放真实资源。
    ///
    /// @author kongweiguang
    pub fn apply_tab_event(
        &mut self,
        event: WorkspaceTabEvent,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.ensure_no_pending_resource()?;
        match event {
            // 文件标签的剪贴板、导航与编辑器读回属于 Root 的 UI 能力，不写入快照 reducer。
            WorkspaceTabEvent::FileAction { .. } => Ok(WorkspaceControllerEffect::Unchanged),
            WorkspaceTabEvent::Activate(tab_id) => self.activate_tab(&tab_id),
            WorkspaceTabEvent::Close(tab_id) => self.begin_close_tab(&tab_id),
            WorkspaceTabEvent::CloseBatch(intent) => self.begin_close_batch(intent),
            WorkspaceTabEvent::Reorder {
                tab_id,
                target_index,
            } => self.reorder_tab(&tab_id, target_index),
            WorkspaceTabEvent::DropOnTab {
                tab_id,
                target_tab_id,
                placement,
            } => self.drop_tab_on_tab(&tab_id, &target_tab_id, placement),
            WorkspaceTabEvent::MoveToUngroupedEnd(tab_id) => {
                self.move_tab_to_ungrouped_end(&tab_id)
            }
            WorkspaceTabEvent::MoveWithinGroup {
                tab_id,
                group_id,
                direction,
            } => self.move_tab_within_group(&tab_id, &group_id, direction),
            WorkspaceTabEvent::MoveToGroup { tab_id, group_id } => {
                self.move_tab_to_group(&tab_id, group_id.as_deref())
            }
            WorkspaceTabEvent::CreateGroupForTab { tab_id, title } => {
                self.create_tab_group_for_tab(&tab_id, &title)
            }
            WorkspaceTabEvent::RenameTab { tab_id, title } => self.rename_tab(&tab_id, &title),
            WorkspaceTabEvent::RenameGroup { group_id, title } => {
                self.rename_tab_group(&group_id, &title)
            }
            WorkspaceTabEvent::SetGroupColor { group_id, color } => {
                self.set_tab_group_color(&group_id, color)
            }
            WorkspaceTabEvent::SetGroupCollapsed {
                group_id,
                collapsed,
            } => self.set_tab_group_collapsed(&group_id, collapsed),
            WorkspaceTabEvent::MoveGroup {
                group_id,
                before_group_id,
            } => self.move_tab_group(&group_id, before_group_id.as_deref()),
            WorkspaceTabEvent::Ungroup(group_id) => self.ungroup_tabs(&group_id),
        }
    }

    /// Pane 交互按父 tab 明确定位；分屏生成新稳定 IDs，Root 只在会话创建成功后提交树。
    /// 键盘关闭的控件来源必须先在 Root 归一成 Close；群发只影响运行态选区，不进入布局持久化 reducer。
    ///
    /// @author kongweiguang
    pub fn apply_pane_event(
        &mut self,
        tab_id: &TabId,
        event: WorkspacePaneEvent,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.ensure_no_pending_resource()?;
        match event {
            WorkspacePaneEvent::Activate(pane_id) => self.activate_pane(tab_id, &pane_id),
            WorkspacePaneEvent::Split {
                pane_id,
                direction,
                before,
            } => self.begin_split(tab_id, &pane_id, direction, before),
            WorkspacePaneEvent::ChooseSplitTarget { .. } => {
                Err(AppError::unsupported("分屏目标选择必须由工作区根层完成"))
            }
            WorkspacePaneEvent::CancelSplitTarget { .. } => Err(AppError::unsupported(
                "分屏目标取消必须由工作区根层核对请求身份",
            )),
            WorkspacePaneEvent::Close(pane_id) => self.begin_close_pane(tab_id, &pane_id),
            WorkspacePaneEvent::CloseFromKeyboard { .. } => {
                Err(AppError::unsupported("键盘关闭来源必须先由工作区根层核对"))
            }
            WorkspacePaneEvent::ToggleBroadcastTarget { .. }
            | WorkspacePaneEvent::ShowBroadcast { .. } => {
                Err(AppError::unsupported("群发目标必须由工作区根层处理"))
            }
            WorkspacePaneEvent::Swap { first, second } => self.swap_panes(tab_id, &first, &second),
            WorkspacePaneEvent::Resize { split_id, sizes } => {
                self.resize_split(tab_id, &split_id, sizes)
            }
        }
    }

    /// 返回尚未完成的资源操作，使 Root 可在失败提示后以相同 ID 显式重试。
    pub fn pending_resource_operation(&self) -> Option<&WorkspaceResourceOperation> {
        self.pending_resource
            .as_ref()
            .map(|pending| &pending.operation)
    }

    /// 只读返回精确 CreatePane 操作持有的候选快照；测量预览不得提前改动持久工作区。
    ///
    /// @author kongweiguang
    pub fn pending_create_pane_candidate(
        &self,
        operation_id: &str,
    ) -> Option<&WorkspaceSessionSnapshot> {
        let pending = self.pending_resource.as_ref()?;
        if pending.operation.operation_id() != operation_id
            || !matches!(
                &pending.operation,
                WorkspaceResourceOperation::CreatePane { .. }
            )
        {
            return None;
        }
        Some(&pending.candidate)
    }

    /// 只有与当前 pending ID 完全一致的资源回执才能提交候选工作区。
    pub fn complete_resource_operation(
        &mut self,
        operation_id: &str,
        result: Result<(), WorkspaceRestoreFailure>,
    ) -> AppResult<WorkspaceControllerEffect> {
        let pending = self
            .pending_resource
            .as_ref()
            .ok_or_else(|| AppError::not_found("没有待完成的工作区资源操作"))?;
        if pending.operation.operation_id() != operation_id {
            return Err(AppError::conflict("工作区资源操作回执已过期"));
        }
        match result {
            Ok(()) => {
                let completed_tab_id = match &pending.operation {
                    WorkspaceResourceOperation::CloseTab { tab_id, .. } => Some(tab_id.clone()),
                    WorkspaceResourceOperation::CreatePane { .. }
                    | WorkspaceResourceOperation::ClosePane { .. } => None,
                };
                if let Some(batch) = &self.close_batch
                    && let Some(expected_tab_id) = batch.intent.tab_ids().get(batch.next_index)
                    && completed_tab_id.as_ref() != Some(expected_tab_id)
                {
                    return Err(AppError::conflict("关闭回执与批量关闭当前目标不一致"));
                }
                let pending = self
                    .pending_resource
                    .take()
                    .ok_or_else(|| AppError::conflict("工作区资源操作状态已变化"))?;
                self.snapshot = pending.candidate;
                self.sync_restore_status_after_change(&pending.operation);
                if completed_tab_id.is_some()
                    && let Some(batch) = self.close_batch.as_mut()
                {
                    batch.next_index += 1;
                }
                self.mark_changed();
                Ok(WorkspaceControllerEffect::Changed {
                    generation: self.generation,
                })
            }
            Err(failure) => {
                if let Some(pending) = self.pending_resource.as_mut() {
                    pending.failed = Some(failure);
                }
                Ok(WorkspaceControllerEffect::Unchanged)
            }
        }
    }

    /// 再次返回原资源请求；Root 可在用户确认后重试，不会悄悄换 target 或重新生成 ID。
    pub fn retry_pending_resource_operation(&self) -> Option<WorkspaceResourceOperation> {
        self.pending_resource
            .as_ref()
            .filter(|pending| pending.failed.is_some())
            .map(|pending| pending.operation.clone())
    }

    /// 用户取消失败操作时丢弃候选，保留原布局和仍运行的 session。
    pub fn discard_pending_resource_operation(&mut self) -> bool {
        let discarded = self.pending_resource.take().is_some();
        if discarded {
            self.close_batch = None;
        }
        discarded
    }

    /// 失败批次只接受同一意图及其尚未处理的 ID 快照重试，已关闭项不会重新加入。
    pub fn retry_pending_close_batch(
        &self,
        intent: &WorkspaceCloseBatchIntent,
    ) -> Option<WorkspaceResourceOperation> {
        let batch = self.close_batch.as_ref()?;
        if !same_close_batch_intent(&batch.intent, intent)
            || batch.intent.tab_ids().get(batch.next_index..) != Some(intent.tab_ids())
        {
            return None;
        }
        self.retry_pending_resource_operation()
    }

    /// 报告失败批次从当前失败项开始剩余的固定目标数，Root 用于说明未继续关闭的范围。
    pub fn pending_close_batch_remaining_count(&self) -> Option<usize> {
        self.close_batch.as_ref().map(|batch| {
            batch
                .intent
                .tab_ids()
                .len()
                .saturating_sub(batch.next_index)
        })
    }

    /// 当前关闭项成功提交后才启动快照中的下一项；无剩余目标时结束批次。
    pub fn advance_close_batch(&mut self) -> AppResult<Option<WorkspaceResourceOperation>> {
        if self.pending_resource.is_some() {
            return Ok(None);
        }
        let Some(batch) = self.close_batch.as_ref() else {
            return Ok(None);
        };
        let Some(tab_id) = batch.intent.tab_ids().get(batch.next_index).cloned() else {
            self.close_batch = None;
            return Ok(None);
        };
        match self.begin_close_tab(&tab_id)? {
            WorkspaceControllerEffect::ResourceOperation(operation) => Ok(Some(operation)),
            _ => Err(AppError::conflict("批量关闭的下一项未建立资源操作")),
        }
    }

    /// 兼容无运行态投影的保存调用；含临时资源的 Root 应调用 begin_save_projected。
    pub fn begin_save(&mut self) -> AppResult<Option<WorkspaceSaveRequest>> {
        let snapshot = self.snapshot.clone();
        self.begin_save_projected(snapshot)
    }

    /// 在建立 CAS in-flight 前接收持久化投影，保证回执状态与实际提交文档使用同一快照。
    ///
    /// UI 的完整快照始终保留在 controller 中；只有 candidate 和 CAS desired 使用投影。
    /// 若投影已等于持久化基线，则无需磁盘 I/O，并把当前完整 UI 状态记为已清洁。
    pub fn begin_save_projected(
        &mut self,
        projected_snapshot: WorkspaceSessionSnapshot,
    ) -> AppResult<Option<WorkspaceSaveRequest>> {
        if self.pending_resource.is_some() || self.in_flight_save.is_some() {
            return Ok(None);
        }
        if !matches!(
            &self.save_state,
            WorkspacePersistenceState::Clean
                | WorkspacePersistenceState::Dirty
                | WorkspacePersistenceState::Failed
        ) {
            return Ok(None);
        }
        projected_snapshot
            .validate()
            .map_err(|_| AppError::invalid("工作区持久化投影无法通过验证"))?;
        if projected_snapshot == self.base_snapshot {
            self.clean_working_snapshot = self.snapshot.clone();
            self.save_state = WorkspacePersistenceState::Clean;
            return Ok(None);
        }

        let mut candidate_document = self.base_document.clone();
        candidate_document.update_known_fields(&projected_snapshot)?;
        let request = WorkspaceSaveRequest {
            request_id: uuid::Uuid::new_v4().to_string(),
            generation: self.generation,
            base_document: self.base_document.clone(),
            expected_revision: self.base_revision.clone(),
            base_snapshot: self.base_snapshot.clone(),
            source_snapshot: self.snapshot.clone(),
            desired_snapshot: projected_snapshot,
            candidate_document,
        };
        self.save_state = WorkspacePersistenceState::Saving {
            request_id: request.request_id.clone(),
            generation: request.generation,
        };
        self.in_flight_save = Some(request.clone());
        Ok(Some(request))
    }

    /// 按 request ID 应用 CAS 回执；clean 判断使用完整来源快照，避免投影隐藏保存期间的新变更。
    pub fn finish_save(
        &mut self,
        request_id: &str,
        outcome: WorkspaceSessionSaveOutcome,
    ) -> AppResult<()> {
        let request = self
            .in_flight_save
            .as_ref()
            .filter(|request| request.request_id == request_id)
            .cloned()
            .ok_or_else(|| AppError::conflict("工作区保存回执已过期"))?;
        self.in_flight_save = None;
        match outcome {
            WorkspaceSessionSaveOutcome::Saved { revision } => {
                self.base_document = request.candidate_document;
                self.base_revision = WorkspaceSessionExpectedRevision::Revision(revision);
                self.base_snapshot = request.desired_snapshot;
                self.clean_working_snapshot = request.source_snapshot;
                self.refresh_save_state();
            }
            WorkspaceSessionSaveOutcome::Conflict { latest } => {
                self.save_state = WorkspacePersistenceState::Conflict {
                    latest: Box::new(latest),
                };
            }
            WorkspaceSessionSaveOutcome::OutcomeUnknown => {
                self.save_state = WorkspacePersistenceState::OutcomeUnknown {
                    request: Box::new(request),
                };
            }
        }
        Ok(())
    }

    /// 确定失败/取消只恢复 dirty 状态；提交未知必须调用 resolve_unknown_readback。
    pub fn finish_save_failure(&mut self, request_id: &str) -> AppResult<()> {
        let active = self
            .in_flight_save
            .as_ref()
            .is_some_and(|request| request.request_id == request_id);
        if !active {
            return Err(AppError::conflict("工作区保存回执已过期"));
        }
        self.in_flight_save = None;
        self.save_state = WorkspacePersistenceState::Failed;
        Ok(())
    }

    /// 未知提交后按投影 desired 显式回读；确认只更新磁盘基线，不替换界面中的完整运行态。
    pub fn resolve_unknown_readback(&mut self, latest: WorkspaceSessionLoad) -> AppResult<()> {
        let WorkspacePersistenceState::OutcomeUnknown { request } = &self.save_state else {
            return Err(AppError::conflict("当前没有待确认的工作区提交结果"));
        };
        let request = request.as_ref().clone();
        match &latest {
            WorkspaceSessionLoad::Loaded { document, revision } => {
                let latest_snapshot = document.workspace_snapshot()?;
                if latest_snapshot == request.desired_snapshot {
                    self.adopt_baseline(
                        document.clone(),
                        WorkspaceSessionExpectedRevision::Revision(revision.clone()),
                        latest_snapshot.clone(),
                    );
                    self.clean_working_snapshot = request.source_snapshot.clone();
                    self.refresh_save_state();
                    return Ok(());
                }
                if latest_snapshot == request.base_snapshot {
                    self.adopt_baseline(
                        document.clone(),
                        WorkspaceSessionExpectedRevision::Revision(revision.clone()),
                        latest_snapshot.clone(),
                    );
                    self.clean_working_snapshot = latest_snapshot;
                    self.save_state = WorkspacePersistenceState::ReadbackConfirmedUncommitted {
                        latest: Box::new(latest),
                    };
                    return Ok(());
                }
            }
            WorkspaceSessionLoad::Missing
                if request.expected_revision == WorkspaceSessionExpectedRevision::Missing =>
            {
                self.save_state = WorkspacePersistenceState::ReadbackConfirmedUncommitted {
                    latest: Box::new(latest),
                };
                return Ok(());
            }
            _ => {}
        }
        self.save_state = WorkspacePersistenceState::Conflict {
            latest: Box::new(latest),
        };
        Ok(())
    }

    /// 用户确认覆盖冲突时采用最新 revision 作 CAS 基线，仍不会绕过并发竞态检查。
    pub fn confirm_overwrite_latest(&mut self) -> AppResult<()> {
        let latest = match &self.save_state {
            WorkspacePersistenceState::Conflict { latest }
            | WorkspacePersistenceState::ReadbackConfirmedUncommitted { latest } => {
                latest.as_ref().clone()
            }
            _ => return Err(AppError::conflict("没有可确认覆盖的工作区冲突")),
        };
        let (document, revision) = match latest {
            WorkspaceSessionLoad::Loaded { document, revision } => (
                document,
                WorkspaceSessionExpectedRevision::Revision(revision),
            ),
            WorkspaceSessionLoad::Missing => (
                WorkspaceSessionDocument::empty_v3(),
                WorkspaceSessionExpectedRevision::Missing,
            ),
            _ => return Err(AppError::conflict("最新工作区不可安全覆盖，请先重新载入")),
        };
        let latest_snapshot = document.workspace_snapshot()?;
        self.adopt_baseline(document, revision, latest_snapshot.clone());
        self.clean_working_snapshot = latest_snapshot;
        self.refresh_save_state();
        Ok(())
    }

    /// 用户显式放弃当前变更并重载完整有效快照，不将未知字段从旧基线带回磁盘。
    pub fn reload_latest(&mut self) -> AppResult<()> {
        let latest = match &self.save_state {
            WorkspacePersistenceState::Conflict { latest }
            | WorkspacePersistenceState::ReadbackConfirmedUncommitted { latest } => {
                latest.as_ref().clone()
            }
            _ => return Err(AppError::conflict("没有可重载的工作区冲突")),
        };
        let (document, revision) = match latest {
            WorkspaceSessionLoad::Loaded { document, revision } => (
                document,
                WorkspaceSessionExpectedRevision::Revision(revision),
            ),
            WorkspaceSessionLoad::Missing => (
                WorkspaceSessionDocument::empty_v3(),
                WorkspaceSessionExpectedRevision::Missing,
            ),
            _ => return Err(AppError::conflict("最新工作区没有可恢复的 v3 快照")),
        };
        let snapshot = document.workspace_snapshot()?;
        self.adopt_baseline(document, revision, snapshot.clone());
        self.snapshot = snapshot.clone();
        self.clean_working_snapshot = snapshot.clone();
        self.restore_status = initial_restore_status(&snapshot);
        self.pending_resource = None;
        self.generation = self.generation.wrapping_add(1);
        self.save_state = WorkspacePersistenceState::Clean;
        Ok(())
    }

    /// 确认已回读未提交后显式允许用户发起下一次 CAS，不在 OutcomeUnknown 分支自动重试。
    pub fn retry_after_readback(&mut self) -> AppResult<()> {
        if !matches!(
            &self.save_state,
            WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
        ) {
            return Err(AppError::conflict("尚未完成未知提交的回读确认"));
        }
        self.refresh_save_state();
        Ok(())
    }

    /// 从已确认的最新文档重设 CAS 基线，工作状态可保留以供显式覆盖。
    fn adopt_baseline(
        &mut self,
        document: WorkspaceSessionDocument,
        revision: WorkspaceSessionExpectedRevision,
        snapshot: WorkspaceSessionSnapshot,
    ) {
        self.base_document = document;
        self.base_revision = revision;
        self.base_snapshot = snapshot;
    }

    /// 用户选择 Tab 后同步 selectedMachine 和 terminal focus；文件类 tab 不保留陈旧 pane focus。
    fn activate_tab(&mut self, tab_id: &TabId) -> AppResult<WorkspaceControllerEffect> {
        let tab = self
            .snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
            .cloned()
            .ok_or_else(|| AppError::not_found("工作区标签不存在"))?;
        let old = (
            self.snapshot.active_tab_id.clone(),
            self.snapshot.focused_pane_id.clone(),
            self.snapshot.selected_machine_id.clone(),
        );
        self.select_tab(&tab);
        self.changed_if_selection_differs(old)
    }

    /// 用户聚焦 pane 时同时选中其 Tab 和 machine，避免 Tab/Pane/目标指向不同会话。
    fn activate_pane(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
    ) -> AppResult<WorkspaceControllerEffect> {
        let tab = self.terminal_tab(tab_id)?.clone();
        let layout = tab
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签没有可聚焦的 pane"))?;
        if !layout.contains_pane(pane_id) {
            return Err(AppError::not_found("终端 pane 不属于当前标签"));
        }
        let pane = self.pane(pane_id)?.clone();
        let old = (
            self.snapshot.active_tab_id.clone(),
            self.snapshot.focused_pane_id.clone(),
            self.snapshot.selected_machine_id.clone(),
        );
        self.snapshot.active_tab_id = Some(tab_id.clone());
        self.snapshot.focused_pane_id = Some(pane_id.clone());
        self.snapshot.selected_machine_id = Some(pane.machine_id);
        self.changed_if_selection_differs(old)
    }

    /// Split 复制明确来源 pane 的连接参数，生成新身份并等待 Root 为其创建独立 session。
    fn begin_split(
        &mut self,
        tab_id: &TabId,
        source_pane_id: &PaneId,
        direction: SplitDirection,
        before: bool,
    ) -> AppResult<WorkspaceControllerEffect> {
        let tab = self.terminal_tab(tab_id)?.clone();
        let layout = tab
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签不支持分屏"))?;
        let source = self.pane(source_pane_id)?.clone();
        if !layout.contains_pane(source_pane_id) {
            return Err(AppError::not_found("终端 pane 不属于当前标签"));
        }
        if !matches!(
            source.target.as_ref(),
            Some(
                WorkspaceTargetRef::Local { .. }
                    | WorkspaceTargetRef::Ssh { .. }
                    | WorkspaceTargetRef::Telnet { .. }
                    | WorkspaceTargetRef::Serial { .. }
                    | WorkspaceTargetRef::DockerContainer { .. }
            )
        ) {
            return Err(AppError::unsupported("当前 pane 目标不可用于创建分屏"));
        }
        self.begin_split_with_pane(tab_id, source_pane_id, source, direction, before)
    }

    /// 分屏目标由 Root 显式确认；仅接受当前恢复管线能无歧义解析的本地 Profile 或终端主机。
    ///
    /// @author kongweiguang
    pub fn begin_split_for_target(
        &mut self,
        tab_id: &TabId,
        source_pane_id: &PaneId,
        target: WorkspaceTargetRef,
        title: String,
        direction: SplitDirection,
        before: bool,
    ) -> AppResult<WorkspaceControllerEffect> {
        self.ensure_no_pending_resource()?;
        let tab = self.terminal_tab(tab_id)?.clone();
        let layout = tab
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签不支持分屏"))?;
        if !layout.contains_pane(source_pane_id) {
            return Err(AppError::not_found("终端 pane 不属于当前标签"));
        }
        let source = self.pane(source_pane_id)?.clone();
        let pane = Self::pane_for_explicit_split_target(source, target, title)?;
        self.begin_split_with_pane(tab_id, source_pane_id, pane, direction, before)
    }

    /// 两种分屏意图共用相同的 stable ID、布局候选和 CreatePane ACK 门禁，避免重复资源状态机。
    ///
    /// @author kongweiguang
    fn begin_split_with_pane(
        &mut self,
        tab_id: &TabId,
        source_pane_id: &PaneId,
        mut new_pane: WorkspaceSessionPane,
        direction: SplitDirection,
        before: bool,
    ) -> AppResult<WorkspaceControllerEffect> {
        let tab = self.terminal_tab(tab_id)?.clone();
        let layout = tab
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签不支持分屏"))?;
        if !layout.contains_pane(source_pane_id) {
            return Err(AppError::not_found("终端 pane 不属于当前标签"));
        }
        let new_pane_id = PaneId::new();
        let split_id = format!("split-{}", uuid::Uuid::new_v4());
        let next_layout = self.workspace_service().split_pane(
            layout,
            source_pane_id,
            new_pane_id.clone(),
            direction,
            split_id,
            before,
        )?;
        let mut candidate = self.snapshot.clone();
        let Some(WorkspaceSessionTab::Terminal { layout, .. }) = candidate
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("终端标签不存在"));
        };
        *layout = next_layout;
        new_pane.id = new_pane_id.clone();
        new_pane.status = WorkspaceMachineStatus::Offline;
        new_pane.latency_ms = None;
        new_pane.tmux_binding = None;
        new_pane.output_history = None;
        candidate.terminal_panes.push(new_pane.clone());
        candidate.active_tab_id = Some(tab_id.clone());
        candidate.focused_pane_id = Some(new_pane_id);
        candidate.selected_machine_id = Some(new_pane.machine_id.clone());
        let operation = WorkspaceResourceOperation::CreatePane {
            operation_id: uuid::Uuid::new_v4().to_string(),
            tab_id: tab_id.clone(),
            source_pane_id: source_pane_id.clone(),
            pane: Box::new(new_pane),
        };
        self.pending_resource = Some(PendingResourceChange {
            operation: operation.clone(),
            candidate,
            failed: None,
        });
        Ok(WorkspaceControllerEffect::ResourceOperation(operation))
    }

    /// 新目标不沿用来源 pane 的 shell、cwd、tmux 或输出正文，避免把旧连接参数施加到另一主机。
    ///
    /// @author kongweiguang
    fn pane_for_explicit_split_target(
        mut pane: WorkspaceSessionPane,
        target: WorkspaceTargetRef,
        title: String,
    ) -> AppResult<WorkspaceSessionPane> {
        let (machine_id, mode, remote_host_id, profile_id, local_machine_scope) = match &target {
            WorkspaceTargetRef::Local {
                profile_id: Some(profile_id),
            } if !profile_id.trim().is_empty() => (
                "machine-local".to_owned(),
                WorkspacePaneMode::Local,
                None,
                Some(profile_id.clone()),
                Some(WorkspaceLocalMachineScope::Sidebar),
            ),
            WorkspaceTargetRef::Ssh { host_id } => (
                host_id.as_str().to_owned(),
                WorkspacePaneMode::Ssh,
                Some(host_id.clone()),
                None,
                None,
            ),
            WorkspaceTargetRef::Telnet { host_id } => (
                host_id.as_str().to_owned(),
                WorkspacePaneMode::Telnet,
                Some(host_id.clone()),
                None,
                None,
            ),
            WorkspaceTargetRef::Serial { host_id } => (
                host_id.as_str().to_owned(),
                WorkspacePaneMode::Serial,
                Some(host_id.clone()),
                None,
                None,
            ),
            _ => {
                return Err(AppError::unsupported(
                    "分屏目标必须是本地 Profile、SSH、Telnet 或 Serial 主机",
                ));
            }
        };
        pane.title = if title.trim().is_empty() {
            "Terminal".to_owned()
        } else {
            title
        };
        pane.machine_id = machine_id;
        pane.mode = mode;
        pane.target = Some(target);
        pane.remote_host_id = remote_host_id;
        pane.container_id = None;
        pane.profile_id = profile_id;
        pane.shell = None;
        pane.args = None;
        pane.cwd = None;
        pane.current_cwd = None;
        pane.env = None;
        pane.remote_command = None;
        pane.tmux_binding = None;
        pane.prompt.clear();
        pane.status = WorkspaceMachineStatus::Offline;
        pane.latency_ms = None;
        pane.local_machine_scope = local_machine_scope;
        pane.output_history = None;
        Ok(pane)
    }

    /// 关闭标签候选先移除全部 pane/目标引用，等 Root 释放 tab 的 live 资源后才应用。
    fn begin_close_tab(&mut self, tab_id: &TabId) -> AppResult<WorkspaceControllerEffect> {
        let index = self
            .snapshot
            .terminal_tabs
            .iter()
            .position(|tab| tab.id() == tab_id)
            .ok_or_else(|| AppError::not_found("工作区标签不存在"))?;
        let tab = self.snapshot.terminal_tabs[index].clone();
        let pane_ids = tab
            .terminal_layout()
            .map(TerminalLayoutNode::pane_ids)
            .unwrap_or_default();
        let mut candidate = self.snapshot.clone();
        remove_tab_from_snapshot(&mut candidate, index);
        if let Some(PendingTabCloseBatch {
            intent: WorkspaceCloseBatchIntent::Group { group_id, .. },
            ..
        }) = &self.close_batch
            && !candidate
                .terminal_tabs
                .iter()
                .any(|tab| tab.tab_group_id().is_some_and(|id| id.as_str() == group_id))
        {
            candidate
                .tab_groups
                .remove(&WorkspaceTabGroupId::parse(group_id)?);
        }
        let operation = WorkspaceResourceOperation::CloseTab {
            operation_id: uuid::Uuid::new_v4().to_string(),
            tab_id: tab_id.clone(),
            tab: Box::new(tab),
            pane_ids,
        };
        self.pending_resource = Some(PendingResourceChange {
            operation: operation.clone(),
            candidate,
            failed: None,
        });
        Ok(WorkspaceControllerEffect::ResourceOperation(operation))
    }

    /// 关闭 pane 先通过领域 layout 校验；最后一个 pane 等价关闭整个终端标签。
    fn begin_close_pane(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
    ) -> AppResult<WorkspaceControllerEffect> {
        let tab = self.terminal_tab(tab_id)?.clone();
        let layout = tab
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签没有 pane"))?;
        if !layout.contains_pane(pane_id) {
            return Err(AppError::not_found("终端 pane 不属于当前标签"));
        }
        let candidate_layout = self.workspace_service().close_pane(layout, pane_id)?;
        let Some(candidate_layout) = candidate_layout else {
            return self.begin_close_tab(tab_id);
        };
        let mut candidate = self.snapshot.clone();
        let Some(WorkspaceSessionTab::Terminal { layout, .. }) = candidate
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("终端标签不存在"));
        };
        *layout = candidate_layout;
        candidate.terminal_panes.retain(|pane| &pane.id != pane_id);
        if candidate.focused_pane_id.as_ref() == Some(pane_id) {
            candidate.focused_pane_id = layout.pane_ids().first().cloned();
            candidate.active_tab_id = Some(tab_id.clone());
            if let Some(focused) = candidate.focused_pane_id.as_ref()
                && let Some(pane) = candidate
                    .terminal_panes
                    .iter()
                    .find(|pane| &pane.id == focused)
            {
                candidate.selected_machine_id = Some(pane.machine_id.clone());
            }
        }
        let operation = WorkspaceResourceOperation::ClosePane {
            operation_id: uuid::Uuid::new_v4().to_string(),
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        self.pending_resource = Some(PendingResourceChange {
            operation: operation.clone(),
            candidate,
            failed: None,
        });
        Ok(WorkspaceControllerEffect::ResourceOperation(operation))
    }

    /// 重排目标索引定义为移动完成后的全局数组索引，保留 v3 数组作为唯一顺序来源。
    fn reorder_tab(
        &mut self,
        tab_id: &TabId,
        target_index: usize,
    ) -> AppResult<WorkspaceControllerEffect> {
        let ordered_indices = ordered_tab_indices(&self.snapshot);
        let Some(current_index) = ordered_indices
            .iter()
            .position(|index| self.snapshot.terminal_tabs[*index].id() == tab_id)
        else {
            return Err(AppError::not_found("工作区标签不存在"));
        };
        let mut candidate = self.snapshot.clone();
        let mut ordered_tabs = ordered_indices
            .into_iter()
            .map(|index| candidate.terminal_tabs[index].clone())
            .collect::<Vec<_>>();
        let tab = ordered_tabs.remove(current_index);
        let destination = target_index.min(ordered_tabs.len());
        if current_index == destination {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        ordered_tabs.insert(destination, tab);
        candidate.terminal_tabs = ordered_tabs;
        self.commit_local_change(candidate)
    }

    /// 根据稳定 TabId 与捕获组 ID 交换相邻成员，避免菜单延迟把索引动作施加到新目标。
    /// 仅改持久标签顺序，不触碰 active/focused 状态、PaneId、SessionId 或 Agent target。
    ///
    /// @author kongweiguang
    fn move_tab_within_group(
        &mut self,
        tab_id: &TabId,
        raw_group_id: &str,
        direction: GroupTabMoveDirection,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        if !self.snapshot.tab_groups.contains_key(&group_id) {
            return Err(AppError::not_found("工作区标签组不存在"));
        }
        let ordered_indices = ordered_tab_indices(&self.snapshot);
        let Some(source_index) = ordered_indices
            .iter()
            .copied()
            .find(|index| self.snapshot.terminal_tabs[*index].id() == tab_id)
        else {
            return Err(AppError::not_found("工作区标签不存在"));
        };
        if self.snapshot.terminal_tabs[source_index].tab_group_id() != Some(&group_id) {
            return Err(AppError::conflict("标签已不属于原分组，请重新打开菜单"));
        }

        let group_indices = ordered_indices
            .into_iter()
            .filter(|index| self.snapshot.terminal_tabs[*index].tab_group_id() == Some(&group_id))
            .collect::<Vec<_>>();
        let Some(position) = group_indices
            .iter()
            .position(|index| *index == source_index)
        else {
            return Err(AppError::not_found("组内标签已不存在"));
        };
        let target_position = match direction {
            GroupTabMoveDirection::Earlier => position.checked_sub(1),
            GroupTabMoveDirection::Later => group_indices.get(position + 1).map(|_| position + 1),
        };
        let Some(target_position) = target_position else {
            return Ok(WorkspaceControllerEffect::Unchanged);
        };
        let target_index = group_indices[target_position];
        let mut candidate = self.snapshot.clone();
        candidate.terminal_tabs.swap(source_index, target_index);
        self.commit_local_change(candidate)
    }

    /// 按稳定目标身份和显式前后落点完成重排；移除源标签后重新解析目标，避免旧索引偏移。
    /// 跨组时继承目标归属并仅清理失去最后成员的源组，不触碰活动选择或 Root 持有的会话资源。
    ///
    /// @author kongweiguang
    fn drop_tab_on_tab(
        &mut self,
        tab_id: &TabId,
        target_tab_id: &TabId,
        placement: TabDropPlacement,
    ) -> AppResult<WorkspaceControllerEffect> {
        if tab_id == target_tab_id {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }

        let ordered_indices = ordered_tab_indices(&self.snapshot);
        let Some(source_index) = ordered_indices
            .iter()
            .position(|index| self.snapshot.terminal_tabs[*index].id() == tab_id)
        else {
            return Err(AppError::not_found("工作区拖放源标签不存在"));
        };
        let Some(target_index) = ordered_indices
            .iter()
            .position(|index| self.snapshot.terminal_tabs[*index].id() == target_tab_id)
        else {
            return Err(AppError::not_found("工作区拖放目标标签不存在"));
        };

        let source_group_id = self.snapshot.terminal_tabs[ordered_indices[source_index]]
            .tab_group_id()
            .cloned();
        let target_group_id = self.snapshot.terminal_tabs[ordered_indices[target_index]]
            .tab_group_id()
            .cloned();
        if target_group_id
            .as_ref()
            .is_some_and(|group_id| !self.snapshot.tab_groups.contains_key(group_id))
        {
            return Err(AppError::not_found("工作区拖放目标分组不存在"));
        }

        let mut candidate = self.snapshot.clone();
        let mut ordered_tabs = ordered_indices
            .into_iter()
            .map(|index| candidate.terminal_tabs[index].clone())
            .collect::<Vec<_>>();
        let mut source_tab = ordered_tabs.remove(source_index);
        let Some(current_target_index) = ordered_tabs
            .iter()
            .position(|tab| tab.id() == target_tab_id)
        else {
            return Err(AppError::not_found("工作区拖放目标标签已失效"));
        };

        set_tab_group(&mut source_tab, target_group_id.clone());
        let destination = match placement {
            TabDropPlacement::Before => current_target_index,
            TabDropPlacement::After => current_target_index + 1,
        };
        ordered_tabs.insert(destination, source_tab);
        candidate.terminal_tabs = ordered_tabs;

        if let Some(source_group_id) = source_group_id
            && Some(&source_group_id) != target_group_id.as_ref()
            && !candidate
                .terminal_tabs
                .iter()
                .any(|tab| tab.tab_group_id() == Some(&source_group_id))
        {
            candidate.tab_groups.remove(&source_group_id);
        }

        self.commit_local_change(candidate)
    }

    /// 将标签原子移入未分组区末尾；基于投影顺序重建快照，只改组归属和顺序。
    /// 尾部未分组源视为无操作，空源组随最后成员离开而清理；资源与选择身份保持快照原值。
    ///
    /// @author kongweiguang
    fn move_tab_to_ungrouped_end(
        &mut self,
        tab_id: &TabId,
    ) -> AppResult<WorkspaceControllerEffect> {
        let ordered_indices = ordered_tab_indices(&self.snapshot);
        let Some(source_position) = ordered_indices
            .iter()
            .position(|index| self.snapshot.terminal_tabs[*index].id() == tab_id)
        else {
            return Err(AppError::not_found("工作区末尾拖放源标签不存在"));
        };
        let source_index = ordered_indices[source_position];
        let source_group_id = self.snapshot.terminal_tabs[source_index]
            .tab_group_id()
            .cloned();
        if source_group_id.is_none() && source_position + 1 == ordered_indices.len() {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }

        let mut candidate = self.snapshot.clone();
        let mut ordered_tabs = ordered_indices
            .into_iter()
            .map(|index| candidate.terminal_tabs[index].clone())
            .collect::<Vec<_>>();
        let mut source_tab = ordered_tabs.remove(source_position);
        set_tab_group(&mut source_tab, None);
        ordered_tabs.push(source_tab);
        candidate.terminal_tabs = ordered_tabs;

        if let Some(source_group_id) = source_group_id
            && !candidate
                .terminal_tabs
                .iter()
                .any(|tab| tab.tab_group_id() == Some(&source_group_id))
        {
            candidate.tab_groups.remove(&source_group_id);
        }

        self.commit_local_change(candidate)
    }

    /// 拖入已有 group 时只改稳定 group ID，不在 UI 层创建没有定义的组模型。
    fn move_tab_to_group(
        &mut self,
        tab_id: &TabId,
        raw_group_id: Option<&str>,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = raw_group_id.map(WorkspaceTabGroupId::parse).transpose()?;
        if group_id
            .as_ref()
            .is_some_and(|group_id| !self.snapshot.tab_groups.contains_key(group_id))
        {
            return Err(AppError::not_found("工作区标签组不存在"));
        }
        let Some(tab) = self
            .snapshot
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("工作区标签不存在"));
        };
        if tab.tab_group_id() == group_id.as_ref() {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        set_tab_group(tab, group_id);
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 新组创建与首个成员一起原子提交，避免持久化一个没有可操作入口的孤立分组。
    fn create_tab_group_for_tab(
        &mut self,
        tab_id: &TabId,
        raw_title: &str,
    ) -> AppResult<WorkspaceControllerEffect> {
        let title = validate_workspace_title(raw_title)?;
        if !self
            .snapshot
            .terminal_tabs
            .iter()
            .any(|tab| tab.id() == tab_id)
        {
            return Err(AppError::not_found("创建分组的标签已不存在"));
        }
        ensure_unique_group_title(&self.snapshot, &title, None)?;
        let group_id = loop {
            let candidate = format!("group-{}", uuid::Uuid::new_v4());
            let parsed = WorkspaceTabGroupId::parse(&candidate)
                .map_err(|_| AppError::invalid("无法生成工作区分组 ID"))?;
            if !self.snapshot.tab_groups.contains_key(&parsed) {
                break parsed;
            }
        };
        self.snapshot.tab_groups.insert(
            group_id.clone(),
            WorkspaceTabGroup {
                title,
                color: None,
                collapsed: false,
            },
        );
        let Some(tab) = self
            .snapshot
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("创建分组的标签已不存在"));
        };
        set_tab_group(tab, Some(group_id));
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 标签改名只改 v3 已知 title 字段，组、布局和其他类型专属元数据保持原样。
    fn rename_tab(
        &mut self,
        tab_id: &TabId,
        raw_title: &str,
    ) -> AppResult<WorkspaceControllerEffect> {
        let title = validate_workspace_title(raw_title)?;
        let Some(tab) = self
            .snapshot
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("工作区标签不存在"));
        };
        if tab.title() == title {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        set_tab_title(tab, title);
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 组改名沿用唯一名称规则并保留颜色、折叠状态和未知 v3 原始字段。
    fn rename_tab_group(
        &mut self,
        raw_group_id: &str,
        raw_title: &str,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        let title = validate_workspace_title(raw_title)?;
        ensure_unique_group_title(&self.snapshot, &title, Some(&group_id))?;
        let group = self
            .snapshot
            .tab_groups
            .get_mut(&group_id)
            .ok_or_else(|| AppError::not_found("工作区标签组不存在"))?;
        if group.title == title {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        group.title = title;
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 颜色只在显式选择时改写 v3 color 字段，重命名不会擦除未知颜色。
    fn set_tab_group_color(
        &mut self,
        raw_group_id: &str,
        color: Option<WorkspaceTabGroupColor>,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        let group = self
            .snapshot
            .tab_groups
            .get_mut(&group_id)
            .ok_or_else(|| AppError::not_found("工作区标签组不存在"))?;
        if group.color == color {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        group.color = color;
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 折叠状态属于现有 v3 字段，视图更新不会重建标签或会话资源。
    fn set_tab_group_collapsed(
        &mut self,
        raw_group_id: &str,
        collapsed: bool,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        let group = self
            .snapshot
            .tab_groups
            .get_mut(&group_id)
            .ok_or_else(|| AppError::not_found("工作区标签组不存在"))?;
        if group.collapsed == collapsed {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        group.collapsed = collapsed;
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 组顺序由 v3 terminalTabs 首次出现位置表达，只移动非空组并保留每组内部顺序。
    fn move_tab_group(
        &mut self,
        raw_group_id: &str,
        raw_before_group_id: Option<&str>,
    ) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        let order = ordered_tab_group_ids(&self.snapshot)
            .into_iter()
            .filter(|id| {
                self.snapshot.terminal_tabs.iter().any(|tab| {
                    tab.tab_group_id()
                        .is_some_and(|tab_group_id| tab_group_id == id)
                })
            })
            .collect::<Vec<_>>();
        let Some(source_index) = order.iter().position(|id| id == &group_id) else {
            return Err(AppError::not_found("工作区标签组不存在"));
        };
        let before_group_id = raw_before_group_id
            .map(WorkspaceTabGroupId::parse)
            .transpose()?;
        let mut target_order = order.clone();
        target_order.remove(source_index);
        let destination = match &before_group_id {
            Some(before) if before == &group_id => return Ok(WorkspaceControllerEffect::Unchanged),
            Some(before) => {
                if !self.snapshot.tab_groups.contains_key(before) {
                    return Err(AppError::not_found("目标标签组不存在"));
                }
                target_order
                    .iter()
                    .position(|candidate| candidate == before)
                    .ok_or_else(|| AppError::conflict("空标签组没有可持久化的重排位置"))?
            }
            None => target_order.len(),
        };
        target_order.insert(destination, group_id);
        if target_order == order {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }

        let mut grouped = target_order
            .iter()
            .map(|id| (id.clone(), Vec::new()))
            .collect::<BTreeMap<_, _>>();
        let mut ungrouped = Vec::new();
        for tab in std::mem::take(&mut self.snapshot.terminal_tabs) {
            match tab.tab_group_id().cloned() {
                Some(id) => grouped.entry(id).or_default().push(tab),
                None => ungrouped.push(tab),
            }
        }
        self.snapshot.terminal_tabs = target_order
            .iter()
            .flat_map(|id| grouped.remove(id).unwrap_or_default())
            .chain(ungrouped)
            .collect();
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 解组会同时去除成员关联和组元数据，空组也可用同一显式动作删除。
    fn ungroup_tabs(&mut self, raw_group_id: &str) -> AppResult<WorkspaceControllerEffect> {
        let group_id = WorkspaceTabGroupId::parse(raw_group_id)?;
        if !self.snapshot.tab_groups.contains_key(&group_id) {
            return Err(AppError::not_found("工作区标签组不存在"));
        }
        for tab in &mut self.snapshot.terminal_tabs {
            if tab.tab_group_id() == Some(&group_id) {
                set_tab_group(tab, None);
            }
        }
        self.snapshot.tab_groups.remove(&group_id);
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 批量关闭拒绝重复、过期或不符合点击时语义的目标，之后只逐项复用 CloseTab ACK。
    fn begin_close_batch(
        &mut self,
        intent: WorkspaceCloseBatchIntent,
    ) -> AppResult<WorkspaceControllerEffect> {
        let targets = intent.tab_ids();
        if targets
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != targets.len()
        {
            return Err(AppError::invalid("批量关闭目标包含重复标签"));
        }
        validate_close_batch_intent(&self.snapshot, &intent)?;
        if targets.is_empty() {
            if let WorkspaceCloseBatchIntent::Group { group_id, .. } = intent {
                let parsed = WorkspaceTabGroupId::parse(&group_id)?;
                self.snapshot
                    .tab_groups
                    .remove(&parsed)
                    .ok_or_else(|| AppError::not_found("工作区标签组不存在"))?;
                self.mark_changed();
                return Ok(WorkspaceControllerEffect::Changed {
                    generation: self.generation,
                });
            }
            return Ok(WorkspaceControllerEffect::Unchanged);
        }

        let first_tab_id = targets[0].clone();
        self.close_batch = Some(PendingTabCloseBatch {
            intent,
            next_index: 0,
        });
        match self.begin_close_tab(&first_tab_id) {
            Ok(effect) => Ok(effect),
            Err(error) => {
                self.close_batch = None;
                Err(error)
            }
        }
    }

    /// Resize 仅提交领域 layout 接受的直接 child 比例，组件的瞬时拖动值不直接写文件。
    fn resize_split(
        &mut self,
        tab_id: &TabId,
        split_id: &str,
        sizes: kerminal_domain::workspace::SplitLayoutSizes,
    ) -> AppResult<WorkspaceControllerEffect> {
        let layout = self
            .terminal_tab(tab_id)?
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签没有 split"))?;
        let candidate_layout = self
            .workspace_service()
            .resize_split(layout, split_id, sizes)?;
        let mut candidate = self.snapshot.clone();
        let Some(WorkspaceSessionTab::Terminal { layout, .. }) = candidate
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("终端标签不存在"));
        };
        *layout = candidate_layout;
        self.commit_local_change(candidate)
    }

    /// 交换仅改变布局叶子映射，pane 对应的 session 和 target 仍由 Root 原样保留。
    fn swap_panes(
        &mut self,
        tab_id: &TabId,
        first: &PaneId,
        second: &PaneId,
    ) -> AppResult<WorkspaceControllerEffect> {
        let layout = self
            .terminal_tab(tab_id)?
            .terminal_layout()
            .ok_or_else(|| AppError::invalid("非终端标签没有 pane"))?;
        let candidate_layout = self.workspace_service().swap_panes(layout, first, second)?;
        let mut candidate = self.snapshot.clone();
        let Some(WorkspaceSessionTab::Terminal { layout, .. }) = candidate
            .terminal_tabs
            .iter_mut()
            .find(|tab| tab.id() == tab_id)
        else {
            return Err(AppError::not_found("终端标签不存在"));
        };
        *layout = candidate_layout;
        self.commit_local_change(candidate)
    }

    /// 激活任意标签时只计算聚合字段；实体和资源实际重建顺序留给 Root。
    fn select_tab(&mut self, tab: &WorkspaceSessionTab) {
        self.snapshot.active_tab_id = Some(tab.id().clone());
        match tab {
            WorkspaceSessionTab::Terminal {
                layout, machine_id, ..
            } => {
                let focus = self
                    .snapshot
                    .focused_pane_id
                    .as_ref()
                    .filter(|focused| layout.contains_pane(focused))
                    .cloned()
                    .or_else(|| layout.pane_ids().first().cloned());
                self.snapshot.focused_pane_id = focus;
                self.snapshot.selected_machine_id = self
                    .snapshot
                    .focused_pane_id
                    .as_ref()
                    .and_then(|focused| {
                        self.snapshot
                            .terminal_panes
                            .iter()
                            .find(|pane| &pane.id == focused)
                    })
                    .map(|pane| pane.machine_id.clone())
                    .or_else(|| Some(machine_id.clone()));
            }
            WorkspaceSessionTab::SftpTransfer { machine_id, .. }
            | WorkspaceSessionTab::WorkspaceFile { machine_id, .. } => {
                self.snapshot.focused_pane_id = None;
                self.snapshot.selected_machine_id = Some(machine_id.clone());
            }
            WorkspaceSessionTab::Unsupported { .. } => {
                self.snapshot.focused_pane_id = None;
                self.snapshot.selected_machine_id = None;
            }
        }
    }

    /// 统一同步本地候选，codec 的全量 schema 验证在任何持久化前再次执行。
    fn commit_local_change(
        &mut self,
        candidate: WorkspaceSessionSnapshot,
    ) -> AppResult<WorkspaceControllerEffect> {
        candidate
            .validate()
            .map_err(|_| AppError::invalid("工作区变更无法通过快照验证"))?;
        if candidate == self.snapshot {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        self.snapshot = candidate;
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 选择字段变化即属于持久化状态，避免重启后活动标签退回旧位置。
    fn changed_if_selection_differs(
        &mut self,
        old: (Option<TabId>, Option<PaneId>, Option<String>),
    ) -> AppResult<WorkspaceControllerEffect> {
        if old
            == (
                self.snapshot.active_tab_id.clone(),
                self.snapshot.focused_pane_id.clone(),
                self.snapshot.selected_machine_id.clone(),
            )
        {
            return Ok(WorkspaceControllerEffect::Unchanged);
        }
        self.mark_changed();
        Ok(WorkspaceControllerEffect::Changed {
            generation: self.generation,
        })
    }

    /// 每次已提交的本地 reducer 变更增加 generation，并解除上一次确定失败的保存状态。
    fn mark_changed(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.save_state = if self.in_flight_save.is_some()
            || matches!(
                &self.save_state,
                WorkspacePersistenceState::Conflict { .. }
                    | WorkspacePersistenceState::OutcomeUnknown { .. }
                    | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
            ) {
            self.save_state.clone()
        } else if self.snapshot == self.clean_working_snapshot {
            WorkspacePersistenceState::Clean
        } else {
            WorkspacePersistenceState::Dirty
        };
    }

    /// 保存回执后比较当前完整快照和请求时快照，避免清掉保存期间产生的新变更。
    fn refresh_save_state(&mut self) {
        if self.snapshot == self.clean_working_snapshot {
            self.save_state = WorkspacePersistenceState::Clean;
        } else {
            self.save_state = WorkspacePersistenceState::Dirty;
        }
    }

    /// 资源变更只允许一个 in-flight 候选，避免旧创建回执覆盖新 split/layout。
    fn ensure_no_pending_resource(&self) -> AppResult<()> {
        if self.pending_resource.is_some() {
            return Err(AppError::conflict("请先完成或取消当前工作区资源操作"));
        }
        if matches!(
            &self.save_state,
            WorkspacePersistenceState::Conflict { .. }
                | WorkspacePersistenceState::OutcomeUnknown { .. }
                | WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
        ) {
            return Err(AppError::conflict("请先解决工作区保存冲突或回读结果"));
        }
        Ok(())
    }

    /// 限定 pane 查找范围在持久化表，避免组件凭空发明 session target。
    fn pane(&self, pane_id: &PaneId) -> AppResult<&WorkspaceSessionPane> {
        self.snapshot
            .terminal_panes
            .iter()
            .find(|pane| &pane.id == pane_id)
            .ok_or_else(|| AppError::not_found("工作区 pane 不存在"))
    }

    /// 限定 layout 命令只应用到确有递归树的 terminal tab。
    fn terminal_tab(&self, tab_id: &TabId) -> AppResult<&WorkspaceSessionTab> {
        self.snapshot
            .terminal_tabs
            .iter()
            .find(|tab| tab.id() == tab_id)
            .ok_or_else(|| AppError::not_found("工作区标签不存在"))
    }

    /// WorkspaceService 不持状态，临时实例只负责领域 split/resize/close 校验。
    fn workspace_service(&self) -> kerminal_application::workspace::WorkspaceService {
        kerminal_application::workspace::WorkspaceService::new()
    }

    /// 成功创建新 pane 后标为 ready，资源失败或关闭后删除对应的旧状态键。
    fn sync_restore_status_after_change(&mut self, completed: &WorkspaceResourceOperation) {
        self.reconcile_restore_status_after_runtime_sync();
        if let WorkspaceResourceOperation::CreatePane { pane, .. } = completed {
            self.restore_status.insert(
                WorkspaceRestoreItemId::Pane(pane.id.clone()),
                WorkspaceRestoreStatus::Ready,
            );
        }
    }

    /// 同步 Root 已确认的运行态时保留旧项状态，并将新建且受支持的资源标为 Ready。
    fn reconcile_restore_status_after_runtime_sync(&mut self) {
        let previous = std::mem::take(&mut self.restore_status);
        self.restore_status = initial_restore_status(&self.snapshot);
        for (item_id, status) in &mut self.restore_status {
            if let Some(previous_status) = previous.get(item_id) {
                *status = *previous_status;
            } else if *status
                != WorkspaceRestoreStatus::Failed(WorkspaceRestoreFailure::UnsupportedTab)
            {
                *status = WorkspaceRestoreStatus::Ready;
            }
        }
    }
}

/// 根据已验证目标初始化恢复状态；未知协议或缺少目标明确不可用，不回退到 machineId。
fn initial_restore_status(
    snapshot: &WorkspaceSessionSnapshot,
) -> BTreeMap<WorkspaceRestoreItemId, WorkspaceRestoreStatus> {
    let mut status = BTreeMap::new();
    for tab in &snapshot.terminal_tabs {
        match tab {
            WorkspaceSessionTab::Terminal { layout, .. } => {
                for pane_id in layout.pane_ids() {
                    let state = snapshot
                        .terminal_panes
                        .iter()
                        .find(|pane| pane.id == pane_id)
                        .map_or(
                            WorkspaceRestoreStatus::Failed(WorkspaceRestoreFailure::MissingTarget),
                            |pane| restore_status_for_target(pane.target.as_ref()),
                        );
                    status.insert(WorkspaceRestoreItemId::Pane(pane_id), state);
                }
            }
            WorkspaceSessionTab::WorkspaceFile { id, target, .. } => {
                status.insert(
                    WorkspaceRestoreItemId::Tab(id.clone()),
                    restore_status_for_target(Some(target)),
                );
            }
            WorkspaceSessionTab::Unsupported { id, .. } => {
                status.insert(
                    WorkspaceRestoreItemId::Tab(id.clone()),
                    WorkspaceRestoreStatus::Failed(WorkspaceRestoreFailure::UnsupportedTab),
                );
            }
            WorkspaceSessionTab::SftpTransfer { id, .. } => {
                status.insert(
                    WorkspaceRestoreItemId::Tab(id.clone()),
                    WorkspaceRestoreStatus::Pending,
                );
            }
        }
    }
    status
}

/// Local target 和历史 pane 缺少显式目标可由 Root 判定并回报，但不假定默认 host。
fn restore_status_for_target(target: Option<&WorkspaceTargetRef>) -> WorkspaceRestoreStatus {
    match target {
        Some(WorkspaceTargetRef::Unavailable { reason, .. }) => {
            WorkspaceRestoreStatus::Failed(WorkspaceRestoreFailure::UnavailableTarget(*reason))
        }
        Some(WorkspaceTargetRef::Local { .. })
        | Some(WorkspaceTargetRef::Ssh { .. })
        | Some(WorkspaceTargetRef::Telnet { .. })
        | Some(WorkspaceTargetRef::Serial { .. })
        | Some(WorkspaceTargetRef::DockerContainer { .. }) => WorkspaceRestoreStatus::Pending,
        None => WorkspaceRestoreStatus::Failed(WorkspaceRestoreFailure::MissingTarget),
    }
}

/// Root 只能把已创建资源的显式目标写入快照；缺失或未知目标不能伪装成 Ready。
fn validate_new_runtime_targets(
    previous: &WorkspaceSessionSnapshot,
    next: &WorkspaceSessionSnapshot,
    transient_local_container: Option<TransientLocalContainerBinding<'_>>,
) -> AppResult<()> {
    for pane in &next.terminal_panes {
        if previous
            .terminal_panes
            .iter()
            .any(|existing| existing.id == pane.id)
        {
            continue;
        }
        if transient_local_container
            .is_some_and(|binding| transient_local_container_matches(next, pane, binding))
        {
            continue;
        }
        if matches!(
            restore_status_for_target(pane.target.as_ref()),
            WorkspaceRestoreStatus::Failed(_)
        ) {
            return Err(AppError::invalid("新建终端 pane 必须有明确且可用的目标"));
        }
    }
    for tab in &next.terminal_tabs {
        if previous
            .terminal_tabs
            .iter()
            .any(|existing| existing.id() == tab.id())
        {
            continue;
        }
        if let WorkspaceSessionTab::WorkspaceFile {
            target: WorkspaceTargetRef::Unavailable { .. },
            ..
        } = tab
        {
            return Err(AppError::invalid("新建文件标签不能使用不可用目标"));
        }
    }
    Ok(())
}

/// 只接受由 Root 指定且归属一致的本机临时容器 pane；其它 target=None 新资源仍拒绝同步。
///
/// @author kongweiguang
fn transient_local_container_matches(
    snapshot: &WorkspaceSessionSnapshot,
    pane: &WorkspaceSessionPane,
    binding: TransientLocalContainerBinding<'_>,
) -> bool {
    !binding.container_id.is_empty()
        && pane.id == *binding.pane_id
        && pane.mode == WorkspacePaneMode::Container
        && pane.target.is_none()
        && pane.remote_host_id.is_none()
        && pane.container_id.as_deref() == Some(binding.container_id)
        && snapshot.terminal_tabs.iter().any(|tab| {
            tab.id() == binding.tab_id
                && tab
                    .terminal_layout()
                    .is_some_and(|layout| layout.contains_pane(binding.pane_id))
        })
}

/// 关闭当前标签后选择同一索引的后继；所有 pane 记录只从被关闭 layout 派生删除。
fn remove_tab_from_snapshot(snapshot: &mut WorkspaceSessionSnapshot, index: usize) {
    let tab = snapshot.terminal_tabs.remove(index);
    if let Some(layout) = tab.terminal_layout() {
        let removed = layout
            .pane_ids()
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        snapshot
            .terminal_panes
            .retain(|pane| !removed.contains(&pane.id));
    }
    let was_active = snapshot.active_tab_id.as_ref() == Some(tab.id());
    if was_active {
        let next = snapshot
            .terminal_tabs
            .get(index.min(snapshot.terminal_tabs.len().saturating_sub(1)))
            .cloned();
        if let Some(next) = next {
            set_active_tab(snapshot, &next);
        } else {
            snapshot.active_tab_id = None;
            snapshot.focused_pane_id = None;
            snapshot.selected_machine_id = None;
        }
    } else if snapshot.focused_pane_id.as_ref().is_some_and(|focused| {
        !snapshot
            .terminal_panes
            .iter()
            .any(|pane| &pane.id == focused)
    }) {
        snapshot.focused_pane_id = None;
    }
}

/// 激活后继 tab 时重新绑定焦点到其第一 pane，非终端 tab 不继承前一终端焦点。
fn set_active_tab(snapshot: &mut WorkspaceSessionSnapshot, tab: &WorkspaceSessionTab) {
    snapshot.active_tab_id = Some(tab.id().clone());
    match tab {
        WorkspaceSessionTab::Terminal {
            layout, machine_id, ..
        } => {
            snapshot.focused_pane_id = layout.pane_ids().first().cloned();
            snapshot.selected_machine_id = snapshot
                .focused_pane_id
                .as_ref()
                .and_then(|focused| {
                    snapshot
                        .terminal_panes
                        .iter()
                        .find(|pane| &pane.id == focused)
                })
                .map(|pane| pane.machine_id.clone())
                .or_else(|| Some(machine_id.clone()));
        }
        WorkspaceSessionTab::SftpTransfer { machine_id, .. }
        | WorkspaceSessionTab::WorkspaceFile { machine_id, .. } => {
            snapshot.focused_pane_id = None;
            snapshot.selected_machine_id = Some(machine_id.clone());
        }
        WorkspaceSessionTab::Unsupported { .. } => {
            snapshot.focused_pane_id = None;
            snapshot.selected_machine_id = None;
        }
    }
}

/// 更新 tab enum 的 group 引用而不触及布局、恢复目标或任何 Root-owned session 映射。
fn set_tab_group(tab: &mut WorkspaceSessionTab, group_id: Option<WorkspaceTabGroupId>) {
    match tab {
        WorkspaceSessionTab::Terminal { tab_group_id, .. }
        | WorkspaceSessionTab::SftpTransfer { tab_group_id, .. }
        | WorkspaceSessionTab::WorkspaceFile { tab_group_id, .. }
        | WorkspaceSessionTab::Unsupported { tab_group_id, .. } => *tab_group_id = group_id,
    }
}

/// 把 persisted group 顺序固定为各组在 v3 tabs 中首次出现的位置；空组按 ID 稳定追加。
fn ordered_tab_group_ids(snapshot: &WorkspaceSessionSnapshot) -> Vec<WorkspaceTabGroupId> {
    let mut ordered = Vec::with_capacity(snapshot.tab_groups.len());
    let mut seen = BTreeSet::new();
    for tab in &snapshot.terminal_tabs {
        if let Some(group_id) = tab.tab_group_id()
            && seen.insert(group_id.clone())
        {
            ordered.push(group_id.clone());
        }
    }
    for group_id in snapshot.tab_groups.keys() {
        if seen.insert(group_id.clone()) {
            ordered.push(group_id.clone());
        }
    }
    ordered
}

/// 生成与 UI 分组栏一致的平铺顺序，拖放索引和关闭意图都以此视图为准。
fn ordered_tab_indices(snapshot: &WorkspaceSessionSnapshot) -> Vec<usize> {
    let mut indices = Vec::with_capacity(snapshot.terminal_tabs.len());
    let group_order = ordered_tab_group_ids(snapshot);
    for group_id in group_order {
        indices.extend(
            snapshot
                .terminal_tabs
                .iter()
                .enumerate()
                .filter_map(|(index, tab)| {
                    (tab.tab_group_id() == Some(&group_id)).then_some(index)
                }),
        );
    }
    indices.extend(
        snapshot
            .terminal_tabs
            .iter()
            .enumerate()
            .filter_map(|(index, tab)| tab.tab_group_id().is_none().then_some(index)),
    );
    indices
}

/// 组和标签标题在 controller 边界再拒绝空白及控制字符，免得其他 caller 绕过表单校验。
fn validate_workspace_title(raw_title: &str) -> AppResult<String> {
    let title = raw_title.trim();
    if title.is_empty() {
        return Err(AppError::invalid("工作区名称不能为空"));
    }
    if title.chars().any(char::is_control) {
        return Err(AppError::invalid("工作区名称不能包含控制字符"));
    }
    Ok(title.to_owned())
}

/// 新建和重命名组共享大小写不敏感的唯一性规则，可排除当前正在改名的组。
fn ensure_unique_group_title(
    snapshot: &WorkspaceSessionSnapshot,
    title: &str,
    excluded_group_id: Option<&WorkspaceTabGroupId>,
) -> AppResult<()> {
    let normalized = title.trim().to_lowercase();
    if snapshot.tab_groups.iter().any(|(group_id, group)| {
        Some(group_id) != excluded_group_id && group.title.trim().to_lowercase() == normalized
    }) {
        return Err(AppError::conflict("已有同名工作区分组"));
    }
    Ok(())
}

/// 只改 tab enum 的 title 字段，保留每个 tab kind 的 target/layout/编辑状态。
fn set_tab_title(tab: &mut WorkspaceSessionTab, title: String) {
    match tab {
        WorkspaceSessionTab::Terminal { title: current, .. }
        | WorkspaceSessionTab::SftpTransfer { title: current, .. }
        | WorkspaceSessionTab::WorkspaceFile { title: current, .. }
        | WorkspaceSessionTab::Unsupported { title: current, .. } => *current = title,
    }
}

/// 关闭批次重试只允许同一锚点语义，不把其他用户操作误认为原批次继续。
///
/// @author kongweiguang
fn same_close_batch_intent(
    original: &WorkspaceCloseBatchIntent,
    retry: &WorkspaceCloseBatchIntent,
) -> bool {
    match (original, retry) {
        (
            WorkspaceCloseBatchIntent::Group {
                group_id: original_group,
                ..
            },
            WorkspaceCloseBatchIntent::Group {
                group_id: retry_group,
                ..
            },
        ) => original_group == retry_group,
        (
            WorkspaceCloseBatchIntent::ToRight {
                anchor_tab_id: original_anchor,
                ..
            },
            WorkspaceCloseBatchIntent::ToRight {
                anchor_tab_id: retry_anchor,
                ..
            },
        ) => original_anchor == retry_anchor,
        (
            WorkspaceCloseBatchIntent::Others {
                keep_tab_id: original_keep,
                ..
            },
            WorkspaceCloseBatchIntent::Others {
                keep_tab_id: retry_keep,
                ..
            },
        ) => original_keep == retry_keep,
        (
            WorkspaceCloseBatchIntent::GroupOthers {
                group_id: original_group,
                keep_tab_id: original_keep,
                ..
            },
            WorkspaceCloseBatchIntent::GroupOthers {
                group_id: retry_group,
                keep_tab_id: retry_keep,
                ..
            },
        ) => original_group == retry_group && original_keep == retry_keep,
        (
            WorkspaceCloseBatchIntent::OutsideGroup {
                group_id: original_group,
                keep_tab_ids: original_keep,
                ..
            },
            WorkspaceCloseBatchIntent::OutsideGroup {
                group_id: retry_group,
                keep_tab_ids: retry_keep,
                ..
            },
        ) => original_group == retry_group && original_keep == retry_keep,
        _ => false,
    }
}

/// 逐项关闭前校验目标与保留组/锚点都仍对应点击时快照，防止确认期间成员变化扩大范围。
///
/// @author kongweiguang
fn validate_close_batch_intent(
    snapshot: &WorkspaceSessionSnapshot,
    intent: &WorkspaceCloseBatchIntent,
) -> AppResult<()> {
    let ordered_ids = ordered_tab_indices(snapshot)
        .into_iter()
        .filter_map(|index| {
            snapshot
                .terminal_tabs
                .get(index)
                .map(|tab| tab.id().clone())
        })
        .collect::<Vec<_>>();
    let expected = match intent {
        WorkspaceCloseBatchIntent::Group { group_id, .. } => {
            let group_id = WorkspaceTabGroupId::parse(group_id)?;
            if !snapshot.tab_groups.contains_key(&group_id) {
                return Err(AppError::not_found("工作区标签组不存在"));
            }
            ordered_ids
                .into_iter()
                .filter(|tab_id| {
                    snapshot
                        .terminal_tabs
                        .iter()
                        .find(|tab| tab.id() == tab_id)
                        .and_then(WorkspaceSessionTab::tab_group_id)
                        == Some(&group_id)
                })
                .collect::<Vec<_>>()
        }
        WorkspaceCloseBatchIntent::ToRight { anchor_tab_id, .. } => {
            let Some(anchor_index) = ordered_ids
                .iter()
                .position(|tab_id| tab_id == anchor_tab_id)
            else {
                return Err(AppError::not_found("右侧关闭锚点标签不存在"));
            };
            ordered_ids.into_iter().skip(anchor_index + 1).collect()
        }
        WorkspaceCloseBatchIntent::Others { keep_tab_id, .. } => {
            if !ordered_ids.iter().any(|tab_id| tab_id == keep_tab_id) {
                return Err(AppError::not_found("保留标签不存在"));
            }
            ordered_ids
                .into_iter()
                .filter(|tab_id| tab_id != keep_tab_id)
                .collect()
        }
        WorkspaceCloseBatchIntent::GroupOthers {
            group_id,
            keep_tab_id,
            ..
        } => {
            let group_id = WorkspaceTabGroupId::parse(group_id)?;
            if !snapshot.tab_groups.contains_key(&group_id) {
                return Err(AppError::not_found("工作区标签组不存在"));
            }
            let group_members = ordered_ids
                .iter()
                .filter(|tab_id| {
                    snapshot
                        .terminal_tabs
                        .iter()
                        .find(|tab| tab.id() == *tab_id)
                        .and_then(WorkspaceSessionTab::tab_group_id)
                        == Some(&group_id)
                })
                .collect::<Vec<_>>();
            if !group_members.contains(&keep_tab_id) {
                return Err(AppError::not_found("保留标签不属于该分组"));
            }
            group_members
                .into_iter()
                .filter(|tab_id| *tab_id != keep_tab_id)
                .cloned()
                .collect()
        }
        WorkspaceCloseBatchIntent::OutsideGroup {
            group_id,
            keep_tab_ids,
            ..
        } => {
            let group_id = WorkspaceTabGroupId::parse(group_id)?;
            if !snapshot.tab_groups.contains_key(&group_id) {
                return Err(AppError::not_found("工作区标签组不存在"));
            }
            let actual_members = ordered_ids
                .iter()
                .filter(|tab_id| {
                    snapshot
                        .terminal_tabs
                        .iter()
                        .find(|tab| tab.id() == *tab_id)
                        .and_then(WorkspaceSessionTab::tab_group_id)
                        == Some(&group_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if actual_members.as_slice() != keep_tab_ids.as_slice() {
                return Err(AppError::conflict("保留分组成员已变化，请重新选择"));
            }
            ordered_ids
                .into_iter()
                .filter(|tab_id| !actual_members.contains(tab_id))
                .collect()
        }
    };
    if expected != intent.tab_ids() {
        return Err(AppError::conflict("批量关闭目标已变化，请重新选择"));
    }
    Ok(())
}
