//! GPU telemetry parser contract tests using synthetic platform output.
//!
//! @author kongweiguang

#[cfg(target_os = "linux")]
use super::parse_nvidia_smi_gpus;
use super::parse_windows_gpu_json;

/// 验证 NVIDIA CLI 的 MiB、百分比和温度字段按实际单位映射。
#[cfg(target_os = "linux")]
#[test]
fn parses_nvidia_smi_metrics_and_unknown_values() {
    let gpus = parse_nvidia_smi_gpus(
        b"NVIDIA GeForce RTX 4060, 550.54, 8192, 1024, 42.56, 55\nNVIDIA GPU, N/A, N/A, N/A, N/A, N/A\n",
    );

    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 4060");
    assert_eq!(gpus[0].vendor.as_deref(), Some("NVIDIA"));
    assert_eq!(gpus[0].driver_version.as_deref(), Some("550.54"));
    assert_eq!(gpus[0].memory_total_bytes, Some(8_589_934_592));
    assert_eq!(gpus[0].memory_used_bytes, Some(1_073_741_824));
    assert_eq!(gpus[0].utilization_percent, Some(42.6));
    assert_eq!(gpus[0].temperature_celsius, Some(55.0));
    assert_eq!(gpus[1].memory_total_bytes, None);
    assert_eq!(gpus[1].utilization_percent, None);
}

/// 验证 Windows 旧有 WMI 与性能计数器字段均能从数字或 JSON null 安全读取。
#[test]
fn parses_windows_gpu_metrics_and_optional_values() {
    let gpus = parse_windows_gpu_json(
        br#"[{"Name":"NVIDIA GeForce RTX 4060","AdapterCompatibility":"NVIDIA","DriverVersion":"31.0.15","AdapterRAM":8589934592,"UtilizationPercent":42.25,"DedicatedUsageBytes":2147483648},{"Name":"Basic Display Adapter","AdapterRAM":null,"UtilizationPercent":null,"DedicatedUsageBytes":null}]"#,
    )
    .expect("synthetic Windows GPU JSON is valid");

    assert_eq!(gpus.len(), 2);
    assert_eq!(gpus[0].memory_total_bytes, Some(8_589_934_592));
    assert_eq!(gpus[0].memory_used_bytes, Some(2_147_483_648));
    assert_eq!(gpus[0].utilization_percent, Some(42.3));
    assert_eq!(gpus[1].memory_total_bytes, None);
    assert_eq!(gpus[1].utilization_percent, None);
}
