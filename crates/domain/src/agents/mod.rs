//! 外部 Agent 会话的领域模型与状态约束。
//!
//! @author kongweiguang

mod launcher_key;
mod provider_session;
mod session;
mod target_binding;
mod types;

pub use launcher_key::AgentLauncherKey;
pub use provider_session::{AgentProviderSession, AgentProviderSessionError};
pub use session::{AgentSession, AgentSessionLaunch, AgentSessionStatus};
pub use target_binding::{AgentTargetBinding, AgentTargetLiveStatus};
pub use types::{AgentDomainError, AgentProvider, AgentSessionScope};
