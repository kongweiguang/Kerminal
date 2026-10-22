//! UI 与 MCP 终端提交测试调用真实 CommandsService 用例。
//!
//! @author kongweiguang

use super::*;
use crate::{
    AppError, AppResult, PublicErrorCode,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandSuggestionProvider, SnippetImportBatchRequest,
        SnippetImportBatchState, SnippetRepository, VersionedCommandDefinition, WorkflowRepository,
    },
    operations::{
        CurrentTerminalProxyRequest, FutureProxyPolicyStore, PortForwardRuntime,
        PortForwardRuntimeHandle, PortForwardService, TerminalProxyApplication,
        TerminalProxyApplyOutcome, TerminalProxyReconcileOutcome, TerminalProxyUnavailableReason,
    },
    sessions::{
        CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot,
        RenderSnapshotRequest, SessionFilter, SessionRuntimeMetadata, SessionService,
        SessionSubscription, ShellCommandOutcome, ShellCommandRequest, TerminalCommandContext,
        TerminalSearchRequest, TerminalSearchResult, TerminalSelection, TerminalWriteReceipt,
        UiCommandLineRequest, UiSessionBinding, UiTerminalCommandSubmissionReceipt,
        UiTerminalCommandSubmissionRequest,
    },
};
use async_trait::async_trait;
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandSuggestionCandidate,
        CommandSuggestionRequest, CommandWorkflow, SnippetCatalogItem, SnippetCatalogListRequest,
        SnippetCatalogOrigin, SnippetDefinition, SuggestionProviderKind, WorkflowListRequest,
    },
    identity::{HostId, SessionId},
    operations::{
        ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyProtocol, RemoteAccessScope,
    },
    preferences::ConfigRevision,
    sessions::{SessionInfo, SessionTarget, SshSessionTarget, TerminalSize},
};
use std::collections::VecDeque;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio_util::sync::CancellationToken;

/// 用稳定内存仓储观察真实 CommandsService 是否提交了历史草稿。
#[derive(Default)]
struct CapturingHistoryRepository {
    drafts: Mutex<Vec<CommandHistoryDraft>>,
    append_error: Mutex<Option<AppError>>,
}

impl CapturingHistoryRepository {
    /// 返回测试期间捕获的历史 draft 副本，避免暴露内部锁。
    fn drafts(&self) -> Vec<CommandHistoryDraft> {
        self.drafts
            .lock()
            .expect("history test mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl CommandHistoryRepository for CapturingHistoryRepository {
    /// 测试仓储始终可用，避免把 SQLite 启动状态混入提交判定。
    fn availability(&self) -> CommandHistoryAvailability {
        CommandHistoryAvailability::Available
    }

    /// 本组场景不查询历史，意外调用时返回空集合。
    async fn list(
        &self,
        _request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        Ok(Vec::new())
    }

    /// 可注入的写入错误在成功场景仍为空；正常时记录 draft 以验证用例到达唯一历史写口。
    ///
    /// @author kongweiguang
    async fn append(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        if let Some(error) = self
            .append_error
            .lock()
            .expect("history test mutex should not be poisoned")
            .take()
        {
            return Err(error);
        }
        self.drafts
            .lock()
            .expect("history test mutex should not be poisoned")
            .push(draft.clone());
        Ok(CommandHistoryEntry {
            id: "history-test-entry".to_owned(),
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

    /// 本组场景不删除历史，返回未命中。
    async fn delete(&self, _entry_id: String) -> AppResult<bool> {
        Ok(false)
    }

    /// 本组场景不清理历史，返回零删除数。
    async fn clear(&self, _request: CommandHistoryClearRequest) -> AppResult<u64> {
        Ok(0)
    }
}

/// 其它命令依赖均显式拒绝未参与的调用，保持测试只覆盖提交与历史边界。
struct UnusedSnippetRepository;

#[async_trait]
impl SnippetRepository for UnusedSnippetRepository {
    /// 此测试不读取片段目录。
    async fn list(
        &self,
        _request: SnippetCatalogListRequest,
    ) -> AppResult<Vec<SnippetCatalogItem>> {
        unused_endpoint()
    }

    /// 本组场景不验证片段目录身份。
    async fn catalog_identity_exists(
        &self,
        _origin: SnippetCatalogOrigin,
        _snippet_id: String,
    ) -> AppResult<bool> {
        unused_endpoint()
    }

    /// 本组场景不提交片段导入批次。
    async fn commit_import_batch(
        &self,
        _request: SnippetImportBatchRequest,
    ) -> AppResult<SnippetImportBatchState> {
        unused_endpoint()
    }

    /// 本组场景不读取片段导入回执。
    async fn import_batch_status(&self, _batch_id: String) -> AppResult<SnippetImportBatchState> {
        unused_endpoint()
    }

    /// 此测试不读取用户片段。
    async fn get_user(&self, _snippet_id: String) -> AppResult<Option<SnippetDefinition>> {
        unused_endpoint()
    }

    /// 此测试不保存用户片段。
    async fn save_user(&self, _definition: SnippetDefinition) -> AppResult<SnippetDefinition> {
        unused_endpoint()
    }

    /// 此测试不删除用户片段。
    async fn delete_user(&self, _snippet_id: String) -> AppResult<bool> {
        unused_endpoint()
    }

    /// 此测试不读取带版本的用户片段。
    async fn get_user_versioned(
        &self,
        _snippet_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<SnippetDefinition>>> {
        unused_endpoint()
    }

    /// 此测试不保存带版本的用户片段。
    async fn save_user_versioned(
        &self,
        _definition: SnippetDefinition,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<SnippetDefinition>> {
        unused_endpoint()
    }

    /// 此测试不删除带版本的用户片段。
    async fn delete_user_versioned(
        &self,
        _snippet_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unused_endpoint()
    }
}

/// 工作流依赖未参与 MCP terminal.write 测试，所有意外调用均失败闭合。
struct UnusedWorkflowRepository;

#[async_trait]
impl WorkflowRepository for UnusedWorkflowRepository {
    /// 此测试不查询工作流目录。
    async fn list(&self, _request: WorkflowListRequest) -> AppResult<Vec<CommandWorkflow>> {
        unused_endpoint()
    }

    /// 此测试不读取工作流。
    async fn get(&self, _workflow_id: String) -> AppResult<Option<CommandWorkflow>> {
        unused_endpoint()
    }

    /// 此测试不保存工作流。
    async fn save(&self, _workflow: CommandWorkflow) -> AppResult<CommandWorkflow> {
        unused_endpoint()
    }

    /// 此测试不删除工作流。
    async fn delete(&self, _workflow_id: String) -> AppResult<bool> {
        unused_endpoint()
    }

    /// 此测试不读取带版本的工作流。
    async fn get_versioned(
        &self,
        _workflow_id: String,
    ) -> AppResult<Option<VersionedCommandDefinition<CommandWorkflow>>> {
        unused_endpoint()
    }

    /// 此测试不保存带版本的工作流。
    async fn save_versioned(
        &self,
        _workflow: CommandWorkflow,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedCommandDefinition<CommandWorkflow>> {
        unused_endpoint()
    }

    /// 此测试不删除带版本的工作流。
    async fn delete_versioned(
        &self,
        _workflow_id: String,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        unused_endpoint()
    }
}

/// 建议源不参与历史提交；非空候选也不应改变 MCP write 的来源或目标。
struct UnusedSuggestionProvider;

#[async_trait]
impl CommandSuggestionProvider for UnusedSuggestionProvider {
    /// 不声明 provider，确保测试不会触发建议查询。
    fn supported_providers(&self) -> Vec<SuggestionProviderKind> {
        Vec::new()
    }

    /// 此测试不生成命令建议。
    async fn suggest(
        &self,
        _request: CommandSuggestionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<CommandSuggestionCandidate>> {
        Ok(Vec::new())
    }
}

/// 假终端记录 raw bytes，并可在完整 ACK 返回前推进 prompt 或结束 session。
struct ReceiptSessionService {
    session: AsyncMutex<SessionInfo>,
    command_context: AsyncMutex<TerminalCommandContext>,
    write_result: AsyncMutex<Option<AppResult<TerminalWriteReceipt>>>,
    after_write_session: AsyncMutex<Option<SessionInfo>>,
    after_write_context: AsyncMutex<Option<TerminalCommandContext>>,
    writes: Mutex<Vec<Vec<u8>>>,
    ui_command_submissions: Mutex<Vec<UiCommandLineRequest>>,
    ui_input_command_submissions: Mutex<Vec<UiTerminalCommandSubmissionRequest>>,
    ui_command_submission_error: Mutex<Option<AppError>>,
    ui_command_cancel_after_ack: Mutex<Option<CancellationToken>>,
    shell_results: AsyncMutex<VecDeque<TestShellCommandResult>>,
    shell_commands: Mutex<Vec<String>>,
    shell_bindings: Mutex<Vec<Option<UiSessionBinding>>>,
    list_calls: AtomicUsize,
    context_calls: AtomicUsize,
}

impl ReceiptSessionService {
    /// 固定输入前状态及 writer 回执，允许测试模拟 ACK 前出现的快速状态变化。
    ///
    /// @author kongweiguang
    fn new(
        session: SessionInfo,
        context: TerminalCommandContext,
        write_result: AppResult<TerminalWriteReceipt>,
        after_write_session: Option<SessionInfo>,
        after_write_context: Option<TerminalCommandContext>,
    ) -> Self {
        Self {
            session: AsyncMutex::new(session),
            command_context: AsyncMutex::new(context),
            write_result: AsyncMutex::new(Some(write_result)),
            after_write_session: AsyncMutex::new(after_write_session),
            after_write_context: AsyncMutex::new(after_write_context),
            writes: Mutex::new(Vec::new()),
            ui_command_submissions: Mutex::new(Vec::new()),
            ui_input_command_submissions: Mutex::new(Vec::new()),
            ui_command_submission_error: Mutex::new(None),
            ui_command_cancel_after_ack: Mutex::new(None),
            shell_results: AsyncMutex::new(VecDeque::new()),
            shell_commands: Mutex::new(Vec::new()),
            shell_bindings: Mutex::new(Vec::new()),
            list_calls: AtomicUsize::new(0),
            context_calls: AtomicUsize::new(0),
        }
    }

    /// 为代理恢复用例排入确定的真实 shell 回执，避免 fake 在队列耗尽时伪报成功。
    ///
    /// @author kongweiguang
    async fn queue_shell_results(&self, results: impl IntoIterator<Item = TestShellCommandResult>) {
        *self.shell_results.lock().await = results.into_iter().collect();
    }

    /// 仅返回 fake 实际收到的脚本副本，用于证明核对不会重放 apply 或 restore。
    ///
    /// @author kongweiguang
    fn shell_commands(&self) -> Vec<String> {
        self.shell_commands
            .lock()
            .expect("session test mutex should not be poisoned")
            .clone()
    }

    /// 返回每条受管命令携带的 binding，证明 stop 后恢复仍指向原 owner 与 generation。
    ///
    /// @author kongweiguang
    fn shell_bindings(&self) -> Vec<Option<UiSessionBinding>> {
        self.shell_bindings
            .lock()
            .expect("session test mutex should not be poisoned")
            .clone()
    }

    /// 返回测试收到的原始输入副本，验证跳过历史不会改变已发送字节。
    fn writes(&self) -> Vec<Vec<u8>> {
        self.writes
            .lock()
            .expect("session test mutex should not be poisoned")
            .clone()
    }

    /// 返回单次 command-line 请求副本，用来确认用例不会把正文和 Enter 拆成两次写入。
    ///
    /// @author kongweiguang
    fn ui_command_submissions(&self) -> Vec<UiCommandLineRequest> {
        self.ui_command_submissions
            .lock()
            .expect("session test mutex should not be poisoned")
            .clone()
    }

    /// 返回一次性 UI 输入命令尝试，区分确切 writer ACK、失败和未知结果且不暴露内部锁。
    ///
    /// @author kongweiguang
    fn ui_input_command_submissions(&self) -> Vec<UiTerminalCommandSubmissionRequest> {
        self.ui_input_command_submissions
            .lock()
            .expect("session test mutex should not be poisoned")
            .clone()
    }

    /// 模拟 production gate 的精确版本资格，同时仍回显普通输入 ACK 以验证 fail-closed 历史边界。
    ///
    /// @author kongweiguang
    async fn apply_ui_command_submission(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<UiTerminalCommandSubmissionReceipt> {
        let session = self.session.lock().await;
        let mut context = self.command_context.lock().await;
        if session.lifecycle != kerminal_domain::sessions::SessionLifecycle::Running
            || session.session_id != request.input.binding.session_id
            || session.owner != request.input.binding.owner
            || context.session_id != request.input.binding.session_id
            || context.owner != request.input.binding.owner
            || context.session_generation != request.input.binding.session_generation
        {
            return Err(AppError::conflict(
                "test UI command submission binding no longer owns the active generation",
            ));
        }
        let command_associated = request.input.expected_context == Some(context.version())
            && context.availability == crate::sessions::TerminalCommandAvailability::Available
            && request.input.projection == crate::sessions::UiInputProjection::Tracked
            && request.input.event == super::enter_event();
        self.ui_input_command_submissions
            .lock()
            .expect("session test mutex should not be poisoned")
            .push(request);
        if let Some(error) = self
            .ui_command_submission_error
            .lock()
            .expect("session test mutex should not be poisoned")
            .take()
        {
            return Err(error);
        }

        context.input_generation = context
            .input_generation
            .checked_add(1)
            .ok_or_else(|| AppError::outcome_unknown("test input generation overflow"))?;
        context.prompt_epoch = context.prompt_epoch.saturating_add(1);
        context.prompt_line_empty = true;
        if let Some(cancellation) = self
            .ui_command_cancel_after_ack
            .lock()
            .expect("session test mutex should not be poisoned")
            .take()
        {
            cancellation.cancel();
        }
        Ok(UiTerminalCommandSubmissionReceipt {
            context: context.clone(),
            command_associated,
        })
    }
}

/// 用退出码或未知回执模拟受管命令的唯一真实结果，不在核对流程中合成成功。
#[derive(Clone, Copy)]
enum TestShellCommandResult {
    Finished(i32),
    Unknown(crate::sessions::ShellCommandUnknownReason),
}

/// 端口转发 fake 只保留实际绑定与故障订阅，停机走生产应用服务的资源回收路径。
struct RecoveryPortForwardRuntime;

struct RecoveryPortForwardHandle {
    endpoint: ForwardEndpoint,
    failures: watch::Receiver<Option<PublicErrorCode>>,
    _failure_sender: watch::Sender<Option<PublicErrorCode>>,
}

#[async_trait]
impl PortForwardRuntime for RecoveryPortForwardRuntime {
    /// 复用请求端点建立 fake handle；测试只验证 owner 的启停，不绑定真实 socket。
    async fn start(
        &self,
        _id: String,
        config: PortForwardConfig,
        _cancel: CancellationToken,
    ) -> AppResult<Arc<dyn PortForwardRuntimeHandle>> {
        let (failure_sender, failures) = watch::channel(None);
        Ok(Arc::new(RecoveryPortForwardHandle {
            endpoint: config.bind,
            failures,
            _failure_sender: failure_sender,
        }))
    }
}

#[async_trait]
impl PortForwardRuntimeHandle for RecoveryPortForwardHandle {
    /// 返回 fake adapter 接受的绑定端点，避免测试引入平台网络资源。
    fn actual_bind(&self) -> ForwardEndpoint {
        self.endpoint.clone()
    }

    /// 维持无故障活动态，stop 测试仍由真实 service 取消该监控任务。
    fn subscribe_failure(&self) -> watch::Receiver<Option<PublicErrorCode>> {
        self.failures.clone()
    }

    /// fake handle 没有外部资源，接受生产 stop 请求以覆盖 owner 清理和策略取消。
    async fn close_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

#[async_trait]
impl SessionService for ReceiptSessionService {
    /// 本测试只绑定已有 session，不创建终端。
    async fn create(&self, _request: CreateSessionRequest) -> AppResult<SessionInfo> {
        unused_endpoint()
    }

    /// 本测试只绑定已有 session，不创建终端。
    async fn create_cancellable(
        &self,
        _request: CreateSessionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_endpoint()
    }

    /// 提交用例只需要按显式 session ID 读取目标元数据。
    async fn list(&self, _filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        self.list_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![self.session.lock().await.clone()])
    }

    /// 测试拒绝 MCP 写入用运行态 metadata 旁路重建资格。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unused_endpoint()
    }

    /// 统计该接口调用数，确认 MCP 历史不依赖 writer ACK 后的新 prompt 快照。
    async fn terminal_command_context(
        &self,
        _session_id: SessionId,
    ) -> AppResult<TerminalCommandContext> {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.command_context.lock().await.clone())
    }

    /// 所有测试输入都通过下方带 receipt 的 writer 路径发送。
    async fn write(&self, _session_id: SessionId, bytes: Vec<u8>) -> AppResult<()> {
        self.writes
            .lock()
            .expect("session test mutex should not be poisoned")
            .push(bytes);
        Ok(())
    }

    /// 先保留原始 bytes，再返回 ack；后态可在返回前变成下一 prompt 或 Exited。
    async fn write_with_receipt(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> AppResult<TerminalWriteReceipt> {
        self.write(session_id, bytes).await?;
        if let Some(session) = self.after_write_session.lock().await.take() {
            *self.session.lock().await = session;
        }
        if let Some(context) = self.after_write_context.lock().await.take() {
            *self.command_context.lock().await = context;
        }
        self.write_result
            .lock()
            .await
            .take()
            .unwrap_or_else(|| Err(AppError::unsupported("test writer has no receipt")))
    }

    /// 本测试不发按键或 paste 事件。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: kerminal_terminal::input::TerminalInputEvent,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// fake 在入口复核 session、UI owner 与 generation，验证应用用例传递精确目标身份。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session.lock().await;
        let context = self.command_context.lock().await;
        if session.lifecycle != kerminal_domain::sessions::SessionLifecycle::Running
            || session.session_id != request.binding.session_id
            || session.owner != request.binding.owner
            || context.session_id != request.binding.session_id
            || context.owner != request.binding.owner
            || context.session_generation != request.binding.session_generation
        {
            return Err(AppError::conflict(
                "test UI command binding no longer owns the active generation",
            ));
        }
        self.ui_command_submissions
            .lock()
            .expect("session test mutex should not be poisoned")
            .push(request);
        Ok(context.clone())
    }

    /// 在同一 fake writer gate 复核固定 binding/context，并捕获正文与 Enter 的单次 atomic submission。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<TerminalCommandContext> {
        self.apply_ui_command_submission(request)
            .await
            .map(|receipt| receipt.context)
    }

    /// 只把测试 fake 自己验证的同代 input 资格作为可信命令关联回执交给 application。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission_with_receipt(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<UiTerminalCommandSubmissionReceipt> {
        self.apply_ui_command_submission(request).await
    }

    /// 本测试不调整终端网格。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不生成 GPU 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unused_endpoint()
    }

    /// 本测试不改终端调色板。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: kerminal_terminal::snapshot::TerminalPalette,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不改 scrollback 限额。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unused_endpoint()
    }

    /// 本测试不读取终端文本快照。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unused_endpoint()
    }

    /// 本测试不滚动终端视口。
    async fn scroll(
        &self,
        _session_id: SessionId,
        _request: crate::sessions::ScrollRequest,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// 该 fake 只验证命令提交，不维护 session 绑定的渲染视口。
    ///
    /// @author kongweiguang
    async fn scroll_ui(
        &self,
        _binding: crate::sessions::UiSessionBinding,
        _request: crate::sessions::ScrollRequest,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不变更终端选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不读取选择文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unused_endpoint()
    }

    /// 本测试不搜索 scrollback。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unused_endpoint()
    }

    /// 本测试不创建远程重连。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        unused_endpoint()
    }

    /// 本测试不关闭运行时会话。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不等待关闭 deadline。
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: tokio::time::Instant,
    ) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不关闭应用会话集合。
    async fn close_all(&self) -> AppResult<()> {
        unused_endpoint()
    }

    /// 本测试不订阅实时终端事件。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unused_endpoint()
    }

    /// 用预设结果驱动受管 shell 调用，保留每条脚本以证明未知结果只被核对。
    ///
    /// @author kongweiguang
    async fn execute_shell_command(
        &self,
        request: ShellCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        let command_id = request.command_id;
        self.shell_bindings
            .lock()
            .expect("session test mutex should not be poisoned")
            .push(request.expected_ui_binding.clone());
        self.shell_commands
            .lock()
            .expect("session test mutex should not be poisoned")
            .push(request.command);
        match self.shell_results.lock().await.pop_front() {
            Some(TestShellCommandResult::Finished(exit_code)) => {
                Ok(ShellCommandOutcome::Finished {
                    command_id,
                    exit_code,
                })
            }
            Some(TestShellCommandResult::Unknown(reason)) => {
                Ok(ShellCommandOutcome::Unknown { command_id, reason })
            }
            None => Err(AppError::unsupported(
                "proxy recovery fake has no queued shell result",
            )),
        }
    }
}

/// 把实际提交用例装配到内存 ports，未涉及的服务依赖采用失败闭合测试 adapter。
fn commands_service(
    history: Arc<CapturingHistoryRepository>,
    sessions: Arc<ReceiptSessionService>,
) -> CommandsService {
    CommandsService::new(
        history,
        Arc::new(UnusedSnippetRepository),
        Arc::new(UnusedWorkflowRepository),
        Arc::new(UnusedSuggestionProvider),
        sessions,
    )
}

/// 为代理恢复场景装配唯一 UI SSH binding、真实应用服务与隔离的转发 adapter。
///
/// @author kongweiguang
struct ProxyRecoveryFixture {
    proxy: TerminalProxyApplication,
    port_forwards: PortForwardService,
    sessions: Arc<ReceiptSessionService>,
    binding: UiSessionBinding,
    forward_id: String,
}

/// 创建无网络副作用的 remote-dynamic 转发，测试只验证生产应用层的 owner 生命周期。
///
/// @author kongweiguang
async fn proxy_recovery_fixture() -> ProxyRecoveryFixture {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let host_id = HostId::parse("host-proxy-recovery").expect("fixture host id is valid");
    let version = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 19,
    };
    let context = super::context(
        session_id.clone(),
        owner.clone(),
        version,
        crate::sessions::TerminalCommandAvailability::Available,
        true,
    );
    let mut session = super::session(session_id.clone(), owner.clone());
    session.target = SessionTarget::Ssh(SshSessionTarget {
        host_id: host_id.clone(),
        working_directory: None,
        command: None,
    });
    let sessions = Arc::new(ReceiptSessionService::new(
        session,
        context,
        Err(AppError::unsupported("proxy test does not write raw input")),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history, sessions.clone());
    let policy = FutureProxyPolicyStore::new();
    let port_forwards = PortForwardService::new(Arc::new(RecoveryPortForwardRuntime))
        .with_future_proxy_policy_store(policy.clone());
    let started = port_forwards
        .start(
            PortForwardConfig {
                host_id,
                name: "isolated recovery fixture".to_owned(),
                kind: ForwardKind::RemoteDynamic,
                bind: ForwardEndpoint::new("127.0.0.1", 1080)
                    .expect("fixture bind endpoint is valid"),
                destination: None,
                proxy_protocol: Some(ProxyProtocol::Socks5),
                remote_access_scope: Some(RemoteAccessScope::Loopback),
                proxy_apply_scope: Default::default(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("isolated fake forward starts");
    let forward_id = started.forward.id;
    let proxy = TerminalProxyApplication::new(
        Arc::new(commands),
        sessions.clone(),
        port_forwards.clone(),
        policy,
    );
    ProxyRecoveryFixture {
        proxy,
        port_forwards,
        sessions,
        binding: UiSessionBinding {
            session_id,
            owner,
            session_generation: version.session_generation,
        },
        forward_id,
    }
}

/// 先取得真实 Applied 回执，再经生产 stop 清理转发，以覆盖隧道停止后的恢复边界。
///
/// @author kongweiguang
async fn stop_after_confirmed_proxy_apply(fixture: &ProxyRecoveryFixture) {
    fixture
        .sessions
        .queue_shell_results([TestShellCommandResult::Finished(0)])
        .await;
    let applied = fixture
        .proxy
        .apply_current(
            CurrentTerminalProxyRequest {
                forward_id: fixture.forward_id.clone(),
                binding: fixture.binding.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("fake shell returns a known apply result");
    assert_eq!(applied.outcome, TerminalProxyApplyOutcome::Applied);

    fixture
        .port_forwards
        .stop(&fixture.forward_id)
        .await
        .expect("production service stops the fake forward");
    assert!(
        fixture
            .port_forwards
            .get(&fixture.forward_id)
            .await
            .is_none()
    );
}

/// 停止原转发后仍能核对并恢复同一 UI session generation，不依赖活动 forward 列表。
///
/// @author kongweiguang
#[tokio::test]
async fn stopped_forward_keeps_same_binding_recovery_available() {
    let fixture = proxy_recovery_fixture().await;
    stop_after_confirmed_proxy_apply(&fixture).await;

    fixture
        .sessions
        .queue_shell_results([
            TestShellCommandResult::Finished(1),
            TestShellCommandResult::Finished(1),
            TestShellCommandResult::Finished(0),
            TestShellCommandResult::Finished(0),
            TestShellCommandResult::Finished(0),
            TestShellCommandResult::Finished(0),
        ])
        .await;
    let reconciled = fixture
        .proxy
        .reconcile_unique_current_application(fixture.binding.clone(), CancellationToken::new())
        .await
        .expect("unique tracked application can be reconciled after stop")
        .expect("original transaction remains tracked");
    assert_eq!(reconciled.forward_id, fixture.forward_id);
    assert!(matches!(
        reconciled.outcome,
        TerminalProxyReconcileOutcome::Applied | TerminalProxyReconcileOutcome::AppliedWithChanges
    ));

    fixture
        .sessions
        .queue_shell_results([
            TestShellCommandResult::Finished(0),
            TestShellCommandResult::Finished(0),
            TestShellCommandResult::Finished(0),
        ])
        .await;
    let restored = fixture
        .proxy
        .restore_current(
            CurrentTerminalProxyRequest {
                forward_id: fixture.forward_id.clone(),
                binding: fixture.binding.clone(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("restore remains callable after the original forward stopped");
    assert_eq!(restored.outcome, TerminalProxyApplyOutcome::Restored);
    assert!(
        fixture
            .sessions
            .shell_bindings()
            .iter()
            .all(|binding| binding.as_ref() == Some(&fixture.binding))
    );
}

/// Wrong generation neither discovers nor mutates the prior generation's recovery record.
///
/// @author kongweiguang
#[tokio::test]
async fn proxy_recovery_refuses_a_different_session_generation_without_shell_write() {
    let fixture = proxy_recovery_fixture().await;
    stop_after_confirmed_proxy_apply(&fixture).await;
    let mut wrong_binding = fixture.binding.clone();
    wrong_binding.session_generation += 1;

    assert!(
        fixture
            .proxy
            .reconcile_unique_current_application(wrong_binding.clone(), CancellationToken::new())
            .await
            .expect("wrong generation has no unique transaction")
            .is_none()
    );
    let result = fixture
        .proxy
        .restore_current(
            CurrentTerminalProxyRequest {
                forward_id: fixture.forward_id.clone(),
                binding: wrong_binding,
            },
            CancellationToken::new(),
        )
        .await
        .expect("stale binding is returned as a safe not-applied result");
    assert!(matches!(
        result.outcome,
        TerminalProxyApplyOutcome::NotApplied {
            reason: TerminalProxyUnavailableReason::SessionBindingMismatch
        }
    ));
    assert_eq!(fixture.sessions.shell_commands().len(), 1);
}

/// Unknown 回执只运行一次原事务 probe；不得自动重放 apply 或 restore shell 脚本。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_proxy_recovery_probes_original_transaction_without_replay() {
    let fixture = proxy_recovery_fixture().await;
    stop_after_confirmed_proxy_apply(&fixture).await;
    fixture
        .sessions
        .queue_shell_results([TestShellCommandResult::Unknown(
            crate::sessions::ShellCommandUnknownReason::MissingCompletion,
        )])
        .await;

    let result = fixture
        .proxy
        .reconcile_unique_current_application(fixture.binding.clone(), CancellationToken::new())
        .await
        .expect("unknown probe remains a typed result")
        .expect("original record remains uniquely tracked");

    assert_eq!(result.forward_id, fixture.forward_id);
    assert!(matches!(
        result.outcome,
        TerminalProxyReconcileOutcome::Unknown { .. }
    ));
    let commands = fixture.sessions.shell_commands();
    assert_eq!(commands.len(), 2, "one apply plus one read-only probe");
    assert!(
        fixture
            .sessions
            .shell_bindings()
            .iter()
            .all(|binding| binding.as_ref() == Some(&fixture.binding))
    );
}

/// 生成缺席路径对应的清晰错误，防止测试 stub 静默伪造业务成功。
fn unused_endpoint<T>() -> AppResult<T> {
    Err(AppError::unsupported(
        "unused command submission test endpoint",
    ))
}

/// 为 UI 历史回归测试构造与缓存 context 一致的绑定和显式 Enter 事件。
///
/// @author kongweiguang
fn ui_input(
    session_id: SessionId,
    owner: kerminal_domain::sessions::SessionOwner,
    version: super::TerminalCommandContextVersion,
) -> UiTerminalInputRequest {
    UiTerminalInputRequest {
        binding: UiSessionBinding {
            session_id,
            owner,
            session_generation: version.session_generation,
        },
        expected_context: Some(version),
        projection: crate::sessions::UiInputProjection::Tracked,
        event: super::enter_event(),
    }
}

/// 只有同一 UI binding/context 的完整 writer ACK 才写入 User 历史。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_requires_proven_writer_ack() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let session = super::session(session_id.clone(), owner.clone());
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        session,
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id.clone(), owner.clone(), expected),
            "dir".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("structured UI writer should acknowledge the command");

    assert_eq!(
        result.context.input_generation,
        expected.input_generation + 1
    );
    assert!(
        result
            .history
            .history_record
            .is_some_and(|record| record.recorded)
    );
    assert_eq!(result.history.history_error_code, None);
    let drafts = history.drafts();
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].command, "dir");
    assert_eq!(
        drafts[0].source,
        kerminal_domain::commands::CommandHistorySource::User
    );
    assert_eq!(
        drafts[0].target,
        kerminal_domain::commands::CommandHistoryTarget::Local
    );
    assert_eq!(drafts[0].session_id.as_ref(), Some(&session_id));
    let expected_pane = match &owner {
        kerminal_domain::sessions::SessionOwner::Ui { pane_id, .. } => pane_id.clone(),
        _ => unreachable!("fixture owner is a UI pane"),
    };
    assert_eq!(drafts[0].pane_id.as_ref(), Some(&expected_pane));
    let submissions = sessions.ui_input_command_submissions();
    assert_eq!(submissions.len(), 1);
    assert_eq!(submissions[0].command, "dir");
    assert_eq!(submissions[0].input.event, super::enter_event());
    assert_eq!(submissions[0].input.expected_context, Some(expected));
}

/// Shell integration 不可用时命令仍可发送，但正文没有足够证据写入历史。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_skips_untrusted_shell_context() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Unavailable(
            crate::sessions::TerminalCommandUnavailableReason::ShellIntegrationDisabled,
        ),
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("shell-integration state must not block ordinary UI input");

    assert_eq!(result.history.history_record, None);
    assert_eq!(result.history.history_error_code, None);
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// prompt epoch 在排队期间变化但 input generation 未变时，writer 可发输入却不能证明旧正文关联。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_skips_stale_prompt_projection() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let mut actual_context = before.clone();
    actual_context.prompt_epoch += 1;
    *sessions.command_context.lock().await = actual_context;
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("stale projection does not reject ordinary terminal input");

    assert_eq!(
        result.context.input_generation,
        expected.input_generation + 1
    );
    assert!(result.context.prompt_epoch > expected.prompt_epoch);
    assert_eq!(result.history.history_record, None);
    assert_eq!(result.history.history_error_code, None);
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// 关闭历史记录策略只跳过辅助记录，不改变单次结构化终端提交。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_respects_record_opt_out() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            false,
            CancellationToken::new(),
        )
        .await
        .expect("record opt-out must not block terminal submission");

    assert_eq!(result.history.history_record, None);
    assert_eq!(result.history.history_error_code, None);
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// 历史仓储失败只进入独立回执，不会改写已成功的终端 writer ACK。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_failure_is_separate_from_writer_success() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    *history
        .append_error
        .lock()
        .expect("history test mutex should not be poisoned") =
        Some(AppError::infrastructure("test history storage failure"));
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("history failure must not turn a successful writer ACK into a send failure");

    assert_eq!(
        result.context.input_generation,
        expected.input_generation + 1
    );
    assert_eq!(result.history.history_record, None);
    assert_eq!(
        result.history.history_error_code,
        Some(crate::PublicErrorCode::Infrastructure)
    );
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// writer 返回未知结果时不追加历史，也不由 application 进行重试或重放。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_skips_unknown_writer_outcome() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    *sessions
        .ui_command_submission_error
        .lock()
        .expect("session test mutex should not be poisoned") =
        Some(AppError::outcome_unknown("test writer result is unknown"));
    let commands = commands_service(history.clone(), sessions.clone());

    let error = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown writer ACK must remain an error");

    assert_eq!(error.public_code(), crate::PublicErrorCode::OutcomeUnknown);
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// writer 已完成 ACK 但调用被取消时保留单次发送结果，不触碰历史仓储或重放输入。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_skips_when_cancelled_after_writer_ack() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let cancellation = CancellationToken::new();
    *sessions
        .ui_command_cancel_after_ack
        .lock()
        .expect("session test mutex should not be poisoned") = Some(cancellation.clone());
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "dir".to_owned(),
            before,
            true,
            cancellation,
        )
        .await
        .expect("a completed writer ACK remains successful if cancellation races afterward");

    assert_eq!(
        result.context.input_generation,
        expected.input_generation + 1
    );
    assert_eq!(result.history.history_record, None);
    assert_eq!(result.history.history_error_code, None);
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// 安全正文过滤继续由既有 record_history 执行，不把凭据样式命令送进仓储。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_typed_submission_history_keeps_sensitive_filter() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let expected = super::TerminalCommandContextVersion {
        session_generation: 7,
        prompt_epoch: 11,
        input_generation: 20,
    };
    let before = super::context(
        session_id.clone(),
        owner.clone(),
        expected,
        crate::sessions::TerminalCommandAvailability::Available,
        false,
    );
    let history = Arc::new(CapturingHistoryRepository::default());
    let sessions = Arc::new(ReceiptSessionService::new(
        super::session(session_id.clone(), owner.clone()),
        before.clone(),
        Err(AppError::unsupported(
            "UI writer uses its structured request",
        )),
        None,
        None,
    ));
    let commands = commands_service(history.clone(), sessions.clone());

    let result = commands
        .submit_ui_terminal_command(
            ui_input(session_id, owner, expected),
            "curl -H 'Authorization: Bearer fixture-token'".to_owned(),
            before,
            true,
            CancellationToken::new(),
        )
        .await
        .expect("history filtering must not block an acknowledged command");

    assert_eq!(result.history.history_error_code, None);
    assert!(
        result
            .history
            .history_record
            .is_some_and(|record| !record.recorded)
    );
    assert!(history.drafts().is_empty());
    assert_eq!(sessions.ui_input_command_submissions().len(), 1);
}

/// 快命令在 writer ACK 前已切到新 prompt 并退出，历史仍使用输入时的 receipt 与目标。
#[tokio::test]
async fn mcp_write_records_original_admission_after_fast_prompt_and_exit() {
    let session_id = super::session_id();
    let owner = kerminal_domain::sessions::SessionOwner::Mcp;
    let before = super::session(session_id.clone(), owner.clone());
    let mut after = before.clone();
    after.lifecycle = kerminal_domain::sessions::SessionLifecycle::Exited;

    let before_context = super::context(
        session_id.clone(),
        owner.clone(),
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        crate::sessions::TerminalCommandAvailability::Unavailable(
            crate::sessions::TerminalCommandUnavailableReason::NonUiOwner,
        ),
        true,
    );
    let mut next_prompt = before_context.clone();
    next_prompt.prompt_epoch += 1;
    next_prompt.input_generation += 1;
    let mut admission = super::admission(session_id.clone(), owner);
    admission.session_generation = 7;
    admission.prompt_epoch = 11;
    admission.input_generation_before = 20;
    admission.accepted_input_generation = 21;
    let payload = "pwd\r";
    let session_service = Arc::new(ReceiptSessionService::new(
        before,
        before_context,
        Ok(TerminalWriteReceipt {
            acknowledged_bytes: payload.len(),
            admission: Some(admission),
        }),
        Some(after),
        Some(next_prompt),
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let result = super::write_mcp_terminal_input(&commands, session_id.clone(), payload.to_owned())
        .await
        .expect("complete writer ACK should keep the input successful");

    assert!(result.history_record.is_some_and(|record| record.recorded));
    assert_eq!(history.drafts().len(), 1);
    assert_eq!(
        history.drafts()[0].source,
        kerminal_domain::commands::CommandHistorySource::Tool
    );
    assert_eq!(history.drafts()[0].session_id.as_ref(), Some(&session_id));
    assert_eq!(session_service.list_calls.load(Ordering::SeqCst), 1);
    assert_eq!(session_service.context_calls.load(Ordering::SeqCst), 0);
    assert_eq!(session_service.writes(), vec![payload.as_bytes().to_vec()]);
}

/// default adapter 无 trusted admission 时仍发送原文，但不能生成 Tool 历史。
#[tokio::test]
async fn mcp_write_preserves_bytes_without_admission_and_skips_history() {
    let session_id = super::session_id();
    let owner = kerminal_domain::sessions::SessionOwner::Mcp;
    let before = super::session(session_id.clone(), owner.clone());
    let context = super::context(
        session_id.clone(),
        owner,
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        crate::sessions::TerminalCommandAvailability::Unavailable(
            crate::sessions::TerminalCommandUnavailableReason::NonUiOwner,
        ),
        true,
    );
    let payload = "echo raw\r";
    let session_service = Arc::new(ReceiptSessionService::new(
        before,
        context,
        Ok(TerminalWriteReceipt {
            acknowledged_bytes: payload.len(),
            admission: None,
        }),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let result = super::write_mcp_terminal_input(&commands, session_id, payload.to_owned())
        .await
        .expect("missing admission must not reject valid raw terminal bytes");

    assert!(result.history_record.is_none());
    assert!(history.drafts().is_empty());
    assert_eq!(session_service.writes(), vec![payload.as_bytes().to_vec()]);
}

/// 部分写入或 flush 结果未知时保留 writer 的 OutcomeUnknown，绝不追加历史。
#[tokio::test]
async fn mcp_write_does_not_record_an_unknown_writer_outcome() {
    let session_id = super::session_id();
    let owner = kerminal_domain::sessions::SessionOwner::Mcp;
    let before = super::session(session_id.clone(), owner.clone());
    let context = super::context(
        session_id.clone(),
        owner,
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        super::TerminalCommandAvailability::Available,
        true,
    );
    let payload = "pwd\r";
    let session_service = Arc::new(ReceiptSessionService::new(
        before,
        context,
        Err(AppError::outcome_unknown(
            "test writer flush outcome is unknown",
        )),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let result = super::write_mcp_terminal_input(&commands, session_id, payload.to_owned()).await;

    assert_eq!(
        result
            .expect_err("unknown writer outcome must remain an error")
            .public_code(),
        super::PublicErrorCode::OutcomeUnknown
    );
    assert!(history.drafts().is_empty());
    assert_eq!(session_service.writes(), vec![payload.as_bytes().to_vec()]);
}

/// session 与 admission 的 owner 不符时保留已完成原文写入，但跳过错误归属的历史。
#[tokio::test]
async fn mcp_write_rejects_metadata_with_a_different_admission_owner() {
    let session_id = super::session_id();
    let owner = kerminal_domain::sessions::SessionOwner::Mcp;
    let before = super::session(session_id.clone(), owner.clone());
    let context = super::context(
        session_id.clone(),
        owner,
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        super::TerminalCommandAvailability::Available,
        true,
    );
    let payload = "pwd\r";
    let admission = super::admission(session_id.clone(), super::ui_owner());
    let session_service = Arc::new(ReceiptSessionService::new(
        before,
        context,
        Ok(TerminalWriteReceipt {
            acknowledged_bytes: payload.len(),
            admission: Some(admission),
        }),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let result = super::write_mcp_terminal_input(&commands, session_id, payload.to_owned())
        .await
        .expect("metadata mismatch must not rewrite or fail the acknowledged bytes");

    assert!(result.history_record.is_none());
    assert!(history.drafts().is_empty());
    assert_eq!(session_service.writes(), vec![payload.as_bytes().to_vec()]);
}

/// 普通命令无需推断 prompt 状态，但必须把正文与 Enter 交给一次绑定代次的 writer 请求。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_command_submission_uses_one_generation_bound_writer_request() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let session = super::session(session_id.clone(), owner.clone());
    let context = super::context(
        session_id.clone(),
        owner.clone(),
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        crate::sessions::TerminalCommandAvailability::Unavailable(
            crate::sessions::TerminalCommandUnavailableReason::PromptNotReady,
        ),
        false,
    );
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner,
        session_generation: 7,
    };
    let command = "echo ready".to_owned();
    let session_service = Arc::new(ReceiptSessionService::new(
        session,
        context.clone(),
        Err(AppError::unsupported("raw writer is not used")),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let receipt = commands
        .send_ui_command(
            binding.clone(),
            command.clone(),
            true,
            CancellationToken::new(),
        )
        .await
        .expect("the bound UI writer should accept one complete command line");

    assert!(receipt.sent);
    assert!(!receipt.completion_tracked);
    assert_eq!(
        session_service.ui_command_submissions(),
        vec![UiCommandLineRequest {
            binding,
            command: command.clone()
        }]
    );
    assert_eq!(history.drafts().len(), 1);
    assert_eq!(history.drafts()[0].command, command);
    assert_eq!(
        history.drafts()[0].source,
        kerminal_domain::commands::CommandHistorySource::User
    );
}

/// 代次变化由原子 writer gate 拒绝后，不记录历史也不回退到无绑定的 raw write。
///
/// @author kongweiguang
#[tokio::test]
async fn ui_command_submission_rejects_a_stale_generation_without_history() {
    let session_id = super::session_id();
    let owner = super::ui_owner();
    let session = super::session(session_id.clone(), owner.clone());
    let context = super::context(
        session_id.clone(),
        owner.clone(),
        super::TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 11,
            input_generation: 20,
        },
        crate::sessions::TerminalCommandAvailability::Unavailable(
            crate::sessions::TerminalCommandUnavailableReason::PromptNotReady,
        ),
        false,
    );
    let binding = UiSessionBinding {
        session_id,
        owner,
        session_generation: 8,
    };
    let session_service = Arc::new(ReceiptSessionService::new(
        session,
        context,
        Err(AppError::unsupported("raw writer is not used")),
        None,
        None,
    ));
    let history = Arc::new(CapturingHistoryRepository::default());
    let commands = commands_service(history.clone(), session_service.clone());

    let error = commands
        .send_ui_command(
            binding,
            "echo stale".to_owned(),
            true,
            CancellationToken::new(),
        )
        .await
        .expect_err("stale visible-pane generation must be rejected");

    assert_eq!(error.public_code(), crate::PublicErrorCode::Conflict);
    assert!(session_service.ui_command_submissions().is_empty());
    assert!(history.drafts().is_empty());
}
