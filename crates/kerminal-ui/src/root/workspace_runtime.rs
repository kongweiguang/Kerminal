//! Workspace 持久化 pane 与真实 terminal session 的短期运行态映射。
//!
//! SessionId、快照和连接目标只保留在进程内；持久化 snapshot 仍只保存冻结的 pane ID/target。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Arc};

use gpui_kit::SharedString;
use kerminal_application::platform::external_launch::ExternalTargetLease;
use kerminal_application::{
    sessions::UiSessionBinding,
    workspace::{WorkspaceSessionPane, WorkspaceSessionSnapshot, WorkspaceSessionTab},
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionOwner, SessionTarget, TerminalSize},
    workspace::TerminalLayoutNode,
};
use kerminal_terminal::snapshot::{RenderSnapshot, RenderSnapshotRequest};

use super::session_queue::{SnapshotRequestGate, SnapshotRequestToken};

/// Pane 的实际资源阶段；恢复失败不会被替换成本机默认 shell。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RuntimePanePhase {
    RestorePending,
    Ready,
    RestoreFailed,
    Closing,
    CloseFailed,
}

/// Root 用此错误决定提示和重试，不携带连接错误正文或持久化路径。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkspaceRuntimeError {
    InvalidSnapshot,
    DuplicatePane,
    PaneMissing,
    PaneNotRestorable,
    DuplicateSession,
    SessionMismatch,
    PaneNotReady,
    OrphanCleanupMissing,
    OrphanCleanupInProgress,
    OrphanCleanupGenerationExhausted,
}

/// 单个持久化 pane 的真实资源状态；SessionId 从不写回 WorkspaceSessionPane。
pub(super) struct RuntimeTerminalPane {
    pub tab_id: TabId,
    pub persisted: WorkspaceSessionPane,
    pub phase: RuntimePanePhase,
    pub session_id: Option<SessionId>,
    /// Only a naturally retired view can regain read-only history access after close cleanup fails.
    pub retired_history_binding: Option<UiSessionBinding>,
    pub target: Option<SessionTarget>,
    /// 最近一次引擎快照的网格；已结束历史 resize 只更新此值，不伪装为 PTY ACK。
    pub view_size: TerminalSize,
    pub size: TerminalSize,
    /// 仅代表已成功 ACK 的 resize；失败或待处理尺寸不能覆盖该值。
    pub pending_resize: Option<TerminalSize>,
    /// 独立记录只读历史重排，避免把历史画布尺寸混入运行态 PTY 确认尺寸。
    pub pending_history_view_resize: Option<TerminalSize>,
    pub snapshot: Option<RenderSnapshot>,
    /// 只缓存当前 session 已通过快照门禁的目录；resize 清画面不清元数据，不写回持久工作区。
    pub current_working_directory: Option<Arc<str>>,
    /// Pane 级安全状态供分屏占位直接显示，避免只在窗口全局 status 丢失失败归属。
    pub status: Option<SharedString>,
    pub last_snapshot_error: bool,
    /// 外部目标由 pane owner 持有；失败重试或关闭失败时不能提前释放临时连接 scope。
    pub external_lease: Option<ExternalTargetLease>,
    /// 临时连接只存在于本进程，保存边界按 pane/tab identity 过滤。
    pub transient: bool,
    /// 外部启动的 SFTP follow-up 只在 terminal session 首次就绪后消费一次。
    pub open_sftp_after_connect: bool,
    snapshot_gate: SnapshotRequestGate,
    snapshot_in_flight: Option<InFlightSnapshot>,
    snapshot_dirty: bool,
}

/// 单 pane 同时只允许一个快照 RPC；session 与 token 一起防止旧回执清除新请求。
#[derive(Clone, Debug, Eq, PartialEq)]
struct InFlightSnapshot {
    session_id: SessionId,
    token: SnapshotRequestToken,
}

/// 将失去 pane 绑定的完整 session owner 按值传入，保留 lease 同一性且不新增分配。
///
/// @author kongweiguang
pub(super) struct OrphanCleanupInput {
    pub(super) tab_id: TabId,
    pub(super) pane_id: PaneId,
    pub(super) session_id: SessionId,
    pub(super) target: SessionTarget,
    pub(super) size: TerminalSize,
    pub(super) snapshot: Option<RenderSnapshot>,
    pub(super) status: SharedString,
    pub(super) external_lease: Option<ExternalTargetLease>,
    pub(super) transient: bool,
}

/// 资源已经创建但无法安全绑定回原 pane 时的可见清理 owner；lease 随真实 close ACK 才释放。
pub(super) struct OrphanCleanupSession {
    pub tab_id: TabId,
    pub pane_id: PaneId,
    pub session_id: SessionId,
    pub target: SessionTarget,
    pub size: TerminalSize,
    pub snapshot: Option<RenderSnapshot>,
    pub status: SharedString,
    pub external_lease: Option<ExternalTargetLease>,
    pub transient: bool,
    /// 每次开始清理递增，拒绝前一轮迟到的 close 回执。
    pub cleanup_generation: u64,
    /// 同一个真实 SessionId 同时只允许一个 close future 执行。
    pub cleanup_in_flight: bool,
}

/// terminal pane 映射表；外部 Tab 生命周期成功后再插入或移除资源。
pub(super) struct WorkspaceRuntime {
    panes: HashMap<PaneId, RuntimeTerminalPane>,
    pane_by_session: HashMap<SessionId, PaneId>,
    layouts_by_tab: HashMap<TabId, TerminalLayoutNode>,
    orphan_cleanup: HashMap<SessionId, OrphanCleanupSession>,
}

impl WorkspaceRuntime {
    /// 从已校验的 v3 快照恢复 terminal pane 意图，SFTP/文件 tab 留给各自 feature owner。
    pub fn from_snapshot(
        snapshot: &WorkspaceSessionSnapshot,
    ) -> Result<Self, WorkspaceRuntimeError> {
        if snapshot.validate().is_err() {
            return Err(WorkspaceRuntimeError::InvalidSnapshot);
        }
        let persisted_panes = snapshot
            .terminal_panes
            .iter()
            .map(|pane| (pane.id.clone(), pane))
            .collect::<HashMap<_, _>>();
        let mut runtime = Self::default();
        for tab in &snapshot.terminal_tabs {
            let WorkspaceSessionTab::Terminal {
                id: tab_id, layout, ..
            } = tab
            else {
                continue;
            };
            runtime.register_tab_layout(tab_id.clone(), layout.clone());
            for pane_id in layout.pane_ids() {
                let pane = persisted_panes
                    .get(&pane_id)
                    .ok_or(WorkspaceRuntimeError::InvalidSnapshot)?;
                runtime.register_pending_pane(tab_id.clone(), (*pane).clone())?;
            }
        }
        Ok(runtime)
    }

    /// 先把 reducer 的候选 pane 放入运行态占位；它不会改持久快照，直到资源创建成功。
    pub fn register_pending_pane(
        &mut self,
        tab_id: TabId,
        pane: WorkspaceSessionPane,
    ) -> Result<(), WorkspaceRuntimeError> {
        if self.panes.contains_key(&pane.id) {
            return Err(WorkspaceRuntimeError::DuplicatePane);
        }
        self.panes.insert(
            pane.id.clone(),
            RuntimeTerminalPane {
                tab_id,
                persisted: pane,
                phase: RuntimePanePhase::RestorePending,
                session_id: None,
                retired_history_binding: None,
                target: None,
                view_size: TerminalSize::default(),
                size: TerminalSize::default(),
                pending_resize: None,
                pending_history_view_resize: None,
                snapshot: None,
                current_working_directory: None,
                status: None,
                last_snapshot_error: false,
                external_lease: None,
                transient: false,
                open_sftp_after_connect: false,
                snapshot_gate: SnapshotRequestGate::default(),
                snapshot_in_flight: None,
                snapshot_dirty: false,
            },
        );
        Ok(())
    }

    /// 在资源创建前保留窗口测得的网格，避免恢复 resolver 用默认尺寸启动 PTY。
    pub fn set_pending_size(
        &mut self,
        pane_id: &PaneId,
        size: TerminalSize,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.is_some()
            || !matches!(
                pane.phase,
                RuntimePanePhase::RestorePending | RuntimePanePhase::RestoreFailed
            )
        {
            return Err(WorkspaceRuntimeError::PaneNotRestorable);
        }
        pane.size = size;
        pane.view_size = size;
        Ok(())
    }

    /// 保留工作区完整递归树，避免新 tab 或拆分 pane 被降级成默认单 pane 布局。
    pub fn register_tab_layout(&mut self, tab_id: TabId, layout: TerminalLayoutNode) {
        self.layouts_by_tab.insert(tab_id, layout);
    }

    /// 查询该 tab 当前实际布局；缺失时调用方必须显示失败，不能合成单 pane 回退。
    pub fn tab_layout(&self, tab_id: &TabId) -> Option<&TerminalLayoutNode> {
        self.layouts_by_tab.get(tab_id)
    }

    /// 只在 session 仍绑定同一 Ready pane 时允许 watcher 刷新，拒绝旧会话事件借 PaneId 跟随替代资源。
    ///
    /// @author kongweiguang
    pub fn snapshot_session_is_current(&self, pane_id: &PaneId, session_id: &SessionId) -> bool {
        self.panes.get(pane_id).is_some_and(|pane| {
            pane.phase == RuntimePanePhase::Ready && pane.session_id.as_ref() == Some(session_id)
        })
    }

    /// 视口滚动与绝对跳转开始前撤销在途绘制代次，避免旧视口快照短暂覆盖新引擎视口。
    ///
    /// @author kongweiguang
    pub fn invalidate_snapshot_for_viewport_change(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane)
            || pane.session_id.as_ref() != Some(session_id)
        {
            return Ok(false);
        }
        pane.snapshot_gate.invalidate();
        pane.snapshot_dirty = true;
        Ok(true)
    }

    /// 只在 Ready pane 或精确保留历史票据的 CloseFailed pane 中请求脏快照，避免重复 RPC 和错误复活。
    ///
    /// @author kongweiguang
    pub fn snapshot_needs_refresh(&self, pane_id: &PaneId) -> bool {
        self.panes.get(pane_id).is_some_and(|pane| {
            runtime_pane_has_view_access(pane_id, pane)
                && pane.snapshot_in_flight.is_none()
                && (pane.snapshot.is_none() || pane.snapshot_dirty)
        })
    }

    /// tab 关闭并提交后移除对应布局，保留其它 pane 的运行态和 layout。
    pub fn remove_tab_layout(&mut self, tab_id: &TabId) {
        self.layouts_by_tab.remove(tab_id);
    }

    /// 绑定 pane 级资源 owner，直到真实关闭成功或 pending pane 明确移除才释放 lease。
    pub fn set_resource_owner(
        &mut self,
        pane_id: &PaneId,
        lease: Option<ExternalTargetLease>,
        transient: bool,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        pane.external_lease = lease;
        pane.transient = transient;
        Ok(())
    }

    /// 将 pane 的可见状态与一次性 follow-up 关联到稳定身份，防止异步错误显示到别的 pane。
    pub fn set_status(
        &mut self,
        pane_id: &PaneId,
        status: Option<SharedString>,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        pane.status = status;
        Ok(())
    }

    /// 登记 SSH 成功后的单次 SFTP 意图；只有 session 进入 Ready 后才可消费。
    pub fn set_open_sftp_after_connect(
        &mut self,
        pane_id: &PaneId,
        enabled: bool,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        pane.open_sftp_after_connect = enabled;
        Ok(())
    }

    /// 取走一次性 follow-up；重复或迟到 session 通知不能重复打开 SFTP。
    pub fn take_open_sftp_after_connect(
        &mut self,
        pane_id: &PaneId,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        Ok(std::mem::take(&mut pane.open_sftp_after_connect))
    }

    /// 即使创建代次已失效，也将未能关闭的 session 绑定到原 pane 供用户重试清理。
    pub fn retain_cleanup_session(
        &mut self,
        pane_id: &PaneId,
        session_id: SessionId,
        target: SessionTarget,
        size: TerminalSize,
        snapshot: Option<RenderSnapshot>,
        status: SharedString,
    ) -> Result<(), WorkspaceRuntimeError> {
        if self.pane_by_session.contains_key(&session_id) {
            return Err(WorkspaceRuntimeError::DuplicateSession);
        }
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.is_some() {
            return Err(WorkspaceRuntimeError::SessionMismatch);
        }
        pane.session_id = Some(session_id.clone());
        pane.retired_history_binding = None;
        pane.target = Some(target);
        pane.size = size;
        pane.view_size = size;
        pane.pending_history_view_resize = None;
        pane.snapshot = snapshot;
        pane.status = Some(status);
        pane.phase = RuntimePanePhase::CloseFailed;
        pane.snapshot_gate.invalidate();
        self.pane_by_session.insert(session_id, pane_id.clone());
        Ok(())
    }

    /// 在 pane 已被替换或移除时仍保留真实 owner；按值接收 DTO 保留 lease 与状态字段原有所有权。
    ///
    /// @author kongweiguang
    pub fn retain_orphan_cleanup(
        &mut self,
        input: OrphanCleanupInput,
    ) -> Result<(), WorkspaceRuntimeError> {
        let OrphanCleanupInput {
            tab_id,
            pane_id,
            session_id,
            target,
            size,
            snapshot,
            status,
            external_lease,
            transient,
        } = input;
        if self.pane_by_session.contains_key(&session_id)
            || self.orphan_cleanup.contains_key(&session_id)
        {
            return Err(WorkspaceRuntimeError::DuplicateSession);
        }
        let entry = OrphanCleanupSession {
            tab_id,
            pane_id,
            session_id: session_id.clone(),
            target,
            size,
            snapshot,
            status,
            external_lease,
            transient,
            cleanup_generation: 0,
            cleanup_in_flight: false,
        };
        self.orphan_cleanup.insert(session_id, entry);
        Ok(())
    }

    /// 新建 tab 的持久化硬失败时原子移交 Ready owner，避免无 Tab 会话失去可重试清理入口。
    /// 仅允许精确 session 映射、有效目标和单 pane 布局，防止误移走已有或分屏资源。
    ///
    /// @author kongweiguang
    pub fn move_ready_pane_to_orphan_cleanup(
        &mut self,
        pane_id: &PaneId,
        status: SharedString,
    ) -> Result<SessionId, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        let session_id = pane
            .session_id
            .clone()
            .ok_or(WorkspaceRuntimeError::PaneNotReady)?;
        let target = pane
            .target
            .clone()
            .ok_or(WorkspaceRuntimeError::SessionMismatch)?;
        if self.pane_by_session.get(&session_id) != Some(pane_id)
            || pane
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.session_id != session_id)
        {
            return Err(WorkspaceRuntimeError::SessionMismatch);
        }
        if self.orphan_cleanup.contains_key(&session_id) {
            return Err(WorkspaceRuntimeError::DuplicateSession);
        }
        let layout = self
            .layouts_by_tab
            .get(&pane.tab_id)
            .ok_or(WorkspaceRuntimeError::InvalidSnapshot)?;
        if !layout.contains_pane(pane_id) || layout.pane_ids().len() != 1 {
            return Err(WorkspaceRuntimeError::InvalidSnapshot);
        }

        // 所有可能失败的校验都在取走资源前完成；以下同步变更将唯一 owner 从 pane 转给 orphan。
        let owner = self
            .panes
            .remove(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        self.pane_by_session.remove(&session_id);
        self.layouts_by_tab.remove(&owner.tab_id);
        self.orphan_cleanup.insert(
            session_id.clone(),
            OrphanCleanupSession {
                tab_id: owner.tab_id,
                pane_id: pane_id.clone(),
                session_id: session_id.clone(),
                target,
                size: owner.size,
                snapshot: owner.snapshot,
                status,
                external_lease: owner.external_lease,
                transient: owner.transient,
                cleanup_generation: 0,
                cleanup_in_flight: false,
            },
        );
        Ok(session_id)
    }

    /// 为精确 session ID 预约一次清理；代次和 in-flight 门禁阻止双击与迟到回执串 owner。
    pub fn begin_orphan_cleanup(
        &mut self,
        session_id: &SessionId,
        status: SharedString,
    ) -> Result<u64, WorkspaceRuntimeError> {
        let owner = self
            .orphan_cleanup
            .get_mut(session_id)
            .ok_or(WorkspaceRuntimeError::OrphanCleanupMissing)?;
        if owner.cleanup_in_flight {
            return Err(WorkspaceRuntimeError::OrphanCleanupInProgress);
        }
        let generation = owner
            .cleanup_generation
            .checked_add(1)
            .ok_or(WorkspaceRuntimeError::OrphanCleanupGenerationExhausted)?;
        owner.cleanup_generation = generation;
        owner.cleanup_in_flight = true;
        owner.status = status;
        Ok(generation)
    }

    /// close 收到明确成功 ACK 后才释放该代次 owner 和其 external lease。
    pub fn finish_orphan_cleanup(
        &mut self,
        session_id: &SessionId,
        generation: u64,
    ) -> Option<OrphanCleanupSession> {
        let owner = self.orphan_cleanup.get(session_id)?;
        if !owner.cleanup_in_flight || owner.cleanup_generation != generation {
            return None;
        }
        self.orphan_cleanup.remove(session_id)
    }

    /// 失败或执行器取消时恢复可重试状态，保留原 session ID、目标和 lease owner。
    pub fn fail_orphan_cleanup(
        &mut self,
        session_id: &SessionId,
        generation: u64,
        status: SharedString,
    ) -> bool {
        let Some(owner) = self.orphan_cleanup.get_mut(session_id) else {
            return false;
        };
        if !owner.cleanup_in_flight || owner.cleanup_generation != generation {
            return false;
        }
        owner.cleanup_in_flight = false;
        owner.status = status;
        true
    }

    /// 返回所有脱离原 pane 但仍需清理的 session，供 Root 提供明确状态和重试入口。
    pub fn orphan_cleanup_sessions(
        &self,
    ) -> impl Iterator<Item = (&SessionId, &OrphanCleanupSession)> {
        self.orphan_cleanup.iter()
    }

    /// 按稳定 session ID 查询 orphan owner；不存在时不从活动 pane 猜测替代资源。
    pub fn orphan_cleanup(&self, session_id: &SessionId) -> Option<&OrphanCleanupSession> {
        self.orphan_cleanup.get(session_id)
    }

    /// 按原 tab 查询清理 owner，关闭流程不得把迟到 session 绑定到同 ID 的新 tab。
    pub fn orphan_cleanup_for_tab(
        &self,
        tab_id: &TabId,
    ) -> impl Iterator<Item = (&SessionId, &OrphanCleanupSession)> {
        self.orphan_cleanup
            .iter()
            .filter(move |(_, owner)| &owner.tab_id == tab_id)
    }

    /// 按原 pane 查询清理 owner，允许 pane 占位显示失败并重试 close。
    pub fn orphan_cleanup_for_pane(
        &self,
        pane_id: &PaneId,
    ) -> impl Iterator<Item = (&SessionId, &OrphanCleanupSession)> {
        self.orphan_cleanup
            .iter()
            .filter(move |(_, owner)| &owner.pane_id == pane_id)
    }

    /// 返回临时 tab/pane 身份供工作区保存投影过滤，不把临时目标放进持久模型。
    pub fn transient_ids(
        &self,
    ) -> (
        std::collections::BTreeSet<TabId>,
        std::collections::BTreeSet<PaneId>,
    ) {
        let panes = self
            .panes
            .iter()
            .filter(|(_, pane)| pane.transient)
            .map(|(pane_id, _)| pane_id.clone())
            .collect::<std::collections::BTreeSet<_>>();
        let tabs = self
            .layouts_by_tab
            .iter()
            .filter(|(_, layout)| {
                let pane_ids = layout.pane_ids();
                !pane_ids.is_empty() && pane_ids.iter().all(|pane_id| panes.contains(pane_id))
            })
            .map(|(tab_id, _)| tab_id.clone())
            .collect();
        (tabs, panes)
    }

    /// 绑定真实 session 时清掉上个实例的目录缓存，防止复用 PaneId 把旧目录投给新连接。
    /// 重试保留原 PaneId，但禁止重复 session 占用。
    ///
    /// @author kongweiguang
    pub fn install_session(
        &mut self,
        pane_id: &PaneId,
        session_id: SessionId,
        target: SessionTarget,
        size: TerminalSize,
    ) -> Result<(), WorkspaceRuntimeError> {
        if self.pane_by_session.contains_key(&session_id) {
            return Err(WorkspaceRuntimeError::DuplicateSession);
        }
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !matches!(
            pane.phase,
            RuntimePanePhase::RestorePending | RuntimePanePhase::RestoreFailed
        ) {
            return Err(WorkspaceRuntimeError::PaneNotRestorable);
        }
        pane.session_id = Some(session_id.clone());
        pane.retired_history_binding = None;
        pane.target = Some(target);
        pane.size = size;
        pane.view_size = size;
        pane.pending_resize = None;
        pane.pending_history_view_resize = None;
        pane.snapshot = None;
        pane.current_working_directory = None;
        pane.status = None;
        pane.last_snapshot_error = false;
        pane.phase = RuntimePanePhase::Ready;
        pane.snapshot_gate.invalidate();
        pane.snapshot_in_flight = None;
        pane.snapshot_dirty = true;
        self.pane_by_session.insert(session_id, pane_id.clone());
        Ok(())
    }

    /// 在连接创建前保存经过明确解析的目标，使连接失败时仍可按原目标重试。
    pub fn set_restore_target(
        &mut self,
        pane_id: &PaneId,
        target: SessionTarget,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !matches!(
            pane.phase,
            RuntimePanePhase::RestorePending | RuntimePanePhase::RestoreFailed
        ) || pane.session_id.is_some()
        {
            return Err(WorkspaceRuntimeError::PaneNotRestorable);
        }
        pane.target = Some(target);
        Ok(())
    }

    /// 首帧与后续快照共用身份和 revision 门禁；只有接受后才缓存该实例的目录元数据。
    ///
    /// @author kongweiguang
    pub fn accept_initial_snapshot(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        snapshot: RenderSnapshot,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready
            || pane.session_id.as_ref() != Some(session_id)
            || snapshot.session_id != *session_id
            || snapshot.size != pane.view_size
        {
            return Ok(false);
        }
        let (token, _) = pane.snapshot_gate.begin();
        if !pane.snapshot_gate.accept(token, snapshot.revision) {
            return Ok(false);
        }
        pane.current_working_directory = snapshot.current_working_directory.clone();
        pane.snapshot = Some(snapshot);
        pane.last_snapshot_error = false;
        pane.snapshot_in_flight = None;
        pane.snapshot_dirty = false;
        Ok(true)
    }

    /// 创建目标失败时仍保留 pane 和原 target，让 renderer 展示失败并允许用户显式重试。
    pub fn mark_restore_failed(&mut self, pane_id: &PaneId) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.is_some() {
            return Err(WorkspaceRuntimeError::PaneNotRestorable);
        }
        pane.phase = RuntimePanePhase::RestoreFailed;
        pane.snapshot = None;
        pane.snapshot_gate.invalidate();
        Ok(())
    }

    /// 仅把精确匹配的失败空 pane 重新置为待测量；连接目标和资源所有权仍由原操作持有。
    ///
    /// Root 还须在调用前核对操作 Entry、取消/延迟关闭及 orphan owner；此处不创建 session、发起 IO
    /// 或清理窗口级错误，避免把用户主动重试伪装成先前资源操作已成功。
    /// @author kongweiguang
    pub fn rearm_failed_split_for_measurement(
        &mut self,
        tab_id: &TabId,
        expected_pane: &WorkspaceSessionPane,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(&expected_pane.id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.tab_id != *tab_id
            || pane.persisted != *expected_pane
            || pane.phase != RuntimePanePhase::RestoreFailed
            || pane.session_id.is_some()
        {
            return Err(WorkspaceRuntimeError::PaneNotRestorable);
        }

        pane.phase = RuntimePanePhase::RestorePending;
        pane.status = None;
        Ok(())
    }

    /// 只有实际尺寸不同且未存在相同请求时才排队，避免 Canvas 重绘重复 resize。
    pub fn resize_needs_enqueue(
        &self,
        pane_id: &PaneId,
        size: TerminalSize,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready || pane.session_id.is_none() {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        Ok(pane.size != size && pane.pending_resize != Some(size))
    }

    /// 历史画布只按 engine view 尺寸去重；已有不同目标在途时允许排队恢复到较早尺寸。
    ///
    /// @author kongweiguang
    pub fn history_view_resize_needs_enqueue(
        &self,
        pane_id: &PaneId,
        size: TerminalSize,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane) || pane.session_id.is_none() {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        Ok(pane.pending_history_view_resize != Some(size)
            && (pane.view_size != size || pane.pending_history_view_resize.is_some()))
    }

    /// 记录已入队的只读网格调整；它与 live PTY 的 pending ACK 独立，关闭时可分别撤销。
    ///
    /// @author kongweiguang
    pub fn mark_history_view_resize_pending(
        &mut self,
        pane_id: &PaneId,
        size: TerminalSize,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane) || pane.session_id.is_none() {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        pane.pending_history_view_resize = Some(size);
        Ok(())
    }

    /// 只在只读 view resize 成功后更新快照网格，保留真实 PTY 尺寸和持久工作区状态。
    ///
    /// @author kongweiguang
    pub fn complete_history_view_resize(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        size: TerminalSize,
        succeeded: bool,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane)
            || pane.session_id.as_ref() != Some(session_id)
        {
            return Ok(false);
        }
        if succeeded && pane.view_size != size {
            pane.view_size = size;
            pane.snapshot = None;
            pane.snapshot_gate.invalidate();
            pane.snapshot_dirty = true;
        }
        if pane.pending_history_view_resize == Some(size) {
            pane.pending_history_view_resize = None;
        }
        Ok(true)
    }

    /// 记录已接受进入 session FIFO 的尺寸，不把尚未获得 PTY ACK 的值当成已确认状态。
    pub fn mark_resize_pending(
        &mut self,
        pane_id: &PaneId,
        size: TerminalSize,
    ) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready || pane.session_id.is_none() {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        pane.pending_resize = Some(size);
        Ok(())
    }

    /// 会话退休为可读历史时撤销未回执的 PTY resize 标记，不能把旧 writer 当作仍待确认。
    ///
    /// @author kongweiguang
    pub fn clear_pending_transport_resize_after_exit(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready || pane.session_id.as_ref() != Some(session_id) {
            return Ok(false);
        }
        pane.pending_resize = None;
        Ok(true)
    }

    /// 队列退休时把历史尺寸标记复位到仍在途的真实目标，避免已撤销的末项永久阻塞重排。
    ///
    /// @author kongweiguang
    pub fn reset_pending_history_view_resize_after_queue_cancel(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        in_flight_size: Option<TerminalSize>,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane)
            || pane.session_id.as_ref() != Some(session_id)
        {
            return Ok(false);
        }
        pane.pending_history_view_resize = in_flight_size;
        Ok(true)
    }

    /// 按真实 RPC 回执提交 pane 尺寸；resize 后保留最后一帧供重绘过渡，直到新网格快照到达。
    ///
    /// 旧帧仅作视觉连续性的临时内容，快照代次仍失效且尺寸门禁拒绝旧回执，避免闪成恢复占位或误认旧网格。
    ///
    /// @author kongweiguang
    pub fn complete_resize(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        size: TerminalSize,
        succeeded: bool,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.phase != RuntimePanePhase::Ready || pane.session_id.as_ref() != Some(session_id) {
            return Ok(false);
        }
        if succeeded {
            let view_grid_changed = pane.view_size != size;
            pane.size = size;
            pane.view_size = size;
            if view_grid_changed {
                pane.snapshot_gate.invalidate();
                pane.snapshot_dirty = true;
            }
        }
        if pane.pending_resize == Some(size) {
            pane.pending_resize = None;
        }
        Ok(true)
    }

    /// 每 pane 单独追踪快照请求；在途时合并后续变化，禁止同一 pane 并发快照 RPC。
    ///
    /// @author kongweiguang
    pub fn begin_snapshot(
        &mut self,
        pane_id: &PaneId,
    ) -> Result<
        Option<(SessionId, SnapshotRequestToken, RenderSnapshotRequest)>,
        WorkspaceRuntimeError,
    > {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane) {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        let session_id = pane
            .session_id
            .clone()
            .ok_or(WorkspaceRuntimeError::PaneNotReady)?;

        if pane.snapshot_in_flight.is_some() {
            pane.snapshot_dirty = true;
            return Ok(None);
        }

        let (token, request) = pane.snapshot_gate.begin();
        pane.snapshot_dirty = false;
        pane.snapshot_in_flight = Some(InFlightSnapshot {
            session_id: session_id.clone(),
            token,
        });
        Ok(Some((session_id, token, request)))
    }

    /// 输出可能先于异步 view binding 到达；保留 dirty 并等绑定补读，避免唯一一次输出永久丢帧。
    /// Closing 和无历史票据的 CloseFailed 不进入补读流程，绑定也必须属于当前 tab/pane/session。
    ///
    /// @author kongweiguang
    pub fn prepare_bound_snapshot(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        binding: Option<&UiSessionBinding>,
    ) -> bool {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return false;
        };
        if &pane.tab_id != tab_id || !runtime_pane_has_view_access(pane_id, pane) {
            return false;
        }
        let Some(binding) = binding else {
            pane.snapshot_dirty = true;
            return false;
        };
        ui_binding_matches_runtime_pane(pane_id, pane, binding)
            && (pane.phase == RuntimePanePhase::Ready
                || pane.retired_history_binding.as_ref() == Some(binding))
    }

    /// 隐藏或尚未安装绑定的 pane 只记录 dirty，恢复可见且绑定完成后读取引擎当前视口。
    ///
    /// @author kongweiguang
    pub fn defer_snapshot(&mut self, pane_id: &PaneId) -> Result<(), WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !runtime_pane_has_view_access(pane_id, pane) {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        pane.snapshot_dirty = true;
        Ok(())
    }

    /// 完成对应的在途请求并返回是否需补读；失败保留 dirty，等待显式重试或重新可见。
    ///
    /// @author kongweiguang
    pub fn finish_snapshot_request(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        token: SnapshotRequestToken,
        accepted: bool,
        allow_follow_up: bool,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        let matches_current = pane
            .snapshot_in_flight
            .as_ref()
            .is_some_and(|request| request.session_id == *session_id && request.token == token);
        if !matches_current {
            return Ok(false);
        }

        let had_pending_changes = pane.snapshot_dirty;
        pane.snapshot_in_flight = None;
        if !accepted {
            pane.snapshot_dirty = true;
        }
        if allow_follow_up
            && had_pending_changes
            && pane.snapshot_dirty
            && runtime_pane_has_view_access(pane_id, pane)
            && pane.session_id.as_ref() == Some(session_id)
        {
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// 只接受 session、尺寸、代次及 revision 均有效的快照，旧回执也不能改目录。
    /// 目录随当前 engine 快照缓存，跨 resize 保留，但不触发持久化或 Agent 重新绑定。
    ///
    /// @author kongweiguang
    pub fn accept_snapshot(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        token: SnapshotRequestToken,
        snapshot: RenderSnapshot,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.as_ref() != Some(session_id) || snapshot.session_id != *session_id {
            return Ok(false);
        }
        if !runtime_pane_has_view_access(pane_id, pane) || snapshot.size != pane.view_size {
            return Ok(false);
        }
        if !pane.snapshot_gate.accept(token, snapshot.revision) {
            return Ok(false);
        }
        pane.current_working_directory = snapshot.current_working_directory.clone();
        pane.snapshot = Some(snapshot);
        pane.last_snapshot_error = false;
        Ok(true)
    }

    /// 失败也必须匹配当前在途 token，避免旧代次错误污染新请求；同绑定失败等待事件，换代才补读。
    ///
    /// @author kongweiguang
    pub fn mark_snapshot_failed(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
        token: SnapshotRequestToken,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.as_ref() != Some(session_id)
            || !runtime_pane_has_view_access(pane_id, pane)
            || !pane
                .snapshot_in_flight
                .as_ref()
                .is_some_and(|request| request.session_id == *session_id && request.token == token)
        {
            return Ok(false);
        }
        pane.last_snapshot_error = true;
        pane.snapshot_dirty = true;
        Ok(true)
    }

    /// 标记关闭中但保留映射，关闭失败时可以恢复同一 session 而不另起终端。
    pub fn begin_close(&mut self, pane_id: &PaneId) -> Result<SessionId, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if !matches!(
            pane.phase,
            RuntimePanePhase::Ready | RuntimePanePhase::CloseFailed
        ) {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        let session_id = pane
            .session_id
            .clone()
            .ok_or(WorkspaceRuntimeError::PaneNotReady)?;
        pane.phase = RuntimePanePhase::Closing;
        pane.pending_resize = None;
        pane.pending_history_view_resize = None;
        pane.snapshot_gate.invalidate();
        pane.snapshot_in_flight = None;
        pane.snapshot_dirty = false;
        Ok(session_id)
    }

    /// close 错误保留 SessionId 和快照，界面可显示失败并再次请求真实关闭。
    pub fn mark_close_failed(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.as_ref() != Some(session_id) || pane.phase != RuntimePanePhase::Closing {
            return Ok(false);
        }
        pane.phase = RuntimePanePhase::CloseFailed;
        Ok(true)
    }

    /// Records the exact UI binding only on natural retirement, so close failure cannot invent history access.
    ///
    /// @author kongweiguang
    pub fn retire_history_binding(&mut self, pane_id: &PaneId, binding: UiSessionBinding) -> bool {
        let Some(pane) = self.panes.get_mut(pane_id) else {
            return false;
        };
        if pane.phase != RuntimePanePhase::Ready
            || !ui_binding_matches_runtime_pane(pane_id, pane, &binding)
        {
            return false;
        }
        pane.retired_history_binding = Some(binding);
        true
    }

    /// Checks the frozen view identity without treating CloseFailed as input or transport ownership.
    ///
    /// @author kongweiguang
    pub fn retired_history_binding_matches(
        &self,
        pane_id: &PaneId,
        binding: &UiSessionBinding,
    ) -> bool {
        self.panes
            .get(pane_id)
            .is_some_and(|pane| retired_history_binding_matches(pane_id, pane, binding))
    }

    /// CloseFailed gains view access only when its retained ticket exactly matches this binding.
    ///
    /// @author kongweiguang
    pub fn close_failed_history_binding_matches(
        &self,
        pane_id: &PaneId,
        binding: &UiSessionBinding,
    ) -> bool {
        self.panes.get(pane_id).is_some_and(|pane| {
            pane.phase == RuntimePanePhase::CloseFailed
                && retired_history_binding_matches(pane_id, pane, binding)
        })
    }

    /// 只有实际 close 成功后才移除运行态资源；持久布局是否删 pane 由 controller 另行提交。
    pub fn remove_closed_session(
        &mut self,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get_mut(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.as_ref() != Some(session_id) || pane.phase != RuntimePanePhase::Closing {
            return Ok(false);
        }
        self.pane_by_session.remove(session_id);
        self.panes.remove(pane_id);
        Ok(true)
    }

    /// pane 布局 close 已提交后删除其空运行态占位；失败的真实 close 必须先被解决。
    pub fn remove_empty_pane(&mut self, pane_id: &PaneId) -> Result<bool, WorkspaceRuntimeError> {
        let pane = self
            .panes
            .get(pane_id)
            .ok_or(WorkspaceRuntimeError::PaneMissing)?;
        if pane.session_id.is_some() || pane.phase == RuntimePanePhase::Closing {
            return Err(WorkspaceRuntimeError::PaneNotReady);
        }
        self.panes.remove(pane_id);
        Ok(true)
    }

    /// 返回 pane 当前不可变投影供 Root 构造 WorkspaceRender，不把 entity 或 snapshot 缓存在 controller。
    pub fn panes(&self) -> impl Iterator<Item = (&PaneId, &RuntimeTerminalPane)> {
        self.panes.iter()
    }

    /// 按 pane identity 只读运行态，根视图用它生成 renderer 投影而不复制 session 映射。
    pub fn pane(&self, pane_id: &PaneId) -> Option<&RuntimeTerminalPane> {
        self.panes.get(pane_id)
    }

    /// 找到拥有该运行态 session 的 pane，避免根据活动 Tab 猜测输入或关闭目标。
    pub fn pane_for_session(&self, session_id: &SessionId) -> Option<&PaneId> {
        self.pane_by_session.get(session_id)
    }

    /// 通过 pane 明确取 SessionId；pending/failed pane 返回 None 而不回退到其它终端。
    pub fn session_for_pane(&self, pane_id: &PaneId) -> Option<&SessionId> {
        self.panes.get(pane_id)?.session_id.as_ref()
    }
}

/// Verifies the frozen binding's pane, tab, and session identity before it is stored or reused.
///
/// @author kongweiguang
fn ui_binding_matches_runtime_pane(
    pane_id: &PaneId,
    pane: &RuntimeTerminalPane,
    binding: &UiSessionBinding,
) -> bool {
    pane.session_id.as_ref() == Some(&binding.session_id)
        && binding.owner
            == (SessionOwner::Ui {
                tab_id: pane.tab_id.clone(),
                pane_id: pane_id.clone(),
            })
}

/// Checks that a pane still owns its naturally retired exact history binding.
///
/// @author kongweiguang
fn retired_history_binding_matches(
    pane_id: &PaneId,
    pane: &RuntimeTerminalPane,
    binding: &UiSessionBinding,
) -> bool {
    pane.retired_history_binding.as_ref() == Some(binding)
        && ui_binding_matches_runtime_pane(pane_id, pane, binding)
}

/// Read-only pane history remains available after close failure only with the exact naturally retired ticket.
///
/// @author kongweiguang
fn runtime_pane_has_view_access(pane_id: &PaneId, pane: &RuntimeTerminalPane) -> bool {
    pane.phase == RuntimePanePhase::Ready
        || (pane.phase == RuntimePanePhase::CloseFailed
            && pane
                .retired_history_binding
                .as_ref()
                .is_some_and(|binding| ui_binding_matches_runtime_pane(pane_id, pane, binding)))
}

impl Default for WorkspaceRuntime {
    /// 没有有效 session 文档时以空映射开始，Root 再显式载入 Missing/Loaded 用例结果。
    fn default() -> Self {
        Self {
            panes: HashMap::new(),
            pane_by_session: HashMap::new(),
            layouts_by_tab: HashMap::new(),
            orphan_cleanup: HashMap::new(),
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_workspace_runtime.rs"]
mod tests;
