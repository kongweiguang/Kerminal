//! Agent 对指定 PTY 的单次、代次受限文本提交契约。
//!
//! @author kongweiguang

use std::fmt;

use kerminal_domain::identity::{AgentSessionId, SessionId};

use crate::{AppError, AppResult};

/// 一次粘贴加 Enter 共用的正文预算；UI 可预览截断，但提交端不得静默裁剪。
///
/// @author kongweiguang
pub const MAX_AGENT_TERMINAL_SUBMISSION_BYTES: usize = 32 * 1024;

/// Agent 只能提交用户确认的文本快照；runtime 最终仍需对 owner 与 generation 做原子复核。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct AgentTerminalSubmissionRequest {
    pub session_id: SessionId,
    pub agent_session_id: AgentSessionId,
    pub expected_session_generation: u64,
    /// 精确保留用户确认的 preview 内容；不会 trim 或在写入端静默改写。
    pub text: String,
}

impl AgentTerminalSubmissionRequest {
    /// 拒绝无正文、超预算或会产生额外终端控制动作的提交，换行和 Tab 作为可见正文保留。
    ///
    /// @author kongweiguang
    pub fn validate(&self) -> AppResult<()> {
        if self.text.is_empty() {
            return Err(AppError::invalid("Agent 终端提交不能为空"));
        }
        if self.text.len() > MAX_AGENT_TERMINAL_SUBMISSION_BYTES {
            return Err(AppError::invalid("Agent 终端提交超过文本长度限制"));
        }
        if self
            .text
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
        {
            return Err(AppError::invalid("Agent 终端提交含不支持的控制字符"));
        }
        Ok(())
    }
}

impl fmt::Debug for AgentTerminalSubmissionRequest {
    /// Agent 正文可能含用户提示或凭据，只允许日志观察预算，不输出文本。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTerminalSubmissionRequest")
            .field("session_id", &self.session_id)
            .field("agent_session_id", &self.agent_session_id)
            .field(
                "expected_session_generation",
                &self.expected_session_generation,
            )
            .field("text_bytes", &self.text.len())
            .field("text", &"<redacted>")
            .finish()
    }
}
