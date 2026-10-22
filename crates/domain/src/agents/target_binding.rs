//! Agent 会话的首选终端绑定值对象。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::identity::{PaneId, SessionId, TabId};

use super::AgentDomainError;

/// 首选用户终端绑定的 live 状态。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentTargetLiveStatus {
    /// 尚未绑定终端。
    Unbound,
    /// session id 已显式解析并确认可用。
    Ready,
    /// 绑定存在，但目标当前无法确认。
    Stale,
    /// 用户终端已关闭。
    Closed,
}

/// 当前右栏 Agent 的首选用户终端；绑定只提供默认目标，不表示授权边界。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct AgentTargetBinding {
    /// 对一次绑定关系做稳定标识，供 UI 对照持久化上下文。
    #[serde(default)]
    pub binding_id: Option<String>,
    /// 单调递增的绑定版本，防止异步旧 UI 状态覆盖新目标。
    #[serde(default)]
    pub generation: u64,
    /// 可见终端所在的 Pane。
    #[serde(default)]
    pub pane_id: Option<PaneId>,
    /// 右栏助手归属的 Tab。
    #[serde(default)]
    pub tab_id: Option<TabId>,
    /// 首选用户终端的显式运行时 id。
    #[serde(default)]
    pub terminal_session_id: Option<SessionId>,
    /// 目标稳定引用，供工作区上下文解释连接类型。
    #[serde(default)]
    pub target_ref: Option<String>,
    /// 目标类型，例如 local、ssh 或 dockerContainer。
    #[serde(default)]
    pub target_kind: Option<String>,
    /// 绑定时观察到的工作目录。
    #[serde(default)]
    pub cwd: Option<String>,
    /// 绑定时观察到的 shell 标签。
    #[serde(default)]
    pub shell: Option<String>,
    /// 绑定状态；只有 Ready 且带 session id 时可解析为首选终端。
    pub live_status: AgentTargetLiveStatus,
    /// 最近一次确认目标仍可用的时间戳。
    #[serde(default)]
    pub last_seen_at: Option<String>,
}

impl AgentTargetBinding {
    /// 创建无目标绑定，避免缺失上下文被解释为任意可见终端。
    pub fn unbound(generation: u64) -> Self {
        Self {
            binding_id: None,
            generation,
            pane_id: None,
            tab_id: None,
            terminal_session_id: None,
            target_ref: None,
            target_kind: None,
            cwd: None,
            shell: None,
            live_status: AgentTargetLiveStatus::Unbound,
            last_seen_at: None,
        }
    }

    /// 仅接受已校验的显式 session id；失效目标返回错误，不查找其它 Tab。
    pub fn preferred_session_id(&self) -> Result<Option<&SessionId>, AgentDomainError> {
        match self.live_status {
            AgentTargetLiveStatus::Unbound => Ok(None),
            AgentTargetLiveStatus::Ready => self
                .terminal_session_id
                .as_ref()
                .map(Some)
                .ok_or(AgentDomainError::PreferredTargetUnavailable),
            AgentTargetLiveStatus::Stale | AgentTargetLiveStatus::Closed => {
                Err(AgentDomainError::PreferredTargetUnavailable)
            }
        }
    }

    /// 拒绝不能解析的 Ready 绑定，避免持久化后才发现缺少显式 session id。
    pub fn validate(&self) -> Result<(), AgentDomainError> {
        if self.live_status == AgentTargetLiveStatus::Ready && self.terminal_session_id.is_none() {
            return Err(AgentDomainError::PreferredTargetUnavailable);
        }
        Ok(())
    }

    /// 判断两条绑定是否指向相同显式终端，以便只允许相同 generation 的幂等重放。
    pub fn points_to_same_session(&self, other: &Self) -> bool {
        self.terminal_session_id == other.terminal_session_id
            && self.pane_id == other.pane_id
            && self.tab_id == other.tab_id
            && self.target_ref == other.target_ref
    }
}
