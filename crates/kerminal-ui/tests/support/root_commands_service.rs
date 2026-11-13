//! 只供工作台宿主测试装配命令用例，避免触碰真实配置或历史数据库。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

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

struct UnusedHistoryRepository {
    calls: Arc<CatalogCalls>,
}
struct UnusedCommandCatalog {
    calls: Arc<CatalogCalls>,
}
struct UnusedSuggestions;

/// 片段/工作流目录读取计数；只统计对应仓储的 list，两个计数互不影响。
#[derive(Default)]
pub struct CatalogCalls {
    pub snippet_lists: AtomicUsize,
    pub workflow_lists: AtomicUsize,
    /// 历史 list 计数供右栏接线测试区分“空结果成功”和“根本没有发起查询”。
    pub history_lists: AtomicUsize,
}

/// 暴露片段目录读取次数，供宿主测试确认切换终端不重启全局目录。
pub fn empty_commands_service_with_calls(
    sessions: Arc<dyn SessionService>,
) -> (CommandsService, Arc<CatalogCalls>) {
    let calls = Arc::new(CatalogCalls::default());
    let service = build_commands_service(
        Arc::new(UnusedHistoryRepository {
            calls: calls.clone(),
        }),
        sessions,
        calls.clone(),
    );
    (service, calls)
}

/// 注入带脚本或门控的历史仓储，同时复用隔离的空目录与会话端口。
/// 该 support 文件会被多个 `#[path]` 测试模块编译，只有右栏接线测试使用此入口；
/// 这里按函数单独放行未使用警告，不用模块级屏蔽掩盖其他真实问题。
///
/// @author kongweiguang
#[allow(dead_code)]
pub fn commands_service_with_history(
    history: Arc<dyn CommandHistoryRepository>,
    sessions: Arc<dyn SessionService>,
) -> (CommandsService, Arc<CatalogCalls>) {
    let calls = Arc::new(CatalogCalls::default());
    let service = build_commands_service(history, sessions, calls.clone());
    (service, calls)
}

/// 目录计数由调用方持有；历史仓储由不同测试按需注入。
fn build_commands_service(
    history: Arc<dyn CommandHistoryRepository>,
    sessions: Arc<dyn SessionService>,
    calls: Arc<CatalogCalls>,
) -> CommandsService {
    CommandsService::new(
        history,
        Arc::new(UnusedCommandCatalog {
            calls: calls.clone(),
        }),
        Arc::new(UnusedCommandCatalog {
            calls: calls.clone(),
        }),
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

    /// 空历史允许真实视图完成初始加载，同时记录查询次数以排除零请求伪成功。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        self.calls.history_lists.fetch_add(1, Ordering::AcqRel);
        Ok(Vec::new())
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
    /// 空内存目录让真实视图完成加载，避免测试读取用户配置文件。
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        self.calls.snippet_lists.fetch_add(1, Ordering::AcqRel);
        Ok(Vec::new())
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

    /// 工作台宿主测试 不读取用户片段定义。
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不保存片段定义。
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不删除片段定义。
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不读取片段版本快照。
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不执行版本化片段保存。
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        Err(AppError::unsupported(
            "snippet catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不执行版本化片段删除。
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
    /// 空工作流目录允许测试切换到工作流后再定位片段。
    async fn list(&self, _request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        self.calls.workflow_lists.fetch_add(1, Ordering::AcqRel);
        Ok(Vec::new())
    }

    /// 工作台宿主测试 不读取工作流定义。
    async fn get(&self, _workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不保存工作流定义。
    async fn save(&self, _workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不删除工作流定义。
    async fn delete(&self, _workflow_id: String) -> AppResult<bool> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不读取工作流版本快照。
    async fn get_versioned(
        &self,
        _workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不执行版本化工作流保存。
    async fn save_versioned(
        &self,
        _workflow: CommandWorkflow,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        Err(AppError::unsupported(
            "workflow catalog is not used in this test",
        ))
    }

    /// 工作台宿主测试 不执行版本化工作流删除。
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

    /// 工作台宿主测试 不执行命令建议查询。
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
