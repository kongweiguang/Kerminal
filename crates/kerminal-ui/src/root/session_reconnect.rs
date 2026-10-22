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

use gpui_kit::Context;
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

/// Renderer 可用的重连状态；身份绑定来自 SessionService，不从 terminal 文本推导。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionReconnectProjection {
    pub binding: UiSessionBinding,
    pub state: SessionReconnectState,
}

/// 标题栏刷新按钮使用服务观察到的精确绑定；同 pane 的重连或重启期间维持禁用态。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionRestartProjection {
    pub binding: UiSessionBinding,
    pub busy: bool,
}

/// 供右栏读取器使用的同轮服务观察；目标与 generation 由 SessionService 一起确认。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SessionPanelObservation {
    pub binding: UiSessionBinding,
    pub target: SessionTarget,
    pub lifecycle: SessionLifecycle,
}

/// 当前 pane 的唯一手动重连动作及服务确认状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionReconnectState {
    Available,
    Connecting { attempt_id: u64 },
    Cancelling { attempt_id: u64 },
    Failed { message: String },
    Cancelled,
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
    kind: SessionOperationKind,
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

    /// 登记一个与缓存 binding 完全相同的唯一重连；同 pane 的取消收尾未 ACK 时拒绝新尝试。
    ///
    /// @author kongweiguang
    fn begin_attempt(
        &mut self,
        pane_id: &PaneId,
        tab_id: &TabId,
        binding: UiSessionBinding,
        target: SessionTarget,
    ) -> Option<(u64, Arc<ReconnectCancellation>)> {
        self.begin_operation(
            pane_id,
            tab_id,
            binding,
            target,
            SessionOperationKind::Reconnect,
        )
    }

    /// 登记唯一的重启操作；与断线重连共享 pane 锁，防止双击或交叉请求替换 transport。
    ///
    /// @author kongweiguang
    fn begin_restart_attempt(
        &mut self,
        pane_id: &PaneId,
        tab_id: &TabId,
        binding: UiSessionBinding,
        target: SessionTarget,
    ) -> Option<(u64, Arc<ReconnectCancellation>)> {
        self.begin_operation(
            pane_id,
            tab_id,
            binding,
            target,
            SessionOperationKind::Restart,
        )
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
                kind,
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

    /// 服务操作完成后才释放 in-flight owner；过期 incarnation 不得改变当前 pane 的显示状态。
    fn finish_attempt(
        &mut self,
        attempt_id: u64,
        result: Result<ReconnectObservation, AppError>,
    ) -> bool {
        let Some(attempt) = self.in_flight.remove(&attempt_id) else {
            return false;
        };
        let Some(state) = self.panes.get_mut(&attempt.pane_id) else {
            return false;
        };
        if state.incarnation != attempt.incarnation || state.active_attempt != Some(attempt_id) {
            return false;
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
                    state.result = ReconnectResultState::Failed(match attempt.kind {
                        SessionOperationKind::Reconnect => {
                            "重连完成后会话身份或代次未能确认。".to_owned()
                        }
                        SessionOperationKind::Restart => {
                            "重启完成后会话身份或代次未能确认。".to_owned()
                        }
                    });
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
        true
    }

    /// 只为已断开的既有会话生成重连投影，避免 pane 运行或关闭时启动重复 transport。
    ///
    /// @author kongweiguang
    fn projection(
        &self,
        pane_id: &PaneId,
        tab_id: &TabId,
        session_id: &SessionId,
        target: &SessionTarget,
        phase: RuntimePanePhase,
    ) -> Option<SessionReconnectProjection> {
        if phase != RuntimePanePhase::Ready {
            return None;
        }
        let state = self.panes.get(pane_id)?;
        if let Some(attempt_id) = state.active_attempt
            && let Some(attempt) = self.in_flight.get(&attempt_id)
            && attempt.kind == SessionOperationKind::Reconnect
            && attempt.incarnation == state.incarnation
            && attempt.tab_id == *tab_id
            && attempt.binding.session_id == *session_id
            && attempt.target == *target
        {
            return Some(SessionReconnectProjection {
                binding: attempt.binding.clone(),
                state: if attempt.cancelling {
                    SessionReconnectState::Cancelling { attempt_id }
                } else {
                    SessionReconnectState::Connecting { attempt_id }
                },
            });
        }

        if self
            .in_flight
            .values()
            .any(|attempt| attempt.pane_id == *pane_id)
        {
            return None;
        }

        let observation = state.observation.as_ref()?;
        let binding = UiSessionBinding {
            session_id: observation.info.session_id.clone(),
            owner: observation.info.owner.clone(),
            session_generation: observation.session_generation,
        };
        if observation.info.session_id != *session_id
            || observation.info.target != *target
            || !observation_matches(observation, tab_id, pane_id, &binding, target)
            || !can_reconnect(&observation.info)
        {
            return None;
        }
        let state = match &state.result {
            ReconnectResultState::Ready => SessionReconnectState::Available,
            ReconnectResultState::Failed(message) => SessionReconnectState::Failed {
                message: message.clone(),
            },
            ReconnectResultState::Cancelled => SessionReconnectState::Cancelled,
        };
        Some(SessionReconnectProjection { binding, state })
    }

    /// 为任何可显式重启的既有 session 返回精确绑定；同 pane 重连或重启在途时按钮固定禁用。
    ///
    /// @author kongweiguang
    fn restart_projection(
        &self,
        pane_id: &PaneId,
        tab_id: &TabId,
        session_id: &SessionId,
        target: &SessionTarget,
        phase: RuntimePanePhase,
    ) -> Option<SessionRestartProjection> {
        if phase != RuntimePanePhase::Ready {
            return None;
        }
        if let Some(attempt) = self.in_flight.values().find(|attempt| {
            attempt.pane_id == *pane_id
                && attempt.tab_id == *tab_id
                && attempt.binding.session_id == *session_id
                && attempt.target == *target
                && self
                    .panes
                    .get(pane_id)
                    .is_some_and(|state| attempt.incarnation == state.incarnation)
        }) {
            return Some(SessionRestartProjection {
                binding: attempt.binding.clone(),
                busy: true,
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
        Some(SessionRestartProjection {
            binding,
            busy: false,
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

    /// 从 pane 现状生成可见 action；异步重连仍会独立复查 service 权威状态。
    pub(super) fn session_reconnect_projection(
        &self,
        pane_id: &PaneId,
    ) -> Option<SessionReconnectProjection> {
        let pane = self.workspace_runtime.pane(pane_id)?;
        self.session_reconnect.projection(
            pane_id,
            &pane.tab_id,
            pane.session_id.as_ref()?,
            pane.target.as_ref()?,
            pane.phase,
        )
    }

    /// 为终端标题栏提供重启按钮绑定；即使重连进行中也投影 busy，避免同 pane 并发操作。
    ///
    /// @author kongweiguang
    pub(super) fn session_restart_projection(
        &self,
        pane_id: &PaneId,
    ) -> Option<SessionRestartProjection> {
        let pane = self.workspace_runtime.pane(pane_id)?;
        self.session_reconnect.restart_projection(
            pane_id,
            &pane.tab_id,
            pane.session_id.as_ref()?,
            pane.target.as_ref()?,
            pane.phase,
        )
    }

    /// 显式重启只作用于事件捕获时的 session binding，并在服务 ACK 与新代次回读后解除 pane 锁。
    ///
    /// @author kongweiguang
    pub(super) fn restart_workspace_pane(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        let Some(pane) = self.workspace_runtime.pane(&pane_id) else {
            return;
        };
        let Some(target) = pane.target.clone() else {
            self.status = Some("此 pane 当前没有可重启的终端会话。".into());
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
            self.status = Some("重启请求已过期，终端绑定需要刷新。".into());
            cx.notify();
            return;
        }
        let Some((attempt_id, cancellation)) = self.session_reconnect.begin_restart_attempt(
            &pane_id,
            &tab_id,
            binding.clone(),
            target.clone(),
        ) else {
            self.status = Some("此终端当前不可重启，状态已变化或正在处理另一请求。".into());
            cx.notify();
            return;
        };

        self.invalidate_pane_input_handler(&binding.session_id, cx);
        let sessions = Arc::clone(&self.services.sessions);
        let executor = self.services.executor.clone();
        let task_pane = pane_id.clone();
        let task_tab = tab_id.clone();
        let completion_session_id = binding.session_id.clone();
        let completion_pane = task_pane.clone();
        let completion_tab = task_tab.clone();
        let this = cx.weak_entity();
        self.refresh_workspace_views(cx);
        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    cancellation.install(task_cancellation.clone());
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重启已取消"));
                    }
                    let before = read_live_observation(
                        Arc::clone(&sessions),
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    validate_restart_before(&before, &binding, &target, &task_tab, &task_pane)?;
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重启已取消"));
                    }

                    let acknowledged = sessions
                        .restart_cancellable(binding.session_id.clone(), task_cancellation.clone())
                        .await?;
                    if acknowledged.session_id != binding.session_id
                        || acknowledged.owner != binding.owner
                        || acknowledged.target != target
                        || acknowledged.lifecycle != SessionLifecycle::Running
                        || acknowledged.exit_status.is_some()
                    {
                        return Err(AppError::conflict("重启返回了不同的会话身份、目标或状态"));
                    }

                    let after = read_live_observation(
                        sessions,
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    validate_restart_after(&after, &before, &binding, &target)?;
                    Ok(after)
                })
                .await;
            let failure_message = result
                .as_ref()
                .err()
                .map(|error| error.public_message().to_owned());
            let _ = this.update(cx, |root, cx| {
                if root.session_reconnect.finish_attempt(attempt_id, result) {
                    // 重启期间启动的 watcher 可能仍携带旧 Running/Starting 查询；先撤销票据，再允许 render 建立新代次绑定。
                    root.invalidate_pane_input_handler(&completion_session_id, cx);
                    if let Some(message) = failure_message {
                        root.status = Some(message.into());
                    }
                    root.refresh_workspace_views(cx);
                    root.refresh_proxy_panel(cx);
                    root.refresh_session_reconnect_state(completion_tab, completion_pane, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 对一个明确终端 session 发起单次原位重连；请求前重查 pane、owner 与缓存 generation。
    ///
    /// @author kongweiguang
    pub(super) fn start_session_reconnect(
        &mut self,
        tab_id: TabId,
        pane_id: PaneId,
        binding: UiSessionBinding,
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
        let Some((attempt_id, cancellation)) = self.session_reconnect.begin_attempt(
            &pane_id,
            &tab_id,
            binding.clone(),
            target.clone(),
        ) else {
            self.status = Some("此终端当前不可重连，状态已变化或正在处理另一请求。".into());
            cx.notify();
            return;
        };
        let sessions = Arc::clone(&self.services.sessions);
        let executor = self.services.executor.clone();
        let task_pane = pane_id.clone();
        let task_tab = tab_id.clone();
        let this = cx.weak_entity();
        self.refresh_workspace_views(cx);
        cx.spawn(async move |_, cx| {
            let result = executor
                .run(move |task_cancellation| async move {
                    cancellation.install(task_cancellation.clone());
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重连已取消"));
                    }
                    let before = read_live_observation(
                        Arc::clone(&sessions),
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    validate_reconnect_before(&before, &binding, &target, &task_tab, &task_pane)?;
                    if task_cancellation.is_cancelled() {
                        return Err(AppError::cancelled("终端重连已取消"));
                    }

                    let acknowledged = sessions
                        .reconnect_cancellable(
                            binding.session_id.clone(),
                            task_cancellation.clone(),
                        )
                        .await?;
                    if acknowledged.session_id != binding.session_id
                        || acknowledged.owner != binding.owner
                        || acknowledged.target != target
                        || acknowledged.lifecycle != SessionLifecycle::Running
                    {
                        return Err(AppError::conflict("重连返回了不同的会话身份或目标"));
                    }

                    let after = read_live_observation(
                        sessions,
                        binding.session_id.clone(),
                        binding.owner.clone(),
                        target.clone(),
                    )
                    .await?;
                    validate_reconnect_after(&after, &before, &binding, &target)?;
                    Ok(after)
                })
                .await;
            let _ = this.update(cx, |root, cx| {
                if root.session_reconnect.finish_attempt(attempt_id, result) {
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

/// 重连 ACK 后再次确认仍是同一 session/owner/target，且新的 runtime generation 已就绪。
///
/// @author kongweiguang
fn validate_reconnect_after(
    after: &ReconnectObservation,
    before: &ReconnectObservation,
    binding: &UiSessionBinding,
    target: &SessionTarget,
) -> Result<(), AppError> {
    if after.info.session_id != binding.session_id
        || after.info.owner != binding.owner
        || after.info.target != *target
        || after.info.lifecycle != SessionLifecycle::Running
        || after.session_generation == before.session_generation
    {
        return Err(AppError::conflict("重连后未能确认原终端会话已运行"));
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

/// 重启 ACK 后必须读回相同 session/owner/target、Running 状态和严格递增的 transport generation。
///
/// @author kongweiguang
fn validate_restart_after(
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
        return Err(AppError::conflict("重启后未能确认原终端会话以新代次运行"));
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
