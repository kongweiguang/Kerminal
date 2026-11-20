//! MCP 运行控制的应用端口与 UI 安全状态投影。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_domain::preferences::ConfigRevision;
use std::net::Ipv4Addr;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::{
    AppError, AppResult,
    preferences::settings::{McpBindMode, McpSettings, VersionedSettings},
};

/// 服务专用的稳定 Vault 标识，不借用任何用户主机身份。
pub const MCP_TOKEN_REFERENCE: &str = "service:kerminal:mcp-token:v1";

/// 令牌的明文只在需要建立 listener 或复制连接配置时短暂持有。
pub struct McpAccessToken {
    reference: String,
    secret: Zeroizing<String>,
}

impl McpAccessToken {
    /// 仅接收服务专属引用，避免 UI 将任意 Vault 条目作为网络访问令牌。
    pub fn new(reference: String, secret: Zeroizing<String>) -> AppResult<Self> {
        if reference != MCP_TOKEN_REFERENCE
            || secret.is_empty()
            || secret.len() > 256
            || !secret.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
            })
        {
            return Err(AppError::invalid("MCP 访问令牌无效"));
        }
        Ok(Self { reference, secret })
    }

    /// 引用可进入设置，但不能代替访问令牌作为认证材料。
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// 只允许认证和显式复制入口读取令牌正文。
    pub fn secret(&self) -> &str {
        self.secret.as_str()
    }
}

impl std::fmt::Debug for McpAccessToken {
    /// 不向日志或错误回执暴露连接凭据。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("McpAccessToken([redacted])")
    }
}

/// 调用方显式复制时使用的临时文本，不允许落入运行态快照。
pub struct McpClientConfig(Zeroizing<String>);

impl McpClientConfig {
    /// 在应用边界包装含令牌的客户端配置，避免普通 Debug 打印。
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    /// 仅供剪贴板入口读取；释放时清除应用持有的缓冲区。
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl std::fmt::Debug for McpClientConfig {
    /// 按敏感值处理复制内容，避免错误路径记录完整配置。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("McpClientConfig([redacted])")
    }
}

/// 专用 Vault 端口只为 MCP 创建或读取令牌，不暴露配置 CRUD 工具。
#[async_trait]
pub trait McpTokenStore: Send + Sync {
    /// 缺少引用只允许显式启用局域网时创建；已有引用必须严格读取原令牌。
    async fn load_or_create(&self, reference: Option<&str>) -> AppResult<McpAccessToken>;
}

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
    /// 当前 listener 的监听范围；Stopped/Unavailable 时回到默认本机范围，不表示仍在监听。
    pub bind_mode: McpBindMode,
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
            bind_mode: McpBindMode::Loopback,
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
            bind_mode: McpBindMode::Loopback,
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
    ///
    /// 这是按需启动入口（例如 Agent 请求），不写入 `autoStart`；用户手动启停使用
    /// `start_manually`，避免一次按需使用被固化成下次启动行为。
    async fn start(&self, cancellation: CancellationToken) -> AppResult<McpRuntimeSnapshot>;

    /// 在调用方统一 deadline 内停止 listener；失败或超时必须保留 handle 供再次尝试。
    async fn stop_until(&self, deadline: Instant) -> AppResult<McpRuntimeSnapshot>;

    /// 桌面启动时按持久化意图恢复一次；实现必须在配置门内只读取一次已验证设置，
    /// 再以该快照启动，避免读到 `true` 后普通启动又按外部新值重新解释。
    ///
    /// 不拥有持久配置的适配器保持当前快照；生产 adapter 覆盖此方法。
    async fn restore_on_startup(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<McpRuntimeSnapshot> {
        Ok(self.snapshot())
    }

    /// 用户手动启动：先以 CAS 确认并持久化 `autoStart = true`，再复用已有启动流程。
    ///
    /// 启动失败（包括端口占用）仍保留已确认的意图，下一次启动继续尝试；取消只能发生在
    /// 触碰 listener 之前，已开始的设置提交不会被中途丢弃。
    async fn start_manually(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<McpRuntimeSnapshot>;

    /// 用户手动停止：先以 CAS 确认并持久化 `autoStart = false`，再在统一 deadline 内收尾。
    ///
    /// 未能确认停止意图时不得先关 listener；`Failed` 且没有 listener 时也允许清理失败状态。
    async fn stop_manually_until(&self, deadline: Instant) -> AppResult<McpRuntimeSnapshot>;

    /// 在同一 owner 内应用监听设置并按指定 revision 保存，失败需保留可观察的恢复状态。
    async fn apply_settings(
        &self,
        _settings: McpSettings,
        _expected_revision: Option<ConfigRevision>,
        _cancellation: CancellationToken,
    ) -> AppResult<VersionedSettings> {
        Err(AppError::unsupported("MCP 监听设置不可用"))
    }

    /// 返回当前机器可供局域网客户端使用的 IPv4 地址，不将 0.0.0.0 当作目标。
    async fn lan_addresses(&self) -> AppResult<Vec<Ipv4Addr>> {
        Err(AppError::unsupported("MCP 局域网地址不可用"))
    }

    /// 显式复制时才生成包含令牌的客户端配置，普通快照始终保持无秘密。
    async fn lan_client_config(&self, _address: Ipv4Addr) -> AppResult<McpClientConfig> {
        Err(AppError::unsupported("MCP 局域网客户端配置不可用"))
    }

    /// 同步拒绝后续 MCP runtime 操作并通知活动操作进入退出收尾。
    fn begin_shutdown(&self);

    /// 用桌面共享绝对 deadline 收尾；超时时实现必须继续持有未完成资源 owner。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()>;
}
