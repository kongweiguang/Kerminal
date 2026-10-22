//! SessionService 真实负载：并发规模、创建关闭 churn 与混合长稳场景。
//!
//! @author kongweiguang

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kerminal_application::sessions::SessionService;
use kerminal_domain::sessions::TerminalSize;
use tokio::time::Instant as TokioInstant;

use super::session_io::{create_session, run_echo_probe, run_throughput_probe};
use super::support::arguments::Options;
use super::support::measurements::{
    RawOutputCounter, SampleMeasure, SampleRecorder, append_sample, throughput,
};

/// 三类负载共享同一服务、CLI 参数和隔离目录，避免各场景入口漏传隔离边界。
///
/// @author kongweiguang
pub(super) struct SessionScenarioInputs<'a> {
    pub(super) service: &'a Arc<dyn SessionService>,
    pub(super) output_counter: &'a RawOutputCounter,
    pub(super) options: &'a Options,
    pub(super) temp_root: &'a Path,
    pub(super) config_root: &'a Path,
    pub(super) home_root: &'a Path,
    pub(super) temp_child_root: &'a Path,
}
/// 创建指定数量真实 SessionService 会话，采集 PTY 与资源数据后逐一关闭。
///
/// @author kongweiguang
pub(super) async fn run_count_scenario(
    inputs: &SessionScenarioInputs<'_>,
    recorder: &mut SampleRecorder<'_>,
    count: usize,
) -> Result<(), String> {
    let service = inputs.service;
    let output_counter = inputs.output_counter;
    let options = inputs.options;
    let temp_root = inputs.temp_root;
    let config_root = inputs.config_root;
    let home_root = inputs.home_root;
    let temp_child_root = inputs.temp_child_root;
    let scenario = format!("sessions-{count}");
    let mut sessions = Vec::with_capacity(count);
    for index in 0..count {
        let start = Instant::now();
        let info = create_session(
            service,
            options,
            temp_root,
            config_root,
            home_root,
            temp_child_root,
        )
        .await?;
        sessions.push(info.session_id);
        append_sample(
            recorder,
            &sessions,
            &scenario,
            "create",
            index + 1,
            SampleMeasure {
                create_latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    append_sample(
        recorder,
        &sessions,
        &scenario,
        "steady",
        count,
        SampleMeasure::default(),
    )
    .await;
    for iteration in 0..options.echo_samples {
        let session_id = &sessions[iteration % sessions.len()];
        let (latency, bytes) = run_echo_probe(service, output_counter, session_id).await?;
        append_sample(
            recorder,
            &sessions,
            &scenario,
            "pty-echo",
            iteration + 1,
            SampleMeasure {
                pty_response_latency_ms: Some(latency),
                pty_output_bytes: Some(bytes),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    let session_id = sessions
        .first()
        .ok_or_else(|| "会话规模场景未创建 session".to_owned())?;
    let (duration, bytes) = run_throughput_probe(
        service,
        output_counter,
        session_id,
        options.throughput_lines,
    )
    .await?;
    append_sample(
        recorder,
        &sessions,
        &scenario,
        "pty-throughput",
        options.throughput_lines,
        SampleMeasure {
            pty_output_duration_ms: Some(duration),
            pty_output_bytes: Some(bytes),
            pty_output_bytes_per_second: throughput(bytes, duration),
            ..SampleMeasure::default()
        },
    )
    .await;

    while let Some(session_id) = sessions.pop() {
        let start = Instant::now();
        service
            .close(session_id)
            .await
            .map_err(|error| format!("关闭 session 失败：{error}"))?;
        append_sample(
            recorder,
            &sessions,
            &scenario,
            "close",
            count - sessions.len(),
            SampleMeasure {
                close_latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    Ok(())
}

/// 循环创建和关闭真实 session，记录每次回收后的资源状态。
///
/// @author kongweiguang
pub(super) async fn run_churn_scenario(
    inputs: &SessionScenarioInputs<'_>,
    recorder: &mut SampleRecorder<'_>,
) -> Result<(), String> {
    let service = inputs.service;
    let options = inputs.options;
    let temp_root = inputs.temp_root;
    let config_root = inputs.config_root;
    let home_root = inputs.home_root;
    let temp_child_root = inputs.temp_child_root;
    for iteration in 0..options.churn_cycles {
        let create_start = Instant::now();
        let info = create_session(
            service,
            options,
            temp_root,
            config_root,
            home_root,
            temp_child_root,
        )
        .await?;
        let create_latency = create_start.elapsed().as_secs_f64() * 1000.0;
        let close_start = Instant::now();
        service
            .close(info.session_id)
            .await
            .map_err(|error| format!("第 {} 次关闭失败：{error}", iteration + 1))?;
        append_sample(
            recorder,
            &[],
            "churn",
            "create-close",
            iteration + 1,
            SampleMeasure {
                create_latency_ms: Some(create_latency),
                close_latency_ms: Some(close_start.elapsed().as_secs_f64() * 1000.0),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    Ok(())
}

/// 用固定数量的本地 PTY 执行可配置长时回显、输出与 resize 混合负载。
///
/// @author kongweiguang
pub(super) async fn run_mixed_scenario(
    inputs: &SessionScenarioInputs<'_>,
    recorder: &mut SampleRecorder<'_>,
) -> Result<(), String> {
    let service = inputs.service;
    let output_counter = inputs.output_counter;
    let options = inputs.options;
    let temp_root = inputs.temp_root;
    let config_root = inputs.config_root;
    let home_root = inputs.home_root;
    let temp_child_root = inputs.temp_child_root;
    let mut sessions = Vec::with_capacity(options.mixed_sessions);
    for index in 0..options.mixed_sessions {
        let start = Instant::now();
        let info = create_session(
            service,
            options,
            temp_root,
            config_root,
            home_root,
            temp_child_root,
        )
        .await?;
        sessions.push(info.session_id);
        append_sample(
            recorder,
            &sessions,
            "mixed",
            "create",
            index + 1,
            SampleMeasure {
                create_latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    let deadline = Instant::now() + options.mixed_duration;
    let interval = options.sample_interval;
    let throughput_period = Duration::from_secs(10);
    let mut next_tick = Instant::now();
    let mut next_throughput = Instant::now() + throughput_period;
    let mut next_resize = Instant::now() + Duration::from_secs(30);
    let mut resize_index = 0usize;
    let mut iteration = 0usize;
    while Instant::now() < deadline {
        let scheduled_tick = next_tick.min(deadline);
        if scheduled_tick > Instant::now() {
            tokio::time::sleep_until(TokioInstant::from_std(scheduled_tick)).await;
        }
        if Instant::now() >= deadline {
            break;
        }
        let session_id = &sessions[iteration % sessions.len()];
        let (latency, bytes) = run_echo_probe(service, output_counter, session_id).await?;
        append_sample(
            recorder,
            &sessions,
            "mixed",
            "pty-echo",
            iteration + 1,
            SampleMeasure {
                pty_response_latency_ms: Some(latency),
                pty_output_bytes: Some(bytes),
                ..SampleMeasure::default()
            },
        )
        .await;
        if Instant::now() >= next_throughput {
            let (duration, output_bytes) = run_throughput_probe(
                service,
                output_counter,
                session_id,
                options.throughput_lines,
            )
            .await?;
            append_sample(
                recorder,
                &sessions,
                "mixed",
                "pty-throughput",
                iteration + 1,
                SampleMeasure {
                    pty_output_duration_ms: Some(duration),
                    pty_output_bytes: Some(output_bytes),
                    pty_output_bytes_per_second: throughput(output_bytes, duration),
                    ..SampleMeasure::default()
                },
            )
            .await;
            while next_throughput <= Instant::now() {
                next_throughput += throughput_period;
            }
        }
        if Instant::now() >= next_resize {
            let (columns, rows) = if resize_index.is_multiple_of(2) {
                (100, 30)
            } else {
                (80, 24)
            };
            let size = TerminalSize::new(columns, rows, 0, 0).map_err(|error| error.to_string())?;
            service
                .resize(session_id.clone(), size)
                .await
                .map_err(|error| format!("mixed PTY resize 失败：{error}"))?;
            append_sample(
                recorder,
                &sessions,
                "mixed",
                "resize",
                iteration + 1,
                SampleMeasure::default(),
            )
            .await;
            resize_index += 1;
            while next_resize <= Instant::now() {
                next_resize += Duration::from_secs(30);
            }
        }
        iteration += 1;
        next_tick = next_tick.checked_add(interval).unwrap_or_else(Instant::now);
        if next_tick < Instant::now() {
            next_tick = Instant::now();
        }
    }
    while let Some(session_id) = sessions.pop() {
        let start = Instant::now();
        service
            .close(session_id)
            .await
            .map_err(|error| format!("关闭 mixed session 失败：{error}"))?;
        append_sample(
            recorder,
            &sessions,
            "mixed",
            "close",
            options.mixed_sessions - sessions.len(),
            SampleMeasure {
                close_latency_ms: Some(start.elapsed().as_secs_f64() * 1000.0),
                ..SampleMeasure::default()
            },
        )
        .await;
    }
    Ok(())
}
