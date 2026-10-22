//! Agent provider、访问范围与领域错误。
//!
//! @author kongweiguang

use std::{error::Error, fmt};

use crate::identity::TabId;
use serde::{Deserialize, Serialize};

/// 受管 Agent 会话可启动的 provider；Custom 的命令快照由启动请求提供。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub enum AgentProvider {
    /// OpenAI Codex CLI。
    Codex,
    /// Claude Code CLI。
    Claude,
    /// Pi coding agent CLI。
    Pi,
    /// 用户配置的命令行 Agent。
    Custom,
}

impl AgentProvider {
    /// 返回稳定 provider 标识，以便配置和历史记录跨版本复用。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Custom => "custom",
        }
    }

    /// 从配置或历史文件的稳定标识解析 provider，未知值必须显式失败。
    pub fn parse(value: &str) -> Result<Self, AgentDomainError> {
        match value {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "pi" => Ok(Self::Pi),
            "custom" => Ok(Self::Custom),
            _ => Err(AgentDomainError::UnknownProvider(value.to_owned())),
        }
    }
}

/// 持久化归属只用于区分右栏 Agent 历史；Agent 的运行权限始终是整个工作区。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum AgentSessionScope {
    /// 右栏会话创建时所在的 Tab，不作为工具访问限制。
    Tab {
        /// 会话归属的稳定 Tab 标识。
        #[serde(rename = "tabId")]
        tab_id: TabId,
    },
    /// 未绑定 Tab 或外部会话的全局归属。
    Global,
}

impl AgentSessionScope {
    /// 始终把历史归属与全局工具访问分开，避免 Tab 记录意外缩小 MCP 能力。
    pub const fn effective_access(&self) -> Self {
        Self::Global
    }

    /// 返回 Tab 归属 id；全局归属没有 Tab id。
    pub fn tab_id(&self) -> Option<&TabId> {
        match self {
            Self::Tab { tab_id } => Some(tab_id),
            Self::Global => None,
        }
    }
}

/// Agent 生命周期中的可恢复领域错误，不依赖应用层或文件系统实现。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentDomainError {
    /// provider 标识不在当前受支持集合中。
    UnknownProvider(String),
    /// launcher key 与 provider 不匹配或不满足稳定格式。
    InvalidLauncherKey,
    /// 启动快照缺少可执行命令或工作目录。
    InvalidLaunchSpec(&'static str),
    /// 历史会话标题为空。
    EmptyTitle,
    /// 会话路径为空，无法建立可恢复的受管工作区。
    EmptyWorkspacePath,
    /// 较旧的目标绑定更新不得覆盖较新的 live 目标。
    StaleBindingGeneration { current: u64, requested: u64 },
    /// 同一绑定 generation 不能重指向不同目标。
    BindingGenerationReused(u64),
    /// 有效首选目标已经失效，调用方必须显式选择或清除绑定。
    PreferredTargetUnavailable,
    /// 目标失效转换只接受 stale 或 closed 状态。
    InvalidTargetInvalidationStatus,
    /// 目标失效转换要求会话当前确实保存了目标绑定。
    TargetBindingMissing,
    /// 受管 Agent 文件版本超出当前实现支持的版本。
    UnsupportedSchemaVersion(u32),
    /// 持久化会话缺少创建或更新时间。
    EmptyTimestamp,
}

impl fmt::Display for AgentDomainError {
    /// 提供不带运行时路径或凭据的稳定错误说明，供 UI 与 MCP 分别映射。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider(provider) => write!(formatter, "未知 Agent provider：{provider}"),
            Self::InvalidLauncherKey => formatter.write_str("Agent launcherKey 不合法"),
            Self::InvalidLaunchSpec(field) => write!(formatter, "Agent 启动信息缺少 {field}"),
            Self::EmptyTitle => formatter.write_str("Agent 会话标题不能为空"),
            Self::EmptyWorkspacePath => formatter.write_str("Agent 工作区路径不能为空"),
            Self::StaleBindingGeneration { current, requested } => write!(
                formatter,
                "Agent 目标绑定已更新：当前 generation {current}，请求 generation {requested}"
            ),
            Self::BindingGenerationReused(generation) => write!(
                formatter,
                "Agent 目标绑定 generation {generation} 已用于其它目标"
            ),
            Self::PreferredTargetUnavailable => {
                formatter.write_str("Agent 首选终端不可用，需显式重新绑定目标")
            }
            Self::InvalidTargetInvalidationStatus => {
                formatter.write_str("Agent 目标失效状态只能是 stale 或 closed")
            }
            Self::TargetBindingMissing => formatter.write_str("Agent 会话没有可标记失效的目标绑定"),
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "不支持 Agent 会话 schema 版本 {version}")
            }
            Self::EmptyTimestamp => formatter.write_str("Agent 会话时间戳不能为空"),
        }
    }
}

impl Error for AgentDomainError {}
