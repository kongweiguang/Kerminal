//! 终端会话的领域类型。
//!
//! 这里仅描述会话身份、目标、所有者和生命周期，不持有窗口、PTY 或 SSH 资源。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;

use crate::identity::{AgentSessionId, HostId, PaneId, SessionId, TabId};

pub use crate::files::ContainerRuntime;
pub mod lifecycle;
pub mod ssh_command;
pub use lifecycle::{SessionLifecycleEvent, SessionLifecycleTransitionError};
pub use ssh_command::SshInitialCommand;

/// 终端单元格与可选像素尺寸。
///
/// @author kongweiguang
/// 保持列行尺寸为非零值，避免 PTY 和仿真网格在最小化窗口时进入无效状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct TerminalSize {
    columns: u16,
    rows: u16,
    pixel_width: u16,
    pixel_height: u16,
}

impl TerminalSize {
    /// 校验并创建终端尺寸；像素尺寸允许为零，因为部分 PTY transport 不支持像素度量。
    ///
    /// @author kongweiguang
    pub fn new(
        columns: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    ) -> Result<Self, TerminalSizeError> {
        if columns == 0 || rows == 0 {
            return Err(TerminalSizeError);
        }
        Ok(Self {
            columns,
            rows,
            pixel_width,
            pixel_height,
        })
    }

    /// 提供网格列数，供 PTY resize 与终端渲染共享同一尺寸来源。
    ///
    /// @author kongweiguang
    pub const fn columns(self) -> u16 {
        self.columns
    }

    /// 提供网格行数，避免调用方绕过非零尺寸校验。
    ///
    /// @author kongweiguang
    pub const fn rows(self) -> u16 {
        self.rows
    }

    /// 提供像素宽度；零表示 transport 未提供像素精度。
    ///
    /// @author kongweiguang
    pub const fn pixel_width(self) -> u16 {
        self.pixel_width
    }

    /// 提供像素高度；零表示 transport 未提供像素精度。
    ///
    /// @author kongweiguang
    pub const fn pixel_height(self) -> u16 {
        self.pixel_height
    }
}

impl Default for TerminalSize {
    /// 生成兼容大多数命令行程序的初始网格，避免服务创建时依赖 UI 窗口尺寸。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            columns: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

/// 尺寸无效时返回的轻量领域错误。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalSizeError;

impl fmt::Display for TerminalSizeError {
    /// 使用稳定短消息表达领域约束，避免把终端实现细节泄露到调用层。
    ///
    /// @author kongweiguang
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("terminal columns and rows must be non-zero")
    }
}

impl std::error::Error for TerminalSizeError {}

/// 会话由谁拥有；所有者决定 UI 是否创建 Tab，但不改变终端连接目标。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionOwner {
    Ui { tab_id: TabId, pane_id: PaneId },
    Agent { agent_session_id: AgentSessionId },
    Mcp,
    Service { name: String },
}

/// 本地命令的完整启动参数，避免 Agent、WSL 与 mock CLI 被折叠成隐式 shell 选择。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalSessionTarget {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub working_directory: Option<PathBuf>,
    pub environment: BTreeMap<OsString, OsString>,
}

/// SSH 连接目标只保存主机引用和可选命令，不把任何凭据放进会话请求。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SshSessionTarget {
    pub host_id: HostId,
    /// 请求在远端指定工作目录；SSH provider 负责拒绝不可安全表达的路径。
    pub working_directory: Option<PathBuf>,
    /// 初始 shell 文本或结构化程序调用；两种解释语义由单一枚举保持互斥。
    pub command: Option<SshInitialCommand>,
}

/// 预留的 Telnet 传输目标；具体连接与认证由后续基础设施适配器实现。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TelnetSessionTarget {
    pub host: String,
    pub port: u16,
}

/// 串口数据位；值与常见串口驱动的 5–8 位域一一对应。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SerialDataBits {
    Five,
    Six,
    Seven,
    #[default]
    Eight,
}

/// 串口停止位；不使用浮点数表达 1.5 位，避免跨平台驱动舍入差异。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SerialStopBits {
    #[default]
    One,
    Two,
}

/// 串口奇偶校验模式。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SerialParity {
    #[default]
    None,
    Even,
    Odd,
}

/// 串口流控模式；Software 表示 XON/XOFF，Hardware 表示 RTS/CTS。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SerialFlowControl {
    #[default]
    None,
    Software,
    Hardware,
}

/// 串口目标与线制配置；保留既有字段语义并显式表达 8N1/无流控等冻结配置。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SerialSessionTarget {
    pub device: PathBuf,
    pub baud_rate: u32,
    pub data_bits: SerialDataBits,
    pub stop_bits: SerialStopBits,
    pub parity: SerialParity,
    pub flow_control: SerialFlowControl,
}

impl SerialSessionTarget {
    /// 按冻结配置创建串口目标：9600 baud、8 data bits、1 stop bit、无校验与无流控。
    ///
    /// @author kongweiguang
    pub fn new(device: PathBuf) -> Self {
        Self {
            device,
            baud_rate: 9600,
            data_bits: SerialDataBits::Eight,
            stop_bits: SerialStopBits::One,
            parity: SerialParity::None,
            flow_control: SerialFlowControl::None,
        }
    }
}

/// 预留的容器 exec 目标；不隐式创建或选择本机 shell。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerSessionTarget {
    /// 可选远程宿主引用；缺省时由本地容器运行时执行。
    pub host_id: Option<HostId>,
    /// 运行时类型显式化，避免 Docker 与 Podman 参数被隐式混用。
    pub runtime: ContainerRuntime,
    pub container_id: String,
    pub command: Vec<OsString>,
}

/// 终端 transport 的实际目标，与会话所有者和是否显示 Tab 解耦。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionTarget {
    Local(LocalSessionTarget),
    Ssh(SshSessionTarget),
    Telnet(TelnetSessionTarget),
    Serial(SerialSessionTarget),
    Container(ContainerSessionTarget),
}

/// 进程退出结果；signal 在无法跨平台标准化时保留为可读文本。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionExitStatus {
    pub code: Option<i32>,
    pub signal: Option<String>,
    pub success: bool,
}

/// 会话 runtime 的可观察生命周期。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionLifecycle {
    Starting,
    Running,
    Closing,
    Exited,
    Closed,
    Failed,
}

/// 稳定的会话描述；多个视图共享同一 ID，不会因切换 Tab 重建连接。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionInfo {
    pub session_id: SessionId,
    pub target: SessionTarget,
    pub owner: SessionOwner,
    pub size: TerminalSize,
    pub lifecycle: SessionLifecycle,
    pub exit_status: Option<SessionExitStatus>,
}
