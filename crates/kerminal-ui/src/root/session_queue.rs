//! 按终端会话隔离的有序输入与资源控制队列。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet, VecDeque};

use kerminal_application::sessions::{UiInputProjection, UiSessionBinding, UiTerminalInputRequest};
use kerminal_domain::{
    identity::SessionId,
    sessions::{SessionOwner, TerminalSize},
};
use kerminal_terminal::input::TerminalInputEvent;

use super::command_line_projection::CommandLineInputTicket;
use super::command_suggestions::TerminalSuggestionAcceptance;
use kerminal_domain::commands::SuggestionQueryMode;

/// 同会话保序、跨会话并行，并同时限制队列内存与活跃写入数。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SessionQueueLimits {
    pub per_session_commands: usize,
    pub total_commands: usize,
    pub total_input_bytes: usize,
    pub concurrent_sessions: usize,
}

impl Default for SessionQueueLimits {
    /// 默认预算沿用现有全局上限，并给单个慢连接留出空间而不阻塞其它会话。
    fn default() -> Self {
        Self {
            per_session_commands: 128,
            total_commands: 512,
            total_input_bytes: 1_048_576,
            concurrent_sessions: 8,
        }
    }
}

/// 键鼠输入、滚动和尺寸更新按 session 排队；关闭由 workspace reducer 单独协调。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum SessionWork {
    Input {
        event: TerminalInputEvent,
        origin: InputOrigin,
    },
    UiInput(Box<UiInputWork>),
    Scroll {
        binding: UiSessionBinding,
        lines: i32,
    },
    ScrollTo {
        binding: UiSessionBinding,
        offset: usize,
    },
    /// 拖选边缘自动滚动携带手势与请求代次，ACK 才能确认对应的新快照。
    SelectionScroll {
        binding: UiSessionBinding,
        offset: usize,
        gesture_id: u64,
        request_id: u64,
    },
    Resize {
        binding: UiSessionBinding,
        size: TerminalSize,
    },
    /// Exited/Failed 历史只调整引擎网格，不代表或确认 PTY transport resize。
    HistoryResize {
        binding: UiSessionBinding,
        size: TerminalSize,
    },
}

/// 将完整 owner-bound 输入作为一个队列分配单元，让 SessionCommand 到 FIFO 只移动同一 Box。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct UiInputWork {
    pub request: UiTerminalInputRequest,
    pub acceptance: Option<SuggestionAcceptanceWrite>,
    pub command_line_ticket: Option<CommandLineInputTicket>,
    pub origin: InputOrigin,
    /// Acceptance-metrics-only correlation; never crosses the application/session API.
    #[cfg(feature = "acceptance-metrics")]
    pub echo_probe_id: Option<String>,
}

impl UiInputWork {
    /// 初始化未准入的输入；准入点重算票据和来源。feature-only echo ID 只留在 UI FIFO，不进入 application DTO。
    ///
    /// @author kongweiguang
    pub(super) fn new(
        request: UiTerminalInputRequest,
        acceptance: Option<SuggestionAcceptanceWrite>,
    ) -> Self {
        Self {
            request,
            acceptance,
            command_line_ticket: None,
            origin: InputOrigin::Direct,
            #[cfg(feature = "acceptance-metrics")]
            echo_probe_id: None,
        }
    }
}

/// 一项建议替换计划的单个 CAS 步骤；下一步只会在前一项成功 ACK 后入队。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SuggestionAcceptanceWrite {
    pub acceptance: TerminalSuggestionAcceptance,
    pub event_index: usize,
    pub mode: SuggestionQueryMode,
}

/// 广播来源带 generation，Stop/Cancel 可只撤销尚未发送的对应输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InputOrigin {
    Direct,
    Broadcast(u64),
}

/// 已从 FIFO 取出的工作；每个 SessionId 同时最多有一个该项在执行。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ReadySessionWork {
    pub session_id: SessionId,
    pub work: SessionWork,
}

/// 队列拒绝原因保持结构化，Root 可给出可操作状态而不暴露内部实现错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SessionQueueError {
    SessionUnavailable,
    QueueFull,
    BroadcastInactive,
    BroadcastInputNotSupported,
}

/// Only explicit terminal keyboard/text/paste input may be broadcast; focus reports and mouse
/// tracking describe one pane's interaction state and must remain bound to their original owner.
pub(super) fn is_broadcastable_input(event: &TerminalInputEvent) -> bool {
    matches!(
        event,
        TerminalInputEvent::Key(_)
            | TerminalInputEvent::KeyWithText { .. }
            | TerminalInputEvent::Text(_)
            | TerminalInputEvent::Paste(_)
    )
}

/// 控制操作撤销尚未发送工作时的准确计数，用于避免“已排空”这类错误提示。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct QueueDropSummary {
    pub commands: usize,
    pub input_events: usize,
    pub input_bytes: usize,
    pub broadcast_events: usize,
    /// 已从 FIFO 撤销的只读历史尺寸项；in-flight work 不计入此数。
    pub history_resize_commands: usize,
}

impl QueueDropSummary {
    /// 汇总实际移除的普通工作项；单独记录历史 resize 供退休流程复位待处理标记。
    fn include(&mut self, work: &SessionWork) {
        self.commands = self.commands.saturating_add(1);
        match work {
            SessionWork::Input { event, origin } => {
                self.input_events = self.input_events.saturating_add(1);
                self.input_bytes = self.input_bytes.saturating_add(input_size(event));
                if matches!(origin, InputOrigin::Broadcast(_)) {
                    self.broadcast_events = self.broadcast_events.saturating_add(1);
                }
            }
            SessionWork::UiInput(work) => {
                self.input_events = self.input_events.saturating_add(1);
                self.input_bytes = self
                    .input_bytes
                    .saturating_add(input_size(&work.request.event));
                if matches!(work.origin, InputOrigin::Broadcast(_)) {
                    self.broadcast_events = self.broadcast_events.saturating_add(1);
                }
            }
            SessionWork::HistoryResize { .. } => {
                self.history_resize_commands = self.history_resize_commands.saturating_add(1);
            }
            SessionWork::Scroll { .. }
            | SessionWork::ScrollTo { .. }
            | SessionWork::SelectionScroll { .. }
            | SessionWork::Resize { .. } => {}
        }
    }

    /// 汇总另一个会话的撤销结果，便于一次 Stop/Close 的 UI 提示精确反映全局变化。
    pub fn merge(&mut self, other: Self) {
        self.commands = self.commands.saturating_add(other.commands);
        self.input_events = self.input_events.saturating_add(other.input_events);
        self.input_bytes = self.input_bytes.saturating_add(other.input_bytes);
        self.broadcast_events = self.broadcast_events.saturating_add(other.broadcast_events);
        self.history_resize_commands = self
            .history_resize_commands
            .saturating_add(other.history_resize_commands);
    }
}

/// 每个 SessionId 有独立 FIFO；Root 每次只启动有限数量的不同会话任务。
pub(super) struct SessionCommandQueues {
    limits: SessionQueueLimits,
    lanes: HashMap<SessionId, SessionLane>,
    ready: VecDeque<SessionId>,
    ready_set: HashSet<SessionId>,
    in_flight: HashSet<SessionId>,
    queued_commands: usize,
    queued_input_bytes: usize,
    broadcast_generation: u64,
    pending_broadcast_generation: Option<u64>,
    active_broadcast: Option<ActiveBroadcast>,
}

struct SessionLane {
    pending: VecDeque<SessionWork>,
    scheduled: bool,
    in_flight: bool,
    in_flight_history_resize_size: Option<TerminalSize>,
}

struct ActiveBroadcast {
    generation: u64,
    targets: Vec<UiSessionBinding>,
}

impl Default for SessionCommandQueues {
    /// 创建独立会话队列调度器；Root 仍拥有 UI 生命周期和服务调用结果。
    fn default() -> Self {
        Self::with_limits(SessionQueueLimits::default())
    }
}

impl SessionCommandQueues {
    /// 可注入有限预算，测试可验证满载、并发和控制命令语义。
    pub fn with_limits(limits: SessionQueueLimits) -> Self {
        Self {
            limits: SessionQueueLimits {
                per_session_commands: limits.per_session_commands.max(1),
                total_commands: limits.total_commands.max(1),
                total_input_bytes: limits.total_input_bytes.max(1),
                concurrent_sessions: limits.concurrent_sessions.max(1),
            },
            lanes: HashMap::new(),
            ready: VecDeque::new(),
            ready_set: HashSet::new(),
            in_flight: HashSet::new(),
            queued_commands: 0,
            queued_input_bytes: 0,
            broadcast_generation: 0,
            pending_broadcast_generation: None,
            active_broadcast: None,
        }
    }

    /// 将直接输入与滚动、resize 放入对应会话 FIFO，拒绝而不覆盖已接受输入。
    pub fn enqueue(
        &mut self,
        session_id: SessionId,
        work: SessionWork,
    ) -> Result<(), SessionQueueError> {
        self.enqueue_with_origin(session_id, work)
    }

    /// 接受计划的后续 CAS 事件必须排在已等待的普通输入之前，同时仍受同一 FIFO 预算约束。
    pub fn enqueue_front(
        &mut self,
        session_id: SessionId,
        work: SessionWork,
    ) -> Result<(), SessionQueueError> {
        let lane = self
            .lanes
            .entry(session_id.clone())
            .or_insert_with(SessionLane::new);
        let input_bytes = work_input_size(&work);
        if lane.pending.len() >= self.limits.per_session_commands
            || self.queued_commands >= self.limits.total_commands
            || self.queued_input_bytes.saturating_add(input_bytes) > self.limits.total_input_bytes
        {
            if lane.pending.is_empty() && !lane.in_flight {
                self.lanes.remove(&session_id);
            }
            return Err(SessionQueueError::QueueFull);
        }
        lane.pending.push_front(work);
        self.queued_commands = self.queued_commands.saturating_add(1);
        self.queued_input_bytes = self.queued_input_bytes.saturating_add(input_bytes);
        self.schedule_if_ready(&session_id);
        Ok(())
    }

    /// 菜单接受仅在目标 FIFO 空闲时开始，避免用过期候选覆盖已排队的用户输入。
    pub fn is_idle(&self, session_id: &SessionId) -> bool {
        self.lanes
            .get(session_id)
            .is_none_or(|lane| !lane.in_flight && lane.pending.is_empty())
    }

    /// 把同一输入按固定 binding 原子加入各 session FIFO，并让每个目标只分配一次工作 Box。
    ///
    /// @author kongweiguang
    pub fn enqueue_broadcast_event(
        &mut self,
        generation: u64,
        event: TerminalInputEvent,
    ) -> Result<usize, SessionQueueError> {
        if !is_broadcastable_input(&event) {
            return Err(SessionQueueError::BroadcastInputNotSupported);
        }
        let Some(active) = self.active_broadcast.as_ref() else {
            return Err(SessionQueueError::BroadcastInactive);
        };
        if active.generation != generation {
            return Err(SessionQueueError::BroadcastInactive);
        }
        let targets = active.targets.clone();
        if targets.is_empty() {
            return Err(SessionQueueError::BroadcastInactive);
        }

        let event_bytes = input_size(&event);
        let additional_bytes = event_bytes.saturating_mul(targets.len());
        if self.queued_commands.saturating_add(targets.len()) > self.limits.total_commands
            || self.queued_input_bytes.saturating_add(additional_bytes)
                > self.limits.total_input_bytes
            || targets.iter().any(|binding| {
                self.lanes
                    .get(&binding.session_id)
                    .is_some_and(|lane| lane.pending.len() >= self.limits.per_session_commands)
            })
        {
            return Err(SessionQueueError::QueueFull);
        }

        for binding in &targets {
            let session_id = &binding.session_id;
            let lane = self
                .lanes
                .entry(session_id.clone())
                .or_insert_with(SessionLane::new);
            let mut work = UiInputWork::new(
                UiTerminalInputRequest {
                    binding: binding.clone(),
                    expected_context: None,
                    projection: UiInputProjection::Invalidates,
                    event: event.clone(),
                },
                None,
            );
            work.origin = InputOrigin::Broadcast(generation);
            lane.pending.push_back(SessionWork::UiInput(Box::new(work)));
            self.queued_commands = self.queued_commands.saturating_add(1);
            self.queued_input_bytes = self.queued_input_bytes.saturating_add(event_bytes);
            self.schedule_if_ready(session_id);
        }
        Ok(targets.len())
    }

    /// 对一次明确的固定目标动作做全量预算预检，并以 Direct origin 原子排入各自 FIFO。
    ///
    /// @author kongweiguang
    pub fn enqueue_broadcast_once(
        &mut self,
        bindings: &[UiSessionBinding],
        event: TerminalInputEvent,
    ) -> Result<usize, SessionQueueError> {
        if !is_broadcastable_input(&event) {
            return Err(SessionQueueError::BroadcastInputNotSupported);
        }
        if bindings.is_empty() || bindings.len() > 64 {
            return Err(SessionQueueError::SessionUnavailable);
        }
        let mut unique = HashSet::with_capacity(bindings.len());
        if bindings.iter().any(|binding| {
            !matches!(&binding.owner, SessionOwner::Ui { .. })
                || !unique.insert(binding.session_id.clone())
        }) {
            return Err(SessionQueueError::SessionUnavailable);
        }
        let event_bytes = input_size(&event);
        let additional_bytes = event_bytes.saturating_mul(bindings.len());
        if self.queued_commands.saturating_add(bindings.len()) > self.limits.total_commands
            || self.queued_input_bytes.saturating_add(additional_bytes)
                > self.limits.total_input_bytes
            || bindings.iter().any(|binding| {
                self.lanes
                    .get(&binding.session_id)
                    .is_some_and(|lane| lane.pending.len() >= self.limits.per_session_commands)
            })
        {
            return Err(SessionQueueError::QueueFull);
        }

        for binding in bindings {
            let session_id = &binding.session_id;
            let lane = self
                .lanes
                .entry(session_id.clone())
                .or_insert_with(SessionLane::new);
            lane.pending
                .push_back(SessionWork::UiInput(Box::new(UiInputWork::new(
                    UiTerminalInputRequest {
                        binding: binding.clone(),
                        expected_context: None,
                        projection: UiInputProjection::Invalidates,
                        event: event.clone(),
                    },
                    None,
                ))));
            self.queued_commands = self.queued_commands.saturating_add(1);
            self.queued_input_bytes = self.queued_input_bytes.saturating_add(event_bytes);
            self.schedule_if_ready(session_id);
        }
        Ok(bindings.len())
    }

    /// 为连接检查创建单调递增 token；新 Start 同时使旧验证回执与旧广播输入失效。
    pub fn begin_broadcast_start(&mut self) -> (u64, QueueDropSummary) {
        self.broadcast_generation = self.broadcast_generation.wrapping_add(1);
        let generation = self.broadcast_generation;
        self.pending_broadcast_generation = Some(generation);
        self.active_broadcast = None;
        let dropped = self.remove_broadcast_work();
        (generation, dropped)
    }

    /// 仅当前 token 可启用去重的 UI bindings；Agent、重复会话或迟到回执都保持关闭。
    pub fn activate_broadcast(&mut self, generation: u64, targets: Vec<UiSessionBinding>) -> bool {
        if self.pending_broadcast_generation != Some(generation)
            || self.broadcast_generation != generation
            || targets.is_empty()
        {
            return false;
        }
        let mut session_ids = HashSet::with_capacity(targets.len());
        if targets.iter().any(|target| {
            !matches!(target.owner, SessionOwner::Ui { .. })
                || !session_ids.insert(target.session_id.clone())
        }) {
            return false;
        }
        self.pending_broadcast_generation = None;
        self.active_broadcast = Some(ActiveBroadcast {
            generation,
            targets,
        });
        true
    }

    /// 异步目标验证回投前确认 token 仍有效，Stop/Cancel/重复 Start 会令其失效。
    pub fn broadcast_start_is_current(&self, generation: u64) -> bool {
        self.pending_broadcast_generation == Some(generation)
            && self.broadcast_generation == generation
    }

    /// 停止广播并撤销未发送的广播输入；正在执行的一次写入允许完成后停止后续项。
    pub fn stop_broadcast(&mut self) -> QueueDropSummary {
        self.broadcast_generation = self.broadcast_generation.wrapping_add(1);
        self.pending_broadcast_generation = None;
        self.active_broadcast = None;
        self.remove_broadcast_work()
    }

    /// 只撤销尚未送出的输入、滚动和尺寸项，不把无法取消的 in-flight 伪报成已撤销。
    /// 历史 resize 数单独返回，使退出收尾能清除被撤销目标、同时保留在途目标。
    /// 空闲 lane 可以释放，历史视口后续仍可按精确只读 binding 重新入队。
    pub fn cancel_pending(&mut self, session_id: &SessionId) -> QueueDropSummary {
        let mut dropped = QueueDropSummary::default();
        let in_flight = if let Some(lane) = self.lanes.get_mut(session_id) {
            while let Some(work) = lane.pending.pop_front() {
                dropped.include(&work);
                self.queued_commands = self.queued_commands.saturating_sub(1);
                self.queued_input_bytes = self
                    .queued_input_bytes
                    .saturating_sub(work_input_size(&work));
            }
            lane.scheduled = false;
            lane.in_flight
        } else {
            return dropped;
        };
        self.ready_set.remove(session_id);
        self.ready.retain(|candidate| candidate != session_id);
        if !in_flight {
            self.lanes.remove(session_id);
        }
        dropped
    }

    /// 读取真实在途的历史网格目标，供队列取消后保留其待完成标记。
    ///
    /// @author kongweiguang
    pub fn in_flight_history_resize_size(&self, session_id: &SessionId) -> Option<TerminalSize> {
        self.lanes
            .get(session_id)
            .and_then(|lane| lane.in_flight_history_resize_size)
    }

    /// 公平取出一个已就绪会话工作，每会话单飞且总并发不超过固定预算。
    pub fn take_ready(&mut self) -> Option<ReadySessionWork> {
        if self.in_flight.len() >= self.limits.concurrent_sessions {
            return None;
        }
        while let Some(session_id) = self.ready.pop_front() {
            self.ready_set.remove(&session_id);
            let Some(lane) = self.lanes.get_mut(&session_id) else {
                continue;
            };
            lane.scheduled = false;
            if lane.in_flight {
                continue;
            }
            let Some(work) = lane.pending.pop_front() else {
                continue;
            };
            self.queued_commands = self.queued_commands.saturating_sub(1);
            self.queued_input_bytes = self
                .queued_input_bytes
                .saturating_sub(work_input_size(&work));
            lane.in_flight = true;
            lane.in_flight_history_resize_size = match &work {
                SessionWork::HistoryResize { size, .. } => Some(*size),
                _ => None,
            };
            self.in_flight.insert(session_id.clone());
            return Some(ReadySessionWork { session_id, work });
        }
        None
    }

    /// 工作结果已处理后调度 FIFO 下一项；关闭与重试不在此队列中执行。
    pub fn complete(&mut self, session_id: &SessionId) -> bool {
        if !self.in_flight.remove(session_id) {
            return false;
        }
        let Some(lane_is_empty) = self.lanes.get_mut(session_id).map(|lane| {
            lane.in_flight = false;
            lane.in_flight_history_resize_size = None;
            lane.pending.is_empty()
        }) else {
            return true;
        };
        if lane_is_empty {
            self.lanes.remove(session_id);
            self.ready_set.remove(session_id);
            self.ready.retain(|candidate| candidate != session_id);
        } else {
            self.schedule_if_ready(session_id);
        }
        true
    }

    /// 只按广播 generation 清除待发送项，普通输入与另一个输入来源保持原序。
    fn remove_broadcast_work(&mut self) -> QueueDropSummary {
        let mut dropped = QueueDropSummary::default();
        let ids = self.lanes.keys().cloned().collect::<Vec<_>>();
        let mut empty_lanes = Vec::new();
        for session_id in ids {
            let Some((has_ready_work, is_empty)) = self.lanes.get_mut(&session_id).map(|lane| {
                let mut retained = VecDeque::with_capacity(lane.pending.len());
                while let Some(work) = lane.pending.pop_front() {
                    let is_broadcast = match &work {
                        SessionWork::Input { origin, .. } => {
                            matches!(origin, InputOrigin::Broadcast(_))
                        }
                        SessionWork::UiInput(input) => {
                            matches!(input.origin, InputOrigin::Broadcast(_))
                        }
                        SessionWork::Scroll { .. }
                        | SessionWork::ScrollTo { .. }
                        | SessionWork::SelectionScroll { .. }
                        | SessionWork::Resize { .. }
                        | SessionWork::HistoryResize { .. } => false,
                    };
                    if is_broadcast {
                        dropped.include(&work);
                        self.queued_commands = self.queued_commands.saturating_sub(1);
                        self.queued_input_bytes = self
                            .queued_input_bytes
                            .saturating_sub(work_input_size(&work));
                    } else {
                        retained.push_back(work);
                    }
                }
                lane.pending = retained;
                (
                    !lane.pending.is_empty(),
                    lane.pending.is_empty() && !lane.in_flight,
                )
            }) else {
                continue;
            };
            if has_ready_work {
                self.schedule_if_ready(&session_id);
            } else if is_empty {
                empty_lanes.push(session_id);
            }
        }
        for session_id in empty_lanes {
            self.lanes.remove(&session_id);
            self.ready_set.remove(&session_id);
            self.ready.retain(|candidate| candidate != &session_id);
        }
        dropped
    }

    /// 带来源检查后接纳队列项，并对相邻滚动/尺寸更新做有序合并。
    fn enqueue_with_origin(
        &mut self,
        session_id: SessionId,
        work: SessionWork,
    ) -> Result<(), SessionQueueError> {
        let lane = self
            .lanes
            .entry(session_id.clone())
            .or_insert_with(SessionLane::new);
        if let Some(previous) = lane.pending.back_mut() {
            match (&mut *previous, &work) {
                (
                    SessionWork::Scroll { binding, lines },
                    SessionWork::Scroll {
                        binding: next_binding,
                        lines: next,
                    },
                ) if *binding == *next_binding => {
                    *lines = lines.saturating_add(*next);
                    return Ok(());
                }
                (
                    SessionWork::ScrollTo { binding, offset },
                    SessionWork::ScrollTo {
                        binding: next_binding,
                        offset: next,
                    },
                ) if *binding == *next_binding => {
                    // 相邻绝对定位只保留最新目标；绑定变化仍必须保留为独立 FIFO 操作。
                    *offset = *next;
                    return Ok(());
                }
                (
                    SessionWork::SelectionScroll {
                        binding,
                        offset,
                        gesture_id,
                        request_id,
                    },
                    SessionWork::SelectionScroll {
                        binding: next_binding,
                        offset: next_offset,
                        gesture_id: next_gesture_id,
                        request_id: next_request_id,
                    },
                ) if *binding == *next_binding && *gesture_id == *next_gesture_id => {
                    // 同一手势的边缘滚动只保留最新目标与请求号；旧请求不会收到虚假 ACK。
                    *offset = *next_offset;
                    *request_id = *next_request_id;
                    return Ok(());
                }
                (
                    SessionWork::Resize { binding, size },
                    SessionWork::Resize {
                        binding: next_binding,
                        size: next,
                    },
                ) if *binding == *next_binding => {
                    *size = *next;
                    return Ok(());
                }
                (
                    SessionWork::HistoryResize { binding, size },
                    SessionWork::HistoryResize {
                        binding: next_binding,
                        size: next,
                    },
                ) if *binding == *next_binding => {
                    *size = *next;
                    return Ok(());
                }
                _ => {}
            }
        }
        let input_bytes = work_input_size(&work);
        if lane.pending.len() >= self.limits.per_session_commands
            || self.queued_commands >= self.limits.total_commands
            || self.queued_input_bytes.saturating_add(input_bytes) > self.limits.total_input_bytes
        {
            if lane.pending.is_empty() && !lane.in_flight {
                self.lanes.remove(&session_id);
            }
            return Err(SessionQueueError::QueueFull);
        }
        lane.pending.push_back(work);
        self.queued_commands = self.queued_commands.saturating_add(1);
        self.queued_input_bytes = self.queued_input_bytes.saturating_add(input_bytes);
        self.schedule_if_ready(&session_id);
        Ok(())
    }

    /// Ready queue 不重复登记同一 session，保持轮转公平并避免队列增长失控。
    fn schedule_if_ready(&mut self, session_id: &SessionId) {
        let Some(lane) = self.lanes.get_mut(session_id) else {
            return;
        };
        if !lane.in_flight && !lane.scheduled && !lane.pending.is_empty() {
            lane.scheduled = true;
            self.ready_set.insert(session_id.clone());
            self.ready.push_back(session_id.clone());
        }
    }
}

impl SessionLane {
    /// 创建空会话 FIFO；关闭状态由 workspace runtime 所有。
    fn new() -> Self {
        Self {
            pending: VecDeque::new(),
            scheduled: false,
            in_flight: false,
            in_flight_history_resize_size: None,
        }
    }
}

/// 估算事件在队列中的上界成本，普通控制键用固定值，文本按 UTF-8 字节数计费。
fn input_size(event: &TerminalInputEvent) -> usize {
    match event {
        TerminalInputEvent::Text(text) | TerminalInputEvent::Paste(text) => text.len(),
        TerminalInputEvent::KeyWithText { text, .. } => text.len().saturating_add(32),
        TerminalInputEvent::Key(_)
        | TerminalInputEvent::Mouse(_)
        | TerminalInputEvent::Focus(_) => 32,
    }
}

/// 只对输入事件计字节，scroll 与 resize 由命令数量预算约束。
fn work_input_size(work: &SessionWork) -> usize {
    match work {
        SessionWork::Input { event, .. } => input_size(event),
        SessionWork::UiInput(work) => input_size(&work.request.event),
        SessionWork::Scroll { .. }
        | SessionWork::ScrollTo { .. }
        | SessionWork::SelectionScroll { .. }
        | SessionWork::Resize { .. }
        | SessionWork::HistoryResize { .. } => 0,
    }
}

/// 每个 pane 的快照回执只带请求代次与 revision，视口位置由终端引擎独占。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SnapshotRequestToken {
    generation: u64,
}

/// 一个 pane 一份 tracker；显式视口操作会使旧代次失效，快照请求只读取引擎当前视口。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SnapshotRequestGate {
    generation: u64,
    accepted_revision: Option<u64>,
}

impl SnapshotRequestGate {
    /// 建立只读当前引擎视口的请求；revision 是唯一增量读取基线，不能由 UI 回写滚动状态。
    ///
    /// @author kongweiguang
    pub fn begin(
        &mut self,
    ) -> (
        SnapshotRequestToken,
        kerminal_terminal::snapshot::RenderSnapshotRequest,
    ) {
        self.generation = self.generation.wrapping_add(1);
        let token = SnapshotRequestToken {
            generation: self.generation,
        };
        (
            token,
            kerminal_terminal::snapshot::RenderSnapshotRequest {
                since_revision: self.accepted_revision,
            },
        )
    }

    /// 接受最新代次且 revision 不回退的引擎快照；SessionId 由持有 pane 的 runtime 另行核对。
    ///
    /// @author kongweiguang
    pub fn accept(&mut self, token: SnapshotRequestToken, revision: u64) -> bool {
        if token.generation != self.generation
            || self
                .accepted_revision
                .is_some_and(|current| revision < current)
        {
            return false;
        }
        self.accepted_revision = Some(revision);
        true
    }

    /// Pane 切换/会话关闭时使所有未完成回执失效，但不改变最后可见快照。
    pub fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/root_session_queue.rs"]
mod tests;
