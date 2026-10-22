//! MCP 运行控制的应用端口与 UI 安全状态投影。
//!
//! @author kongweiguang

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::AppResult;

/// 应用用例间共享的绝对截止时间类型，避免 UI crate 直接依赖 Tokio。
pub use tokio::time::Instant;

/// MCP listener 的可观察阶段；Stopping 表示清理仍有 owner，调用方可继续 stop 重试。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpRuntimeState {
    Unavailable,
    Stopped,
    Starting,
    Synchronizing,
    Running,
    Stopping,
    Failed,
}

/// 不含 listener、session 或基础设施句柄的 MCP 运行态快照。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpRuntimeSnapshot {
    pub state: McpRuntimeState,
    /// 当前 owner 实际绑定的 loopback endpoint；停止超时期间保留用于诊断。
    pub endpoint: Option<String>,
    /// 仅在 Running 阶段提供可复制配置，避免复制已经停止或正在关闭的地址。
    pub client_json: Option<String>,
    /// Listener 与 Agent workspace endpoint 尚在同步时为 true，供 UI 提供准确重试动作。
    pub endpoint_sync_pending: bool,
    /// 仅在服务未装配时提供不可用原因。
    pub unavailable_reason: Option<String>,
    /// 启动失败或停止未完成时的脱敏原因，不包含底层 source 文本。
    pub failure_reason: Option<String>,
}

impl McpRuntimeSnapshot {
    /// 构造无服务能力时的显式状态，让 UI 不必把缺失依赖显示成已停止。
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            state: McpRuntimeState::Unavailable,
            endpoint: None,
            client_json: None,
            endpoint_sync_pending: false,
            unavailable_reason: Some(reason.into()),
            failure_reason: None,
        }
    }

    /// 构造进程内尚未启动 MCP listener 的稳定初态。
    pub fn stopped() -> Self {
        Self {
            state: McpRuntimeState::Stopped,
            endpoint: None,
            client_json: None,
            endpoint_sync_pending: false,
            unavailable_reason: None,
            failure_reason: None,
        }
    }
}

/// MCP runtime 的唯一应用边界；具体 listener owner 由 kerminal-mcp adapter 管理。
#[async_trait]
pub trait McpRuntimeControl: Send + Sync {
    /// 读取不等待长操作的最近状态快照，供面板在启动或关闭期间继续刷新。
    fn snapshot(&self) -> McpRuntimeSnapshot;

    /// 确保唯一 listener 启动；取消令牌用于在安全边界请求启动后立即收束。
    async fn start(&self, cancellation: CancellationToken) -> AppResult<McpRuntimeSnapshot>;

    /// 在调用方统一 deadline 内停止 listener；失败或超时必须保留 handle 供再次尝试。
    async fn stop_until(&self, deadline: Instant) -> AppResult<McpRuntimeSnapshot>;

    /// 同步拒绝后续 MCP runtime 操作并通知活动操作进入退出收尾。
    fn begin_shutdown(&self);

    /// 用桌面共享绝对 deadline 收尾；超时时实现必须继续持有未完成资源 owner。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()>;
}
