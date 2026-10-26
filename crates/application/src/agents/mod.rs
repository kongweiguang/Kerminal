//! Agent 会话用例端口与跨界 DTO。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_domain::{
    agents::{
        AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSession, AgentSessionScope,
        AgentSessionStatus, AgentTargetBinding, AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, SessionId},
    sessions::SessionInfo,
};
use kerminal_terminal::snapshot::{PlainTextSnapshot, TerminalPalette};
use std::fmt;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

mod history;
mod launch;
mod repository;
mod session_service;
mod workspace_documents;
pub use launch::{
    AgentLaunchResolver, ResolvedAgentLauncher, builtin_resume_argv, initial_provider_session,
    resolve_agent_launcher,
};
pub use repository::{
    AgentProviderSessionSnapshot, AgentProviderSessionStatus, AgentResumeCheckpoint,
    AgentResumeCheckpointPhase, AgentResumeCheckpointSnapshot, AgentWorkspaceDocument,
    AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind, AgentWorkspaceDocumentSnapshot,
    AgentWorkspaceRepository, AgentWorkspaceRevision, AgentWorkspaceSessionSnapshot,
    AgentWorkspaceWriteReceipt, CommitAgentResumeCheckpointRequest,
    PrepareAgentResumeCheckpointRequest,
};
pub use session_service::AgentSessionService;

/// 右栏历史的显示范围；归档是过滤视图，不执行物理删除。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum AgentHistoryMode {
    #[default]
    CurrentTarget,
    All,
    Archived,
}

/// 当前候选变化只影响历史筛选和新建请求，不会修改任何既有 Agent binding。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentHistoryRequest {
    pub scope: AgentSessionScope,
    pub target_candidate: Option<AgentTargetBinding>,
    pub mode: AgentHistoryMode,
}

/// 同时返回全量 session 供切换器使用，以及经过 application 规则筛选的历史身份。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentHistoryProjection {
    pub sessions: Vec<AgentSessionView>,
    pub visible_session_ids: Vec<AgentSessionId>,
    pub current_target_count: usize,
    pub all_count: usize,
    pub archived_count: usize,
}

/// 从历史启动定义新会话；source 仅提供配置快照，目标、ID 和 workspace 均重新创建。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateAgentSessionFromHistoryRequest {
    pub source_agent_session_id: AgentSessionId,
    pub title: Option<String>,
    pub scope: AgentSessionScope,
    pub target: AgentTargetBinding,
    /// 传给新 PTY 的首帧色盘，在 CLI 首次读取 OSC 颜色前生效。
    pub initial_palette: Option<TerminalPalette>,
}

/// 以调用方读取到的标题作为前置值，避免并发改名覆盖其他窗口的新标题。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenameAgentSessionTitleRequest {
    pub agent_session_id: AgentSessionId,
    pub expected_title: String,
    pub title: String,
}

/// 新建 Agent 请求可携带显式 launcher 意图；启动前从最新已验证配置解析并固化快照。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreateAgentSessionRequest {
    pub title: Option<String>,
    pub scope: AgentSessionScope,
    pub target: AgentTargetBinding,
    /// 显式选择只影响本次新建；application 会按最新配置重新验证，缺失时不回退。
    pub launcher_key: Option<AgentLauncherKey>,
    /// 传给新 PTY 的首帧色盘，在 CLI 首次读取 OSC 颜色前生效。
    pub initial_palette: Option<TerminalPalette>,
}

/// 当前配置中可供新建界面选择的启动项，不包含命令内容或凭据。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLauncherCatalog {
    pub selected_key: AgentLauncherKey,
    pub options: Vec<AgentLauncherOption>,
}

/// 安全的 Launcher 选择项仅公开稳定身份、provider 和用户可见名称。
///
/// @author kongweiguang
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLauncherOption {
    pub key: AgentLauncherKey,
    pub provider: AgentProvider,
    pub label: String,
}

/// 保留用例实现中的语义名称，同时让所有入口共享一个请求类型。
pub type CreateSelectedAgentSessionRequest = CreateAgentSessionRequest;

/// 保存的恢复定义是否可作为显式继续入口；不代表本机已安装对应 CLI 或启动环境可用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentResumeCapability {
    /// 当前 provider 文档和恢复定义均通过静态策略校验。
    Supported,
    /// 当前有效 provider 文档明确声明不支持恢复。
    Unsupported,
    /// 文档缺失、损坏或定义未通过静态校验，暂不能确认能力。
    MetadataUnavailable,
}

/// Agent 查询视图保留业务身份、运行期 CLI terminal、恢复能力和未确认恢复状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionView {
    pub session: AgentSession,
    pub terminal: Option<SessionInfo>,
    /// CLI 已收到启动确认、但 session/provider 还未完成读回核对时保留其真实 owner。
    pub resume_recovery: Option<AgentResumeRecovery>,
    /// 只投影已持久化且静态合法的恢复定义，不探测当前 CLI、profile 或外部进程。
    pub resume_capability: AgentResumeCapability,
}

/// 一次已启动但尚未从 session 与 provider 文件确认提交结果的恢复操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentResumeRecovery {
    pub terminal_session_id: SessionId,
    pub status: AgentResumeRecoveryStatus,
}

/// Pending 表示已知需要重试元数据提交；Unknown 表示存储结果或读回状态不一致。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentResumeRecoveryStatus {
    Pending,
    Unknown,
}

/// MCP 与其它查询边界可安全消费的 Agent 记录；不含 launch env，并携带未确认的恢复 owner。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionRecordProjection {
    pub session: AgentSessionRecordSessionProjection,
    pub provider: Option<AgentProviderSession>,
    /// ACK 后尚未能确认 session/provider 提交结果时，保留原 CLI owner 与恢复状态。
    pub resume_recovery: Option<AgentResumeRecovery>,
    pub target_binding: Option<AgentTargetBindingContextProjection>,
    pub mcp_endpoint: Option<AgentMcpEndpointContextProjection>,
    pub paths: AgentSessionPathsProjection,
    pub diagnostics: Vec<AgentSessionRecordDiagnostic>,
}

/// 会话主记录的安全字段；运行时环境变量不属于可查询 MCP 记录。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionRecordSessionProjection {
    pub schema_version: u32,
    pub agent_session_id: String,
    pub agent_id: AgentProvider,
    pub launcher_key: Option<String>,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
    pub status: AgentSessionStatus,
    pub workspace_root: String,
    pub session_root: String,
    pub launch: AgentSessionLaunchProjection,
    pub scope: Option<AgentSessionScope>,
    pub target: Option<AgentTargetBinding>,
}

/// CLI 恢复快照只暴露旧记录字段，不把进程环境中的秘密带入 MCP。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionLaunchProjection {
    pub command_label: String,
    pub shell: String,
    pub args: Vec<String>,
    pub cwd: String,
}

/// 已验证的 `context/target-binding.json` 查询投影。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTargetBindingContextProjection {
    pub schema_version: u32,
    pub agent_session_id: String,
    pub scope: Option<AgentSessionScope>,
    pub binding: AgentTargetBindingContextBindingProjection,
    pub agent_terminal: Option<AgentTerminalContextProjection>,
    pub generated_at: String,
}

/// 持久 target-binding context 中与旧记录兼容的绑定字段。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTargetBindingContextBindingProjection {
    pub binding_id: Option<String>,
    pub generation: u64,
    pub status: String,
    pub stale: bool,
    pub pane_id: Option<String>,
    pub tab_id: Option<String>,
    pub target_terminal_session_id: Option<String>,
    pub target_ref: Option<String>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
}

/// Agent 自身的真实 CLI terminal 摘要，与用户首选 target 分开保存。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTerminalContextProjection {
    pub session_id: Option<String>,
    pub status: String,
}

/// 已验证的 `context/mcp-endpoint.json` 查询投影。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMcpEndpointContextProjection {
    pub schema_version: u32,
    pub agent_session_id: String,
    pub endpoint: Option<String>,
    pub transport: String,
    pub tools_only: bool,
    pub generated_at: String,
}

/// 与 Agent workspace 关联的关键受管文件路径。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionPathsProjection {
    pub workspace_root: String,
    pub session_root: String,
    pub session_toml: String,
    pub provider_toml: String,
    pub context: AgentSessionContextPathsProjection,
}

/// Agent context 目录中的固定文件路径集合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionContextPathsProjection {
    pub target_binding_json: String,
    pub mcp_endpoint_json: String,
    pub terminal_snapshot_json: String,
    pub workspace_snapshot_json: String,
}

/// 持久文档异常以稳定诊断返回，不把解析器原文或用户正文附加到错误中。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentSessionRecordDiagnostic {
    pub path: Option<String>,
    pub code: String,
    pub message: String,
    pub line: Option<usize>,
    pub column: Option<usize>,
}

/// 目标解析结果同时保留失效状态和旧 SessionId，调用方不得用其它 Tab 替代。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTargetResolution {
    pub agent_session_id: AgentSessionId,
    pub binding: AgentTargetBinding,
    pub resolved_session_id: Option<SessionId>,
    pub status: AgentTargetLiveStatus,
    /// 目标状态本次被用例核验的 Unix 毫秒时间，不从缺失的历史字段造值。
    pub observed_at_ms: u64,
}

/// Agent 当前上下文快照；失效或未绑定时 snapshot 为空，不能隐式读取活动终端。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTargetContext {
    pub session: AgentSessionView,
    pub record: AgentSessionRecordProjection,
    pub target: AgentTargetResolution,
    pub snapshot: Option<PlainTextSnapshot>,
    /// 终端引擎在脱敏前实际捕获的 UTF-8 字节数，兼容外部快照契约。
    pub captured_bytes: usize,
    /// 终端文本是否按常见凭据形式脱敏，供调用方和 context 文件准确标注。
    pub redacted: bool,
}

/// 已脱敏的终端输出与目标身份；只用于更新 Agent 工作区中的 snapshot context。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTerminalSnapshotRequest {
    pub agent_session_id: AgentSessionId,
    pub target_session_id: SessionId,
    pub output: String,
    pub max_bytes: usize,
    pub truncated: bool,
    pub redacted: bool,
}

/// Agent 侧已脱敏且按 UTF-8 字节预算裁剪的临时文本预览，不写入 workspace。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct AgentTextPreview {
    pub text: String,
    pub redacted: bool,
    pub truncated: bool,
}

impl fmt::Debug for AgentTextPreview {
    /// 预览正文仍可能包含用户上下文，调试输出仅提供长度和安全状态。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentTextPreview")
            .field("text_bytes", &self.text.len())
            .field("text", &"<redacted>")
            .field("redacted", &self.redacted)
            .field("truncated", &self.truncated)
            .finish()
    }
}

/// 用户确认后要发送到指定 Agent CLI 的精确文本和 terminal 代次，不含隐式目标查找。
///
/// @author kongweiguang
#[derive(Clone, Eq, PartialEq)]
pub struct SendAgentCommandRequest {
    pub agent_session_id: AgentSessionId,
    pub terminal_session_id: SessionId,
    pub expected_session_generation: u64,
    pub text: String,
}

impl fmt::Debug for SendAgentCommandRequest {
    /// 多行提示或命令输出可能包含用户数据，禁止在日志中回显正文。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SendAgentCommandRequest")
            .field("agent_session_id", &self.agent_session_id)
            .field("terminal_session_id", &self.terminal_session_id)
            .field(
                "expected_session_generation",
                &self.expected_session_generation,
            )
            .field("text_bytes", &self.text.len())
            .field("text", &"<redacted>")
            .finish()
    }
}

/// 终端完整接收的字节数；只代表 Paste+Enter 输入被 writer ACK，不代表 Agent 已执行或理解。
///
/// @author kongweiguang
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentTextSubmissionReceipt {
    pub acknowledged_bytes: usize,
}

/// Agent 业务用例端口；所有操作显式接收取消令牌，避免默认包装误报中途可取消。
///
/// @author kongweiguang
#[async_trait]
pub trait AgentService: Send + Sync {
    /// 同步 live MCP endpoint 到当前运行的受管 workspace；归档或自定义文件不属于该更新范围。
    async fn configure_mcp_endpoint(
        &self,
        endpoint: Option<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()>;

    /// 查询当前已验证设置中的 Launcher 选项；catalog 不探测 CLI 是否已安装。
    ///
    /// @author kongweiguang
    async fn launcher_catalog(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog>;

    /// 只更新全局 selected key，并以读取时的 settings revision 做 CAS 保存。
    ///
    /// @author kongweiguang
    async fn select_launcher(
        &self,
        key: AgentLauncherKey,
        cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog>;

    /// 创建受管会话和 CLI terminal；启动成功后若后续失败，实现在返回前必须回收半成品。
    async fn create(
        &self,
        request: CreateAgentSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 列出历史会话及当前运行态关联，取消后不得返回陈旧列表。
    async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>>;

    /// 按右栏归属和当前显式候选计算历史视图及数量；调用只读且不触发目标重绑。
    async fn history(
        &self,
        request: AgentHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection>;

    /// 从某条历史会话的启动定义创建全新 AgentSession，不复用旧 CLI 或 provider 会话 ID。
    async fn create_from_history(
        &self,
        request: CreateAgentSessionFromHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 使用标题 CAS 持久化改名；仓储冲突交由调用方保留草稿并显式重试。
    async fn rename_title(
        &self,
        request: RenameAgentSessionTitleRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 读取单个 Agent 会话；未找到时返回 not_found，不退回其它历史条目。
    async fn get(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 读取脱敏记录投影、provider 状态和待确认恢复 owner，供 current_session 单次消费。
    async fn record_projection(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionRecordProjection>;

    /// 显式继续会话；存储结果未确认时保留已启动 owner，并通过 view 暴露可恢复状态。
    async fn resume(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 继续会话时先把 Root 当前色盘装入 PTY engine，避免恢复后的 CLI 启动时查询到旧默认色。
    ///
    /// @author kongweiguang
    async fn resume_with_palette(
        &self,
        agent_session_id: AgentSessionId,
        initial_palette: Option<TerminalPalette>,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        if initial_palette.is_some() {
            return Err(AppError::unsupported(
                "Agent resume with an initial terminal palette is unavailable",
            ));
        }
        self.resume(agent_session_id, cancellation).await
    }

    /// 只核对已有恢复事务；缺省实现让外部 adapter 保持兼容且不会误调用继续启动。
    ///
    /// @author kongweiguang
    async fn reconcile_resume(
        &self,
        _agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        Err(AppError::unsupported(
            "Agent resume reconciliation is unavailable",
        ))
    }

    /// 归档会话并保留 session 文件和自定义上下文；关闭提交开始后须返回实际终态。
    async fn archive(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 仅在用户显式请求时绑定首选目标；普通 Tab 切换不得调用此用例。
    async fn bind_preferred_target(
        &self,
        agent_session_id: AgentSessionId,
        binding: AgentTargetBinding,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView>;

    /// 返回首选 target 的 live/stale 状态，不会搜索或替换其它会话。
    async fn resolve_preferred_target(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution>;

    /// 读取 Agent 元数据与指定目标的有界快照；失效目标不提供替代输出。
    async fn target_context(
        &self,
        agent_session_id: AgentSessionId,
        max_snapshot_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext>;

    /// 创建只存在于当前确认流程的脱敏预览；只读校验 Agent 记录，不读取首选终端或启动 CLI。
    ///
    /// @author kongweiguang
    async fn preview_text(
        &self,
        agent_session_id: AgentSessionId,
        text: String,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview>;

    /// 将用户确认的多行预览原文原子提交到指定 Agent CLI；未知 ACK 由 caller 查询，不自动重放。
    ///
    /// @author kongweiguang
    async fn send_command_block_text(
        &self,
        request: SendAgentCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt>;

    /// 将调用方已脱敏的实际 session 快照写入对应 Agent context 文件。
    async fn record_terminal_snapshot(
        &self,
        request: AgentTerminalSnapshotRequest,
        cancellation: CancellationToken,
    ) -> AppResult<()>;
}
