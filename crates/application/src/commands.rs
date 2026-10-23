//! 命令历史、片段、工作流、建议和终端广播用例。
//!
//! 用户目录、SQLite 与终端 session 都经端口注入；此模块不接触文件路径或基础设施实现。
//!
//! @author kongweiguang

use std::{sync::Arc, time::SystemTime};

use async_trait::async_trait;
use kerminal_domain::{
    commands::{
        BroadcastAnalysis, BroadcastTarget, CommandHistoryEntry, CommandHistoryListRequest,
        CommandHistoryRecordRequest, CommandHistoryRecordResult, CommandHistorySource,
        CommandHistoryTarget, CommandSuggestionCandidate, CommandSuggestionFeedbackRequest,
        CommandSuggestionRequest, CommandWorkflow, NormalizedCommandHistory, SnippetCatalogItem,
        SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetDefinition, SuggestionProviderKind,
        WorkflowListRequest, WorkflowStepInput, WorkflowUpdateRequest, validate_history_context,
        validate_workflow,
    },
    identity::{HostId, PaneId, SessionId},
    preferences::ConfigRevision,
    sessions::{SessionInfo, SessionLifecycle, SessionOwner, SessionTarget},
};

use crate::{
    AppError, AppResult, PublicErrorCode,
    configuration_changes::{ConfigurationChangeService, ConfigurationChangeSubscription},
    preferences::PreferencesService,
    sessions::{SessionFilter, SessionService, UiSessionBinding, UiSnippetCommandLineRequest},
};
use tokio_util::sync::CancellationToken;

mod coordinator;
mod execution;
mod snippet_catalog;
mod snippet_execution;
mod snippet_preferences;
mod snippet_transfer;
mod snippet_transfer_file;
mod snippets;
mod submission;
mod suggestions;
mod terminal_suggestions;
mod versioned;
mod workflow_execution;

/// 用户片段单次交换的统一条目上限，供应用用例与基础设施 journal 共用。
pub const MAX_SNIPPET_TRANSFER_ITEMS: usize = 1_000;
/// 用户片段导入或导出的统一 UTF-8 JSON 预算，防止单次批量操作无界占用内存和磁盘。
pub const MAX_SNIPPET_TRANSFER_BYTES: usize = 32 * 1024 * 1024;

pub use execution::CommandExecutionReceipt;
pub use kerminal_domain::commands::{
    SnippetApplicabilityAssessment, SnippetApplicabilityReason, SnippetApplicabilityStatus,
};
pub use snippet_catalog::{
    SnippetApplicabilityProjection, SnippetCatalogCursor, SnippetCatalogPage,
    SnippetCatalogPageOutcome, SnippetCatalogPageRequest, SnippetCatalogRevision,
    SnippetCatalogSection, SnippetExecutionCatalogSnapshot,
};
pub use snippet_execution::{
    SnippetExecutionService, SnippetFillPlan, SnippetRunConfirmation, SnippetRunConfirmationInfo,
    SnippetRunHistory, SnippetRunPlan, SnippetRunPreparation, SnippetRunResult,
    SnippetRunTargetSummary, SnippetRunUsage,
};
pub use snippet_preferences::{
    SnippetPreferenceAvailability, SnippetPreferenceRepository, SnippetPreferenceUnavailableReason,
};
pub use snippet_transfer::{
    SnippetImportBatchRequest, SnippetImportBatchState, SnippetImportCandidate, SnippetImportIssue,
    SnippetImportPreview, SnippetImportReceipt, export_user_snippets_json,
};
pub use snippet_transfer_file::{
    SnippetTransferFileConfirmation, SnippetTransferFilePort, SnippetTransferFileService,
    SnippetTransferFileWriteReceipt,
};
pub use snippets::{
    SnippetRenderDialect, SnippetRenderResult, direct_snippet_catalog_command,
    direct_snippet_command, render_snippet_definition,
};
pub use submission::{
    TerminalInputSubmissionReceipt, UiTerminalInputSubmissionResult, UiTerminalSubmissionRequest,
};
pub use suggestions::CompositeCommandSuggestionProvider;
pub use suggestions::HistoryCommandSuggestionProvider;
pub use terminal_suggestions::{TerminalSuggestionBatch, TerminalSuggestionQuery};
pub use versioned::VersionedCommandDefinition;
pub use workflow_execution::{
    WorkflowExecution, WorkflowExecutionControl, WorkflowExecutionRequest, WorkflowProgressEvent,
    WorkflowProgressPublisher, WorkflowProgressSubscription, WorkflowRunProgress,
    WorkflowStepExecution, WorkflowStepProgress,
};

/// 命令建议反馈和审计的联合清理结果，使用数据库实际删除数量供诊断界面回报。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CommandSuggestionRetentionResult {
    pub feedback_deleted: u64,
    pub audit_deleted: u64,
}

/// 敏感输入在 application 边界被跳过，不允许 UI 将脱敏判断交给 SQLite 实现。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandSuggestionFeedbackOutcome {
    Recorded,
    SkippedSensitive,
}

/// 历史追加输入不含数据库分配的 id/timestamp，避免用例伪造存储身份和顺序。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandHistoryDraft {
    pub command: String,
    pub source: CommandHistorySource,
    pub target: CommandHistoryTarget,
    pub session_id: Option<SessionId>,
    pub pane_id: Option<PaneId>,
    pub tab_id: Option<kerminal_domain::identity::TabId>,
    pub profile_id: Option<String>,
    pub remote_host_id: Option<HostId>,
    pub cwd: Option<String>,
    pub shell: Option<String>,
}

/// 清理历史可按显式会话、分屏或主机缩小范围；空筛选代表用户明确请求清空全部历史。
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CommandHistoryClearRequest {
    pub target: Option<CommandHistoryTarget>,
    pub session_id: Option<SessionId>,
    pub pane_id: Option<PaneId>,
    pub remote_host_id: Option<HostId>,
}

/// 历史仓储启动状态供 UI 和 MCP 明确呈现；不可用时不能伪装成空历史。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandHistoryAvailability {
    Available,
    Unavailable {
        reason: CommandHistoryUnavailableReason,
    },
}

/// 仓储启动失败只暴露稳定类别，不携带本地路径、SQL 或秘密字段。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandHistoryUnavailableReason {
    /// 旧数据库必须通过独立迁移工具升级，普通运行入口不会隐式改写它。
    MigrationRequired(u32),
    UnsupportedSchemaVersion(u32),
    CorruptDatabase,
    StorageUnavailable,
    BackupFailed,
    StorageLockUnavailable,
}

/// SQLite 适配器实现的历史 repository port；调用方不依赖表名或 SQL 版本。
#[async_trait]
pub trait CommandHistoryRepository: Send + Sync + 'static {
    /// 返回仓储初始化时的稳定状态，调用方据此区别空结果和不可用历史。
    fn availability(&self) -> CommandHistoryAvailability;

    /// 按已经约束过的过滤条件查询，结果按时间倒序并受 limit 限制。
    async fn list(&self, request: CommandHistoryListRequest)
    -> AppResult<Vec<CommandHistoryEntry>>;

    /// 持久化安全 draft 并由存储层分配稳定 ID 与创建时间。
    async fn append(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry>;

    /// 删除单条记录；不存在时返回 false，重复删除按幂等结果处理。
    async fn delete(&self, entry_id: String) -> AppResult<bool>;

    /// 清理指定范围记录并返回实际删除数量。
    async fn clear(&self, request: CommandHistoryClearRequest) -> AppResult<u64>;
}

/// 用户片段文件与内置只读 catalog 的统一访问端口。
#[async_trait]
pub trait SnippetRepository: Send + Sync + 'static {
    /// 将内置和用户片段统一投影；`limit: None` 读取完整目录，显式 limit 才限制返回量。
    async fn list(&self, request: SnippetCatalogListRequest) -> AppResult<Vec<SnippetCatalogItem>>;

    /// 按来源和稳定 ID 精确读取单条目录项；默认完整枚举仅供轻量 fake 兼容，文件适配器覆盖为直接读取。
    async fn get_catalog_item(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<Option<SnippetCatalogItem>> {
        self.list(SnippetCatalogListRequest {
            origin: Some(origin),
            limit: None,
            ..SnippetCatalogListRequest::default()
        })
        .await
        .map(|items| items.into_iter().find(|item| item.id == snippet_id))
    }

    /// 按真实目录来源校验身份，阻止 SQLite 收藏表保存孤儿或伪造的 snippet key。
    async fn catalog_identity_exists(
        &self,
        origin: kerminal_domain::commands::SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<bool>;

    /// 在专用 journal 与同一存储锁内原子导入整批新用户文件，顺序按请求保留并追加到目录末尾。
    async fn commit_import_batch(
        &self,
        request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState>;

    /// 读取导入批次的可证明状态；未知结果只查询，不由调用方自动重放写入。
    async fn import_batch_status(&self, batch_id: String) -> AppResult<SnippetImportBatchState>;

    /// 只读取可编辑的用户定义，内置项不会被转换成可写副本。
    async fn get_user(&self, snippet_id: String) -> AppResult<Option<SnippetDefinition>>;

    /// 原子保存经过领域验证的用户定义。
    async fn save_user(&self, definition: SnippetDefinition) -> AppResult<SnippetDefinition>;

    /// 删除用户片段；内置条目和未知 ID 均返回 false。
    async fn delete_user(&self, snippet_id: String) -> AppResult<bool>;

    /// 读取带原始字节 revision 的片段，缺少 CAS 能力的仓储不得伪装成安全更新。
    async fn get_user_versioned(
        &self,
        snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>>;

    /// 按读取时 revision 执行创建或替换，避免覆盖文件系统 watcher 之外的编辑器修改。
    async fn save_user_versioned(
        &self,
        definition: SnippetDefinition,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>>;

    /// 只有版本化 UI 可以删除用户草稿，CAS 不匹配时保留外部编辑结果。
    async fn delete_user_versioned(
        &self,
        snippet_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool>;
}

/// 工作流配置文件 repository；持久化实现负责原子替换与损坏文件的恢复状态。
#[async_trait]
pub trait WorkflowRepository: Send + Sync + 'static {
    /// 按关键词、scope、tag 查询并稳定排序工作流定义。
    async fn list(&self, request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>>;

    /// 按稳定工作流 ID 读取当前定义。
    async fn get(&self, workflow_id: String) -> AppResult<Option<CommandWorkflow>>;

    /// 原子新增或更新经过领域校验的工作流。
    async fn save(&self, workflow: CommandWorkflow) -> AppResult<CommandWorkflow>;

    /// 删除工作流定义；运行实例由应用层独立管理。
    async fn delete(&self, workflow_id: String) -> AppResult<bool>;

    /// 读取带原始字节 revision 的工作流，避免调用方用非原子读取构造 CAS。
    async fn get_versioned(
        &self,
        workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>>;

    /// 按先前读取的 revision 创建或替换工作流定义。
    async fn save_versioned(
        &self,
        workflow: CommandWorkflow,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>>;

    /// 删除时必须携带读取 revision，避免陈旧的删除操作移除外部刚编辑的定义。
    async fn delete_versioned(
        &self,
        workflow_id: String,
        expected_revision: ConfigRevision,
    ) -> AppResult<bool>;
}

/// Suggestion SQLite port 与 command.sqlite 使用同一事务边界，不保存展开的片段秘密。
#[async_trait]
pub trait CommandSuggestionFeedbackRepository: Send + Sync + 'static {
    /// 原子保存一次安全反馈及其审计记录；SQLite adapter 复核长度和敏感文本。
    async fn record_feedback(&self, request: CommandSuggestionFeedbackRequest) -> AppResult<()>;

    /// 按当前配置的天数清理 feedback 与 audit，时间计算和事务由存储适配器完成。
    async fn prune_expired(
        &self,
        feedback_retention_days: u32,
        audit_retention_days: u32,
    ) -> AppResult<CommandSuggestionRetentionResult>;
}

/// shell 建议生成器由终端快照、历史或平台语境实现，应用层不猜测当前 active Tab。
#[async_trait]
pub trait CommandSuggestionProvider: Send + Sync + 'static {
    /// 声明此 provider 的限界范围，让应用组合器只调用配置选中的来源。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind>;

    /// 根据明确 session 与输入前缀产生候选；token 让远端探测在取消时结束内部 I/O。
    async fn suggest(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>>;
}

/// 广播绑定显式指定 pane/session，切 Tab 不会把一条命令重定向到另一个终端。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastSessionBinding {
    pub pane_id: PaneId,
    pub session_id: SessionId,
    pub target: CommandHistoryTarget,
}

/// 每个广播目标单独报告写入结果，部分失败不会伪装为全成功或重发已送达目标。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastDeliveryResult {
    pub pane_id: PaneId,
    pub session_id: SessionId,
    pub attempted: bool,
    pub sent: bool,
    pub cancelled: bool,
    pub error_code: Option<PublicErrorCode>,
    pub history_recorded: Option<bool>,
    pub history_skip_reason: Option<String>,
    pub history_error_code: Option<PublicErrorCode>,
}

/// 广播响应携带风险说明与逐目标结果，UI 可以清楚呈现部分送达。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BroadcastExecutionResult {
    pub analysis: BroadcastAnalysis,
    pub deliveries: Vec<BroadcastDeliveryResult>,
}

/// 同一逐目标执行循环服务普通广播和片段发送；片段用完整 UI binding 做 generation CAS。
enum BroadcastBindings {
    Stable(Vec<BroadcastSessionBinding>),
    Ui(Vec<UiSessionBinding>),
}

/// 保持广播入口的来源、敏感性和目标写法成组传递，避免调用参数与历史策略错位。
#[derive(Clone, Copy)]
struct BroadcastMode {
    history_source: CommandHistorySource,
    sensitive: bool,
}

/// 写入前解析出的稳定接收方；带 UI binding 的片段路径由 session writer 原子核对 owner/generation。
struct ResolvedBroadcastTarget {
    pane_id: PaneId,
    session_id: SessionId,
    ui_binding: Option<UiSessionBinding>,
}

/// 逐目标交付所需的不可变快照；共享循环统一处理取消、写入回执和历史策略。
struct BroadcastDeliveryPlan<'a> {
    analysis: BroadcastAnalysis,
    targets: Vec<ResolvedBroadcastTarget>,
    sessions: &'a [SessionInfo],
    cancellations: &'a [CancellationToken],
    mode: BroadcastMode,
}

/// 命令用例聚合稳定 repository ports 与唯一 SessionService。
#[derive(Clone)]
pub struct CommandsService {
    history: Arc<dyn CommandHistoryRepository>,
    snippets: Arc<dyn SnippetRepository>,
    workflows: Arc<dyn WorkflowRepository>,
    suggestions: Arc<dyn CommandSuggestionProvider>,
    sessions: Arc<dyn SessionService>,
    preferences: Option<PreferencesService>,
    suggestion_feedback: Option<Arc<dyn CommandSuggestionFeedbackRepository>>,
    snippet_preferences: Option<Arc<dyn SnippetPreferenceRepository>>,
    configuration_changes: Option<ConfigurationChangeService>,
    command_coordinator: coordinator::SessionCommandCoordinator,
}

impl CommandsService {
    /// 返回历史子系统启动状态，保留只读恢复或 SQLite 初始化故障的可见原因。
    pub fn history_availability(&self) -> CommandHistoryAvailability {
        self.history.availability()
    }

    /// 返回共享 watcher 的独立游标；未由组合根装配时保留静态目录读取行为。
    pub fn subscribe_configuration_changes(&self) -> Option<ConfigurationChangeSubscription> {
        self.configuration_changes
            .as_ref()
            .map(ConfigurationChangeService::subscribe)
    }

    /// 读取指定会话的当前目标元数据；UI 用于构造精确建议上下文，不推断活动 Tab。
    pub async fn session_info(&self, session_id: SessionId) -> AppResult<SessionInfo> {
        self.sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .find(|session| session.session_id == session_id)
            .ok_or_else(|| AppError::not_found("指定终端会话不存在"))
    }

    /// 用已固定版本的 UUID 为 UI 新建片段和工作流生成无路径语义的稳定 ID。
    pub fn new_definition_id(&self) -> String {
        uuid::Uuid::new_v4().to_string()
    }

    /// 只返回仍运行且由可见工作区拥有的分屏，供用户显式构造广播目标集合。
    pub async fn list_ui_sessions(&self) -> AppResult<Vec<SessionInfo>> {
        Ok(self
            .sessions
            .list(SessionFilter::default())
            .await?
            .into_iter()
            .filter(|session| {
                session.lifecycle == SessionLifecycle::Running
                    && matches!(&session.owner, SessionOwner::Ui { .. })
            })
            .collect())
    }

    /// 显式注入各限界上下文依赖，避免服务定位器掩盖配置、存储和终端写入边界。
    pub fn new(
        history: Arc<dyn CommandHistoryRepository>,
        snippets: Arc<dyn SnippetRepository>,
        workflows: Arc<dyn WorkflowRepository>,
        suggestions: Arc<dyn CommandSuggestionProvider>,
        sessions: Arc<dyn SessionService>,
    ) -> Self {
        Self {
            history,
            snippets,
            workflows,
            suggestions,
            sessions,
            preferences: None,
            suggestion_feedback: None,
            snippet_preferences: None,
            configuration_changes: None,
            command_coordinator: coordinator::SessionCommandCoordinator::default(),
        }
    }

    /// 复用 desktop 持有的共享 watcher，避免命令面板自行创建重复文件监听器。
    pub fn with_configuration_changes(mut self, service: ConfigurationChangeService) -> Self {
        self.configuration_changes = Some(service);
        self
    }

    /// 复用同一 settings 用例，使终端建议每次按文件中的用户策略实时筛选来源。
    pub fn with_preferences(mut self, service: PreferencesService) -> Self {
        self.preferences = Some(service);
        self
    }

    /// 让同一个 command.sqlite repository 同时承担建议反馈和保留期清理。
    pub fn with_suggestion_feedback_repository(
        mut self,
        repository: Arc<dyn CommandSuggestionFeedbackRepository>,
    ) -> Self {
        self.suggestion_feedback = Some(repository);
        self
    }

    /// 查询历史时应用旧默认值与上限，阻止异常 limit 扫描或返回过大数据库结果。
    pub async fn list_history(
        &self,
        mut request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        if request
            .query
            .as_ref()
            .is_some_and(|query| query.chars().count() > 4_000)
        {
            return Err(AppError::invalid("历史关键词超过长度限制"));
        }
        request.limit = Some(request.limit.unwrap_or(100).clamp(1, 500));
        self.history.list(request).await
    }

    /// 先规范化、校验上下文并过滤敏感命令，再调用存储 port，确保凭据不会落入 SQLite。
    pub async fn record_history(
        &self,
        request: CommandHistoryRecordRequest,
    ) -> AppResult<CommandHistoryRecordResult> {
        if request.record == Some(false) {
            return Ok(skipped_history("当前会话已关闭历史记录"));
        }
        let normalized = NormalizedCommandHistory::new(&request.command)
            .map_err(|_| AppError::invalid("历史命令为空或超过长度限制"))?;
        validate_history_context(
            request.cwd.as_deref(),
            request.shell.as_deref(),
            &[
                request.session_id.as_ref().map(|value| value.as_str()),
                request.pane_id.as_ref().map(|value| value.as_str()),
                request.tab_id.as_ref().map(|value| value.as_str()),
                request.profile_id.as_deref(),
                request.remote_host_id.as_ref().map(|value| value.as_str()),
            ],
        )
        .map_err(|_| AppError::invalid("历史上下文超过长度限制"))?;
        if let Some(reason) = normalized.sensitive_skip_reason() {
            return Ok(skipped_history(reason));
        }
        let entry = self
            .history
            .append(CommandHistoryDraft {
                command: normalized.command,
                source: request.source,
                target: request.target,
                session_id: request.session_id,
                pane_id: request.pane_id,
                tab_id: request.tab_id,
                profile_id: request.profile_id,
                remote_host_id: request.remote_host_id,
                cwd: request.cwd,
                shell: request.shell,
            })
            .await?;
        Ok(CommandHistoryRecordResult {
            recorded: true,
            entry: Some(entry),
            skip_reason: None,
        })
    }

    /// 删除单条历史时只传稳定条目 ID，不允许用拼接条件执行任意 SQL。
    pub async fn delete_history(&self, entry_id: String) -> AppResult<bool> {
        if entry_id.is_empty()
            || entry_id.chars().count() > 160
            || entry_id.chars().any(char::is_control)
        {
            return Err(AppError::invalid("历史记录标识无效"));
        }
        self.history.delete(entry_id).await
    }

    /// 按显式上下文范围清理历史并返回删除数量，UI 可据此确认清理结果。
    pub async fn clear_history(&self, request: CommandHistoryClearRequest) -> AppResult<u64> {
        self.history.clear(request).await
    }

    /// 查询合并的用户与内置片段目录；底层文件损坏由 repository 明确返回错误。
    pub async fn list_snippets(
        &self,
        request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        if request
            .query
            .as_ref()
            .is_some_and(|query| query.chars().count() > 256)
        {
            return Err(AppError::invalid("片段关键词超过长度限制"));
        }
        let mut request = request;
        request.limit = Some(
            request
                .limit
                .unwrap_or(100)
                .clamp(1, MAX_SNIPPET_TRANSFER_ITEMS),
        );
        let items = self.snippets.list(request).await?;
        snippet_catalog::decorate_catalog_preferences(self, items).await
    }

    /// 保存用户片段前执行模板、变量、安全策略和上下文绑定的领域校验。
    pub async fn save_snippet(
        &self,
        definition: SnippetDefinition,
    ) -> AppResult<SnippetDefinition> {
        definition
            .validate()
            .map_err(|_| AppError::invalid("命令片段定义未通过校验"))?;
        self.snippets
            .save_user_versioned(definition, None)
            .await
            .map(|saved| saved.definition)
    }

    /// 读取用户片段详情；内置片段以目录投影展示，不能被当作用户文件修改。
    pub async fn get_user_snippet(
        &self,
        snippet_id: String,
    ) -> AppResult<Option<SnippetDefinition>> {
        validate_catalog_id(&snippet_id)?;
        self.snippets
            .get_user_versioned(snippet_id)
            .await
            .map(|result| result.map(|saved| saved.definition))
    }

    /// 先取 revision 再由 adapter 原子复核后删除，避免陈旧 UI 删除外部刚更新的片段。
    pub async fn delete_snippet(&self, snippet_id: String) -> AppResult<bool> {
        validate_catalog_id(&snippet_id)?;
        let Some(current) = self.snippets.get_user_versioned(snippet_id.clone()).await? else {
            return Ok(false);
        };
        let revision = current
            .revision
            .ok_or_else(|| AppError::conflict("命令片段缺少 revision，无法安全删除"))?;
        self.snippets
            .delete_user_versioned(snippet_id, revision)
            .await
    }

    /// 查询工作流定义；返回值不含运行实例状态，避免配置对象与聚合生命周期混合。
    pub async fn list_workflows(
        &self,
        request: WorkflowListRequest,
    ) -> AppResult<Vec<CommandWorkflow>> {
        if request
            .query
            .as_ref()
            .is_some_and(|query| query.chars().count() > 256)
        {
            return Err(AppError::invalid("工作流关键词超过长度限制"));
        }
        self.workflows.list(request).await
    }

    /// 载入单个工作流配置用于编辑或执行，未知 ID 不会回退到首个条目。
    pub async fn get_workflow(&self, workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        validate_catalog_id(&workflow_id)?;
        self.workflows
            .get_versioned(workflow_id)
            .await
            .map(|result| result.map(|saved| saved.definition))
    }

    /// 保存前将完整定义映射到领域输入契约，拒绝空流程、过长命令及异常标签。
    pub async fn save_workflow(&self, workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        let steps: Vec<_> = workflow
            .steps
            .iter()
            .map(|step| WorkflowStepInput {
                id: Some(step.id.clone()),
                title: step.title.clone(),
                command: step.command.clone(),
                description: step.description.clone(),
                scope: step.scope,
                requires_confirmation: step.requires_confirmation,
            })
            .collect();
        validate_workflow(
            &workflow.title,
            workflow.description.as_deref(),
            &workflow.tags,
            &steps,
        )
        .map_err(|_| AppError::invalid("工作流定义未通过校验"))?;
        self.workflows
            .save_versioned(workflow, None)
            .await
            .map(|saved| saved.definition)
    }

    /// 更新时保留现有步骤身份由 repository 决定，输入仍统一经过领域校验。
    pub async fn update_workflow(
        &self,
        request: WorkflowUpdateRequest,
    ) -> AppResult<CommandWorkflow> {
        validate_workflow(
            &request.title,
            request.description.as_deref(),
            &request.tags,
            &request.steps,
        )
        .map_err(|_| AppError::invalid("工作流定义未通过校验"))?;
        let current = self
            .workflows
            .get_versioned(request.id.clone())
            .await?
            .ok_or_else(|| AppError::not_found("工作流不存在"))?;
        if request.steps.iter().any(|input| {
            input
                .id
                .as_deref()
                .is_some_and(|id| !current.definition.steps.iter().any(|step| step.id == id))
        }) {
            return Err(AppError::invalid("工作流步骤标识不属于当前定义"));
        }
        let workflow = apply_workflow_update(current.definition, request)?;
        self.workflows
            .save_versioned(workflow, current.revision)
            .await
            .map(|saved| saved.definition)
    }

    /// 以读取时 revision 原子删除定义；运行实例仍绑定已启动的快照。
    pub async fn delete_workflow(&self, workflow_id: String) -> AppResult<bool> {
        validate_catalog_id(&workflow_id)?;
        let Some(current) = self.workflows.get_versioned(workflow_id.clone()).await? else {
            return Ok(false);
        };
        let revision = current
            .revision
            .ok_or_else(|| AppError::conflict("工作流缺少 revision，无法安全删除"))?;
        self.workflows.delete_versioned(workflow_id, revision).await
    }

    /// 返回显式 session 上的建议候选，不从当前 Tab 或其它 session 猜目标。
    pub async fn suggestions(
        &self,
        request: CommandSuggestionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        if request.input.chars().count() > 4_000 {
            return Err(AppError::invalid("建议输入超过长度限制"));
        }
        self.suggestions.suggest(request, cancellation).await
    }

    /// 按候选来源执行最小化反馈存储；snippet 只写标识，禁止把展开内容持久化。
    pub async fn record_suggestion_feedback(
        &self,
        mut request: CommandSuggestionFeedbackRequest,
    ) -> AppResult<CommandSuggestionFeedbackOutcome> {
        if request.provider == SuggestionProviderKind::Snippet {
            let snippet_id = request
                .source_id
                .as_deref()
                .filter(|value| valid_feedback_source_id(value))
                .unwrap_or("unknown");
            request.input = "@snippet".to_owned();
            request.replacement_text = format!("@snippet:{snippet_id}");
        } else if input_is_sensitive(&request.input)
            || input_is_sensitive(&request.replacement_text)
        {
            return Ok(CommandSuggestionFeedbackOutcome::SkippedSensitive);
        }
        if request.input.chars().count() > 4_000
            || request.replacement_text.chars().count() > 4_000
            || request.input.contains('\0')
            || request.replacement_text.contains('\0')
        {
            return Ok(CommandSuggestionFeedbackOutcome::SkippedSensitive);
        }
        let repository = self
            .suggestion_feedback
            .as_ref()
            .ok_or_else(|| AppError::capability_not_supported("命令建议反馈存储未装配"))?;
        repository.record_feedback(request).await?;
        Ok(CommandSuggestionFeedbackOutcome::Recorded)
    }

    /// 按当前 settings 的 feedback/audit retention 清理既有数据库记录，供启动用例调用一次。
    pub async fn prune_suggestion_data(&self) -> AppResult<CommandSuggestionRetentionResult> {
        let preferences = self
            .preferences
            .as_ref()
            .ok_or_else(|| AppError::capability_not_supported("建议保留期未装配设置读取端口"))?;
        let repository = self
            .suggestion_feedback
            .as_ref()
            .ok_or_else(|| AppError::capability_not_supported("建议保留期未装配历史存储端口"))?;
        let settings = preferences
            .load_settings()
            .await?
            .settings
            .terminal
            .inline_suggestion;
        repository
            .prune_expired(
                settings.feedback_retention_days,
                settings.audit_retention_days,
            )
            .await
    }

    /// 直接调用以单一取消令牌执行广播，并复用统一的逐目标编排与回执构造。
    ///
    /// @author kongweiguang
    pub async fn broadcast(
        &self,
        command: String,
        targets: Vec<BroadcastTarget>,
        bindings: Vec<BroadcastSessionBinding>,
        confirmed: bool,
        cancellation: CancellationToken,
    ) -> AppResult<BroadcastExecutionResult> {
        self.broadcast_inner(
            command,
            targets,
            BroadcastBindings::Stable(bindings),
            confirmed,
            &[cancellation],
            BroadcastMode {
                history_source: CommandHistorySource::Broadcast,
                sensitive: false,
            },
        )
        .await
    }

    /// 借用取消源集合复用单令牌和多令牌入口；UI 片段先用 trim 结果拒绝空白，再恢复原展开正文，普通广播保留既有 trim 语义。
    ///
    /// @author kongweiguang
    async fn broadcast_inner(
        &self,
        command: String,
        targets: Vec<BroadcastTarget>,
        bindings: BroadcastBindings,
        confirmed: bool,
        cancellations: &[CancellationToken],
        mode: BroadcastMode,
    ) -> AppResult<BroadcastExecutionResult> {
        let binding_count = match &bindings {
            BroadcastBindings::Stable(bindings) => bindings.len(),
            BroadcastBindings::Ui(bindings) => bindings.len(),
        };
        if targets.is_empty() || targets.len() > 64 || binding_count != targets.len() {
            return Err(AppError::invalid("广播目标数量无效"));
        }
        let mut analysis = BroadcastAnalysis::analyze(&command, targets);
        if !analysis.is_sendable() {
            return Err(AppError::invalid("广播命令或目标为空"));
        }
        if analysis.requires_confirmation && !confirmed {
            return Err(AppError::conflict("广播需要用户确认"));
        }
        if matches!(&bindings, BroadcastBindings::Ui(_)) {
            analysis.command = command;
            analysis.data = format!("{}\r", analysis.command);
        }
        validate_broadcast_binding_shape(&analysis, &bindings)?;
        let sessions = self.sessions.list(SessionFilter::default()).await?;
        let resolved = resolve_broadcast_targets(&analysis, &bindings, &sessions)?;
        self.deliver_broadcast_targets(BroadcastDeliveryPlan {
            analysis,
            targets: resolved,
            sessions: &sessions,
            cancellations,
            mode,
        })
        .await
    }

    /// 逐目标一次写入，并共享普通广播和片段发送的取消、敏感历史与部分失败语义。
    ///
    /// @author kongweiguang
    async fn deliver_broadcast_targets(
        &self,
        plan: BroadcastDeliveryPlan<'_>,
    ) -> AppResult<BroadcastExecutionResult> {
        let mut deliveries = Vec::with_capacity(plan.targets.len());
        for target in plan.targets {
            let cancelled = plan
                .cancellations
                .iter()
                .any(CancellationToken::is_cancelled);
            if cancelled {
                deliveries.push(BroadcastDeliveryResult {
                    pane_id: target.pane_id,
                    session_id: target.session_id,
                    attempted: false,
                    sent: false,
                    cancelled: true,
                    error_code: None,
                    history_recorded: None,
                    history_skip_reason: None,
                    history_error_code: None,
                });
                continue;
            }
            let session = plan
                .sessions
                .iter()
                .find(|session| session.session_id == target.session_id)
                .ok_or_else(|| AppError::not_found("广播终端会话不存在"))?;
            let result = match &target.ui_binding {
                Some(binding) => self
                    .sessions
                    .submit_ui_snippet_command_line(UiSnippetCommandLineRequest {
                        binding: binding.clone(),
                        command: plan.analysis.command.clone(),
                    })
                    .await
                    .map(|_| ()),
                None => {
                    self.sessions
                        .write(
                            target.session_id.clone(),
                            plan.analysis.data.as_bytes().to_vec(),
                        )
                        .await
                }
            };
            let (sent, error_code) = match result {
                Ok(()) => (true, None),
                Err(error) => (false, Some(error.public_code())),
            };
            let (history_recorded, history_skip_reason, history_error_code) =
                if sent && plan.mode.sensitive {
                    (
                        Some(false),
                        Some("敏感片段不会写入命令历史".to_owned()),
                        None,
                    )
                } else if sent {
                    match self
                        .record_history(CommandHistoryRecordRequest {
                            command: plan.analysis.command.clone(),
                            source: plan.mode.history_source,
                            target: execution::history_target(&session.target),
                            record: Some(true),
                            session_id: Some(session.session_id.clone()),
                            pane_id: execution::owner_pane(&session.owner),
                            tab_id: execution::owner_tab(&session.owner),
                            profile_id: None,
                            remote_host_id: execution::target_host(&session.target),
                            cwd: execution::target_cwd(&session.target),
                            shell: execution::local_shell(&session.target),
                        })
                        .await
                    {
                        Ok(result) => (Some(result.recorded), result.skip_reason, None),
                        Err(error) => (None, None, Some(error.public_code())),
                    }
                } else {
                    (None, None, None)
                };
            deliveries.push(BroadcastDeliveryResult {
                pane_id: target.pane_id,
                session_id: target.session_id,
                attempted: true,
                sent,
                cancelled: false,
                error_code,
                history_recorded,
                history_skip_reason,
                history_error_code,
            });
        }
        Ok(BroadcastExecutionResult {
            analysis: plan.analysis,
            deliveries,
        })
    }

    /// 将用户取消与应用关闭统一收敛，并在每个目标边界复核原始信号以排除调度竞态。
    ///
    /// @author kongweiguang
    pub async fn broadcast_with_cancellation(
        &self,
        command: String,
        targets: Vec<BroadcastTarget>,
        bindings: Vec<BroadcastSessionBinding>,
        confirmed: bool,
        user_cancellation: CancellationToken,
        application_cancellation: CancellationToken,
    ) -> AppResult<BroadcastExecutionResult> {
        let cancellation = CancellationToken::new();
        if user_cancellation.is_cancelled() || application_cancellation.is_cancelled() {
            cancellation.cancel();
        }
        let cancellation_sources = [
            cancellation.clone(),
            user_cancellation.clone(),
            application_cancellation.clone(),
        ];
        let mut operation = Box::pin(self.broadcast_inner(
            command,
            targets,
            BroadcastBindings::Stable(bindings),
            confirmed,
            &cancellation_sources,
            BroadcastMode {
                history_source: CommandHistorySource::Broadcast,
                sensitive: false,
            },
        ));
        tokio::select! {
            result = &mut operation => return result,
            _ = user_cancellation.cancelled() => {},
            _ = application_cancellation.cancelled() => {},
        }
        cancellation.cancel();
        operation.await
    }

    /// 片段按钮本身是用户的发送动作，因此不再弹第二次风险确认；敏感正文不会进入历史。
    ///
    /// @author kongweiguang
    pub async fn broadcast_snippet_with_cancellation(
        &self,
        snippet: SnippetRenderResult,
        targets: Vec<BroadcastTarget>,
        bindings: Vec<UiSessionBinding>,
        user_cancellation: CancellationToken,
        application_cancellation: CancellationToken,
    ) -> AppResult<BroadcastExecutionResult> {
        let cancellation = CancellationToken::new();
        if user_cancellation.is_cancelled() || application_cancellation.is_cancelled() {
            cancellation.cancel();
        }
        let cancellation_sources = [
            cancellation.clone(),
            user_cancellation.clone(),
            application_cancellation.clone(),
        ];
        let mut operation = Box::pin(self.broadcast_inner(
            snippet.command,
            targets,
            BroadcastBindings::Ui(bindings),
            true,
            &cancellation_sources,
            BroadcastMode {
                history_source: CommandHistorySource::Snippet,
                sensitive: snippet.sensitive,
            },
        ));
        tokio::select! {
            result = &mut operation => return result,
            _ = user_cancellation.cancelled() => {},
            _ = application_cancellation.cancelled() => {},
        }
        cancellation.cancel();
        operation.await
    }
}

/// 构造不会把敏感命令或上下文保留到历史中的明确跳过结果。
fn skipped_history(reason: &str) -> CommandHistoryRecordResult {
    CommandHistoryRecordResult {
        recorded: false,
        entry: None,
        skip_reason: Some(reason.to_owned()),
    }
}

/// 拒绝空值、路径分隔符和控制字符，阻止 catalog ID 逃逸配置目录。
fn validate_catalog_id(value: &str) -> AppResult<()> {
    if value.trim().is_empty()
        || value.chars().count() > 160
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '/' | '\\'))
    {
        Err(AppError::invalid("目录标识无效"))
    } else {
        Ok(())
    }
}

/// 保证 broadcast 的 pane/session/target 三元组与风险分析中的目标集合完全一致。
fn validate_broadcast_bindings(
    analysis: &BroadcastAnalysis,
    bindings: &[BroadcastSessionBinding],
) -> AppResult<()> {
    for target in &analysis.targets {
        let count = bindings
            .iter()
            .filter(|binding| binding.pane_id == target.pane_id && binding.target == target.target)
            .count();
        if count != 1 {
            return Err(AppError::invalid("广播分屏绑定缺失或重复"));
        }
    }
    if bindings.iter().any(|binding| {
        !analysis
            .targets
            .iter()
            .any(|target| target.pane_id == binding.pane_id)
    }) {
        return Err(AppError::invalid("广播包含未分析的分屏"));
    }
    Ok(())
}

/// 在读取 session 快照前拒绝缺失、重复、非 UI 或未选中的片段目标绑定。
///
/// @author kongweiguang
fn validate_broadcast_binding_shape(
    analysis: &BroadcastAnalysis,
    bindings: &BroadcastBindings,
) -> AppResult<()> {
    match bindings {
        BroadcastBindings::Stable(bindings) => validate_broadcast_bindings(analysis, bindings),
        BroadcastBindings::Ui(bindings) => {
            if bindings
                .iter()
                .any(|binding| !matches!(&binding.owner, SessionOwner::Ui { .. }))
            {
                return Err(AppError::invalid("片段广播只能绑定可见 UI 分屏"));
            }
            for target in &analysis.targets {
                let count = bindings
                    .iter()
                    .filter(|binding| {
                        matches!(
                            &binding.owner,
                            SessionOwner::Ui { pane_id, .. } if pane_id == &target.pane_id
                        )
                    })
                    .count();
                if count != 1 {
                    return Err(AppError::invalid("片段广播分屏绑定缺失或重复"));
                }
            }
            if bindings.iter().any(|binding| {
                let SessionOwner::Ui { pane_id, .. } = &binding.owner else {
                    return true;
                };
                !analysis
                    .targets
                    .iter()
                    .any(|target| target.pane_id == *pane_id)
            }) {
                return Err(AppError::invalid("片段广播包含未选择的分屏"));
            }
            Ok(())
        }
    }
}

/// 在任何写入前确认显式 session 仍运行且仍归属相同 pane，失效目标不自动换 Tab。
fn resolve_broadcast_sessions(
    analysis: &BroadcastAnalysis,
    bindings: &[BroadcastSessionBinding],
    sessions: &[SessionInfo],
) -> AppResult<Vec<(PaneId, SessionId)>> {
    analysis
        .targets
        .iter()
        .map(|target| {
            let binding = bindings
                .iter()
                .find(|binding| binding.pane_id == target.pane_id)
                .ok_or_else(|| AppError::not_found("广播分屏绑定已失效"))?;
            let session = sessions
                .iter()
                .find(|session| session.session_id == binding.session_id)
                .ok_or_else(|| AppError::not_found("广播终端会话不存在"))?;
            let owned_by_pane = matches!(
                &session.owner,
                SessionOwner::Ui { pane_id, .. } if pane_id == &binding.pane_id
            );
            if !owned_by_pane
                || session.lifecycle != SessionLifecycle::Running
                || !target_matches_session(target.target, &session.target)
            {
                return Err(AppError::conflict("广播终端目标已经变化"));
            }
            Ok((binding.pane_id.clone(), binding.session_id.clone()))
        })
        .collect()
}

/// 解析稳定广播或完整 UI binding；片段 generation 最终仍由唯一 session writer 原子复核。
///
/// @author kongweiguang
fn resolve_broadcast_targets(
    analysis: &BroadcastAnalysis,
    bindings: &BroadcastBindings,
    sessions: &[SessionInfo],
) -> AppResult<Vec<ResolvedBroadcastTarget>> {
    match bindings {
        BroadcastBindings::Stable(bindings) => {
            resolve_broadcast_sessions(analysis, bindings, sessions).map(|targets| {
                targets
                    .into_iter()
                    .map(|(pane_id, session_id)| ResolvedBroadcastTarget {
                        pane_id,
                        session_id,
                        ui_binding: None,
                    })
                    .collect()
            })
        }
        BroadcastBindings::Ui(bindings) => analysis
            .targets
            .iter()
            .map(|target| {
                let binding = bindings
                    .iter()
                    .find(|binding| {
                        matches!(
                            &binding.owner,
                            SessionOwner::Ui { pane_id, .. } if pane_id == &target.pane_id
                        )
                    })
                    .ok_or_else(|| AppError::not_found("片段广播分屏绑定已失效"))?;
                let session = sessions
                    .iter()
                    .find(|session| session.session_id == binding.session_id)
                    .ok_or_else(|| AppError::not_found("片段广播终端会话不存在"))?;
                if session.owner != binding.owner
                    || session.lifecycle != SessionLifecycle::Running
                    || !target_matches_session(target.target, &session.target)
                {
                    return Err(AppError::conflict("片段广播终端目标已经变化"));
                }
                Ok(ResolvedBroadcastTarget {
                    pane_id: target.pane_id.clone(),
                    session_id: binding.session_id.clone(),
                    ui_binding: Some(binding.clone()),
                })
            })
            .collect(),
    }
}

/// 对照协议类别而不把本地与远端 host ID 丢失后误认为同一运行目标。
fn target_matches_session(expected: CommandHistoryTarget, actual: &SessionTarget) -> bool {
    matches!(
        (expected, actual),
        (CommandHistoryTarget::Local, SessionTarget::Local(_))
            | (CommandHistoryTarget::Ssh, SessionTarget::Ssh(_))
            | (CommandHistoryTarget::Telnet, SessionTarget::Telnet(_))
            | (CommandHistoryTarget::Serial, SessionTarget::Serial(_))
            | (CommandHistoryTarget::Container, SessionTarget::Container(_))
    )
}

/// 将工作流 update request 映射回已有定义，保留元数据并按 step ID 复用时间字段。
fn apply_workflow_update(
    mut current: CommandWorkflow,
    request: WorkflowUpdateRequest,
) -> AppResult<CommandWorkflow> {
    let mut steps = Vec::with_capacity(request.steps.len());
    for (index, input) in request.steps.into_iter().enumerate() {
        let existing = input
            .id
            .as_deref()
            .and_then(|id| current.steps.iter().find(|step| step.id == id));
        let id = existing.map_or_else(|| uuid::Uuid::new_v4().to_string(), |step| step.id.clone());
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .to_string();
        steps.push(kerminal_domain::commands::CommandWorkflowStep {
            id,
            title: input.title,
            command: input.command,
            description: input.description,
            scope: input.scope,
            requires_confirmation: input.requires_confirmation,
            sort_order: index as i64,
            created_at: existing.map_or_else(|| now.clone(), |step| step.created_at.clone()),
            updated_at: now,
        });
    }
    current.title = request.title;
    current.description = request.description;
    current.tags = request.tags;
    current.scope = request.scope;
    current.sort_order = request.sort_order;
    current.steps = steps;
    current.updated_at = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .to_string();
    Ok(current)
}

/// 对反馈内容应用领域敏感命令规则，空值或非法文本采取 fail-closed 处理。
fn input_is_sensitive(input: &str) -> bool {
    match NormalizedCommandHistory::new(input) {
        Ok(normalized) => normalized.sensitive_skip_reason().is_some(),
        Err(_) => true,
    }
}

/// 只允许将稳定、短且无路径语义的来源 ID 写入 snippet 反馈标记。
fn valid_feedback_source_id(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value.len() <= 160
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
