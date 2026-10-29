//! 本机 GPU 探针与服务器资源监控共用的数据契约。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

/// 采集源缺失或读取失败时使用的固定分类，不携带底层路径和错误文本。
#[derive(Debug, Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DiagnosticUnavailableReason {
    NotFound,
    InvalidData,
    PermissionDenied,
    Unsupported,
    CollectionFailed,
}

/// 已识别显卡的可用计量；不支持的指标保持 None，不以零代替未知。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeGpuHealth {
    pub name: String,
    pub vendor: Option<String>,
    pub driver_version: Option<String>,
    pub memory_total_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub utilization_percent: Option<f32>,
    pub temperature_celsius: Option<f32>,
}
