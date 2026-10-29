//! Docker/Podman 的领域投影；命令行和 SSH 传输留在基础设施层。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::identity::HostId;

pub use crate::files::ContainerRuntime;

/// 容器生命周期状态的稳定 UI/接口值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerStatus {
    Running,
    Exited,
    Paused,
    Restarting,
    Created,
    Dead,
    Unknown,
}

/// 容器目标按宿主、运行时和容器标识构成精确身份，可作为键使用，且不拼接 shell 命令。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerTarget {
    pub host_id: Option<HostId>,
    pub runtime: ContainerRuntime,
    pub container_id: String,
}

/// Compose 标签在列表与检查结果之间统一后的元数据。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeMetadata {
    pub project: Option<String>,
    pub service: Option<String>,
    pub working_dir: Option<String>,
    pub config_files: Vec<String>,
    pub config_paths: Vec<String>,
    pub container_number: Option<String>,
    pub oneoff: bool,
}

/// 停止保留容器，拆除移除项目容器与网络；两者都不隐式删除持久卷。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ComposeAction {
    Up,
    Stop,
    Down,
    Restart,
}

/// 由容器标签提取的 Compose 项目执行目标，不承载任意命令文本。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeProjectTarget {
    pub host_id: Option<HostId>,
    pub runtime: ContainerRuntime,
    pub project: String,
    pub working_dir: String,
    pub config_files: Vec<String>,
}

/// Compose 操作成功只表示命令已完成，后续列表刷新由调用方独立执行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposeActionReceipt {
    pub target: ComposeProjectTarget,
    pub action: ComposeAction,
    pub refresh_required: bool,
}

/// 列表行是查询投影，不强行承担容器聚合根职责。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerSummary {
    pub target: ContainerTarget,
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub status: ContainerStatus,
    pub status_text: String,
    pub ports: Vec<String>,
    pub labels: Vec<(String, String)>,
    pub compose: Option<ComposeMetadata>,
}

/// 容器 inspect 结果，仅保留界面、排障和 MCP 需要的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerInspect {
    pub target: ContainerTarget,
    pub id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub running: bool,
    pub created: Option<String>,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub entrypoint: Vec<String>,
    pub command: Vec<String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub ports: Vec<String>,
    pub networks: Vec<String>,
    /// 详情展示来自本次 inspect；保持既有 MCP/持久化序列化投影，避免 UI 补齐字段改变外部契约。
    #[serde(skip)]
    pub labels: Vec<(String, String)>,
}

/// 容器资源快照；原始文本仅用于展示，计算字段继续保持可空。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerStats {
    pub target: ContainerTarget,
    pub cpu_percent: Option<String>,
    pub memory_percent: Option<String>,
    pub memory_usage: Option<String>,
    pub network_io: Option<String>,
    pub block_io: Option<String>,
    pub pids: Option<String>,
    pub raw: String,
}

/// 容器文件浏览中的节点类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerFileKind {
    File,
    Directory,
    Symlink,
    Other,
}

/// 容器目录的一项；路径保持目标机原样，UI 不自行拼接路径。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerFileEntry {
    pub name: String,
    pub path: String,
    pub kind: ContainerFileKind,
    pub size: Option<u64>,
    pub modified: Option<String>,
    pub permissions: Option<String>,
    pub permissions_mode: Option<u32>,
}

/// 容器列表筛选条件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerListQuery {
    pub host_id: Option<HostId>,
    pub runtime: ContainerRuntime,
    pub include_stopped: bool,
}

/// 容器可执行的有限生命周期动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ContainerAction {
    Start,
    Stop,
    Restart,
    Remove,
}

/// 一个成功的远端写操作与后续摘要刷新分开表示，避免刷新失败伪装成写入失败。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerMutationReceipt {
    pub target: ContainerTarget,
    pub action: ContainerAction,
    pub applied: bool,
    pub refresh_required: bool,
}
