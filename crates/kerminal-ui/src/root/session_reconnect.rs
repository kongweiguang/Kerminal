//! 前台终端 pane 的显式重连控制；不创建新 session，也不替换既有目标。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui_kit::{Context, Window};
use kerminal_application::{
    AppError, PublicErrorCode, SessionService,
    sessions::{SessionFilter, UiSessionBinding},
};
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget},
};
use tokio_util::sync::CancellationToken;

use super::{RootView, workspace_runtime::RuntimePanePhase};

/// 标题栏唯一边界会话动作；运行中重建 transport 与断开后恢复连接都投影为同一槽位。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionActionProjection {
    pub binding: UiSessionBinding,
    pub busy: bool,
    /// 进行中的尝试用于让标题栏同一按钮取消；稳定态没有可取消目标。
    pub attempt_id: Option<u64>,
    /// 取消已提交但清理回执未到，按钮必须保留禁用态，不能改投新请求。
    pub cancelling: bool,
}

/// 供右栏读取器使用的同轮服务观察；目标与 generation 由 SessionService 一起确认。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionPanelObservation {
    pub binding: UiSessionBinding,
    pub target: SessionTarget,
    pub lifecycle: SessionLifecycle,
}

/// 同一 SessionId 的真实列表信息与 engine generation；二者必须在同一查询轮次吻合。
#[derive(Clone, Debug, Eq, PartialEq)]
struct ReconnectObservation {
    info: SessionInfo,
    session_generation: u64,
}

/// 异步刷新回执只覆盖发起时仍在当前 pane incarnation 中的查询。
#[derive(Clone, Debug, Eq, PartialEq)]
struct RefreshTicket {
    incarnation: u64,
    revision: u64,
    session_id: SessionId,
    owner: SessionOwner,
    target: SessionTarget,
}

/// executor 的取消 token 由后台 task 安装；UI 可能在安装前取消，故用原子标记衔接竞态。
#[derive(Default)]
struct ReconnectCancellation {
    requested: AtomicBool,
    runtime_token: Mutex<Option<CancellationToken>>,
}

impl ReconnectCancellation {
    /// 将 executor token 安装为唯一 transport 取消源，并合并此前已到达的 pane 取消。
    fn install(&self, token: CancellationToken) {
        let mut slot = self
            .runtime_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *slot = Some(token.clone());
        if self.requested.load(Ordering::Acquire) {
            token.cancel();
        }
    }

    /// 只发出协作取消；具体 channel/PTY 清理仍由 SessionService future 持有到 ACK。
    fn cancel(&self) {
        self.requested.store(true, Ordering::Release);
        if let Some(token) = self
            .runtime_token
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            token.cancel();
        }
    }
}

/// 一个 pane 的缓存只服务 renderer；正在执行的 attempt 单独保留到 executor 收到服务 ACK。
struct PaneReconnectState {
    incarnation: u64,
    refresh_revision: u64,
    observation: Option<ReconnectObservation>,
    active_attempt: Option<u64>,
    result: ReconnectResultState,
}

/// 最近一次已完成操作的显示状态；输入/退出码等终端正文不会存入这里。
#[derive(Clone, Debug, Eq, PartialEq)]
enum ReconnectResultState {
    Ready,
    Failed(String),
    Cancelled,
}

/// executor 任务仍负责正在进行的 reconnect 和取消清理；UI 仅保存其投影关联。
struct InFlightReconnect {
    pane_id: PaneId,
    tab_id: TabId,
    incarnation: u64,
    binding: UiSessionBinding,
    target: SessionTarget,
    cancellation: Arc<ReconnectCancellation>,
    cancelling: bool,
}

/// 用操作种类区分同一生命周期控制器内的断线重连与用户显式重启。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionOperationKind {
    Reconnect,
    Restart,
}

/// 按稳定 PaneId 维护重连观察；跨 pane 替换的旧任务不能覆盖新 pane 状态。
#[derive(Default)]
pub(super) struct SessionReconnectController {
    next_identity: u64,
    panes: HashMap<PaneId, PaneReconnectState>,
    in_flight: HashMap<u64, InFlightReconnect>,
}

impl SessionReconnectController {
    /// 开始一轮 live-session 查询并返回用于拒绝迟到结果的 pane incarnation/revision。
    fn begin_refresh(
        &mut self,
        pane_id: &PaneId,
        session_id: SessionId,
        owner: SessionOwner,
        target: SessionTarget,
    ) -> RefreshTicket {
        let incarnation = self.ensure_pane(pane_id).incarnation;
        let state = self.panes.get_mut(pane_id);
        match state {
            Some(state) => {
                state.refresh_revision = state.refresh_revision.wrapping_add(1).max(1);
                RefreshTicket {
                    incarnation,
                    revision: state.refresh_revision,
                    session_id,
                    owner,
                    target,
                }
            }
            None => RefreshTicket {
                incarnation,
                revision: 0,
                session_id,
                owner,
                target,
            },
        }
    }

    /// 仅当前 pane incarnation 的最新查询能替换观察，失败查询会撤下过期的重连按钮。
    fn finish_refresh(
        &mut self,
        pane_id: &PaneId,
        ticket: RefreshTicket,
        result: Result<ReconnectObservation, AppError>,
    ) {
        let Some(state) = self.panes.get_mut(pane_id) else {
            return;
        };
        if state.incarnation != ticket.incarnation || state.refresh_revision != ticket.revision {
            return;
        }
        match result {
            Ok(observation) => {
                if observation.info.session_id != ticket.session_id
                    || observation.info.owner != ticket.owner
                    || observation.info.target != ticket.target
                {
                    state.observation = None;
                    return;
                }
                let became_running = observation.info.lifecycle == SessionLifecycle::Running;
                state.observation = Some(observation);
                if state.active_attempt.is_none() && became_running {
                    state.result = ReconnectResultState::Ready;
                }
            }
            Err(_) => state.observation = None,
        }
    }

    /// 登记与缓存 binding 完全相同的唯一重新连接；运行中走 restart，断开后走 reconnect。
    ///
    /// 两种 service 入口的准入生命周期不同，但都复用同一 pane 锁与 generation 校验，
    /// 因此返回实际操作类型供后台任务选择不会扩大普通 MCP reconnect 的契约。
    ///
    /// @author kongweiguang
    fn begin_attempt(
        &mut self,
        pane_id: &PaneId,
        tab_id: &TabId,
        binding: UiSessionBinding,
        target: SessionTarget,
    ) -> Option<(u64, Arc<ReconnectCancellation>, SessionOperationKind)> {
        let kind = match self
            .panes
            .get(pane_id)?
            .observation
            .as_ref()?
            .info
            .lifecycle
        {
            SessionLifecycle::Running => SessionOperationKind::Restart,
            SessionLifecycle::Exited | SessionLifecycle::Failed => SessionOperationKind::Reconnect,
            _ => return None,
        };
        let (attempt_id, cancellation) =
            self.begin_operation(pane_id, tab_id, binding, target, kind)?;
        Some((attempt_id, cancellation, kind))
    }

    /// 将两类原位服务操作纳入同一 pane 锁，并使操作开始前的 refresh 回执失效。
    ///
    /// @author kongweiguang
    fn begin_operation(
        &mut self,
        pane_id: &PaneId,
        tab_id: &TabId,
        binding: UiSessionBinding,
        target: SessionTarget,
        kind: SessionOperationKind,
    ) -> Option<(u64, Arc<ReconnectCancellation>)> {
        if self
            .in_flight
            .values()
            .any(|attempt| attempt.pane_id == *pane_id)
        {
            return None;
        }
        let state = self.panes.get(pane_id)?;
        let observation = state.observation.as_ref()?;
        let operation_allowed = match kind {
            SessionOperationKind::Reconnect => can_reconnect(&observation.info),
            SessionOperationKind::Restart => can_restart(&observation.info),
        };
        if !observation_matches(observation, tab_id, pane_id, &binding, &target)
            || !operation_allowed
            || state.active_attempt.is_some()
        {
            return None;
        }
        let incarnation = state.incarnation;
        let attempt_id = self.next_identity();
        let cancellation = Arc::new(ReconnectCancellation::default());
        if let Some(state) = self.panes.get_mut(pane_id) {
            state.refresh_revision = state.refresh_revision.wrapping_add(1).max(1);
            state.active_attempt = Some(attempt_id);
            state.result = ReconnectResultState::Ready;
        }
        self.in_flight.insert(
            attempt_id,
            InFlightReconnect {
                pane_id: pane_id.clone(),
                tab_id: tab_id.clone(),
                incarnation,
                binding,
                target,
                cancellation: Arc::clone(&cancellation),
                cancelling: false,
            },
        );
        Some((attempt_id, cancellation))
    }

    /// 只取消完全匹配的 attempt；保留状态与 owner，直到 manager 完成 transport 清理并返回。
    fn cancel_attempt(
        &mut self,
        pane_id: &PaneId,
        binding: &UiSessionBinding,
        attempt_id: u64,
    ) -> bool {
        let Some(attempt) = self.in_flight.get_mut(&attempt_id) else {
            return false;
        };
        if attempt.pane_id != *pane_id || attempt.binding != *binding {
            return false;
        }
        attempt.cancelling = true;
        attempt.cancellation.cancel();
        if let Some(state) = self.panes.get_mut(pane_id)
            && state.incarnation == attempt.incarnation
            && state.active_attempt == Some(attempt_id)
        {
            state.refresh_revision = state.refresh_revision.wrapping_add(1).max(1);
        }
        true
    }

    /// pane 被关闭或替换时撤下可见状态并取消任务，但 in-flight owner 留到 service ACK。
    fn cancel_for_pane(&mut self, pane_id: &PaneId) {
        self.panes.remove(pane_id);
        for attempt in self
            .in_flight
            .values_mut()
            .filter(|attempt| attempt.pane_id == *pane_id)
        {
            attempt.cancelling = true;
            attempt.cancellation.cancel();
        }
    }

    /// 接受已由 Root 对照 runtime pane 核验的 Closed 回执；旧查询失效后只沿用原服务观察身份。
    ///
    /// @author kongweiguang
    fn record_closed_observation(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        session_id: &SessionId,
        target: &SessionTarget,
    ) {
        let owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        let preserved = self
            .panes
            .get(pane_id)
            .and_then(|state| state.observation.as_ref())
            .filter(|observation| {
                observation.info.session_id == *session_id
                    && observation.info.owner == owner
                    && observation.info.target == *target
            })
            .cloned()
            .map(|mut observation| {
                observation.info.lifecycle = SessionLifecycle::Closed;
                observation
            });

        // 先移除旧 incarnation，再恢复缓存，迟到 refresh/ACK 均不能覆盖 Closed 回执。
        self.cancel_for_pane(pane_id);
        if let Some(observation) = preserved {
            self.ensure_pane(pane_id).observation = Some(observation);
        }
    }

    /// 服务操作完成后才释放 in-flight owner；返回可见终态供 Root 决定提示与回焦。
    ///
    /// 过期 incarnation 不得改变当前 pane 的显示状态，因此返回 `None` 而不是伪成功。
    fn finish_attempt(
        &mut self,
        attempt_id: u64,
        result: Result<ReconnectObservation, AppError>,
    ) -> Option<ReconnectResultState> {
        let attempt = self.in_flight.remove(&attempt_id)?;
        let state = self.panes.get_mut(&attempt.pane_id)?;
        if state.incarnation != attempt.incarnation || state.active_attempt != Some(attempt_id) {
            return None;
        }
        state.active_attempt = None;
        match result {
            Ok(observation) => {
                if observation.info.session_id != attempt.binding.session_id
                    || observation.info.target != attempt.target
                    || observation.info.owner != attempt.binding.owner
                    || observation.info.lifecycle != SessionLifecycle::Running
                    || observation.session_generation <= attempt.binding.session_generation
                {
                    state.result = ReconnectResultState::Failed(
                        "重新连接完成后会话身份或代次未能确认。".to_owned(),
                    );
                } else {
                    state.observation = Some(observation);
                    state.result = ReconnectResultState::Ready;
                }
            }
            Err(error) if error.public_code() == PublicErrorCode::Cancelled => {
                state.result = ReconnectResultState::Cancelled;
            }
            Err(error) => {
                state.result = ReconnectResultState::Failed(error.public_message().to_owned());
            }
        }
        Some(state.result.clone())
    }

    /// 为任何可重新连接的既有 session 返回精确绑定；同 pane 操作在途时只暴露同一 attempt。
    ///
    /// @author kongweiguang
    fn action_projection(
        &self,
        pane_id: &PaneId,
        tab_id: &TabId,
        session_id: &SessionId,
        target: &SessionTarget,
        phase: RuntimePanePhase,
    ) -> Option<SessionActionProjection> {
        if phase != RuntimePanePhase::Ready {
            return None;
        }
        if let Some((attempt_id, attempt)) = self.in_flight.iter().find(|(_, attempt)| {
            attempt.pane_id == *pane_id
                && attempt.tab_id == *tab_id
                && attempt.binding.session_id == *session_id
                && attempt.target == *target
                && self
                    .panes
                    .get(pane_id)
                    .is_some_and(|state| attempt.incarnation == state.incarnation)
        }) {
            return Some(SessionActionProjection {
                binding: attempt.binding.clone(),
                busy: true,
                attempt_id: Some(*attempt_id),
                cancelling: attempt.cancelling,
            });
        }
        if self
            .in_flight
            .values()
            .any(|attempt| attempt.pane_id == *pane_id)
        {
            return None;
        }

        let state = self.panes.get(pane_id)?;
        let observation = state.observation.as_ref()?;
        let binding = UiSessionBinding {
            session_id: observation.info.session_id.clone(),
            owner: observation.info.owner.clone(),
            session_generation: observation.session_generation,
        };
        if observation.info.session_id != *session_id
            || observation.info.target != *target
            || !observation_matches(observation, tab_id, pane_id, &binding, target)
            || !can_restart(&observation.info)
        {
            return None;
        }
        Some(SessionActionProjection {
            binding,
            busy: false,
            attempt_id: None,
            cancelling: false,
        })
    }

    /// 仅导出最新且完整匹配 pane 身份的服务观察；重连未收敛时不把旧 generation 借给面板。
    ///
    /// @author kongweiguang
    pub(super) fn panel_observation(
        &self,
        pane_id: &PaneId,
        tab_id: &TabId,
        session_id: &SessionId,
        target: &SessionTarget,
    ) -> Option<SessionPanelObservation> {
        let observation = self.panes.get(pane_id)?.observation.as_ref()?;
        let has_uncancelled_attempt = self
            .in_flight
            .values()
            .any(|attempt| attempt.pane_id == *pane_id && !attempt.cancelling);
        if has_uncancelled_attempt
            || (observation.info.lifecycle != SessionLifecycle::Closed
                && self
                    .in_flight
                    .values()
                    .any(|attempt| attempt.pane_id == *pane_id))
        {
            return None;
        }
        let binding = UiSessionBinding {
            session_id: observation.info.session_id.clone(),
            owner: observation.info.owner.clone(),
            session_generation: observation.session_generation,
        };
        if observation.info.session_id != *session_id
            || observation.info.target != *target
            || !observation_matches(observation, tab_id, pane_id, &binding, target)
        {
            return None;
        }
        Some(SessionPanelObservation {
            binding,
            target: observation.info.target.clone(),
            lifecycle: observation.info.lifecycle,
        })
    }

    /// 创建或取得 pane incarnation；递增值隔离相同 PaneId 的替换生命周期。
    fn ensure_pane(&mut self, pane_id: &PaneId) -> &mut PaneReconnectState {
        if !self.panes.contains_key(pane_id) {
            let incarnation = self.next_identity();
            self.panes.insert(
                pane_id.clone(),
                PaneReconnectState {
                    incarnation,
                    refresh_revision: 0,
                    observation: None,
                    active_attempt: None,
                    result: ReconnectResultState::Ready,
                },
            );
        }
        // 插入分支保证存在；若 HashMap 内部状态异常则返回可替换的空态而不 panic。
        self.panes
            .entry(pane_id.clone())
            .or_insert_with(|| PaneReconnectState {
                incarnation: 0,
                refresh_revision: 0,
                observation: None,
                active_attempt: None,
                result: ReconnectResultState::Ready,
            })
    }

    /// 生成本 controller 内不复用的递增 ID，避免 pane 替换后的迟到 ACK 撞上新操作。
    fn next_identity(&mut self) -> u64 {
        self.next_identity = self.next_identity.wrapping_add(1).max(1);
        self.next_identity
    }
}

impl RootView {
    /// 查询可重连目标的真实 session 与 generation；旧查询不能覆盖 pane 替换结果。
    ///
    /// @author kongweiguang
    pub(super) fn refresh_session_reconnect_state(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
            self.session_reconnect.cancel_for_pane(&pane_id);
            return;
        };
        let Some(session_id) = pane.session_id.clone() else {
            self.session_reconnect.cancel_for_pane(&pane_id);
            return;
        };
        let Some(target) = pane.target.clone() else {
            self.session_reconnect.cancel_for_pane(&pane_id);
            self.refresh_workspace_views(cx);
            return;
        };
        if pane.tab_id != tab_id || pane.phase != RuntimePanePhase::Ready {
            self.session_reconnect.cancel_for_pane(&pane_id);
            return;
        }
        let owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        let ticket = self.session_reconnect.begin_refresh(
            &pane_id,
            session_id.clone(),
            owner.clone(),
            target.clone(),
        );
        let sessions = Arc::clone(&self.services.sessions);
        let executor = self.services.executor.clone();
        let this = cx.weak_entity();
        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |_| async move {
                    read_live_observation(sessions, session_id, owner, target).await
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if root.workspace_pane_matches(&tab_id, &pane_id, &ticket) {
                    root.session_reconnect
                        .finish_refresh(&pane_id, ticket, result);
                    root.refresh_workspace_views(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 为终端标题栏提供唯一重新连接按钮绑定；进行中也投影同一 attempt 供取消。
    ///
    /// @author kongweiguang
    pub(super) fn session_action_projection(
        &self,
        pane_id: &PaneId,
    ) -> Option<SessionActionProjection> {
        let pane = self.workspace_runtime.pane(pane_id)?;
        self.session_reconnect.action_projection(
            pane_id,
            &pane.tab_id,
            pane.session_id.as_ref()?,
            pane.target.as_ref()?,
            pane.phase,
        )
    }

    /// 标题栏断开标记只来自当前 pane 的完整服务观察；启动、重连进行中都不冒充断开。
    /// 断开的终端正文保持只读，用户需要一眼看到唯一的重连入口。
    ///
    /// @author kongweiguang
    pub(super) fn pane_session_disconnected(&self, pane_id: &PaneId) -> bool {
        self.session_reconnect
            .panes
            .get(pane_id)
            .and_then(|state| state.observation.as_ref())
            .is_some_and(|observation| {
                matches!(
                    observation.info.lifecycle,
                    SessionLifecycle::Exited | SessionLifecycle::Failed
                )
            })
    }

    /// 原位重新连接固定 pane 和 owner；运行中重建 transport，断开后恢复连接，对用户保持同一语义。
    ///
    /// 开始即撤销旧输入与未完成选区，避免旧 generation 继续写入；完成、失败和取消都重新读取
    /// 精确 binding；旧目录和快照票据同步失效，不能在新代首帧前从持久化回退继承旧目录。
    /// 回焦意图保留到交接时，只有用户仍停留在原焦点且新绑定可写才恢复输入。
    ///
    /// @author kongweiguang
    pub(super) fn reconnect_workspace_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
            return;
        };
        let Some(target) = pane.target.clone() else {
            self.status = Some("此 pane 当前没有可重连的终端会话。".into());
            cx.notify();
            return;
        };
        let expected_owner = SessionOwner::Ui {
            tab_id: tab_id.clone(),
            pane_id: pane_id.clone(),
        };
        if pane.tab_id != tab_id
            || pane.phase != RuntimePanePhase::Ready
            || pane.session_id.as_ref() != Some(&binding.session_id)
            || binding.owner != expected_owner
        {
            self.status = Some("重连请求已过期，终端绑定需要刷新。".into());
            cx.notify();
            return;
        }
        let Some((attempt_id, cancellation, operation)) = self.session_reconnect.begin_attempt(
            &pane_id,
            &tab_id,
            binding.clone(),
            target.clone(),
        ) else {
            self.status = Some("此终端当前不可重连，状态已变化或正在处理另一请求。".into());
            cx.notify();
            return;
        };
        self.remember_reconnect_pane_focus_intent(
            tab_id.clone(),
            pane_id.clone(),
            binding.session_id.clone(),
            window,
            cx,
        );
        self.refresh_proxy_panel(cx);
        let _ = self
            .workspace_runtime
            .invalidate_session_snapshot(&pane_id, &binding.session_id);
        // 旧 handler 与选区都绑定旧 generation；先撤销，完成后再以 service 新绑定重建。
        self.invalidate_pane_input_handler(&binding.session_id, cx);
        let sessions = Arc::clone(&self.services.sessions);
        let executor = self.services.executor.clone();
        let task_pane = pane_id.clone();
        let task_tab = tab_id.clone();
        let completion_session_id = binding.session_id.clone();
        let completion_pane = pane_id.clone();
        let completion_tab = tab_id.clone();
        let this = cx.weak_entity();
        self.refresh_workspace_views(cx);
        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    cancellation.install(task_cancellation.clone());
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重新连接已取消"));
                    }
                    let before = read_live_observation(
                        Arc::clone(&sessions),
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    match operation {
                        SessionOperationKind::Restart => {
                            validate_restart_before(
                                &before, &binding, &target, &task_tab, &task_pane,
                            )?;
                        }
                        SessionOperationKind::Reconnect => {
                            validate_reconnect_before(
                                &before, &binding, &target, &task_tab, &task_pane,
                            )?;
                        }
                    }
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重新连接已取消"));
                    }

                    let acknowledged = match operation {
                        SessionOperationKind::Restart => {
                            sessions
                                .restart_cancellable(
                                    binding.session_id.clone(),
                                    task_cancellation.clone(),
                                )
                                .await?
                        }
                        SessionOperationKind::Reconnect => {
                            sessions
                                .reconnect_cancellable(
                                    binding.session_id.clone(),
                                    task_cancellation.clone(),
                                )
                                .await?
                        }
                    };
                    if acknowledged.session_id != binding.session_id
                        || acknowledged.owner != binding.owner
                        || acknowledged.target != target
                        || acknowledged.lifecycle != SessionLifecycle::Running
                        || acknowledged.exit_status.is_some()
                    {
                        return Err(AppError::conflict(
                            "重新连接返回了不同的会话身份、目标或状态",
                        ));
                    }

                    let after = read_live_observation(
                        sessions,
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    validate_reconnected_after(&after, &before, &binding, &target)?;
                    Ok(after)
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if let Some(outcome) = root.session_reconnect.finish_attempt(attempt_id, result) {
                    if let ReconnectResultState::Failed(message) = outcome {
                        root.status = Some(message.into());
                    }
                    // finish_attempt 已释放 busy；此时异步重读新 generation 才能重建可输入 handler。
                    root.request_snapshot_binding_reconcile(
                        completion_tab,
                        completion_pane,
                        completion_session_id,
                        cx,
                    );
                    root.refresh_workspace_views(cx);
                    root.refresh_proxy_panel(cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 取消按钮只标记该 binding/attempt；service 收到 token 并完成资源回收前仍显示取消中。
    pub(super) fn cancel_session_reconnect(
        &mut self,
        pane_id: PaneId,
        binding: UiSessionBinding,
        attempt_id: u64,
        cx: &mut Context<Self>,
    ) {
        if self
            .session_reconnect
            .cancel_attempt(&pane_id, &binding, attempt_id)
        {
            self.refresh_workspace_views(cx);
            cx.notify();
        }
    }

    /// 只有仍属于同一 tab/pane/session/target 的 Closed 回执能保留原只读身份并标记终态。
    ///
    /// @author kongweiguang
    pub(super) fn record_closed_session_for_container_panel(
        &mut self,
        tab_id: &TabId,
        pane_id: &PaneId,
        session_id: &SessionId,
    ) {
        let Some(pane) = self.workspace_runtime.pane(pane_id) else {
            return;
        };
        let Some(target) = matching_closed_target(
            &pane.tab_id,
            pane.session_id.as_ref(),
            pane.target.as_ref(),
            pane.phase,
            tab_id,
            session_id,
        ) else {
            return;
        };
        self.session_reconnect
            .record_closed_observation(tab_id, pane_id, session_id, target);
    }

    /// 校验 refresh 回执仍属于当前 runtime pane，不用 UI 投影替代 session service 权威数据。
    fn workspace_pane_matches(
        &self,
        tab_id: &TabId,
        pane_id: &PaneId,
        ticket: &RefreshTicket,
    ) -> bool {
        self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
            pane.tab_id == *tab_id
                && pane.phase == RuntimePanePhase::Ready
                && pane.session_id.as_ref() == Some(&ticket.session_id)
                && pane.target.as_ref() == Some(&ticket.target)
                && self
                    .session_reconnect
                    .panes
                    .get(pane_id)
                    .is_some_and(|state| {
                        state.incarnation == ticket.incarnation
                            && state.refresh_revision == ticket.revision
                    })
        })
    }
}

/// 以 runtime pane 为 Closed 事件提供唯一权威目标，拒绝迟到的其他 session 或 pane 代次回执。
///
/// @author kongweiguang
fn matching_closed_target<'a>(
    runtime_tab_id: &TabId,
    runtime_session_id: Option<&SessionId>,
    runtime_target: Option<&'a SessionTarget>,
    phase: RuntimePanePhase,
    receipt_tab_id: &TabId,
    receipt_session_id: &SessionId,
) -> Option<&'a SessionTarget> {
    if phase == RuntimePanePhase::Ready
        && runtime_tab_id == receipt_tab_id
        && runtime_session_id == Some(receipt_session_id)
    {
        runtime_target
    } else {
        None
    }
}

/// 只查询传入的稳定 session id，并要求真实 owner/target 与过滤条件完全一致。
async fn read_live_observation(
    sessions: Arc<dyn SessionService>,
    session_id: SessionId,
    owner: SessionOwner,
    target: SessionTarget,
) -> Result<ReconnectObservation, AppError> {
    let listed = sessions
        .list(SessionFilter {
            owner: Some(owner.clone()),
            target: Some(target.clone()),
        })
        .await?;
    let info = listed
        .into_iter()
        .find(|info| info.session_id == session_id)
        .ok_or_else(|| AppError::conflict("终端会话 owner 或连接目标已变化"))?;
    if info.session_id != session_id || info.owner != owner || info.target != target {
        return Err(AppError::conflict("终端会话 owner 或连接目标已变化"));
    }
    let context = sessions
        .terminal_command_context(session_id.clone())
        .await?;
    if context.session_id != session_id || context.owner != owner {
        return Err(AppError::conflict("终端会话 generation 查询已失效"));
    }
    Ok(ReconnectObservation {
        info,
        session_generation: context.session_generation,
    })
}

/// 重连只接受原 session 处于 Exited/Failed 的服务事实，target、owner 或 generation 变化均拒绝。
///
/// @author kongweiguang
fn validate_reconnect_before(
    observation: &ReconnectObservation,
    binding: &UiSessionBinding,
    target: &SessionTarget,
    tab_id: &TabId,
    pane_id: &PaneId,
) -> Result<(), AppError> {
    if !observation_matches(observation, tab_id, pane_id, binding, target)
        || observation.session_generation != binding.session_generation
        || !can_reconnect(&observation.info)
    {
        return Err(AppError::conflict("终端已变化或当前生命周期不允许重连"));
    }
    Ok(())
}

/// 重新连接 ACK 后再次确认仍是同一 session/owner/target，且新的 runtime generation 已就绪。
///
/// @author kongweiguang
fn validate_reconnected_after(
    after: &ReconnectObservation,
    before: &ReconnectObservation,
    binding: &UiSessionBinding,
    target: &SessionTarget,
) -> Result<(), AppError> {
    if after.info.session_id != binding.session_id
        || after.info.owner != binding.owner
        || after.info.target != *target
        || after.info.lifecycle != SessionLifecycle::Running
        || after.session_generation <= before.session_generation
    {
        return Err(AppError::conflict(
            "重新连接后未能确认原终端会话以新代次运行",
        ));
    }
    Ok(())
}

/// 重启只接受同一 UI session 的当前代次及 Running/Exited/Failed，拒绝迟到菜单和 Starting 请求。
///
/// @author kongweiguang
fn validate_restart_before(
    observation: &ReconnectObservation,
    binding: &UiSessionBinding,
    target: &SessionTarget,
    tab_id: &TabId,
    pane_id: &PaneId,
) -> Result<(), AppError> {
    if !observation_matches(observation, tab_id, pane_id, binding, target)
        || observation.session_generation != binding.session_generation
        || !can_restart(&observation.info)
    {
        return Err(AppError::conflict("终端已变化或当前生命周期不允许重启"));
    }
    Ok(())
}

/// 身份必须匹配 pane 自身的 Ui owner 和完整 target，不能从 renderer 状态字符串推断。
///
/// @author kongweiguang
fn observation_matches(
    observation: &ReconnectObservation,
    tab_id: &TabId,
    pane_id: &PaneId,
    binding: &UiSessionBinding,
    target: &SessionTarget,
) -> bool {
    observation.info.session_id == binding.session_id
        && observation.info.owner == binding.owner
        && observation.info.target == *target
        && binding.owner
            == (SessionOwner::Ui {
                tab_id: tab_id.clone(),
                pane_id: pane_id.clone(),
            })
}

/// 显式重连只接受断开的生命周期；实际连接仍由 service 按原 target 执行。
/// 自动重连仍由 infrastructure 的 SSH 专用策略决定，不由此手动入口扩大。
///
/// @author kongweiguang
fn can_reconnect(info: &SessionInfo) -> bool {
    matches!(
        info.lifecycle,
        SessionLifecycle::Exited | SessionLifecycle::Failed
    )
}

/// 显式重启只支持 service 已承诺安全复用身份的 Running、Exited 和 Failed 生命周期。
///
/// @author kongweiguang
fn can_restart(info: &SessionInfo) -> bool {
    matches!(
        info.lifecycle,
        SessionLifecycle::Running | SessionLifecycle::Exited | SessionLifecycle::Failed
    )
}

#[cfg(test)]
#[path = "../../tests/unit/root_session_reconnect.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/root_protocol_reconnect.rs"]
mod protocol_tests;
