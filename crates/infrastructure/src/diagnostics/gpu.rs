//! 本机 GPU 探测共享实现，供运行诊断与服务器资源页复用。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_application::{
    PublicErrorCode,
    diagnostics::{DiagnosticUnavailableReason, RuntimeGpuHealth},
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation,
        OperationsCommandExecutor,
    },
};
use tokio_util::sync::CancellationToken;

#[cfg(target_os = "linux")]
use super::context::unavailable_for_io;
#[cfg(target_os = "linux")]
use std::{fs, path::Path};

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
const GPU_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
const GPU_PROBE_MAX_OUTPUT_BYTES: usize = 128 * 1024;

/// 保留超时语义供资源页展示，同时兼容诊断契约较粗的 unavailable 分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalGpuProbeFailure {
    Unavailable(DiagnosticUnavailableReason),
    TimedOut,
}

/// 让各消费者共用同一平台探测与命令执行路径，避免重复启动 GPU 工具或收集无关指标。
pub(crate) async fn probe_local_gpus(
    executor: &dyn OperationsCommandExecutor,
    cancellation: CancellationToken,
) -> Result<Vec<RuntimeGpuHealth>, LocalGpuProbeFailure> {
    if cancellation.is_cancelled() {
        return Err(LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::CollectionFailed,
        ));
    }

    #[cfg(target_os = "linux")]
    {
        let (mut gpus, mut failure) = match probe_sysfs_gpus() {
            Ok(gpus) => (gpus, None),
            Err(reason) => (Vec::new(), Some(LocalGpuProbeFailure::Unavailable(reason))),
        };
        match run_bounded_command(
            executor,
            "nvidia-smi",
            &[
                "--query-gpu=name,driver_version,memory.total,memory.used,utilization.gpu,temperature.gpu",
                "--format=csv,noheader,nounits",
            ],
            cancellation,
        )
        .await
        {
            Ok(output) => {
                let nvidia_gpus = parse_nvidia_smi_gpus(&output);
                if !nvidia_gpus.is_empty() {
                    gpus.retain(|gpu| gpu.vendor.as_deref() != Some("NVIDIA"));
                    gpus.extend(nvidia_gpus);
                }
            }
            Err(reason) => {
                failure.get_or_insert(reason);
            }
        };
        if gpus.is_empty() {
            Err(failure.unwrap_or(LocalGpuProbeFailure::Unavailable(
                DiagnosticUnavailableReason::NotFound,
            )))
        } else {
            Ok(gpus)
        }
    }

    #[cfg(target_os = "windows")]
    {
        probe_windows_gpus(executor, cancellation).await
    }

    #[cfg(target_os = "macos")]
    {
        probe_macos_gpus(executor, cancellation).await
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        let _ = (executor, cancellation);
        Err(LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::Unsupported,
        ))
    }
}

#[cfg(target_os = "linux")]
/// 通过 Linux DRM sysfs 识别显示设备；缺少厂商 CLI 时仍可保留设备身份。
fn probe_sysfs_gpus() -> Result<Vec<RuntimeGpuHealth>, DiagnosticUnavailableReason> {
    let drm_root = Path::new("/sys/class/drm");
    let mut gpus = Vec::new();
    let mut probe_failure = None;

    match fs::read_dir(drm_root) {
        Ok(entries) => {
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        probe_failure = Some(unavailable_for_io(error));
                        continue;
                    }
                };
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.strip_prefix("card").is_some_and(|suffix| {
                    !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
                }) {
                    continue;
                }

                let device = entry.path().join("device");
                let vendor_id = read_trimmed(&device.join("vendor"));
                let device_id = read_trimmed(&device.join("device"));
                let vendor = vendor_id.as_deref().and_then(vendor_name);
                let model = match (vendor_id.as_deref(), device_id.as_deref()) {
                    (Some(vendor_id), Some(device_id)) => {
                        format!("{} [{vendor_id}:{device_id}]", vendor.unwrap_or("GPU"))
                    }
                    (_, Some(device_id)) => format!("GPU [{device_id}]"),
                    _ => "GPU".to_owned(),
                };
                gpus.push(RuntimeGpuHealth {
                    name: model,
                    vendor: vendor.map(str::to_owned),
                    driver_version: None,
                    memory_total_bytes: None,
                    memory_used_bytes: None,
                    utilization_percent: None,
                    temperature_celsius: None,
                });
            }
        }
        Err(error) => probe_failure = Some(unavailable_for_io(error)),
    }

    if gpus.is_empty() {
        Err(probe_failure.unwrap_or(DiagnosticUnavailableReason::NotFound))
    } else {
        Ok(gpus)
    }
}

#[cfg(target_os = "windows")]
/// 复用旧版只读 WMI 与性能计数器探测，不将其扩展成进程或存储全量诊断。
async fn probe_windows_gpus(
    executor: &dyn OperationsCommandExecutor,
    cancellation: CancellationToken,
) -> Result<Vec<RuntimeGpuHealth>, LocalGpuProbeFailure> {
    let script = r#"
$controllers = @(Get-CimInstance Win32_VideoController |
  Select-Object Name,AdapterCompatibility,DriverVersion,AdapterRAM)
$usageByGpu = @{}
$totalUsage = 0.0
try {
  $samples = (Get-Counter '\GPU Engine(*)\Utilization Percentage' -ErrorAction Stop).CounterSamples
  foreach ($sample in $samples) {
    $value = [double]$sample.CookedValue
    $totalUsage += $value
    if ($sample.InstanceName -match 'phys_(\d+)') {
      $index = [int]$Matches[1]
      if (-not $usageByGpu.ContainsKey($index)) { $usageByGpu[$index] = 0.0 }
      $usageByGpu[$index] += $value
    }
  }
} catch {}
$dedicatedUsageByGpu = @{}
try {
  $samples = (Get-Counter '\GPU Adapter Memory(*)\Dedicated Usage' -ErrorAction Stop).CounterSamples
  foreach ($sample in $samples) {
    if ($sample.InstanceName -match 'phys_(\d+)') {
      $index = [int]$Matches[1]
      if (-not $dedicatedUsageByGpu.ContainsKey($index)) { $dedicatedUsageByGpu[$index] = 0.0 }
      $dedicatedUsageByGpu[$index] += [double]$sample.CookedValue
    }
  }
} catch {}
$items = for ($i = 0; $i -lt $controllers.Count; $i++) {
  $controller = $controllers[$i]
  $usage = $null
  if ($usageByGpu.ContainsKey($i)) {
    $usage = [math]::Min(100.0, [math]::Round([double]$usageByGpu[$i], 1))
  } elseif ($controllers.Count -eq 1 -and $totalUsage -gt 0) {
    $usage = [math]::Min(100.0, [math]::Round($totalUsage, 1))
  }
  $dedicatedUsage = $null
  if ($dedicatedUsageByGpu.ContainsKey($i)) {
    $dedicatedUsage = [uint64][math]::Max(0.0, [math]::Round([double]$dedicatedUsageByGpu[$i], 0))
  }
  [pscustomobject]@{
    Name = $controller.Name
    AdapterCompatibility = $controller.AdapterCompatibility
    DriverVersion = $controller.DriverVersion
    AdapterRAM = $controller.AdapterRAM
    UtilizationPercent = $usage
    DedicatedUsageBytes = $dedicatedUsage
  }
}
$items | ConvertTo-Json -Compress
"#;
    let output = run_bounded_command(
        executor,
        "powershell.exe",
        &["-NoProfile", "-NonInteractive", "-Command", script],
        cancellation,
    )
    .await?;
    parse_windows_gpu_json(&output).map_err(LocalGpuProbeFailure::Unavailable)
}

#[cfg(any(target_os = "windows", test))]
/// 将 Windows WMI 与性能计数器合并的 JSON 映射为稳定 GPU 数据。
pub(super) fn parse_windows_gpu_json(
    output: &[u8],
) -> Result<Vec<RuntimeGpuHealth>, DiagnosticUnavailableReason> {
    let value: serde_json::Value =
        serde_json::from_slice(output).map_err(|_| DiagnosticUnavailableReason::InvalidData)?;
    let records = match &value {
        serde_json::Value::Array(records) => records.iter().collect::<Vec<_>>(),
        serde_json::Value::Object(_) => vec![&value],
        serde_json::Value::Null => Vec::new(),
        _ => return Err(DiagnosticUnavailableReason::InvalidData),
    };
    let gpus = records
        .into_iter()
        .filter_map(|record| {
            let name = json_string(record, "Name")?;
            Some(RuntimeGpuHealth {
                name,
                vendor: json_string(record, "AdapterCompatibility"),
                driver_version: json_string(record, "DriverVersion"),
                memory_total_bytes: json_u64(record, "AdapterRAM"),
                memory_used_bytes: json_u64(record, "DedicatedUsageBytes"),
                utilization_percent: json_f32(record, "UtilizationPercent")
                    .map(|value| (value.clamp(0.0, 100.0) * 10.0).round() / 10.0),
                temperature_celsius: None,
            })
        })
        .collect::<Vec<_>>();
    if gpus.is_empty() {
        Err(DiagnosticUnavailableReason::NotFound)
    } else {
        Ok(gpus)
    }
}

#[cfg(target_os = "macos")]
/// 使用系统报告读取 macOS 显示设备摘要，缺失信息保持为空。
async fn probe_macos_gpus(
    executor: &dyn OperationsCommandExecutor,
    cancellation: CancellationToken,
) -> Result<Vec<RuntimeGpuHealth>, LocalGpuProbeFailure> {
    let output = run_bounded_command(
        executor,
        "/usr/sbin/system_profiler",
        &["-json", "SPDisplaysDataType"],
        cancellation,
    )
    .await?;
    let value: serde_json::Value = serde_json::from_slice(&output)
        .map_err(|_| LocalGpuProbeFailure::Unavailable(DiagnosticUnavailableReason::InvalidData))?;
    let records = value
        .get("SPDisplaysDataType")
        .and_then(serde_json::Value::as_array)
        .ok_or(LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::InvalidData,
        ))?;
    let gpus = records
        .iter()
        .filter_map(|record| {
            let name =
                json_string(record, "sppci_model").or_else(|| json_string(record, "_name"))?;
            Some(RuntimeGpuHealth {
                name,
                vendor: json_string(record, "sppci_vendor"),
                driver_version: None,
                memory_total_bytes: record.get("spdisplays_vram").and_then(value_to_bytes),
                memory_used_bytes: None,
                utilization_percent: None,
                temperature_celsius: None,
            })
        })
        .collect::<Vec<_>>();
    if gpus.is_empty() {
        Err(LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::NotFound,
        ))
    } else {
        Ok(gpus)
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
/// 通过共享运维执行器运行固定 argv，并交由其生命周期策略清理超时或取消的进程。
async fn run_bounded_command(
    executor: &dyn OperationsCommandExecutor,
    program: &str,
    arguments: &[&str],
    cancellation: CancellationToken,
) -> Result<Vec<u8>, LocalGpuProbeFailure> {
    let invocation = CommandInvocation::new(
        program,
        arguments
            .iter()
            .map(|argument| (*argument).to_owned())
            .collect(),
    );
    let result = executor
        .execute(
            CommandExecutionTarget::Local,
            invocation,
            CommandExecutionLimits {
                timeout: GPU_PROBE_TIMEOUT,
                max_output_bytes: GPU_PROBE_MAX_OUTPUT_BYTES,
            },
            cancellation,
        )
        .await
        .map_err(|error| match error.public_code() {
            PublicErrorCode::NotFound => {
                LocalGpuProbeFailure::Unavailable(DiagnosticUnavailableReason::NotFound)
            }
            PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
                LocalGpuProbeFailure::Unavailable(DiagnosticUnavailableReason::Unsupported)
            }
            PublicErrorCode::Timeout => LocalGpuProbeFailure::TimedOut,
            _ => LocalGpuProbeFailure::Unavailable(DiagnosticUnavailableReason::CollectionFailed),
        })?;

    if result.exit_code != Some(0) || result.truncated {
        return Err(LocalGpuProbeFailure::Unavailable(
            DiagnosticUnavailableReason::CollectionFailed,
        ));
    }
    Ok(result.stdout)
}

#[cfg(target_os = "linux")]
/// 读取 sysfs 的短标识字段，缺失时留空而不记录设备路径。
fn read_trimmed(path: &Path) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

#[cfg(target_os = "linux")]
/// 将 PCI vendor id 映射为稳定厂商名，未知厂商不猜测其品牌。
fn vendor_name(vendor_id: &str) -> Option<&'static str> {
    match vendor_id.to_ascii_lowercase().as_str() {
        "0x1002" => Some("AMD"),
        "0x10de" => Some("NVIDIA"),
        "0x8086" => Some("Intel"),
        "0x106b" => Some("Apple"),
        "0x1af4" => Some("Virtio"),
        "0x15ad" => Some("VMware"),
        "0x80ee" => Some("VirtualBox"),
        _ => None,
    }
}

#[cfg(target_os = "linux")]
/// 过滤 NVIDIA 查询中的空值和 N/A 标记，保留不支持的度量为 None。
fn optional_command_text(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("N/A") || value.eq_ignore_ascii_case("[N/A]")
    {
        return None;
    }
    Some(value.to_owned())
}

#[cfg(target_os = "linux")]
/// 解析 NVIDIA CLI 的定长 CSV，延续诊断已有字段和计量单位。
pub(super) fn parse_nvidia_smi_gpus(output: &[u8]) -> Vec<RuntimeGpuHealth> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(parse_nvidia_smi_gpu_line)
        .collect()
}

#[cfg(target_os = "linux")]
/// 把无标题 NVIDIA 查询结果行转换成摘要，保留 N/A 数值为未知。
fn parse_nvidia_smi_gpu_line(line: &str) -> Option<RuntimeGpuHealth> {
    let parts = line.split(',').map(str::trim).collect::<Vec<_>>();
    if parts.len() < 6 {
        return None;
    }

    Some(RuntimeGpuHealth {
        name: optional_command_text(parts[0])?,
        vendor: Some("NVIDIA".to_owned()),
        driver_version: optional_command_text(parts[1]),
        memory_total_bytes: parse_mib_to_bytes(parts[2]),
        memory_used_bytes: parse_mib_to_bytes(parts[3]),
        utilization_percent: parse_optional_f32(parts[4])
            .map(|value| (value.clamp(0.0, 100.0) * 10.0).round() / 10.0),
        temperature_celsius: parse_optional_f32(parts[5]),
    })
}

#[cfg(target_os = "linux")]
/// 解析真实有限浮点值，不支持的 NVIDIA 采样项继续保持未知。
fn parse_optional_f32(value: &str) -> Option<f32> {
    let value = optional_command_text(value)?.parse::<f32>().ok()?;
    value.is_finite().then_some(value)
}

#[cfg(target_os = "linux")]
/// 转换 MiB 文本为字节数；负数、非有限值和溢出均表示为未知。
fn parse_mib_to_bytes(value: &str) -> Option<u64> {
    let mib = optional_command_text(value)?.parse::<f64>().ok()?;
    if !mib.is_finite() || mib < 0.0 {
        return None;
    }
    let bytes = (mib * 1_048_576.0).round();
    (bytes < u64::MAX as f64).then_some(bytes as u64)
}

#[cfg(any(target_os = "windows", target_os = "macos", test))]
/// 只接受平台 JSON 中的非空字符串字段，防止错误类型混入设备名称。
/// Linux GPU 探测不消费 JSON；测试构建仍保留 Windows/macOS 解析路径。
fn json_string(value: &serde_json::Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

#[cfg(any(target_os = "windows", test))]
/// 同时接受 WMI 的 JSON 整数和 PowerShell 序列化数字文本。
fn json_u64(value: &serde_json::Value, key: &str) -> Option<u64> {
    let value = value.get(key)?;
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    if let Some(value) = value.as_i64() {
        return u64::try_from(value).ok();
    }
    value.as_str()?.trim().parse().ok()
}

#[cfg(any(target_os = "windows", test))]
/// 只接受有限 GPU 百分比数值，防止 JSON 非法浮点进入响应。
fn json_f32(value: &serde_json::Value, key: &str) -> Option<f32> {
    let value = value.get(key)?;
    let parsed = value
        .as_f64()
        .map(|number| number as f32)
        .or_else(|| value.as_str()?.trim().parse::<f32>().ok())?;
    parsed.is_finite().then_some(parsed)
}

#[cfg(target_os = "macos")]
/// 解析系统报告中的整数或常见 MB/GB 文本；单位不明时不输出容量。
fn value_to_bytes(value: &serde_json::Value) -> Option<u64> {
    if let Some(value) = value.as_u64() {
        return Some(value);
    }
    let text = value.as_str()?.trim();
    let lower = text.to_ascii_lowercase();
    let (number, multiplier) = if let Some(value) = lower.strip_suffix(" gb") {
        (value.trim(), 1024_u64.pow(3))
    } else if let Some(value) = lower.strip_suffix(" mb") {
        (value.trim(), 1024_u64.pow(2))
    } else if let Some(value) = lower.strip_suffix(" kb") {
        (value.trim(), 1024_u64)
    } else {
        return None;
    };
    let number = number.parse::<f64>().ok()?;
    (number.is_finite() && number >= 0.0).then(|| (number * multiplier as f64).round() as u64)
}
