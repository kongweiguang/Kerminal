//! 外部 Agent provider 标识。
//!
//! @author kongweiguang

use serde::{Deserialize, Serialize};

/// Pi 从受信任 cwd 自动读取 `.pi/mcp.json`，因此启动时不覆盖原生配置发现路径。
pub const PI_AGENT_LAUNCH_COMMAND: &str = "pi --approve";
/// 恢复仍以稳定 session cwd 作为 Pi 会话和项目 MCP 配置的共同作用域。
pub const PI_AGENT_RESUME_COMMAND: &str = "pi --approve --continue";

/// 外部 Agent 类型。
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum AgentId {
    /// OpenAI Codex CLI。
    Codex,
    /// Claude Code CLI。
    Claude,
    /// PI coding agent CLI。
    Pi,
    /// 用户自定义命令。
    Custom,
}
