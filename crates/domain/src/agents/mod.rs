//! 外部 Agent 会话的领域模型与状态约束。
//!
//! @author kongweiguang

mod launcher_key;
mod permission_mode;
mod provider_session;
mod session;
mod target_binding;
mod types;

pub use launcher_key::AgentLauncherKey;
pub use permission_mode::AgentLaunchPermissionMode;
pub use provider_session::{AgentProviderSession, AgentProviderSessionError};
pub use session::{AgentSession, AgentSessionLaunch, AgentSessionStatus};
pub use target_binding::{AgentTargetBinding, AgentTargetLiveStatus};
pub use types::{AgentDomainError, AgentProvider, AgentSessionScope};

use crate::identity::AgentSessionId;

/// 从已验证的全局 MCP 基地址派生 Agent 专属入口。
///
/// provider 配置、context 文件和 HTTP server 路由必须使用同一格式；集中在这里
/// 可以避免各写入点各拼一次字符串，导致 CLI 连到全局端点而绕过会话目标隔离。
///
/// @author kongweiguang
pub fn agent_mcp_endpoint(base: &str, agent_session_id: &AgentSessionId) -> String {
    format!(
        "{}/agents/{}",
        base.trim_end_matches('/'),
        agent_session_id.as_str()
    )
}
