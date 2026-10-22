//! Provider 自身恢复信息；它是 AgentSession 的兼容元数据，不另建生命周期聚合。
//!
//! @author kongweiguang

use std::{error::Error, fmt};

use serde::{Deserialize, Serialize};

use super::AgentProvider;

/// Provider 文件的领域约束错误，不把它误报为 AgentSession schema 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentProviderSessionError {
    UnsupportedSchemaVersion(u32),
}

impl fmt::Display for AgentProviderSessionError {
    /// 提供不含 provider 命令或会话标识的稳定错误文本。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchemaVersion(version) => {
                write!(formatter, "不支持 Agent provider schema 版本 {version}")
            }
        }
    }
}

impl Error for AgentProviderSessionError {}

/// Agent provider 持久元数据的当前值对象。
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AgentProviderSession {
    /// 当前 provider 文件 schema，固定为 1。
    pub schema_version: u32,
    /// 必须与所属 AgentSession 的 provider 相同。
    pub provider: AgentProvider,
    /// Provider 自身的会话标识；缺失时保持缺失，不从其它字段推断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    /// Provider 自身的显式恢复命令；恢复能力策略由应用层校验。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_command: Option<String>,
    /// 当前 provider 是否声明支持显式恢复。
    pub resume_supported: bool,
    /// 最近一次恢复时间；历史文件缺少该值时仍保持 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_resume_at: Option<String>,
}

impl fmt::Debug for AgentProviderSession {
    /// provider 命令与会话标识可能含用户数据，调试输出只暴露存在性和稳定枚举。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentProviderSession")
            .field("schema_version", &self.schema_version)
            .field("provider", &self.provider)
            .field(
                "provider_session_id",
                &self.provider_session_id.as_ref().map(|_| "<redacted>"),
            )
            .field(
                "resume_command",
                &self.resume_command.as_ref().map(|_| "<redacted>"),
            )
            .field("resume_supported", &self.resume_supported)
            .field(
                "last_resume_at",
                &self.last_resume_at.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl AgentProviderSession {
    /// 只接受现行持久格式，避免新版本静默重写未识别的 provider schema。
    pub fn validate(&self) -> Result<(), AgentProviderSessionError> {
        if self.schema_version != 1 {
            return Err(AgentProviderSessionError::UnsupportedSchemaVersion(
                self.schema_version,
            ));
        }
        Ok(())
    }
}
