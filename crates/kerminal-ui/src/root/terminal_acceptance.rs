//! 只在隔离验收会话中调度不含普通用户文本的终端回显探针。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    env,
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

use gpui_kit::Context;
use kerminal_application::sessions::{
    SessionFilter, TerminalCommandAvailability, TerminalCommandContext,
    TerminalCommandContextVersion, TerminalCommandUnavailableReason, UiInputProjection,
    UiSessionBinding, UiTerminalInputRequest,
};
use kerminal_domain::{
    identity::SessionId,
    sessions::{SessionOwner, SessionTarget},
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};
use kerminal_terminal::snapshot::RenderSnapshot;

use super::{RootView, SessionCommand, session_queue, workspace_runtime::RuntimePanePhase};
use crate::acceptance_metrics::{
    TERMINAL_ECHO_EXPECTED_SESSIONS_ENV, TERMINAL_ECHO_TEST_CHARACTER,
    TerminalEchoAdmissionRejection, TerminalEchoCleanupStopReason, TerminalEchoProbe,
    TerminalEchoTarget, parse_terminal_echo_expected_sessions, terminal_echo_probe_registry,
};

const METRICS_ENABLE_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS";
const TERMINAL_ECHO_ENABLE_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO";
const TERMINAL_ECHO_FRESH_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO_FRESH";
const CONFIG_ROOT_ENV: &str = "KERMINAL_CONFIG_ROOT";
const MEASUREMENT_DURATION_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_DURATION_MS";
const PROBE_COUNT_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO_COUNT";
const PROBE_INTERVAL_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO_INTERVAL_MS";
const MEASUREMENT_START_POLL_INTERVAL: Duration = Duration::from_millis(100);
const MEASUREMENT_START_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_PROBE_COUNT: u64 = 20;
const MAX_PROBE_COUNT: usize = 100;
const DEFAULT_PROBE_INTERVAL_MS: u64 = 500;
const MIN_PROBE_INTERVAL_MS: u64 = 100;
const MAX_PROBE_INTERVAL_MS: u64 = 10_000;
const MAX_MEASUREMENT_DURATION_MS: u64 = 7_200_000;
const DIAGNOSTICS_DIRECTORY: &str = "target/diagnostics";
const INPUT_ACK_POLL_INTERVAL: Duration = Duration::from_millis(25);

type ProbeLoopKey = (SessionId, u64);

/// 防止同一运行代次重复启动计时循环；进程级状态不进入业务或持久化模型。
///
/// @author kongweiguang
fn active_probe_loops() -> &'static Mutex<HashSet<ProbeLoopKey>> {
    static ACTIVE: OnceLock<Mutex<HashSet<ProbeLoopKey>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

#[derive(Clone, Copy)]
struct ProbeConfig {
    count: usize,
    interval: Duration,
    duration: Duration,
    expected_total: Option<usize>,
}

/// 只有显式开启普通窗口指标、终端 echo、fresh 隔离根，并位于本 worktree diagnostics 下才允许注入。
/// 可选 expectedTotal 门禁只接受 1/10/50；缺省时不增加会话列表读取。
///
/// @author kongweiguang
fn probe_config_from_environment() -> Option<ProbeConfig> {
    if env::var(METRICS_ENABLE_ENV).ok().as_deref() != Some("1")
        || env::var(TERMINAL_ECHO_ENABLE_ENV).ok().as_deref() != Some("1")
        || env::var(TERMINAL_ECHO_FRESH_ENV).ok().as_deref() != Some("1")
        || !config_root_is_fresh_diagnostics_child()
    {
        return None;
    }

    let duration_ms = env::var(MEASUREMENT_DURATION_ENV)
        .ok()?
        .parse::<u64>()
        .ok()?;
    if !(1..=MAX_MEASUREMENT_DURATION_MS).contains(&duration_ms) {
        return None;
    }
    let configured_count = usize::try_from(bounded_env_value(
        PROBE_COUNT_ENV,
        DEFAULT_PROBE_COUNT,
        1,
        MAX_PROBE_COUNT as u64,
    )?)
    .ok()?;
    let interval_ms = bounded_env_value(
        PROBE_INTERVAL_ENV,
        DEFAULT_PROBE_INTERVAL_MS,
        MIN_PROBE_INTERVAL_MS,
        MAX_PROBE_INTERVAL_MS,
    )?;
    let expected_total = match env::var(TERMINAL_ECHO_EXPECTED_SESSIONS_ENV) {
        Ok(value) => parse_terminal_echo_expected_sessions(Some(&value)).ok()?,
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => return None,
    };
    // 桌面采集器通过 registry 标记基线时刻；只允许测量窗口内的采样周期。
    let available_intervals = duration_ms.saturating_sub(1) / interval_ms;
    let count = configured_count.min(usize::try_from(available_intervals).ok()?);
    (count > 0).then_some(ProbeConfig {
        count,
        interval: Duration::from_millis(interval_ms),
        duration: Duration::from_millis(duration_ms),
        expected_total,
    })
}

/// 读取有界数值环境项；错误值关闭 probe，避免悄悄采用意外的采样节奏。
///
/// @author kongweiguang
fn bounded_env_value(name: &str, default: u64, minimum: u64, maximum: u64) -> Option<u64> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value.parse::<u64>().ok()?;
            (minimum..=maximum).contains(&parsed).then_some(parsed)
        }
        Err(env::VarError::NotPresent) => Some(default),
        Err(env::VarError::NotUnicode(_)) => None,
    }
}

/// 将配置根与仓库 diagnostics 目录都解析为真实路径，拒绝相对路径、符号链接逃逸和目录本身。
///
/// @author kongweiguang
fn config_root_is_fresh_diagnostics_child() -> bool {
    let Some(config_root) = env::var_os(CONFIG_ROOT_ENV).map(PathBuf::from) else {
        return false;
    };
    let repository_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let Ok(repository_root) = repository_root.canonicalize() else {
        return false;
    };
    let Ok(diagnostics_root) = repository_root.join(DIAGNOSTICS_DIRECTORY).canonicalize() else {
        return false;
    };
    let Ok(config_root) = config_root.canonicalize() else {
        return false;
    };
    config_root != diagnostics_root && config_root.starts_with(diagnostics_root)
}

impl RootView {
    /// 仅由 fresh local terminal/split 的首次 ready handler 接入点调用；workspace restore 不得调用此入口。
    ///
    /// 每轮提交前都会重验 session binding 和目标。采样时限不超过当前窗口测量区间，普通用户文本不会进入 registry。
    ///
    /// @author kongweiguang
    pub(super) fn maybe_start_terminal_echo_probes(
        &mut self,
        binding: &UiSessionBinding,
        cx: &mut Context<Self>,
    ) {
        terminal_echo_probe_registry().record_hook_invocation();
        let Some(config) = probe_config_from_environment() else {
            if env::var(METRICS_ENABLE_ENV).ok().as_deref() == Some("1") {
                terminal_echo_probe_registry().record_admission_rejection(
                    TerminalEchoAdmissionRejection::ConfigurationRejected,
                );
            }
            return;
        };
        let key = (binding.session_id.clone(), binding.session_generation);
        {
            let mut active = active_probe_loops()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if !active.insert(key.clone()) {
                terminal_echo_probe_registry()
                    .record_admission_rejection(TerminalEchoAdmissionRejection::DuplicateProbeLoop);
                return;
            }
        }

        let binding = binding.clone();
        cx.spawn(async move |this, cx| {
            let registry = terminal_echo_probe_registry();
            let startup_wait_started = Instant::now();
            let measurement_started = loop {
                if let Some(started_at) = registry.measurement_started_at() {
                    break started_at;
                }
                if startup_wait_started.elapsed() >= MEASUREMENT_START_TIMEOUT {
                    registry.record_admission_rejection(
                        TerminalEchoAdmissionRejection::MeasurementStartUnavailable,
                    );
                    active_probe_loops()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key);
                    return;
                }
                cx.background_executor()
                    .timer(MEASUREMENT_START_POLL_INTERVAL)
                    .await;
                if this.update(cx, |_, _| ()).is_err() {
                    active_probe_loops()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&key);
                    return;
                }
            };
            // 只有上一轮已完成精确 writer ACK、画布命中和 tracked Backspace ACK 的 version，
            // 才能证明服务端 dirty 位仍对应已由 UI 清空的那一行；任何输入代次变化都会使凭证失效。
            let mut cleaned_prompt_version: Option<TerminalCommandContextVersion> = None;
            for _ in 0..config.count {
                cx.background_executor().timer(config.interval).await;
                if measurement_started.elapsed() >= config.duration {
                    break;
                }
                let registry = terminal_echo_probe_registry();
                registry.expire_probes(Instant::now());
                let current_target = this.update(cx, |root, _| {
                    root.terminal_echo_binding_is_current(&binding).then(|| {
                        (
                            root.services.sessions.clone(),
                            root.services.executor.clone(),
                        )
                    })
                });
                let (sessions, executor) = match current_target {
                    Ok(Some(target)) => target,
                    Ok(None) => {
                        terminal_echo_probe_registry().record_admission_rejection(
                            TerminalEchoAdmissionRejection::BindingNotCurrent,
                        );
                        continue;
                    }
                    Err(_) => break,
                };
                if let Some(expected_total) = config.expected_total {
                    let session_service = sessions.clone();
                    let listed_sessions = executor
                        .clone()
                        .run(move |_| async move {
                            session_service.list(SessionFilter::default()).await
                        })
                        .await;
                    let actual_total = match listed_sessions {
                        Ok(sessions) => sessions.len(),
                        Err(_) => {
                            terminal_echo_probe_registry().record_admission_rejection(
                                TerminalEchoAdmissionRejection::SessionCountReadFailed,
                            );
                            continue;
                        }
                    };
                    if terminal_echo_session_count_rejection(Some(expected_total), actual_total)
                        .is_some()
                    {
                        terminal_echo_probe_registry().record_admission_rejection(
                            TerminalEchoAdmissionRejection::SessionCountMismatch,
                        );
                        continue;
                    }
                }
                let context_session_id = binding.session_id.clone();
                let context_sessions = sessions.clone();
                let context = match executor
                    .run(move |_| async move {
                        context_sessions
                            .terminal_command_context(context_session_id)
                            .await
                    })
                    .await
                {
                    Ok(context) => context,
                    Err(_) => {
                        terminal_echo_probe_registry().record_admission_rejection(
                            TerminalEchoAdmissionRejection::ContextReadFailed,
                        );
                        break;
                    }
                };
                if cleaned_prompt_version.is_some_and(|version| version != context.version()) {
                    cleaned_prompt_version = None;
                }
                if let Some(rejection) =
                    terminal_echo_context_rejection(&binding, &context, cleaned_prompt_version)
                {
                    terminal_echo_probe_registry().record_admission_rejection(rejection);
                    continue;
                }
                if measurement_started.elapsed() >= config.duration {
                    break;
                }
                // 暂取凭证交给本次准入；只有明确未入队时恢复，入队后的未知结果均结束循环。
                let candidate_clean_prompt_version = cleaned_prompt_version.take();
                let probe = this
                    .update(cx, |root, cx| {
                        root.enqueue_terminal_echo_probe(
                            &binding,
                            &context,
                            candidate_clean_prompt_version,
                            cx,
                        )
                    })
                    .ok()
                    .flatten();
                let Some(probe) = probe else {
                    cleaned_prompt_version = candidate_clean_prompt_version;
                    terminal_echo_probe_registry().record_admission_rejection(
                        TerminalEchoAdmissionRejection::ProbeSubmissionRejected,
                    );
                    continue;
                };
                let initial_context = context.version();
                let probe_deadline =
                    Instant::now() + crate::acceptance_metrics::terminal_echo_probe_timeout();
                let mut writer_context = None;
                let mut stop_reason = None;
                while Instant::now() < probe_deadline {
                    cx.background_executor()
                        .timer(INPUT_ACK_POLL_INTERVAL)
                        .await;
                    registry.record_poll();
                    if writer_context.is_none() {
                        let is_current = this
                            .update(cx, |root, _| {
                                root.terminal_echo_binding_is_current(&binding)
                            })
                            .unwrap_or(false);
                        if !is_current {
                            stop_reason = Some(TerminalEchoCleanupStopReason::BindingChanged);
                            break;
                        }
                        let lookup_session_id = binding.session_id.clone();
                        let context_sessions = sessions.clone();
                        let actual_context = match executor
                            .clone()
                            .run(move |_| async move {
                                context_sessions
                                    .terminal_command_context(lookup_session_id)
                                    .await
                            })
                            .await
                        {
                            Ok(context) => context,
                            Err(_) => {
                                stop_reason = Some(if writer_context.is_some() {
                                    TerminalEchoCleanupStopReason::ContextChanged
                                } else {
                                    TerminalEchoCleanupStopReason::CharacterAckUnavailable
                                });
                                break;
                            }
                        };
                        if !terminal_echo_context_identity_matches(&binding, &actual_context)
                            || actual_context.prompt_epoch != initial_context.prompt_epoch
                        {
                            stop_reason = Some(TerminalEchoCleanupStopReason::ContextChanged);
                            break;
                        }
                        let expected_input_generation = initial_context.input_generation;
                        let Some(acknowledged_generation) =
                            expected_input_generation.checked_add(1)
                        else {
                            stop_reason = Some(TerminalEchoCleanupStopReason::ContextChanged);
                            break;
                        };
                        if actual_context.input_generation == expected_input_generation {
                            continue;
                        }
                        if actual_context.input_generation != acknowledged_generation {
                            stop_reason = Some(TerminalEchoCleanupStopReason::ContextChanged);
                            break;
                        }

                        let ui_state = this
                            .update(cx, |root, _| {
                                terminal_echo_ui_state(
                                    root,
                                    &binding,
                                    TERMINAL_ECHO_TEST_CHARACTER,
                                    1,
                                    actual_context.version(),
                                )
                            })
                            .unwrap_or(TerminalEchoUiState::BindingChanged);
                        match ui_state {
                            TerminalEchoUiState::Ready => {
                                if registry.record_writer_ack(
                                    &probe,
                                    actual_context.version(),
                                    true,
                                    true,
                                    Instant::now(),
                                ) {
                                    writer_context = Some(actual_context);
                                } else {
                                    stop_reason =
                                        Some(TerminalEchoCleanupStopReason::ContextChanged);
                                    break;
                                }
                            }
                            TerminalEchoUiState::QueueBusy => continue,
                            TerminalEchoUiState::BindingChanged => {
                                stop_reason = Some(TerminalEchoCleanupStopReason::BindingChanged);
                                break;
                            }
                            TerminalEchoUiState::ProjectionChanged => {
                                stop_reason =
                                    Some(TerminalEchoCleanupStopReason::ProjectionChanged);
                                break;
                            }
                        }
                    } else if let Some(ack_context) = writer_context.as_ref() {
                        let lookup_session_id = binding.session_id.clone();
                        let context_sessions = sessions.clone();
                        let actual_context = match executor
                            .clone()
                            .run(move |_| async move {
                                context_sessions
                                    .terminal_command_context(lookup_session_id)
                                    .await
                            })
                            .await
                        {
                            Ok(context) => context,
                            Err(_) => {
                                stop_reason = Some(TerminalEchoCleanupStopReason::ContextChanged);
                                break;
                            }
                        };
                        if actual_context.version() != ack_context.version() {
                            stop_reason = Some(TerminalEchoCleanupStopReason::ContextChanged);
                            break;
                        }
                        match this
                            .update(cx, |root, _| {
                                terminal_echo_ui_state(
                                    root,
                                    &binding,
                                    TERMINAL_ECHO_TEST_CHARACTER,
                                    1,
                                    actual_context.version(),
                                )
                            })
                            .unwrap_or(TerminalEchoUiState::BindingChanged)
                        {
                            TerminalEchoUiState::Ready => {}
                            TerminalEchoUiState::QueueBusy => {
                                stop_reason = Some(TerminalEchoCleanupStopReason::QueueNotIdle);
                                break;
                            }
                            TerminalEchoUiState::BindingChanged => {
                                stop_reason = Some(TerminalEchoCleanupStopReason::BindingChanged);
                                break;
                            }
                            TerminalEchoUiState::ProjectionChanged => {
                                stop_reason =
                                    Some(TerminalEchoCleanupStopReason::ProjectionChanged);
                                break;
                            }
                        }
                    }
                    registry.expire_probes(Instant::now());
                    if registry.sample_is_ready(&probe) {
                        break;
                    }
                }

                // Paint 超时仍要把已发送字符清理掉，但未形成有效 Canvas 样本的轮次不能授权下一轮。
                let has_valid_canvas_sample = registry.sample_is_ready(&probe);
                if let Some(reason) = stop_reason {
                    registry.record_cleanup_stop(&probe, reason);
                    break;
                }
                if writer_context.is_none() {
                    registry.record_cleanup_stop(
                        &probe,
                        TerminalEchoCleanupStopReason::CharacterAckUnavailable,
                    );
                    break;
                }

                let Some(ack_context) = writer_context.take() else {
                    registry.record_cleanup_stop(
                        &probe,
                        TerminalEchoCleanupStopReason::CharacterAckUnavailable,
                    );
                    break;
                };
                let lookup_session_id = binding.session_id.clone();
                let context_sessions = sessions.clone();
                let cleanup_context = match executor
                    .clone()
                    .run(move |_| async move {
                        context_sessions
                            .terminal_command_context(lookup_session_id)
                            .await
                    })
                    .await
                {
                    Ok(context) => context,
                    Err(_) => {
                        registry.record_cleanup_stop(
                            &probe,
                            TerminalEchoCleanupStopReason::ContextChanged,
                        );
                        break;
                    }
                };
                if cleanup_context.version() != ack_context.version() {
                    registry
                        .record_cleanup_stop(&probe, TerminalEchoCleanupStopReason::ContextChanged);
                    break;
                }
                let cleanup_request = terminal_echo_input_request(
                    &binding,
                    &cleanup_context,
                    TerminalInputEvent::Key(TerminalKeyEvent {
                        key: TerminalKey::Backspace,
                        phase: KeyPhase::Press,
                        modifiers: InputModifiers::default(),
                    }),
                );
                let cleanup_enqueue_result = this
                    .update(cx, |root, cx| {
                        match terminal_echo_ui_state(
                            root,
                            &binding,
                            TERMINAL_ECHO_TEST_CHARACTER,
                            1,
                            cleanup_context.version(),
                        ) {
                            TerminalEchoUiState::Ready => {
                                if !registry.authorize_cleanup(
                                    &probe,
                                    cleanup_context.version(),
                                    true,
                                    true,
                                ) {
                                    return Err(TerminalEchoCleanupStopReason::ProjectionChanged);
                                }
                                if root.enqueue_session_command(
                                    SessionCommand::UiInput(Box::new(
                                        session_queue::UiInputWork::new(cleanup_request, None),
                                    )),
                                    cx,
                                ) {
                                    Ok(())
                                } else {
                                    Err(TerminalEchoCleanupStopReason::CleanupQueueRejected)
                                }
                            }
                            TerminalEchoUiState::BindingChanged => {
                                Err(TerminalEchoCleanupStopReason::BindingChanged)
                            }
                            TerminalEchoUiState::QueueBusy => {
                                Err(TerminalEchoCleanupStopReason::QueueNotIdle)
                            }
                            TerminalEchoUiState::ProjectionChanged => {
                                Err(TerminalEchoCleanupStopReason::ProjectionChanged)
                            }
                        }
                    })
                    .unwrap_or(Err(TerminalEchoCleanupStopReason::BindingChanged));
                if let Err(reason) = cleanup_enqueue_result {
                    registry.record_cleanup_stop(&probe, reason);
                    break;
                }
                // report 可能刚好已取走测量状态；无论是否仍可记账，都继续等现有 FIFO 的真实 ACK，不重发。
                let cleanup_tracked = registry.record_cleanup_enqueued(&probe);

                let cleanup_deadline =
                    Instant::now() + crate::acceptance_metrics::terminal_echo_probe_timeout();
                let mut cleanup_complete = false;
                let mut cleanup_ack_version = None;
                while Instant::now() < cleanup_deadline {
                    cx.background_executor()
                        .timer(INPUT_ACK_POLL_INTERVAL)
                        .await;
                    registry.record_poll();
                    let is_current = this
                        .update(cx, |root, _| {
                            root.terminal_echo_binding_is_current(&binding)
                        })
                        .unwrap_or(false);
                    if !is_current {
                        registry.record_cleanup_stop(
                            &probe,
                            TerminalEchoCleanupStopReason::BindingChanged,
                        );
                        break;
                    }
                    let lookup_session_id = binding.session_id.clone();
                    let context_sessions = sessions.clone();
                    let actual_context = match executor
                        .clone()
                        .run(move |_| async move {
                            context_sessions
                                .terminal_command_context(lookup_session_id)
                                .await
                        })
                        .await
                    {
                        Ok(context) => context,
                        Err(_) => continue,
                    };
                    if actual_context.session_generation != binding.session_generation
                        || actual_context.prompt_epoch != ack_context.prompt_epoch
                    {
                        registry.record_cleanup_stop(
                            &probe,
                            TerminalEchoCleanupStopReason::ContextChanged,
                        );
                        break;
                    }
                    if actual_context.input_generation == ack_context.input_generation {
                        continue;
                    }
                    if ack_context.input_generation.checked_add(1)
                        != Some(actual_context.input_generation)
                    {
                        registry.record_cleanup_stop(
                            &probe,
                            TerminalEchoCleanupStopReason::ContextChanged,
                        );
                        break;
                    }
                    match this
                        .update(cx, |root, _| {
                            terminal_echo_ui_state(root, &binding, "", 0, actual_context.version())
                        })
                        .unwrap_or(TerminalEchoUiState::BindingChanged)
                    {
                        TerminalEchoUiState::Ready => {
                            let acknowledged_version = actual_context.version();
                            let recorded = registry.record_cleanup_ack(
                                &probe,
                                acknowledged_version,
                                true,
                                true,
                            );
                            cleanup_ack_version = terminal_echo_clean_prompt_version(
                                recorded,
                                has_valid_canvas_sample,
                                acknowledged_version,
                            );
                            cleanup_complete = recorded || !cleanup_tracked;
                            break;
                        }
                        TerminalEchoUiState::QueueBusy => continue,
                        TerminalEchoUiState::BindingChanged => {
                            registry.record_cleanup_stop(
                                &probe,
                                TerminalEchoCleanupStopReason::BindingChanged,
                            );
                            break;
                        }
                        TerminalEchoUiState::ProjectionChanged => {
                            registry.record_cleanup_stop(
                                &probe,
                                TerminalEchoCleanupStopReason::ProjectionChanged,
                            );
                            break;
                        }
                    }
                }
                if !cleanup_complete {
                    // 已在状态分支记下具体失效原因的 probe 已从 registry 移除；否则这是 writer ACK 超时。
                    registry.record_cleanup_stop(
                        &probe,
                        TerminalEchoCleanupStopReason::CleanupAckUnavailable,
                    );
                    break;
                }
                // 此凭证只在上述精确 ACK 与空投影检查都通过后发布；它不重置 service 的 OSC dirty 状态。
                cleaned_prompt_version = cleanup_ack_version;
            }
            active_probe_loops()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .remove(&key);
        })
        .detach();
    }

    /// 每个 probe 重新验证精确 handler、Ready pane 与本地 shell，切换或重连后立即停止旧循环。
    ///
    /// @author kongweiguang
    fn terminal_echo_binding_is_current(&self, binding: &UiSessionBinding) -> bool {
        let SessionOwner::Ui { tab_id, pane_id } = &binding.owner else {
            return false;
        };
        if self.agent_terminal_session_id.as_ref() == Some(&binding.session_id)
            || self.session_command_owner(&binding.session_id).as_ref() != Some(&binding.owner)
            || !self
                .pane_input_handlers
                .get(&binding.session_id)
                .is_some_and(|registration| registration.binding == *binding)
        {
            return false;
        }
        self.workspace_runtime.pane(pane_id).is_some_and(|pane| {
            pane.phase == RuntimePanePhase::Ready
                && pane.tab_id == *tab_id
                && pane.session_id.as_ref() == Some(&binding.session_id)
                && matches!(pane.target.as_ref(), Some(SessionTarget::Local(local))
                    if supported_interactive_shell(&local.program, &local.args))
        })
    }

    /// 先确认空行投影、空闲 FIFO 与光标 cell，再经普通 UI 输入队列发送单个 `x`；不提交 Enter。
    /// 目标 revision 与 context CAS 防止迟到 paint 或并发输入被计入；后续状态机只在精确 ACK 后清理。
    ///
    /// @author kongweiguang
    fn enqueue_terminal_echo_probe(
        &mut self,
        binding: &UiSessionBinding,
        context: &TerminalCommandContext,
        cleaned_prompt_version: Option<TerminalCommandContextVersion>,
        cx: &mut Context<Self>,
    ) -> Option<TerminalEchoProbe> {
        if probe_config_from_environment().is_none() {
            terminal_echo_probe_registry()
                .record_admission_rejection(TerminalEchoAdmissionRejection::ConfigurationRejected);
            return None;
        }
        if !self.terminal_echo_binding_is_current(binding) {
            terminal_echo_probe_registry()
                .record_admission_rejection(TerminalEchoAdmissionRejection::BindingNotCurrent);
            return None;
        }
        if let Some(rejection) =
            terminal_echo_context_rejection(binding, context, cleaned_prompt_version)
        {
            terminal_echo_probe_registry().record_admission_rejection(rejection);
            return None;
        }
        if !self.session_commands.is_idle(&binding.session_id) {
            terminal_echo_probe_registry()
                .record_admission_rejection(TerminalEchoAdmissionRejection::InputQueueBusy);
            return None;
        }
        if !observe_and_confirm_empty_terminal_echo_projection(
            &mut self.command_line_projection,
            binding,
            context,
        ) {
            terminal_echo_probe_registry().record_admission_rejection(
                TerminalEchoAdmissionRejection::EmptyProjectionUnavailable,
            );
            return None;
        }
        let Some(snapshot) = self.snapshot_for_session(&binding.session_id) else {
            terminal_echo_probe_registry()
                .record_admission_rejection(TerminalEchoAdmissionRejection::SnapshotUnavailable);
            return None;
        };
        if snapshot.session_id != binding.session_id {
            terminal_echo_probe_registry()
                .record_admission_rejection(TerminalEchoAdmissionRejection::SessionMismatch);
            return None;
        }
        let Some(target) = terminal_echo_snapshot_target(snapshot) else {
            terminal_echo_probe_registry().record_admission_rejection(
                TerminalEchoAdmissionRejection::SnapshotTargetUnavailable,
            );
            return None;
        };
        let probe = terminal_echo_probe_registry().issue_probe(
            binding.session_id.to_string(),
            binding.session_generation,
            target,
        );
        let submitted_at = Instant::now();
        if !terminal_echo_probe_registry().record_submission(
            &probe,
            binding.session_id.as_str(),
            binding.session_generation,
            context.version(),
            snapshot.revision,
            submitted_at,
        ) {
            terminal_echo_probe_registry().record_admission_rejection(
                TerminalEchoAdmissionRejection::ProbeSubmissionRejected,
            );
            return None;
        }

        let request = terminal_echo_input_request(
            binding,
            context,
            TerminalInputEvent::Text(TERMINAL_ECHO_TEST_CHARACTER.to_owned()),
        );
        let work = session_queue::UiInputWork::new(request, None);
        #[cfg(feature = "acceptance-metrics")]
        let work = {
            let mut work = work;
            work.echo_probe_id = Some(probe.probe_id().to_owned());
            work
        };
        let accepted = self.enqueue_session_command(SessionCommand::UiInput(Box::new(work)), cx);
        if !accepted {
            terminal_echo_probe_registry().record_queue_rejected(&probe);
        } else {
            terminal_echo_probe_registry().record_enqueued(&probe);
        }
        accepted.then_some(probe)
    }
}

/// 组装探针的单字符或 Backspace 输入；两者都沿用 Tracked、精确 context CAS 和普通 UI FIFO。
///
/// @author kongweiguang
fn terminal_echo_input_request(
    binding: &UiSessionBinding,
    context: &TerminalCommandContext,
    event: TerminalInputEvent,
) -> UiTerminalInputRequest {
    UiTerminalInputRequest {
        binding: binding.clone(),
        expected_context: Some(context.version()),
        projection: UiInputProjection::Tracked,
        event,
    }
}

/// 将通过服务端 prompt/binding 门禁的最新 context 同步到短暂输入投影后再判定空行。
/// Handler 安装可能早于 PromptNotReady 到 Available 的同 epoch 转换；复用 projection 自身
/// 的代次/失效规则，避免把未观察的 shell 行猜成空字符串。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
fn observe_and_confirm_empty_terminal_echo_projection(
    projection: &mut super::command_line_projection::CommandLineProjection,
    binding: &UiSessionBinding,
    context: &TerminalCommandContext,
) -> bool {
    projection.observe_context(binding, context);
    projection.confirms_exact_line(binding, "", 0, context.version())
}

/// 只接受底部可见光标对应的真实单列 ASCII 空格 span；空 span 缺失或坐标不一致不能当作空白。
///
/// @author kongweiguang
fn terminal_echo_snapshot_target(snapshot: &RenderSnapshot) -> Option<TerminalEchoTarget> {
    let cursor = snapshot.cursor;
    if snapshot.scrollback_offset != 0
        || !cursor.visible
        || cursor.row >= snapshot.size.rows()
        || cursor.column.saturating_add(1) >= snapshot.size.columns()
    {
        return None;
    }
    let row = snapshot
        .rows
        .iter()
        .find(|row| row.viewport_row == cursor.row)?;
    let cell_end = cursor.column.checked_add(1)?;
    let mut cursor_span = None;
    for run in row.runs.iter() {
        for span in run
            .cell_spans
            .iter()
            .filter(|span| span.column_range.contains(&cursor.column))
        {
            if cursor_span.is_some()
                || span.column_range.start != cursor.column
                || span.column_range.end != cell_end
                || span.column_range.start < run.start_column
                || span.column_range.end > run.end_column
            {
                return None;
            }
            let glyph = run.text.get(span.byte_range.clone())?;
            if glyph.as_bytes() != b" " {
                return None;
            }
            cursor_span = Some(span);
        }
    }
    cursor_span.map(|_| TerminalEchoTarget {
        viewport_row: cursor.row,
        column: cursor.column,
    })
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TerminalEchoUiState {
    Ready,
    BindingChanged,
    QueueBusy,
    ProjectionChanged,
}

/// 给 ACK 与 Backspace 共用同一 UI 侧判定：必须是当前 handler、空闲 FIFO 和精确投影。
///
/// @author kongweiguang
fn terminal_echo_ui_state(
    root: &RootView,
    binding: &UiSessionBinding,
    expected_text: &str,
    expected_cursor: usize,
    expected_context: TerminalCommandContextVersion,
) -> TerminalEchoUiState {
    if !root.terminal_echo_binding_is_current(binding) {
        return TerminalEchoUiState::BindingChanged;
    }
    if !root.session_commands.is_idle(&binding.session_id) {
        return TerminalEchoUiState::QueueBusy;
    }
    if !root.command_line_projection.confirms_exact_line(
        binding,
        expected_text,
        expected_cursor,
        expected_context,
    ) {
        return TerminalEchoUiState::ProjectionChanged;
    }
    TerminalEchoUiState::Ready
}

/// 版本轮询不放松 owner/session/generation fencing；仅将完全相同的绑定归入一个 ACK 生命周期。
///
/// @author kongweiguang
fn terminal_echo_context_identity_matches(
    binding: &UiSessionBinding,
    context: &TerminalCommandContext,
) -> bool {
    context.session_id == binding.session_id
        && context.owner == binding.owner
        && context.session_generation == binding.session_generation
}

/// 只有配置要求的真实会话总数已齐备时才允许读取 context 并排队字符探针。
///
/// @author kongweiguang
fn terminal_echo_session_count_rejection(
    expected_total: Option<usize>,
    actual_total: usize,
) -> Option<TerminalEchoAdmissionRejection> {
    expected_total
        .filter(|expected| *expected != actual_total)
        .map(|_| TerminalEchoAdmissionRejection::SessionCountMismatch)
}

/// 只有服务端确认的空提示符，或与精确清理 ACK 完全同代的空投影凭证，才允许 probe；其它门禁不放宽。
///
/// @author kongweiguang
fn terminal_echo_context_rejection(
    binding: &UiSessionBinding,
    context: &TerminalCommandContext,
    cleaned_prompt_version: Option<TerminalCommandContextVersion>,
) -> Option<TerminalEchoAdmissionRejection> {
    if context.session_id != binding.session_id {
        return Some(TerminalEchoAdmissionRejection::SessionMismatch);
    }
    if context.owner != binding.owner {
        return Some(TerminalEchoAdmissionRejection::OwnerMismatch);
    }
    if context.session_generation != binding.session_generation {
        return Some(TerminalEchoAdmissionRejection::GenerationMismatch);
    }
    if let TerminalCommandAvailability::Unavailable(reason) = context.availability {
        return Some(match reason {
            TerminalCommandUnavailableReason::NonUiOwner => {
                TerminalEchoAdmissionRejection::NonUiOwner
            }
            TerminalCommandUnavailableReason::SessionNotRunning => {
                TerminalEchoAdmissionRejection::SessionNotRunning
            }
            TerminalCommandUnavailableReason::ShellIntegrationDisabled => {
                TerminalEchoAdmissionRejection::ShellIntegrationDisabled
            }
            TerminalCommandUnavailableReason::ShellIntegrationNotObserved => {
                TerminalEchoAdmissionRejection::ShellIntegrationNotObserved
            }
            TerminalCommandUnavailableReason::PromptNotReady => {
                TerminalEchoAdmissionRejection::PromptNotReady
            }
            TerminalCommandUnavailableReason::AlternateScreen => {
                TerminalEchoAdmissionRejection::AlternateScreen
            }
            TerminalCommandUnavailableReason::UnsupportedShellDialect => {
                TerminalEchoAdmissionRejection::UnsupportedShellDialect
            }
            TerminalCommandUnavailableReason::ExternalInputObserved => {
                TerminalEchoAdmissionRejection::ExternalInputObserved
            }
            TerminalCommandUnavailableReason::InputWritePending => {
                TerminalEchoAdmissionRejection::InputWritePending
            }
            TerminalCommandUnavailableReason::UiInputProjectionInvalidated => {
                TerminalEchoAdmissionRejection::UiInputProjectionInvalidated
            }
            TerminalCommandUnavailableReason::ManagedCommandActive => {
                TerminalEchoAdmissionRejection::ManagedCommandActive
            }
        });
    }
    if !context.prompt_ready {
        return Some(TerminalEchoAdmissionRejection::PromptNotReady);
    }
    if !context.prompt_line_empty && cleaned_prompt_version != Some(context.version()) {
        return Some(TerminalEchoAdmissionRejection::PromptLineNotEmpty);
    }
    if context.alternate_screen {
        return Some(TerminalEchoAdmissionRejection::AlternateScreen);
    }
    (context.availability != TerminalCommandAvailability::Available)
        .then_some(TerminalEchoAdmissionRejection::ContextUnavailable)
}

/// 仅有效 Canvas 样本和 registry 精确清理 ACK 同时成立时才发放下一轮可复用的行空凭证。
/// 普通清理回退仍完成安全恢复，但不能把未测量或未知结果升级成下一轮准入证据。
///
/// @author kongweiguang
fn terminal_echo_clean_prompt_version(
    cleanup_ack_recorded: bool,
    canvas_sample_matched: bool,
    acknowledged_version: TerminalCommandContextVersion,
) -> Option<TerminalCommandContextVersion> {
    (cleanup_ack_recorded && canvas_sample_matched).then_some(acknowledged_version)
}

/// 仅允许纯交互参数的已知 shell，避免对用户选择的任意本地进程写入测试命令。
///
/// @author kongweiguang
fn supported_interactive_shell(program: &OsStr, args: &[OsString]) -> bool {
    let Some(shell_name) = Path::new(program)
        .file_stem()
        .and_then(OsStr::to_str)
        .map(str::to_ascii_lowercase)
    else {
        return false;
    };
    match shell_name.as_str() {
        "pwsh" | "powershell" => powershell_arguments_are_interactive(args),
        "bash" | "sh" | "zsh" | "fish" => posix_shell_arguments_are_interactive(&shell_name, args),
        "cmd" => cmd_arguments_are_interactive(args),
        _ => false,
    }
}

/// 允许 PowerShell 的交互安全选项，拒绝命令、脚本和编码命令参数，避免把探针送入程序 stdin。
///
/// @author kongweiguang
fn powershell_arguments_are_interactive(args: &[OsString]) -> bool {
    args.iter().all(|argument| {
        argument.to_str().is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "-nologo" | "-noexit" | "-noprofile" | "-noprofileloadtime"
            )
        })
    })
}

/// 仅允许 POSIX shell 的交互/login flags；命令、profile 覆盖和位置参数均 fail closed。
///
/// @author kongweiguang
fn posix_shell_arguments_are_interactive(shell: &str, args: &[OsString]) -> bool {
    args.iter().all(|argument| {
        let Some(value) = argument.to_str() else {
            return false;
        };
        let option = value.to_ascii_lowercase();
        match shell {
            "bash" | "sh" => matches!(option.as_str(), "-i" | "-l" | "--login"),
            "zsh" => matches!(option.as_str(), "-i" | "-l" | "-il" | "-li"),
            "fish" => matches!(option.as_str(), "-i" | "--interactive" | "-l" | "--login"),
            _ => false,
        }
    })
}

/// CMD 只接收不会指定脚本或一次性命令的 console flags。
///
/// @author kongweiguang
fn cmd_arguments_are_interactive(args: &[OsString]) -> bool {
    args.iter().all(|argument| {
        argument.to_str().is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "/d" | "/q" | "/a" | "/u"
            )
        })
    })
}

#[cfg(test)]
#[path = "../../tests/unit/root_terminal_acceptance.rs"]
mod tests;
