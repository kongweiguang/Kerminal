//! 从真实 SessionService 样本生成可比较的后端 p95 汇总。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use serde::Serialize;

use super::measurements::SampleRecord;

/// 描述一个场景、操作和活动会话规模下的后端指标分布。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct BackendP95Summary {
    pub(in crate::acceptance) scenario: String,
    pub(in crate::acceptance) operation: String,
    pub(in crate::acceptance) active_sessions: usize,
    pub(in crate::acceptance) sample_count: usize,
    pub(in crate::acceptance) metrics: BTreeMap<String, MetricP95>,
}

/// 保存某一指标真实参与计算的样本数和 nearest-rank p95 值。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct MetricP95 {
    pub(in crate::acceptance) sample_count: usize,
    pub(in crate::acceptance) p95: f64,
}

/// 按场景、操作和同时活动会话数分组，避免把不同负载规模混成一个分位值。
pub(in crate::acceptance) fn summarize_backend_samples(
    samples: &[SampleRecord],
) -> Vec<BackendP95Summary> {
    let mut groups = BTreeMap::<(String, String, usize), Vec<&SampleRecord>>::new();
    for sample in samples {
        groups
            .entry((
                sample.scenario.clone(),
                sample.operation.clone(),
                sample.active_sessions,
            ))
            .or_default()
            .push(sample);
    }

    groups
        .into_iter()
        .map(|((scenario, operation, active_sessions), records)| {
            let mut metrics = BTreeMap::new();
            insert_p95(
                &mut metrics,
                "createLatencyMs",
                records.iter().filter_map(|record| record.create_latency_ms),
            );
            insert_p95(
                &mut metrics,
                "closeLatencyMs",
                records.iter().filter_map(|record| record.close_latency_ms),
            );
            insert_p95(
                &mut metrics,
                "ptyResponseLatencyMs",
                records
                    .iter()
                    .filter_map(|record| record.pty_response_latency_ms),
            );
            insert_p95(
                &mut metrics,
                "ptyOutputDurationMs",
                records
                    .iter()
                    .filter_map(|record| record.pty_output_duration_ms),
            );
            insert_p95(
                &mut metrics,
                "ptyOutputBytes",
                records
                    .iter()
                    .filter_map(|record| record.pty_output_bytes)
                    .map(|value| value as f64),
            );
            insert_p95(
                &mut metrics,
                "ptyOutputBytesPerSecond",
                records
                    .iter()
                    .filter_map(|record| record.pty_output_bytes_per_second),
            );
            insert_p95(
                &mut metrics,
                "processRssBytes",
                records
                    .iter()
                    .filter_map(|record| record.process_rss_bytes)
                    .map(|value| value as f64),
            );
            insert_p95(
                &mut metrics,
                "ptyChildrenRssBytes",
                records
                    .iter()
                    .filter_map(|record| record.pty_children_rss_bytes)
                    .map(|value| value as f64),
            );
            insert_p95(
                &mut metrics,
                "processCpuPercent",
                records
                    .iter()
                    .filter_map(|record| record.process_cpu_percent)
                    .map(f64::from),
            );
            insert_p95(
                &mut metrics,
                "ptyChildrenCpuPercent",
                records
                    .iter()
                    .filter_map(|record| record.pty_children_cpu_percent)
                    .map(f64::from),
            );
            insert_p95(
                &mut metrics,
                "hostCpuPercent",
                records
                    .iter()
                    .filter_map(|record| record.host_cpu_percent)
                    .map(f64::from),
            );
            insert_p95(
                &mut metrics,
                "totalMemoryBytes",
                records
                    .iter()
                    .filter_map(|record| record.total_memory_bytes)
                    .map(|value| value as f64),
            );
            insert_p95(
                &mut metrics,
                "ptyProcessCount",
                records
                    .iter()
                    .filter(|record| record.unavailable_process_metadata == 0)
                    .map(|record| record.pty_process_count as f64),
            );
            insert_p95(
                &mut metrics,
                "unavailableProcessMetadata",
                records
                    .iter()
                    .map(|record| record.unavailable_process_metadata as f64),
            );

            BackendP95Summary {
                scenario,
                operation,
                active_sessions,
                sample_count: records.len(),
                metrics,
            }
        })
        .collect()
}

/// 只汇总有限的实测值，并用 nearest-rank 选择已有样本而不插值或伪造零值。
fn insert_p95(
    metrics: &mut BTreeMap<String, MetricP95>,
    name: &str,
    values: impl Iterator<Item = f64>,
) {
    let mut values = values.filter(|value| value.is_finite()).collect::<Vec<_>>();
    if values.is_empty() {
        return;
    }
    values.sort_by(f64::total_cmp);
    let index = values.len() - values.len() / 20 - 1;
    metrics.insert(
        name.to_owned(),
        MetricP95 {
            sample_count: values.len(),
            p95: values[index],
        },
    );
}

#[cfg(test)]
#[path = "percentiles_tests.rs"]
mod tests;
