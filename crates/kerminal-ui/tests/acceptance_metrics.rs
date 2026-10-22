#![cfg(feature = "acceptance-metrics")]
//! GPUI 帧时序适配器的独立验收测试。
//!
//! @author kongweiguang

use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use gpui_kit::{ScrollDelta, TouchPhase, WindowId, point, profiler::WindowProfiler, px};
use kerminal_application::sessions::TerminalCommandContextVersion;
use kerminal_ui::acceptance_metrics::{
    FrameMetricsEnvironment, FrameMetricsError, HistogramUnit, InputMetricsError,
    MeasurementInterval, RendererKind, TerminalEchoAdmissionRejection,
    TerminalEchoExpectedSessionsConfigError, TerminalEchoPaintObservation, TerminalEchoPhase,
    TerminalEchoProbeRegistry, TerminalEchoTarget, TerminalWheelDiagnostics,
    parse_terminal_echo_expected_sessions, summarize_frame_metrics, summarize_input_metrics,
};

static NEXT_WINDOW_ID: AtomicU64 = AtomicU64::new(1);

/// 验证可选会话总数只接受明确的 1/10/50 workload 矩阵，缺省保持旧采样行为。
///
/// @author kongweiguang
#[test]
fn terminal_echo_expected_total_accepts_only_supported_session_counts() {
    assert_eq!(parse_terminal_echo_expected_sessions(None), Ok(None));
    for (value, expected) in [("1", 1), ("10", 10), ("50", 50)] {
        assert_eq!(
            parse_terminal_echo_expected_sessions(Some(value)),
            Ok(Some(expected))
        );
    }
    for value in ["", "0", "5", "01", " 50", "100"] {
        assert_eq!(
            parse_terminal_echo_expected_sessions(Some(value)),
            Err(TerminalEchoExpectedSessionsConfigError::InvalidValue)
        );
    }
}

/// 验证 Canvas wheel 计数记录原始 phase/delta，并在 reset/take 边界清零且不推断滚动成功。
///
/// @author kongweiguang
#[test]
fn terminal_wheel_diagnostics_count_canvas_callback_phases_and_raw_delta() {
    let registry = TerminalEchoProbeRegistry::default();
    registry.record_canvas_wheel_callback(
        TouchPhase::Started,
        &ScrollDelta::Pixels(point(px(0.0), px(9.0))),
    );
    registry.reset();

    registry
        .record_canvas_wheel_callback(TouchPhase::Started, &ScrollDelta::Lines(point(0.0, 0.0)));
    registry.record_canvas_wheel_callback(TouchPhase::Moved, &ScrollDelta::Lines(point(0.0, 0.75)));
    registry.record_canvas_wheel_callback(
        TouchPhase::Moved,
        &ScrollDelta::Pixels(point(px(0.0), px(0.0))),
    );
    registry.record_canvas_wheel_callback(
        TouchPhase::Ended,
        &ScrollDelta::Pixels(point(px(2.0), px(0.0))),
    );
    registry.record_canvas_wheel_callback(
        TouchPhase::Cancelled,
        &ScrollDelta::Lines(point(f32::NAN, 0.0)),
    );

    let measured = registry.take_measurement();
    assert_eq!(
        measured.terminal_wheel,
        TerminalWheelDiagnostics {
            callbacks: 5,
            started: 1,
            moved: 2,
            ended: 1,
            cancelled: 1,
            raw_nonzero_delta_events: 2,
        }
    );
    assert_eq!(
        registry.take_measurement().terminal_wheel,
        TerminalWheelDiagnostics::default()
    );
}

/// 创建独立 profiler，避免测试依赖原生窗口或合成器。
///
/// @author kongweiguang
fn new_profiler() -> WindowProfiler {
    let id = WindowId::from(NEXT_WINDOW_ID.fetch_add(1, Ordering::Relaxed));
    WindowProfiler::new(id).expect("GPUI window profiler should initialize")
}

/// 通过真实 GPUI profiler 的绘制入口记录样本，不模拟直方图实现。
///
/// @author kongweiguang
fn record_draws(profiler: &mut WindowProfiler, count: usize) {
    for _ in 0..count {
        profiler.begin_draw();
        profiler.end_draw(None, 0);
    }
}

/// 验证预热基线差分、空样本语义和不兼容基线失败路径。
///
/// @author kongweiguang
#[test]
fn reports_delta_and_rejects_an_incompatible_baseline() {
    let environment =
        FrameMetricsEnvironment::new("ci-linux-vulkan", "terminal-output", RendererKind::Gpu);
    let mut profiler = new_profiler();

    let empty = profiler.frame_duration_snapshot();
    let empty_report = summarize_frame_metrics(&empty, None, environment.clone())
        .expect("empty window snapshot should be reportable");
    assert_eq!(
        empty_report.interval,
        MeasurementInterval::WindowLifetimeCumulative
    );
    assert_eq!(empty_report.gpui_window_draw.sample_count, 0);
    assert_eq!(empty_report.gpui_window_draw.p95, None);
    assert_eq!(empty_report.gpui_dirty_to_present.p95, None);
    assert!(
        empty_report
            .gpui_window_draw
            .buckets
            .iter()
            .all(|bucket| bucket.sample_count == 0)
    );

    record_draws(&mut profiler, 2);
    let warmup_baseline = profiler.frame_duration_snapshot();
    record_draws(&mut profiler, 3);
    let measured = profiler.frame_duration_snapshot();

    let report = summarize_frame_metrics(&measured, Some(&warmup_baseline), environment)
        .expect("same-window baseline should be subtracted");
    assert_eq!(report.interval, MeasurementInterval::SinceBaseline);
    assert_eq!(report.gpui_window_draw.sample_count, 3);
    assert!(report.gpui_window_draw.p95.is_some());
    assert_eq!(report.gpui_window_draw.unit, HistogramUnit::Nanoseconds);
    assert_eq!(
        report
            .gpui_window_draw
            .buckets
            .iter()
            .map(|bucket| bucket.sample_count)
            .sum::<u64>(),
        report.gpui_window_draw.sample_count
    );
    assert_eq!(report.environment.environment_id, "ci-linux-vulkan");
    assert_eq!(report.environment.renderer, RendererKind::Gpu);

    let other_window = new_profiler().frame_duration_snapshot();
    assert_eq!(
        summarize_frame_metrics(&other_window, Some(&warmup_baseline), report.environment)
            .unwrap_err(),
        FrameMetricsError::WindowDraw
    );
}

/// 验证原始报告结构统一序列化为 camelCase，便于不同平台和脚本稳定读取。
///
/// @author kongweiguang
#[test]
fn report_fields_use_camel_case() {
    let profiler = new_profiler();
    let report = summarize_frame_metrics(
        &profiler.frame_duration_snapshot(),
        None,
        FrameMetricsEnvironment::new("test", "manual", RendererKind::Unknown),
    )
    .expect("empty profiler snapshots should remain serializable");
    let serialized = serde_json::to_value(report).expect("metrics should serialize");

    assert!(serialized.get("gpuiDirtyToPresent").is_some());
    assert!(
        serialized["gpuiDirtyToPresent"]
            .get("sampleCount")
            .is_some()
    );
    assert!(serialized["environment"].get("environmentId").is_some());
}

/// 验证窗口级输入分布差分保留独立单位，并在零样本时保持未测状态。
///
/// @author kongweiguang
#[test]
fn reports_generic_window_input_separately_from_terminal_echo() {
    let profiler = new_profiler();
    let mut baseline = profiler.input_latency_snapshot();
    baseline
        .latency_histogram
        .record(1_000_000)
        .expect("latency sample should fit the profiler histogram");
    baseline
        .events_per_frame_histogram
        .record(1)
        .expect("event sample should fit the profiler histogram");

    let mut current = baseline.clone();
    current
        .latency_histogram
        .record(2_000_000)
        .expect("latency sample should fit the profiler histogram");
    current
        .events_per_frame_histogram
        .record(3)
        .expect("event sample should fit the profiler histogram");
    current.mid_draw_events_dropped = 2;

    let metrics = summarize_input_metrics(&current, Some(&baseline))
        .expect("same-window profiler snapshots should subtract their histograms");
    assert_eq!(metrics.interval, MeasurementInterval::SinceBaseline);
    assert_eq!(metrics.gpui_input_to_frame.sample_count, 1);
    assert_eq!(metrics.gpui_input_to_frame.unit, HistogramUnit::Nanoseconds);
    assert_eq!(metrics.gpui_input_events_per_frame.sample_count, 1);
    assert_eq!(
        metrics.gpui_input_events_per_frame.unit,
        HistogramUnit::EventsPerFrame
    );
    assert_eq!(metrics.mid_draw_events_dropped, 2);

    let empty = profiler.input_latency_snapshot();
    let empty_metrics = summarize_input_metrics(&empty, Some(&empty))
        .expect("an empty post-baseline interval is valid but has no measured p95");
    assert_eq!(empty_metrics.gpui_input_to_frame.sample_count, 0);
    assert_eq!(empty_metrics.gpui_input_to_frame.p95, None);
    assert_eq!(empty_metrics.gpui_input_events_per_frame.sample_count, 0);
}

/// 验证输入快照回退时返回差分错误，避免生成看似成功的窗口级指标。
///
/// @author kongweiguang
#[test]
fn rejects_input_baseline_that_exceeds_current_snapshot() {
    let profiler = new_profiler();
    let mut baseline = profiler.input_latency_snapshot();
    baseline
        .latency_histogram
        .record(100)
        .expect("latency sample should fit the profiler histogram");
    let current = profiler.input_latency_snapshot();

    assert_eq!(
        summarize_input_metrics(&current, Some(&baseline)).unwrap_err(),
        InputMetricsError::InputLatency
    );
}

/// 构造无正文的版本票据，让回归只验证 CAS 代次，不依赖 shell 或会话服务。
///
/// @author kongweiguang
fn echo_context(
    session_generation: u64,
    prompt_epoch: u64,
    input_generation: u64,
) -> TerminalCommandContextVersion {
    TerminalCommandContextVersion {
        session_generation,
        prompt_epoch,
        input_generation,
    }
}

/// 固定目标 cell 供 registry 状态测试共享，避免把字符内容存入探针对象。
///
/// @author kongweiguang
fn echo_target() -> TerminalEchoTarget {
    TerminalEchoTarget {
        viewport_row: 3,
        column: 9,
    }
}

/// 构造成功单 cell paint 的几何与活动状态证据；失败和非活动变体在调用点显式覆盖。
///
/// @author kongweiguang
fn painted_character() -> TerminalEchoPaintObservation {
    TerminalEchoPaintObservation {
        target_cell_is_character: true,
        painted_run_covers_target: true,
        cursor_visible: true,
        cursor_row: 3,
        cursor_column: 10,
        at_bottom: true,
        window_active_at_paint: true,
    }
}

/// 验证样本必须关联同一 session/generation、精确 cell、新 revision、writer ACK 与成功 paint。
///
/// @author kongweiguang
#[test]
fn terminal_echo_requires_exact_writer_and_canvas_evidence() {
    let registry = TerminalEchoProbeRegistry::default();
    let probe = registry.issue_probe("session-a", 7, echo_target());
    let initial_context = echo_context(7, 2, 10);
    let writer_context = echo_context(7, 2, 11);
    let another_probe = registry.issue_probe("session-a", 7, echo_target());
    assert_ne!(probe.probe_id(), another_probe.probe_id());
    let submitted_at = Instant::now();
    assert!(registry.record_submission(&probe, "session-a", 7, initial_context, 40, submitted_at,));
    assert!(registry.record_enqueued(&probe));

    let paint_time = submitted_at + Duration::from_millis(11);
    let mut invalid = painted_character();
    invalid.target_cell_is_character = false;
    registry.record_canvas_character_paint("session-a", 7, 41, echo_target(), invalid, paint_time);
    registry.record_canvas_character_paint(
        "session-b",
        7,
        41,
        echo_target(),
        painted_character(),
        paint_time,
    );
    registry.record_canvas_character_paint(
        "session-a",
        8,
        41,
        echo_target(),
        painted_character(),
        paint_time,
    );
    registry.record_canvas_character_paint(
        "session-a",
        7,
        40,
        echo_target(),
        painted_character(),
        paint_time,
    );
    registry.record_canvas_character_paint(
        "session-a",
        7,
        41,
        TerminalEchoTarget {
            column: 8,
            ..echo_target()
        },
        painted_character(),
        paint_time,
    );
    assert!(!registry.sample_is_ready(&probe));
    assert!(!registry.record_writer_ack(&probe, echo_context(7, 2, 10), true, true, paint_time,));
    assert!(!registry.record_writer_ack(&probe, echo_context(7, 2, 12), true, true, paint_time,));

    // Paint 先到也只能存其 revision/time；只有同一 writer 的精确 ACK 到达后才产样本。
    let mut correct_paint = painted_character();
    correct_paint.painted_run_covers_target = false;
    registry.record_canvas_character_paint(
        "session-a",
        7,
        42,
        echo_target(),
        correct_paint,
        paint_time,
    );
    assert!(!registry.sample_is_ready(&probe));
    registry.record_canvas_character_paint(
        "session-a",
        7,
        42,
        echo_target(),
        TerminalEchoPaintObservation {
            window_active_at_paint: false,
            ..painted_character()
        },
        paint_time,
    );
    assert!(registry.record_writer_ack(
        &probe,
        writer_context,
        true,
        true,
        paint_time + Duration::from_millis(20),
    ));
    assert!(registry.sample_is_ready(&probe));

    let measurement = registry.take_measurement();
    assert_eq!(measurement.samples.len(), 1);
    assert_eq!(measurement.samples[0].session_id, "session-a");
    assert_eq!(measurement.samples[0].session_generation, 7);
    assert_eq!(measurement.samples[0].revision, 42);
    assert_eq!(measurement.samples[0].latency_ns, 11_000_000);
    assert!(!measurement.samples[0].window_active_at_paint);
}

/// 验证清理只能在字符 writer ACK 和精确投影后排队；样本与调试输出不保存输入字符。
///
/// @author kongweiguang
#[test]
fn terminal_echo_cleanup_requires_exact_character_ack_and_empty_backspace_ack() {
    let registry = TerminalEchoProbeRegistry::default();
    let completed = registry.issue_probe("session-ok", 5, echo_target());
    assert!(!format!("{completed:?}").contains("x"));
    let initial = echo_context(5, 3, 8);
    let character_ack = echo_context(5, 3, 9);
    let submitted_at = Instant::now();
    assert!(registry.record_submission(&completed, "session-ok", 5, initial, 3, submitted_at));
    assert!(registry.record_enqueued(&completed));
    registry.record_canvas_character_paint(
        "session-ok",
        5,
        4,
        echo_target(),
        painted_character(),
        submitted_at + Duration::from_millis(1),
    );
    assert!(registry.record_writer_ack(&completed, character_ack, true, true, submitted_at));
    assert!(!registry.authorize_cleanup(&completed, character_ack, false, true));
    assert!(!registry.authorize_cleanup(&completed, character_ack, true, false));
    assert!(registry.authorize_cleanup(&completed, character_ack, true, true));
    assert!(registry.record_cleanup_enqueued(&completed));
    assert!(!registry.record_cleanup_ack(&completed, echo_context(5, 3, 11), true, true,));
    assert!(!registry.record_cleanup_ack(&completed, echo_context(5, 3, 10), false, true,));
    assert!(registry.record_cleanup_ack(&completed, echo_context(5, 3, 10), true, true,));
    let measured = registry.take_measurement();
    assert_eq!(measured.samples.len(), 1);
    assert!(measured.samples[0].window_active_at_paint);
    let serialized = serde_json::to_string(&measured.samples).expect("sample DTO should serialize");
    assert!(!serialized.contains("ordinary keyboard text"));
    assert!(registry.take_samples().is_empty());
}

/// 验证测量区分拒绝、入队、writer ACK、Canvas paint 与清理；超时仍可安全闭环。
///
/// 显式 reset 让拒绝计入 admission_rejections；reset 前发生的拒绝会保留在启动期计数。
///
/// @author kongweiguang
#[test]
fn terminal_echo_diagnostics_distinguish_admission_enqueue_paint_and_timeout() {
    let registry = TerminalEchoProbeRegistry::default();
    registry.reset();
    registry.record_poll();
    registry
        .record_admission_rejection(TerminalEchoAdmissionRejection::UiInputProjectionInvalidated);
    let probe = registry.issue_probe("isolated-session", 9, echo_target());
    let initial_context = echo_context(9, 1, 4);
    let writer_context = echo_context(9, 1, 5);
    let submitted_at = Instant::now();
    assert!(registry.record_submission(
        &probe,
        "isolated-session",
        9,
        initial_context,
        22,
        submitted_at
    ));
    assert!(registry.record_enqueued(&probe));
    registry.record_canvas_character_paint(
        "isolated-session",
        9,
        23,
        echo_target(),
        painted_character(),
        submitted_at + Duration::from_millis(5),
    );
    assert!(registry.record_writer_ack(
        &probe,
        writer_context,
        true,
        true,
        submitted_at + Duration::from_millis(6)
    ));
    assert!(registry.authorize_cleanup(&probe, writer_context, true, true));
    assert!(registry.record_cleanup_enqueued(&probe));
    assert!(registry.record_cleanup_ack(&probe, echo_context(9, 1, 6), true, true));

    let pending = registry.issue_probe("isolated-session", 9, echo_target());
    assert!(registry.record_submission(
        &pending,
        "isolated-session",
        9,
        echo_context(9, 1, 6),
        23,
        submitted_at - Duration::from_secs(20),
    ));
    assert!(registry.record_enqueued(&pending));
    assert_eq!(registry.expire_probes(Instant::now()), 1);

    let measurement = registry.take_measurement();
    assert_eq!(measurement.samples.len(), 1);
    assert_eq!(measurement.diagnostics.polls, 1);
    assert_eq!(measurement.diagnostics.issued, 2);
    assert_eq!(measurement.diagnostics.enqueued, 2);
    assert_eq!(measurement.diagnostics.paint_matched, 1);
    assert_eq!(measurement.diagnostics.timed_out, 1);
    assert_eq!(measurement.diagnostics.writer_acknowledged, 1);
    assert_eq!(measurement.diagnostics.cleanup_acknowledged, 1);
    assert_eq!(measurement.diagnostics.cleanup_stopped, 1);
    assert_eq!(
        measurement
            .diagnostics
            .admission_rejections
            .get(&TerminalEchoAdmissionRejection::UiInputProjectionInvalidated),
        Some(&1)
    );
}

/// 验证被 Root FIFO 明确拒绝的探针单独计数，且不会被后续标记绘制误认成回显样本。
///
/// @author kongweiguang
#[test]
fn terminal_echo_queue_rejection_cannot_become_a_paint_match() {
    let registry = TerminalEchoProbeRegistry::default();
    let probe = registry.issue_probe("isolated-session", 2, echo_target());
    let submitted_at = Instant::now();
    assert!(registry.record_submission(
        &probe,
        "isolated-session",
        2,
        echo_context(2, 1, 1),
        4,
        submitted_at,
    ));
    assert!(registry.record_queue_rejected(&probe));
    assert!(registry.paint_target("isolated-session", 2).is_none());
    let measurement = registry.take_measurement();
    assert_eq!(measurement.diagnostics.issued, 1);
    assert_eq!(measurement.diagnostics.enqueued, 0);
    assert_eq!(measurement.diagnostics.queue_rejected, 1);
    assert_eq!(measurement.diagnostics.paint_matched, 0);
}

/// 验证 warmup 前到达的 fresh hook 与拒绝计数在 desktop measurement reset 后仍可诊断。
///
/// @author kongweiguang
#[test]
fn terminal_echo_warmup_reset_preserves_startup_hook_diagnostics() {
    let registry = TerminalEchoProbeRegistry::default();
    registry.record_hook_invocation();
    registry.record_admission_rejection(TerminalEchoAdmissionRejection::FreshPaneHidden);
    registry.reset();
    registry.record_poll();
    registry.record_admission_rejection(TerminalEchoAdmissionRejection::ContextReadFailed);

    let measurement = registry.take_measurement();
    assert_eq!(measurement.diagnostics.hook_invocations, 1);
    assert_eq!(measurement.diagnostics.polls, 1);
    assert_eq!(
        measurement
            .diagnostics
            .startup_admission_rejections
            .get(&TerminalEchoAdmissionRejection::FreshPaneHidden),
        Some(&1)
    );
    assert_eq!(
        serde_json::to_value(TerminalEchoAdmissionRejection::FreshPaneHidden)
            .expect("admission rejection should serialize"),
        serde_json::json!("fresh_pane_hidden")
    );
    assert_eq!(
        measurement
            .diagnostics
            .admission_rejections
            .get(&TerminalEchoAdmissionRejection::ContextReadFailed),
        Some(&1)
    );
}

/// Proves phase association rejects stale identity/generation and preserves Canvas timing before ACK.
///
/// @author kongweiguang
#[test]
fn phase_journal_uses_matching_generation_and_original_paint_time() {
    let registry = TerminalEchoProbeRegistry::default();
    let target = TerminalEchoTarget {
        viewport_row: 0,
        column: 1,
    };
    let probe = registry.issue_probe("session-a", 7, target);
    let initial_context = TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 3,
        input_generation: 10,
    };
    let submitted_at = Instant::now();
    assert!(
        registry.record_submission(&probe, "session-a", 7, initial_context, 100, submitted_at,)
    );
    assert!(registry.record_enqueued(&probe));

    registry.record_phase_for_probe("stale-probe-id", TerminalEchoPhase::FifoDequeued, None);
    registry.record_phase_for_session("session-a", 6, TerminalEchoPhase::WatchReceived, Some(101));
    registry.record_phase_for_session("session-a", 7, TerminalEchoPhase::WatchReceived, Some(101));

    let painted_at = submitted_at + Duration::from_millis(12);
    registry.record_canvas_character_paint(
        "session-a",
        7,
        101,
        target,
        TerminalEchoPaintObservation {
            target_cell_is_character: true,
            painted_run_covers_target: true,
            cursor_visible: true,
            cursor_row: 0,
            cursor_column: 2,
            at_bottom: true,
            window_active_at_paint: true,
        },
        painted_at,
    );
    registry.record_phase_for_session(
        "session-a",
        7,
        TerminalEchoPhase::SnapshotReturned,
        Some(101),
    );

    let acknowledged_context = TerminalCommandContextVersion {
        input_generation: 11,
        ..initial_context
    };
    assert!(registry.record_writer_ack(
        &probe,
        acknowledged_context,
        true,
        true,
        submitted_at + Duration::from_millis(20),
    ));
    let measurement = registry.take_measurement();
    assert_eq!(measurement.samples.len(), 1);
    let sample = &measurement.samples[0];
    assert_eq!(sample.revision, 101);
    assert_eq!(sample.latency_ns, 12_000_000);
    assert!(sample.phase_events.iter().any(|event| {
        event.phase == TerminalEchoPhase::WatchReceived && event.revision == Some(101)
    }));
    assert!(sample.phase_events.iter().any(|event| {
        event.phase == TerminalEchoPhase::CanvasPainted && event.revision == Some(101)
    }));
    assert!(
        !sample
            .phase_events
            .iter()
            .any(|event| { event.phase == TerminalEchoPhase::SnapshotReturned })
    );
}
