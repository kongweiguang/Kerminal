#![cfg(feature = "acceptance-metrics")]
//! GPUI 窗口帧与隔离单字符输入的验收适配器。
//!
//! 原始直方图只覆盖 GPUI 的失效到平台呈现调用、窗口绘制与动画呈现间隔；
//! 它不测量操作系统合成器、扫描输出或光子级可见延迟。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};

use gpui_kit::{
    FrameDurationSnapshot, InputLatencySnapshot, ScrollDelta, TouchPhase, Window,
    profiler::{FRAME_DURATION_BUCKETS_MS, histogram_bucket_counts},
};
use serde::Serialize;

const MAX_PENDING_TERMINAL_ECHO_PROBES: usize = 256;
const MAX_TERMINAL_ECHO_SAMPLES: usize = 10_000;
const MAX_TERMINAL_ECHO_PHASE_EVENTS_PER_PROBE: usize = 64;
const TERMINAL_ECHO_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
pub const TERMINAL_ECHO_TEST_CHARACTER: &str = "x";

/// Root writer ACK 与 Canvas paint 共用这一时限，避免一个阶段提前丢弃另一个阶段仍需的清理状态。
///
/// @author kongweiguang
pub const fn terminal_echo_probe_timeout() -> std::time::Duration {
    TERMINAL_ECHO_PROBE_TIMEOUT
}
pub const TERMINAL_ECHO_EXPECTED_SESSIONS_ENV: &str =
    "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO_EXPECTED_SESSIONS";

/// expectedTotal 配置值无效时返回的稳定错误类别，不附带原始环境变量正文。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TerminalEchoExpectedSessionsConfigError {
    InvalidValue,
}

/// 只接受三个 workload 总会话规模；缺省保留原有无数量门禁的采样行为。
///
/// @author kongweiguang
pub fn parse_terminal_echo_expected_sessions(
    value: Option<&str>,
) -> Result<Option<usize>, TerminalEchoExpectedSessionsConfigError> {
    match value {
        None => Ok(None),
        Some("1") => Ok(Some(1)),
        Some("10") => Ok(Some(10)),
        Some("50") => Ok(Some(50)),
        Some(_) => Err(TerminalEchoExpectedSessionsConfigError::InvalidValue),
    }
}

/// 单 ASCII 字符的验收探针只携带目标 cell，不缓存命令或终端文本。
#[derive(Clone)]
pub struct TerminalEchoProbe {
    probe_id: String,
    session_id: String,
    session_generation: u64,
    target: TerminalEchoTarget,
}

/// Canvas 与 writer ACK 必须关联到同一网格 cell；行列为终端 cell 坐标。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEchoTarget {
    pub viewport_row: u16,
    pub column: u16,
}

/// Canvas 只向 registry 提供单 cell 匹配和成功 paint 的布尔证据，不传入行文本。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalEchoPaintObservation {
    pub target_cell_is_character: bool,
    pub painted_run_covers_target: bool,
    pub cursor_visible: bool,
    pub cursor_row: u16,
    pub cursor_column: u16,
    pub at_bottom: bool,
    /// 与同次 paint 的几何事实一起记录，不参与探针准入或延迟边界。
    pub window_active_at_paint: bool,
}

impl TerminalEchoProbe {
    /// 暴露不包含输入正文的随机探针标识，供验收驱动记录提交状态。
    ///
    /// @author kongweiguang
    pub fn probe_id(&self) -> &str {
        &self.probe_id
    }

    /// 返回与随机探针绑定的输入代次，便于 Root 在发送前再次核对连接生命周期。
    ///
    /// @author kongweiguang
    pub fn session_generation(&self) -> u64 {
        self.session_generation
    }

    /// 固定目标 cell 供 Root 请求和 Canvas paint 复用同一坐标证据。
    ///
    /// @author kongweiguang
    pub const fn target(&self) -> TerminalEchoTarget {
        self.target
    }
}

impl fmt::Debug for TerminalEchoProbe {
    /// 调试日志只展示探针和会话关联，绝不输出合成字符或终端正文。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TerminalEchoProbe")
            .field("probe_id", &self.probe_id)
            .field("session_id", &self.session_id)
            .field("session_generation", &self.session_generation)
            .field("target", &self.target)
            .finish()
    }
}

/// 有界 echo 路径阶段；仅覆盖 UI 可见边界，不能解释为 PTY 内部阶段。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEchoPhase {
    FifoDequeued,
    FifoEnqueueAccepted,
    SessionWriteStarted,
    SessionWriteReturned,
    WatchReceived,
    SnapshotRequested,
    SnapshotReturned,
    SnapshotAccepted,
    SnapshotRejected,
    CanvasPainted,
}

/// A bounded timestamp from the UI-visible probe path; `revision` is present only where sourced.
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEchoPhaseEvent {
    pub phase: TerminalEchoPhase,
    pub revision: Option<u64>,
    pub offset_ns: u64,
}

/// 单字符 FIFO→Canvas 实测结果；保留会话、snapshot revision 与 paint 时窗口状态，不保存绘制文本或字符。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEchoSample {
    pub probe_id: String,
    pub session_id: String,
    pub session_generation: u64,
    pub revision: u64,
    /// Root FIFO 入队尝试到 GPUI Canvas 实际文字绘制提交的耗时，不代表 OS 屏幕呈现延迟。
    pub latency_ns: u64,
    /// 仅记录文字实际 paint 回调时的窗口状态，不代表测量区间内始终有焦点。
    pub window_active_at_paint: bool,
    /// Feature-only phase stamps; absent infrastructure stages are intentionally not inferred.
    pub phase_events: Vec<TerminalEchoPhaseEvent>,
    pub phase_events_dropped: u32,
}

/// 固定的 probe admission 拒绝类别；值不包含 session 文本、命令或输入正文。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEchoAdmissionRejection {
    ConfigurationRejected,
    BindingNotCurrent,
    FreshPaneHidden,
    DuplicateProbeLoop,
    MeasurementStartUnavailable,
    SessionCountReadFailed,
    SessionCountMismatch,
    ContextReadFailed,
    SessionMismatch,
    OwnerMismatch,
    GenerationMismatch,
    PromptNotReady,
    PromptLineNotEmpty,
    AlternateScreen,
    NonUiOwner,
    SessionNotRunning,
    ShellIntegrationDisabled,
    ShellIntegrationNotObserved,
    UnsupportedShellDialect,
    ExternalInputObserved,
    InputWritePending,
    UiInputProjectionInvalidated,
    ManagedCommandActive,
    ContextUnavailable,
    SnapshotUnavailable,
    LocalTargetUnavailable,
    ProbePreparationFailed,
    ProbeSubmissionRejected,
    EmptyProjectionUnavailable,
    SnapshotTargetUnavailable,
    InputQueueBusy,
    QueueRejected,
}

/// 清理只能依赖当前 writer ACK 投影；状态未知时固定停机，不猜测或重试 Backspace。
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminalEchoCleanupStopReason {
    BindingChanged,
    ContextChanged,
    ProjectionChanged,
    QueueNotIdle,
    CharacterAckUnavailable,
    CleanupQueueRejected,
    CleanupAckUnavailable,
    MeasurementEndedBeforeCleanupAck,
}

/// 供报告序列化的有限 probe 状态计数；拒绝原因只存固定枚举与次数。
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEchoProbeDiagnostics {
    pub hook_invocations: u64,
    pub polls: u64,
    pub issued: u64,
    pub enqueued: u64,
    pub queue_rejected: u64,
    pub writer_acknowledged: u64,
    pub paint_matched: u64,
    pub timed_out: u64,
    pub cleanup_enqueued: u64,
    pub cleanup_acknowledged: u64,
    pub cleanup_stopped: u64,
    pub cleanup_stop_reasons: BTreeMap<TerminalEchoCleanupStopReason, u64>,
    pub admission_rejections: BTreeMap<TerminalEchoAdmissionRejection, u64>,
    pub startup_admission_rejections: BTreeMap<TerminalEchoAdmissionRejection, u64>,
}

/// Canvas wheel callback 的入口计数；只保留阶段和非零原始 delta 事件数，不含坐标或会话信息。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalWheelDiagnostics {
    pub callbacks: u64,
    pub started: u64,
    pub moved: u64,
    pub ended: u64,
    pub cancelled: u64,
    pub raw_nonzero_delta_events: u64,
}

/// 完结一次测量时同时取走样本和状态计数，避免 pending probe 静默丢失。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TerminalEchoProbeMeasurement {
    pub samples: Vec<TerminalEchoSample>,
    pub diagnostics: TerminalEchoProbeDiagnostics,
    pub terminal_wheel: TerminalWheelDiagnostics,
}

struct PendingTerminalEchoProbe {
    probe_id: String,
    session_id: String,
    session_generation: u64,
    target: TerminalEchoTarget,
    submitted_after_revision: u64,
    submitted_at: Instant,
    enqueued: bool,
    initial_context: kerminal_application::sessions::TerminalCommandContextVersion,
    writer_ack_at: Option<Instant>,
    writer_context: Option<kerminal_application::sessions::TerminalCommandContextVersion>,
    paint: Option<(u64, Instant, bool)>,
    sample_recorded: bool,
    timed_out: bool,
    cleanup_authorized: bool,
    cleanup_enqueued: bool,
    phase_events: Vec<TerminalEchoPhaseEvent>,
    phase_events_dropped: u32,
}

#[derive(Default)]
struct TerminalEchoProbeState {
    pending: VecDeque<PendingTerminalEchoProbe>,
    completed: VecDeque<TerminalEchoSample>,
    measurement_started_at: Option<Instant>,
    diagnostics: TerminalEchoProbeDiagnostics,
    terminal_wheel: TerminalWheelDiagnostics,
}

/// 仅在 `acceptance-metrics` 构建中存在的短期、限量字符探针关联器。
///
/// 样本必须同时具备严格递增的 writer ACK 与匹配 cell 的成功 Canvas text-paint 提交；普通输入正文从不进入状态。
#[derive(Clone, Default)]
pub struct TerminalEchoProbeRegistry {
    state: Arc<Mutex<TerminalEchoProbeState>>,
}

impl TerminalEchoProbeRegistry {
    /// 统计 Canvas 收到的原始 wheel callback 阶段与非零 delta；计数位于处理/命中/路由之前。
    ///
    /// 非有限分量不算有效非零 delta；这里不换算行数，也不据此推断 Root 已处理滚动。
    ///
    /// @author kongweiguang
    pub fn record_canvas_wheel_callback(&self, phase: TouchPhase, delta: &ScrollDelta) {
        let has_nonzero_delta = match delta {
            ScrollDelta::Lines(lines) => [lines.x, lines.y]
                .into_iter()
                .any(|value| value.is_finite() && value != 0.0),
            ScrollDelta::Pixels(pixels) => [pixels.x.as_f32(), pixels.y.as_f32()]
                .into_iter()
                .any(|value| value.is_finite() && value != 0.0),
        };
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let diagnostics = &mut state.terminal_wheel;
        diagnostics.callbacks = diagnostics.callbacks.saturating_add(1);
        match phase {
            TouchPhase::Started => diagnostics.started = diagnostics.started.saturating_add(1),
            TouchPhase::Moved => diagnostics.moved = diagnostics.moved.saturating_add(1),
            TouchPhase::Ended => diagnostics.ended = diagnostics.ended.saturating_add(1),
            TouchPhase::Cancelled => {
                diagnostics.cancelled = diagnostics.cancelled.saturating_add(1)
            }
        }
        if has_nonzero_delta {
            diagnostics.raw_nonzero_delta_events =
                diagnostics.raw_nonzero_delta_events.saturating_add(1);
        }
    }

    /// 为隔离会话签发唯一票据；票据只保存目标坐标，不保存合成字符或用户输入。
    ///
    /// @author kongweiguang
    pub fn issue_probe(
        &self,
        session_id: impl Into<String>,
        session_generation: u64,
        target: TerminalEchoTarget,
    ) -> TerminalEchoProbe {
        let probe_id = uuid::Uuid::new_v4().to_string();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.diagnostics.issued = state.diagnostics.issued.saturating_add(1);
        TerminalEchoProbe {
            probe_id,
            session_id: session_id.into(),
            session_generation,
            target,
        }
    }

    /// 只接受签发探针在其绑定会话上的真实提交，并固定输入前 revision 作为迟到帧下界。
    ///
    /// @author kongweiguang
    pub fn record_submission(
        &self,
        probe: &TerminalEchoProbe,
        session_id: &str,
        session_generation: u64,
        initial_context: kerminal_application::sessions::TerminalCommandContextVersion,
        after_revision: u64,
        submitted_at: Instant,
    ) -> bool {
        if probe.session_id != session_id
            || probe.session_generation != session_generation
            || initial_context.session_generation != session_generation
        {
            return false;
        }
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.pending.len() >= MAX_PENDING_TERMINAL_ECHO_PROBES
            || state
                .pending
                .iter()
                .any(|pending| pending.probe_id == probe.probe_id)
        {
            return false;
        }
        state.pending.push_back(PendingTerminalEchoProbe {
            probe_id: probe.probe_id.clone(),
            session_id: probe.session_id.clone(),
            session_generation: probe.session_generation,
            target: probe.target,
            submitted_after_revision: after_revision,
            submitted_at,
            enqueued: false,
            initial_context,
            writer_ack_at: None,
            writer_context: None,
            paint: None,
            sample_recorded: false,
            timed_out: false,
            cleanup_authorized: false,
            cleanup_enqueued: false,
            phase_events: Vec::new(),
            phase_events_dropped: 0,
        });
        true
    }

    /// 统计一次已执行上下文轮询；窗口采集器关闭时 registry 仍只保留进程内汇总。
    ///
    /// @author kongweiguang
    pub fn record_poll(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.diagnostics.polls = state.diagnostics.polls.saturating_add(1);
    }

    /// 仅记录已有 probe 的 Root FIFO、SessionService、subscription、snapshot 与 Canvas 回调边界；不推断 PTY 内部阶段。
    ///
    /// @author kongweiguang
    pub fn record_phase_for_probe(
        &self,
        probe_id: &str,
        phase: TerminalEchoPhase,
        revision: Option<u64>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = state.pending.iter_mut().find(|pending| {
            pending.probe_id == probe_id
                && !pending.timed_out
                && pending.paint.is_none()
                && !pending.cleanup_enqueued
        }) {
            push_terminal_echo_phase(pending, phase, revision, Instant::now());
        }
    }

    /// 把没有 probe DTO 的 session subscription/snapshot 回调关联到同一 session generation 的活动 probe。
    ///
    /// watch 与 snapshot 事件按有界 revision 日志保留，分析时必须对照 Canvas 最终 paint revision。
    ///
    /// @author kongweiguang
    pub fn record_phase_for_session(
        &self,
        session_id: &str,
        session_generation: u64,
        phase: TerminalEchoPhase,
        revision: Option<u64>,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pending) = state.pending.iter_mut().find(|pending| {
            pending.session_id == session_id
                && pending.session_generation == session_generation
                && pending.enqueued
                && !pending.timed_out
                && pending.paint.is_none()
                && !pending.cleanup_enqueued
        }) {
            push_terminal_echo_phase(pending, phase, revision, Instant::now());
        }
    }

    /// 保留 fresh hook 是否到达 Root 的证据，即使它发生在 desktop warmup reset 之前。
    ///
    /// @author kongweiguang
    pub fn record_hook_invocation(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.diagnostics.hook_invocations = state.diagnostics.hook_invocations.saturating_add(1);
    }

    /// 以固定拒绝码记录 context 或生命周期门禁，不把异常文本带入原生报告。
    ///
    /// @author kongweiguang
    pub fn record_admission_rejection(&self, rejection: TerminalEchoAdmissionRejection) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rejections = if state.measurement_started_at.is_some() {
            &mut state.diagnostics.admission_rejections
        } else {
            &mut state.diagnostics.startup_admission_rejections
        };
        let count = rejections.entry(rejection).or_default();
        *count = count.saturating_add(1);
    }

    /// 只在 Root FIFO 明确接受请求后标记 enqueued，Canvas 即使很快绘制也不能伪造该阶段。
    ///
    /// @author kongweiguang
    pub fn record_enqueued(&self, probe: &TerminalEchoProbe) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        if pending.enqueued {
            return false;
        }
        pending.enqueued = true;
        push_terminal_echo_phase(
            pending,
            TerminalEchoPhase::FifoEnqueueAccepted,
            None,
            Instant::now(),
        );
        state.diagnostics.enqueued = state.diagnostics.enqueued.saturating_add(1);
        true
    }

    /// FIFO 拒绝与绘制超时分开计数；无写入 ACK 时不生成可清理票据。
    ///
    /// @author kongweiguang
    pub fn record_queue_rejected(&self, probe: &TerminalEchoProbe) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        state.pending.remove(index);
        state.diagnostics.queue_rejected = state.diagnostics.queue_rejected.saturating_add(1);
        let count = state
            .diagnostics
            .admission_rejections
            .entry(TerminalEchoAdmissionRejection::QueueRejected)
            .or_default();
        *count = count.saturating_add(1);
        true
    }

    /// 超时只关闭 Canvas 计时，不自动删除待清理状态；Root 仍须按 exact writer ACK 决定安全清理。
    ///
    /// @author kongweiguang
    pub fn expire_probes(&self, now: Instant) -> u64 {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut expired = 0_u64;
        for pending in &mut state.pending {
            if pending.enqueued
                && !pending.timed_out
                && now.saturating_duration_since(pending.submitted_at)
                    >= TERMINAL_ECHO_PROBE_TIMEOUT
            {
                pending.timed_out = true;
                expired = expired.saturating_add(1);
            }
        }
        state.diagnostics.timed_out = state.diagnostics.timed_out.saturating_add(expired);
        expired
    }

    /// 返回当前 session/generation 唯一待验的 cell；Canvas 只检查该坐标而不复制任意正文。
    ///
    /// @author kongweiguang
    pub fn paint_target(
        &self,
        session_id: &str,
        session_generation: u64,
    ) -> Option<TerminalEchoTarget> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .iter()
            .find(|pending| {
                pending.enqueued
                    && !pending.cleanup_enqueued
                    && pending.session_id == session_id
                    && pending.session_generation == session_generation
            })
            .map(|pending| pending.target)
    }

    /// 只有普通 UI writer ACK、预期版本、精确投影和空闲 FIFO 全部一致才允许认定字符输入完成。
    ///
    /// @author kongweiguang
    pub fn record_writer_ack(
        &self,
        probe: &TerminalEchoProbe,
        actual_context: kerminal_application::sessions::TerminalCommandContextVersion,
        projection_matches: bool,
        queue_idle: bool,
        acknowledged_at: Instant,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !projection_matches || !queue_idle {
            return false;
        }
        let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        let accepted_input_delta = actual_context
            .input_generation
            .checked_sub(pending.initial_context.input_generation);
        if !pending.enqueued
            || pending.session_id != probe.session_id
            || pending.session_generation != probe.session_generation
            || pending.writer_context.is_some()
            || accepted_input_delta != Some(1)
            || actual_context.session_generation != pending.initial_context.session_generation
            || actual_context.prompt_epoch != pending.initial_context.prompt_epoch
        {
            return false;
        }
        let probe_id = pending.probe_id.clone();
        pending.writer_ack_at = Some(acknowledged_at);
        pending.writer_context = Some(actual_context);
        state.diagnostics.writer_acknowledged =
            state.diagnostics.writer_acknowledged.saturating_add(1);
        record_completed_sample(&mut state, &probe_id);
        true
    }

    /// 在任何异步写入前，拒绝不同 session、旧 revision、错 cell、错光标或滚离底部的 paint。
    /// 活动状态只与成功匹配的 paint 一起记录，不改变 admission、延迟边界或清理准入。
    ///
    /// @author kongweiguang
    pub fn record_canvas_character_paint(
        &self,
        session_id: &str,
        session_generation: u64,
        revision: u64,
        target: TerminalEchoTarget,
        observation: TerminalEchoPaintObservation,
        painted_at: Instant,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state.pending.iter_mut().find(|pending| {
            pending.enqueued
                && !pending.cleanup_enqueued
                && pending.session_id == session_id
                && pending.session_generation == session_generation
                && pending.target == target
                && revision > pending.submitted_after_revision
                && painted_at >= pending.submitted_at
                && !pending.timed_out
        }) else {
            return;
        };
        if !observation.target_cell_is_character
            || !observation.painted_run_covers_target
            || !observation.cursor_visible
            || observation.cursor_row != target.viewport_row
            || observation.cursor_column != target.column.saturating_add(1)
            || !observation.at_bottom
        {
            return;
        }
        let probe_id = pending.probe_id.clone();
        if pending.paint.is_none() {
            pending.paint = Some((revision, painted_at, observation.window_active_at_paint));
            push_terminal_echo_phase(
                pending,
                TerminalEchoPhase::CanvasPainted,
                Some(revision),
                painted_at,
            );
        }
        record_completed_sample(&mut state, &probe_id);
    }

    /// Root 重新核对同一 writer ACK context、单字符投影和空闲 FIFO 后才拿到清理准入。
    ///
    /// @author kongweiguang
    pub fn authorize_cleanup(
        &self,
        probe: &TerminalEchoProbe,
        current_context: kerminal_application::sessions::TerminalCommandContextVersion,
        projection_is_exact_character: bool,
        queue_idle: bool,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        if pending.writer_context != Some(current_context)
            || pending.cleanup_enqueued
            || !projection_is_exact_character
            || !queue_idle
        {
            return false;
        }
        pending.cleanup_authorized = true;
        true
    }

    /// 只有已授权的 cleanup work 被 Root FIFO 接纳后才累计清理提交数。
    ///
    /// @author kongweiguang
    pub fn record_cleanup_enqueued(&self, probe: &TerminalEchoProbe) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        if !pending.cleanup_authorized || pending.cleanup_enqueued {
            return false;
        }
        pending.cleanup_enqueued = true;
        state.diagnostics.cleanup_enqueued = state.diagnostics.cleanup_enqueued.saturating_add(1);
        true
    }

    /// Backspace 只有在同一 generation/prompt 的下一输入代次、空行投影及 FIFO 空闲都获 ACK 时才闭环。
    ///
    /// @author kongweiguang
    pub fn record_cleanup_ack(
        &self,
        probe: &TerminalEchoProbe,
        actual_context: kerminal_application::sessions::TerminalCommandContextVersion,
        projection_is_empty: bool,
        queue_idle: bool,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        let pending = &state.pending[index];
        let input_delta = pending.writer_context.and_then(|context| {
            actual_context
                .input_generation
                .checked_sub(context.input_generation)
        });
        let valid = pending.cleanup_enqueued
            && input_delta == Some(1)
            && pending.writer_context.is_some_and(|context| {
                context.session_generation == actual_context.session_generation
                    && context.prompt_epoch == actual_context.prompt_epoch
            })
            && projection_is_empty
            && queue_idle;
        if !valid {
            return false;
        }
        state.pending.remove(index);
        state.diagnostics.cleanup_acknowledged =
            state.diagnostics.cleanup_acknowledged.saturating_add(1);
        true
    }

    /// 任何未知写入结果、绑定变化或投影不符都会结束该探针，不能重复发送或猜测删除字符。
    ///
    /// @author kongweiguang
    pub fn record_cleanup_stop(
        &self,
        probe: &TerminalEchoProbe,
        reason: TerminalEchoCleanupStopReason,
    ) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(index) = state
            .pending
            .iter()
            .position(|pending| pending.probe_id == probe.probe_id)
        else {
            return false;
        };
        state.pending.remove(index);
        state.diagnostics.cleanup_stopped = state.diagnostics.cleanup_stopped.saturating_add(1);
        let count = state
            .diagnostics
            .cleanup_stop_reasons
            .entry(reason)
            .or_default();
        *count = count.saturating_add(1);
        true
    }

    /// 查询单次探针是否同时具备真实 writer ACK 和有效 Canvas paint；完成后仍等待清理 ACK。
    ///
    /// @author kongweiguang
    pub fn sample_is_ready(&self, probe: &TerminalEchoProbe) -> bool {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .iter()
            .any(|pending| pending.probe_id == probe.probe_id && pending.sample_recorded)
    }

    /// 读取并清空当前测量区间的有限样本，避免跨次验收串联结果。
    ///
    /// @author kongweiguang
    pub fn take_samples(&self) -> Vec<TerminalEchoSample> {
        self.take_measurement().samples
    }

    /// 测量窗口结束时显式记下未完成 paint/cleanup，并原子取走 echo 与 Canvas wheel 计数。
    ///
    /// @author kongweiguang
    pub fn take_measurement(&self) -> TerminalEchoProbeMeasurement {
        self.expire_probes(Instant::now());
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let unresolved = u64::try_from(
            state
                .pending
                .iter()
                .filter(|pending| pending.enqueued && !pending.timed_out)
                .count(),
        )
        .unwrap_or(u64::MAX);
        let cleanup_unresolved = u64::try_from(state.pending.len()).unwrap_or(u64::MAX);
        if cleanup_unresolved > 0 {
            let count = state
                .diagnostics
                .cleanup_stop_reasons
                .entry(TerminalEchoCleanupStopReason::MeasurementEndedBeforeCleanupAck)
                .or_default();
            *count = count.saturating_add(cleanup_unresolved);
            state.diagnostics.cleanup_stopped = state
                .diagnostics
                .cleanup_stopped
                .saturating_add(cleanup_unresolved);
        }
        state.pending.clear();
        state.diagnostics.timed_out = state.diagnostics.timed_out.saturating_add(unresolved);
        TerminalEchoProbeMeasurement {
            samples: state.completed.drain(..).collect(),
            diagnostics: std::mem::take(&mut state.diagnostics),
            terminal_wheel: std::mem::take(&mut state.terminal_wheel),
        }
    }

    /// 在窗口基线后清除预热或上一轮残留，重置 wheel 计数并记下测量区间的共同起点。
    ///
    /// @author kongweiguang
    pub fn reset(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending.clear();
        state.completed.clear();
        let hook_invocations = state.diagnostics.hook_invocations;
        let startup_admission_rejections =
            std::mem::take(&mut state.diagnostics.startup_admission_rejections);
        state.diagnostics = TerminalEchoProbeDiagnostics {
            hook_invocations,
            startup_admission_rejections,
            ..TerminalEchoProbeDiagnostics::default()
        };
        state.terminal_wheel = TerminalWheelDiagnostics::default();
        state.measurement_started_at = Some(Instant::now());
    }

    /// 返回 desktop warmup 基线清理时刻，使 Root probe 与同一测量窗口对齐。
    ///
    /// @author kongweiguang
    pub fn measurement_started_at(&self) -> Option<Instant> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .measurement_started_at
    }
}

/// 仅当同一探针已获得 writer ACK 与成功绘制的目标 cell 时产出一次样本，并保留 pending 直到清理闭环。
///
/// @author kongweiguang
fn record_completed_sample(state: &mut TerminalEchoProbeState, probe_id: &str) {
    let sample = {
        let Some(pending) = state
            .pending
            .iter_mut()
            .find(|pending| pending.probe_id == probe_id)
        else {
            return;
        };
        let (Some(_writer_ack_at), Some((revision, painted_at, window_active_at_paint))) =
            (pending.writer_ack_at, pending.paint)
        else {
            return;
        };
        if pending.sample_recorded || pending.timed_out || painted_at < pending.submitted_at {
            return;
        }
        pending.sample_recorded = true;
        TerminalEchoSample {
            probe_id: pending.probe_id.clone(),
            session_id: pending.session_id.clone(),
            session_generation: pending.session_generation,
            revision,
            latency_ns: u64::try_from(painted_at.duration_since(pending.submitted_at).as_nanos())
                .unwrap_or(u64::MAX),
            window_active_at_paint,
            phase_events: {
                let mut events = pending.phase_events.clone();
                events.sort_by_key(|event| event.offset_ns);
                events
            },
            phase_events_dropped: pending.phase_events_dropped,
        }
    };
    if state.completed.len() == MAX_TERMINAL_ECHO_SAMPLES {
        state.completed.pop_front();
    }
    state.completed.push_back(sample);
    state.diagnostics.paint_matched = state.diagnostics.paint_matched.saturating_add(1);
}

/// 按提交基准追加有界诊断时点；超出预算显式计数，避免高频刷新让诊断内存无界增长。
///
/// @author kongweiguang
fn push_terminal_echo_phase(
    pending: &mut PendingTerminalEchoProbe,
    phase: TerminalEchoPhase,
    revision: Option<u64>,
    at: Instant,
) {
    if pending.phase_events.len() >= MAX_TERMINAL_ECHO_PHASE_EVENTS_PER_PROBE {
        if phase == TerminalEchoPhase::CanvasPainted {
            let replaceable = pending.phase_events.iter().position(|event| {
                matches!(
                    event.phase,
                    TerminalEchoPhase::WatchReceived
                        | TerminalEchoPhase::SnapshotRequested
                        | TerminalEchoPhase::SnapshotReturned
                        | TerminalEchoPhase::SnapshotAccepted
                        | TerminalEchoPhase::SnapshotRejected
                )
            });
            if let Some(index) = replaceable {
                pending.phase_events.remove(index);
                pending.phase_events_dropped = pending.phase_events_dropped.saturating_add(1);
            } else {
                pending.phase_events_dropped = pending.phase_events_dropped.saturating_add(1);
                return;
            }
        } else {
            pending.phase_events_dropped = pending.phase_events_dropped.saturating_add(1);
            return;
        }
    }
    let offset_ns = u64::try_from(
        at.saturating_duration_since(pending.submitted_at)
            .as_nanos(),
    )
    .unwrap_or(u64::MAX);
    pending.phase_events.push(TerminalEchoPhaseEvent {
        phase,
        revision,
        offset_ns,
    });
}

/// Root、Canvas 与 desktop 报告共用同一个 feature-only registry，不改变生产默认路径。
///
/// @author kongweiguang
pub fn terminal_echo_probe_registry() -> &'static TerminalEchoProbeRegistry {
    static REGISTRY: OnceLock<TerminalEchoProbeRegistry> = OnceLock::new();
    REGISTRY.get_or_init(TerminalEchoProbeRegistry::default)
}

/// 指标使用的时间范围；基线快照允许剔除预热帧。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementInterval {
    /// 累计窗口 profiler 创建后的全部样本。
    WindowLifetimeCumulative,
    /// 只统计传入基线之后新增的样本。
    SinceBaseline,
}

/// 由验收驱动明确标注的实际渲染后端类别。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RendererKind {
    Gpu,
    Cpu,
    Unknown,
}

/// 便于与原始验收报告关联的无机器隐私环境标识。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FrameMetricsEnvironment {
    /// 由验收运行者提供的稳定环境名称，不自动读取主机名。
    pub environment_id: String,
    /// 负载名称，例如 terminal_output_10_sessions。
    pub workload: String,
    /// Rust 编译目标操作系统。
    pub os: String,
    /// Rust 编译目标架构。
    pub architecture: String,
    /// 运行者根据实际 GPU/CPU 后端证据填写，采集器不猜测。
    pub renderer: RendererKind,
}

impl FrameMetricsEnvironment {
    /// 固定编译目标信息，并要求调用方显式提供环境、负载和渲染器标识。
    pub fn new(
        environment_id: impl Into<String>,
        workload: impl Into<String>,
        renderer: RendererKind,
    ) -> Self {
        Self {
            environment_id: environment_id.into(),
            workload: workload.into(),
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            renderer,
        }
    }
}

/// 一个 GPUI 直方图的样本数、95 分位和差分桶。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistogramSummary {
    pub sample_count: u64,
    /// 纳秒分布为纳秒，事件批量分布为事件数；单位由所属字段确定。
    pub p95: Option<u64>,
    pub unit: HistogramUnit,
    /// HDR 桶的上界按字段说明的单位排列；末尾 `None` 表示高于最后一个上界。
    pub buckets: Vec<HistogramBucket>,
}

/// 一个差分 HDR 直方图桶；保留聚合分布，避免误把分位数差当作样本差。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistogramBucket {
    /// 纳秒指标为纳秒，事件批量指标为事件数；`None` 是溢出桶。
    pub upper_bound: Option<u64>,
    pub sample_count: u64,
}

/// 汇总直方图分位数的单位，避免把每帧输入个数误标成时间。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HistogramUnit {
    Nanoseconds,
    EventsPerFrame,
}

/// 独立记录 GPUI 全局输入到窗口帧的统计，不代表特定终端的 PTY 回显延迟。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowInputMetrics {
    pub interval: MeasurementInterval,
    /// GPUI 输入事件到平台帧提交的延迟；不是本地终端输入回显。
    pub gpui_input_to_frame: HistogramSummary,
    /// 每个响应帧合并的 GPUI 输入事件数。
    pub gpui_input_events_per_frame: HistogramSummary,
    /// profiler 排除的 draw 中到达输入事件总数差分。
    pub mid_draw_events_dropped: u64,
}

/// 单个窗口的一次 GPUI 帧采样结果。
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WindowFrameMetrics {
    pub interval: MeasurementInterval,
    pub environment: FrameMetricsEnvironment,
    /// 从首次失效到 GPUI 调用平台 draw/present 的间隔，不代表 OS 屏幕呈现延迟。
    pub gpui_dirty_to_present: HistogramSummary,
    /// GPUI Window::draw 本身的处理时长。
    pub gpui_window_draw: HistogramSummary,
    /// 窗口处于动画状态时相邻平台呈现调用的间隔。
    pub gpui_animated_present_interval: HistogramSummary,
}

/// 直方图基线与当前快照不匹配时停止生成结果。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FrameMetricsError {
    DirtyToPresent,
    WindowDraw,
    PresentInterval,
}

/// 输入快照计数无法扣除时返回的具体差分错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputMetricsError {
    InputLatency,
    EventsPerFrame,
    MidDrawDropped,
}

impl fmt::Display for InputMetricsError {
    /// 为窗口级输入统计保留稳定、可检索且不混淆 PTY 回显的错误名称。
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let metric = match self {
            Self::InputLatency => "gpui_input_to_frame",
            Self::EventsPerFrame => "gpui_input_events_per_frame",
            Self::MidDrawDropped => "gpui_mid_draw_events_dropped",
        };
        write!(formatter, "无法从当前 {metric} 指标中扣除基线")
    }
}

impl std::error::Error for InputMetricsError {}

impl fmt::Display for FrameMetricsError {
    /// 给每种直方图差分失败提供稳定、可检索的说明。
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let histogram = match self {
            Self::DirtyToPresent => "dirty_to_present",
            Self::WindowDraw => "window_draw",
            Self::PresentInterval => "animated_present_interval",
        };
        write!(formatter, "无法从当前 {histogram} 直方图中扣除基线")
    }
}

impl std::error::Error for FrameMetricsError {}

/// 从两个同一窗口的 GPUI 快照计算原始时序指标。
///
/// 传入的基线应在预热完成后读取；基线和当前快照必须来自同一窗口。
/// 省略基线时保留窗口生命周期累计口径。所有差分在本地直方图克隆上执行，
/// 一项差分失败会整体返回错误，避免输出部分或伪造的性能结果。
///
/// @author kongweiguang
pub fn summarize_frame_metrics(
    current: &FrameDurationSnapshot,
    baseline: Option<&FrameDurationSnapshot>,
    environment: FrameMetricsEnvironment,
) -> Result<WindowFrameMetrics, FrameMetricsError> {
    let mut dirty_to_present = current.dirty_to_present_histogram.clone();
    let mut window_draw = current.draw_duration_histogram.clone();
    let mut animated_present_interval = current.present_interval_histogram.clone();

    if let Some(baseline) = baseline {
        dirty_to_present
            .subtract(&baseline.dirty_to_present_histogram)
            .map_err(|_| FrameMetricsError::DirtyToPresent)?;
        window_draw
            .subtract(&baseline.draw_duration_histogram)
            .map_err(|_| FrameMetricsError::WindowDraw)?;
        animated_present_interval
            .subtract(&baseline.present_interval_histogram)
            .map_err(|_| FrameMetricsError::PresentInterval)?;
    }

    let bounds_ns = FRAME_DURATION_BUCKETS_MS.map(|bound| bound * 1_000_000);
    Ok(WindowFrameMetrics {
        interval: if baseline.is_some() {
            MeasurementInterval::SinceBaseline
        } else {
            MeasurementInterval::WindowLifetimeCumulative
        },
        environment,
        gpui_dirty_to_present: histogram_summary(
            dirty_to_present.len(),
            (!dirty_to_present.is_empty()).then(|| dirty_to_present.value_at_quantile(0.95)),
            &bounds_ns,
            histogram_bucket_counts(&dirty_to_present, bounds_ns),
            HistogramUnit::Nanoseconds,
        ),
        gpui_window_draw: histogram_summary(
            window_draw.len(),
            (!window_draw.is_empty()).then(|| window_draw.value_at_quantile(0.95)),
            &bounds_ns,
            histogram_bucket_counts(&window_draw, bounds_ns),
            HistogramUnit::Nanoseconds,
        ),
        gpui_animated_present_interval: histogram_summary(
            animated_present_interval.len(),
            (!animated_present_interval.is_empty())
                .then(|| animated_present_interval.value_at_quantile(0.95)),
            &bounds_ns,
            histogram_bucket_counts(&animated_present_interval, bounds_ns),
            HistogramUnit::Nanoseconds,
        ),
    })
}

/// 对同一窗口的 GPUI 输入快照单独差分，避免将全局输入帧统计标成终端回显。
///
/// @author kongweiguang
pub fn summarize_input_metrics(
    current: &InputLatencySnapshot,
    baseline: Option<&InputLatencySnapshot>,
) -> Result<WindowInputMetrics, InputMetricsError> {
    let mut input_latency = current.latency_histogram.clone();
    let mut events_per_frame = current.events_per_frame_histogram.clone();
    let mut mid_draw_events_dropped = current.mid_draw_events_dropped;

    if let Some(baseline) = baseline {
        input_latency
            .subtract(&baseline.latency_histogram)
            .map_err(|_| InputMetricsError::InputLatency)?;
        events_per_frame
            .subtract(&baseline.events_per_frame_histogram)
            .map_err(|_| InputMetricsError::EventsPerFrame)?;
        mid_draw_events_dropped = mid_draw_events_dropped
            .checked_sub(baseline.mid_draw_events_dropped)
            .ok_or(InputMetricsError::MidDrawDropped)?;
    }

    let latency_bounds_ns = FRAME_DURATION_BUCKETS_MS.map(|bound| bound * 1_000_000);
    const EVENT_BOUNDS: [u64; 8] = [1, 2, 4, 8, 16, 32, 64, 128];
    Ok(WindowInputMetrics {
        interval: if baseline.is_some() {
            MeasurementInterval::SinceBaseline
        } else {
            MeasurementInterval::WindowLifetimeCumulative
        },
        gpui_input_to_frame: histogram_summary(
            input_latency.len(),
            (!input_latency.is_empty()).then(|| input_latency.value_at_quantile(0.95)),
            &latency_bounds_ns,
            histogram_bucket_counts(&input_latency, latency_bounds_ns),
            HistogramUnit::Nanoseconds,
        ),
        gpui_input_events_per_frame: histogram_summary(
            events_per_frame.len(),
            (!events_per_frame.is_empty()).then(|| events_per_frame.value_at_quantile(0.95)),
            &EVENT_BOUNDS,
            histogram_bucket_counts(&events_per_frame, EVENT_BOUNDS),
            HistogramUnit::EventsPerFrame,
        ),
        mid_draw_events_dropped,
    })
}

/// 在空直方图时不取分位数，并转成不依赖第三方序列化类型的稳定报告 DTO。
///
/// @author kongweiguang
fn histogram_summary(
    sample_count: u64,
    p95: Option<u64>,
    bounds: &[u64],
    bucket_counts: Vec<u64>,
    unit: HistogramUnit,
) -> HistogramSummary {
    HistogramSummary {
        sample_count,
        p95,
        unit,
        buckets: bounds
            .iter()
            .copied()
            .map(Some)
            .chain(std::iter::once(None))
            .zip(bucket_counts)
            .map(|(upper_bound, sample_count)| HistogramBucket {
                upper_bound,
                sample_count,
            })
            .collect(),
    }
}

/// 从 GPUI 窗口读取一次 profiler 快照，再复用纯快照转换路径。
///
/// 不注册逐帧回调，也不主动制造重绘；调用方应在负载结束后读取。
///
/// @author kongweiguang
pub fn capture_window_frame_metrics(
    window: &Window,
    baseline: Option<&FrameDurationSnapshot>,
    environment: FrameMetricsEnvironment,
) -> Result<WindowFrameMetrics, FrameMetricsError> {
    let current = window.frame_duration_snapshot();
    summarize_frame_metrics(&current, baseline, environment)
}

/// 从真实 GPUI 窗口获取通用输入快照；该指标不关联 SessionId 或 PTY 输出。
///
/// @author kongweiguang
pub fn capture_window_input_metrics(
    window: &Window,
    baseline: Option<&InputLatencySnapshot>,
) -> Result<WindowInputMetrics, InputMetricsError> {
    let current = window.input_latency_snapshot();
    summarize_input_metrics(&current, baseline)
}
