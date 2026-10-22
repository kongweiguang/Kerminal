//! 真实输出计数、PTY/进程采样和逐项测量模型。
//!
//! @author kongweiguang

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use super::report::AcceptanceReport;
use async_trait::async_trait;
use kerminal_application::sessions::SessionService;
use kerminal_application::sessions::logging::{
    OutputRecordResult, SessionLogState, SessionOutputRecorder,
};
use kerminal_domain::identity::SessionId;
use serde::Serialize;
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System,
};
/// 单条样本保留实际 PTY 原始输出计数，并将创建、关闭、回显和吞吐操作分开。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct SampleRecord {
    pub(in crate::acceptance) scenario: String,
    pub(in crate::acceptance) operation: String,
    pub(in crate::acceptance) iteration: usize,
    pub(in crate::acceptance) elapsed_ms: u128,
    pub(in crate::acceptance) active_sessions: usize,
    pub(in crate::acceptance) create_latency_ms: Option<f64>,
    pub(in crate::acceptance) close_latency_ms: Option<f64>,
    pub(in crate::acceptance) pty_response_latency_ms: Option<f64>,
    pub(in crate::acceptance) pty_output_duration_ms: Option<f64>,
    pub(in crate::acceptance) pty_output_bytes: Option<u64>,
    pub(in crate::acceptance) pty_output_bytes_per_second: Option<f64>,
    pub(in crate::acceptance) process_rss_bytes: Option<u64>,
    pub(in crate::acceptance) pty_children_rss_bytes: Option<u64>,
    pub(in crate::acceptance) process_cpu_percent: Option<f32>,
    pub(in crate::acceptance) pty_children_cpu_percent: Option<f32>,
    pub(in crate::acceptance) host_cpu_percent: Option<f32>,
    pub(in crate::acceptance) total_memory_bytes: Option<u64>,
    pub(in crate::acceptance) pty_process_count: usize,
    pub(in crate::acceptance) unavailable_process_metadata: usize,
}

/// 以稳定场景和操作标识记录失败，不把 shell 输出或用户配置写入报告。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::acceptance) struct FailureRecord {
    pub(in crate::acceptance) scenario: String,
    pub(in crate::acceptance) operation: String,
    pub(in crate::acceptance) iteration: Option<usize>,
    pub(in crate::acceptance) error: String,
}

/// 只累计真实 PTY reader 字节，不创建日志文件或保留终端正文。
#[derive(Default)]
pub(in crate::acceptance) struct RawOutputCounter {
    sessions: Mutex<HashMap<SessionId, Arc<AtomicU64>>>,
}

/// 刷新当前进程和 SessionService 返回的 PTY 子进程资源数据。
pub(in crate::acceptance) struct ResourceSampler {
    system: System,
    cpu_sample_ready: bool,
}

/// 每条样本中的系统资源部分；无法读取的 PID 保持为 None。
pub(in crate::acceptance) struct ResourceSnapshot {
    process_rss_bytes: Option<u64>,
    pty_children_rss_bytes: Option<u64>,
    process_cpu_percent: Option<f32>,
    pty_children_cpu_percent: Option<f32>,
    host_cpu_percent: Option<f32>,
    total_memory_bytes: Option<u64>,
    pty_process_count: usize,
    unavailable_process_metadata: usize,
}

/// 汇集同一轮验收的计时、资源采样器与报告，避免样本写入遗漏共享测量上下文。
///
/// @author kongweiguang
pub(in crate::acceptance) struct SampleRecorder<'a> {
    report: &'a mut AcceptanceReport,
    sampler: &'a mut ResourceSampler,
    service: &'a Arc<dyn SessionService>,
    run_started: Instant,
}

impl<'a> SampleRecorder<'a> {
    /// 借用整轮共用的采样资源，保持场景之间 CPU 预热与计时基线连续。
    ///
    /// @author kongweiguang
    pub(in crate::acceptance) fn new(
        service: &'a Arc<dyn SessionService>,
        sampler: &'a mut ResourceSampler,
        report: &'a mut AcceptanceReport,
        run_started: Instant,
    ) -> Self {
        Self {
            report,
            sampler,
            service,
            run_started,
        }
    }
}

/// 将样本测量值和实际可用的进程数据组合写入 JSON/CSV 共用记录。
///
/// @author kongweiguang
pub(in crate::acceptance) async fn append_sample(
    recorder: &mut SampleRecorder<'_>,
    sessions: &[SessionId],
    scenario: &str,
    operation: &str,
    iteration: usize,
    measure: SampleMeasure,
) {
    let resources = recorder.sampler.snapshot(recorder.service, sessions).await;
    recorder.report.samples.push(SampleRecord {
        scenario: scenario.to_owned(),
        operation: operation.to_owned(),
        iteration,
        elapsed_ms: recorder.run_started.elapsed().as_millis(),
        active_sessions: sessions.len(),
        create_latency_ms: measure.create_latency_ms,
        close_latency_ms: measure.close_latency_ms,
        pty_response_latency_ms: measure.pty_response_latency_ms,
        pty_output_duration_ms: measure.pty_output_duration_ms,
        pty_output_bytes: measure.pty_output_bytes,
        pty_output_bytes_per_second: measure.pty_output_bytes_per_second,
        process_rss_bytes: resources.process_rss_bytes,
        pty_children_rss_bytes: resources.pty_children_rss_bytes,
        process_cpu_percent: resources.process_cpu_percent,
        pty_children_cpu_percent: resources.pty_children_cpu_percent,
        host_cpu_percent: resources.host_cpu_percent,
        total_memory_bytes: resources.total_memory_bytes,
        pty_process_count: resources.pty_process_count,
        unavailable_process_metadata: resources.unavailable_process_metadata,
    });
}

/// 明确区分创建、关闭、PTY 响应和输出吞吐值，避免缺失样本被写为伪零。
#[derive(Default)]
pub(in crate::acceptance) struct SampleMeasure {
    pub(in crate::acceptance) create_latency_ms: Option<f64>,
    pub(in crate::acceptance) close_latency_ms: Option<f64>,
    pub(in crate::acceptance) pty_response_latency_ms: Option<f64>,
    pub(in crate::acceptance) pty_output_duration_ms: Option<f64>,
    pub(in crate::acceptance) pty_output_bytes: Option<u64>,
    pub(in crate::acceptance) pty_output_bytes_per_second: Option<f64>,
}
impl ResourceSampler {
    /// 先加载有限 CPU、内存和进程字段，避免读取用户命令行或进程环境。
    pub(in crate::acceptance) fn new() -> Self {
        let system = System::new_with_specifics(
            RefreshKind::new()
                .with_cpu(CpuRefreshKind::new().with_cpu_usage())
                .with_memory(MemoryRefreshKind::everything()),
        );
        Self {
            system,
            cpu_sample_ready: false,
        }
    }

    /// 仅查询 SessionService 明确返回的 PTY pid 与 runner 自身，不枚举其它进程。
    async fn snapshot(
        &mut self,
        service: &Arc<dyn SessionService>,
        sessions: &[SessionId],
    ) -> ResourceSnapshot {
        let own_pid = Pid::from_u32(std::process::id());
        let mut child_pids = Vec::with_capacity(sessions.len());
        let mut unavailable = 0usize;
        for session_id in sessions {
            match service.runtime_metadata(session_id.clone()).await {
                Ok(metadata) => match metadata.process_id {
                    Some(pid) => child_pids.push(Pid::from_u32(pid)),
                    None => unavailable += 1,
                },
                Err(_) => unavailable += 1,
            }
        }
        let mut process_ids = Vec::with_capacity(child_pids.len() + 1);
        process_ids.push(own_pid);
        process_ids.extend(child_pids.iter().copied());
        self.system
            .refresh_memory_specifics(MemoryRefreshKind::everything());
        self.system
            .refresh_cpu_specifics(CpuRefreshKind::new().with_cpu_usage());
        let cpu_sample_ready = self.cpu_sample_ready;
        self.cpu_sample_ready = true;
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&process_ids),
            ProcessRefreshKind::new().with_cpu().with_memory(),
        );
        let process = self.system.process(own_pid);
        let child_processes = child_pids
            .iter()
            .filter_map(|pid| self.system.process(*pid))
            .collect::<Vec<_>>();
        let children_measured = unavailable == 0 && child_processes.len() == child_pids.len();
        ResourceSnapshot {
            process_rss_bytes: process.map(|process| process.memory()),
            pty_children_rss_bytes: children_measured
                .then(|| child_processes.iter().map(|process| process.memory()).sum()),
            process_cpu_percent: cpu_sample_ready
                .then(|| process.map(|process| process.cpu_usage()))
                .flatten(),
            pty_children_cpu_percent: (children_measured && cpu_sample_ready).then(|| {
                child_processes
                    .iter()
                    .map(|process| process.cpu_usage())
                    .sum()
            }),
            host_cpu_percent: cpu_sample_ready.then(|| self.system.global_cpu_usage()),
            total_memory_bytes: reported_nonzero(self.system.total_memory()),
            pty_process_count: child_processes.len(),
            unavailable_process_metadata: unavailable,
        }
    }
}

/// 将 sysinfo 的未采集零值作为不可用，保留真实零内存与缺失采样的区别。
fn reported_nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

#[async_trait]
impl SessionOutputRecorder for RawOutputCounter {
    /// 只做短锁加原子计数，不排队磁盘 I/O，也不保留任何终端正文。
    fn try_record_output(
        &self,
        session_id: &SessionId,
        _sequence: u64,
        bytes: &[u8],
    ) -> OutputRecordResult {
        let counter = {
            let mut sessions = self
                .sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            sessions
                .entry(session_id.clone())
                .or_insert_with(|| Arc::new(AtomicU64::new(0)))
                .clone()
        };
        counter.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        OutputRecordResult::Ignored
    }

    /// 输出计数器没有持久 writer，关闭时只返回旧契约中的 inactive 状态。
    async fn finish_session(
        &self,
        _session_id: &SessionId,
        _deadline: Instant,
    ) -> kerminal_application::AppResult<SessionLogState> {
        Ok(SessionLogState::inactive())
    }

    /// 计数器不拥有后台任务，shutdown 无资源需要等待或释放。
    async fn shutdown_until(&self, _deadline: Instant) -> kerminal_application::AppResult<()> {
        Ok(())
    }
}

impl RawOutputCounter {
    /// 读取当前 session 的 reader 字节总数；未输出时返回真实的零计数。
    pub(in crate::acceptance) fn bytes(&self, session_id: &SessionId) -> u64 {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }
}

/// 将实际 PTY 输出字节数和命令端到端观察时间换算为 bytes/s。
pub(in crate::acceptance) fn throughput(bytes: u64, elapsed_ms: f64) -> Option<f64> {
    (elapsed_ms > 0.0).then(|| bytes as f64 * 1000.0 / elapsed_ms)
}

/// 以 UTC Unix epoch 毫秒记录绝对时间，不依赖系统区域设置或本地时区。
pub(in crate::acceptance) fn unix_time_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
