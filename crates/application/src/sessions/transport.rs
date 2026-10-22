//! 由本地 PTY、SSH channel 或容器 exec 提供的已拥有异步 I/O。
//!
//! @author kongweiguang

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::sessions::{SessionExitStatus, TerminalSize};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};
use kerminal_domain::sessions::SessionTarget;

/// 未能交付 session transport 时仍需由 SessionManager 回收的资源 owner。
///
/// @author kongweiguang
#[async_trait]
pub trait SessionOpenCleanupOwner: Send + Sync {
    /// 在共享绝对 deadline 内关闭本次 open 已取得的资源，超时后必须继续持有句柄供后续重试。
    ///
    /// owner 仅关闭资源，不提供会话退出状态；重复调用必须安全，且不得关闭 provider 共享连接。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()>;
}

/// SessionTransportFactory 在资源回收失败时返回主错误和可重试 cleanup owner。
///
/// cleanup 为 None 表示没有取得需要保留的资源，或 provider 已完成清理；Some 表示 manager
/// 必须接管 owner，直到 `close_until` 成功后才能释放同一 session slot。
///
/// @author kongweiguang
#[must_use = "open cleanup owners must be retained by SessionManager"]
pub struct SessionTransportOpenError {
    error: AppError,
    cleanup: Option<Arc<dyn SessionOpenCleanupOwner>>,
}

impl SessionTransportOpenError {
    /// 构造没有待回收资源的失败，例如目标校验失败或 provider 已同步完成 rollback。
    ///
    /// @author kongweiguang
    pub fn new(error: AppError) -> Self {
        Self {
            error,
            cleanup: None,
        }
    }

    /// 返回 primary error 并交付唯一 cleanup owner，避免把活动资源藏进错误 source 后丢失句柄。
    ///
    /// @author kongweiguang
    pub fn with_cleanup(error: AppError, cleanup: Arc<dyn SessionOpenCleanupOwner>) -> Self {
        Self {
            error,
            cleanup: Some(cleanup),
        }
    }

    /// 把主错误与可选 owner 一起交给 SessionManager，由同一 session ID 登记和重试回收。
    ///
    /// @author kongweiguang
    pub fn into_parts(self) -> (AppError, Option<Arc<dyn SessionOpenCleanupOwner>>) {
        (self.error, self.cleanup)
    }
}

impl From<AppError> for SessionTransportOpenError {
    /// 让未取得资源的早退仍可直接使用 `?`，同时保持错误类别不变。
    ///
    /// @author kongweiguang
    fn from(error: AppError) -> Self {
        Self::new(error)
    }
}

impl std::fmt::Debug for SessionTransportOpenError {
    /// 记录稳定主错误和 owner 是否存在，不格式化 owner 内部句柄或连接数据。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionTransportOpenError")
            .field("error", &self.error)
            .field("has_cleanup_owner", &self.cleanup.is_some())
            .finish()
    }
}

impl std::fmt::Display for SessionTransportOpenError {
    /// 保留 primary AppError 的稳定公开文案，不对调用方暴露底层 endpoint 或资源 owner。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, formatter)
    }
}

impl std::error::Error for SessionTransportOpenError {
    /// 保留主错误根因；cleanup owner 由 `into_parts` 显式交给 manager，不藏入错误链。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// 已拆分的 owned stream 与 transport 生命周期控制。
///
/// @author kongweiguang
/// reader/writer 的所有权转给唯一 SessionService，避免 SSH/UI/MCP 分别维护读写循环。
pub struct SessionTransport {
    pub reader: Pin<Box<dyn AsyncRead + Send + 'static>>,
    pub writer: Pin<Box<dyn AsyncWrite + Send + 'static>>,
    pub control: Arc<dyn SessionTransportControl>,
    /// Provider 已从显式目标或已验证连接配置确认的 shell 方言；未知时不允许拼接受管命令。
    pub shell_dialect: RemoteShellDialect,
    /// PTY adapter 可观察的本机 child PID；远端 channel 或不暴露进程号的 provider 留空。
    pub process_id: Option<u32>,
    /// Provider 确认 hook 已准备或已注入；不代表 PTY 已送达首次 prompt marker。
    pub shell_integration_enabled: bool,
}

impl SessionTransport {
    /// 从拆分后的异步 stream 组装 transport，统一 ownership 与关闭入口。
    ///
    /// @author kongweiguang
    pub fn new<R, W, C>(reader: R, writer: W, control: C) -> Self
    where
        R: AsyncRead + Send + 'static,
        W: AsyncWrite + Send + 'static,
        C: SessionTransportControl + 'static,
    {
        Self {
            reader: Box::pin(reader),
            writer: Box::pin(writer),
            control: Arc::new(control),
            shell_dialect: RemoteShellDialect::Unknown,
            process_id: None,
            shell_integration_enabled: false,
        }
    }

    /// 标记 provider 已确认的 shell 语法，避免 workflow 调用方猜测远端默认 shell。
    ///
    /// @author kongweiguang
    pub fn with_shell_dialect(mut self, shell_dialect: RemoteShellDialect) -> Self {
        self.shell_dialect = shell_dialect;
        self
    }

    /// 附加 provider 实际取得的本机进程号，避免应用层按 shell 命令猜 PID。
    ///
    /// @author kongweiguang
    pub fn with_process_id(mut self, process_id: Option<u32>) -> Self {
        self.process_id = process_id;
        self
    }

    /// 记录 provider 已完成 shell hook bootstrap，供 runtime 状态区分未启用与尚未观测。
    ///
    /// @author kongweiguang
    pub fn with_shell_integration_enabled(mut self, enabled: bool) -> Self {
        self.shell_integration_enabled = enabled;
        self
    }
}

/// transport 可声明的本地终端 resize 能力；应用端口不依赖 terminal 实现细节。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TerminalResizeCapability {
    /// SSH、Unix PTY 及未声明能力的 transport 保持默认 Alacritty 策略。
    #[default]
    Generic,
    /// transport 确认使用 Windows 本地 ConPTY 的 viewport-growth 语义。
    WindowsConPty,
}

/// PTY/channel 的 resize、退出等待和关闭能力。
///
/// @author kongweiguang
#[async_trait]
pub trait SessionTransportControl: Send + Sync {
    /// 返回由实际 PTY 后端证明的 resize 能力；默认 generic 保护所有既有 adapter。
    ///
    /// @author kongweiguang
    fn terminal_resize_capability(&self) -> TerminalResizeCapability {
        TerminalResizeCapability::Generic
    }

    /// 更新实际 transport 的窗口大小，保证 TUI 收到与网格一致的尺寸。
    async fn resize(&self, size: TerminalSize) -> AppResult<()>;

    /// 独立观察本地 child 的退出，不等待 PTY 输出 EOF 或 I/O worker 回收。
    ///
    /// `Some` 只确认原生 child 状态；调用方仍须 drain 输出并通过 `wait_exit` 完成资源回收。
    /// 默认 `None` 表示该协议没有独立 child 退出信号，应继续以 reader EOF 为准，避免远端
    /// channel 的关闭语义被误套成本地进程语义。
    ///
    /// @author kongweiguang
    async fn observe_child_exit(&self) -> AppResult<Option<SessionExitStatus>> {
        Ok(None)
    }

    /// Native child 已退出后，只释放 PTY master/HPCON 并保留输出 reader drain。
    ///
    /// 该路径不能复用 `close_until`：用户主动关闭会终止进程树并允许结束态丢弃队列，
    /// 而自然退出必须保留已确认后代输出。默认明确拒绝，避免实现了 child 观察却静默漏掉
    /// 必需的 PTY 生命周期收口。
    ///
    /// @author kongweiguang
    async fn release_after_child_exit_until(&self, _deadline: Instant) -> AppResult<()> {
        Err(AppError::unsupported(
            "终端 transport 不支持 native child 退出后的 PTY 释放",
        ))
    }

    /// 按调用方共享的绝对 deadline 关闭当前 PTY/channel，适配器负责保留超时后的清理所有权。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: Instant) -> AppResult<()>;

    /// 为普通单会话关闭提供有限默认预算；全局 shutdown 应直接传递其统一 deadline。
    ///
    /// @author kongweiguang
    async fn close(&self) -> AppResult<()> {
        self.close_until(Instant::now() + Duration::from_secs(5))
            .await
    }

    /// 等待 child/channel 最终退出状态，供 SessionInfo 和关闭流程确认回收。
    async fn wait_exit(&self) -> AppResult<SessionExitStatus>;
}

/// 按显式目标建立一个 owned 终端通道的应用端口。
///
/// @author kongweiguang
/// 多协议 provider 在 desktop composition root 组合；该端口让 UI、MCP 和 Agent 共用同一创建路径。
#[async_trait]
pub trait SessionTransportFactory: Send + Sync {
    /// 建立目标 transport；取消只释放本次新开的通道，不应关闭 provider 共享的连接。
    ///
    /// SessionManager 会保留此 future，直到其返回结果，不能因调用方取消而 drop 它。
    /// Provider 返回错误前必须回收尚未交付的 PTY/channel；返回成功后 transport 的关闭所有权
    /// 转交给 manager，即使创建请求已取消，manager 也会关闭迟到的成功结果。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError>;
}

/// 把通用 transport 错误提升为应用边界统一可读的错误。
///
/// @author kongweiguang
pub fn io_transport_error(error: io::Error) -> crate::AppError {
    crate::AppError::infrastructure_with_source("终端 I/O 操作失败", error)
}
