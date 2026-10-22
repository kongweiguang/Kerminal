//! 诊断运行健康采集与平台 GPU 白名单适配。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError, AppResult,
    diagnostics::{
        DiagnosticSourceStatus, DiagnosticUnavailableReason, RuntimeHealthAvailability,
        RuntimeHealthSnapshot, RuntimeProcessHealth, RuntimeSamplingInfo, RuntimeStorageHealth,
        RuntimeSystemHealth,
    },
};
use std::{
    thread,
    time::{SystemTime, UNIX_EPOCH},
};
use sysinfo::{
    CpuRefreshKind, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, RefreshKind,
    System,
};
use tokio::task;
use tokio_util::sync::CancellationToken;

use super::{
    APP_LOG_FILE_NAME, APP_LOG_MAX_FILE_SIZE_BYTES, APP_LOG_ROTATION_KEEP_FILES,
    LocalDiagnosticsAdapter,
    context::{diagnostic_root_label, directory_size, file_size},
    gpu::{LocalGpuProbeFailure, probe_local_gpus},
};

#[cfg(all(test, target_os = "linux"))]
use super::gpu::parse_nvidia_smi_gpus;
#[cfg(test)]
use super::gpu::parse_windows_gpu_json;

/// sysinfo 放入阻塞线程，GPU 指标则走与资源监控共用的短时平台探针。
pub(super) async fn collect(adapter: &LocalDiagnosticsAdapter) -> AppResult<RuntimeHealthSnapshot> {
    let blocking_adapter = adapter.clone();
    let mut snapshot = task::spawn_blocking(move || collect_blocking(&blocking_adapter))
        .await
        .map_err(|error| AppError::infrastructure_with_source("无法采集运行健康状态", error))??;
    let gpu_result = probe_local_gpus(adapter.operations.as_ref(), CancellationToken::new()).await;
    (snapshot.system.gpus, snapshot.availability.gpu) = match gpu_result {
        Ok(gpus) => (Some(gpus), DiagnosticSourceStatus::Available),
        Err(LocalGpuProbeFailure::Unavailable(reason)) => {
            (None, DiagnosticSourceStatus::Unavailable(reason))
        }
        Err(LocalGpuProbeFailure::TimedOut) => (
            None,
            DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::CollectionFailed),
        ),
    };

    Ok(snapshot)
}

/// 只刷新本进程的 CPU、内存和 I/O 字段，不请求进程参数、环境变量或工作目录。
fn collect_blocking(adapter: &LocalDiagnosticsAdapter) -> AppResult<RuntimeHealthSnapshot> {
    let pid = Pid::from_u32(std::process::id());
    let cpu_refresh = CpuRefreshKind::new().with_cpu_usage();
    let process_refresh = ProcessRefreshKind::new()
        .with_cpu()
        .with_memory()
        .with_disk_usage();
    let mut system = System::new_with_specifics(
        RefreshKind::new()
            .with_cpu(cpu_refresh)
            .with_memory(MemoryRefreshKind::everything()),
    );
    let process_ids = [pid];
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&process_ids), process_refresh);
    let sample_interval = sysinfo::MINIMUM_CPU_UPDATE_INTERVAL;
    thread::sleep(sample_interval);
    system.refresh_cpu_specifics(cpu_refresh);
    system.refresh_processes_specifics(ProcessesToUpdate::Some(&process_ids), process_refresh);

    let process = system
        .process(pid)
        .ok_or_else(|| AppError::infrastructure("当前进程状态不可读取"))?;
    let process_disk = process.disk_usage();
    let process_health = RuntimeProcessHealth {
        pid: std::process::id(),
        name: process.name().to_string_lossy().into_owned(),
        memory_bytes: Some(process.memory()),
        virtual_memory_bytes: Some(process.virtual_memory()),
        cpu_usage_percent: finite_percent(process.cpu_usage()),
        started_at_seconds: Some(process.start_time()),
        uptime_seconds: Some(process.run_time()),
        disk_read_bytes: Some(process_disk.total_read_bytes),
        disk_written_bytes: Some(process_disk.total_written_bytes),
    };

    let (gpus, gpu_status) = (
        None,
        DiagnosticSourceStatus::Unavailable(DiagnosticUnavailableReason::Unsupported),
    );
    let (storage, storage_status, database_status, logs_status) = collect_storage_health(adapter);
    let core_usage = system
        .cpus()
        .iter()
        .map(|cpu| finite_percent(cpu.cpu_usage()))
        .collect::<Option<Vec<_>>>();
    let core_count = (!system.cpus().is_empty()).then_some(system.cpus().len());

    Ok(RuntimeHealthSnapshot {
        captured_at: unix_timestamp_string(),
        process: process_health,
        system: RuntimeSystemHealth {
            os: System::long_os_version()
                .or_else(System::name)
                .unwrap_or_else(|| std::env::consts::OS.to_owned()),
            arch: std::env::consts::ARCH.to_owned(),
            host_name: System::host_name(),
            kernel_version: System::kernel_version(),
            os_version: System::os_version(),
            cpu_count: core_count,
            global_cpu_usage_percent: finite_percent(system.global_cpu_usage()),
            cpu_core_usage_percents: core_usage,
            total_memory_bytes: nonzero(system.total_memory()),
            used_memory_bytes: nonzero(system.used_memory()),
            available_memory_bytes: nonzero(system.available_memory()),
            total_swap_bytes: nonzero(system.total_swap()),
            used_swap_bytes: nonzero(system.used_swap()),
            uptime_seconds: Some(System::uptime()),
            boot_time_seconds: Some(System::boot_time()),
            gpus,
        },
        storage,
        sampling: RuntimeSamplingInfo {
            source: "sysinfo::System".to_owned(),
            cpu_sample_interval_ms: sample_interval.as_millis() as u64,
            cpu_refreshed_twice: true,
        },
        availability: RuntimeHealthAvailability {
            process: DiagnosticSourceStatus::Available,
            system: DiagnosticSourceStatus::Available,
            gpu: gpu_status,
            storage: storage_status,
            command_database: database_status,
            app_logs: logs_status,
        },
        redacted: false,
    })
}

/// 以独立状态记录根目录、SQLite 与日志探测，避免一处缺失掩盖其他来源的真实结果。
fn collect_storage_health(
    adapter: &LocalDiagnosticsAdapter,
) -> (
    RuntimeStorageHealth,
    DiagnosticSourceStatus,
    DiagnosticSourceStatus,
    DiagnosticSourceStatus,
) {
    let root = adapter.storage.root();
    let database_file = adapter.command_history.database_file();
    let log_file = root.join("logs").join(APP_LOG_FILE_NAME);
    let root_size = directory_size(root);
    let database_size = file_size(database_file);
    let database_schema = match database_size {
        Ok(_) => adapter
            .command_history
            .schema_version()
            .map_err(|_| DiagnosticUnavailableReason::CollectionFailed),
        Err(reason) => Err(reason),
    };
    let log_size = file_size(&log_file);
    let storage_status = source_status(root_size.as_ref().map(|_| ()).map_err(|reason| *reason));
    let database_status = source_status(match (database_size, database_schema) {
        (Ok(_), Ok(_)) => Ok(()),
        (Err(reason), _) | (_, Err(reason)) => Err(reason),
    });
    let logs_status = source_status(log_size.as_ref().map(|_| ()).map_err(|reason| *reason));
    let label = diagnostic_root_label();

    (
        RuntimeStorageHealth {
            root: label.to_owned(),
            command_database_file: format!("{label}/data/command.sqlite"),
            app_log_file: format!("{label}/logs/{APP_LOG_FILE_NAME}"),
            logs: format!("{label}/logs"),
            diagnostics: format!("{label}/diagnostics"),
            root_size_bytes: root_size.ok(),
            command_database_file_size_bytes: database_size.ok(),
            app_log_file_size_bytes: log_size.ok(),
            app_log_max_file_size_bytes: Some(APP_LOG_MAX_FILE_SIZE_BYTES),
            app_log_rotation_keep_files: Some(APP_LOG_ROTATION_KEEP_FILES),
        },
        storage_status,
        database_status,
        logs_status,
    )
}

/// 将采集结果折叠为稳定的有限状态，不把底层路径或错误消息放进报告。
fn source_status(result: Result<(), DiagnosticUnavailableReason>) -> DiagnosticSourceStatus {
    match result {
        Ok(()) => DiagnosticSourceStatus::Available,
        Err(reason) => DiagnosticSourceStatus::Unavailable(reason),
    }
}

/// 丢弃 NaN 与无穷值，防止非 JSON 数值进入协议响应或 ZIP 报告。
fn finite_percent(value: f32) -> Option<f32> {
    value.is_finite().then(|| (value * 10.0).round() / 10.0)
}

/// sysinfo 将无可用容量表示为 0；把它转成 None，避免把不支持误报成真实空闲状态。
fn nonzero(value: u64) -> Option<u64> {
    (value > 0).then_some(value)
}

/// 兼容旧诊断包的 Unix 秒时间戳；时钟无效时显式标为 unknown。
fn unix_timestamp_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
#[path = "../../tests/support/diagnostics_health.rs"]
mod tests;
