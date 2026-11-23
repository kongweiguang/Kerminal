//! 可见 SSH 分屏的传输投影和用户操作。
//!
//! @author kongweiguang

use std::path::PathBuf;

/// 同一错误上下文供会话门禁和 UI 使用，只有自动焦点报告可以静默丢弃。
pub const ZMODEM_INPUT_BUSY: &str = "当前终端正在进行 ZMODEM 传输";

/// 方向以用户的本机为基准，与远端命令接收或发送的方向相反。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZmodemDirection {
    Upload,
    Download,
}

/// 将收尾和需要重连区分开，只有确认协议让出字节流才恢复终端输入。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ZmodemPhase {
    WaitingForSelection,
    Transferring,
    Cancelling,
    Finished,
    Cancelled,
    Failed,
    NeedsReconnect,
}

/// UI 只读取不可变投影，确认和取消仍由会话 runtime 串行处理。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ZmodemStatus {
    pub id: u64,
    pub direction: ZmodemDirection,
    pub phase: ZmodemPhase,
    pub file_name: Option<String>,
    pub bytes: u64,
    pub total: Option<u64>,
    pub completed: u32,
    pub error: Option<&'static str>,
    pub save_directory: Option<PathBuf>,
}

/// 文件授权仅来自对应分屏选择器，协议文字本身不能授予本地路径访问权。
#[derive(Debug)]
pub enum ZmodemDecision {
    Upload(Vec<PathBuf>),
    Download(PathBuf),
    Cancel,
    Dismiss,
}
