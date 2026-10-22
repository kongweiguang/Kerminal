//! p95 汇总的独立回归用例。
//!
//! @author kongweiguang

use super::super::measurements::SampleRecord;
use super::summarize_backend_samples;

/// 构造不含 shell 正文的纯合成测量样本供分位算法测试。
fn sample(
    scenario: &str,
    operation: &str,
    active_sessions: usize,
    latency_ms: Option<f64>,
    throughput_bytes_per_second: Option<f64>,
) -> SampleRecord {
    SampleRecord {
        scenario: scenario.to_owned(),
        operation: operation.to_owned(),
        iteration: 1,
        elapsed_ms: 1,
        active_sessions,
        create_latency_ms: None,
        close_latency_ms: None,
        pty_response_latency_ms: latency_ms,
        pty_output_duration_ms: None,
        pty_output_bytes: None,
        pty_output_bytes_per_second: throughput_bytes_per_second,
        process_rss_bytes: None,
        pty_children_rss_bytes: None,
        process_cpu_percent: None,
        pty_children_cpu_percent: None,
        host_cpu_percent: None,
        total_memory_bytes: None,
        pty_process_count: 0,
        unavailable_process_metadata: 1,
    }
}

/// 验证 p95 使用 nearest-rank，并且不同活动会话规模不会相互污染。
#[test]
fn p95_uses_raw_nearest_rank_samples_per_load_group() {
    let mut samples = (1..=20)
        .map(|value| {
            sample(
                "sessions-10",
                "pty-echo",
                10,
                Some(value as f64),
                Some((value * 100) as f64),
            )
        })
        .collect::<Vec<_>>();
    samples.push(sample(
        "sessions-1",
        "pty-echo",
        1,
        Some(900.0),
        Some(9000.0),
    ));

    let summaries = summarize_backend_samples(&samples);
    let ten_session_group = summaries
        .iter()
        .find(|summary| summary.scenario == "sessions-10")
        .expect("ten-session group should exist");
    let latency = ten_session_group
        .metrics
        .get("ptyResponseLatencyMs")
        .expect("latency samples should be summarized");
    assert_eq!(ten_session_group.sample_count, 20);
    assert_eq!(latency.sample_count, 20);
    assert_eq!(latency.p95, 19.0);
    assert_eq!(
        ten_session_group
            .metrics
            .get("ptyOutputBytesPerSecond")
            .expect("throughput samples should be summarized")
            .p95,
        1900.0
    );

    let one_session_group = summaries
        .iter()
        .find(|summary| summary.scenario == "sessions-1")
        .expect("one-session group should exist");
    assert_eq!(
        one_session_group
            .metrics
            .get("ptyResponseLatencyMs")
            .expect("one-session latency should be summarized")
            .p95,
        900.0
    );
}

/// 缺失、非有限或无法确认的值不应变成零延迟、零吞吐或零 PTY 数量。
#[test]
fn unavailable_metrics_do_not_create_zero_p95_values() {
    let samples = vec![sample(
        "mixed",
        "pty-echo",
        10,
        Some(f64::NAN),
        Some(f64::INFINITY),
    )];

    let summaries = summarize_backend_samples(&samples);
    let metrics = &summaries[0].metrics;
    assert!(!metrics.contains_key("ptyResponseLatencyMs"));
    assert!(!metrics.contains_key("ptyOutputBytesPerSecond"));
    assert!(!metrics.contains_key("ptyProcessCount"));
    assert_eq!(
        metrics
            .get("unavailableProcessMetadata")
            .expect("unavailable metadata count remains reportable")
            .p95,
        1.0
    );
}

/// 验证吞吐探针总时长使用独立字段，不混入 echo 的 PTY 响应延迟分位。
#[test]
fn throughput_probe_duration_is_not_reported_as_echo_latency() {
    let mut throughput_sample = sample("mixed", "pty-throughput", 10, None, Some(4096.0));
    throughput_sample.pty_output_duration_ms = Some(12.5);

    let summaries = summarize_backend_samples(&[throughput_sample]);
    let metrics = &summaries[0].metrics;
    assert!(!metrics.contains_key("ptyResponseLatencyMs"));
    assert_eq!(
        metrics
            .get("ptyOutputDurationMs")
            .expect("throughput duration has its own metric")
            .p95,
        12.5
    );
}
