//! OSC 133 command-block 的安全应用投影；正文仅接受带上下文的 UI 写入确认。
//!
//! @author kongweiguang

use std::fmt;
use std::sync::Arc;

use kerminal_domain::commands::NormalizedCommandHistory;
use kerminal_domain::identity::SessionId;
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyAlternates,
};

/// 每条运行中 session 仅保留最新的有限命令块，避免长会话无界增长。
///
/// @author kongweiguang
pub const COMMAND_BLOCK_RETENTION_LIMIT: usize = 128;

/// 保留正文只供短暂关联 UI 已确认输入；字节上限也防止异常 projection 长期占用 session 内存。
///
/// @author kongweiguang
pub const MAX_COMMAND_BLOCK_TEXT_BYTES: usize = 32 * 1024;

/// 单次输出查询的上限与 terminal engine 每块保留预算对齐，避免 UI 请求扩大常驻内存。
///
/// @author kongweiguang
pub const MAX_COMMAND_BLOCK_OUTPUT_BYTES: usize = 256 * 1024;

/// 单个命令块在同一 session 重连代次内的稳定身份。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CommandBlockId {
    pub session_generation: u64,
    pub prompt_epoch: u64,
    pub ordinal: u64,
}

/// 命令正文的可见状态；未被可信 UI 写入确认时明确保持未知。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub enum CommandBlockText {
    Known(String),
    Unknown(CommandBlockTextUnknownReason),
}

impl fmt::Debug for CommandBlockText {
    /// Debug 输出不复制命令正文，避免调用方误把它写入诊断日志。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Known(_) => formatter.write_str("Known(<redacted>)"),
            Self::Unknown(reason) => formatter.debug_tuple("Unknown").field(reason).finish(),
        }
    }
}

/// 正文不能安全展示或无法证明时的稳定原因。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockTextUnknownReason {
    NotCaptured,
    SensitiveOrUnsafe,
    WriteNotAcknowledged,
}

/// 命令块的执行状态，只由真实 OSC 133 C/D 和 session 生命周期推进。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockState {
    Running,
    Finished { exit_code: Option<i32> },
    Unknown(CommandBlockUnknownReason),
}

/// 命令未观察到对应 D 或其 session 被打断时的稳定状态原因。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockUnknownReason {
    Superseded,
    Disconnected,
    ReaderFailed,
    WriterFailed,
    Closed,
    Reconnected,
}

/// 只含真实命令边界、可选的安全 UI 正文及退出码，不缓存终端输出或 prompt。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandBlock {
    pub id: CommandBlockId,
    pub command: CommandBlockText,
    pub state: CommandBlockState,
}

/// 单条 SessionService 的权威有界快照；订阅事件只负责提醒重新读取此快照。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandBlockSnapshot {
    pub session_id: SessionId,
    pub session_generation: u64,
    pub revision: u64,
    pub blocks: Vec<CommandBlock>,
}

/// Transcript 查询只允许在一次命令块创建的边界内返回文本，不使用终端当前滚动缓冲区补猜。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockOutputIncompleteReason {
    Superseded,
    Closed,
    Disconnected,
    Reconnected,
}

/// 输出快照状态与 OSC 133 生命周期对齐；未捕获和容量驱逐均有显式结果。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandBlockOutputState {
    NotCaptured,
    Capturing,
    Complete { exit_code: Option<i32> },
    Incomplete(CommandBlockOutputIncompleteReason),
    Evicted,
}

/// 短期运行态 transcript 正文；只通过显式 accessor 给 UI 使用，格式化始终脱敏。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct CommandBlockOutputText(Arc<str>);

impl CommandBlockOutputText {
    /// 只包装 terminal engine 已限制长度的转录片段，供 infrastructure adapter 原样投影。
    ///
    /// @author kongweiguang
    pub fn from_capture(text: Arc<str>) -> Self {
        Self(text)
    }

    /// 将受字节预算约束的文本借给显式展示/复制调用方，不额外复制正文。
    ///
    /// @author kongweiguang
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CommandBlockOutputText {
    /// Transcript 可能含命令输出中的凭据，Debug 仅公开其存在性与字节数。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandBlockOutputText")
            .field("bytes", &self.0.len())
            .field("text", &"<redacted>")
            .finish()
    }
}

/// 按命令块身份查询的有限 transcript；内容不落盘，超预算截断与驱逐可见。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct CommandBlockOutputSnapshot {
    pub session_id: SessionId,
    pub block_id: CommandBlockId,
    pub state: CommandBlockOutputState,
    pub capture_revision: Option<u64>,
    pub text: Option<CommandBlockOutputText>,
    pub captured_bytes: usize,
    pub returned_bytes: usize,
    pub truncated: bool,
}

impl fmt::Debug for CommandBlockOutputSnapshot {
    /// 身份和预算可供诊断，transcript 字段不进入日志或错误格式化。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CommandBlockOutputSnapshot")
            .field("session_id", &self.session_id)
            .field("block_id", &self.block_id)
            .field("state", &self.state)
            .field("capture_revision", &self.capture_revision)
            .field("text", &self.text.as_ref().map(|_| "<redacted>"))
            .field("captured_bytes", &self.captured_bytes)
            .field("returned_bytes", &self.returned_bytes)
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// 供可见 UI 在提交事件时附带自己维护的精确命令行投影。
///
/// 该正文仅在内存中短暂等待 writer ACK 与 OSC 133 C 关联；确认后只进入有界运行态快照，不落盘或写日志。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct UiTerminalCommandSubmissionRequest {
    pub input: super::UiTerminalInputRequest,
    pub command: String,
}

impl fmt::Debug for UiTerminalCommandSubmissionRequest {
    /// 输入事件和命令正文都可能含敏感文本，Debug 只保留类型标记。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UiTerminalCommandSubmissionRequest")
            .field("input", &"<redacted>")
            .field("command", &"<redacted>")
            .finish()
    }
}

/// 将 UI 提交正文限制为单行、有限长度并沿用历史命令的凭据过滤规则。
///
/// @author kongweiguang
pub fn classify_command_block_text(command: &str) -> CommandBlockText {
    if command.len() > MAX_COMMAND_BLOCK_TEXT_BYTES || command.chars().any(char::is_control) {
        return CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe);
    }
    let Ok(normalized) = NormalizedCommandHistory::new(command) else {
        return CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe);
    };
    if normalized.sensitive_skip_reason().is_some() {
        return CommandBlockText::Unknown(CommandBlockTextUnknownReason::SensitiveOrUnsafe);
    }
    CommandBlockText::Known(normalized.command)
}

/// 仅把没有修饰键副作用的 Enter 识别为正文候选提交，不从 paste 或屏幕内容猜测。
///
/// @author kongweiguang
pub fn is_plain_command_enter(event: &TerminalInputEvent) -> bool {
    let plain_enter = |key: TerminalKey, phase: KeyPhase, modifiers: InputModifiers| {
        key == TerminalKey::Enter
            && phase == KeyPhase::Press
            && modifiers == InputModifiers::default()
    };
    match event {
        TerminalInputEvent::Key(event) => plain_enter(event.key, event.phase, event.modifiers),
        TerminalInputEvent::KeyWithText {
            event,
            text,
            alternates,
        } => {
            plain_enter(event.key, event.phase, event.modifiers)
                && matches!(text.as_str(), "" | "\r" | "\n")
                && *alternates == TerminalKeyAlternates::default()
        }
        TerminalInputEvent::Text(text) => matches!(text.as_str(), "\r" | "\n" | "\r\n"),
        TerminalInputEvent::Paste(_)
        | TerminalInputEvent::Mouse(_)
        | TerminalInputEvent::Focus(_) => false,
    }
}
