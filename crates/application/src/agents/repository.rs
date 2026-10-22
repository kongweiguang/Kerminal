//! Agent 持久化工作区的窄端口；application 传递读取 revision，adapter 负责恢复写入事务。
//!
//! @author kongweiguang

use std::fmt;

use async_trait::async_trait;
use kerminal_domain::{
    agents::{AgentProviderSession, AgentSession},
    identity::{AgentSessionId, SessionId},
};
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

/// 应用用例可读取或提交的固定文件集合，拒绝把任意相对路径交给 adapter。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentWorkspaceDocumentKind {
    Session,
    Provider,
    AgentInstructions,
    ClaudeInstructions,
    CodexConfig,
    ClaudeMcpConfig,
    PiMcpConfig,
    McpEndpointContext,
    TargetBindingContext,
    TerminalSnapshotContext,
    WorkspaceSnapshotContext,
}

/// Provider 文件的可读状态；坏文件与未来 schema 均保留为独立状态，不能当作缺失播种默认值。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentProviderSessionStatus {
    Available(AgentProviderSession),
    Missing,
    Malformed,
    UnsupportedSchemaVersion(u32),
    ProviderMismatch,
}

/// Provider 正文、所在路径和原始 CAS revision 来自同一次受锁读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentProviderSessionSnapshot {
    pub status: AgentProviderSessionStatus,
    pub revision: AgentWorkspaceRevision,
    pub path: String,
}

/// Agent 文档读取时的磁盘版本；Missing 也是明确的 CAS 基线。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentWorkspaceRevision {
    Missing,
    Sha256(String),
}

/// Pending resume 的两阶段持久确认；Unknown 是业务投影，不写成可误读的成功态。
///
/// @author kongweiguang
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentResumeCheckpointPhase {
    Prepared,
    Committed,
}

/// 只记录核对所需身份和 revision，不复制 session/provider 正文或启动数据。
///
/// @author kongweiguang
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumeCheckpoint {
    pub attempt_id: uuid::Uuid,
    pub terminal_session_id: SessionId,
    pub base_session_revision: AgentWorkspaceRevision,
    pub base_provider_revision: AgentWorkspaceRevision,
    pub intended_session_revision: AgentWorkspaceRevision,
    pub intended_provider_revision: AgentWorkspaceRevision,
    pub phase: AgentResumeCheckpointPhase,
}

/// checkpoint 原始字节的 CAS revision 与其 typed 投影必须一起传递。
///
/// @author kongweiguang
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResumeCheckpointSnapshot {
    pub checkpoint: AgentResumeCheckpoint,
    pub revision: AgentWorkspaceRevision,
}

/// 预备 checkpoint 的一次性 typed 请求；不提供任意文件路径或命令字段。
///
/// @author kongweiguang
#[derive(Clone)]
pub struct PrepareAgentResumeCheckpointRequest {
    pub agent_session_id: AgentSessionId,
    pub attempt_id: uuid::Uuid,
    pub terminal_session_id: SessionId,
    pub base_session_revision: AgentWorkspaceRevision,
    pub base_provider_revision: AgentWorkspaceRevision,
    pub intended_session: AgentSession,
    pub intended_provider: AgentProviderSession,
}

/// 将原 attempt 的 typed 意图并入既有 workspace journal，不从磁盘重建启动数据。
///
/// @author kongweiguang
#[derive(Clone)]
pub struct CommitAgentResumeCheckpointRequest {
    pub agent_session_id: AgentSessionId,
    pub prepared: AgentResumeCheckpointSnapshot,
    pub intended_session: AgentSession,
    pub intended_provider: AgentProviderSession,
}

/// Session 聚合和其原始文件 revision 必须一起传递，避免提交时现读覆盖外部编辑。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWorkspaceSessionSnapshot {
    pub session: AgentSession,
    pub revision: AgentWorkspaceRevision,
}

/// 文档正文和原始字节 revision 来自同一次受锁读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWorkspaceDocumentSnapshot {
    pub contents: Option<String>,
    pub revision: AgentWorkspaceRevision,
}

/// 成功提交后返回的文档版本，使同一用例中的后续状态写继续消费 CAS。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentWorkspaceWriteReceipt {
    pub kind: AgentWorkspaceDocumentKind,
    pub revision: AgentWorkspaceRevision,
}

/// 区分应用文本与旧版文件契约；Session 独占堆内存，避免每份文本受管文档承担聚合大小。
///
/// @author kongweiguang
#[derive(Clone, PartialEq, Eq)]
pub enum AgentWorkspaceDocumentContent {
    Text(String),
    Session(Box<AgentSession>),
    Provider(AgentProviderSession),
}

impl fmt::Debug for AgentWorkspaceDocumentContent {
    /// 只显示内容种类，不展开 Agent 启动快照、Provider 恢复命令或用户文本。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Self::Text(_) => "Text",
            Self::Session(_) => "Session",
            Self::Provider(_) => "Provider",
        };
        formatter.write_str(kind)
    }
}

/// 一份已由 application 生成的受管文档；正文值不参与 Debug，避免日志泄露敏感值。
#[derive(Clone, PartialEq, Eq)]
pub struct AgentWorkspaceDocument {
    pub kind: AgentWorkspaceDocumentKind,
    pub content: AgentWorkspaceDocumentContent,
    pub expected_revision: AgentWorkspaceRevision,
}

impl fmt::Debug for AgentWorkspaceDocument {
    /// 只记录文件类别和内容种类，不展开 session.toml 或 Agent 配置正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("AgentWorkspaceDocument")
            .field("kind", &self.kind)
            .field("content", &self.content)
            .field("expected_revision", &"<opaque>")
            .finish()
    }
}

/// Agent workspace 的实际文件边界；应用层负责调用顺序和文档策略，adapter 负责安全 I/O。
#[async_trait]
pub trait AgentWorkspaceRepository: Send + Sync {
    /// 返回当前应用配置根，作为持久 AgentSession 的 workspace 身份快照。
    fn workspace_root(&self) -> String;

    /// 创建一个本次独占的新 session 目录；已存在目录必须冲突而不能复用。
    async fn create_session_root(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<String>;

    /// 只回滚本调用成功创建且 ID 完全匹配的 workspace，不能清理历史目录。
    async fn rollback_created_session_root(
        &self,
        agent_session_id: AgentSessionId,
    ) -> AppResult<()>;

    /// 读取并验证一个准确 ID 的聚合；未来 schema 或损坏状态必须显式失败。
    async fn read_session(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceSessionSnapshot>;

    /// 枚举并逐个验证持久会话；损坏记录不能静默过滤成空列表。
    async fn list_sessions(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceSessionSnapshot>>;

    /// 读取一个固定受管文档，缺失返回 None 以供 application 首次生成。
    async fn read_document(
        &self,
        agent_session_id: AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceDocumentSnapshot>;

    /// 读取 provider.toml 并与 session.toml 校验归属；状态分类不修改原始文件。
    async fn read_provider_session(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentProviderSessionSnapshot>;

    /// 仅在读取 revision 仍匹配且当前文件可验证时更新 provider.toml。
    async fn write_provider_session(
        &self,
        agent_session_id: AgentSessionId,
        provider_session: AgentProviderSession,
        expected_revision: AgentWorkspaceRevision,
        cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceWriteReceipt>;

    /// 按读取时 CAS 基线批量提交；adapter 用 journal 恢复，不承诺多文件瞬时原子可见。
    async fn write_documents(
        &self,
        agent_session_id: AgentSessionId,
        documents: Vec<AgentWorkspaceDocument>,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>>;

    /// 在创建 PTY 前核对两份 base revision 并持久化精确 intended hashes；未实现时必须 fail closed。
    ///
    /// @author kongweiguang
    async fn prepare_resume_checkpoint(
        &self,
        request: PrepareAgentResumeCheckpointRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let _ = (request, cancellation);
        Err(AppError::unsupported(
            "Agent workspace repository does not support durable resume checkpoints",
        ))
    }

    /// 以原 typed 意图和 checkpoint CAS 复用 workspace journal 提交 Session、Provider 与 Committed 标记。
    ///
    /// @author kongweiguang
    async fn commit_resume_checkpoint(
        &self,
        request: CommitAgentResumeCheckpointRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let _ = (request, cancellation);
        Err(AppError::unsupported(
            "Agent workspace repository does not support durable resume checkpoints",
        ))
    }

    /// 读取准确 attempt 的 durable checkpoint；不支持的 adapter 不能把未知误报为不存在。
    ///
    /// @author kongweiguang
    async fn read_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
        let _ = (agent_session_id, cancellation);
        Err(AppError::unsupported(
            "Agent workspace repository does not support durable resume checkpoints",
        ))
    }

    /// 仅按 caller 已核实的 attempt 与 checkpoint revision 清除 marker；不承担 terminal cleanup 判断。
    /// adapter 还必须锁内核对 Session/Provider：Prepared 匹配 base、Committed 匹配 intended；第三值必须保留记录并报冲突。
    ///
    /// @author kongweiguang
    async fn clear_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        expected: AgentResumeCheckpointSnapshot,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _ = (agent_session_id, expected, cancellation);
        Err(AppError::unsupported(
            "Agent workspace repository does not support durable resume checkpoints",
        ))
    }
}
