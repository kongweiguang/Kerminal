//! 只供 HTTP 生命周期测试装配命令用例，避免触碰真实配置或历史数据库。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandSuggestionProvider, CommandsService,
        SnippetImportBatchRequest, SnippetImportBatchState, SnippetRepository,
        VersionedCommandDefinition, WorkflowRepository,
    },
    sessions::SessionService,
};
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandSuggestionCandidate,
        CommandSuggestionRequest, CommandWorkflow, SnippetCatalogItem, SnippetCatalogListRequest,
        SnippetCatalogOrigin, SnippetDefinition, SuggestionProviderKind, WorkflowListRequest,
    },
    preferences::ConfigRevision,
};
use tokio_util::sync::CancellationToken;

struct UnusedHistoryRepository;
struct UnusedCommandCatalog;
struct UnusedSuggestions;

/// 构造不访问磁盘的 CommandsService；fake session 不提供可信 prompt，历史仓储不会被调用。
pub fn empty_commands_service(sessions: Arc<dyn SessionService>) -> CommandsService {
    CommandsService::new(
        Arc::new(UnusedHistoryRepository),
        Arc::new(UnusedCommandCatalog),
        Arc::new(UnusedCommandCatalog),
        Arc::new(UnusedSuggestions),
        sessions,
    )
}

#[async_trait]
impl CommandHistoryRepository for UnusedHistoryRepository {
    /// 标明 fake 仓储可用，历史提交一旦发生仍会进入 append 断言边界。
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 此 HTTP lifecycle 不验证历史查询，因此拒绝意外扩大测试职责。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        Err(AppError::unsupported(
            "history storage is not used in this test",
        ))
    }

    /// 生命周期 fixture 不提供可信 prompt；如意外记录历史则显式暴露错误。
    async fn append(&self, _draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        Err(AppError::unsupported(
            "history storage is not used in this test",
        ))
    }

    /// 本测试不覆盖删除历史工具。
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        Err(AppError::unsupported(
            "history storage is not used in this test",
        ))
    }

    /// 本测试不覆盖清理历史工具。
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        Err(AppError::unsupported(
            "history storage is not used in this test",
        ))
    }
}

#[async_trait]
impl SnippetRepository for UnusedCommandCatalog {
    /// HTTP lifecycle 不读取片段目录，防止测试碰触配置文件。
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 生命周期测试不验证目录身份；拒绝而非伪造“不存在”，避免掩盖意外调用。
    async fn catalog_identity_exists(
        &self,
        _origin: SnippetCatalogOrigin,
        _snippet_id: String,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 本测试不执行片段批量导入，保持文件与 journal 完全不触碰。
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        Err(AppError::unsupported(
            "snippet import is not used in this test",
        ))
    }

    /// 本测试不查询片段导入状态，避免把未装配的导入服务伪装成状态结果。
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        Err(AppError::unsupported(
            "snippet import is not used in this test",
        ))
    }

    /// HTTP lifecycle 不读取用户片段定义。
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不保存片段定义。
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不删除片段定义。
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不读取片段版本快照。
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不执行版本化片段保存。
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不执行版本化片段删除。
    async fn delete_user_versioned(
        &self,
        _snippet_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }
}

#[async_trait]
impl WorkflowRepository for UnusedCommandCatalog {
    /// HTTP lifecycle 不读取工作流目录。
    async fn list(&self, _request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不读取工作流定义。
    async fn get(&self, _workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不保存工作流定义。
    async fn save(&self, _workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不删除工作流定义。
    async fn delete(&self, _workflow_id: String) -> AppResult<bool> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不读取工作流版本快照。
    async fn get_versioned(
        &self,
        _workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不执行版本化工作流保存。
    async fn save_versioned(
        &self,
        _workflow: CommandWorkflow,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// HTTP lifecycle 不执行版本化工作流删除。
    async fn delete_versioned(
        &self,
        _workflow_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }
}

#[async_trait]
impl CommandSuggestionProvider for UnusedSuggestions {
    /// 不声明候选来源，避免测试运行时启动建议 provider。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        Vec::new()
    }

    /// HTTP lifecycle 不执行命令建议查询。
    async fn suggest(
        &self,
        _request: CommandSuggestionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        Err(AppError::unsupported(
            "suggestions are not used in this test",
        ))
    }
}
