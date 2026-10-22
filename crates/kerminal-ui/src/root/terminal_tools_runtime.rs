//! 按真实 Pane/Agent 所有者隔离终端工具异步回执。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};

use gpui_kit::{App, Context, Entity, EntityId, Task, Window};
use kerminal_application::{
    AppError,
    sessions::{UiTerminalSearchCommitGate, UiTerminalSearchRequest, UiTerminalSearchViewport},
};
use kerminal_domain::identity::{PaneId, SessionId};
use kerminal_terminal::snapshot::{SelectionViewport, TerminalSearchRequest, TerminalSelection};
use tokio_util::sync::CancellationToken;

use crate::features::terminal_tools::{TerminalToolsEvent, TerminalToolsView};

use super::RootView;

/// 工具请求的所有者包含稳定视图身份和资源代次，避免迟到结果落到复用的 Pane 上。
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct TerminalToolsOwner {
    pub(super) source_id: EntityId,
    pub(super) session_id: SessionId,
    scope: TerminalToolsOwnerScope,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum TerminalToolsOwnerScope {
    Workspace { pane_id: PaneId, generation: u64 },
    Agent { generation: u64 },
}

/// 同一视图内可独立取消或串行化的终端工具操作。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum TerminalToolsRequestKind {
    Search,
    Selection,
    SelectionAnchor,
    Copy,
    Broadcast,
    LogMutation,
    LogRead,
}

/// 每次请求的 generation 与来源快照，异步回投必须同时匹配二者。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct TerminalToolsRequest {
    pub(super) owner: TerminalToolsOwner,
    kind: TerminalToolsRequestKind,
    generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TerminalToolsRequestPolicy {
    /// 新的读取/选择取代旧请求；丢弃 GPUI task 会触发 executor 的协作取消。
    ReplaceCancellable,
    /// 日志启动/停止已有明确结果前拒绝重复状态变更。
    RejectWhileRunning,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestKey {
    source_id: EntityId,
    kind: TerminalToolsRequestKind,
}

struct ActiveRequest {
    owner: TerminalToolsOwner,
    generation: u64,
    correlation: Option<u64>,
    gesture_id: Option<u64>,
    cancellable: bool,
    running: bool,
    cancellation: Option<CancellationToken>,
    search_commit_gate: Option<UiTerminalSearchCommitGate>,
    selection_ack: Option<SelectionAcknowledgement>,
    _task: Option<Task<()>>,
}

/// 把选择端点、手势阶段和产生坐标的证据绑定成一个事件包，避免入口漏传或错配。
struct TerminalSelectionUpdate {
    selection: Option<TerminalSelection>,
    gesture_id: Option<u64>,
    started: bool,
    finalized: bool,
    expected_revision: Option<u64>,
    viewport: Option<SelectionViewport>,
}

/// 以原子结果配合一次性 cancellation wakeup，避免 UI crate 引入第二套 async channel。
#[derive(Clone)]
struct SelectionAcknowledgement {
    result: Arc<AtomicU8>,
    completed: CancellationToken,
}

impl SelectionAcknowledgement {
    /// 新建未完成状态；独立 cancellation wakeup 让等待者无需持锁轮询。
    fn new() -> Self {
        Self {
            result: Arc::new(AtomicU8::new(0)),
            completed: CancellationToken::new(),
        }
    }

    /// 用单次结果唤醒所有等待者，重复完成仍保持幂等可读。
    fn finish(&self, accepted: bool) {
        self.result
            .store(if accepted { 1 } else { 2 }, Ordering::Release);
        self.completed.cancel();
    }

    /// 读取已确认或拒绝的终态；零值代表 service 尚未返回。
    fn result(&self) -> Option<bool> {
        match self.result.load(Ordering::Acquire) {
            1 => Some(true),
            2 => Some(false),
            _ => None,
        }
    }

    /// 等待 service 回执或调用方取消，避免 Copy 读取上一轮拖选的文本。
    async fn wait(&self, cancellation: CancellationToken) -> Result<(), AppError> {
        let result = cancellation
            .run_until_cancelled(async {
                if self.result().is_none() {
                    self.completed.cancelled().await;
                }
                self.result()
            })
            .await;
        match result.flatten() {
            Some(true) => Ok(()),
            Some(false) => Err(AppError::cancelled("终端选择尚未被服务端确认")),
            None => Err(AppError::cancelled("复制终端选择已取消")),
        }
    }
}

/// Root 持有唯一 runtime；每个来源和操作最多保留一个受监督的 UI task。
#[derive(Default)]
pub(super) struct TerminalToolsRuntime {
    next_generation: u64,
    active: HashMap<RequestKey, ActiveRequest>,
    confirmed_selection_present: HashSet<TerminalToolsOwner>,
}

impl TerminalToolsRuntime {
    /// 创建本轮请求身份；仅允许可取消的查询被新请求替换，日志状态变更必须先收敛。
    pub(super) fn begin(
        &mut self,
        owner: TerminalToolsOwner,
        kind: TerminalToolsRequestKind,
        correlation: Option<u64>,
        policy: TerminalToolsRequestPolicy,
    ) -> Option<TerminalToolsRequest> {
        self.begin_with_gesture_id(owner, kind, correlation, policy, None)
    }

    /// 将拖动请求绑定手势代次，并由新 anchor 原子失效旧 endpoint，避免调用方遗漏跨 kind 清理。
    /// Anchor ACK 不受同手势 Move 替换影响；新 MouseDown 才取消此前 endpoint。
    fn begin_with_gesture_id(
        &mut self,
        owner: TerminalToolsOwner,
        kind: TerminalToolsRequestKind,
        correlation: Option<u64>,
        policy: TerminalToolsRequestPolicy,
        gesture_id: Option<u64>,
    ) -> Option<TerminalToolsRequest> {
        if kind == TerminalToolsRequestKind::SelectionAnchor {
            let previous_endpoint_key = RequestKey {
                source_id: owner.source_id,
                kind: TerminalToolsRequestKind::Selection,
            };
            if let Some(previous_endpoint) = self.active.remove(&previous_endpoint_key) {
                cancel_active_request(&previous_endpoint);
            }
        }

        let key = RequestKey {
            source_id: owner.source_id,
            kind,
        };
        if let Some(current) = self.active.get(&key)
            && current.running
            && !current.cancellable
        {
            return None;
        }
        if let Some(previous) = self.active.remove(&key) {
            cancel_active_request(&previous);
        }

        self.next_generation = self.next_generation.wrapping_add(1).max(1);
        let generation = self.next_generation;
        let cancellable = policy == TerminalToolsRequestPolicy::ReplaceCancellable;
        let selection_ack = matches!(
            kind,
            TerminalToolsRequestKind::Selection | TerminalToolsRequestKind::SelectionAnchor
        )
        .then(SelectionAcknowledgement::new);
        self.active.insert(
            key,
            ActiveRequest {
                owner: owner.clone(),
                generation,
                correlation,
                gesture_id,
                cancellable,
                running: true,
                cancellation: cancellable.then(CancellationToken::new),
                search_commit_gate: (kind == TerminalToolsRequestKind::Search)
                    .then(UiTerminalSearchCommitGate::new),
                selection_ack,
                _task: None,
            },
        );
        Some(TerminalToolsRequest {
            owner,
            kind,
            generation,
        })
    }

    /// 保存 task 句柄以支持替换或关闭时协作取消；失效 ticket 的 task 立即被丢弃。
    pub(super) fn install_task(&mut self, request: &TerminalToolsRequest, task: Task<()>) {
        let Some(active) = self.active.get_mut(&request.key()) else {
            return;
        };
        if active.generation != request.generation || active.owner != request.owner {
            return;
        }
        active._task = Some(task);
    }

    /// 标记工作结束但暂存已完成句柄，避免在其自身回调栈内销毁当前 GPUI task。
    pub(super) fn finish(&mut self, request: &TerminalToolsRequest) {
        if let Some(active) = self.current_mut(request) {
            active.running = false;
        }
    }

    /// 只有 application 已接受同一 view binding 的 selection 后才 ACK，供紧随其后的 Copy 等待。
    fn finish_selection(&mut self, request: &TerminalToolsRequest, accepted: bool) {
        if let Some(active) = self.current_mut(request) {
            if let Some(ack) = &active.selection_ack {
                ack.finish(accepted);
            }
            active.running = false;
        }
    }

    /// 返回最新 selection 的 ACK 状态；没有历史写入时 Copy 可直接读取当前已确认选择。
    fn selection_ack(&self, owner: &TerminalToolsOwner) -> Option<SelectionAcknowledgement> {
        [
            TerminalToolsRequestKind::Selection,
            TerminalToolsRequestKind::SelectionAnchor,
        ]
        .into_iter()
        .filter_map(|kind| {
            self.active.get(&RequestKey {
                source_id: owner.source_id,
                kind,
            })
        })
        .filter(|active| active.owner == *owner)
        .max_by_key(|active| active.generation)
        .and_then(|active| active.selection_ack.as_ref())
        .cloned()
    }

    /// Move/End 只能等待同一手势的 MouseDown 回执，不得误用下一次拖动的 anchor。
    fn selection_anchor_ack(
        &self,
        owner: &TerminalToolsOwner,
        gesture_id: u64,
    ) -> Option<SelectionAcknowledgement> {
        self.active
            .get(&RequestKey {
                source_id: owner.source_id,
                kind: TerminalToolsRequestKind::SelectionAnchor,
            })
            .filter(|active| active.owner == *owner && active.gesture_id == Some(gesture_id))
            .and_then(|active| active.selection_ack.as_ref())
            .cloned()
    }

    /// MouseUp 后 snapshot 可能尚未刷新；未 ACK 的最终选择仍应让紧随的 Ctrl+C 进入复制路径。
    pub(super) fn has_pending_selection(&self, owner: &TerminalToolsOwner) -> bool {
        [
            TerminalToolsRequestKind::Selection,
            TerminalToolsRequestKind::SelectionAnchor,
        ]
        .into_iter()
        .any(|kind| {
            self.active
                .get(&RequestKey {
                    source_id: owner.source_id,
                    kind,
                })
                .is_some_and(|active| {
                    active.owner == *owner
                        && active.running
                        && active
                            .selection_ack
                            .as_ref()
                            .is_some_and(|ack| ack.result().is_none())
                })
        })
    }

    /// 最终选择 ACK 到新快照之间沿用已确认文本状态，避免 Ctrl+C 把刚完成的拖选误判为中断。
    pub(super) fn selection_may_be_present(&self, owner: &TerminalToolsOwner) -> bool {
        self.confirmed_selection_present.contains(owner) || self.has_pending_selection(owner)
    }

    /// 仅在 service 已确认新手势或最终文本后更新 marker；失败请求不得抹掉旧的有效选择。
    pub(super) fn set_confirmed_selection_present(
        &mut self,
        owner: &TerminalToolsOwner,
        present: bool,
    ) {
        if present {
            self.confirmed_selection_present.insert(owner.clone());
        } else {
            self.confirmed_selection_present.remove(owner);
        }
    }

    /// 判断请求是否仍是该来源/操作的最后一轮，防止旧回执覆盖新状态。
    pub(super) fn is_current(&self, request: &TerminalToolsRequest) -> bool {
        self.current(request).is_some()
    }

    /// 让替换/关闭请求取消后端的大历史扫描，而不只丢弃旧结果的 UI 回执。
    pub(super) fn cancellation_token(
        &self,
        request: &TerminalToolsRequest,
    ) -> Option<CancellationToken> {
        self.current(request)?.cancellation.clone()
    }

    /// 返回与当前搜索 ticket 同生命周期的提交门，使关闭和替换同步在线性化点前撤销定位。
    ///
    /// @author kongweiguang
    fn search_commit_gate(
        &self,
        request: &TerminalToolsRequest,
    ) -> Option<UiTerminalSearchCommitGate> {
        self.current(request)?.search_commit_gate.clone()
    }

    /// 区分日志读取和日志状态变更，避免旧读取回执覆盖刚完成的 start/stop。
    pub(super) fn is_running(
        &self,
        owner: &TerminalToolsOwner,
        kind: TerminalToolsRequestKind,
    ) -> bool {
        let key = RequestKey {
            source_id: owner.source_id,
            kind,
        };
        self.active
            .get(&key)
            .is_some_and(|active| active.owner == *owner && active.running)
    }

    /// 将搜索命中导航绑定到产生它的搜索 request，而不是仅按 SessionId 接受迟到坐标。
    pub(super) fn is_search_current(&self, owner: &TerminalToolsOwner, request_id: u64) -> bool {
        let key = RequestKey {
            source_id: owner.source_id,
            kind: TerminalToolsRequestKind::Search,
        };
        self.active
            .get(&key)
            .is_some_and(|active| active.owner == *owner && active.correlation == Some(request_id))
    }

    /// 取消某个视图操作；非取消型日志写入继续等待明确回执。
    pub(super) fn cancel(&mut self, owner: &TerminalToolsOwner, kind: TerminalToolsRequestKind) {
        let key = RequestKey {
            source_id: owner.source_id,
            kind,
        };
        let cancellable = self
            .active
            .get(&key)
            .is_some_and(|active| active.owner == *owner && active.cancellable);
        if cancellable && let Some(active) = self.active.remove(&key) {
            cancel_active_request(&active);
        }
    }

    /// 手势失效只取消匹配 gesture id 的 anchor 与 endpoint，保留已被 engine 确认的当前范围。
    fn cancel_selection_gesture(&mut self, owner: &TerminalToolsOwner, gesture_id: u64) {
        for kind in [
            TerminalToolsRequestKind::Selection,
            TerminalToolsRequestKind::SelectionAnchor,
        ] {
            let key = RequestKey {
                source_id: owner.source_id,
                kind,
            };
            let should_cancel = self.active.get(&key).is_some_and(|active| {
                active.owner == *owner
                    && active.gesture_id == Some(gesture_id)
                    && active.cancellable
            });
            if should_cancel && let Some(active) = self.active.remove(&key) {
                cancel_active_request(&active);
            }
        }
    }

    /// 资源关闭时取消该 SessionId 的读取任务，但保留日志 start/stop 的结果所有者。
    pub(super) fn cancel_session(&mut self, session_id: &SessionId) {
        self.confirmed_selection_present
            .retain(|owner| &owner.session_id != session_id);
        self.active.retain(|_, active| {
            let keep = &active.owner.session_id != session_id || !active.cancellable;
            if !keep {
                cancel_active_request(active);
            }
            keep
        });
    }

    /// Agent 或 Pane 的工具视图重新绑定时取消其旧读取任务。
    pub(super) fn cancel_source(&mut self, source_id: EntityId) {
        self.confirmed_selection_present
            .retain(|owner| owner.source_id != source_id);
        self.active.retain(|key, active| {
            let keep = key.source_id != source_id || !active.cancellable;
            if !keep {
                cancel_active_request(active);
            }
            keep
        });
    }

    fn current(&self, request: &TerminalToolsRequest) -> Option<&ActiveRequest> {
        self.active.get(&request.key()).filter(|active| {
            active.generation == request.generation && active.owner == request.owner
        })
    }

    fn current_mut(&mut self, request: &TerminalToolsRequest) -> Option<&mut ActiveRequest> {
        self.active.get_mut(&request.key()).filter(|active| {
            active.generation == request.generation && active.owner == request.owner
        })
    }
}

impl Drop for TerminalToolsRuntime {
    /// Root 可能绕过逐来源清理直接退出；丢弃任务前显式取消读取并释放等待 selection ACK 的副本。
    fn drop(&mut self) {
        for active in self.active.values() {
            cancel_active_request(active);
        }
    }
}

/// 取消请求时同时唤醒依赖它的 Copy；避免等待者在来源关闭后悬挂或读旧选择。
///
/// @author kongweiguang
fn cancel_active_request(active: &ActiveRequest) {
    if let Some(commit_gate) = &active.search_commit_gate {
        commit_gate.cancel();
    }
    if let Some(cancellation) = &active.cancellation {
        cancellation.cancel();
    }
    if let Some(ack) = &active.selection_ack {
        ack.finish(false);
    }
}

impl TerminalToolsRequest {
    /// 请求键不包含 generation，确保同一来源的新请求能替换旧 task。
    fn key(&self) -> RequestKey {
        RequestKey {
            source_id: self.owner.source_id,
            kind: self.kind,
        }
    }
}

#[derive(Clone, Copy)]
enum SearchIntent {
    Reveal {
        selected_match: Option<usize>,
        safe_top_rows: u16,
    },
    PreserveViewport {
        selected_match: Option<usize>,
    },
}

impl RootView {
    /// 只向仍由当前 owner 持有且搜索栏可见的同会话工具视图发视图 revision 刷新意图。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_open_terminal_search_for_view_revision(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(binding) = self.terminal_view_bindings.get(session_id).cloned() else {
            return;
        };
        if self.terminal_snapshot_for_binding(&binding).is_none() {
            return;
        }

        let mut sources = Vec::new();
        if let Some(source) = self.workspace_tools.get(session_id) {
            sources.push(source.clone());
        }
        if self.agent_terminal_session_id.as_ref() == Some(session_id)
            && let Some(source) = self.agent_terminal_tools.as_ref()
        {
            sources.push(source.clone());
        }
        let mut seen = HashSet::new();
        for source in sources {
            if !seen.insert(source.entity_id()) {
                continue;
            }
            let Some(owner) = resolve_owner(self, &source, session_id, cx) else {
                continue;
            };
            if !owner_is_current(self, &owner, cx) {
                continue;
            }
            source.update(cx, |tools, cx| {
                tools.refresh_search_after_view_revision(session_id, cx);
            });
        }
    }

    /// 按来源工具实体分发事件；每个异步回执保留 session、pane/agent 与请求代次，不承接画布鼠标写入。
    ///
    /// @author kongweiguang
    pub(super) fn handle_terminal_tools_event_from(
        &mut self,
        source: Entity<TerminalToolsView>,
        event: &TerminalToolsEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            TerminalToolsEvent::Search {
                session_id,
                request_id,
                request,
                navigate,
                safe_top_rows,
            } => {
                if !self.search_from_tools(
                    source.clone(),
                    session_id,
                    *request_id,
                    request.clone(),
                    if *navigate {
                        SearchIntent::Reveal {
                            selected_match: None,
                            safe_top_rows: *safe_top_rows,
                        }
                    } else {
                        SearchIntent::PreserveViewport {
                            selected_match: None,
                        }
                    },
                    cx,
                ) {
                    source.update(cx, |tools, cx| {
                        tools.set_search_busy(session_id, *request_id, false, cx);
                    });
                }
            }
            TerminalToolsEvent::SelectMatch {
                session_id,
                request_id,
                request,
                selected_match,
                safe_top_rows,
            } => {
                if !self.search_from_tools(
                    source.clone(),
                    session_id,
                    *request_id,
                    request.clone(),
                    SearchIntent::Reveal {
                        selected_match: Some(*selected_match),
                        safe_top_rows: *safe_top_rows,
                    },
                    cx,
                ) {
                    source.update(cx, |tools, cx| {
                        tools.set_search_busy(session_id, *request_id, false, cx);
                    });
                }
            }
            TerminalToolsEvent::RefreshSearch {
                session_id,
                request_id,
                request,
                selected_match,
            } => {
                if !self.search_from_tools(
                    source.clone(),
                    session_id,
                    *request_id,
                    request.clone(),
                    SearchIntent::PreserveViewport {
                        selected_match: *selected_match,
                    },
                    cx,
                ) {
                    source.update(cx, |tools, cx| {
                        tools.set_search_busy(session_id, *request_id, false, cx);
                    });
                }
            }
            TerminalToolsEvent::ClearSearch { session_id } => {
                if let Some(owner) = resolve_owner(self, &source, session_id, cx) {
                    self.terminal_tools_runtime
                        .cancel(&owner, TerminalToolsRequestKind::Search);
                }
            }
            TerminalToolsEvent::CloseSearch { session_id } => {
                if let Some(owner) = resolve_owner(self, &source, session_id, cx) {
                    self.terminal_tools_runtime
                        .cancel(&owner, TerminalToolsRequestKind::Search);
                    if let Some(binding) = self.terminal_view_bindings.get(session_id).cloned() {
                        self.restore_terminal_focus_after_search_close(&binding, window, cx);
                    }
                }
            }
            TerminalToolsEvent::SetSelection {
                session_id,
                selection,
                gesture_id,
                started,
                finalized,
                expected_revision,
                viewport,
            } => {
                self.update_selection_from_tools(
                    source,
                    session_id,
                    TerminalSelectionUpdate {
                        selection: *selection,
                        gesture_id: *gesture_id,
                        started: *started,
                        finalized: *finalized,
                        expected_revision: *expected_revision,
                        viewport: viewport.clone(),
                    },
                    cx,
                );
            }
            TerminalToolsEvent::CancelSelectionGesture {
                session_id,
                gesture_id,
            } => {
                if let Some(owner) = resolve_owner(self, &source, session_id, cx) {
                    self.terminal_tools_runtime
                        .cancel_selection_gesture(&owner, *gesture_id);
                }
            }
            TerminalToolsEvent::ClearSelection { session_id } => self.update_selection_from_tools(
                source,
                session_id,
                TerminalSelectionUpdate {
                    selection: None,
                    gesture_id: None,
                    started: false,
                    finalized: true,
                    expected_revision: None,
                    viewport: None,
                },
                cx,
            ),
            TerminalToolsEvent::CopySelection { session_id } => {
                self.copy_selection_from_tools(source, session_id, cx);
            }
            TerminalToolsEvent::OpenLink { link } => {
                if source_is_current(self, &source, cx) {
                    cx.open_url(link.as_str());
                }
            }
            TerminalToolsEvent::BroadcastStart { .. }
            | TerminalToolsEvent::BroadcastStop
            | TerminalToolsEvent::BroadcastCancel
            | TerminalToolsEvent::LogStart { .. }
            | TerminalToolsEvent::LogStop { .. }
            | TerminalToolsEvent::LogRefresh { .. } => {
                super::terminal_tool_actions::handle_terminal_tools_event_from(
                    self, source, event, window, cx,
                );
            }
            // 命令块订阅在调用本 dispatcher 前已处理该变体；此处拒绝任何迟到的重复路由。
            TerminalToolsEvent::CommandBlockAction { .. } => {}
        }
    }

    /// 输入预览与显式定位共用带提交门的 ticket；坐标只在匹配 revision 时回投。
    ///
    /// @author kongweiguang
    fn search_from_tools(
        &mut self,
        source: Entity<TerminalToolsView>,
        session_id: &SessionId,
        request_id: u64,
        request: TerminalSearchRequest,
        intent: SearchIntent,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(owner) = resolve_owner(self, &source, session_id, cx) else {
            return false;
        };
        let Some(binding) = self.terminal_view_bindings.get(session_id).cloned() else {
            return false;
        };
        if binding.session_id != *session_id
            || self.terminal_snapshot_for_binding(&binding).is_none()
        {
            return false;
        }
        let Some(ticket) = self.terminal_tools_runtime.begin(
            owner,
            TerminalToolsRequestKind::Search,
            Some(request_id),
            TerminalToolsRequestPolicy::ReplaceCancellable,
        ) else {
            return false;
        };
        let Some(caller_cancellation) = self.terminal_tools_runtime.cancellation_token(&ticket)
        else {
            self.terminal_tools_runtime
                .cancel(&ticket.owner, TerminalToolsRequestKind::Search);
            return false;
        };
        let Some(commit_gate) = self.terminal_tools_runtime.search_commit_gate(&ticket) else {
            self.terminal_tools_runtime
                .cancel(&ticket.owner, TerminalToolsRequestKind::Search);
            return false;
        };
        let session_id = session_id.clone();
        let service = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        source.update(cx, |tools, cx| {
            tools.set_search_busy(&session_id, request_id, true, cx);
        });

        let callback_ticket = ticket.clone();
        let callback_owner = ticket.owner.clone();
        let callback_binding = binding.clone();
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(caller_cancellation, move |cancellation| async move {
                    let (selected_match, viewport) = match intent {
                        SearchIntent::Reveal {
                            selected_match,
                            safe_top_rows,
                        } => (
                            selected_match,
                            UiTerminalSearchViewport::Reveal { safe_top_rows },
                        ),
                        SearchIntent::PreserveViewport { selected_match } => {
                            (selected_match, UiTerminalSearchViewport::PreserveViewport)
                        }
                    };
                    service
                        .search_ui_cancellable(
                            binding,
                            UiTerminalSearchRequest {
                                request,
                                selected_match,
                                viewport,
                                commit_gate,
                            },
                            cancellation,
                        )
                        .await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.terminal_tools_runtime.is_current(&callback_ticket)
                    || !root
                        .terminal_tools_runtime
                        .is_search_current(&callback_owner, request_id)
                {
                    return;
                }
                root.terminal_tools_runtime.finish(&callback_ticket);
                if !owner_is_current(root, &callback_owner, cx)
                    || root.terminal_view_bindings.get(&session_id) != Some(&callback_binding)
                {
                    source.update(cx, |tools, cx| {
                        tools.set_search_busy(&session_id, request_id, false, cx);
                    });
                    return;
                }
                source.update(cx, |tools, cx| {
                    tools.set_search_busy(&session_id, request_id, false, cx);
                    match result {
                        Ok(result) => {
                            tools.set_search_result(
                                &session_id,
                                request_id,
                                result.result,
                                result.revision,
                                result.reveal_safe_margin_reached,
                                cx,
                            );
                        }
                        Err(error) => tools.set_search_error(
                            &session_id,
                            request_id,
                            error.public_message(),
                            cx,
                        ),
                    }
                });
                root.request_visible_snapshot_for_session(&session_id, cx);
            });
        });
        self.terminal_tools_runtime.install_task(&ticket, task);
        true
    }

    /// 选择事件整体携带手势阶段与网格证据；viewport 优先，旧事件才退回 revision CAS。
    ///
    /// @author kongweiguang
    fn update_selection_from_tools(
        &mut self,
        source: Entity<TerminalToolsView>,
        session_id: &SessionId,
        update: TerminalSelectionUpdate,
        cx: &mut Context<Self>,
    ) {
        let TerminalSelectionUpdate {
            selection,
            gesture_id,
            started,
            finalized,
            expected_revision,
            viewport,
        } = update;
        let Some(owner) = resolve_owner(self, &source, session_id, cx) else {
            return;
        };
        let Some(binding) = self.terminal_view_bindings.get(session_id).cloned() else {
            return;
        };
        if binding.session_id != *session_id
            || self.terminal_snapshot_for_binding(&binding).is_none()
        {
            return;
        }
        if started
            && (expected_revision.is_none() && viewport.is_none()
                || !selection.is_some_and(|selection| selection.start == selection.end))
        {
            return;
        }
        // revision 只保护 MouseDown 的新 anchor；已接受手势的 Move/End 沿用原 anchor。
        let expected_revision = (started && viewport.is_none())
            .then_some(expected_revision)
            .flatten();
        let anchor_ack = match (started, gesture_id) {
            (true, Some(_)) => None,
            (false, Some(gesture_id)) => {
                let Some(ack) = self
                    .terminal_tools_runtime
                    .selection_anchor_ack(&owner, gesture_id)
                else {
                    return;
                };
                Some(ack)
            }
            (false, None) => None,
            (true, None) => return,
        };
        self.terminal_tools_runtime
            .cancel(&owner, TerminalToolsRequestKind::Copy);
        if started || gesture_id.is_none() {
            self.terminal_tools_runtime
                .cancel(&owner, TerminalToolsRequestKind::Selection);
        }
        if gesture_id.is_none() {
            self.terminal_tools_runtime
                .cancel(&owner, TerminalToolsRequestKind::SelectionAnchor);
        }
        let kind = if started {
            TerminalToolsRequestKind::SelectionAnchor
        } else {
            TerminalToolsRequestKind::Selection
        };
        let Some(ticket) = self.terminal_tools_runtime.begin_with_gesture_id(
            owner.clone(),
            kind,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
            gesture_id,
        ) else {
            return;
        };
        let copy_on_selection = finalized
            && selection.is_some_and(|selection| {
                selection.start != selection.end
                    || selection.kind
                        != kerminal_terminal::snapshot::TerminalSelectionKind::Character
            })
            && super::preferences::interaction_policy(self)
                .is_some_and(|policy| policy.copy_on_selection());
        let started_selection_kind = selection.map(|selection| selection.kind);
        let sessions = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let session_id = session_id.clone();
        let callback_binding = binding.clone();
        let selection_binding = binding.clone();
        let callback_ticket = ticket.clone();
        let callback_owner = owner.clone();
        let Some(caller_cancellation) = self.terminal_tools_runtime.cancellation_token(&ticket)
        else {
            self.terminal_tools_runtime
                .cancel(&owner, TerminalToolsRequestKind::Selection);
            return;
        };
        let task = cx.spawn(async move |this, cx| {
            let result = executor
                .run_cancellable(caller_cancellation, move |cancellation| async move {
                    if let Some(anchor_ack) = anchor_ack {
                        wait_for_selection_ack(anchor_ack, cancellation.clone()).await?;
                    }
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端选择更新已取消"));
                    }
                    if let Some(viewport) = viewport {
                        sessions
                            .set_ui_selection_cancellable_in_viewport(
                                binding,
                                selection,
                                gesture_id,
                                viewport,
                                cancellation.clone(),
                            )
                            .await?;
                    } else {
                        sessions
                            .set_ui_selection_cancellable_with_revision(
                                binding,
                                selection,
                                gesture_id,
                                expected_revision,
                                cancellation.clone(),
                            )
                            .await?;
                    }
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端选择更新已取消"));
                    }
                    if finalized {
                        sessions.selected_text_ui(selection_binding).await
                    } else {
                        Ok(None)
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.terminal_tools_runtime.is_current(&callback_ticket) {
                    return;
                }
                let accepted = result.is_ok()
                    && owner_is_current(root, &callback_owner, cx)
                    && root.terminal_view_bindings.get(&session_id) == Some(&callback_binding);
                root.terminal_tools_runtime
                    .finish_selection(&callback_ticket, accepted);
                if !accepted {
                    if let Err(error) = &result
                        && error.public_code()
                            != kerminal_application::PublicErrorCode::Cancelled
                    {
                        root.status = Some(error.public_message().into());
                        if let Some(gesture_id) = gesture_id {
                            root.terminal_tools_runtime
                                .cancel_selection_gesture(&callback_owner, gesture_id);
                            source.update(cx, |tools, cx| {
                                tools.cancel_canvas_selection_gesture_if(gesture_id, cx);
                            });
                        } else {
                            root.terminal_tools_runtime
                                .cancel(&callback_owner, TerminalToolsRequestKind::Selection);
                        }
                        if gesture_id.is_some()
                            && matches!(
                                &result,
                                Err(error)
                                    if error.public_code()
                                        == kerminal_application::PublicErrorCode::Conflict
                            )
                        {
                            root.request_visible_snapshot_for_session(&session_id, cx);
                        }
                        cx.notify();
                    }
                    return;
                }
                match result {
                    Ok(text) => {
                        if started {
                            // The accepted anchor clears an old character range; word/line starts can already select content.
                            root.terminal_tools_runtime
                                .set_confirmed_selection_present(
                                    &callback_owner,
                                    started_selection_kind.is_some_and(|kind| {
                                        kind != kerminal_terminal::snapshot::TerminalSelectionKind::Character
                                    }),
                                );
                        } else if finalized {
                            root.terminal_tools_runtime
                                .set_confirmed_selection_present(
                                    &callback_owner,
                                    text.as_ref().is_some_and(|text| !text.is_empty()),
                                );
                        }
                        if finalized {
                            let selected_chars = text.as_ref().map(|text| text.chars().count());
                            if copy_on_selection
                                && let Some(text) = text
                                && !write_selection_clipboard(cx, text)
                            {
                                root.status = Some(crate::locale::text(
                                    "剪贴板未确认复制结果，选择已保留，可重试复制。",
                                    "Clipboard copy was not confirmed; the selection is kept so you can retry.",
                                ));
                            }
                            source.update(cx, |tools, cx| {
                                tools.set_selection_summary(&session_id, selected_chars, cx);
                            });
                        }
                        root.request_visible_snapshot_for_session(&session_id, cx);
                    }
                    Err(error) => root.status = Some(error.public_message().into()),
                }
                cx.notify();
            });
        });
        self.terminal_tools_runtime.install_task(&ticket, task);
    }

    /// 读取原 UI binding 的选择文本；非 pane 工具目标沿旧 session-ID adapter 保持兼容。
    ///
    /// @author kongweiguang
    fn copy_selection_from_tools(
        &mut self,
        source: Entity<TerminalToolsView>,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        let Some(owner) = resolve_owner(self, &source, session_id, cx) else {
            return;
        };
        let Some(ticket) = self.terminal_tools_runtime.begin(
            owner,
            TerminalToolsRequestKind::Copy,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        ) else {
            return;
        };
        let Some(caller_cancellation) = self.terminal_tools_runtime.cancellation_token(&ticket)
        else {
            self.terminal_tools_runtime
                .cancel(&ticket.owner, TerminalToolsRequestKind::Copy);
            return;
        };
        let selection_ack = self.terminal_tools_runtime.selection_ack(&ticket.owner);
        let service = self.services.sessions.clone();
        let executor = self.services.executor.clone();
        let view_binding = self.terminal_view_bindings.get(session_id).cloned();
        let session_id = session_id.clone();
        let callback_ticket = ticket.clone();
        let callback_owner = ticket.owner.clone();
        let task = cx.spawn(async move |this, cx| {
            let read_session_id = session_id.clone();
            let read_binding = view_binding;
            let result = executor
                .run_cancellable(caller_cancellation, move |cancellation| async move {
                    if let Some(selection_ack) = selection_ack {
                        wait_for_selection_ack(selection_ack, cancellation.clone()).await?;
                    }
                    if cancellation.is_cancelled() {
                        return Err(AppError::cancelled("复制终端选择已取消"));
                    }
                    let result = if let Some(binding) = read_binding {
                        service.selected_text_ui(binding).await
                    } else {
                        service.selected_text(read_session_id).await
                    };
                    if cancellation.is_cancelled() {
                        Err(AppError::cancelled("复制终端选择已取消"))
                    } else {
                        result
                    }
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if !root.terminal_tools_runtime.is_current(&callback_ticket) {
                    return;
                }
                root.terminal_tools_runtime.finish(&callback_ticket);
                if !owner_is_current(root, &callback_owner, cx) {
                    return;
                }
                match result {
                    Ok(Some(text)) => {
                        root.status = if write_selection_clipboard(cx, text) {
                            None
                        } else {
                            Some(crate::locale::text(
                                "剪贴板未确认复制结果，选择已保留，可重试复制。",
                                "Clipboard copy was not confirmed; the selection is kept so you can retry.",
                            ))
                        };
                    }
                    Ok(None) => {
                        root.status = Some(crate::locale::text(
                            "当前没有可复制的终端选择",
                            "There is no terminal selection to copy.",
                        ));
                    }
                    Err(error) => {
                        root.status = Some(if error.public_code()
                            == kerminal_application::PublicErrorCode::Cancelled
                        {
                            crate::locale::text(
                                "终端选择尚未确认，未复制；请确认选择后重试。",
                                "The terminal selection was not confirmed; nothing was copied. Confirm it and retry.",
                            )
                        } else {
                            error.public_message().into()
                        })
                    }
                }
                cx.notify();
            });
        });
        self.terminal_tools_runtime.install_task(&ticket, task);
    }

    /// 搜索/选择后只为当前仍可见的 pane 取新快照；隐藏 Agent 不抢用中央工作区。
    fn request_visible_snapshot_for_session(
        &mut self,
        session_id: &SessionId,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane_id) = self.workspace_runtime.pane_for_session(session_id) {
            let Some(pane) = self.workspace_runtime.pane(pane_id) else {
                return;
            };
            let tab_id = pane.tab_id.clone();
            let pane_id = pane_id.clone();
            self.request_workspace_pane_snapshot(tab_id, pane_id, cx);
        } else if self.focused_agent_terminal
            && self.agent_terminal_session_id.as_ref() == Some(session_id)
        {
            self.request_agent_snapshot(cx);
        }
    }

    /// 组合快照、当前拖选与 service ACK，供快捷键和右键菜单在快照回投期间保持选择可用。
    pub(crate) fn terminal_selection_may_be_present(
        &self,
        binding: &super::UiSessionBinding,
        source: &Entity<TerminalToolsView>,
        cx: &App,
    ) -> bool {
        if self.terminal_view_bindings.get(&binding.session_id) != Some(binding)
            || self.terminal_snapshot_for_binding(binding).is_none()
        {
            return false;
        }
        if source.read(cx).has_active_selection_gesture(binding) {
            return true;
        }
        resolve_owner(self, source, &binding.session_id, cx)
            .is_some_and(|owner| self.terminal_tools_runtime.selection_may_be_present(&owner))
            || self
                .terminal_snapshot_for_binding(binding)
                .is_some_and(|snapshot| snapshot.selection.is_some())
    }
}

/// Copy 必须等最后一次选择更新经 service 接受；取消或失效时拒绝读取旧 selection。
async fn wait_for_selection_ack(
    acknowledgement: SelectionAcknowledgement,
    cancellation: CancellationToken,
) -> Result<(), AppError> {
    acknowledgement.wait(cancellation).await
}

/// GPUI 写入接口不返回平台错误，因此立即回读确认；失败时调用方保留 selection 供重试。
fn write_selection_clipboard(cx: &mut Context<RootView>, text: String) -> bool {
    cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(text.clone()));
    cx.read_from_clipboard()
        .and_then(|item| item.text())
        .is_some_and(|clipboard_text| clipboard_text == text)
}

/// 仅从仍由 Root 持有的 pane/Agent 工具实体建立操作 owner，不接受全局镜像回执。
pub(super) fn resolve_owner(
    root: &RootView,
    source: &Entity<TerminalToolsView>,
    session_id: &SessionId,
    cx: &App,
) -> Option<TerminalToolsOwner> {
    if source.read(cx).session_id() != Some(session_id) {
        return None;
    }
    let source_id = source.entity_id();
    if root.agent_terminal_session_id.as_ref() == Some(session_id)
        && root
            .agent_terminal_tools
            .as_ref()
            .is_some_and(|tools| tools.entity_id() == source_id)
    {
        return Some(TerminalToolsOwner {
            source_id,
            session_id: session_id.clone(),
            scope: TerminalToolsOwnerScope::Agent {
                generation: root.agent_watch_generation,
            },
        });
    }

    if root
        .workspace_tools
        .get(session_id)
        .is_some_and(|tools| tools.entity_id() == source_id)
    {
        let pane_id = root.workspace_runtime.pane_for_session(session_id)?.clone();
        if root.workspace_runtime.session_for_pane(&pane_id) != Some(session_id) {
            return None;
        }
        let generation = root
            .workspace_restore_generations
            .get(&pane_id)
            .copied()
            .unwrap_or_default();
        return Some(TerminalToolsOwner {
            source_id,
            session_id: session_id.clone(),
            scope: TerminalToolsOwnerScope::Workspace {
                pane_id,
                generation,
            },
        });
    }
    None
}

/// 资源身份或 generation 变化后立即拒绝回投，不把结果转投到当前活动 Tab。
pub(super) fn owner_is_current(root: &RootView, owner: &TerminalToolsOwner, cx: &App) -> bool {
    match &owner.scope {
        TerminalToolsOwnerScope::Agent { generation } => {
            root.agent_watch_generation == *generation
                && root.agent_terminal_session_id.as_ref() == Some(&owner.session_id)
                && root
                    .agent_terminal_tools
                    .as_ref()
                    .is_some_and(|tools| tools.entity_id() == owner.source_id)
                && root
                    .agent_terminal_tools
                    .as_ref()
                    .is_some_and(|tools| tools.read(cx).session_id() == Some(&owner.session_id))
        }
        TerminalToolsOwnerScope::Workspace {
            pane_id,
            generation,
        } => {
            root.workspace_restore_generations
                .get(pane_id)
                .copied()
                .unwrap_or_default()
                == *generation
                && root.workspace_runtime.session_for_pane(pane_id) == Some(&owner.session_id)
                && root
                    .workspace_tools
                    .get(&owner.session_id)
                    .is_some_and(|tools| tools.entity_id() == owner.source_id)
                && root
                    .workspace_tools
                    .get(&owner.session_id)
                    .is_some_and(|tools| tools.read(cx).session_id() == Some(&owner.session_id))
        }
    }
}

/// 只允许 Root 当前注册的 pane/Agent 工具视图执行链接或无 session 的广播动作。
pub(super) fn source_is_current(
    root: &RootView,
    source: &Entity<TerminalToolsView>,
    cx: &App,
) -> bool {
    source
        .read(cx)
        .session_id()
        .and_then(|session_id| resolve_owner(root, source, session_id, cx))
        .is_some_and(|owner| owner_is_current(root, &owner, cx))
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_tools_runtime.rs"]
mod tests;
