//! SSH 会话的初始命令语义；结构化进程参数保留 argv 边界。
//!
//! @author kongweiguang

use std::fmt;

/// 描述 SSH PTY 建立后要执行的初始动作，不在领域层拼接解释器命令。
///
/// `Shell` 保留调用方明确提供的 shell 文本语义；`Process` 保留程序与参数边界，
/// 由 SSH adapter 按连接已确认的方言安全编码。Debug 不输出命令数据，避免外部参数或
/// shell 内容进入诊断日志。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub enum SshInitialCommand {
    /// 调用方明确选择的远端 shell 文本。
    Shell(String),
    /// 结构化的远端程序调用；adapter 负责拒绝空程序或无法安全编码的方言。
    Process { program: String, args: Vec<String> },
}

impl fmt::Debug for SshInitialCommand {
    /// 只显示命令形态和参数数量，避免日志泄露 shell 文本或 argv 内容。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shell(_) => formatter.debug_tuple("Shell").field(&"[redacted]").finish(),
            Self::Process { args, .. } => formatter
                .debug_struct("Process")
                .field("program", &"[redacted]")
                .field("argument_count", &args.len())
                .finish(),
        }
    }
}
