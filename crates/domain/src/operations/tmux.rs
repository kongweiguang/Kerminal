//! tmux 目标与会话摘要；本模块不执行命令，也不持有终端会话。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::identity::HostId;

/// tmux server socket 作用域。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxTarget {
    pub host_id: Option<HostId>,
    /// 本地 tmux attach 使用的已保存启动 profile；不会对 SSH 主机套用本地环境。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<String>,
    pub socket_name: Option<String>,
    pub socket_path: Option<String>,
    pub tmux_path: Option<String>,
}

impl TmuxTarget {
    /// 拒绝含混 socket 配置，避免同一请求落入意外的 tmux server。
    pub fn validate(&self) -> Result<(), TmuxTargetError> {
        if self.socket_name.is_some() && self.socket_path.is_some() {
            return Err(TmuxTargetError::ConflictingSocketOptions);
        }
        if self
            .socket_name
            .as_deref()
            .into_iter()
            .chain(self.socket_path.as_deref())
            .chain(self.tmux_path.as_deref())
            .any(|value| value.trim().is_empty() || value.chars().any(char::is_control))
        {
            return Err(TmuxTargetError::InvalidText);
        }
        if let Some(profile_id) = self.profile_id.as_deref() {
            if profile_id.trim().is_empty() || profile_id.chars().any(char::is_control) {
                return Err(TmuxTargetError::InvalidProfileId);
            }
            if self.host_id.is_some() {
                return Err(TmuxTargetError::ProfileRequiresLocalTarget);
            }
        }
        Ok(())
    }
}

/// tmux 目標配置錯誤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmuxTargetError {
    ConflictingSocketOptions,
    InvalidText,
    InvalidProfileId,
    ProfileRequiresLocalTarget,
}

/// tmux session 摘要；同一 host 不同 socket 是不同的 target scope。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxSession {
    pub id: String,
    pub name: String,
    pub attached: bool,
    pub clients: u32,
    pub windows: u32,
    pub current_path: Option<String>,
    pub created_at: Option<u64>,
    pub activity_at: Option<u64>,
    pub target_ref: String,
}

/// tmux window 查询投影；ID 与目标 scope 一起返回，避免调用方只凭窗口序号猜测对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxWindow {
    pub id: String,
    /// tmux 原始 window 序号；旧快照缺少时读取为零以保持反序列化兼容。
    #[serde(default)]
    pub index: u32,
    pub name: String,
    pub active: bool,
    pub pane_count: u32,
    pub width: u32,
    pub height: u32,
    /// tmux 当前布局名；旧快照或未报告布局时为空。
    #[serde(default)]
    pub layout: Option<String>,
    /// tmux 原始窗口 flags；不在领域层重新解释 tmux 版本相关标记。
    #[serde(default)]
    pub flags: Option<String>,
    pub current_path: Option<String>,
    pub target_ref: String,
}

/// tmux pane 查询投影；不暴露进程句柄或可变运行态对象。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxPane {
    pub id: String,
    /// tmux 原始 pane 序号；列表顺序变化后仍能还原用户看到的索引。
    #[serde(default)]
    pub index: u32,
    pub session_id: String,
    pub window_id: String,
    pub title: String,
    pub current_command: String,
    pub current_path: Option<String>,
    pub width: u32,
    pub height: u32,
    pub active: bool,
    pub dead: bool,
    pub target_ref: String,
}

/// pane 捕获结果保留实际行数与截断标记，避免 MCP 将有界快照当完整历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxPaneCapture {
    pub pane_id: String,
    pub text: String,
    pub captured_lines: u32,
    pub truncated: bool,
}

/// attach 计划只是可执行规格，不会隐式创建 Kerminal tab 或 tmux session。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxAttachPlan {
    pub target: TmuxTarget,
    pub session_id: String,
    pub session_name: String,
    pub program: String,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
}

/// 远端 tmux 创建/重命名确认成功后的返回值，刷新可单独重试。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxMutationReceipt {
    pub session: TmuxSession,
    pub refresh_required: bool,
}
