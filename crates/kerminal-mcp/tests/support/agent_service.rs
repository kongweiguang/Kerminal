//! Scope 回归测试使用的最小 Agent 查询用例。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    agents::{
        AgentHistoryProjection, AgentHistoryRequest, AgentLauncherCatalog, AgentService,
        AgentSessionRecordProjection, AgentSessionView, AgentTargetContext, AgentTargetResolution,
        AgentTerminalSnapshotRequest, AgentTextPreview, AgentTextSubmissionReceipt,
        CreateAgentSessionFromHistoryRequest, CreateAgentSessionRequest,
        RenameAgentSessionTitleRequest, SendAgentCommandRequest,
    },
};
use kerminal_domain::{agents::AgentTargetBinding, identity::AgentSessionId};
use tokio_util::sync::CancellationToken;

/// HTTP endpoint middleware 只允许读取固定 Agent，所有未涉及用例都明确拒绝。
pub struct FixedAgentService {
    view: AgentSessionView,
}

impl FixedAgentService {
    /// 固定一个真实有效状态投影，供路由中间件验证 session-scoped endpoint。
    pub fn new(view: AgentSessionView) -> Self {
        Self { view }
    }
}

#[async_trait]
impl AgentService for FixedAgentService {
    /// 本测试不改写受管 Agent 的 MCP endpoint。
    async fn configure_mcp_endpoint(
        &self,
        _endpoint: Option<String>,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        unused_operation()
    }

    /// 此测试只验证固定 Agent 的 scope，不查询 Launcher 配置。
    async fn launcher_catalog(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_operation()
    }

    /// 此测试不保存 Launcher 选择。
    async fn select_launcher(
        &self,
        _key: kerminal_domain::agents::AgentLauncherKey,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        unused_operation()
    }

    /// 本测试只验证现有 Agent 的工具 scope，不创建 Agent。
    async fn create(
        &self,
        _request: CreateAgentSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// 本测试不查询 Agent 列表。
    async fn list(&self, _cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>> {
        unused_operation()
    }

    /// Scope fake 不查询历史筛选投影。
    async fn history(
        &self,
        _request: AgentHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection> {
        unused_operation()
    }

    /// Scope fake 不创建基于历史启动快照的新会话。
    async fn create_from_history(
        &self,
        _request: CreateAgentSessionFromHistoryRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// Scope fake 不改名历史 Agent。
    async fn rename_title(
        &self,
        _request: RenameAgentSessionTitleRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// 返回固定有效 Agent；其它 ID 不得映射到这个 endpoint owner。
    async fn get(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("test Agent lookup cancelled"));
        }
        if agent_session_id != self.view.session.agent_session_id {
            return Err(AppError::not_found("test Agent not found"));
        }
        Ok(self.view.clone())
    }

    /// 本测试不读取 Agent 记录投影。
    async fn record_projection(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionRecordProjection> {
        unused_operation()
    }

    /// 本测试不恢复 Agent CLI。
    async fn resume(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// 本测试不归档 Agent。
    async fn archive(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// 本测试不改变 Agent 首选终端。
    async fn bind_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _binding: AgentTargetBinding,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        unused_operation()
    }

    /// 本测试不解析 Agent 的首选终端。
    async fn resolve_preferred_target(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        unused_operation()
    }

    /// 本测试不读取 Agent 目标上下文。
    async fn target_context(
        &self,
        _agent_session_id: AgentSessionId,
        _max_snapshot_bytes: usize,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext> {
        unused_operation()
    }

    /// 此固定 fake 只服务 endpoint scope 查询，不提供未测试的文本预览能力。
    ///
    /// @author kongweiguang
    async fn preview_text(
        &self,
        _agent_session_id: AgentSessionId,
        _text: String,
        _max_bytes: usize,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview> {
        unused_operation()
    }

    /// 此固定 fake 不控制 Agent CLI writer，避免把 MCP scope 测试误作真实发送验证。
    ///
    /// @author kongweiguang
    async fn send_command_block_text(
        &self,
        _request: SendAgentCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt> {
        unused_operation()
    }

    /// 本测试不记录 Agent 终端快照。
    async fn record_terminal_snapshot(
        &self,
        _request: AgentTerminalSnapshotRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        unused_operation()
    }
}

/// 未涉及的 Agent 操作必须显式失败，避免测试 fake 制造伪成功。
fn unused_operation<T>() -> AppResult<T> {
    Err(AppError::unsupported("unused test Agent operation"))
}
