//! MCP 监听设置及无秘密的持久化契约。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

use crate::{AppError, AppResult};

/// 控制实际监听范围；本机 Agent 始终通过回环地址访问同一服务。
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum McpBindMode {
    #[default]
    Loopback,
    Lan,
}

/// 仅保留 Vault 引用，不把局域网令牌正文放入普通设置文件。
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", default)]
pub struct McpSettings {
    pub bind_mode: McpBindMode,
    pub port: u16,
    pub token_ref: Option<String>,
}

impl Default for McpSettings {
    /// 旧文件缺少本节时保持既有本机入口，不在读取时创建令牌。
    fn default() -> Self {
        Self {
            bind_mode: McpBindMode::Loopback,
            port: 37657,
            token_ref: None,
        }
    }
}

impl McpSettings {
    /// 用户输入端口必须是确定的监听端口；零仅用于内部临时测试 listener。
    pub fn validated(self) -> AppResult<Self> {
        if self.port == 0 {
            return Err(AppError::invalid("MCP 端口需要在 1 到 65535 之间"));
        }
        if self
            .token_ref
            .as_ref()
            .is_some_and(|reference| reference != crate::mcp_runtime::MCP_TOKEN_REFERENCE)
        {
            return Err(AppError::invalid("MCP 令牌引用无效"));
        }
        if self.bind_mode == McpBindMode::Lan && self.token_ref.is_none() {
            return Err(AppError::invalid("局域网 MCP 需要已保存的访问令牌"));
        }
        Ok(self)
    }
}
