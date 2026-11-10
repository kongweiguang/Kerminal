//! 桌面原生帧采样配置与结果状态的外置单元测试。
//!
//! @author kongweiguang

use std::{
    path::Path,
    time::{Duration, Instant},
};

use gpui_kit::{
    FrameRequestSource, WindowId,
    profiler::journal::{
        DrainedEntries, ForegroundJournalEntry, IntervalBoundary, PlatformSignal, PresentedFrame,
    },
    profiler::{DrawPhases, FrameTiming, PresentTiming},
};
use kerminal_ui::acceptance_metrics::{
    FrameMetricsEnvironment, HistogramBucket, HistogramSummary, HistogramUnit, MeasurementInterval,
    RendererKind, TerminalEchoAdmissionRejection, TerminalEchoProbeDiagnostics,
    TerminalEchoProbeMeasurement, TerminalEchoSample, TerminalWheelDiagnostics, WindowFrameMetrics,
    WindowInputMetrics,
};

use super::{
    AcceptanceMetricsConfigError, BoundedTimingSamples, CaptureStatus, FramePhaseAccumulator,
    MAX_FRAME_PHASE_SAMPLES_PER_PHASE, MAX_MEASUREMENT_DURATION_MS, MAX_WARMUP_DURATION_MS,
    NativeWindowMetricsReport, TerminalEchoCaptureStatus, WindowMetricsBoundary,
    WindowMetricsSnapshot, next_temporary_path, parse_config, parse_terminal_echo_opt_in,
    parse_warmup_duration, summarize_terminal_echo_measurement, summarize_timing_distribution,
};

/// 验证未显式启用采样时，其他变量不会触发后台任务或配置错误。
///
/// @author kongweiguang
#[test]
fn disabled_sampling_ignores_measurement_arguments() {
    assert!(matches!(
        parse_config(None, None, None, None, None),
        Ok(None)
    ));
    assert!(matches!(
        parse_config(Some("0"), None, None, None, None),
        Ok(None)
    ));
    assert!(matches!(
        parse_config(Some("false"), None, None, None, None),
        Ok(None)
    ));
}

/// 验证显式启用必须提供有限时长和绝对输出文件路径。
///
/// @author kongweiguang
#[test]
fn enabled_sampling_validates_duration_and_output() {
    let absolute_output = std::env::temp_dir().join("kerminal-acceptance.json");
    let maximum_duration = MAX_MEASUREMENT_DURATION_MS.to_string();
    assert!(matches!(
        parse_config(
            Some("enabled"),
            Some("1000"),
            Some(&absolute_output),
            None,
            None
        ),
        Err(AcceptanceMetricsConfigError::InvalidEnableValue)
    ));
    assert!(matches!(
        parse_config(Some("1"), None, Some(&absolute_output), None, None),
        Err(AcceptanceMetricsConfigError::MissingDuration)
    ));
    assert!(matches!(
        parse_config(Some("1"), Some("abc"), Some(&absolute_output), None, None),
        Err(AcceptanceMetricsConfigError::InvalidDuration)
    ));
    assert!(matches!(
        parse_config(Some("1"), Some("0"), Some(&absolute_output), None, None),
        Err(AcceptanceMetricsConfigError::DurationOutOfRange)
    ));
    assert!(matches!(
        parse_config(
            Some("1"),
            Some(maximum_duration.as_str()),
            Some(&absolute_output),
            None,
            None,
        ),
        Ok(Some(_))
    ));
    assert!(matches!(
        parse_config(
            Some("1"),
            Some("7200001"),
            Some(&absolute_output),
            None,
            None
        ),
        Err(AcceptanceMetricsConfigError::DurationOutOfRange)
    ));
    assert!(matches!(
        parse_config(Some("1"), Some("1000"), None, None, None),
        Err(AcceptanceMetricsConfigError::MissingOutput)
    ));
    assert!(matches!(
        parse_config(
            Some("1"),
            Some("1000"),
            Some(Path::new("relative-report.json")),
            None,
            None,
        ),
        Err(AcceptanceMetricsConfigError::OutputMustBeAbsoluteFile)
    ));
}

/// 验证 warmup 缺省兼容旧五秒，且只接受包含两端的有限毫秒范围。
///
/// @author kongweiguang
#[test]
fn warmup_duration_defaults_and_rejects_invalid_or_out_of_range_values() {
    assert_eq!(
        parse_warmup_duration(None),
        Ok(Duration::from_millis(5_000))
    );
    for value in ["1", "300000"] {
        assert_eq!(
            parse_warmup_duration(Some(value)),
            Ok(Duration::from_millis(value.parse().unwrap()))
        );
    }
    assert_eq!(
        parse_warmup_duration(Some("invalid")),
        Err(AcceptanceMetricsConfigError::InvalidWarmupDuration)
    );
    for value in ["0", "300001"] {
        assert_eq!(
            parse_warmup_duration(Some(value)),
            Err(AcceptanceMetricsConfigError::WarmupDurationOutOfRange)
        );
    }
    assert_eq!(MAX_WARMUP_DURATION_MS, 300_000);
}

/// 验证 echo probe 单独 opt-in，缺省关闭且拒绝含糊值，避免普通 metrics 启动时向终端注入输入。
///
/// @author kongweiguang
#[test]
fn terminal_echo_requires_an_exact_opt_in() {
    assert_eq!(parse_terminal_echo_opt_in(None), Ok(false));
    assert_eq!(parse_terminal_echo_opt_in(Some("0")), Ok(false));
    assert_eq!(parse_terminal_echo_opt_in(Some("false")), Ok(false));
    assert_eq!(parse_terminal_echo_opt_in(Some("1")), Ok(true));
    assert_eq!(
        parse_terminal_echo_opt_in(Some("true")),
        Err(AcceptanceMetricsConfigError::InvalidEnableValue)
    );

    let output = std::env::temp_dir().join("kerminal-acceptance.json");
    let config = parse_config(Some("1"), Some("1000"), Some(&output), None, None)
        .expect("valid window metrics configuration should parse")
        .expect("explicit metrics enable should create configuration");
    assert!(!config.terminal_echo_enabled);
    assert_eq!(config.terminal_echo_expected_total, None);
}

/// 验证报告标签使用明确缺省值且拒绝控制字符，避免环境信息被静默改写。
///
/// @author kongweiguang
#[test]
fn labels_are_bounded_and_default_to_unknown_and_manual() {
    let output = std::env::temp_dir().join("kerminal-acceptance.json");
    let config = parse_config(Some("1"), Some("1000"), Some(&output), None, None)
        .expect("valid configuration should parse")
        .expect("explicit enable should create configuration");
    assert_eq!(config.duration, Duration::from_millis(1000));
    assert_eq!(config.warmup_duration, Duration::from_millis(5_000));
    assert_eq!(config.environment_id, "unknown");
    assert_eq!(config.workload, "manual");

    assert!(matches!(
        parse_config(
            Some("1"),
            Some("1000"),
            Some(&output),
            Some("bad\nlabel"),
            None
        ),
        Err(AcceptanceMetricsConfigError::InvalidLabel)
    ));
}

/// 验证采样结果只表示样本是否存在，不会把采集动作冒充为性能通过。
///
/// @author kongweiguang
#[test]
fn zero_frame_samples_remain_insufficient() {
    assert_eq!(
        CaptureStatus::from_frame_samples(0),
        CaptureStatus::InsufficientFrameSamples
    );
    assert_eq!(
        CaptureStatus::from_frame_samples(1),
        CaptureStatus::FrameSamplesCollected
    );
}

/// 验证提交临时文件保留目标目录并使用与原文件不同的唯一名称。
///
/// @author kongweiguang
#[test]
fn temporary_report_stays_next_to_target() {
    let output = std::env::temp_dir().join("kerminal-acceptance.json");
    let temporary = next_temporary_path(&output).expect("file target should have a filename");
    assert_eq!(temporary.parent(), output.parent());
    assert_ne!(temporary, output);
}

/// 验证完整报告保留起止窗口观测且将 GPUI 未提供的刷新间隔编码为空值。
///
/// @author kongweiguang
#[test]
fn window_metadata_serialization_preserves_boundary_samples_and_unknown_refresh() {
    let window_metadata = WindowMetricsBoundary {
        start: WindowMetricsSnapshot {
            logical_width: 960.0,
            logical_height: 640.0,
            scale_factor: 1.0,
            window_active: false,
            reported_refresh_interval_ns: None,
        },
        end: WindowMetricsSnapshot {
            logical_width: 1280.0,
            logical_height: 720.0,
            scale_factor: 1.5,
            window_active: true,
            reported_refresh_interval_ns: Some(16_666_667),
        },
    };
    let histogram = HistogramSummary {
        sample_count: 1,
        p95: Some(1_000_000),
        unit: HistogramUnit::Nanoseconds,
        buckets: vec![HistogramBucket {
            upper_bound: Some(2_000_000),
            sample_count: 1,
        }],
    };
    let interval = MeasurementInterval::SinceBaseline;
    let report = NativeWindowMetricsReport {
        schema_version: 4,
        status: CaptureStatus::FrameSamplesCollected,
        warmup_duration_ms: 5_000,
        target_measurement_duration_ms: 1_000,
        actual_measurement_duration_ms: 1_000,
        window_metadata,
        frame: WindowFrameMetrics {
            interval,
            environment: FrameMetricsEnvironment::new(
                "test",
                "window_metadata",
                RendererKind::Unknown,
            ),
            gpui_dirty_to_present: histogram.clone(),
            gpui_window_draw: histogram.clone(),
            gpui_animated_present_interval: histogram.clone(),
        },
        frame_phases: FramePhaseAccumulator::default().finish(),
        gpui_input: WindowInputMetrics {
            interval,
            gpui_input_to_frame: histogram.clone(),
            gpui_input_events_per_frame: HistogramSummary {
                unit: HistogramUnit::EventsPerFrame,
                ..histogram
            },
            mid_draw_events_dropped: 0,
        },
        terminal_wheel: TerminalWheelDiagnostics {
            callbacks: 5,
            started: 1,
            moved: 2,
            ended: 1,
            cancelled: 1,
            raw_nonzero_delta_events: 2,
        },
        terminal_echo: summarize_terminal_echo_measurement(
            true,
            Some(50),
            TerminalEchoProbeMeasurement {
                samples: vec![TerminalEchoSample {
                    probe_id: "probe-id".to_owned(),
                    session_id: "session-id".to_owned(),
                    session_generation: 4,
                    revision: 17,
                    latency_ns: 42_000_000,
                    window_active_at_paint: false,
                    phase_events: vec![],
                    phase_events_dropped: 0,
                }],
                diagnostics: TerminalEchoProbeDiagnostics::default(),
                terminal_wheel: TerminalWheelDiagnostics::default(),
            },
        ),
    };

    let value = serde_json::to_value(report).expect("acceptance report should serialize");
    let metadata = &value["windowMetadata"];

    assert_eq!(value["schemaVersion"], 4);
    assert_eq!(
        value["framePhases"]["actualDwmVsyncInterarrivalMeasured"],
        false
    );
    assert_eq!(value["framePhases"]["presentedFrameCount"], 0);
    assert_eq!(value["framePhases"]["gpuiDraw"]["sampleCount"], 0);
    assert_eq!(value["framePhases"]["gpuiDraw"]["retainedSampleCount"], 0);
    assert_eq!(value["framePhases"]["gpuiDraw"]["sampleDropped"], 0);
    assert_eq!(value["terminalWheel"]["callbacks"], 5);
    assert_eq!(value["terminalWheel"]["started"], 1);
    assert_eq!(value["terminalWheel"]["moved"], 2);
    assert_eq!(value["terminalWheel"]["ended"], 1);
    assert_eq!(value["terminalWheel"]["cancelled"], 1);
    assert_eq!(value["terminalWheel"]["rawNonzeroDeltaEvents"], 2);
    assert_eq!(metadata["start"]["logicalWidth"], 960.0);
    assert_eq!(metadata["start"]["logicalHeight"], 640.0);
    assert_eq!(metadata["start"]["scaleFactor"], 1.0);
    assert!(metadata["start"]["reportedRefreshIntervalNs"].is_null());
    assert!(
        !metadata["start"]["windowActive"]
            .as_bool()
            .expect("start state should be boolean")
    );
    assert_eq!(metadata["end"]["logicalWidth"], 1280.0);
    assert_eq!(metadata["end"]["logicalHeight"], 720.0);
    assert_eq!(metadata["end"]["scaleFactor"], 1.5);
    assert_eq!(metadata["end"]["reportedRefreshIntervalNs"], 16_666_667);
    assert!(
        metadata["end"]["windowActive"]
            .as_bool()
            .expect("end state should be boolean")
    );
    assert_eq!(value["frame"]["environment"]["renderer"], "unknown");
    assert_eq!(value["terminalEcho"]["status"], "samples_collected");
    assert_eq!(value["terminalEcho"]["expectedTotal"], 50);
    assert_eq!(value["terminalEcho"]["sampleCount"], 1);
    assert_eq!(value["terminalEcho"]["probeSemanticsVersion"], 2);
    assert_eq!(
        value["terminalEcho"]["sampleKind"],
        "single_ascii_character_ui_fifo_to_canvas_text_paint"
    );
    assert_eq!(value["terminalEcho"]["p95LatencyNs"], 42_000_000);
    assert_eq!(
        value["terminalEcho"]["measurementBoundary"],
        "root_tracked_character_fifo_enqueue_start_to_gpui_canvas_text_paint_submission"
    );
    assert_eq!(value["terminalEcho"]["osScreenPresentMeasured"], false);
    assert_eq!(value["terminalEcho"]["samples"][0]["sessionGeneration"], 4);
    assert_eq!(value["terminalEcho"]["samples"][0]["revision"], 17);
    assert!(
        !value["terminalEcho"]["samples"][0]["windowActiveAtPaint"]
            .as_bool()
            .expect("paint state should be boolean")
    );
    assert_eq!(value["terminalEcho"]["probeDiagnostics"]["paintMatched"], 0);
    assert!(
        value["terminalEcho"]["probeDiagnostics"]["admissionRejections"]
            .as_object()
            .expect("fixed rejection categories serialize as an object")
            .is_empty()
    );
}

/// 验证 journal 摘要按窗口过滤并保留各阶段的单调时间差；刷新间隔不充当 VSync 实测。
///
/// @author kongweiguang
#[test]
fn frame_phase_diagnostics_separate_callback_draw_and_platform_submission() {
    let origin = Instant::now();
    let at = |millis| origin + Duration::from_millis(millis);
    let window_id = WindowId::from(41);
    let other_window_id = WindowId::from(42);
    let frame = FrameTiming {
        window_id,
        dirty_at: Some(at(0)),
        invalidations: 1,
        draw_start: at(13),
        draw_end: at(17),
        refresh_interval: Some(Duration::from_millis(7)),
        signal_at: Some(at(10)),
        phases: DrawPhases::default(),
    };
    let mut accumulator = FramePhaseAccumulator::default();
    accumulator.merge_drained(
        DrainedEntries {
            entries: vec![ForegroundJournalEntry::PlatformSignal(PlatformSignal {
                window_id,
                signal_at: Some(at(10)),
                handled_at: at(12),
                source: FrameRequestSource::NativeCallback,
            })],
            lost: 1,
        },
        window_id,
    );
    accumulator.merge_drained(
        DrainedEntries {
            entries: vec![
                ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(PresentedFrame {
                    frame,
                    presentation: PresentTiming {
                        window_id,
                        present_start: at(18),
                        present_end: at(22),
                        animation_interval: None,
                    },
                })),
                ForegroundJournalEntry::ReentrantFrameSkipped {
                    window_id,
                    at: at(11),
                },
                ForegroundJournalEntry::PlatformSignal(PlatformSignal {
                    window_id: other_window_id,
                    signal_at: Some(at(10)),
                    handled_at: at(30),
                    source: FrameRequestSource::NativeCallback,
                }),
                ForegroundJournalEntry::ReentrantFrameSkipped {
                    window_id: other_window_id,
                    at: at(11),
                },
            ],
            lost: 2,
        },
        window_id,
    );
    let summary = accumulator.finish();

    assert_eq!(summary.journal_entries_seen, 5);
    assert_eq!(summary.journal_entries_lost, 3);
    assert_eq!(summary.presented_frame_count, 1);
    assert_eq!(summary.gpui_callback_count, 1);
    assert_eq!(summary.callbacks_with_signal_timestamp, 1);
    assert_eq!(summary.reentrant_frame_skips, 1);
    assert_eq!(
        summary.first_invalidation_to_frame_signal.p95_ns,
        Some(10_000_000)
    );
    assert_eq!(
        summary.frame_signal_to_gpui_callback.p95_ns,
        Some(2_000_000)
    );
    assert_eq!(summary.frame_signal_to_draw_start.p95_ns, Some(3_000_000));
    assert_eq!(summary.gpui_draw.p95_ns, Some(4_000_000));
    assert_eq!(
        summary.draw_end_to_platform_present_start.p95_ns,
        Some(1_000_000)
    );
    assert_eq!(summary.platform_present_submission.p95_ns, Some(4_000_000));
    assert_eq!(
        summary.first_invalidation_to_platform_present_end.p95_ns,
        Some(22_000_000)
    );
    assert!(!summary.actual_dwm_vsync_interarrival_measured);
}

/// 验证各分段合并后只保留首个固定样本前缀，超额样本仍计数且不伪装成全程分位数。
///
/// @author kongweiguang
#[test]
fn frame_phase_samples_keep_a_bounded_prefix_and_report_dropped_samples() {
    let mut samples = BoundedTimingSamples::default();
    for nanos in 0..=(MAX_FRAME_PHASE_SAMPLES_PER_PHASE as u64 + 2) {
        samples.push(nanos);
    }

    let summary = summarize_timing_distribution(samples);
    assert_eq!(summary.sample_count, 10_003);
    assert_eq!(
        summary.retained_sample_count,
        MAX_FRAME_PHASE_SAMPLES_PER_PHASE
    );
    assert_eq!(summary.sample_dropped, 3);
    assert_eq!(summary.p50_ns, Some(4_999));
    assert_eq!(summary.p95_ns, Some(9_499));
    assert_eq!(summary.max_ns, Some(9_999));
}

/// 验证没有匹配的 Canvas 绘制时报告不足样本，且关闭状态不能残留伪造 p95。
///
/// @author kongweiguang
#[test]
fn terminal_echo_report_distinguishes_disabled_and_missing_samples() {
    let disabled = summarize_terminal_echo_measurement(
        false,
        None,
        TerminalEchoProbeMeasurement {
            samples: Vec::new(),
            diagnostics: TerminalEchoProbeDiagnostics::default(),
            terminal_wheel: TerminalWheelDiagnostics::default(),
        },
    );
    assert_eq!(disabled.status, TerminalEchoCaptureStatus::Disabled);
    assert_eq!(disabled.p95_latency_ns, None);

    let missing = summarize_terminal_echo_measurement(
        true,
        None,
        TerminalEchoProbeMeasurement {
            samples: Vec::new(),
            diagnostics: TerminalEchoProbeDiagnostics::default(),
            terminal_wheel: TerminalWheelDiagnostics::default(),
        },
    );
    assert_eq!(
        missing.status,
        TerminalEchoCaptureStatus::InsufficientSamples
    );
    assert_eq!(missing.sample_count, 0);
    assert_eq!(missing.p95_latency_ns, None);
}

/// 验证终端子报告标记单字符 v2 语义，并保留 admission 与输入/绘制阶段诊断。
///
/// @author kongweiguang
#[test]
fn terminal_echo_report_serializes_probe_admission_and_lifecycle_counts() {
    let mut diagnostics = TerminalEchoProbeDiagnostics {
        hook_invocations: 3,
        polls: 8,
        issued: 2,
        enqueued: 1,
        queue_rejected: 1,
        paint_matched: 1,
        timed_out: 0,
        admission_rejections: Default::default(),
        startup_admission_rejections: Default::default(),
        ..TerminalEchoProbeDiagnostics::default()
    };
    diagnostics.admission_rejections.insert(
        TerminalEchoAdmissionRejection::UiInputProjectionInvalidated,
        6,
    );
    diagnostics
        .admission_rejections
        .insert(TerminalEchoAdmissionRejection::SessionCountMismatch, 2);
    let report = summarize_terminal_echo_measurement(
        true,
        Some(50),
        TerminalEchoProbeMeasurement {
            samples: Vec::new(),
            diagnostics,
            terminal_wheel: TerminalWheelDiagnostics::default(),
        },
    );
    let value = serde_json::to_value(report).expect("probe diagnostics should serialize");
    let diagnostics = &value["probeDiagnostics"];
    assert_eq!(value["expectedTotal"], 50);
    assert_eq!(diagnostics["polls"], 8);
    assert_eq!(diagnostics["hookInvocations"], 3);
    assert_eq!(diagnostics["issued"], 2);
    assert_eq!(diagnostics["enqueued"], 1);
    assert_eq!(diagnostics["queueRejected"], 1);
    assert_eq!(diagnostics["paintMatched"], 1);
    assert_eq!(diagnostics["timedOut"], 0);
    assert_eq!(diagnostics["writerAcknowledged"], 0);
    assert_eq!(diagnostics["cleanupEnqueued"], 0);
    assert_eq!(diagnostics["cleanupAcknowledged"], 0);
    assert_eq!(diagnostics["cleanupStopped"], 0);
    assert!(diagnostics["cleanupStopReasons"].is_object());
    assert_eq!(
        diagnostics["admissionRejections"]["ui_input_projection_invalidated"],
        6
    );
    assert_eq!(
        diagnostics["admissionRejections"]["session_count_mismatch"],
        2
    );
}

/// 原始时间必须来自同一个 PresentedFrame；非法顺序仍保留诊断，不能被识别为有效采样。
///
/// @author kongweiguang
#[test]
fn raw_frame_samples_preserve_same_frame_and_report_invalid_timestamps() {
    let origin = Instant::now();
    let at = |nanos| origin + Duration::from_nanos(nanos);
    let window_id = WindowId::from(41);
    let frame = |dirty_at, draw_start, draw_end, present_start, present_end| PresentedFrame {
        frame: FrameTiming {
            window_id,
            dirty_at,
            invalidations: 1,
            draw_start,
            draw_end,
            refresh_interval: None,
            signal_at: None,
            phases: DrawPhases::default(),
        },
        presentation: PresentTiming {
            window_id,
            present_start,
            present_end,
            animation_interval: None,
        },
    };
    let mut samples = super::RawFrameSamples::new(origin);
    samples.push(frame(Some(at(1)), at(2), at(3), at(4), at(6)));
    samples.push(frame(Some(at(8)), at(7), at(9), at(10), at(11)));
    samples.push(frame(None, at(12), at(13), at(14), at(15)));
    let mut mismatched = frame(Some(at(16)), at(17), at(18), at(19), at(20));
    mismatched.presentation.window_id = WindowId::from(42);
    samples.push(mismatched);
    let report = serde_json::to_value(samples.finish()).unwrap();
    assert_eq!(report["dropped"], 0);
    assert_eq!(report["invalid"], 2);
    assert_eq!(report["samples"][0]["dirtyAtNs"], 1);
    assert_eq!(report["samples"][0]["drawStartNs"], 2);
    assert_eq!(report["samples"][0]["drawEndNs"], 3);
    assert_eq!(report["samples"][0]["presentStartNs"], 4);
    assert_eq!(report["samples"][0]["presentEndNs"], 6);
    assert!(report["samples"][2]["dirtyAtNs"].is_null());
    assert_eq!(report["samples"][3]["windowIdsMatch"], false);
    assert_eq!(report["samples"].as_array().unwrap().len(), 4);
}

/// journal 尾部必须由截止后的同窗帧形成 fence，迟到的截止前帧仍计入原始数据。
///
/// @author kongweiguang
#[test]
fn raw_frame_fence_includes_delayed_pre_boundary_frames_without_post_boundary_frames() {
    let origin = Instant::now();
    let at = |millis| origin + Duration::from_millis(millis);
    let window_id = WindowId::from(51);
    let presented = |id, start, end| {
        ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(PresentedFrame {
            frame: FrameTiming {
                window_id: id,
                dirty_at: Some(at(start)),
                invalidations: 1,
                draw_start: at(start),
                draw_end: at(start + 1),
                refresh_interval: None,
                signal_at: None,
                phases: DrawPhases::default(),
            },
            presentation: PresentTiming {
                window_id: id,
                present_start: at(start + 1),
                present_end: at(end),
                animation_interval: None,
            },
        }))
    };
    let mut accumulator = FramePhaseAccumulator {
        raw_frames: Some(super::RawFrameSamples::new(origin)),
        ..FramePhaseAccumulator::default()
    };
    accumulator.merge_fence_drained(
        DrainedEntries {
            entries: vec![presented(WindowId::from(52), 11, 13)],
            lost: 0,
        },
        window_id,
        at(10),
    );
    assert!(!accumulator.journal_fence_observed);
    accumulator.merge_fence_drained(
        DrainedEntries {
            entries: vec![presented(window_id, 3, 5), presented(window_id, 11, 13)],
            lost: 1,
        },
        window_id,
        at(10),
    );
    let summary = accumulator.finish();
    assert!(summary.journal_fence_observed);
    assert_eq!(summary.journal_fence_dirty_at_ns, Some(11_000_000));
    assert_eq!(summary.journal_fence_draw_start_ns, Some(11_000_000));
    assert_eq!(summary.journal_fence_present_end_ns, Some(13_000_000));
    assert_eq!(summary.journal_entries_lost, 1);
    assert_eq!(summary.presented_frame_count, 1);
    let raw = summary.raw_frames.unwrap();
    assert_eq!(raw.samples.len(), 1);
    assert_eq!(raw.samples[0].dirty_at_ns, Some(3_000_000));
    assert_eq!(raw.samples[0].present_end_ns, 5_000_000);
}

/// 电源事件的察觉时间不等于中断时间；截止后、fence 前后观察到的事件都须保守计入。
///
/// @author kongweiguang
#[test]
fn raw_frame_fence_counts_power_interruptions_observed_after_cutoff() {
    let origin = Instant::now();
    let at = |seconds| origin + Duration::from_secs(seconds);
    let window_id = WindowId::from(55);
    let power_transition = |ended_at| {
        ForegroundJournalEntry::Boundary(IntervalBoundary::PowerTransition {
            ended_at: at(ended_at),
        })
    };
    let fence = ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(PresentedFrame {
        frame: FrameTiming {
            window_id,
            dirty_at: Some(at(23)),
            invalidations: 1,
            draw_start: at(23),
            draw_end: at(24),
            refresh_interval: None,
            signal_at: None,
            phases: DrawPhases::default(),
        },
        presentation: PresentTiming {
            window_id,
            present_start: at(24),
            present_end: at(25),
            animation_interval: None,
        },
    }));
    let mut accumulator = FramePhaseAccumulator {
        raw_frames: Some(super::RawFrameSamples::new(origin)),
        ..FramePhaseAccumulator::default()
    };
    accumulator.merge_fence_drained(
        DrainedEntries {
            entries: vec![power_transition(19), power_transition(21)],
            lost: 0,
        },
        window_id,
        at(20),
    );
    assert!(!accumulator.journal_fence_observed);
    assert_eq!(accumulator.power_transitions, 2);

    // 同一批次中 fence 前后均已被观察，保守统计不能依赖事件在批次内的位置。
    accumulator.merge_fence_drained(
        DrainedEntries {
            entries: vec![power_transition(22), fence, power_transition(26)],
            lost: 0,
        },
        window_id,
        at(20),
    );
    let summary = accumulator.finish();
    assert!(summary.journal_fence_observed);
    assert_eq!(summary.journal_fence_draw_start_ns, Some(23_000_000_000));
    assert_eq!(summary.journal_entries_lost, 0);
    assert_eq!(summary.power_transitions, 4);
    assert_eq!(
        serde_json::to_value(&summary).unwrap()["powerTransitions"],
        4
    );
}

/// 不能假设采样回调绝不与同窗绘制跨界；延迟完成帧必须连同原始时间保留为不完整证据。
///
/// @author kongweiguang
#[test]
fn raw_frame_crossing_measurement_end_is_retained_even_with_later_fence() {
    let origin = Instant::now();
    let at = |seconds| origin + Duration::from_secs(seconds);
    let window_id = WindowId::from(54);
    let presented = |dirty, draw, draw_end, present_start, present_end| {
        ForegroundJournalEntry::Boundary(IntervalBoundary::Presented(PresentedFrame {
            frame: FrameTiming {
                window_id,
                dirty_at: Some(at(dirty)),
                invalidations: 1,
                draw_start: at(draw),
                draw_end: at(draw_end),
                refresh_interval: None,
                signal_at: None,
                phases: DrawPhases::default(),
            },
            presentation: PresentTiming {
                window_id,
                present_start: at(present_start),
                present_end: at(present_end),
                animation_interval: None,
            },
        }))
    };
    let mut accumulator = FramePhaseAccumulator {
        raw_frames: Some(super::RawFrameSamples::new(origin)),
        ..FramePhaseAccumulator::default()
    };
    accumulator.merge_fence_drained(
        DrainedEntries {
            entries: vec![presented(12, 19, 20, 20, 21), presented(22, 22, 22, 22, 23)],
            lost: 0,
        },
        window_id,
        at(20),
    );
    let summary = accumulator.finish();
    assert!(summary.journal_fence_observed);
    assert_eq!(summary.journal_fence_dirty_at_ns, Some(22_000_000_000));
    assert_eq!(summary.journal_cross_boundary_frames, 1);
    assert_eq!(summary.presented_frame_count, 1);
    let raw = summary.raw_frames.unwrap();
    assert_eq!(raw.samples.len(), 1);
    assert_eq!(raw.samples[0].dirty_at_ns, Some(12_000_000_000));
    assert_eq!(raw.samples[0].draw_start_ns, 19_000_000_000);
    assert_eq!(raw.samples[0].present_end_ns, 21_000_000_000);
}

/// 容量达到上限后必须显式累计丢帧，不能让保留前缀冒充整个测量窗口。
///
/// @author kongweiguang
#[test]
fn raw_frame_capacity_reports_every_unretained_frame() {
    let origin = Instant::now();
    let window_id = WindowId::from(53);
    let presented = PresentedFrame {
        frame: FrameTiming {
            window_id,
            dirty_at: Some(origin),
            invalidations: 1,
            draw_start: origin,
            draw_end: origin,
            refresh_interval: None,
            signal_at: None,
            phases: DrawPhases::default(),
        },
        presentation: PresentTiming {
            window_id,
            present_start: origin,
            present_end: origin,
            animation_interval: None,
        },
    };
    let mut samples = super::RawFrameSamples::new(origin);
    for _ in 0..=MAX_FRAME_PHASE_SAMPLES_PER_PHASE {
        samples.push(presented);
    }
    let report = samples.finish();
    assert_eq!(report.samples.len(), MAX_FRAME_PHASE_SAMPLES_PER_PHASE);
    assert_eq!(report.dropped, 1);
}

/// 原始 invalidation 可早于测量起点；负偏移及时钟缺口必须保留供后续门禁判断。
///
/// @author kongweiguang
#[test]
fn raw_frame_offsets_preserve_pre_origin_invalidation_and_missing_end() {
    let origin = Instant::now() + Duration::from_millis(5);
    assert_eq!(
        super::raw_frame_offset_ns(origin, origin - Duration::from_millis(2)),
        Some(-2_000_000)
    );
    let report = serde_json::to_value(super::RawFrameSamples::new(origin).finish()).unwrap();
    assert!(report["measurementEndNs"].is_null());
    assert!(report["measurementEndUnixMs"].is_null());
}
