//! 当前 Shell 编辑缓冲的能力与版本化快照，不改变命令生命周期契约。
//!
//! @author kongweiguang

use std::fmt;

use super::{TerminalCommandContextVersion, UiSessionBinding};

/// 与终端协议解析共用同一标量上限，保证展示的候选可由真实 Shell 快照承载。
pub const SHELL_EDITING_MAX_INPUT_CHARACTERS: usize =
    kerminal_terminal::shell_editing::MAX_SHELL_EDITING_INPUT_CHARACTERS;

/// 单次追加的 UTF-8 字节上限；字符数限制与该上限分别约束多字节后缀。
pub const SHELL_EDITING_MAX_SUFFIX_BYTES: usize = 4_096;

/// 快照拒绝只针对当前过长输入；调用方据此提示缩短，不把连接标为不可用。
pub const SHELL_EDITING_INPUT_TOO_LONG_CONTEXT: &str = "Shell 当前输入过长，缩短后可恢复命令建议";

/// 只有实际安装的交互适配器可报告精确 Shell，不能把 POSIX 方言猜成 Bash。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellEditingShell {
    Bash,
    Zsh,
    Fish,
    PowerShell,
}

/// 可读编辑缓冲与原有 OSC 命令生命周期是独立能力；准备中不能当作可用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellEditingAvailability {
    Pending,
    Available,
    Unavailable(ShellEditingUnavailableReason),
}

/// 不支持、绑定冲突与当前 prompt 不可证明要分别处理，避免错误触发额外输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellEditingUnavailableReason {
    Unsupported,
    IntegrationNotObserved,
    KeyBindingConflict,
    PromptNotReady,
    ProbeTimedOut,
    SessionChanged,
}

/// 仅表示一次读回确认的 Shell 编辑状态，不能从屏幕或 UI 侧输入投影构造。
///
/// `cursor` 是 Unicode 标量偏移；`context` 与 `binding` 一起隔离重连和旧 prompt。
/// 正文与目录可能包含凭据或私人路径，因此 `Debug` 不输出它们。
#[derive(Clone, Eq, PartialEq)]
pub struct ShellEditingSnapshot {
    pub binding: UiSessionBinding,
    pub context: TerminalCommandContextVersion,
    /// 由会话内成功接收的快照单调递增；即使输入代次相同也不可复用旧读回。
    pub revision: u64,
    pub input: String,
    pub cursor: usize,
    pub cwd: Option<String>,
    pub shell: ShellEditingShell,
    pub shell_version: Option<String>,
    /// None 表示当前适配器无法证明原生预测是否绘制，不应把未知当作 false。
    pub shell_prediction_visible: Option<bool>,
}

impl ShellEditingSnapshot {
    /// 只允许逻辑行末追加，避免把 Shell 光标索引解释成字节位置后覆盖用户输入。
    ///
    /// @author kongweiguang
    pub fn cursor_at_end(&self) -> bool {
        self.cursor == self.input.chars().count()
    }
}

impl fmt::Debug for ShellEditingSnapshot {
    /// 默认诊断不暴露正在输入的命令或目录；调用方必须显式读取才能使用正文。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShellEditingSnapshot")
            .field("binding", &self.binding)
            .field("context", &self.context)
            .field("revision", &self.revision)
            .field("input", &"[redacted]")
            .field("cursor", &self.cursor)
            .field("cwd", &self.cwd.as_ref().map(|_| "[redacted]"))
            .field("shell", &self.shell)
            .field("shell_version", &self.shell_version)
            .field("shell_prediction_visible", &self.shell_prediction_visible)
            .finish()
    }
}
