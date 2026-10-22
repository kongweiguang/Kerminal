//! JSON 与 CSV 证据序列化。
//!
//! @author kongweiguang

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::Serialize;

use super::arguments::Options;
use super::environment::SourceEnvironment;
use super::measurements::{FailureRecord, SampleRecord};
use super::percentiles::{BackendP95Summary, summarize_backend_samples};
/// 保存一次验收运行的主机、源码、参数与逐项测量结果。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct AcceptanceReport {
    pub(in crate::acceptance) schema_version: u32,
    pub(in crate::acceptance) run_id: String,
    pub(in crate::acceptance) status: &'static str,
    pub(in crate::acceptance) started_at_utc_unix_ms: u128,
    pub(in crate::acceptance) finished_at_utc_unix_ms: u128,
    pub(in crate::acceptance) source: SourceEnvironment,
    pub(in crate::acceptance) options: OptionRecord,
    pub(in crate::acceptance) ui_frame_latency_ms: Option<f64>,
    pub(in crate::acceptance) ui_frame_measurement: &'static str,
    pub(in crate::acceptance) notes: Vec<String>,
    pub(in crate::acceptance) failures: Vec<FailureRecord>,
    pub(in crate::acceptance) samples: Vec<SampleRecord>,
    pub(in crate::acceptance) temporary_directory_cleanup: String,
    pub(in crate::acceptance) retained_temporary_directory: Option<String>,
}

/// 在原始报告上附加由样本计算的汇总和解释边界，不修改报告内的原始数据。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AcceptanceEvidence<'a> {
    #[serde(flatten)]
    report: &'a AcceptanceReport,
    backend_p95: &'a [BackendP95Summary],
    measurement_boundary: MeasurementBoundary,
}

/// 明确统计口径，避免后端测量被误读成 UI 或 release 性能结论。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MeasurementBoundary {
    backend_p95_method: &'static str,
    backend_response_latency: &'static str,
    ui_frame_latency: &'static str,
    performance_profile: &'static str,
}

impl Default for MeasurementBoundary {
    /// 固定报告口径说明，使 JSON 消费者无需从源代码推断指标含义。
    fn default() -> Self {
        Self {
            backend_p95_method: "nearest-rank over finite raw samples, grouped by scenario, operation, and active SessionService count; each metric reports its own sample count",
            backend_response_latency: "PTY response latency is the SessionService write-to-plaintext-snapshot marker observation; it is not UI, physical keyboard, IME, or screen-presentation latency",
            ui_frame_latency: "not measured until a real GPUI Window profiler is connected; uiFrameLatencyMs remains null",
            performance_profile: "only profile=release is suitable for optimized performance comparison; debug, custom, and unknown profiles are not release performance evidence",
        }
    }
}

/// 记录可复现的压力参数，输出目录不参与配置读取。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct OptionRecord {
    pub(in crate::acceptance) session_counts: Vec<usize>,
    pub(in crate::acceptance) churn_cycles: usize,
    pub(in crate::acceptance) echo_samples_per_session_count: usize,
    pub(in crate::acceptance) mixed_duration_seconds: u64,
    pub(in crate::acceptance) mixed_sessions: usize,
    pub(in crate::acceptance) sample_interval_ms: u64,
    pub(in crate::acceptance) throughput_lines: usize,
    pub(in crate::acceptance) output_dir: String,
    pub(in crate::acceptance) shell_program: String,
}

impl OptionRecord {
    /// 将执行参数变成不含私密环境值的稳定报告契约。
    pub(in crate::acceptance) fn from_options(options: &Options, output_dir: &Path) -> Self {
        Self {
            session_counts: options.session_counts.clone(),
            churn_cycles: options.churn_cycles,
            echo_samples_per_session_count: options.echo_samples,
            mixed_duration_seconds: options.mixed_duration.as_secs(),
            mixed_sessions: options.mixed_sessions,
            sample_interval_ms: options.sample_interval.as_millis() as u64,
            throughput_lines: options.throughput_lines,
            output_dir: output_dir.display().to_string(),
            shell_program: options
                .shell_program
                .as_ref()
                .map(|program| program.to_string_lossy().into_owned())
                .unwrap_or_else(default_shell_name),
        }
    }
}
/// 生成默认 shell 名称供环境报告说明输入编码方式。
fn default_shell_name() -> String {
    #[cfg(windows)]
    {
        "powershell.exe".to_owned()
    }
    #[cfg(unix)]
    {
        "/bin/sh".to_owned()
    }
}

/// 将报告和逐项测量写入唯一文件名，保留以前验收运行的原始证据。
pub(in crate::acceptance) fn write_artifacts(
    output_dir: &Path,
    run_id: &str,
    report: &AcceptanceReport,
) -> io::Result<()> {
    fs::create_dir_all(output_dir)?;
    let json_path = output_dir.join(format!("session-acceptance-{run_id}.json"));
    let csv_path = output_dir.join(format!("session-acceptance-{run_id}.csv"));
    let p95_csv_path = output_dir.join(format!("session-acceptance-{run_id}-backend-p95.csv"));
    let backend_p95 = summarize_backend_samples(&report.samples);
    let evidence = AcceptanceEvidence {
        report,
        backend_p95: &backend_p95,
        measurement_boundary: MeasurementBoundary::default(),
    };
    let json = serde_json::to_vec_pretty(&evidence).map_err(io::Error::other)?;
    fs::write(json_path, json)?;
    write_samples_csv(csv_path, &report.samples)?;
    write_backend_p95_csv(p95_csv_path, &backend_p95)
}

/// 以标准 CSV 引号规则转义失败文本，并为 JSON 样本保留相同字段语义。
fn write_samples_csv(path: PathBuf, samples: &[SampleRecord]) -> io::Result<()> {
    let mut content = String::from(
        "scenario,operation,iteration,elapsed_ms,active_sessions,create_latency_ms,close_latency_ms,pty_response_latency_ms,pty_output_duration_ms,pty_output_bytes,pty_output_bytes_per_second,process_rss_bytes,pty_children_rss_bytes,process_cpu_percent,pty_children_cpu_percent,host_cpu_percent,total_memory_bytes,pty_process_count,unavailable_process_metadata\n",
    );
    for sample in samples {
        let values = [
            csv_cell(&sample.scenario),
            csv_cell(&sample.operation),
            sample.iteration.to_string(),
            sample.elapsed_ms.to_string(),
            sample.active_sessions.to_string(),
            csv_option(sample.create_latency_ms),
            csv_option(sample.close_latency_ms),
            csv_option(sample.pty_response_latency_ms),
            csv_option(sample.pty_output_duration_ms),
            csv_option(sample.pty_output_bytes),
            csv_option(sample.pty_output_bytes_per_second),
            csv_option(sample.process_rss_bytes),
            csv_option(sample.pty_children_rss_bytes),
            csv_option(sample.process_cpu_percent),
            csv_option(sample.pty_children_cpu_percent),
            csv_option(sample.host_cpu_percent),
            csv_option(sample.total_memory_bytes),
            sample.pty_process_count.to_string(),
            sample.unavailable_process_metadata.to_string(),
        ];
        content.push_str(&values.join(","));
        content.push('\n');
    }
    fs::write(path, content)
}

/// 将分组 p95 摊平成独立 CSV，避免把汇总值重复写进原始样本行。
fn write_backend_p95_csv(path: PathBuf, summaries: &[BackendP95Summary]) -> io::Result<()> {
    let mut content = String::from(
        "scenario,operation,active_sessions,group_sample_count,metric,metric_sample_count,p95\n",
    );
    for summary in summaries {
        for (metric, percentile) in &summary.metrics {
            let values = [
                csv_cell(&summary.scenario),
                csv_cell(&summary.operation),
                summary.active_sessions.to_string(),
                summary.sample_count.to_string(),
                csv_cell(metric),
                percentile.sample_count.to_string(),
                percentile.p95.to_string(),
            ];
            content.push_str(&values.join(","));
            content.push('\n');
        }
    }
    fs::write(path, content)
}

/// 序列化带逗号、引号或换行的文本字段，避免 CSV 行错位。
fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_owned()
    }
}

/// 使用空字段表达未采集值，不把不可用指标伪装成零。
fn csv_option<T: ToString>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}
