//! Linux 进程投影保留总进程数与正在运行数的独立语义。
//!
//! @author kongweiguang

use super::probes::{failure_status, parse_lspci_gpus, parse_npu_smi_info, parse_nvidia_smi_csv};
use super::*;
use kerminal_domain::operations::ServerInfoProbeFailure;

/// 本机 uptime 只属于 Fast/Full 采样，避免 Slow/Static 单独请求伪装成新鲜 Fast 值。
///
/// @author kongweiguang
#[test]
fn local_uptime_is_present_only_in_fast_tiers() {
    for tier in [ServerInfoTier::Slow, ServerInfoTier::Static] {
        let snapshot = collect_local_snapshot(tier).unwrap();
        assert_eq!(snapshot.uptime_seconds, None, "{tier:?}");
    }
    for tier in [ServerInfoTier::Fast, ServerInfoTier::Full] {
        let snapshot = collect_local_snapshot(tier).unwrap();
        assert!(snapshot.uptime_seconds.is_some(), "{tier:?}");
    }
}

/// 固定 CSV 字段恢复 NVIDIA 指标，N/A 保持未知而不是变成数值零。
#[test]
fn nvidia_smi_csv_preserves_optional_metrics() {
    let gpus =
        parse_nvidia_smi_csv("NVIDIA RTX 4090, 555.42, 24576, 6144, 36.5, 54, 220.1\n").unwrap();

    assert_eq!(gpus.len(), 1);
    assert_eq!(gpus[0].vendor.as_deref(), Some("NVIDIA"));
    assert_eq!(gpus[0].driver_version.as_deref(), Some("555.42"));
    assert_eq!(gpus[0].memory_total_bytes, Some(24 * 1024 * 1024 * 1024));
    assert_eq!(gpus[0].utilization_percent, Some(36.5));
    assert_eq!(gpus[0].temperature_celsius, Some(54.0));
    assert_eq!(gpus[0].power_watts, Some(220.1));

    let partial = parse_nvidia_smi_csv("NVIDIA device, N/A, N/A, N/A, N/A, N/A\n").unwrap();
    assert!(partial[0].memory_total_bytes.is_none());
    assert!(partial[0].utilization_percent.is_none());
}

/// lspci 回退只提供识别到的设备名称，不伪造运行中利用率或温度。
#[test]
fn lspci_fallback_reports_device_identity_only() {
    let gpus = parse_lspci_gpus(
        "01:00.0 VGA compatible controller: NVIDIA Corporation AD102 [GeForce RTX 4090]\n02:00.0 Ethernet controller: Intel\n",
    );

    assert_eq!(gpus.len(), 1);
    assert_eq!(gpus[0].id, "01:00.0");
    assert_eq!(gpus[0].vendor.as_deref(), Some("NVIDIA"));
    assert!(
        gpus[0]
            .model
            .as_deref()
            .unwrap()
            .contains("GeForce RTX 4090")
    );
    assert!(gpus[0].utilization_percent.is_none());
}

/// Ascend 多行设备输出会把健康、chip/bus、AI Core 与 HBM 映射到同一设备。
#[test]
fn npu_smi_info_parses_device_and_chip_rows() {
    let output =
        "| 2 910B4 | OK | 84 W 44 C |\n| 0 | 0000:06:00.0 | 0 %, 4 / 8 MB, 2657 / 32768 MB |\n";
    let npus = parse_npu_smi_info(output).unwrap();

    assert_eq!(npus.len(), 1);
    assert_eq!(npus[0].id, "2:0");
    assert_eq!(npus[0].chip_id, Some(0));
    assert_eq!(npus[0].model.as_deref(), Some("910B4"));
    assert_eq!(npus[0].health.as_deref(), Some("OK"));
    assert_eq!(npus[0].bus_id.as_deref(), Some("0000:06:00.0"));
    assert_eq!(npus[0].aicore_percent, Some(0.0));
    assert_eq!(npus[0].hbm_used_bytes, Some(2657 * 1024 * 1024));
    assert_eq!(npus[0].hbm_total_bytes, Some(32768 * 1024 * 1024));
    assert_eq!(npus[0].power_watts, Some(84.0));
    assert_eq!(npus[0].temperature_celsius, Some(44.0));
}

/// 探针退出类型只投影稳定原因，不把远端错误输出带进 UI。
#[test]
fn optional_probe_failure_is_classified_without_raw_output() {
    assert_eq!(
        failure_status(&AppError::remote_exit(Some(126))),
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::PermissionDenied)
    );
    assert_eq!(
        failure_status(&AppError::timeout("GPU probe")),
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::TimedOut)
    );
}

/// 本机 GPU 复用诊断字段时保留已知指标，未知功耗保持 None。
#[test]
fn local_gpu_projection_preserves_only_measured_values() {
    let snapshots = runtime_gpus_to_snapshots(vec![RuntimeGpuHealth {
        name: "NVIDIA RTX fixture".to_owned(),
        vendor: Some("NVIDIA".to_owned()),
        driver_version: Some("555.42".to_owned()),
        memory_total_bytes: Some(8_192),
        memory_used_bytes: Some(2_048),
        utilization_percent: Some(42.5),
        temperature_celsius: Some(55.0),
    }]);

    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].id, "0");
    assert_eq!(snapshots[0].model.as_deref(), Some("NVIDIA RTX fixture"));
    assert_eq!(snapshots[0].driver_version.as_deref(), Some("555.42"));
    assert_eq!(snapshots[0].memory_total_bytes, Some(8_192));
    assert_eq!(snapshots[0].utilization_percent, Some(42.5));
    assert_eq!(snapshots[0].power_watts, None);
}

/// GPU 不可用原因使用固定分类，防止本机探针文本进入运行态结果。
#[test]
fn local_gpu_failure_maps_to_stable_probe_status() {
    assert_eq!(
        unavailable_gpu_status(crate::diagnostics::gpu::LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::PermissionDenied,
        )),
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::PermissionDenied)
    );
    assert_eq!(
        unavailable_gpu_status(crate::diagnostics::gpu::LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::CollectionFailed,
        )),
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::CommandFailed)
    );
    assert_eq!(
        unavailable_gpu_status(crate::diagnostics::gpu::LocalGpuProbeFailure::TimedOut),
        ServerInfoProbeStatus::Unavailable(ServerInfoProbeFailure::TimedOut)
    );
}

/// ps 的 R/R+ 才计入运行数，sleeping 和 stopped 进程仍计入总数。
#[test]
fn linux_process_summary_counts_running_state_separately() {
    let output = "101 R 1.5 2.0 64 bash\n102 S 0.0 0.5 32 sshd: worker\n103 R+ 3.0 1.0 16 top\n104 T 0.0 0.1 8 stopped\ninvalid row\n";

    let (process_count, running_count, processes) = parse_linux_processes(output);

    assert_eq!(process_count, Some(4));
    assert_eq!(running_count, Some(2));
    assert_eq!(processes.len(), 4);
    assert_eq!(
        processes
            .iter()
            .find(|process| process.pid == 102)
            .unwrap()
            .name,
        "sshd: worker"
    );
    assert_eq!(
        processes
            .iter()
            .find(|process| process.pid == 101)
            .unwrap()
            .memory_bytes,
        Some(64 * 1024)
    );
}

/// uptime parser floor-quantizes valid kernel seconds and leaves malformed/negative values unknown.
///
/// @author kongweiguang
#[test]
fn linux_uptime_parser_keeps_invalid_samples_unknown() {
    assert_eq!(parse_linux_uptime("3723.875 931.25\n"), Some(3_723));
    assert_eq!(parse_linux_uptime("0.9 0.2"), Some(0));
    assert_eq!(parse_linux_uptime(""), None);
    assert_eq!(parse_linux_uptime("-1.0 2.0"), None);
    assert_eq!(parse_linux_uptime("NaN 2.0"), None);
    assert_eq!(parse_linux_uptime("not-uptime"), None);
}

/// meminfo 的 Cached 原样换算字节；旧内核没有该行时维持未知且不影响必需字段。
///
/// @author kongweiguang
#[test]
fn linux_memory_parser_keeps_cached_optional() {
    let with_cached = parse_linux_memory(
        "MemTotal: 32768 kB\nMemAvailable: 16384 kB\nCached: 1234 kB\nSwapTotal: 4096 kB\nSwapFree: 1024 kB\n",
    )
    .unwrap();
    assert_eq!(with_cached.cached_bytes, Some(1_234 * 1024));
    assert_eq!(with_cached.total_bytes, Some(32_768 * 1024));

    let legacy =
        parse_linux_memory("MemTotal: 2048 kB\nMemFree: 512 kB\nCached: malformed kB\n").unwrap();
    assert_eq!(legacy.cached_bytes, None);
    assert_eq!(legacy.available_bytes, Some(512 * 1024));
}
