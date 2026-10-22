#![cfg(feature = "acceptance-metrics")]
//! 显式启用的原生 GPUI 帧采样器；默认桌面启动不会建立计时任务。
//!
//! @author kongweiguang

use std::{
    env, fmt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gpui_kit::{App, Window};
use kerminal_application::{AppError, AppResult, executor::ApplicationTaskExecutor};
use kerminal_ui::acceptance_metrics::{
    FrameMetricsEnvironment, RendererKind, TERMINAL_ECHO_EXPECTED_SESSIONS_ENV,
    TerminalEchoProbeDiagnostics, TerminalEchoProbeMeasurement, TerminalEchoSample,
    TerminalWheelDiagnostics, WindowFrameMetrics, WindowInputMetrics,
    parse_terminal_echo_expected_sessions, summarize_frame_metrics, summarize_input_metrics,
    terminal_echo_probe_registry,
};
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;

const ENABLE_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS";
const WINDOW_TITLE_ENV: &str = "KERMINAL_ACCEPTANCE_WINDOW_TITLE";
const MAX_DIAGNOSTIC_WINDOW_TITLE_CHARS: usize = 120;
const DURATION_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_DURATION_MS";
const OUTPUT_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_OUTPUT";
const ENVIRONMENT_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_ENVIRONMENT";
const WORKLOAD_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_WORKLOAD";
const TERMINAL_ECHO_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_TERMINAL_ECHO";
const TERMINAL_ECHO_PROBE_SEMANTICS_VERSION: u32 = 2;
const WARMUP_DURATION_ENV: &str = "KERMINAL_ACCEPTANCE_METRICS_WARMUP_MS";
const DEFAULT_WARMUP_DURATION_MS: u64 = 5_000;
const MAX_WARMUP_DURATION_MS: u64 = 300_000;
const MAX_MEASUREMENT_DURATION_MS: u64 = 7_200_000;
const FRAME_PHASE_JOURNAL_DRAIN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_FRAME_PHASE_SAMPLES_PER_PHASE: usize = 10_000;

static TEMPORARY_FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 只在显式验收采样开启时读取隔离窗口标题，避免普通桌面启动受环境变量影响。
///
/// 非 UTF-8、空白、过长或含控制字符的标题均回退为 `None`，由窗口使用产品默认标题。
///
/// @author kongweiguang
pub(crate) fn diagnostic_window_title() -> Option<String> {
    if env::var(ENABLE_ENV).ok().as_deref() != Some("1") {
        return None;
    }

    let title = env::var(WINDOW_TITLE_ENV).ok()?;
    is_valid_diagnostic_window_title(&title).then_some(title)
}

/// 限制诊断标题的显示长度与内容，避免换行、终端控制字符或空白值污染原生窗口标题。
///
/// @author kongweiguang
fn is_valid_diagnostic_window_title(title: &str) -> bool {
    !title.trim().is_empty()
        && title.chars().count() <= MAX_DIAGNOSTIC_WINDOW_TITLE_CHARS
        && !title.chars().any(char::is_control)
}

/// 表示显式验收变量是否创建了采样任务。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InstallDisposition {
    Disabled,
    Scheduled,
}

/// 采样启用配置不完整或值无效时返回的可检索错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AcceptanceMetricsConfigError {
    InvalidEnableValue,
    MissingDuration,
    InvalidDuration,
    DurationOutOfRange,
    InvalidWarmupDuration,
    WarmupDurationOutOfRange,
    MissingOutput,
    OutputMustBeAbsoluteFile,
    InvalidTextEnvironmentValue,
    InvalidLabel,
    InvalidExpectedSessionCount,
}

impl fmt::Display for AcceptanceMetricsConfigError {
    /// 配置错误只报告稳定原因码，不把环境变量内容或输出路径写进日志。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidEnableValue => "启用标记必须为 1、0 或 false",
            Self::MissingDuration => "启用采样时必须设置有限测量时长",
            Self::InvalidDuration => "测量时长必须是毫秒整数",
            Self::DurationOutOfRange => "测量时长必须在 1 到 7200000 毫秒之间",
            Self::InvalidWarmupDuration => "预热时长必须是毫秒整数",
            Self::WarmupDurationOutOfRange => "预热时长必须在 1 到 300000 毫秒之间",
            Self::MissingOutput => "启用采样时必须设置报告输出文件",
            Self::OutputMustBeAbsoluteFile => "报告输出必须是绝对文件路径",
            Self::InvalidTextEnvironmentValue => "文本配置环境变量不是有效 UTF-8",
            Self::InvalidLabel => "环境或负载标签为空、过长或包含控制字符",
            Self::InvalidExpectedSessionCount => {
                "终端 echo expected session 数量必须为 1、10 或 50"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for AcceptanceMetricsConfigError {}

/// 仅在真实桌面窗口安装一次定时采样；配置缺失时不启动任务或修改应用行为。
///
/// 窗口快照在 GPUI 回调内读取，有界预热后以固定时长差分；仅序列化和文件操作进入
/// 应用共享执行器，防止测量写盘阻塞窗口线程。起止窗口元数据只描述两个采样边界，
/// 不据此推断整个测量期间稳定或物理显示器刷新率。独立 journal collector 在预热边界取游标，
/// 只把其有界测量区间分段摘要写入报告；窗口输入与终端 PTY 回显仍保留各自的测量口径。
///
/// @author kongweiguang
pub(crate) fn install(
    window: &Window,
    cx: &App,
    executor: ApplicationTaskExecutor,
) -> Result<InstallDisposition, AcceptanceMetricsConfigError> {
    let Some(config) = AcceptanceMetricsConfig::from_environment()? else {
        return Ok(InstallDisposition::Disabled);
    };

    window
        .spawn(cx, async move |window_cx| {
            window_cx
                .background_executor()
                .timer(config.warmup_duration)
                .await;
            let baseline = match window_cx.update(|window, cx| {
                (
                    window.frame_duration_snapshot(),
                    window.input_latency_snapshot(),
                    WindowMetricsSnapshot::capture(window, cx),
                    window.window_handle().window_id(),
                    // collector 在预热边界取游标，只读取本次测量之后的有界 journal 项。
                    cx.foreground_journal().collector(),
                )
            }) {
                Ok(baseline) => baseline,
                Err(error) => {
                    tracing::error!(
                        event = "NATIVE_WINDOW_METRICS_BASELINE_FAILED",
                        error = %error,
                    );
                    return;
                }
            };

            // 丢弃启动预热期结果；测量区间和 Canvas probe 样本从同一个窗口边界开始。
            terminal_echo_probe_registry().reset();

            let window_id = baseline.3;
            let mut journal_collector = baseline.4;
            let mut frame_phase_accumulator = FramePhaseAccumulator::default();
            let measurement_started_at = Instant::now();
            let measurement_deadline = measurement_started_at + config.duration;
            loop {
                let remaining = measurement_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let drain_after = remaining.min(FRAME_PHASE_JOURNAL_DRAIN_INTERVAL);
                window_cx.background_executor().timer(drain_after).await;
                if let Err(error) = window_cx.update(|_, _| {
                    // 每秒在窗口边界取一段并立即合并，避免末尾一次性 drain 超过环容量。
                    frame_phase_accumulator
                        .merge_drained(journal_collector.collect_unseen(), window_id);
                }) {
                    tracing::error!(
                        event = "NATIVE_WINDOW_METRICS_JOURNAL_DRAIN_FAILED",
                        error = %error,
                    );
                    return;
                }
            }

            let current = match window_cx.update(|window, cx| {
                // 最后一段与最终窗口快照在同一回调中收取，报告计数覆盖完整测量窗。
                frame_phase_accumulator
                    .merge_drained(journal_collector.collect_unseen(), window_id);
                (
                    window.frame_duration_snapshot(),
                    window.input_latency_snapshot(),
                    WindowMetricsSnapshot::capture(window, cx),
                    terminal_echo_probe_registry().take_measurement(),
                )
            }) {
                Ok(current) => current,
                Err(error) => {
                    tracing::error!(
                        event = "NATIVE_WINDOW_METRICS_FINAL_SNAPSHOT_FAILED",
                        error = %error,
                    );
                    return;
                }
            };

            let environment = FrameMetricsEnvironment::new(
                config.environment_id,
                config.workload,
                RendererKind::Unknown,
            );
            let frame = match capture_window_frame_metrics_from_snapshots(
                &current.0,
                &baseline.0,
                environment,
            ) {
                Ok(frame) => frame,
                Err(error) => {
                    tracing::error!(
                        event = "NATIVE_WINDOW_METRICS_FRAME_DIFFERENCE_FAILED",
                        error = %error,
                    );
                    return;
                }
            };
            let input = match summarize_window_input(&current.1, &baseline.1) {
                Ok(input) => input,
                Err(error) => {
                    tracing::error!(
                        event = "NATIVE_WINDOW_METRICS_INPUT_DIFFERENCE_FAILED",
                        error = %error,
                    );
                    return;
                }
            };
            let frame_phases = frame_phase_accumulator.finish();

            let report = NativeWindowMetricsReport {
                schema_version: 4,
                status: CaptureStatus::from_frame_samples(frame.gpui_dirty_to_present.sample_count),
                warmup_duration_ms: config.warmup_duration.as_millis() as u64,
                target_measurement_duration_ms: config.duration.as_millis() as u64,
                actual_measurement_duration_ms: u64::try_from(
                    measurement_started_at.elapsed().as_millis(),
                )
                .unwrap_or(u64::MAX),
                window_metadata: WindowMetricsBoundary {
                    start: baseline.2,
                    end: current.2,
                },
                frame,
                frame_phases,
                gpui_input: input,
                terminal_wheel: current.3.terminal_wheel,
                terminal_echo: summarize_terminal_echo_measurement(
                    config.terminal_echo_enabled,
                    config.terminal_echo_expected_total,
                    current.3,
                ),
            };
            let output_path = config.output_path;
            let result = executor
                .run(move |cancellation| async move {
                    persist_report(output_path, report, cancellation).await
                })
                .await;

            if let Err(error) = result {
                tracing::error!(
                    event = "NATIVE_WINDOW_METRICS_REPORT_WRITE_FAILED",
                    code = error.public_code().as_str(),
                    message = error.public_message(),
                );
            }
        })
        .detach();

    Ok(InstallDisposition::Scheduled)
}

/// 只在启用标记准确时读取验收配置；其他环境变量不会意外启动采样，warmup 缺省为五秒。
///
/// @author kongweiguang
fn parse_config(
    enabled: Option<&str>,
    duration_ms: Option<&str>,
    output_path: Option<&Path>,
    environment_id: Option<&str>,
    workload: Option<&str>,
) -> Result<Option<AcceptanceMetricsConfig>, AcceptanceMetricsConfigError> {
    match enabled {
        None | Some("0") | Some("false") => return Ok(None),
        Some("1") => {}
        Some(_) => return Err(AcceptanceMetricsConfigError::InvalidEnableValue),
    }

    let duration_ms = duration_ms.ok_or(AcceptanceMetricsConfigError::MissingDuration)?;
    let duration_ms = duration_ms
        .parse::<u64>()
        .map_err(|_| AcceptanceMetricsConfigError::InvalidDuration)?;
    if duration_ms == 0 || duration_ms > MAX_MEASUREMENT_DURATION_MS {
        return Err(AcceptanceMetricsConfigError::DurationOutOfRange);
    }

    let output_path = output_path.ok_or(AcceptanceMetricsConfigError::MissingOutput)?;
    if !output_path.is_absolute() || output_path.file_name().is_none() {
        return Err(AcceptanceMetricsConfigError::OutputMustBeAbsoluteFile);
    }

    Ok(Some(AcceptanceMetricsConfig {
        duration: Duration::from_millis(duration_ms),
        warmup_duration: Duration::from_millis(DEFAULT_WARMUP_DURATION_MS),
        output_path: output_path.to_path_buf(),
        environment_id: parse_label(environment_id, "unknown")?,
        workload: parse_label(workload, "manual")?,
        terminal_echo_enabled: false,
        terminal_echo_expected_total: None,
    }))
}

/// 解析可选 warmup 毫秒值；缺省保留五秒，越界或非整数值会拒绝采样启动。
///
/// @author kongweiguang
fn parse_warmup_duration(value: Option<&str>) -> Result<Duration, AcceptanceMetricsConfigError> {
    let duration_ms = match value {
        None => DEFAULT_WARMUP_DURATION_MS,
        Some(value) => value
            .parse::<u64>()
            .map_err(|_| AcceptanceMetricsConfigError::InvalidWarmupDuration)?,
    };
    if !(1..=MAX_WARMUP_DURATION_MS).contains(&duration_ms) {
        return Err(AcceptanceMetricsConfigError::WarmupDurationOutOfRange);
    }
    Ok(Duration::from_millis(duration_ms))
}

/// 从环境变量构造配置并拒绝无法解码的标签，避免报告被静默改写。
///
/// @author kongweiguang
fn parse_label(value: Option<&str>, default: &str) -> Result<String, AcceptanceMetricsConfigError> {
    let value = value.unwrap_or(default);
    if value.trim().is_empty() || value.chars().count() > 256 || value.chars().any(char::is_control)
    {
        return Err(AcceptanceMetricsConfigError::InvalidLabel);
    }
    Ok(value.to_owned())
}

/// 读取可选 UTF-8 环境值；原始值不会写到错误日志。
///
/// @author kongweiguang
fn read_text_environment(name: &str) -> Result<Option<String>, AcceptanceMetricsConfigError> {
    env::var(name).map(Some).or_else(|error| match error {
        env::VarError::NotPresent => Ok(None),
        env::VarError::NotUnicode(_) => {
            Err(AcceptanceMetricsConfigError::InvalidTextEnvironmentValue)
        }
    })
}

/// 仅将环境中配置的绝对路径用于报告输出，不推断或写入用户配置目录。
///
/// @author kongweiguang
fn output_path_from_environment() -> Option<PathBuf> {
    env::var_os(OUTPUT_ENV).map(PathBuf::from)
}

/// 只有准确的 `1` 才允许探针运行；默认关闭并拒绝模糊布尔值。
///
/// @author kongweiguang
fn parse_terminal_echo_opt_in(value: Option<&str>) -> Result<bool, AcceptanceMetricsConfigError> {
    match value {
        None | Some("0") | Some("false") => Ok(false),
        Some("1") => Ok(true),
        Some(_) => Err(AcceptanceMetricsConfigError::InvalidEnableValue),
    }
}

/// 汇总同一窗口的基线和终值；本地 UI 输入是 GPUI 输入到帧指标，不代表 PTY echo。
///
/// @author kongweiguang
fn summarize_window_input(
    current: &gpui_kit::InputLatencySnapshot,
    baseline: &gpui_kit::InputLatencySnapshot,
) -> Result<WindowInputMetrics, kerminal_ui::acceptance_metrics::InputMetricsError> {
    summarize_input_metrics(current, Some(baseline))
}

/// 从同一个 GPUI Window 的 profiler 快照差分帧数据，保留快照来源和口径。
///
/// @author kongweiguang
fn capture_window_frame_metrics_from_snapshots(
    current: &gpui_kit::FrameDurationSnapshot,
    baseline: &gpui_kit::FrameDurationSnapshot,
    environment: FrameMetricsEnvironment,
) -> Result<WindowFrameMetrics, kerminal_ui::acceptance_metrics::FrameMetricsError> {
    summarize_frame_metrics(current, Some(baseline), environment)
}

/// 本次原生窗口性能采样的状态，不表示通过性能阈值。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum CaptureStatus {
    FrameSamplesCollected,
    InsufficientFrameSamples,
}

impl CaptureStatus {
    /// 仅按真实差分样本数标记数据可用性，不把“已采集”混同为通过性能阈值。
    ///
    /// @author kongweiguang
    fn from_frame_samples(sample_count: u64) -> Self {
        if sample_count == 0 {
            Self::InsufficientFrameSamples
        } else {
            Self::FrameSamplesCollected
        }
    }
}

/// 可持久化的原生窗口报告，明确保留通用 GPUI input 与帧数据的独立字段。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct NativeWindowMetricsReport {
    schema_version: u32,
    status: CaptureStatus,
    warmup_duration_ms: u64,
    target_measurement_duration_ms: u64,
    actual_measurement_duration_ms: u64,
    window_metadata: WindowMetricsBoundary,
    frame: WindowFrameMetrics,
    frame_phases: FramePhaseDiagnostics,
    gpui_input: WindowInputMetrics,
    terminal_wheel: TerminalWheelDiagnostics,
    terminal_echo: TerminalEchoMetricsReport,
}

/// 汇总同一窗口的有界 GPUI journal 分段；它不把 callback 时间或刷新元数据当作真实 VSync。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FramePhaseDiagnostics {
    journal_entries_seen: u64,
    journal_entries_lost: u64,
    presented_frame_count: u64,
    gpui_callback_count: u64,
    callbacks_with_signal_timestamp: u64,
    reentrant_frame_skips: u64,
    actual_dwm_vsync_interarrival_measured: bool,
    first_invalidation_to_frame_signal: TimingDistribution,
    frame_signal_to_gpui_callback: TimingDistribution,
    frame_signal_to_draw_start: TimingDistribution,
    gpui_draw: TimingDistribution,
    draw_end_to_platform_present_start: TimingDistribution,
    platform_present_submission: TimingDistribution,
    first_invalidation_to_platform_present_end: TimingDistribution,
}

/// 样本总数、保留数和丢弃数分开报告，分位数只基于保留样本。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TimingDistribution {
    sample_count: u64,
    retained_sample_count: usize,
    sample_dropped: u64,
    p50_ns: Option<u64>,
    p95_ns: Option<u64>,
    max_ns: Option<u64>,
}

/// 跨多个 journal drain 有界合并同一窗口的相位样本和生命周期计数。
#[derive(Default)]
struct FramePhaseAccumulator {
    journal_entries_seen: u64,
    journal_entries_lost: u64,
    presented_frame_count: u64,
    gpui_callback_count: u64,
    callbacks_with_signal_timestamp: u64,
    reentrant_frame_skips: u64,
    samples: FramePhaseDurationSamples,
}

impl FramePhaseAccumulator {
    /// 遍历本次 drain 后立即丢弃原始 entries；相位数据仅保留每类前一万个样本。
    ///
    /// @author kongweiguang
    fn merge_drained(
        &mut self,
        drained: gpui_kit::profiler::journal::DrainedEntries,
        window_id: gpui_kit::WindowId,
    ) {
        use gpui_kit::profiler::journal::{ForegroundJournalEntry, IntervalBoundary};

        self.journal_entries_seen = self
            .journal_entries_seen
            .saturating_add(u64::try_from(drained.entries.len()).unwrap_or(u64::MAX));
        self.journal_entries_lost = self.journal_entries_lost.saturating_add(drained.lost);

        for entry in drained.entries {
            match entry {
                ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(presented))
                    if presented.frame.window_id == window_id =>
                {
                    self.presented_frame_count = self.presented_frame_count.saturating_add(1);
                    if let (Some(dirty_at), Some(signal_at)) =
                        (presented.frame.dirty_at, presented.frame.signal_at)
                    {
                        push_duration_sample(
                            &mut self.samples.first_invalidation_to_frame_signal,
                            dirty_at,
                            signal_at,
                        );
                    }
                    if let Some(signal_at) = presented.frame.signal_at {
                        push_duration_sample(
                            &mut self.samples.frame_signal_to_draw_start,
                            signal_at,
                            presented.frame.draw_start,
                        );
                    }
                    push_duration_sample(
                        &mut self.samples.gpui_draw,
                        presented.frame.draw_start,
                        presented.frame.draw_end,
                    );
                    push_duration_sample(
                        &mut self.samples.draw_end_to_platform_present_start,
                        presented.frame.draw_end,
                        presented.presentation.present_start,
                    );
                    push_duration_sample(
                        &mut self.samples.platform_present_submission,
                        presented.presentation.present_start,
                        presented.presentation.present_end,
                    );
                    if let Some(dirty_at) = presented.frame.dirty_at {
                        push_duration_sample(
                            &mut self.samples.first_invalidation_to_platform_present_end,
                            dirty_at,
                            presented.presentation.present_end,
                        );
                    }
                }
                ForegroundJournalEntry::PlatformSignal(signal) if signal.window_id == window_id => {
                    self.gpui_callback_count = self.gpui_callback_count.saturating_add(1);
                    if let Some(signal_at) = signal.signal_at {
                        self.callbacks_with_signal_timestamp =
                            self.callbacks_with_signal_timestamp.saturating_add(1);
                        push_duration_sample(
                            &mut self.samples.frame_signal_to_gpui_callback,
                            signal_at,
                            signal.handled_at,
                        );
                    }
                }
                ForegroundJournalEntry::ReentrantFrameSkipped {
                    window_id: skipped_window,
                    ..
                } if skipped_window == window_id => {
                    self.reentrant_frame_skips = self.reentrant_frame_skips.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    /// 结束累计并对每相位保留的确定性前缀计算分位数，不把分段 p95 相互平均。
    ///
    /// @author kongweiguang
    fn finish(self) -> FramePhaseDiagnostics {
        FramePhaseDiagnostics {
            journal_entries_seen: self.journal_entries_seen,
            journal_entries_lost: self.journal_entries_lost,
            presented_frame_count: self.presented_frame_count,
            gpui_callback_count: self.gpui_callback_count,
            callbacks_with_signal_timestamp: self.callbacks_with_signal_timestamp,
            reentrant_frame_skips: self.reentrant_frame_skips,
            actual_dwm_vsync_interarrival_measured: false,
            first_invalidation_to_frame_signal: summarize_timing_distribution(
                self.samples.first_invalidation_to_frame_signal,
            ),
            frame_signal_to_gpui_callback: summarize_timing_distribution(
                self.samples.frame_signal_to_gpui_callback,
            ),
            frame_signal_to_draw_start: summarize_timing_distribution(
                self.samples.frame_signal_to_draw_start,
            ),
            gpui_draw: summarize_timing_distribution(self.samples.gpui_draw),
            draw_end_to_platform_present_start: summarize_timing_distribution(
                self.samples.draw_end_to_platform_present_start,
            ),
            platform_present_submission: summarize_timing_distribution(
                self.samples.platform_present_submission,
            ),
            first_invalidation_to_platform_present_end: summarize_timing_distribution(
                self.samples.first_invalidation_to_platform_present_end,
            ),
        }
    }
}

/// `handled_at` 是 GPUI callback 时刻；现有 journal 未记录真实 DWM VSync 到达采样。
#[derive(Default)]
struct FramePhaseDurationSamples {
    first_invalidation_to_frame_signal: BoundedTimingSamples,
    frame_signal_to_gpui_callback: BoundedTimingSamples,
    frame_signal_to_draw_start: BoundedTimingSamples,
    gpui_draw: BoundedTimingSamples,
    draw_end_to_platform_present_start: BoundedTimingSamples,
    platform_present_submission: BoundedTimingSamples,
    first_invalidation_to_platform_present_end: BoundedTimingSamples,
}

#[derive(Default)]
struct BoundedTimingSamples {
    sample_count: u64,
    sample_dropped: u64,
    retained_ns: Vec<u64>,
}

impl BoundedTimingSamples {
    /// 计入所有有效观测，但只保留最先出现的固定数量以维持确定、可解释的分位数输入。
    ///
    /// @author kongweiguang
    fn push(&mut self, nanos: u64) {
        self.sample_count = self.sample_count.saturating_add(1);
        if self.retained_ns.len() < MAX_FRAME_PHASE_SAMPLES_PER_PHASE {
            self.retained_ns.push(nanos);
        } else {
            self.sample_dropped = self.sample_dropped.saturating_add(1);
        }
    }
}

/// 忽略倒序或无法放入报告整数范围的时间戳，并将有效值交给有界样本容器。
///
/// @author kongweiguang
fn push_duration_sample(samples: &mut BoundedTimingSamples, start: Instant, end: Instant) {
    if let Some(nanos) = end
        .checked_duration_since(start)
        .and_then(|duration| u64::try_from(duration.as_nanos()).ok())
    {
        samples.push(nanos);
    }
}

/// 用 nearest-rank 汇总固定前缀；sampleDropped 非零时分位数只代表保留子集。
///
/// @author kongweiguang
fn summarize_timing_distribution(mut samples: BoundedTimingSamples) -> TimingDistribution {
    samples.retained_ns.sort_unstable();
    let percentile = |percent: usize| {
        samples
            .retained_ns
            .len()
            .checked_mul(percent)
            .and_then(|rank| rank.checked_add(99))
            .map(|rank| rank / 100)
            .and_then(|rank| rank.checked_sub(1))
            .and_then(|index| samples.retained_ns.get(index).copied())
    };
    TimingDistribution {
        sample_count: samples.sample_count,
        retained_sample_count: samples.retained_ns.len(),
        sample_dropped: samples.sample_dropped,
        p50_ns: percentile(50),
        p95_ns: percentile(95),
        max_ns: samples.retained_ns.last().copied(),
    }
}
/// 区分 opt-in 未启用、没有对应绘制和已采到真实 Canvas 提交样本。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalEchoCaptureStatus {
    Disabled,
    InsufficientSamples,
    SamplesCollected,
}

/// 显式标注 v2 探针由单个 ASCII `x` 触发，防止旧 shell-output marker 被解释为字符输入样本。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalEchoSampleKind {
    SingleAsciiCharacterUiFifoToCanvasTextPaint,
}

/// 回显延迟终点是 GPUI Canvas 的文字绘制提交，不代表 OS present 或屏幕扫描输出。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TerminalEchoMetricsReport {
    enabled: bool,
    /// 从配置复制的 workload gate，使报告能自描述 probe 的会话数量准入条件。
    expected_total: Option<usize>,
    status: TerminalEchoCaptureStatus,
    sample_count: usize,
    /// 单独版本化探针语义，避免桌面报告 schema 变更与采样行为混为一谈。
    probe_semantics_version: u32,
    /// 固定样本种类；v2 只代表单字符 UI 输入，不接受旧 shell-output marker。
    sample_kind: TerminalEchoSampleKind,
    /// 固定说明耗时从 Root 跟踪的字符 FIFO 入队开始到 Canvas 文字 paint 提交结束。
    measurement_boundary: TerminalEchoMeasurementBoundary,
    /// 当前采集点不观测 OS compositor present 或屏幕扫描输出。
    os_screen_present_measured: bool,
    /// 95 分位仅来自探针 marker 与同 session/revision 的真实 Canvas paint 关联样本。
    p95_latency_ns: Option<u64>,
    /// poll/admission 与输入/Canvas 生命周期计数，不含输入正文或 marker 文本。
    probe_diagnostics: TerminalEchoProbeDiagnostics,
    samples: Vec<TerminalEchoSample>,
}

/// 终端 echo 的计时边界属于 GPUI Canvas 绘制提交，不代表操作系统呈现或扫描输出。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TerminalEchoMeasurementBoundary {
    RootTrackedCharacterFifoEnqueueStartToGpuiCanvasTextPaintSubmission,
}

/// 汇总 v2 单字符 Canvas 匹配、admission 计数与配置中的 expectedTotal，保留不足样本和拒绝原因。
///
/// @author kongweiguang
fn summarize_terminal_echo_measurement(
    enabled: bool,
    expected_total: Option<usize>,
    measurement: TerminalEchoProbeMeasurement,
) -> TerminalEchoMetricsReport {
    let TerminalEchoProbeMeasurement {
        samples,
        diagnostics,
        ..
    } = measurement;
    let mut latencies = samples
        .iter()
        .map(|sample| sample.latency_ns)
        .collect::<Vec<_>>();
    latencies.sort_unstable();
    let p95_index = latencies
        .len()
        .checked_mul(95)
        .and_then(|scaled| scaled.checked_add(99))
        .map(|scaled| scaled / 100)
        .and_then(|rank| rank.checked_sub(1));
    let sample_count = samples.len();
    TerminalEchoMetricsReport {
        enabled,
        expected_total,
        status: if !enabled {
            TerminalEchoCaptureStatus::Disabled
        } else if sample_count == 0 {
            TerminalEchoCaptureStatus::InsufficientSamples
        } else {
            TerminalEchoCaptureStatus::SamplesCollected
        },
        sample_count,
        probe_semantics_version: TERMINAL_ECHO_PROBE_SEMANTICS_VERSION,
        sample_kind: TerminalEchoSampleKind::SingleAsciiCharacterUiFifoToCanvasTextPaint,
        measurement_boundary: TerminalEchoMeasurementBoundary::RootTrackedCharacterFifoEnqueueStartToGpuiCanvasTextPaintSubmission,
        os_screen_present_measured: false,
        p95_latency_ns: p95_index.and_then(|index| latencies.get(index).copied()),
        probe_diagnostics: diagnostics,
        samples,
    }
}

/// 仅保留测量起止两次 GPUI 窗口观测，不把边界值扩展为整个运行期间的稳定性结论。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowMetricsBoundary {
    start: WindowMetricsSnapshot,
    end: WindowMetricsSnapshot,
}

/// GPUI 当前窗口的逻辑尺寸、关联显示器缩放和可选刷新间隔报告。
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowMetricsSnapshot {
    /// 来自 `Window::bounds()`，单位为 GPUI 逻辑像素。
    logical_width: f32,
    /// 来自 `Window::bounds()`，单位为 GPUI 逻辑像素。
    logical_height: f32,
    /// GPUI 为当前窗口关联显示器报告的缩放因子。
    scale_factor: f32,
    /// 采样时窗口是否处于活动状态，仅描述该起止边界瞬间。
    window_active: bool,
    /// GPUI 可选报告的刷新间隔，单位为纳秒；不表示扫描输出或光子级刷新。
    reported_refresh_interval_ns: Option<u64>,
}

impl WindowMetricsSnapshot {
    /// 在既有窗口快照回调中同步读取尺寸、焦点状态与显示参数，不增加探测任务或隐私标识。
    /// 活动状态只供分析测量边界，不参与采样口径或窗口行为。
    ///
    /// @author kongweiguang
    fn capture(window: &Window, cx: &App) -> Self {
        let bounds = window.bounds();
        Self {
            logical_width: bounds.size.width.as_f32(),
            logical_height: bounds.size.height.as_f32(),
            scale_factor: window.scale_factor(),
            window_active: window.is_window_active(),
            reported_refresh_interval_ns: window
                .refresh_interval(cx)
                .and_then(|interval| u64::try_from(interval.as_nanos()).ok()),
        }
    }
}

/// 单次有限时长采样的环境配置；采样始终由显式环境变量控制。
struct AcceptanceMetricsConfig {
    duration: Duration,
    warmup_duration: Duration,
    output_path: PathBuf,
    environment_id: String,
    workload: String,
    terminal_echo_enabled: bool,
    terminal_echo_expected_total: Option<usize>,
}

impl AcceptanceMetricsConfig {
    /// 将启用、时长和输出参数集中校验；warmup 覆盖仅由显式 opt-in 环境变量应用。
    ///
    /// @author kongweiguang
    fn from_environment() -> Result<Option<Self>, AcceptanceMetricsConfigError> {
        let enabled = read_text_environment(ENABLE_ENV)?;
        match enabled.as_deref() {
            None | Some("0") | Some("false") => return Ok(None),
            Some("1") => {}
            Some(_) => return Err(AcceptanceMetricsConfigError::InvalidEnableValue),
        }

        let duration = read_text_environment(DURATION_ENV)?;
        let warmup_value = read_text_environment(WARMUP_DURATION_ENV)?;
        let warmup_duration = parse_warmup_duration(warmup_value.as_deref())?;
        let environment_id = read_text_environment(ENVIRONMENT_ENV)?;
        let workload = read_text_environment(WORKLOAD_ENV)?;
        let terminal_echo_enabled_value = read_text_environment(TERMINAL_ECHO_ENV)?;
        let terminal_echo_enabled =
            parse_terminal_echo_opt_in(terminal_echo_enabled_value.as_deref())?;
        let terminal_echo_expected_total = if terminal_echo_enabled {
            let expected_total_value = read_text_environment(TERMINAL_ECHO_EXPECTED_SESSIONS_ENV)?;
            parse_terminal_echo_expected_sessions(expected_total_value.as_deref())
                .map_err(|_| AcceptanceMetricsConfigError::InvalidExpectedSessionCount)?
        } else {
            None
        };
        let output_path = output_path_from_environment();
        let mut config = parse_config(
            enabled.as_deref(),
            duration.as_deref(),
            output_path.as_deref(),
            environment_id.as_deref(),
            workload.as_deref(),
        )?;
        if let Some(config) = &mut config {
            config.warmup_duration = warmup_duration;
            config.terminal_echo_enabled = terminal_echo_enabled;
            config.terminal_echo_expected_total = terminal_echo_expected_total;
        }
        Ok(config)
    }
}

/// 在应用 runtime 中原子写入 JSON 报告；同目录硬链接确保完整提交且不覆盖既有文件。
///
/// @author kongweiguang
async fn persist_report(
    output_path: PathBuf,
    report: NativeWindowMetricsReport,
    cancellation: CancellationToken,
) -> AppResult<()> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("原生窗口性能报告写入"));
    }

    let payload = serde_json::to_vec_pretty(&report)
        .map_err(|error| AppError::infrastructure_with_source("原生窗口性能报告序列化", error))?;
    let parent = output_path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| AppError::infrastructure("原生窗口性能报告没有父目录"))?;
    tokio::fs::create_dir_all(parent)
        .await
        .map_err(|error| AppError::infrastructure_with_source("原生窗口性能报告目录创建", error))?;

    if tokio::fs::try_exists(&output_path)
        .await
        .map_err(|error| AppError::infrastructure_with_source("原生窗口性能报告目标检查", error))?
    {
        return Err(AppError::conflict("原生窗口性能报告目标已存在"));
    }

    let temporary_path = next_temporary_path(&output_path)
        .ok_or_else(|| AppError::infrastructure("原生窗口性能报告临时文件名无效"))?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .await
        .map_err(|error| {
            AppError::infrastructure_with_source("原生窗口性能报告临时文件创建", error)
        })?;

    if let Err(error) = file.write_all(&payload).await {
        drop(file);
        cleanup_temporary_file(&temporary_path).await;
        return Err(AppError::infrastructure_with_source(
            "原生窗口性能报告写入",
            error,
        ));
    }
    if let Err(error) = file.sync_all().await {
        drop(file);
        cleanup_temporary_file(&temporary_path).await;
        return Err(AppError::infrastructure_with_source(
            "原生窗口性能报告同步",
            error,
        ));
    }
    drop(file);

    if cancellation.is_cancelled() {
        cleanup_temporary_file(&temporary_path).await;
        return Err(AppError::cancelled("原生窗口性能报告写入"));
    }
    match tokio::fs::hard_link(&temporary_path, &output_path).await {
        Ok(()) => {
            cleanup_temporary_file(&temporary_path).await;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            cleanup_temporary_file(&temporary_path).await;
            Err(AppError::conflict("原生窗口性能报告目标已存在"))
        }
        Err(error) => {
            cleanup_temporary_file(&temporary_path).await;
            Err(AppError::infrastructure_with_source(
                "原生窗口性能报告原子提交",
                error,
            ))
        }
    }
}

/// 为同目录提交生成互不冲突的临时文件名，不覆盖残留的诊断材料。
///
/// @author kongweiguang
fn next_temporary_path(output_path: &Path) -> Option<PathBuf> {
    let mut temporary_name = output_path.file_name()?.to_os_string();
    let sequence = TEMPORARY_FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    temporary_name.push(format!(".{}.{}.tmp", std::process::id(), sequence));
    Some(output_path.with_file_name(temporary_name))
}

/// 清理未提交的临时报告；清理失败时保留稳定日志事件以便诊断。
///
/// @author kongweiguang
async fn cleanup_temporary_file(path: &Path) {
    if let Err(error) = tokio::fs::remove_file(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::error!(
            event = "NATIVE_WINDOW_METRICS_TEMP_FILE_CLEANUP_FAILED",
            error_kind = ?error.kind(),
        );
    }
}

#[cfg(test)]
#[path = "../tests/unit/acceptance_metrics.rs"]
mod tests;
