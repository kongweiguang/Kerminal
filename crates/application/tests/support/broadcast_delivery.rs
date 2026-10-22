//! 通过公开 CommandsService 装配广播交付回归所需的隔离端口。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, VecDeque},
    ffi::OsString,
    sync::{Arc, Mutex},
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
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalInputEvent, TerminalPalette, TerminalSearchRequest,
        TerminalSearchResult, TerminalSelection, UiCommandLineRequest, UiSessionBinding,
    },
};
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistoryTarget,
        CommandSuggestionCandidate, CommandSuggestionRequest, CommandWorkflow, SnippetCatalogItem,
        SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetDefinition, SuggestionProviderKind,
        WorkflowListRequest,
    },
    identity::{PaneId, SessionId, TabId},
    preferences::ConfigRevision,
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio_util::sync::CancellationToken;

/// 对未涉及的 command port 调用失败闭合，防止测试 fake 隐式扩大职责。
///
/// @author kongweiguang
fn unused_port<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "broadcast delivery fixture received an unrelated port call",
    ))
}

/// 历史 port 只需接受成功目标的记录；广播结果仍由真实 CommandsService 产生。
struct BroadcastHistory;

#[async_trait]
impl CommandHistoryRepository for BroadcastHistory {
    /// 广播用例不检查存储启动状态，固定报告可用以走通成功目标的历史路径。
    ///
    /// @author kongweiguang
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 广播不读取历史；意外查询明确拒绝，避免测试吞掉额外依赖。
    ///
    /// @author kongweiguang
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        unused_port()
    }

    /// 成功写入只需获得一个固定回执；draft 原字段回填以验证 service 走过真实 history port。
    ///
    /// @author kongweiguang
    async fn append(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        Ok(CommandHistoryEntry {
            id: "broadcast-test-history".to_owned(),
            command: draft.command,
            source: draft.source,
            target: draft.target,
            session_id: draft.session_id,
            pane_id: draft.pane_id,
            tab_id: draft.tab_id,
            profile_id: draft.profile_id,
            remote_host_id: draft.remote_host_id,
            cwd: draft.cwd,
            shell: draft.shell,
            created_at: "2026-10-22T00:00:00Z".to_owned(),
        })
    }

    /// 删除不属于广播回归，任何意外调用都应暴露。
    ///
    /// @author kongweiguang
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        unused_port()
    }

    /// 清理不属于广播回归，任何意外调用都应暴露。
    ///
    /// @author kongweiguang
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        unused_port()
    }
}

/// 广播不访问片段目录；测试 adapter 对全部片段写路径显式拒绝。
struct UnusedSnippets;

#[async_trait]
impl SnippetRepository for UnusedSnippets {
    /// 本回归不读取片段目录。
    ///
    /// @author kongweiguang
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        unused_port()
    }

    /// 本回归不导入片段。
    ///
    /// @author kongweiguang
    async fn catalog_identity_exists(
        &self,
        _origin: SnippetCatalogOrigin,
        _snippet_id: String,
    ) -> AppResult<bool> {
        unused_port()
    }

    /// 本回归不提交片段批次。
    ///
    /// @author kongweiguang
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        unused_port()
    }

    /// 本回归不读取片段批次回执。
    ///
    /// @author kongweiguang
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        unused_port()
    }

    /// 本回归不读取可编辑片段。
    ///
    /// @author kongweiguang
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        unused_port()
    }

    /// 本回归不保存片段。
    ///
    /// @author kongweiguang
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        unused_port()
    }

    /// 本回归不删除片段。
    ///
    /// @author kongweiguang
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        unused_port()
    }

    /// 本回归不读取版本化片段。
    ///
    /// @author kongweiguang
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        unused_port()
    }

    /// 本回归不保存版本化片段。
    ///
    /// @author kongweiguang
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        unused_port()
    }

    /// 本回归不删除版本化片段。
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

/// 广播不访问工作流目录；未请求的工作流能力失败闭合。
struct UnusedWorkflows;

#[async_trait]
impl WorkflowRepository for UnusedWorkflows {
    /// 本回归不查询工作流目录。
    ///
    /// @author kongweiguang
    async fn list(&self, _request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        unused_port()
    }

    /// 本回归不读取工作流。
    ///
    /// @author kongweiguang
    async fn get(&self, _workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        unused_port()
    }

    /// 本回归不保存工作流。
    ///
    /// @author kongweiguang
    async fn save(&self, _workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        unused_port()
    }

    /// 本回归不删除工作流。
    ///
    /// @author kongweiguang
    async fn delete(&self, _workflow_id: String) -> AppResult<bool> {
        unused_port()
    }

    /// 本回归不读取版本化工作流。
    ///
    /// @author kongweiguang
    async fn get_versioned(
        &self,
        _workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        unused_port()
    }

    /// 本回归不保存版本化工作流。
    ///
    /// @author kongweiguang
    async fn save_versioned(
        &self,
        _workflow: CommandWorkflow,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        unused_port()
    }

    /// 本回归不删除版本化工作流。
    ///
    /// @author kongweiguang
    async fn delete_versioned(
        &self,
        _workflow_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unused_port()
    }
}

/// 建议 provider 不参与广播；空能力列表让测试只覆盖 CommandsService 的显式广播入口。
struct UnusedSuggestions;

#[async_trait]
impl CommandSuggestionProvider for UnusedSuggestions {
    /// 广播没有建议查询来源。
    ///
    /// @author kongweiguang
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        Vec::new()
    }

    /// 本回归不生成终端建议。
    ///
    /// @author kongweiguang
    async fn suggest(
        &self,
        _request: CommandSuggestionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        Ok(Vec::new())
    }
}

/// 一次终端写入的确定结果；取消动作发生在该目标已经进入 write 后。
pub(crate) enum WriteAction {
    Succeed,
    OutcomeUnknown,
    CancelThenSucceed(CancellationToken),
}

/// 记录真实用例传来的目标顺序，并在 write 边界注入确定结果。
pub(crate) struct BroadcastSessions {
    sessions: Vec<SessionInfo>,
    actions: Mutex<VecDeque<WriteAction>>,
    writes: Mutex<Vec<SessionId>>,
    list_calls: Mutex<usize>,
}

impl BroadcastSessions {
    /// 固定会话快照与逐次写入回执，使测试覆盖应用端的 validation 和 fanout。
    ///
    /// @author kongweiguang
    pub(crate) fn new(
        sessions: Vec<SessionInfo>,
        actions: impl IntoIterator<Item = WriteAction>,
    ) -> Self {
        Self {
            sessions,
            actions: Mutex::new(actions.into_iter().collect()),
            writes: Mutex::new(Vec::new()),
            list_calls: Mutex::new(0),
        }
    }

    /// 返回每个实际进入 writer 的 session 顺序，区分未尝试目标与未知写入结果。
    ///
    /// @author kongweiguang
    pub(crate) fn writes(&self) -> Vec<SessionId> {
        self.writes
            .lock()
            .expect("broadcast fixture mutex should not be poisoned")
            .clone()
    }

    /// 无效绑定应在读取运行会话快照之前被拒绝。
    ///
    /// @author kongweiguang
    pub(crate) fn list_calls(&self) -> usize {
        *self
            .list_calls
            .lock()
            .expect("broadcast fixture mutex should not be poisoned")
    }
}

#[async_trait]
impl SessionService for BroadcastSessions {
    /// 广播只操作已存在的 session，不从测试 adapter 创建额外会话。
    ///
    /// @author kongweiguang
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unused_port()
    }

    /// 广播只操作已存在的 session，不从测试 adapter 创建额外会话。
    ///
    /// @author kongweiguang
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_port()
    }

    /// 返回一次冻结的会话快照，后续广播必须沿显式 binding 解析这些目标。
    ///
    /// @author kongweiguang
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        *self
            .list_calls
            .lock()
            .expect("broadcast fixture mutex should not be poisoned") += 1;
        Ok(self.sessions.clone())
    }

    /// 广播不依赖运行时元数据。
    ///
    /// @author kongweiguang
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unused_port()
    }

    /// 每个调用都代表真实广播目标已尝试；故障与取消都在此边界注入。
    ///
    /// @author kongweiguang
    async fn write(&self, session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        self.writes
            .lock()
            .expect("broadcast fixture mutex should not be poisoned")
            .push(session_id);
        let action = self
            .actions
            .lock()
            .expect("broadcast fixture mutex should not be poisoned")
            .pop_front()
            .unwrap_or(WriteAction::OutcomeUnknown);
        match action {
            WriteAction::Succeed => Ok(()),
            WriteAction::OutcomeUnknown => Err(AppError::outcome_unknown(
                "broadcast terminal write outcome is unknown",
            )),
            WriteAction::CancelThenSucceed(cancellation) => {
                cancellation.cancel();
                tokio::task::yield_now().await;
                Ok(())
            }
        }
    }

    /// 广播用例发送已分析的命令字节，不通过输入事件路径。
    ///
    /// @author kongweiguang
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 广播不提交 UI command-line writer 请求。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<kerminal_application::sessions::TerminalCommandContext> {
        unused_port()
    }

    /// 广播不调整终端网格。
    ///
    /// @author kongweiguang
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unused_port()
    }

    /// 广播不读取 GPU 渲染快照。
    ///
    /// @author kongweiguang
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unused_port()
    }

    /// 广播不更新终端调色板。
    ///
    /// @author kongweiguang
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 广播不调整终端 scrollback。
    ///
    /// @author kongweiguang
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unused_port()
    }

    /// 广播不读取纯文本快照。
    ///
    /// @author kongweiguang
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unused_port()
    }

    /// 广播不滚动终端视口。
    ///
    /// @author kongweiguang
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unused_port()
    }

    /// 广播不滚动绑定后的可见 UI 视口。
    ///
    /// @author kongweiguang
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 广播不修改终端文本选择。
    ///
    /// @author kongweiguang
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 广播不读取终端文本选择。
    ///
    /// @author kongweiguang
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unused_port()
    }

    /// 广播不搜索终端 scrollback。
    ///
    /// @author kongweiguang
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unused_port()
    }

    /// 广播不重连终端会话。
    ///
    /// @author kongweiguang
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_port()
    }

    /// 广播不关闭会话。
    ///
    /// @author kongweiguang
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unused_port()
    }

    /// 广播不执行 deadline 关闭。
    ///
    /// @author kongweiguang
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: SessionDeadline,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 广播不关闭会话集合。
    ///
    /// @author kongweiguang
    async fn close_all(&self) -> AppResult<()> {
        unused_port()
    }

    /// 广播不订阅会话状态。
    ///
    /// @author kongweiguang
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unused_port()
    }

    /// 广播不执行结构化 shell 命令。
    ///
    /// @author kongweiguang
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        unused_port()
    }
}

/// 在 public CommandsService 构造边界装配测试 ports，不访问应用私有字段。
///
/// @author kongweiguang
pub(crate) fn commands_service(sessions: Arc<BroadcastSessions>) -> CommandsService {
    CommandsService::new(
        Arc::new(BroadcastHistory),
        Arc::new(UnusedSnippets),
        Arc::new(UnusedWorkflows),
        Arc::new(UnusedSuggestions),
        sessions,
    )
}

/// 构造真实运行中的本地 UI session，owner pane 与传入 binding 一致。
///
/// @author kongweiguang
pub(crate) fn ui_session(session_id: SessionId, tab_id: TabId, pane_id: PaneId) -> SessionInfo {
    SessionInfo {
        session_id,
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        }),
        owner: SessionOwner::Ui { tab_id, pane_id },
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    }
}

/// 显式映射领域目标与应用绑定，避免广播测试用可见名称推断 session 身份。
///
/// @author kongweiguang
pub(crate) fn target_and_binding(
    session_id: SessionId,
    pane_id: PaneId,
    target: CommandHistoryTarget,
    title: &str,
) -> (
    kerminal_domain::commands::BroadcastTarget,
    kerminal_application::commands::BroadcastSessionBinding,
) {
    (
        kerminal_domain::commands::BroadcastTarget {
            pane_id: pane_id.clone(),
            title: title.to_owned(),
            target,
        },
        kerminal_application::commands::BroadcastSessionBinding {
            pane_id,
            session_id,
            target,
        },
    )
}
