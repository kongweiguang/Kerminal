//! Linux GPU/NPU 探针输出解析与稳定失败分类。
//!
//! @author kongweiguang

use kerminal_application::{AppError, PublicErrorCode};
use kerminal_domain::operations::{
    GpuSnapshot, NpuSnapshot, ServerInfoProbeFailure, ServerInfoProbeStatus,
};

/// 仅将受支持的固定错误码投影给 UI，不把远端 stderr、设备路径或主机输入写入快照。
pub(super) fn failure_status(error: &AppError) -> ServerInfoProbeStatus {
    let failure = match error {
        AppError::RemoteExit { status: Some(126) } => ServerInfoProbeFailure::PermissionDenied,
        AppError::RemoteExit { status: Some(127) } => ServerInfoProbeFailure::NotFound,
        _ => match error.public_code() {
            PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
                ServerInfoProbeFailure::Unsupported
            }
            PublicErrorCode::NotFound => ServerInfoProbeFailure::NotFound,
            PublicErrorCode::Timeout => ServerInfoProbeFailure::TimedOut,
            PublicErrorCode::Invalid => ServerInfoProbeFailure::InvalidData,
            _ => ServerInfoProbeFailure::CommandFailed,
        },
    };
    ServerInfoProbeStatus::Unavailable(failure)
}

/// 将 nvidia-smi 的逗号分隔输出映射成可缺省的度量，不支持或 N/A 值保持未知。
pub(super) fn parse_nvidia_smi_csv(
    output: &str,
) -> Result<Vec<GpuSnapshot>, ServerInfoProbeFailure> {
    let mut gpus = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<_> = line.split(',').map(str::trim).collect();
        if fields.len() < 6 || fields[0].is_empty() {
            return Err(ServerInfoProbeFailure::InvalidData);
        }
        let index = gpus.len();
        gpus.push(GpuSnapshot {
            id: index.to_string(),
            model: Some(fields[0].to_owned()),
            vendor: Some("NVIDIA".to_owned()),
            driver_version: optional_field(fields.get(1).copied()),
            memory_total_bytes: mib_field(fields.get(2).copied()),
            memory_used_bytes: mib_field(fields.get(3).copied()),
            utilization_percent: percent_field(fields.get(4).copied()),
            temperature_celsius: number_field(fields.get(5).copied()),
            power_watts: number_field(fields.get(6).copied()),
        });
    }
    Ok(gpus)
}

/// 从 lspci 的设备类别行提取 GPU 名称，回退探针只承诺设备识别、不伪造运行计量。
pub(super) fn parse_lspci_gpus(output: &str) -> Vec<GpuSnapshot> {
    output
        .lines()
        .filter_map(|line| {
            let normalized = line.to_ascii_lowercase();
            if ![
                "vga compatible controller",
                "3d controller",
                "display controller",
            ]
            .iter()
            .any(|kind| normalized.contains(kind))
            {
                return None;
            }
            let (_, model) = line.rsplit_once(": ")?;
            let model = model.trim();
            let vendor = if normalized.contains("nvidia") {
                Some("NVIDIA".to_owned())
            } else if normalized.contains("amd") || normalized.contains("ati") {
                Some("AMD".to_owned())
            } else if normalized.contains("intel") {
                Some("Intel".to_owned())
            } else {
                None
            };
            let id = line.split_whitespace().next()?.to_owned();
            (!model.is_empty()).then(|| GpuSnapshot {
                id,
                model: Some(model.to_owned()),
                vendor,
                driver_version: None,
                utilization_percent: None,
                memory_total_bytes: None,
                memory_used_bytes: None,
                temperature_celsius: None,
                power_watts: None,
            })
        })
        .collect()
}

/// 解析 npu-smi 的设备与后续 chip 行；格式未识别时报告 InvalidData 而不返回假空列表。
pub(super) fn parse_npu_smi_info(output: &str) -> Result<Vec<NpuSnapshot>, ServerInfoProbeFailure> {
    let mut devices = Vec::<NpuSnapshot>::new();
    let mut active_device = None;
    for line in output.lines() {
        let columns: Vec<_> = line.split('|').map(str::trim).collect();
        if columns.len() < 4 {
            continue;
        }
        let identity = columns[1];
        let health = columns[2];
        if is_npu_health(health) {
            let mut parts = identity.split_whitespace();
            let Some(device_id) = parts.next().and_then(|part| part.parse::<u32>().ok()) else {
                continue;
            };
            let model = parts.collect::<Vec<_>>().join(" ");
            let stats: Vec<_> = numeric_values(columns[3]).into_iter().collect();
            let mut device = NpuSnapshot {
                id: device_id.to_string(),
                model: (!model.is_empty()).then_some(model),
                chip_id: None,
                health: Some(health.to_owned()),
                aicore_percent: None,
                hbm_total_bytes: None,
                hbm_used_bytes: None,
                temperature_celsius: stats.get(1).copied(),
                power_watts: stats.first().copied(),
                bus_id: None,
            };
            if device.model.is_none() {
                device.health = None;
                continue;
            }
            devices.push(device);
            active_device = Some(devices.len() - 1);
            continue;
        }

        let Some(index) = active_device else {
            continue;
        };
        let Some(chip_id) = identity.parse::<u32>().ok() else {
            continue;
        };
        if columns[2].is_empty() {
            continue;
        }
        let device = &mut devices[index];
        device.chip_id = Some(chip_id);
        device.bus_id = Some(columns[2].to_owned());
        device.id = format!("{}:{chip_id}", device.id);
        let stats: Vec<_> = columns[3].split(',').map(str::trim).collect();
        device.aicore_percent = stats.first().and_then(|value| first_number(value));
        if let Some((used, total)) = stats.get(2).and_then(|value| number_pair(value)) {
            device.hbm_used_bytes = mib_bytes(used);
            device.hbm_total_bytes = mib_bytes(total);
        }
        active_device = None;
    }
    if devices.is_empty() && !output.trim().is_empty() {
        return Err(ServerInfoProbeFailure::InvalidData);
    }
    Ok(devices)
}

/// NPU 状态列使用 npu-smi 已知健康枚举，表头/分隔线不会被当作设备行。
fn is_npu_health(value: &str) -> bool {
    matches!(value, "OK" | "Warning" | "Alarm" | "Critical" | "UNKNOWN")
}

/// 解析人类可读计量中的数字片段；单位和标点不会被错误拼进浮点输入。
fn numeric_values(value: &str) -> Vec<f64> {
    value
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .filter_map(|part| part.parse::<f64>().ok())
        .filter(|number| number.is_finite() && *number >= 0.0)
        .collect()
}

/// 从形如“已用 / 总量”的 NPU 内存字段读取两个非负数。
fn number_pair(value: &str) -> Option<(f64, f64)> {
    let mut values = numeric_values(value).into_iter();
    Some((values.next()?, values.next()?))
}

/// 从单个探针字段提取首个有限非负数。
fn first_number(value: &str) -> Option<f64> {
    numeric_values(value).into_iter().next()
}

/// 百分比只接受设备能表达的 0–100 区间。
fn percent_field(value: Option<&str>) -> Option<f64> {
    let value = first_number(value?)?;
    (value <= 100.0).then_some(value)
}

/// 转换 MiB 数值，范围或单位未知时保留 None。
fn mib_field(value: Option<&str>) -> Option<u64> {
    mib_bytes(first_number(value?)?)
}

/// 只转换有限、非负且未溢出的 MiB 数值。
fn mib_bytes(value: f64) -> Option<u64> {
    let bytes = value * 1024.0 * 1024.0;
    (bytes.is_finite() && bytes <= u64::MAX as f64).then_some(bytes as u64)
}

/// 解析温度或功耗字段；N/A、空值与非法范围仍以未知表示。
fn number_field(value: Option<&str>) -> Option<f64> {
    let number = first_number(value?)?;
    (number <= 100_000.0).then_some(number)
}

/// 仅将明确的 N/A/空值转换成 None，保留原始版本标签用于诊断展示。
fn optional_field(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    (!value.is_empty()
        && !value.eq_ignore_ascii_case("n/a")
        && !value.eq_ignore_ascii_case("not supported"))
    .then(|| value.to_owned())
}
