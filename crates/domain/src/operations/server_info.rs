//! 只读主机资源快照和值得比较的网络计数。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::identity::HostId;

/// 监控目标可以是 SSH 主机或其容器。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ServerInfoTarget {
    Local,
    Ssh {
        host_id: HostId,
    },
    Container {
        host_id: Option<HostId>,
        runtime: super::container::ContainerRuntime,
        container_id: String,
    },
}

/// CPU 总体和每核的只读使用率。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CpuSnapshot {
    pub usage_percent: Option<f64>,
    pub per_core_percent: Vec<Option<f64>>,
    pub load_average: Vec<f64>,
    pub model: Option<String>,
}

/// 内存与 Swap 容量，单位为字节。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MemorySnapshot {
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
    /// Linux meminfo 的真实 Cached 容量；本机/容器来源未知时保持空值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
}

/// 文件系统容量摘要，单位为字节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSnapshot {
    pub filesystem: String,
    pub mount: String,
    pub total_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub available_bytes: Option<u64>,
}

/// 网卡累计计数；速率只在两个有效样本之间计算。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkCounterSnapshot {
    pub name: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// 采样差值结果；计数重置时速率为空，不能显示负速率。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRateSnapshot {
    pub name: String,
    pub rx_bytes_per_second: Option<f64>,
    pub tx_bytes_per_second: Option<f64>,
    pub counter_reset: bool,
}

/// 服务器快照中各类字段最近一次真实采样时间；未采样时保留 None。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoSampleTimes {
    pub static_info_at_unix_ms: Option<u64>,
    pub fast_at_unix_ms: Option<u64>,
    pub slow_at_unix_ms: Option<u64>,
    pub gpu_at_unix_ms: Option<u64>,
    pub npu_at_unix_ms: Option<u64>,
}

/// 设备探针不能仅靠空列表表达，否则 UI 无法区分未采样、无设备和权限失败。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", content = "reason", rename_all = "camelCase")]
pub enum ServerInfoProbeStatus {
    #[default]
    NotSampled,
    Available,
    Unavailable(ServerInfoProbeFailure),
}

/// 可选设备探针的脱敏失败类别，不把本机命令输出或路径放进领域契约。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ServerInfoProbeFailure {
    Unsupported,
    NotFound,
    PermissionDenied,
    TimedOut,
    CommandFailed,
    InvalidData,
}

/// 进程摘要；采集不到的数值保持为空。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSnapshot {
    pub pid: u32,
    pub name: String,
    pub cpu_percent: Option<f64>,
    pub memory_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
}

/// GPU 设备摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GpuSnapshot {
    pub id: String,
    pub model: Option<String>,
    pub vendor: Option<String>,
    pub driver_version: Option<String>,
    pub utilization_percent: Option<f64>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub temperature_celsius: Option<f64>,
    pub power_watts: Option<f64>,
}

/// Ascend 等 NPU 设备独立于 GPU 展示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NpuSnapshot {
    pub id: String,
    pub model: Option<String>,
    pub chip_id: Option<u32>,
    pub health: Option<String>,
    pub aicore_percent: Option<f64>,
    pub hbm_total_bytes: Option<u64>,
    pub hbm_used_bytes: Option<u64>,
    pub temperature_celsius: Option<f64>,
    pub power_watts: Option<f64>,
    pub bus_id: Option<String>,
}

/// 监控快照按目标和采样层级合并；缺失字段不伪造为零。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfoSnapshot {
    pub target: ServerInfoTarget,
    pub captured_at_unix_ms: u64,
    #[serde(default)]
    pub sample_times: ServerInfoSampleTimes,
    /// Fast 样本的启动时长；容器或探针未知时保持 None，而不是借用宿主机值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uptime_seconds: Option<u64>,
    pub hostname: Option<String>,
    pub operating_system: Option<String>,
    pub architecture: Option<String>,
    pub kernel: Option<String>,
    pub cpu: CpuSnapshot,
    pub memory: MemorySnapshot,
    pub disks: Vec<DiskSnapshot>,
    pub network_counters: Vec<NetworkCounterSnapshot>,
    pub network_rates: Vec<NetworkRateSnapshot>,
    pub process_count: Option<u64>,
    pub running_process_count: Option<u64>,
    pub processes: Vec<ProcessSnapshot>,
    #[serde(default)]
    pub disk_probe_status: ServerInfoProbeStatus,
    #[serde(default)]
    pub network_probe_status: ServerInfoProbeStatus,
    #[serde(default)]
    pub process_probe_status: ServerInfoProbeStatus,
    #[serde(default)]
    pub gpu_probe_status: ServerInfoProbeStatus,
    pub gpus: Vec<GpuSnapshot>,
    #[serde(default)]
    pub npu_probe_status: ServerInfoProbeStatus,
    pub npus: Vec<NpuSnapshot>,
}

/// 仅对计数单调的采样窗口生成速率，避免重启/计数回绕显示负值。
pub fn derive_network_rate(
    previous: &NetworkCounterSnapshot,
    current: &NetworkCounterSnapshot,
    elapsed_seconds: f64,
) -> NetworkRateSnapshot {
    if previous.name != current.name || elapsed_seconds <= 0.0 || !elapsed_seconds.is_finite() {
        return NetworkRateSnapshot {
            name: current.name.clone(),
            rx_bytes_per_second: None,
            tx_bytes_per_second: None,
            counter_reset: false,
        };
    }
    let reset = current.rx_bytes < previous.rx_bytes || current.tx_bytes < previous.tx_bytes;
    NetworkRateSnapshot {
        name: current.name.clone(),
        rx_bytes_per_second: (!reset)
            .then(|| (current.rx_bytes - previous.rx_bytes) as f64 / elapsed_seconds),
        tx_bytes_per_second: (!reset)
            .then(|| (current.tx_bytes - previous.tx_bytes) as f64 / elapsed_seconds),
        counter_reset: reset,
    }
}
