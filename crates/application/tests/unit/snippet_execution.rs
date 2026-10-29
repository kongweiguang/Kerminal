//! 片段运行的独立用例回归：确认、目录版本、终端回执与安全历史相互隔离。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget,
        CommandWorkflow, SnippetApplicabilityReason, SnippetApplicabilityStatus,
        SnippetCatalogItem, SnippetCatalogListRequest, SnippetCatalogOrigin, SnippetDefaultAction,
        SnippetDefinition, SnippetPreference, SnippetPreferenceKey, SnippetRisk, SnippetScope,
        SnippetUsageClearResult, SnippetUsageReceipt, SnippetUsageRecordRequest, SnippetVariable,
        SnippetVariableKind, SnippetVariableRenderStrategy, SuggestionProviderKind,
        WorkflowListRequest,
    },
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
    preferences::ConfigRevision,
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult, PublicErrorCode,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandSuggestionCandidate, CommandSuggestionProvider,
        CommandSuggestionRequest, CommandsService, SnippetExecutionService, SnippetFillPlan,
        SnippetImportBatchRequest, SnippetImportBatchState, SnippetPreferenceAvailability,
        SnippetPreferenceRepository, SnippetRenderResult, SnippetRepository, SnippetRunHistory,
        SnippetRunPreparation, SnippetRunResult, SnippetRunUsage, VersionedCommandDefinition,
        WorkflowRepository,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalCommandAvailability, TerminalCommandContext,
        TerminalCommandUnavailableReason, TerminalInputEvent, TerminalPalette,
        TerminalSearchRequest, TerminalSearchResult, TerminalSelection, UiCommandLineRequest,
        UiInputProjection, UiSessionBinding, UiTerminalInputRequest,
    },
};

const SNIPPET_ID: &str = "fixture-snippet";
const SESSION_TEXT: &str = "fixture-session";

#[derive(Clone, Copy)]
enum ExecutionMode {
    Finished,
    Failed,
    Rejected,
    Unknown,
    MismatchedFinished,
    MismatchedUnknown,
    WaitForCancellation,
    WaitForLateFinishedReceipt,
}

/// 控制 fake 的 Paste 回执，使 Unknown 分支可以验证不能在不确定写入后重放。
#[derive(Clone, Copy)]
enum UiInputMode {
    Accepted,
    OutcomeUnknown,
}

struct MemorySessions {
    info: Mutex<SessionInfo>,
    context: Mutex<TerminalCommandContext>,
    mode: ExecutionMode,
    ui_input_mode: Mutex<UiInputMode>,
    calls: AtomicUsize,
    ui_input_calls: AtomicUsize,
    requests: Mutex<Vec<ShellCommandRequest>>,
    ui_input_requests: Mutex<Vec<UiTerminalInputRequest>>,
    command_started: tokio::sync::Notify,
    cancellation_observed: tokio::sync::Notify,
    release_cancelled_result: tokio::sync::Notify,
    cancelled_result_released: tokio::sync::Notify,
}

impl MemorySessions {
    /// 用确定的 UI owner、目标和 shell tracker 固定测试对象，不创建外部进程。
    /// @author kongweiguang
    fn new(info: SessionInfo, context: TerminalCommandContext, mode: ExecutionMode) -> Self {
        Self {
            info: Mutex::new(info),
            context: Mutex::new(context),
            mode,
            ui_input_mode: Mutex::new(UiInputMode::Accepted),
            calls: AtomicUsize::new(0),
            ui_input_calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
            ui_input_requests: Mutex::new(Vec::new()),
            command_started: tokio::sync::Notify::new(),
            cancellation_observed: tokio::sync::Notify::new(),
            release_cancelled_result: tokio::sync::Notify::new(),
            cancelled_result_released: tokio::sync::Notify::new(),
        }
    }

    /// 更新可信方言而保留会话代次与提示符状态，验证同一 binding 下的适用性变化。
    fn set_shell_dialect(&self, shell_dialect: RemoteShellDialect) {
        let mut context = self
            .context
            .lock()
            .expect("session fixture mutex should not be poisoned");
        context.shell_dialect = shell_dialect;
    }

    /// 返回真实 use case 交给 terminal port 的命令请求，仅在测试进程内检查安全策略。
    fn requests(&self) -> Vec<ShellCommandRequest> {
        self.requests
            .lock()
            .expect("session fixture mutex should not be poisoned")
            .clone()
    }

    /// 返回终端 use case 调用次数，以断言拒绝和 Unknown 路径不会自动重放。
    fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// 返回 owner 约束输入 writer 收到的原始事务，供测试核对 Paste 类型和版本绑定。
    /// @author kongweiguang
    fn ui_input_requests(&self) -> Vec<UiTerminalInputRequest> {
        self.ui_input_requests
            .lock()
            .expect("UI input request fixture mutex should not be poisoned")
            .clone()
    }

    /// 返回 Paste writer 调用次数，确保 stale 与取消分支在写入边界之前闭合。
    /// @author kongweiguang
    fn ui_input_call_count(&self) -> usize {
        self.ui_input_calls.load(Ordering::SeqCst)
    }

    /// 设置下次 owner 约束输入的回执，模拟已提交但调用方无法确认的 writer 结果。
    /// @author kongweiguang
    fn set_ui_input_mode(&self, mode: UiInputMode) {
        *self
            .ui_input_mode
            .lock()
            .expect("UI input mode fixture mutex should not be poisoned") = mode;
    }

    /// 替换固定 session 目标，以验证 plan 不会被重连或目标切换静默改绑。
    /// @author kongweiguang
    fn set_target(&self, target: SessionTarget) {
        self.info
            .lock()
            .expect("session fixture mutex should not be poisoned")
            .target = target;
    }
}

#[async_trait]
impl SessionService for MemorySessions {
    /// 片段测试只绑定预先构造的 session，不创建第二个终端。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unsupported()
    }

    /// 片段测试不创建终端，也不调用真实 transport factory。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unsupported()
    }

    /// 按请求筛选返回固定 session，不让测试隐式依赖当前 Tab；可变快照用于 stale 目标用例。
    /// @author kongweiguang
    async fn list(&self, filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        let info = self
            .info
            .lock()
            .expect("session fixture mutex should not be poisoned");
        if filter
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &info.owner)
            || filter
                .target
                .as_ref()
                .is_some_and(|target| target != &info.target)
        {
            return Ok(Vec::new());
        }
        Ok(vec![info.clone()])
    }

    /// 此场景不查询本地 PID 或 shell integration 安装状态。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unsupported()
    }

    /// 返回固定可信 tracker 状态，供渲染器按实际 Posix dialect 转义变量。
    async fn terminal_command_context(
        &self,
        _session_id: SessionId,
    ) -> AppResult<TerminalCommandContext> {
        Ok(self
            .context
            .lock()
            .expect("session fixture mutex should not be poisoned")
            .clone())
    }

    /// 片段运行经受管命令端口执行，不走 raw bytes 写入。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        unsupported()
    }

    /// 片段测试不注入按键或粘贴事件。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unsupported()
    }

    /// 模拟真实 UI writer 的 owner/context 准入；携带 context 的输入只有 Tracked 投影才可写入。
    /// Unknown 模式保留已到达 writer 的请求，用于证明应用层不会隐式重放。
    /// @author kongweiguang
    async fn write_ui_input(
        &self,
        request: UiTerminalInputRequest,
    ) -> AppResult<TerminalCommandContext> {
        self.ui_input_calls.fetch_add(1, Ordering::SeqCst);
        let invalid_context_projection =
            request.expected_context.is_some() && request.projection != UiInputProjection::Tracked;
        self.ui_input_requests
            .lock()
            .expect("UI input request fixture mutex should not be poisoned")
            .push(request);
        if invalid_context_projection {
            return Err(AppError::conflict(
                "fixture context-bound UI input requires a tracked projection",
            ));
        }
        if matches!(
            *self
                .ui_input_mode
                .lock()
                .expect("UI input mode fixture mutex should not be poisoned"),
            UiInputMode::OutcomeUnknown
        ) {
            return Err(AppError::outcome_unknown(
                "fixture UI paste acknowledgement unknown",
            ));
        }
        Ok(self
            .context
            .lock()
            .expect("session fixture mutex should not be poisoned")
            .clone())
    }

    /// 片段执行通过 shell integration，不得退化为可见命令行逐行提交。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        unsupported()
    }

    /// 片段执行不改变目标终端网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不构造 GPU 终端渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unsupported()
    }

    /// 此测试不改写终端颜色配置。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不改写终端 scrollback 限额。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unsupported()
    }

    /// 片段运行使用 OSC 133 结果端口，不从屏幕快照推测完成状态。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unsupported()
    }

    /// 本用例不改变终端滚动位置。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不经可见 pane 视口执行绑定滚动。
    async fn scroll_ui(
        &self,
        _binding: UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不改变终端文本选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不读取终端选择区文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unsupported()
    }

    /// 此测试不查询 scrollback 文本。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unsupported()
    }

    /// 片段运行不重连已经固定的测试 session。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unsupported()
    }

    /// 此测试不关闭 session runtime。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不执行带 deadline 的 runtime 清理。
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: SessionDeadline,
    ) -> AppResult<()> {
        unsupported()
    }

    /// 此测试不关闭应用级终端集合。
    async fn close_all(&self) -> AppResult<()> {
        unsupported()
    }

    /// 此用例不订阅输出；执行结果由显式命令 outcome 返回。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unsupported()
    }

    /// 记录单次受管命令并返回可控终态，不启动 shell 子进程。
    /// 确定失败与准入拒绝不混用；取消后可返回迟到 Finished，确保取消请求不覆盖真实回执。
    /// @author kongweiguang
    async fn execute_shell_command(
        &self,
        request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let request_id = request.command_id.clone();
        self.requests
            .lock()
            .expect("session fixture mutex should not be poisoned")
            .push(request);
        Ok(match self.mode {
            ExecutionMode::Finished => ShellCommandOutcome::Finished {
                command_id: request_id,
                exit_code: 0,
            },
            ExecutionMode::Failed => ShellCommandOutcome::Finished {
                command_id: request_id,
                exit_code: 17,
            },
            ExecutionMode::Rejected => {
                return Err(AppError::conflict(
                    "fixture rejected command before injection",
                ));
            }
            ExecutionMode::Unknown => ShellCommandOutcome::Unknown {
                command_id: request_id,
                reason: crate::sessions::ShellCommandUnknownReason::MissingCompletion,
            },
            ExecutionMode::MismatchedFinished => ShellCommandOutcome::Finished {
                command_id: "foreign-command-id".to_owned(),
                exit_code: 0,
            },
            ExecutionMode::MismatchedUnknown => ShellCommandOutcome::Unknown {
                command_id: "foreign-command-id".to_owned(),
                reason: crate::sessions::ShellCommandUnknownReason::MissingCompletion,
            },
            ExecutionMode::WaitForCancellation | ExecutionMode::WaitForLateFinishedReceipt => {
                self.command_started.notify_one();
                tokio::time::timeout(Duration::from_secs(2), cancellation.cancelled())
                    .await
                    .map_err(|_| {
                        AppError::infrastructure("workflow test did not cancel command")
                    })?;
                self.cancellation_observed.notify_one();
                tokio::time::timeout(
                    Duration::from_secs(2),
                    self.release_cancelled_result.notified(),
                )
                .await
                .map_err(|_| {
                    AppError::infrastructure("workflow test did not release command result")
                })?;
                self.cancelled_result_released.notify_one();
                match self.mode {
                    ExecutionMode::WaitForCancellation => ShellCommandOutcome::Unknown {
                        command_id: request_id,
                        reason: crate::sessions::ShellCommandUnknownReason::Cancelled,
                    },
                    ExecutionMode::WaitForLateFinishedReceipt => ShellCommandOutcome::Finished {
                        command_id: request_id,
                        exit_code: 0,
                    },
                    _ => unreachable!("only cancellation receipt modes share this branch"),
                }
            }
        })
    }
}

struct MemorySnippets {
    versioned: Mutex<VersionedCommandDefinition<SnippetDefinition>>,
    catalog_item: Mutex<Option<SnippetCatalogItem>>,
}

impl MemorySnippets {
    /// 固定精确用户项及原始字节 revision，允许用例模拟外部文件编辑。
    fn new(definition: SnippetDefinition) -> Self {
        Self {
            versioned: Mutex::new(VersionedCommandDefinition {
                definition,
                revision: Some(ConfigRevision::from_sha256("revision-1".to_owned())),
            }),
            catalog_item: Mutex::new(None),
        }
    }

    /// 替换只读内置目录元数据，验证它参与运行计划的 revision，而不改变模板正文。
    fn replace_catalog_item(&self, item: SnippetCatalogItem) {
        *self
            .catalog_item
            .lock()
            .expect("snippet fixture catalog mutex should not be poisoned") = Some(item);
    }

    /// 在预览和运行之间替换文件与 revision，模拟另一个编辑器完成保存。
    fn replace(&self, definition: SnippetDefinition, revision: &str) {
        *self
            .versioned
            .lock()
            .expect("snippet fixture mutex should not be poisoned") = VersionedCommandDefinition {
            definition,
            revision: Some(ConfigRevision::from_sha256(revision.to_owned())),
        };
    }
}

#[async_trait]
impl SnippetRepository for MemorySnippets {
    /// 列表仅供目录测试；本测试调用精确的版本化读取端口。
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        Ok(self
            .catalog_item
            .lock()
            .expect("snippet fixture catalog mutex should not be poisoned")
            .clone()
            .into_iter()
            .collect())
    }

    /// 精确读取 fixture 中的内置项；用户项仍通过版本化用户定义端口读取。
    async fn get_catalog_item(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<Option<SnippetCatalogItem>> {
        Ok(self
            .catalog_item
            .lock()
            .expect("snippet fixture catalog mutex should not be poisoned")
            .as_ref()
            .filter(|item| item.origin == origin && item.id == snippet_id)
            .cloned())
    }

    /// 确认使用回执仍对应当前精确用户定义。
    async fn catalog_identity_exists(
        &self,
        origin: SnippetCatalogOrigin,
        snippet_id: String,
    ) -> AppResult<bool> {
        if origin == SnippetCatalogOrigin::Builtin {
            return Ok(self
                .catalog_item
                .lock()
                .expect("snippet fixture catalog mutex should not be poisoned")
                .as_ref()
                .is_some_and(|item| item.id == snippet_id));
        }
        let versioned = self
            .versioned
            .lock()
            .expect("snippet fixture mutex should not be poisoned");
        Ok(versioned.definition.id == snippet_id)
    }

    /// 批量导入不属于本测试，意外调用时明确失败。
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        unsupported()
    }

    /// 批次状态不属于本测试，意外调用时明确失败。
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        unsupported()
    }

    /// 精确读取用户定义，不通过有限列表回退或猜测来源。
    async fn get_user(&self, snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        let versioned = self
            .versioned
            .lock()
            .expect("snippet fixture mutex should not be poisoned");
        Ok((versioned.definition.id == snippet_id).then(|| versioned.definition.clone()))
    }

    /// 本用例不保存编辑器内容。
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        unsupported()
    }

    /// 本用例不删除目录项。
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        unsupported()
    }

    /// 返回当前内容与其 revision，供 execution plan 前后做完整快照比较。
    async fn get_user_versioned(
        &self,
        snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        let versioned = self
            .versioned
            .lock()
            .expect("snippet fixture mutex should not be poisoned")
            .clone();
        Ok((versioned.definition.id == snippet_id).then_some(versioned))
    }

    /// 版本化编辑不属于本测试。
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        unsupported()
    }

    /// 版本化删除不属于本测试。
    async fn delete_user_versioned(
        &self,
        _snippet_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unsupported()
    }
}

struct MemoryHistory {
    drafts: Mutex<Vec<CommandHistoryDraft>>,
    fail_append: bool,
}

impl MemoryHistory {
    /// 独立观察 CommandsService 的历史过滤结果，不写 SQLite 或用户数据。
    fn new(fail_append: bool) -> Self {
        Self {
            drafts: Mutex::new(Vec::new()),
            fail_append,
        }
    }

    /// 返回 adapter 收到的安全 draft，确保敏感内容未被提交。
    fn drafts(&self) -> Vec<CommandHistoryDraft> {
        self.drafts
            .lock()
            .expect("history fixture mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl CommandHistoryRepository for MemoryHistory {
    /// 隔离历史仓储可用，真实应用层仍执行命令规范化和敏感过滤。
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 查询历史不属于当前运行用例。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        Ok(Vec::new())
    }

    /// 捕获完整安全记录请求，或模拟存储失败而不影响已完成命令事实。
    async fn append(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        if self.fail_append {
            return Err(AppError::infrastructure("history fixture unavailable"));
        }
        let mut drafts = self
            .drafts
            .lock()
            .expect("history fixture mutex should not be poisoned");
        drafts.push(draft.clone());
        Ok(CommandHistoryEntry {
            id: format!("history-{}", drafts.len()),
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
            created_at: "2026-10-17T00:00:00Z".to_owned(),
        })
    }

    /// 历史删除不属于当前执行用例。
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        Ok(false)
    }

    /// 历史清理不属于当前执行用例。
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        Ok(0)
    }
}

struct MemorySnippetPreferences {
    receipts: Mutex<Vec<SnippetUsageRecordRequest>>,
    fail_record: bool,
}

impl MemorySnippetPreferences {
    /// 记录执行用量独立于命令历史，允许二者分别成功或失败。
    fn new(fail_record: bool) -> Self {
        Self {
            receipts: Mutex::new(Vec::new()),
            fail_record,
        }
    }

    /// 返回片段使用仓储收到的稳定 receipt 请求。
    fn receipts(&self) -> Vec<SnippetUsageRecordRequest> {
        self.receipts
            .lock()
            .expect("usage fixture mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl SnippetPreferenceRepository for MemorySnippetPreferences {
    /// 测试中的偏好仓储固定可用，写入结果仍由真实应用用例请求。
    fn availability(&self) -> SnippetPreferenceAvailability {
        SnippetPreferenceAvailability::Available
    }

    /// 目录偏好批量读取不属于当前执行用例。
    async fn list(&self, _keys: Vec<SnippetPreferenceKey>) -> AppResult<Vec<SnippetPreference>> {
        Ok(Vec::new())
    }

    /// 收藏变更不属于当前执行用例。
    async fn set_favorite(
        &self,
        _key: SnippetPreferenceKey,
        _favorite: bool,
    ) -> AppResult<SnippetPreference> {
        unsupported()
    }

    /// 记录稳定使用回执，或模拟偏好库故障但不反转 shell 完成结果。
    async fn record_usage(
        &self,
        request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt> {
        if self.fail_record {
            return Err(AppError::infrastructure("usage fixture unavailable"));
        }
        self.receipts
            .lock()
            .expect("usage fixture mutex should not be poisoned")
            .push(request.clone());
        Ok(SnippetUsageReceipt {
            receipt_id: request.receipt_id,
            key: request.key,
            action: request.action,
            occurred_at_unix_ms: 1,
        })
    }

    /// 清理用量历史不属于当前执行用例。
    async fn clear_usage(&self) -> AppResult<SnippetUsageClearResult> {
        Ok(SnippetUsageClearResult::default())
    }
}

struct UnusedWorkflows;

#[async_trait]
impl WorkflowRepository for UnusedWorkflows {
    /// 当前测试不列出工作流。
    async fn list(&self, _request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        unsupported()
    }

    /// 当前测试不读取工作流。
    async fn get(&self, _workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        unsupported()
    }

    /// 当前测试不保存工作流。
    async fn save(&self, _workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        unsupported()
    }

    /// 当前测试不删除工作流。
    async fn delete(&self, _workflow_id: String) -> AppResult<bool> {
        unsupported()
    }

    /// 当前测试不查询版本化工作流。
    async fn get_versioned(
        &self,
        _workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        unsupported()
    }

    /// 当前测试不保存版本化工作流。
    async fn save_versioned(
        &self,
        _workflow: CommandWorkflow,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        unsupported()
    }

    /// 当前测试不删除版本化工作流。
    async fn delete_versioned(
        &self,
        _workflow_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unsupported()
    }
}

struct NoSuggestions;

#[async_trait]
impl CommandSuggestionProvider for NoSuggestions {
    /// 该假服务不声明任何建议来源。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        Vec::new()
    }

    /// 当前测试不生成命令建议。
    async fn suggest(
        &self,
        _request: CommandSuggestionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        Ok(Vec::new())
    }
}

struct World {
    service: SnippetExecutionService,
    commands: Arc<CommandsService>,
    snippets: Arc<MemorySnippets>,
    sessions: Arc<MemorySessions>,
    history: Arc<MemoryHistory>,
    preferences: Arc<MemorySnippetPreferences>,
    key: SnippetPreferenceKey,
    binding: UiSessionBinding,
}

/// 装配完全隔离的 Commands 与 SessionService，所有调用都停留在内存 fake 端口。
fn world(
    definition: SnippetDefinition,
    mode: ExecutionMode,
    fail_history: bool,
    fail_usage: bool,
) -> World {
    world_with_origin(
        definition,
        mode,
        fail_history,
        fail_usage,
        SnippetCatalogOrigin::User,
        None,
    )
}

/// 装配指定来源的片段 fixture；内置适用性声明与用户可编辑定义保持不同读取路径。
fn world_with_origin(
    definition: SnippetDefinition,
    mode: ExecutionMode,
    fail_history: bool,
    fail_usage: bool,
    origin: SnippetCatalogOrigin,
    catalog_item: Option<SnippetCatalogItem>,
) -> World {
    let session_id = SessionId::parse(SESSION_TEXT).expect("fixture session ID should be valid");
    let owner = SessionOwner::Ui {
        tab_id: TabId::parse("fixture-tab").expect("fixture tab ID should be valid"),
        pane_id: PaneId::parse("fixture-pane").expect("fixture pane ID should be valid"),
    };
    let target = SessionTarget::Local(LocalSessionTarget {
        program: OsString::from("bash"),
        args: Vec::new(),
        working_directory: Some(PathBuf::from("C:/fixture/workspace")),
        environment: BTreeMap::new(),
    });
    let info = SessionInfo {
        session_id: session_id.clone(),
        target,
        owner: owner.clone(),
        size: TerminalSize::default(),
        lifecycle: SessionLifecycle::Running,
        exit_status: None,
    };
    let context = TerminalCommandContext {
        session_id: session_id.clone(),
        owner: owner.clone(),
        session_generation: 7,
        prompt_epoch: 3,
        input_generation: 0,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    };
    let sessions = Arc::new(MemorySessions::new(info, context, mode));
    let snippets = Arc::new(MemorySnippets::new(definition));
    if let Some(catalog_item) = catalog_item {
        snippets.replace_catalog_item(catalog_item);
    }
    let history = Arc::new(MemoryHistory::new(fail_history));
    let preferences = Arc::new(MemorySnippetPreferences::new(fail_usage));
    let commands = Arc::new(
        CommandsService::new(
            history.clone(),
            snippets.clone(),
            Arc::new(UnusedWorkflows),
            Arc::new(NoSuggestions),
            sessions.clone(),
        )
        .with_snippet_preference_repository(preferences.clone()),
    );
    World {
        service: SnippetExecutionService::new(commands.clone(), sessions.clone()),
        commands,
        snippets,
        sessions,
        history,
        preferences,
        key: SnippetPreferenceKey {
            origin,
            snippet_id: SNIPPET_ID.to_owned(),
        },
        binding: UiSessionBinding {
            session_id,
            owner,
            session_generation: 7,
        },
    }
}

/// 构造内置目录投影，使适用性测试走真实 execution snapshot 和 prepare/commit 用例。
fn builtin_world(
    definition: SnippetDefinition,
    mode: ExecutionMode,
    platforms: &[&str],
    shells: &[&str],
) -> World {
    let item = catalog_item_for_definition(
        &definition,
        SnippetCatalogOrigin::Builtin,
        platforms,
        shells,
    );
    world_with_origin(
        definition,
        mode,
        false,
        false,
        SnippetCatalogOrigin::Builtin,
        Some(item),
    )
}

/// 只填充执行和适用性需要的目录字段，其余展示元数据使用确定空值。
fn catalog_item_for_definition(
    definition: &SnippetDefinition,
    origin: SnippetCatalogOrigin,
    platforms: &[&str],
    shells: &[&str],
) -> SnippetCatalogItem {
    SnippetCatalogItem {
        id: definition.id.clone(),
        origin,
        title: definition.title.clone(),
        description: definition.description.clone().unwrap_or_default(),
        template: definition.command.clone(),
        category: definition.category.clone().unwrap_or_default(),
        pack: "fixture".to_owned(),
        tags: definition.tags.clone(),
        scope: definition.scope,
        platforms: platforms.iter().map(|value| (*value).to_owned()).collect(),
        shells: shells.iter().map(|value| (*value).to_owned()).collect(),
        capabilities: Vec::new(),
        risk: definition.risk,
        sensitive: false,
        duration: String::new(),
        default_action: definition.default_action,
        variables: definition.variables.clone(),
        context_bindings: definition.context_bindings.clone(),
        derived_from: definition.derived_from.clone(),
        catalog_version: Some("fixture-catalog-v1".to_owned()),
        command_spec: None,
        owner: None,
        tested_version: None,
        source_name: None,
        source_url: None,
        deprecated: false,
        favorite: false,
        use_count: 0,
        last_used_at_unix_ms: None,
        sort_order: definition.sort_order,
        updated_at: String::new(),
    }
}

/// 为测试构造默认只读定义；风险和敏感变量可由单项用例覆盖。
fn definition(command: &str, risk: SnippetRisk) -> SnippetDefinition {
    SnippetDefinition {
        id: SNIPPET_ID.to_owned(),
        title: "fixture snippet".to_owned(),
        command: command.to_owned(),
        description: None,
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: None,
        risk,
        default_action: SnippetDefaultAction::Run,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    }
}

/// 开始真实应用准备流程并使用明确的 session binding，不调用 UI 当前 Tab 全局状态。
async fn prepare(
    world: &World,
    record_history: bool,
    variables: BTreeMap<String, String>,
) -> crate::commands::SnippetRunPlan {
    world
        .service
        .prepare_run(SnippetRunPreparation {
            key: world.key.clone(),
            binding: world.binding.clone(),
            variables,
            record_history,
        })
        .await
        .expect("fixture definition and binding should produce a plan")
}

/// 仅对应用标记需要确认的计划生成其专属 token，避免测试漏掉真实审批门禁。
fn confirmation(
    plan: &crate::commands::SnippetRunPlan,
) -> Option<crate::commands::SnippetRunConfirmation> {
    plan.confirmation()
        .requires_confirmation
        .then(|| plan.confirm())
}

/// 统一按 fixture 的当前精确定义与 revision 准备 Fill，避免用例把运行目标留给全局状态推断。
/// @author kongweiguang
async fn prepare_fill(
    world: &World,
    expected_definition: SnippetDefinition,
    expected_revision: Option<ConfigRevision>,
) -> SnippetFillPlan {
    world
        .service
        .prepare_fill(
            SnippetRunPreparation {
                key: world.key.clone(),
                binding: world.binding.clone(),
                variables: BTreeMap::new(),
                record_history: true,
            },
            expected_definition,
            expected_revision,
        )
        .await
        .expect("fixture definition and binding should produce a fill plan")
}

/// Fill 必须发送一个带原始 binding/context 的 Paste，既不提交命令也不触碰执行记录。
/// @author kongweiguang
#[tokio::test]
async fn fill_writes_exact_ui_paste_without_enter_history_or_usage() {
    let definition = definition("echo ready", SnippetRisk::Inspect);
    let world = world(definition.clone(), ExecutionMode::Finished, false, false);
    let prepared_context = world
        .sessions
        .context
        .lock()
        .expect("session fixture mutex should not be poisoned")
        .version();
    let plan = prepare_fill(
        &world,
        definition,
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;

    world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect("current single-line plan should populate the visible terminal draft");

    let requests = world.sessions.ui_input_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(world.sessions.ui_input_call_count(), 1);
    assert_eq!(world.sessions.call_count(), 0);
    assert_eq!(requests[0].binding, world.binding);
    assert_eq!(requests[0].expected_context, Some(prepared_context));
    assert_eq!(requests[0].projection, UiInputProjection::Tracked);
    assert_eq!(
        requests[0].event,
        TerminalInputEvent::Paste("echo ready".to_owned())
    );
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// 普通填入在无 Shell 集成终端仍只发送 Paste；首尾空白按 main 去除，行内 Tab 保留。
/// @author kongweiguang
#[tokio::test]
async fn direct_fill_supports_unobserved_shell_without_enter_or_history() {
    let world = world(
        definition("echo ready", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    {
        let mut context = world.sessions.context.lock().unwrap();
        context.shell_dialect = RemoteShellDialect::Unknown;
        context.availability = TerminalCommandAvailability::Unavailable(
            TerminalCommandUnavailableReason::ShellIntegrationNotObserved,
        );
    }
    world
        .service
        .fill_direct(
            SnippetRenderResult {
                command: " \n\tping\t-c 4 127.0.0.1\r\n ".to_owned(),
                sensitive: false,
            },
            world.binding.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let requests = world.sessions.ui_input_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].binding, world.binding);
    assert_eq!(requests[0].expected_context, None);
    assert_eq!(
        requests[0].event,
        TerminalInputEvent::Paste("ping\t-c 4 127.0.0.1".to_owned())
    );
    assert_eq!(world.sessions.call_count(), 0);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// 无效单行、取消和 owner 变化都在 writer 前闭合，不能因普通填入不探测提示符而放松目标边界。
/// @author kongweiguang
#[tokio::test]
async fn direct_fill_rejects_invalid_cancelled_and_stale_targets_before_write() {
    let world = world(
        definition("echo ready", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    for command in ["", "echo first\necho second", "echo first\recho second"] {
        assert!(
            world
                .service
                .fill_direct(
                    SnippetRenderResult {
                        command: command.to_owned(),
                        sensitive: false
                    },
                    world.binding.clone(),
                    CancellationToken::new()
                )
                .await
                .is_err()
        );
    }
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        world
            .service
            .fill_direct(
                SnippetRenderResult {
                    command: "echo ready".to_owned(),
                    sensitive: false
                },
                world.binding.clone(),
                cancellation
            )
            .await
            .is_err()
    );
    let mut stale = world.binding.clone();
    stale.owner = SessionOwner::Ui {
        tab_id: TabId::parse("other-tab").unwrap(),
        pane_id: PaneId::parse("other-pane").unwrap(),
    };
    assert!(
        world
            .service
            .fill_direct(
                SnippetRenderResult {
                    command: "echo ready".to_owned(),
                    sensitive: false
                },
                stale,
                CancellationToken::new()
            )
            .await
            .is_err()
    );
    assert_eq!(world.sessions.ui_input_call_count(), 0);
}

/// writer 回执未知时只报告原结果，不能因为缺少 Shell 集成而自动重放 Paste。
/// @author kongweiguang
#[tokio::test]
async fn direct_fill_unknown_receipt_is_not_replayed() {
    let world = world(
        definition("echo ready", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    world
        .sessions
        .set_ui_input_mode(UiInputMode::OutcomeUnknown);
    let result = world
        .service
        .fill_direct(
            SnippetRenderResult {
                command: "echo ready".to_owned(),
                sensitive: false,
            },
            world.binding.clone(),
            CancellationToken::new(),
        )
        .await;
    assert_eq!(
        result.unwrap_err().public_code(),
        PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(world.sessions.ui_input_call_count(), 1);
}

/// Fill 仅接受安全的单行输入；换行和其余控制字符必须在任何 writer 调用前引导用户走 Run。
/// @author kongweiguang
#[tokio::test]
async fn fill_rejects_multiline_and_control_character_templates() {
    for command in ["echo first\necho second", "echo \u{1b}danger"] {
        let definition = definition(command, SnippetRisk::Inspect);
        let world = world(definition.clone(), ExecutionMode::Finished, false, false);
        let error = world
            .service
            .prepare_fill(
                SnippetRunPreparation {
                    key: world.key.clone(),
                    binding: world.binding.clone(),
                    variables: BTreeMap::new(),
                    record_history: true,
                },
                definition,
                Some(ConfigRevision::from_sha256("revision-1".to_owned())),
            )
            .await
            .expect_err("non-single-line content must use the explicit Run flow");

        assert_eq!(error.public_code(), PublicErrorCode::Invalid);
        assert!(error.public_message().contains("运行预览"));
        assert_eq!(world.sessions.ui_input_call_count(), 0);
    }
}

/// Prepare 的定义正文或 revision 与已展示快照不符时，必须要求重选，不得先生成可用 Fill 计划。
/// @author kongweiguang
#[tokio::test]
async fn fill_rejects_stale_expected_definition_and_revision() {
    let definition = definition("echo current", SnippetRisk::Inspect);
    let world = world(definition.clone(), ExecutionMode::Finished, false, false);
    let mut stale_definition = definition.clone();
    stale_definition.command = "echo stale".to_owned();

    let definition_error = world
        .service
        .prepare_fill(
            SnippetRunPreparation {
                key: world.key.clone(),
                binding: world.binding.clone(),
                variables: BTreeMap::new(),
                record_history: true,
            },
            stale_definition,
            Some(ConfigRevision::from_sha256("revision-1".to_owned())),
        )
        .await
        .expect_err("the selected definition must match the loaded catalog snapshot");
    assert_eq!(definition_error.public_code(), PublicErrorCode::Conflict);

    let revision_error = world
        .service
        .prepare_fill(
            SnippetRunPreparation {
                key: world.key.clone(),
                binding: world.binding.clone(),
                variables: BTreeMap::new(),
                record_history: true,
            },
            definition,
            Some(ConfigRevision::from_sha256("revision-2".to_owned())),
        )
        .await
        .expect_err("the selected revision must match the loaded catalog snapshot");
    assert_eq!(revision_error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(world.sessions.ui_input_call_count(), 0);
}

/// Fill 在等待后重新核对目录正文与 revision，旧计划不能写入被替换的片段。
/// @author kongweiguang
#[tokio::test]
async fn fill_rejects_catalog_change_after_preparation() {
    let current_definition = definition("echo before", SnippetRisk::Inspect);
    let world = world(
        current_definition.clone(),
        ExecutionMode::Finished,
        false,
        false,
    );
    let plan = prepare_fill(
        &world,
        current_definition,
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    world
        .snippets
        .replace(definition("echo after", SnippetRisk::Inspect), "revision-2");

    let error = world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect_err("catalog mutation must invalidate the fill plan");

    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(world.sessions.ui_input_call_count(), 0);
}

/// 目标、session generation 或 prompt/input version 改变都会使准备时的输入计划失效。
/// @author kongweiguang
#[tokio::test]
async fn fill_rejects_target_generation_and_context_changes() {
    let base = definition("echo stable", SnippetRisk::Inspect);

    let target_world = world(base.clone(), ExecutionMode::Finished, false, false);
    let plan = prepare_fill(
        &target_world,
        base.clone(),
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    target_world
        .sessions
        .set_target(SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("pwsh"),
            args: Vec::new(),
            working_directory: Some(PathBuf::from("C:/fixture/other")),
            environment: BTreeMap::new(),
        }));
    let error = target_world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect_err("the fill must remain bound to the prepared session target");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(target_world.sessions.ui_input_call_count(), 0);

    let generation_world = world(base.clone(), ExecutionMode::Finished, false, false);
    let plan = prepare_fill(
        &generation_world,
        base.clone(),
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    generation_world
        .sessions
        .context
        .lock()
        .expect("session fixture mutex should not be poisoned")
        .session_generation = 8;
    let error = generation_world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect_err("a reconnect must not inherit a previous fill plan");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(generation_world.sessions.ui_input_call_count(), 0);

    let context_world = world(base.clone(), ExecutionMode::Finished, false, false);
    let plan = prepare_fill(
        &context_world,
        base,
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    context_world
        .sessions
        .context
        .lock()
        .expect("session fixture mutex should not be poisoned")
        .prompt_epoch += 1;
    let error = context_world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect_err("input context changes must invalidate the prepared paste");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(context_world.sessions.ui_input_call_count(), 0);
}

/// 在任何 writer 调用前观察到取消时立即停止，避免取消与用户再次点击之间形成双重粘贴。
/// @author kongweiguang
#[tokio::test]
async fn fill_cancelled_before_write_does_not_touch_terminal() {
    let definition = definition("echo cancelled", SnippetRisk::Inspect);
    let world = world(definition.clone(), ExecutionMode::Finished, false, false);
    let plan = prepare_fill(
        &world,
        definition,
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = world
        .service
        .fill(plan, cancellation)
        .await
        .expect_err("pre-cancelled fill must not write into the terminal");

    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(world.sessions.ui_input_call_count(), 0);
    assert!(world.sessions.ui_input_requests().is_empty());
}

/// OutcomeUnknown 表示 Paste 是否到达 shell 不可确认；保持一次请求且不自动重放或记录执行。
/// @author kongweiguang
#[tokio::test]
async fn fill_unknown_writer_result_is_not_replayed_or_recorded() {
    let definition = definition("echo uncertain paste", SnippetRisk::Inspect);
    let world = world(definition.clone(), ExecutionMode::Finished, false, false);
    let plan = prepare_fill(
        &world,
        definition,
        Some(ConfigRevision::from_sha256("revision-1".to_owned())),
    )
    .await;
    world
        .sessions
        .set_ui_input_mode(UiInputMode::OutcomeUnknown);

    let error = world
        .service
        .fill(plan, CancellationToken::new())
        .await
        .expect_err("unknown writer result must be surfaced without replay");

    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(world.sessions.ui_input_call_count(), 1);
    assert_eq!(world.sessions.ui_input_requests().len(), 1);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// 详情评估保留内置声明；没有精确目标或 Bash/Zsh 名称时只返回 Unknown。
#[tokio::test]
async fn applicability_projection_preserves_constraints_and_unknown_reasons() {
    let world = builtin_world(
        definition("pwd", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        &["linux"],
        &["bash", "zsh"],
    );
    let projection = world
        .commands
        .snippet_applicability(world.key.clone(), Some(world.binding.clone()))
        .await
        .expect("valid query should return applicability")
        .expect("fixture builtin should exist");
    assert_eq!(projection.platforms, vec!["linux".to_owned()]);
    assert_eq!(projection.shells, vec!["bash".to_owned(), "zsh".to_owned()]);
    assert_eq!(
        projection.assessment.status,
        SnippetApplicabilityStatus::Unknown
    );
    assert_eq!(
        projection.assessment.reason,
        Some(SnippetApplicabilityReason::ExactShellNotReported)
    );

    let no_target = world
        .commands
        .snippet_applicability(world.key.clone(), None)
        .await
        .expect("missing target is an assessment, not a query failure")
        .expect("fixture builtin should exist");
    assert_eq!(
        no_target.assessment.status,
        SnippetApplicabilityStatus::Unknown
    );
    assert_eq!(
        no_target.assessment.reason,
        Some(SnippetApplicabilityReason::TargetNotAvailable)
    );
}

/// 用户定义即使来自内置片段的另存副本，也不自动继承只读目录的执行范围。
#[tokio::test]
async fn user_snippet_applicability_does_not_inherit_builtin_declarations() {
    let mut definition = definition("pwd", SnippetRisk::Inspect);
    definition.derived_from = Some("snippet.builtin.current-directory".to_owned());
    let world = world(definition, ExecutionMode::Finished, false, false);
    let projection = world
        .commands
        .snippet_applicability(world.key.clone(), Some(world.binding.clone()))
        .await
        .expect("valid query should return applicability")
        .expect("fixture user item should exist");

    assert!(projection.platforms.is_empty());
    assert!(projection.shells.is_empty());
    assert_eq!(
        projection.assessment.status,
        SnippetApplicabilityStatus::Unrestricted
    );
    assert_eq!(projection.assessment.reason, None);
}

/// 即使 live dialect 与声明一致，未报告的远端平台也不能被 UI 标成兼容。
#[tokio::test]
async fn applicability_projection_keeps_platform_unreported() {
    let world = builtin_world(
        definition("Get-Location", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        &["windows"],
        &["power_shell"],
    );
    world
        .sessions
        .set_shell_dialect(RemoteShellDialect::PowerShell);

    let projection = world
        .commands
        .snippet_applicability(world.key.clone(), Some(world.binding.clone()))
        .await
        .expect("valid query should return applicability")
        .expect("fixture builtin should exist");

    assert_eq!(
        projection.assessment.status,
        SnippetApplicabilityStatus::Unknown
    );
    assert_eq!(
        projection.assessment.reason,
        Some(SnippetApplicabilityReason::PlatformNotReported)
    );
}

/// 只有可信方言能证明 shell 家族冲突时才拒绝 prepare，且终端 writer 不会收到命令。
#[tokio::test]
async fn builtin_shell_family_mismatch_is_rejected_before_execution() {
    let world = builtin_world(
        definition("Get-Location", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        &[],
        &["powershell"],
    );

    let error = world
        .service
        .prepare_run(SnippetRunPreparation {
            key: world.key.clone(),
            binding: world.binding.clone(),
            variables: BTreeMap::new(),
            record_history: false,
        })
        .await
        .expect_err("POSIX and PowerShell are provably incompatible shell families");

    assert_eq!(error.public_code(), PublicErrorCode::CapabilityNotSupported);
    assert_eq!(world.sessions.call_count(), 0);
}

/// 投影失效不能伪称会话代次变化，也不能在准备阶段触发终端写入。
///
/// @author kongweiguang
#[tokio::test]
async fn invalidated_prompt_projection_reports_safe_recovery_without_running() {
    let world = world(
        definition("pwd && ls -la", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    world
        .sessions
        .context
        .lock()
        .expect("session fixture mutex should not be poisoned")
        .availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::UiInputProjectionInvalidated,
    );

    let error = world
        .service
        .prepare_run(SnippetRunPreparation {
            key: world.key.clone(),
            binding: world.binding.clone(),
            variables: BTreeMap::new(),
            record_history: false,
        })
        .await
        .expect_err("an untrusted terminal input projection must stop run preparation");

    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        error.public_message(),
        "终端输入状态暂不可核对，请回到新的命令提示符后重试"
    );
    assert_eq!(world.sessions.call_count(), 0);
    assert_eq!(world.sessions.ui_input_call_count(), 0);
}

/// POSIX 方言无法区分 Bash/Zsh，因此 Unknown 仍沿用显式预览和运行流程。
#[tokio::test]
async fn ambiguous_posix_builtin_remains_explicitly_runnable() {
    let world = builtin_world(
        definition("pwd", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        &[],
        &["bash", "zsh"],
    );
    let plan = prepare(&world, false, BTreeMap::new()).await;
    let result = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect("an unproved exact shell is not treated as a definite mismatch");

    assert!(matches!(
        result,
        SnippetRunResult::Finished { exit_code: 0, .. }
    ));
    assert_eq!(world.sessions.call_count(), 1);
}

/// 目录声明改变会使旧 plan 冲突，即使模板正文和目录版本字符串保持不变。
#[tokio::test]
async fn changed_builtin_applicability_declarations_invalidate_prepared_plan() {
    let world = builtin_world(
        definition("pwd", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        &[],
        &["bash", "zsh"],
    );
    let plan = prepare(&world, false, BTreeMap::new()).await;
    let changed = catalog_item_for_definition(
        &definition("pwd", SnippetRisk::Inspect),
        SnippetCatalogOrigin::Builtin,
        &[],
        &["fish"],
    );
    world.snippets.replace_catalog_item(changed);

    let error = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect_err("declaration changes must invalidate the frozen catalog snapshot");

    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(world.sessions.call_count(), 0);
}

/// 目录变更后的旧计划在调用 writer 前冲突，不能用旧命令或当前内容自动替代。
#[tokio::test]
async fn changed_definition_after_preview_is_rejected_before_execution() {
    let world = world(
        definition("echo before", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;
    world
        .snippets
        .replace(definition("echo after", SnippetRisk::Inspect), "revision-2");

    let error = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect_err("a plan must not run after the source file changes");

    assert_eq!(error.public_code(), crate::PublicErrorCode::Conflict);
    assert_eq!(world.sessions.call_count(), 0);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// Finished 命令将用量与安全历史分别回执，且确认页显示准备时锁定的 session owner。
/// Arc 只改变结果共享方式，不改变持久化语义。
/// @author kongweiguang
#[tokio::test]
async fn finished_run_records_snippet_usage_and_command_history() {
    let world = world(
        definition("echo ready", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;
    assert_eq!(plan.confirmation().session_id.as_str(), SESSION_TEXT);
    assert_eq!(plan.confirmation().owner, world.binding.owner);
    assert_eq!(plan.confirmation().command_preview, "echo ready");

    let result = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect("matching Finished outcome should remain successful");

    let SnippetRunResult::Finished {
        command_id,
        exit_code,
        usage,
        history,
    } = result
    else {
        panic!("test terminal should return Finished")
    };
    assert_eq!(exit_code, 0);
    assert!(!command_id.is_empty());
    assert!(matches!(usage, SnippetRunUsage::Recorded(_)));
    assert!(matches!(
        history,
        SnippetRunHistory::Evaluated(result) if result.recorded
    ));
    assert_eq!(world.sessions.call_count(), 1);
    assert_eq!(world.preferences.receipts().len(), 1);
    let drafts = world.history.drafts();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].source, CommandHistorySource::Snippet);
    assert_eq!(drafts[0].target, CommandHistoryTarget::Local);
    assert_eq!(
        drafts[0].session_id.as_ref(),
        Some(&world.binding.session_id)
    );
    assert_eq!(
        drafts[0].pane_id.as_ref(),
        Some(match &world.binding.owner {
            SessionOwner::Ui { pane_id, .. } => pane_id,
            _ => unreachable!(),
        })
    );
    assert_eq!(
        world.sessions.requests()[0].expected_ui_binding.as_ref(),
        Some(&world.binding)
    );
}

/// 敏感值只进入执行计划与 terminal request；预览、Debug、历史仓储均不接收展开值。
#[tokio::test]
async fn sensitive_run_requires_confirmation_and_never_persists_expanded_command() {
    let mut definition = definition("echo {{secret}}", SnippetRisk::Inspect);
    definition.variables.push(SnippetVariable {
        name: "secret".to_owned(),
        label: "secret".to_owned(),
        description: String::new(),
        kind: SnippetVariableKind::Secret,
        required: true,
        default_value: None,
        suggestions: Vec::new(),
        validation: None,
        render_strategy: SnippetVariableRenderStrategy::ShellArg,
        sensitive: true,
    });
    let world = world(definition, ExecutionMode::Finished, false, false);
    let secret = "never-store-this-value";
    let plan = prepare(
        &world,
        true,
        BTreeMap::from([("secret".to_owned(), secret.to_owned())]),
    )
    .await;
    assert!(plan.confirmation().requires_confirmation);
    assert_eq!(plan.confirmation().command_preview, "echo {{secret}}");
    assert!(!format!("{plan:?}").contains(secret));
    assert!(!format!("{:?}", plan.confirmation()).contains(secret));
    let confirmation = confirmation(&plan);

    let result = world
        .service
        .run(plan, confirmation, CancellationToken::new())
        .await
        .expect("explicitly confirmed sensitive command should execute");

    assert!(matches!(
        result,
        SnippetRunResult::Finished {
            history: SnippetRunHistory::SkippedSensitive,
            ..
        }
    ));
    assert!(world.history.drafts().is_empty());
    assert!(world.sessions.requests()[0].command.contains(secret));
    assert_eq!(world.preferences.receipts().len(), 1);
}

/// 变更风险必须使用同一 plan 的确认令牌，缺少确认时不会调用终端或写入统计。
#[tokio::test]
async fn change_risk_requires_plan_bound_confirmation() {
    let world = world(
        definition("touch fixture", SnippetRisk::Change),
        ExecutionMode::Finished,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;
    assert!(plan.confirmation().requires_confirmation);

    let error = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect_err("change risk is never submitted without confirmation");

    assert_eq!(error.public_code(), crate::PublicErrorCode::Conflict);
    assert_eq!(world.sessions.call_count(), 0);
    assert!(world.preferences.receipts().is_empty());
}

/// 终端 Unknown 保留原请求身份且不生成成功历史或统计，也绝不自动重放命令。
#[tokio::test]
async fn unknown_result_is_not_recorded_or_retried() {
    let world = world(
        definition("echo uncertain", SnippetRisk::Inspect),
        ExecutionMode::Unknown,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;

    let result = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect("a typed unknown outcome must be surfaced without retry");

    assert!(matches!(result, SnippetRunResult::Unknown { .. }));
    assert_eq!(world.sessions.call_count(), 1);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// 终端回执串错 request ID 时转为 outcome_unknown，禁止把另一条命令记到本片段名下。
#[tokio::test]
async fn mismatched_terminal_receipt_is_unknown_and_never_attributed() {
    let world = world(
        definition("echo mismatch", SnippetRisk::Inspect),
        ExecutionMode::MismatchedFinished,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;

    let error = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect_err("a foreign completion receipt cannot confirm this request");

    assert_eq!(error.public_code(), crate::PublicErrorCode::OutcomeUnknown);
    assert_eq!(world.sessions.call_count(), 1);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// Unknown 回执也必须匹配本次 command ID，避免把外部命令的不确定结果归属给片段或自动重试。
///
/// @author kongweiguang
#[tokio::test]
async fn mismatched_unknown_terminal_receipt_is_not_retried_or_attributed() {
    let world = world(
        definition("echo mismatched unknown", SnippetRisk::Inspect),
        ExecutionMode::MismatchedUnknown,
        false,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;

    let error = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect_err("a foreign unknown receipt cannot identify this snippet request");

    assert_eq!(error.public_code(), crate::PublicErrorCode::OutcomeUnknown);
    assert_eq!(world.sessions.call_count(), 1);
    assert!(world.history.drafts().is_empty());
    assert!(world.preferences.receipts().is_empty());
}

/// 用量库失败单独返回，不能把已经 Finished 的 shell 命令改报执行失败。
#[tokio::test]
async fn usage_storage_failure_does_not_reverse_finished_execution() {
    let world = world(
        definition("echo usage", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        false,
        true,
    );
    let plan = prepare(&world, false, BTreeMap::new()).await;

    let result = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect("storage failure is a separate result from shell execution");

    assert!(matches!(
        result,
        SnippetRunResult::Finished {
            usage: SnippetRunUsage::NotRecorded {
                error_code: crate::PublicErrorCode::Infrastructure
            },
            history: SnippetRunHistory::NotRequested,
            ..
        }
    ));
    assert_eq!(world.sessions.call_count(), 1);
    assert!(world.history.drafts().is_empty());
}

/// 历史存储故障只改变历史回执，usage 与 Finished 终态仍分别保留。
#[tokio::test]
async fn history_storage_failure_is_reported_separately() {
    let world = world(
        definition("echo history", SnippetRisk::Inspect),
        ExecutionMode::Finished,
        true,
        false,
    );
    let plan = prepare(&world, true, BTreeMap::new()).await;

    let result = world
        .service
        .run(plan, None, CancellationToken::new())
        .await
        .expect("history adapter failure must not reverse a finished command");

    assert!(matches!(
        result,
        SnippetRunResult::Finished {
            usage: SnippetRunUsage::Recorded(_),
            history: SnippetRunHistory::Failed {
                error_code: crate::PublicErrorCode::Infrastructure
            },
            ..
        }
    ));
    assert_eq!(world.sessions.call_count(), 1);
    assert_eq!(world.preferences.receipts().len(), 1);
}

/// 用稳定的 unsupported 错误让未覆盖的测试端口失败闭合，不伪造应用操作成功。
fn unsupported<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "unused snippet execution fixture port",
    ))
}

#[path = "workflow_execution.rs"]
mod workflow_service_tests;
