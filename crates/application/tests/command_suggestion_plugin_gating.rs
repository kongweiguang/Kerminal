//! 验证 `kerminal.commands` 启停视图对 `::` 片段建议的入口门禁。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandSuggestionProvider, HistoryCommandSuggestionProvider,
        SnippetImportBatchRequest, SnippetImportBatchState, SnippetRepository,
        VersionedCommandDefinition,
    },
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder, capabilities::COMMANDS_UNIT},
};
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistoryTarget,
        CommandSuggestionRequest, SnippetCatalogItem, SnippetCatalogListRequest,
        SnippetCatalogOrigin, SnippetDefaultAction, SnippetDefinition, SnippetRisk, SnippetScope,
        SuggestionProviderKind, SuggestionQueryMode,
    },
    preferences::ConfigRevision,
};
use tokio_util::sync::CancellationToken;

/// 对未涉及的 command port 调用显式失败，避免 fake 隐式掩盖额外依赖。
///
/// @author kongweiguang
fn unused_port<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "command suggestion plugin gating fixture received an unrelated port call",
    ))
}

/// 片段门禁测试只查询 Snippet 来源，历史 port 不应被访问。
struct UnusedHistory;

#[async_trait]
impl CommandHistoryRepository for UnusedHistory {
    /// 门禁测试不依赖历史存储状态，固定报告可用即可。
    ///
    /// @author kongweiguang
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 请求已限定 Snippet 来源，历史枚举一旦发生说明门禁把范围扩大了。
    ///
    /// @author kongweiguang
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        unused_port()
    }

    /// 建议查询不写历史。
    ///
    /// @author kongweiguang
    async fn append(&self, _draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        unused_port()
    }

    /// 建议查询不删除历史。
    ///
    /// @author kongweiguang
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        unused_port()
    }

    /// 建议查询不清理历史。
    ///
    /// @author kongweiguang
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        unused_port()
    }
}

/// 统计目录读取次数，用于证明 commands 禁用时建议入口不触碰仓储。
struct CountingSnippets {
    lists: AtomicUsize,
}

impl CountingSnippets {
    /// 从零计数开始，测试只关心本次查询是否到达仓储。
    ///
    /// @author kongweiguang
    fn new() -> Self {
        Self {
            lists: AtomicUsize::new(0),
        }
    }

    /// 返回已发生的目录枚举次数，供门禁断言使用。
    ///
    /// @author kongweiguang
    fn list_count(&self) -> usize {
        self.lists.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl SnippetRepository for CountingSnippets {
    /// 记录一次目录读取并返回可被 `::de` 匹配的用户片段。
    ///
    /// @author kongweiguang
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        self.lists.fetch_add(1, Ordering::SeqCst);
        Ok(vec![catalog_item()])
    }

    /// 建议查询不校验片段收藏身份。
    ///
    /// @author kongweiguang
    async fn catalog_identity_exists(
        &self,
        _origin: SnippetCatalogOrigin,
        _snippet_id: String,
    ) -> AppResult<bool> {
        unused_port()
    }

    /// 建议查询不导入片段批次。
    ///
    /// @author kongweiguang
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        unused_port()
    }

    /// 建议查询不读取导入回执。
    ///
    /// @author kongweiguang
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        unused_port()
    }

    /// 建议查询不读取可编辑片段。
    ///
    /// @author kongweiguang
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        unused_port()
    }

    /// 建议查询不保存片段。
    ///
    /// @author kongweiguang
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        unused_port()
    }

    /// 建议查询不删除片段。
    ///
    /// @author kongweiguang
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        unused_port()
    }

    /// 建议查询不读取版本化片段。
    ///
    /// @author kongweiguang
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        unused_port()
    }

    /// 建议查询不写入版本化片段。
    ///
    /// @author kongweiguang
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        unused_port()
    }

    /// 建议查询不删除版本化片段。
    ///
    /// @author kongweiguang
    async fn delete_user_versioned(
        &self,
        _snippet_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unused_port()
    }
}

/// 唯一目录项只提供门禁断言所需的标题与稳定 ID，其余字段保持中性值。
///
/// @author kongweiguang
fn catalog_item() -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: "user-deploy".to_owned(),
        origin: SnippetCatalogOrigin::User,
        title: "deploy".to_owned(),
        description: "部署".to_owned(),
        template: "deploy {{env}}".to_owned(),
        category: String::new(),
        pack: String::new(),
        tags: Vec::new(),
        scope: SnippetScope::Any,
        platforms: Vec::new(),
        shells: Vec::new(),
        capabilities: Vec::new(),
        risk: SnippetRisk::Inspect,
        sensitive: false,
        duration: String::new(),
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
        catalog_version: None,
        command_spec: None,
        owner: None,
        tested_version: None,
        source_name: None,
        source_url: None,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: 0,
        updated_at: String::new(),
    }
}

/// 用显式内置状态冻结生产声明，只改变 commands 单元的启用视图。
///
/// @author kongweiguang
fn registry(commands_enabled: bool) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states = BTreeMap::from([(
        PluginId::new(COMMANDS_UNIT).expect("commands 是合法单元身份"),
        commands_enabled,
    )]);
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("内置启停视图应可冻结"),
    )
}

/// 输入固定为 `::de`，只请求 Snippet 来源，避免历史查询干扰仓储计数。
///
/// @author kongweiguang
fn request() -> CommandSuggestionRequest {
    CommandSuggestionRequest {
        input: "::de".to_owned(),
        cursor: 4,
        target: CommandHistoryTarget::Local,
        session_id: None,
        pane_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: None,
        shell: None,
        providers: Some(vec![SuggestionProviderKind::Snippet]),
        limit: Some(20),
        mode: SuggestionQueryMode::Inline,
        generation: Some(1),
        context_key: Some("context".to_owned()),
    }
}

/// commands 禁用时 `::` 前缀必须立即返回空，且不访问片段目录。
///
/// 片段面板已经拒绝打开，若此处仍产出候选，用户会看到一个无法完成的能力入口。
///
/// @author kongweiguang
#[tokio::test]
async fn disabled_commands_suppress_snippet_candidates_without_repository_access() {
    let snippets = Arc::new(CountingSnippets::new());
    let provider = HistoryCommandSuggestionProvider::new(
        Arc::new(UnusedHistory),
        Arc::clone(&snippets) as Arc<dyn SnippetRepository>,
    )
    .with_plugins(registry(false));

    let candidates = provider
        .suggest(request(), CancellationToken::new())
        .await
        .expect("禁用 commands 应按空建议成功返回");

    assert!(candidates.is_empty());
    assert_eq!(snippets.list_count(), 0);
}

/// commands 启用时保持既有片段候选与目录查询行为。
///
/// @author kongweiguang
#[tokio::test]
async fn enabled_commands_keep_snippet_candidates() {
    let snippets = Arc::new(CountingSnippets::new());
    let provider = HistoryCommandSuggestionProvider::new(
        Arc::new(UnusedHistory),
        Arc::clone(&snippets) as Arc<dyn SnippetRepository>,
    )
    .with_plugins(registry(true));

    let candidates = provider
        .suggest(request(), CancellationToken::new())
        .await
        .expect("启用 commands 时片段建议应正常返回");

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, "snippet:user-deploy");
    assert_eq!(candidates[0].display_text, "deploy");
    assert_eq!(snippets.list_count(), 1);
}

/// 未注入 registry 的离线/旧测试构造保持原行为，不被 commands 门禁误伤。
///
/// @author kongweiguang
#[tokio::test]
async fn provider_without_registry_keeps_legacy_snippet_candidates() {
    let snippets = Arc::new(CountingSnippets::new());
    let provider = HistoryCommandSuggestionProvider::new(
        Arc::new(UnusedHistory),
        Arc::clone(&snippets) as Arc<dyn SnippetRepository>,
    );

    let candidates = provider
        .suggest(request(), CancellationToken::new())
        .await
        .expect("未注入 registry 时保持原有建议行为");

    assert_eq!(candidates.len(), 1);
    assert_eq!(snippets.list_count(), 1);
}
