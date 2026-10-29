//! Agent CLI 权限模式与历史会话复用兼容规则。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use super::{AgentProvider, AgentSessionLaunch};

/// Agent 会话实际启动时使用的权限模式；新模式按 main 分支的 argv 标记保持兼容。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentLaunchPermissionMode {
    /// 未应用额外跳过审批参数。
    #[default]
    Default,
    /// Codex 或 Claude 启动参数明确跳过对应权限审批。
    SkipPermissions,
}

impl AgentLaunchPermissionMode {
    /// main 默认对 Codex 与 Claude 使用 full-permission；其他 CLI 保留命令原样。
    pub const fn default_for(provider: AgentProvider) -> Self {
        match provider {
            AgentProvider::Codex | AgentProvider::Claude => Self::SkipPermissions,
            AgentProvider::Pi | AgentProvider::Custom => Self::Default,
        }
    }

    /// 从实际 argv 推断模式，旧 session 无显式字段时与 main 的恢复筛选保持一致。
    pub fn from_launch(provider: AgentProvider, launch: &AgentSessionLaunch) -> Self {
        let flag = match provider {
            AgentProvider::Codex => "--dangerously-bypass-approvals-and-sandbox",
            AgentProvider::Claude => "--dangerously-skip-permissions",
            AgentProvider::Pi | AgentProvider::Custom => return Self::Default,
        };
        if launch.args.iter().any(|argument| argument == flag) {
            Self::SkipPermissions
        } else {
            Self::Default
        }
    }

    /// 完整权限入口可继续同 launcher 的旧标准权限会话，反向提升权限则不复用。
    pub const fn can_reuse(self, stored: Self) -> bool {
        !matches!((self, stored), (Self::Default, Self::SkipPermissions))
    }
}
