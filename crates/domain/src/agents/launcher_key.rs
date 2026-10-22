//! Agent Launcher 的稳定身份值对象。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use super::{AgentDomainError, AgentProvider};

/// 标识内置 Agent 或某一条已保存 Custom 定义。
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct AgentLauncherKey(String);

impl AgentLauncherKey {
    /// 校验 launcher 身份和 provider 一致，避免多个 Custom 会话因 provider 相同而串历史。
    pub fn new(
        value: impl Into<String>,
        provider: AgentProvider,
    ) -> Result<Self, AgentDomainError> {
        let value = value.into();
        let valid = match provider {
            AgentProvider::Codex => value == "builtin:codex",
            AgentProvider::Claude => value == "builtin:claude",
            AgentProvider::Pi => value == "builtin:pi",
            AgentProvider::Custom => value.strip_prefix("custom:").is_some_and(is_canonical_uuid),
        };
        if valid {
            Ok(Self(value))
        } else {
            Err(AgentDomainError::InvalidLauncherKey)
        }
    }

    /// 构造与 provider 对应的稳定内置 launcher key。
    pub fn builtin(provider: AgentProvider) -> Result<Self, AgentDomainError> {
        let value = match provider {
            AgentProvider::Codex => "builtin:codex",
            AgentProvider::Claude => "builtin:claude",
            AgentProvider::Pi => "builtin:pi",
            AgentProvider::Custom => return Err(AgentDomainError::InvalidLauncherKey),
        };
        Ok(Self(value.to_owned()))
    }

    /// 返回存储和跨进程配置使用的稳定 key。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 只接受 canonical UUID 字符形态，避免给 Custom 配置引入第二套 ID 规范。
fn is_canonical_uuid(value: &str) -> bool {
    value.len() == 36
        && value.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}
