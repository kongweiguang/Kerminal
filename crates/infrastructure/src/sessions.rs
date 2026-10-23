//! 唯一终端会话 runtime：拥有 reader/writer、VT engine、生命周期和合并更新信号。
//!
//! @author kongweiguang

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Mutex as StdMutex,
    atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering},
};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use kerminal_application::sessions::logging::{OutputRecordResult, SessionOutputRecorder};
use kerminal_application::sessions::{
    AgentTerminalSubmissionRequest, AttachTransportRequest, COMMAND_BLOCK_RETENTION_LIMIT,
    CommandBlock, CommandBlockId, CommandBlockOutputIncompleteReason, CommandBlockOutputSnapshot,
    CommandBlockOutputState, CommandBlockOutputText, CommandBlockSnapshot, CommandBlockState,
    CommandBlockText, CommandBlockTextUnknownReason, CommandBlockUnknownReason,
    CreateSessionRequest, DEFAULT_SESSION_CLOSE_BUDGET, MAX_COMMAND_BLOCK_OUTPUT_BYTES,
    OutputRevision, ScrollRequest, SelectionViewport, SessionChange, SessionChangeKind,
    SessionCleanupPending, SessionFilter, SessionOpenCleanupOwner, SessionRuntimeMetadata,
    SessionService, SessionSubscription, SessionTransport, SessionTransportControl,
    SessionTransportFactory, SessionTransportOpenError, ShellCommandOutcome, ShellCommandRequest,
    ShellCommandUnknownReason, TerminalCommandAvailability, TerminalCommandContext,
    TerminalCommandContextVersion, TerminalCommandUnavailableReason, TerminalResizeCapability,
    TerminalWriteAdmission, TerminalWriteReceipt, UiCommandLineRequest, UiInputProjection,
    UiSessionBinding, UiSnippetCommandLineRequest, UiTerminalCommandSubmissionReceipt,
    UiTerminalCommandSubmissionRequest, UiTerminalInputRequest, UiTerminalSearchRequest,
    UiTerminalSearchResult, UiTerminalSearchViewport, classify_command_block_text,
    io_transport_error, is_plain_command_enter,
};
use kerminal_domain::connections::RemoteShellDialect;
use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::{
    SessionExitStatus, SessionInfo, SessionLifecycle, SessionLifecycleEvent, SessionOwner,
    SessionTarget, TerminalSize,
};
use kerminal_terminal::ShellIntegrationMarker;
use kerminal_terminal::engine::{
    TerminalEngine, TerminalEngineError, TerminalLimits, TerminalResizePolicy,
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent, encode_input,
};
use kerminal_terminal::output_capture::{
    TerminalOutputEvent, TerminalTranscriptState, TranscriptCaptureId, TranscriptIncompleteReason,
    TranscriptUnavailableReason,
};
use kerminal_terminal::snapshot::{
    PlainTextSnapshot, PlainTextSnapshotRequest, RenderSnapshot, RenderSnapshotRequest,
    TerminalPalette, TerminalSearchRequest, TerminalSearchResult, TerminalSelection,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{
    Mutex, Notify, OwnedSemaphorePermit, RwLock, Semaphore, broadcast, oneshot, watch,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

use crate::local_pty::resize_diagnostics_enabled;
use kerminal_application::{AppError, AppResult};

#[path = "sessions/auto_reconnect.rs"]
mod auto_reconnect;

use auto_reconnect::{AutoReconnectContext, AutoReconnectDecision, AutoReconnectPolicy};

const SHELL_EVENT_CAPACITY: usize = 64;
const COMMAND_INTERRUPT_NONE: u8 = 0;
const COMMAND_INTERRUPT_USER_INPUT: u8 = 1;
const COMMAND_INTERRUPT_DISCONNECT: u8 = 2;
const AUTO_RECONNECT_STABILITY_WINDOW: Duration = Duration::from_secs(30);
static NEXT_RESIZE_DIAGNOSTIC_SESSION_SEQUENCE: AtomicU64 = AtomicU64::new(1);
const MAX_RESIZE_DIAGNOSTIC_READER_CHUNKS: u64 = 16;

/// 各会话的资源上限；reader 与 writer 使用有界流，通知采用可合并的 watch 状态。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionRuntimeConfig {
    pub terminal_limits: TerminalLimits,
    pub maximum_sessions: usize,
    pub read_chunk_bytes: usize,
    pub close_wait: Duration,
}

impl Default for SessionRuntimeConfig {
    /// 以有限会话数、VT 容量和读块大小保护桌面与 MCP 共用的进程内 runtime。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            terminal_limits: TerminalLimits::default(),
            maximum_sessions: 256,
            read_chunk_bytes: 16 * 1024,
            close_wait: DEFAULT_SESSION_CLOSE_BUDGET,
        }
    }
}

/// 目标类型在组合 provider 中的显式路由键。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionTargetKind {
    Local,
    Ssh,
    Telnet,
    Serial,
    Container,
}

impl SessionTargetKind {
    /// 从领域目标读取协议类型，避免 provider 按 UI owner 或 Tab 状态猜测 transport。
    ///
    /// @author kongweiguang
    const fn from_target(target: &SessionTarget) -> Self {
        match target {
            SessionTarget::Local(_) => Self::Local,
            SessionTarget::Ssh(_) => Self::Ssh,
            SessionTarget::Telnet(_) => Self::Telnet,
            SessionTarget::Serial(_) => Self::Serial,
            SessionTarget::Container(_) => Self::Container,
        }
    }
}

/// 将 Local 与可选远程 provider 组合起来，SessionManager 仍是唯一会话注册表。
pub struct CompositeSessionTransportFactory {
    providers: HashMap<SessionTargetKind, Arc<dyn SessionTransportFactory>>,
}

impl CompositeSessionTransportFactory {
    /// 以本地 PTY provider 初始化工厂；其它 transport 可在 composition root 注入。
    ///
    /// @author kongweiguang
    pub fn new(local: Arc<dyn SessionTransportFactory>) -> Self {
        Self {
            providers: HashMap::from([(SessionTargetKind::Local, local)]),
        }
    }

    /// 注册指定协议的 provider；重复键采用后注册者，便于桌面切换明确配置。
    ///
    /// @author kongweiguang
    pub fn with_provider(
        mut self,
        kind: SessionTargetKind,
        provider: Arc<dyn SessionTransportFactory>,
    ) -> Self {
        self.providers.insert(kind, provider);
        self
    }
}

#[async_trait]
impl SessionTransportFactory for CompositeSessionTransportFactory {
    /// 只调用与显式目标匹配的 provider，失配时不尝试其它协议或静默换目标。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let kind = SessionTargetKind::from_target(target);
        let provider = self.providers.get(&kind).ok_or(AppError::unsupported(
            "当前会话目标没有已注册的 transport provider",
        ))?;
        provider.open(target, size, cancel).await
    }
}

/// 桌面、MCP、Agent 和运维操作共享的唯一 SessionService 实例。
#[derive(Clone)]
pub struct SessionManager {
    factory: Arc<dyn SessionTransportFactory>,
    runtime: tokio::runtime::Handle,
    config: SessionRuntimeConfig,
    sessions: Arc<StdMutex<SessionRegistry>>,
    lifecycle_gate: Arc<RwLock<()>>,
    shutdown_gate: Arc<Mutex<()>>,
    shutting_down: Arc<AtomicBool>,
    shutdown_complete: Arc<AtomicBool>,
    shutdown_cancel: CancellationToken,
    pending_creates: Arc<AtomicUsize>,
    pending_create_changed: Arc<Notify>,
    session_slots: Arc<Semaphore>,
    output_recorder: Option<Arc<dyn SessionOutputRecorder>>,
    pending_cleanup: Arc<Mutex<HashMap<SessionId, PendingCleanupOwner>>>,
    pending_cleanup_gate: Arc<Mutex<()>>,
    create_tasks: TaskTracker,
    auto_reconnect_enabled: Arc<AtomicBool>,
    auto_reconnect_policy: AutoReconnectPolicy,
}

/// live、creating、pending-cleanup 与已发布 ID 墓碑必须在同一短临界区内维护。
///
/// @author kongweiguang
#[derive(Default)]
struct SessionRegistry {
    live: HashMap<SessionId, Arc<ManagedSession>>,
    creating: HashSet<SessionId>,
    pending_cleanup: HashSet<SessionId>,
    /// 一旦发布就不复用 ID，避免旧 binding 与同 owner 的新会话发生 ABA。
    published_ids: HashSet<SessionId>,
}

/// 由 manager-owned 创建任务持有；发布成功或清理 owner 接管前不可释放 ID。
///
/// @author kongweiguang
struct SessionIdReservation {
    registry: Arc<StdMutex<SessionRegistry>>,
    session_id: SessionId,
    armed: AtomicBool,
}

impl SessionIdReservation {
    /// 在同一 registry 锁下把创建 reservation 原子转换为唯一 live owner，并留下永久墓碑。
    ///
    /// @author kongweiguang
    fn publish(&self, session: Arc<ManagedSession>) -> AppResult<()> {
        let mut registry = self
            .registry
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !registry.creating.contains(&self.session_id)
            || registry.live.contains_key(&self.session_id)
            || registry.pending_cleanup.contains(&self.session_id)
            || registry.published_ids.contains(&self.session_id)
        {
            return Err(AppError::conflict("终端会话 ID 已被占用"));
        }
        registry.creating.remove(&self.session_id);
        registry.published_ids.insert(self.session_id.clone());
        registry.live.insert(self.session_id.clone(), session);
        self.armed.store(false, Ordering::Release);
        Ok(())
    }
}

impl Drop for SessionIdReservation {
    /// future 失败或取消时释放创建标记；待清理 owner 通过 Arc 延长本 guard 生命周期。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if self.armed.swap(false, Ordering::AcqRel) {
            self.registry
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .creating
                .remove(&self.session_id);
        }
    }
}

/// 保留 runtime 与调用方两种取消来源；拥有或借用同一对 token，不创建子 token 或合并取消语义。
///
/// @author kongweiguang
struct SessionOpenCancellation<T> {
    runtime: T,
    caller: T,
}

/// 一次 transport open 的 session 身份、清理资源与取消源保持同一所有权包络。
///
/// @author kongweiguang
struct SessionOpenContext {
    session_id: SessionId,
    reservation: Option<Arc<SessionIdReservation>>,
    slot: Option<OwnedSemaphorePermit>,
    cancellations: SessionOpenCancellation<CancellationToken>,
}

impl SessionManager {
    /// 注入 transport provider、应用 runtime 和明确的资源预算；可直接包装为 `Arc<dyn SessionService>`。
    ///
    /// @author kongweiguang
    pub fn new(
        factory: Arc<dyn SessionTransportFactory>,
        runtime: tokio::runtime::Handle,
        config: SessionRuntimeConfig,
    ) -> Self {
        let maximum_sessions = config.maximum_sessions;
        Self {
            factory,
            runtime,
            config,
            sessions: Arc::new(StdMutex::new(SessionRegistry::default())),
            lifecycle_gate: Arc::new(RwLock::new(())),
            shutdown_gate: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown_complete: Arc::new(AtomicBool::new(false)),
            shutdown_cancel: CancellationToken::new(),
            pending_creates: Arc::new(AtomicUsize::new(0)),
            pending_create_changed: Arc::new(Notify::new()),
            session_slots: Arc::new(Semaphore::new(maximum_sessions)),
            output_recorder: None,
            pending_cleanup: Arc::new(Mutex::new(HashMap::new())),
            pending_cleanup_gate: Arc::new(Mutex::new(())),
            create_tasks: TaskTracker::new(),
            auto_reconnect_enabled: Arc::new(AtomicBool::new(false)),
            auto_reconnect_policy: AutoReconnectPolicy::default(),
        }
    }

    /// 注入有界原始输出 recorder；VT reader 只尝试入队，不等待日志磁盘写入。
    ///
    /// @author kongweiguang
    pub fn with_output_recorder(mut self, recorder: Arc<dyn SessionOutputRecorder>) -> Self {
        self.output_recorder = Some(recorder);
        self
    }

    /// 通过 factory 建立 transport，再由统一 attach 路径登记和启动 reader task。
    ///
    /// @author kongweiguang
    pub async fn create(&self, request: CreateSessionRequest) -> AppResult<SessionInfo> {
        self.create_cancellable(request, CancellationToken::new())
            .await
    }

    /// 将调用方取消与 manager shutdown 合并为本次创建的 token，并保证未发布资源被关闭。
    ///
    /// @author kongweiguang
    pub async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        caller_cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.create_with_transport_factory(request, Arc::clone(&self.factory), caller_cancellation)
            .await
    }

    /// 仅供内部恢复事务使用；显式 ID 与自动 ID 共享同一 live/creating/cleanup reservation gate。
    ///
    /// @author kongweiguang
    pub async fn create_with_session_id(
        &self,
        request: CreateSessionRequest,
        session_id: SessionId,
        caller_cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.create_with_transport_factory_and_id(
            request,
            Arc::clone(&self.factory),
            caller_cancellation,
            Some(session_id),
        )
        .await
    }

    /// 把单次 provider 与创建流程交给 manager-owned task，调用方取消或 drop 时仍保留清理 owner。
    ///
    /// 创建前先取得容量 permit，且在生命周期读门禁内登记任务；shutdown 写门禁通过后不会再有新任务越过扫描。
    /// 未确认的发布结果仍按原顺序关闭，保留 manager 对迟到 transport 的清理所有权。
    ///
    /// @author kongweiguang
    pub async fn create_with_transport_factory(
        &self,
        request: CreateSessionRequest,
        factory: Arc<dyn SessionTransportFactory>,
        caller_cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.create_with_transport_factory_and_id(request, factory, caller_cancellation, None)
            .await
    }

    /// 在 manager-owned task 启动前预留目标 ID；同一门禁也拒绝成功发布过的 ID。
    ///
    /// @author kongweiguang
    async fn create_with_transport_factory_and_id(
        &self,
        request: CreateSessionRequest,
        factory: Arc<dyn SessionTransportFactory>,
        caller_cancellation: CancellationToken,
        requested_session_id: Option<SessionId>,
    ) -> AppResult<SessionInfo> {
        if caller_cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端会话创建已取消"));
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(AppError::conflict("终端 runtime 正在关闭"));
        }
        TerminalEngine::validate_size(request.size, self.config.terminal_limits)
            .map_err(engine_error)?;
        let cancel = self.shutdown_cancel.child_token();
        let pending = self.register_pending_creation(cancel.clone()).await?;
        let session_id = requested_session_id.unwrap_or_default();
        let reservation = self.reserve_session_id(session_id.clone())?;
        let cleanup_deadline = Instant::now() + self.config.close_wait;
        tokio::select! {
            biased;
            _ = caller_cancellation.cancelled() => return Err(AppError::cancelled("终端会话创建已取消")),
            _ = cancel.cancelled() => return Err(AppError::conflict("终端 runtime 正在关闭")),
            result = self.retry_all_pending_cleanup(cleanup_deadline) => result?,
        }
        let permit = Arc::clone(&self.session_slots)
            .try_acquire_owned()
            .map_err(|_| AppError::conflict("终端会话数量已达到上限"))?;
        let (mut result_sender, mut result_receiver) = oneshot::channel();
        let (ack_sender, ack_receiver) = oneshot::channel();
        let manager = self.clone();
        let worker_token = caller_cancellation.child_token();
        let open_context = SessionOpenContext {
            session_id,
            reservation: Some(reservation),
            slot: Some(permit),
            cancellations: SessionOpenCancellation {
                runtime: cancel,
                caller: worker_token.clone(),
            },
        };
        let _publication = self.lifecycle_gate.read().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(AppError::conflict("终端 runtime 正在关闭"));
        }
        self.create_tasks.spawn_on(async move {
            let mut operation =
                Box::pin(manager.create_with_permit(request, factory, open_context, pending));
            let result = tokio::select! {
                biased;
                result = &mut operation => result,
                _ = result_sender.closed() => {
                    worker_token.cancel();
                    operation.await
                }
            };
            let abandoned_session = result
                .as_ref()
                .ok()
                .map(|info| info.session_id.clone());
            match result_sender.send(result) {
                Ok(()) => {
                    let acknowledged = tokio::select! {
                        biased;
                        result = ack_receiver => result.is_ok(),
                        _ = manager.shutdown_cancel.cancelled() => false,
                    };
                    if !acknowledged
                        && let Some(session_id) = abandoned_session
                        && let Err(error) = manager
                            .close_until(
                                session_id.clone(),
                                Instant::now() + manager.config.close_wait,
                            )
                            .await
                    {
                        tracing::error!(
                            session_id = %session_id,
                            error = %error,
                            "terminal create response was abandoned and cleanup remains pending"
                        );
                    }
                }
                Err(Ok(info)) => {
                    if let Err(error) = manager
                        .close_until(info.session_id, Instant::now() + manager.config.close_wait)
                        .await
                    {
                        tracing::error!(error = %error, "abandoned terminal create could not be closed");
                    }
                }
                Err(Err(error)) => {
                    tracing::debug!(error = %error, "terminal create caller dropped before result");
                }
            }
        }, &self.runtime);
        drop(_publication);

        tokio::select! {
            biased;
            result = &mut result_receiver => {
                let result = result.map_err(|_| AppError::infrastructure("终端创建任务意外结束"))?;
                let _ = ack_sender.send(());
                result
            }
            _ = caller_cancellation.cancelled() => Err(AppError::cancelled("终端会话创建已取消")),
        }
    }

    /// 检查 live、creating 与 pending cleanup 并一次登记 ID；锁不跨越 provider 或异步调用。
    ///
    /// @author kongweiguang
    fn reserve_session_id(&self, session_id: SessionId) -> AppResult<Arc<SessionIdReservation>> {
        let mut registry = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registry.live.contains_key(&session_id)
            || registry.creating.contains(&session_id)
            || registry.pending_cleanup.contains(&session_id)
            || registry.published_ids.contains(&session_id)
        {
            return Err(AppError::conflict("终端会话 ID 已被占用或仍待清理"));
        }
        registry.creating.insert(session_id.clone());
        Ok(Arc::new(SessionIdReservation {
            registry: Arc::clone(&self.sessions),
            session_id,
            armed: AtomicBool::new(true),
        }))
    }

    /// 在 manager-owned task 中传递同一 open context，避免拆散 ID、permit 与取消/清理 owner。
    ///
    /// @author kongweiguang
    async fn create_with_permit(
        &self,
        request: CreateSessionRequest,
        factory: Arc<dyn SessionTransportFactory>,
        open_context: SessionOpenContext,
        mut pending: PendingCreationGuard,
    ) -> AppResult<SessionInfo> {
        let (transport, open_context) = self
            .open_transport_cancellable(
                factory.as_ref(),
                &request.target,
                request.size,
                open_context,
            )
            .await?;
        let result = self
            .attach_with_permit(
                AttachTransportRequest {
                    target: request.target,
                    owner: request.owner,
                    size: request.size,
                },
                transport,
                open_context,
            )
            .await;
        if result.is_ok() {
            pending.disarm();
        }
        result
    }

    /// 清理创建阶段的半成品；失败时把控制句柄与容量 slot 留在 manager 内供全局 shutdown 重试。
    ///
    /// @author kongweiguang
    async fn cleanup_unpublished_transport(
        &self,
        session_id: SessionId,
        control: Arc<dyn SessionTransportControl>,
        slot: Option<OwnedSemaphorePermit>,
        primary: AppError,
    ) -> AppError {
        self.cleanup_unpublished_transport_with_reservation(
            session_id, control, slot, primary, None,
        )
        .await
    }

    /// 未发布 transport 清理失败时，把创建 ID guard 转交给 manager cleanup owner。
    ///
    /// @author kongweiguang
    async fn cleanup_unpublished_transport_with_reservation(
        &self,
        session_id: SessionId,
        control: Arc<dyn SessionTransportControl>,
        slot: Option<OwnedSemaphorePermit>,
        primary: AppError,
        reservation: Option<Arc<SessionIdReservation>>,
    ) -> AppError {
        let deadline = Instant::now() + self.config.close_wait;
        match close_unpublished_transport(&control, deadline).await {
            Ok(()) => primary,
            Err(cleanup) => {
                tracing::error!(session_id = %session_id, error = %cleanup, "unpublished terminal transport cleanup remains pending");
                self.mark_pending_cleanup(&session_id);
                self.pending_cleanup.lock().await.insert(
                    session_id,
                    PendingCleanupOwner::Transport {
                        control,
                        _slot: slot,
                        identity: Arc::new(()),
                        _id_reservation: reservation,
                    },
                );
                AppError::infrastructure_with_source(
                    "终端操作失败，资源仍由 runtime 回收",
                    SessionCleanupPending {
                        primary_code: primary.public_code(),
                        cleanup_code: cleanup.public_code(),
                    },
                )
            }
        }
    }

    /// 关闭 factory 未交付的资源；失败时把 owner 与本次 slot 一起登记，等待显式重试或 shutdown。
    ///
    /// @author kongweiguang
    async fn cleanup_failed_open(
        &self,
        session_id: SessionId,
        open_error: SessionTransportOpenError,
        slot: Option<OwnedSemaphorePermit>,
        cancellation_error: Option<AppError>,
        reservation: Option<Arc<SessionIdReservation>>,
    ) -> AppError {
        let (provider_error, cleanup_owner) = open_error.into_parts();
        let primary = cancellation_error
            .map(|error| error.with_source(provider_error.clone()))
            .unwrap_or(provider_error);
        let Some(owner) = cleanup_owner else {
            return primary;
        };

        let deadline = Instant::now() + self.config.close_wait;
        let cleanup_result = tokio::time::timeout_at(deadline, owner.close_until(deadline)).await;
        let cleanup_error = match cleanup_result {
            Ok(Ok(())) => return primary,
            Ok(Err(error)) => error,
            Err(_) => AppError::timeout("回收终端 provider 未交付资源"),
        };
        tracing::error!(session_id = %session_id, error = %cleanup_error, "failed-open resource cleanup remains pending");
        self.mark_pending_cleanup(&session_id);
        self.pending_cleanup.lock().await.insert(
            session_id,
            PendingCleanupOwner::Open {
                owner,
                _slot: slot,
                identity: Arc::new(()),
                _id_reservation: reservation,
            },
        );
        AppError::infrastructure_with_source(
            "终端会话创建失败，资源仍由 runtime 回收",
            OpenCleanupFailure {
                primary,
                cleanup: cleanup_error,
            },
        )
    }

    /// 清理已启动 reader 的未发布 session；失败时保留整个 ManagedSession 与其容量 permit。
    ///
    /// @author kongweiguang
    async fn cleanup_unpublished_session(
        &self,
        session: Arc<ManagedSession>,
        primary: AppError,
        reservation: Option<Arc<SessionIdReservation>>,
    ) -> AppError {
        let session_id = session.session_id.clone();
        let deadline = Instant::now() + self.config.close_wait;
        match close_managed_session(Arc::clone(&session), deadline, None, None).await {
            Ok(_) => primary,
            Err(cleanup) => {
                tracing::error!(session_id = %session_id, error = %cleanup, "unpublished terminal session cleanup remains pending");
                self.mark_pending_cleanup(&session_id);
                self.pending_cleanup.lock().await.insert(
                    session_id,
                    PendingCleanupOwner::Session {
                        session,
                        identity: Arc::new(()),
                        _id_reservation: reservation,
                    },
                );
                AppError::infrastructure_with_source(
                    "终端操作失败，资源仍由 runtime 回收",
                    SessionCleanupPending {
                        primary_code: primary.public_code(),
                        cleanup_code: cleanup.public_code(),
                    },
                )
            }
        }
    }

    /// 清理 owner 登记前先封锁 ID，避免同一 key 在异步 map 写入间隙被重新创建。
    ///
    /// @author kongweiguang
    fn mark_pending_cleanup(&self, session_id: &SessionId) {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pending_cleanup
            .insert(session_id.clone());
    }

    /// 仅在 cleanup map 已按 owner identity 移除条目后解除 ID 冲突屏障。
    ///
    /// @author kongweiguang
    fn clear_pending_cleanup(&self, session_id: &SessionId) {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .pending_cleanup
            .remove(session_id);
    }

    /// 对已有待清理 owner 做一次有界重试；仍失败时保留 owner，不把失败条目从 registry 丢掉。
    ///
    /// @author kongweiguang
    async fn retry_pending_cleanup(
        &self,
        session_id: &SessionId,
        deadline: Instant,
    ) -> AppResult<()> {
        let _gate = tokio::time::timeout_at(deadline, self.pending_cleanup_gate.lock())
            .await
            .map_err(|_| AppError::timeout("等待终端待清理资源锁"))?;
        let attempt = self
            .pending_cleanup
            .lock()
            .await
            .get(session_id)
            .map(PendingCleanupOwner::attempt);
        let Some(attempt) = attempt else {
            return Ok(());
        };
        let identity = Arc::clone(&attempt.identity);
        match attempt.cleanup(deadline).await {
            Ok(()) => {
                let mut pending = self.pending_cleanup.lock().await;
                let removed = if pending
                    .get(session_id)
                    .is_some_and(|owner| Arc::ptr_eq(owner.identity(), &identity))
                {
                    pending.remove(session_id);
                    true
                } else {
                    false
                };
                drop(pending);
                if removed {
                    self.clear_pending_cleanup(session_id);
                }
                Ok(())
            }
            Err(error) => Err(AppError::infrastructure_with_source(
                "终端资源仍在回收，请在 shutdown 重试",
                SessionCleanupPending {
                    primary_code: AppError::infrastructure("终端操作等待清理").public_code(),
                    cleanup_code: error.public_code(),
                },
            )),
        }
    }

    /// 新建会话前并发重试全部已知 owner 一次；仍未释放的 slot 会阻止新的资源创建。
    ///
    /// @author kongweiguang
    async fn retry_all_pending_cleanup(&self, deadline: Instant) -> AppResult<()> {
        let _gate = tokio::time::timeout_at(deadline, self.pending_cleanup_gate.lock())
            .await
            .map_err(|_| AppError::timeout("等待终端待清理资源锁"))?;
        let attempts = self
            .pending_cleanup
            .lock()
            .await
            .iter()
            .map(|(session_id, owner)| (session_id.clone(), owner.attempt()))
            .collect::<Vec<_>>();
        if attempts.is_empty() {
            return Ok(());
        }

        let mut cleanups = tokio::task::JoinSet::new();
        for (session_id, attempt) in attempts {
            cleanups.spawn(async move {
                let identity = Arc::clone(&attempt.identity);
                (session_id, identity, attempt.cleanup(deadline).await)
            });
        }

        let mut failures = Vec::new();
        while !cleanups.is_empty() {
            let joined = tokio::time::timeout_at(deadline, cleanups.join_next()).await;
            match joined {
                Ok(Some(Ok((session_id, identity, Ok(()))))) => {
                    let mut pending = self.pending_cleanup.lock().await;
                    let removed = if pending
                        .get(&session_id)
                        .is_some_and(|owner| Arc::ptr_eq(owner.identity(), &identity))
                    {
                        pending.remove(&session_id);
                        true
                    } else {
                        false
                    };
                    drop(pending);
                    if removed {
                        self.clear_pending_cleanup(&session_id);
                    }
                }
                Ok(Some(Ok((session_id, _identity, Err(error))))) => {
                    tracing::error!(session_id = %session_id, error = %error, "pending terminal cleanup retry failed before create");
                    failures.push(error);
                }
                Ok(Some(Err(error))) => {
                    tracing::error!(error = %error, "pending terminal cleanup retry task failed before create");
                    failures.push(AppError::infrastructure_with_source(
                        "终端待清理任务失败",
                        error,
                    ));
                }
                Ok(None) => break,
                Err(_) => {
                    cleanups.abort_all();
                    while cleanups.join_next().await.is_some() {}
                    failures.push(AppError::timeout("创建前回收待清理终端资源"));
                    break;
                }
            }
        }

        if failures.is_empty() {
            return Ok(());
        }
        let pending_count = self.pending_cleanup.lock().await.len();
        tracing::error!(
            pending_count,
            "terminal create blocked by pending cleanup owners"
        );
        let first_failure = failures.remove(0);
        Err(AppError::infrastructure_with_source(
            "终端仍有资源待回收，本次创建未开始",
            PendingCleanupRetryFailure {
                pending_count,
                failures,
                first_failure,
            },
        ))
    }

    /// 在关闭门禁与容量 permit 已确认后接入 transport，并原子地把 context reservation 转成 live；
    /// 若内部上下文缺少必需 owner，先清理已打开的 transport，再返回固定错误而不 panic。
    ///
    /// @author kongweiguang
    async fn attach_with_permit(
        &self,
        request: AttachTransportRequest,
        transport: SessionTransport,
        mut open_context: SessionOpenContext,
    ) -> AppResult<SessionInfo> {
        let session_id = open_context.session_id.clone();
        let reservation = match open_context.reservation.clone() {
            Some(reservation) => reservation,
            None => {
                return Err(self
                    .cleanup_unpublished_transport_with_reservation(
                        session_id,
                        Arc::clone(&transport.control),
                        open_context.slot.take(),
                        AppError::infrastructure("终端创建上下文缺少 ID reservation"),
                        None,
                    )
                    .await);
            }
        };
        let permit = match open_context.slot.take() {
            Some(permit) => permit,
            None => {
                return Err(self
                    .cleanup_unpublished_transport_with_reservation(
                        session_id,
                        Arc::clone(&transport.control),
                        None,
                        AppError::infrastructure("终端创建上下文缺少 capacity permit"),
                        Some(reservation),
                    )
                    .await);
            }
        };
        let cancellation = open_context.cancellations.runtime.clone();
        let caller_cancellation = open_context.cancellations.caller.clone();
        if cancellation.is_cancelled()
            || caller_cancellation.is_cancelled()
            || self.shutting_down.load(Ordering::Acquire)
        {
            let primary = if caller_cancellation.is_cancelled() {
                AppError::cancelled("终端会话创建已取消")
            } else {
                AppError::conflict("终端 runtime 正在关闭")
            };
            cancellation.cancel();
            return Err(self
                .cleanup_unpublished_transport_with_reservation(
                    session_id,
                    Arc::clone(&transport.control),
                    Some(permit),
                    primary,
                    Some(Arc::clone(&reservation)),
                )
                .await);
        }
        let resize_policy = match transport.control.terminal_resize_capability() {
            TerminalResizeCapability::Generic => TerminalResizePolicy::Generic,
            TerminalResizeCapability::WindowsConPty => TerminalResizePolicy::ConPty,
        };
        let engine = match TerminalEngine::new_with_resize_policy(
            session_id.clone(),
            request.size,
            self.config.terminal_limits,
            resize_policy,
        ) {
            Ok(engine) => engine,
            Err(error) => {
                return Err(self
                    .cleanup_unpublished_transport_with_reservation(
                        session_id,
                        Arc::clone(&transport.control),
                        Some(permit),
                        engine_error(error),
                        Some(Arc::clone(&reservation)),
                    )
                    .await);
            }
        };
        let resized = tokio::select! {
            _ = caller_cancellation.cancelled() => {
                cancellation.cancel();
                return Err(self.cleanup_unpublished_transport_with_reservation(
                    session_id,
                    Arc::clone(&transport.control),
                    Some(permit),
                    AppError::cancelled("终端会话创建已取消"),
                    Some(Arc::clone(&reservation)),
                ).await);
            }
            _ = cancellation.cancelled() => {
                return Err(self.cleanup_unpublished_transport_with_reservation(
                    session_id,
                    Arc::clone(&transport.control),
                    Some(permit),
                    AppError::conflict("终端 runtime 正在关闭"),
                    Some(Arc::clone(&reservation)),
                ).await);
            }
            result = transport.control.resize(request.size) => result,
        };
        if let Err(error) = resized {
            return Err(self
                .cleanup_unpublished_transport_with_reservation(
                    session_id,
                    Arc::clone(&transport.control),
                    Some(permit),
                    error,
                    Some(Arc::clone(&reservation)),
                )
                .await);
        }

        let info = SessionInfo {
            session_id: session_id.clone(),
            target: request.target,
            owner: request.owner,
            size: request.size,
            lifecycle: SessionLifecycle::Running,
            exit_status: None,
        };
        let (changes, receiver) = watch::channel(SessionChange {
            revision: 0,
            kind: SessionChangeKind::Lifecycle,
        });
        drop(receiver);
        let (shell_events, unused_shell_receiver) = broadcast::channel(SHELL_EVENT_CAPACITY);
        drop(unused_shell_receiver);
        let shell_dialect = transport.shell_dialect;
        let process_id = transport.process_id;
        let shell_integration_enabled = transport.shell_integration_enabled;
        let session = Arc::new(ManagedSession {
            session_id: session_id.clone(),
            transport_generation: AtomicU64::new(1),
            retiring_transport_generation: Mutex::new(None),
            resize_diagnostic_session_sequence: NEXT_RESIZE_DIAGNOSTIC_SESSION_SEQUENCE
                .fetch_add(1, Ordering::Relaxed),
            resize_diagnostic_generation: AtomicU64::new(0),
            resize_diagnostic_cpr_generation: AtomicU64::new(0),
            resize_diagnostic_reader_budget: AtomicU64::new(0),
            native_exit_deadline: Mutex::new(None),
            process_id: RwLock::new(process_id),
            shell_integration_enabled: RwLock::new(shell_integration_enabled),
            state: Mutex::new(SessionState {
                engine,
                info: info.clone(),
                close_history_proof: None,
            }),
            writer: Mutex::new(transport.writer),
            writer_shutdown: AtomicBool::new(false),
            control: RwLock::new(Arc::clone(&transport.control)),
            cancellation: Mutex::new(cancellation.clone()),
            changes,
            input_gate: Mutex::new(()),
            shell_integration: Mutex::new(ShellIntegrationState::default()),
            shell_events,
            shell_dialect: RwLock::new(shell_dialect),
            operation_gate: Mutex::new(()),
            child_exit_task: Mutex::new(None),
            reader_task: Mutex::new(None),
            output_sequence: AtomicU64::new(0),
            auto_reconnect: Mutex::new(AutoReconnectRuntime::default()),
            auto_reconnect_changed: Notify::new(),
            _slot: permit,
        });
        let generation = session.transport_generation.load(Ordering::Acquire);
        let child_exit_task = self.spawn_child_exit_observer(
            Arc::clone(&session),
            Arc::clone(&transport.control),
            generation,
        );
        *session.child_exit_task.lock().await = Some(child_exit_task);
        let reader_task = self.spawn_reader(
            Arc::clone(&session),
            transport.reader,
            cancellation.clone(),
            Arc::clone(&transport.control),
            generation,
        );
        *session.reader_task.lock().await = Some(reader_task);
        let publication = tokio::select! {
            _ = caller_cancellation.cancelled() => None,
            _ = cancellation.cancelled() => None,
            guard = self.lifecycle_gate.read() => Some(guard),
        };
        let Some(_publication) = publication else {
            cancellation.cancel();
            let primary = if caller_cancellation.is_cancelled() {
                AppError::cancelled("终端会话创建已取消")
            } else {
                AppError::conflict("终端 runtime 正在关闭")
            };
            return Err(self
                .cleanup_unpublished_session(session, primary, Some(Arc::clone(&reservation)))
                .await);
        };
        if self.shutting_down.load(Ordering::Acquire)
            || cancellation.is_cancelled()
            || caller_cancellation.is_cancelled()
        {
            drop(_publication);
            cancellation.cancel();
            let primary = if caller_cancellation.is_cancelled() {
                AppError::cancelled("终端会话创建已取消")
            } else {
                AppError::conflict("终端 runtime 正在关闭")
            };
            return Err(self
                .cleanup_unpublished_session(session, primary, Some(Arc::clone(&reservation)))
                .await);
        }
        if self.shutting_down.load(Ordering::Acquire)
            || cancellation.is_cancelled()
            || caller_cancellation.is_cancelled()
        {
            drop(_publication);
            cancellation.cancel();
            let primary = if caller_cancellation.is_cancelled() {
                AppError::cancelled("终端会话创建已取消")
            } else {
                AppError::conflict("终端 runtime 正在关闭")
            };
            return Err(self
                .cleanup_unpublished_session(session, primary, Some(Arc::clone(&reservation)))
                .await);
        }
        if let Err(error) = reservation.publish(Arc::clone(&session)) {
            drop(_publication);
            cancellation.cancel();
            return Err(self
                .cleanup_unpublished_session(session, error, Some(reservation))
                .await);
        }
        drop(_publication);
        Ok(session.state.lock().await.info.clone())
    }

    /// 仅在短生命周期门禁内登记正在创建的操作，让 shutdown 可以随后取消并等待它们收尾。
    ///
    /// @author kongweiguang
    async fn register_pending_creation(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<PendingCreationGuard> {
        let _publication = self.lifecycle_gate.read().await;
        if self.shutting_down.load(Ordering::Acquire) || cancellation.is_cancelled() {
            return Err(AppError::conflict("终端 runtime 正在关闭"));
        }
        self.pending_creates.fetch_add(1, Ordering::AcqRel);
        if self.shutting_down.load(Ordering::Acquire) || cancellation.is_cancelled() {
            self.pending_creates.fetch_sub(1, Ordering::AcqRel);
            self.pending_create_changed.notify_waiters();
            return Err(AppError::conflict("终端 runtime 正在关闭"));
        }
        Ok(PendingCreationGuard {
            manager: self.clone(),
            cancellation,
            armed: true,
        })
    }

    /// 保留 caller 优先的取消判定；open context 让清理 owner 与 ID/permit 同步转移。
    ///
    /// @author kongweiguang
    async fn open_transport_cancellable(
        &self,
        factory: &dyn SessionTransportFactory,
        target: &SessionTarget,
        size: TerminalSize,
        mut open_context: SessionOpenContext,
    ) -> AppResult<(SessionTransport, SessionOpenContext)> {
        let cancellation = open_context.cancellations.runtime.clone();
        let caller_cancellation = open_context.cancellations.caller.clone();
        let session_id = open_context.session_id.clone();
        let reservation = open_context.reservation.clone();
        let mut slot = open_context.slot.take();
        if caller_cancellation.is_cancelled() {
            cancellation.cancel();
            return Err(AppError::cancelled("终端会话创建已取消"));
        }
        if cancellation.is_cancelled() {
            return Err(AppError::conflict("终端 runtime 正在关闭"));
        }
        let provider_cancellation = CancellationToken::new();
        let mut opening = Box::pin(factory.open(target, size, provider_cancellation.clone()));
        let cancelled = tokio::select! {
            biased;
            result = &mut opening => {
                return match result {
                    Ok(transport) if !caller_cancellation.is_cancelled() && !cancellation.is_cancelled() => {
                        open_context.slot = slot.take();
                        Ok((transport, open_context))
                    }
                    Ok(transport) => {
                        let primary = if caller_cancellation.is_cancelled() {
                            AppError::cancelled("终端会话创建已取消")
                        } else {
                            AppError::conflict("终端 runtime 正在关闭")
                        };
                        cancellation.cancel();
                        Err(self.cleanup_unpublished_transport_with_reservation(
                            session_id.clone(),
                            Arc::clone(&transport.control),
                            slot.take(),
                            primary,
                            reservation.clone(),
                        ).await)
                    }
                    Err(error) if caller_cancellation.is_cancelled() => Err(self.cleanup_failed_open(
                        session_id.clone(),
                        error,
                        slot.take(),
                        Some(AppError::cancelled("终端会话创建已取消")),
                        reservation.clone(),
                    ).await),
                    Err(error) if cancellation.is_cancelled() => Err(self.cleanup_failed_open(
                        session_id.clone(),
                        error,
                        slot.take(),
                        Some(AppError::conflict("终端 runtime 正在关闭")),
                        reservation.clone(),
                    ).await),
                    Err(error) => Err(self.cleanup_failed_open(
                        session_id.clone(), error, slot.take(), None, reservation.clone(),
                    ).await),
                };
            }
            _ = caller_cancellation.cancelled() => AppError::cancelled("终端会话创建已取消"),
            _ = cancellation.cancelled() => AppError::conflict("终端 runtime 正在关闭"),
        };
        cancellation.cancel();
        provider_cancellation.cancel();
        match opening.await {
            Ok(transport) => Err(self
                .cleanup_unpublished_transport_with_reservation(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    slot.take(),
                    cancelled,
                    reservation.clone(),
                )
                .await),
            Err(error) => Err(self
                .cleanup_failed_open(
                    session_id.clone(),
                    error,
                    slot.take(),
                    Some(cancelled),
                    reservation,
                )
                .await),
        }
    }

    /// 以 Notify 避免轮询，在统一 shutdown deadline 内等待已登记的创建流程释放资源。
    ///
    /// @author kongweiguang
    async fn wait_for_pending_creates(&self, deadline: Instant) -> bool {
        loop {
            let changed = self.pending_create_changed.notified();
            if self.pending_creates.load(Ordering::Acquire) == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.pending_creates.load(Ordering::Acquire) == 0;
            }
        }
    }

    /// 先复制 live owner 再取消 supervisor；同步 registry 锁不跨越任何异步等待。
    ///
    /// @author kongweiguang
    async fn update_auto_reconnect_setting(&self, enabled: bool) -> AppResult<()> {
        self.auto_reconnect_enabled
            .store(enabled, Ordering::Release);
        if enabled {
            return Ok(());
        }

        let sessions = {
            let registry = self
                .sessions
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            registry.live.values().cloned().collect::<Vec<_>>()
        };
        for session in sessions {
            let state = session.auto_reconnect.lock().await;
            if let Some(cancellation) = &state.cancellation {
                cancellation.cancel();
            }
            drop(state);
            session.auto_reconnect_changed.notify_one();
        }
        Ok(())
    }

    /// 只把实际 wait_exit 的 typed disconnect 交给每条 session 唯一持有的 retry supervisor。
    ///
    /// @author kongweiguang
    async fn schedule_auto_reconnect(&self, session: Arc<ManagedSession>, failure: AppError) {
        if !self.auto_reconnect_enabled.load(Ordering::Acquire)
            || self.shutting_down.load(Ordering::Acquire)
            || !failure.is_retryable_remote_disconnect()
        {
            return;
        }

        let mut runtime = session.auto_reconnect.lock().await;
        runtime.pending_failure = Some(failure);
        if !runtime
            .task
            .as_ref()
            .is_some_and(|task| !task.is_finished())
        {
            let cancellation = self.shutdown_cancel.child_token();
            runtime.cancellation = Some(cancellation.clone());
            let manager = self.clone();
            let task_session = Arc::clone(&session);
            runtime.task = Some(self.runtime.spawn(async move {
                manager
                    .run_auto_reconnect_supervisor(task_session, cancellation)
                    .await;
            }));
        }
        drop(runtime);
        session.auto_reconnect_changed.notify_one();
    }

    /// 在固定退避预算内重连同一 UI 交互式 SSH session，并保留跨快速断线的尝试计数。
    ///
    /// @author kongweiguang
    async fn run_auto_reconnect_supervisor(
        &self,
        session: Arc<ManagedSession>,
        cancellation: CancellationToken,
    ) {
        'supervisor: loop {
            loop {
                if cancellation.is_cancelled() || self.shutting_down.load(Ordering::Acquire) {
                    break;
                }
                let pending = session.auto_reconnect.lock().await.pending_failure.take();
                let Some(failure) = pending else {
                    let changed = session.auto_reconnect_changed.notified();
                    tokio::pin!(changed);
                    if session
                        .auto_reconnect
                        .lock()
                        .await
                        .pending_failure
                        .is_some()
                    {
                        continue;
                    }
                    tokio::select! {
                        _ = cancellation.cancelled() => break,
                        _ = changed => continue,
                    }
                };

                let (info, generation, command_active) = {
                    let state = session.state.lock().await;
                    let shell = session.shell_integration.lock().await;
                    (
                        state.info.clone(),
                        shell.session_generation,
                        shell.active_command.is_some(),
                    )
                };
                if info.lifecycle != SessionLifecycle::Failed {
                    tracing::debug!(
                        session_id = %session.session_id,
                        lifecycle = ?info.lifecycle,
                        "automatic reconnect ignored a stale failure"
                    );
                    break;
                }
                if command_active {
                    tracing::debug!(
                        session_id = %session.session_id,
                        "automatic reconnect suppressed while a managed shell command is active"
                    );
                    break;
                }
                let attempts_started = session.auto_reconnect.lock().await.attempts_started;
                let decision = self.auto_reconnect_policy.decide(
                    self.auto_reconnect_enabled.load(Ordering::Acquire),
                    AutoReconnectContext {
                        owner: &info.owner,
                        target: &info.target,
                        failure: &failure,
                        manually_closed: false,
                        exit_status: info.exit_status.as_ref(),
                        attempts_started,
                        session_generation: generation,
                    },
                );
                let plan = match decision {
                    AutoReconnectDecision::Retry(plan) => plan,
                    AutoReconnectDecision::Suppressed(reason) => {
                        tracing::debug!(
                            session_id = %session.session_id,
                            ?reason,
                            "automatic reconnect suppressed"
                        );
                        break;
                    }
                };
                session.auto_reconnect.lock().await.attempts_started = plan.attempt;

                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(plan.delay) => {}
                }
                let (current_lifecycle, current_generation) = {
                    let state = session.state.lock().await;
                    let shell = session.shell_integration.lock().await;
                    (state.info.lifecycle, shell.session_generation)
                };
                let plan_is_current = self
                    .auto_reconnect_policy
                    .validate_plan(
                        plan,
                        self.auto_reconnect_enabled.load(Ordering::Acquire),
                        cancellation.is_cancelled() || self.shutting_down.load(Ordering::Acquire),
                        current_generation,
                    )
                    .is_ok()
                    && current_lifecycle == SessionLifecycle::Failed;
                if !plan_is_current {
                    break;
                }

                match self
                    .recover_session_cancellable_mode(
                        session.session_id.clone(),
                        cancellation.clone(),
                        SessionRecoveryMode::AutomaticReconnect,
                    )
                    .await
                {
                    Ok(_) => {
                        let connected_generation = {
                            let shell = session.shell_integration.lock().await;
                            shell.session_generation
                        };
                        tokio::select! {
                            _ = cancellation.cancelled() => break,
                            _ = session.auto_reconnect_changed.notified() => continue,
                            _ = tokio::time::sleep(AUTO_RECONNECT_STABILITY_WINDOW) => {
                                let (lifecycle, generation) = {
                                    let state = session.state.lock().await;
                                    let shell = session.shell_integration.lock().await;
                                    (state.info.lifecycle, shell.session_generation)
                                };
                                if lifecycle == SessionLifecycle::Running
                                    && generation == connected_generation
                                {
                                    let mut runtime = session.auto_reconnect.lock().await;
                                    if runtime.pending_failure.is_none() {
                                        runtime.attempts_started = 0;
                                        break;
                                    }
                                } else if lifecycle != SessionLifecycle::Failed {
                                    break;
                                }
                            }
                        }
                    }
                    Err(error) if error.is_retryable_remote_disconnect() => {
                        session.auto_reconnect.lock().await.pending_failure = Some(error);
                        session.auto_reconnect_changed.notify_one();
                    }
                    Err(error) => {
                        tracing::debug!(
                            session_id = %session.session_id,
                            error = %error,
                            "automatic reconnect stopped after a non-retryable failure"
                        );
                        break;
                    }
                }
            }

            let session_failed =
                session.state.lock().await.info.lifecycle == SessionLifecycle::Failed;
            let mut runtime = session.auto_reconnect.lock().await;
            let has_retry_to_process = session_failed
                && self.auto_reconnect_enabled.load(Ordering::Acquire)
                && !self.shutting_down.load(Ordering::Acquire)
                && runtime.pending_failure.is_some();
            if has_retry_to_process {
                drop(runtime);
                session.auto_reconnect_changed.notify_one();
                continue 'supervisor;
            }
            runtime.pending_failure = None;
            runtime.cancellation = None;
            runtime.task = None;
            drop(runtime);
            session.auto_reconnect_changed.notify_one();
            return;
        }
    }

    /// 取消并 join 当前重连 supervisor；超时保留 task owner 供下一次 close 重试。
    ///
    /// @author kongweiguang
    async fn cancel_auto_reconnect_until(
        session: &ManagedSession,
        deadline: Instant,
    ) -> AppResult<()> {
        {
            let runtime = tokio::time::timeout_at(deadline, session.auto_reconnect.lock())
                .await
                .map_err(|_| AppError::timeout("等待自动重连状态锁"))?;
            if let Some(cancellation) = &runtime.cancellation {
                cancellation.cancel();
            }
            session.auto_reconnect_changed.notify_one();
        }

        loop {
            let changed = session.auto_reconnect_changed.notified();
            tokio::pin!(changed);
            let finished = {
                let mut runtime = tokio::time::timeout_at(deadline, session.auto_reconnect.lock())
                    .await
                    .map_err(|_| AppError::timeout("等待自动重连清理锁"))?;
                if let Some(cancellation) = &runtime.cancellation {
                    cancellation.cancel();
                }
                let has_task = runtime.task.is_some();
                let task_finished = runtime.task.as_ref().is_some_and(|task| task.is_finished());
                match (has_task, task_finished) {
                    (true, true) => {
                        runtime.cancellation = None;
                        runtime.pending_failure = None;
                        runtime.task.take()
                    }
                    (true, false) => None,
                    (false, _) => {
                        runtime.cancellation = None;
                        runtime.pending_failure = None;
                        return Ok(());
                    }
                }
            };
            if let Some(task) = finished {
                tokio::time::timeout_at(deadline, task)
                    .await
                    .map_err(|_| AppError::timeout("等待自动重连 task join"))?
                    .map_err(|error| {
                        AppError::infrastructure_with_source("自动重连 task 失败", error)
                    })?;
                return Ok(());
            }
            tokio::time::timeout_at(deadline, changed)
                .await
                .map_err(|_| AppError::timeout("等待自动重连取消"))?;
        }
    }

    /// 在注入的 Tokio runtime 上消费有界 PTY 输出；日志、解析、响应与关闭仍顺序处理，条件合并不截断 reader。
    ///
    /// @author kongweiguang
    fn spawn_reader(
        &self,
        session: Arc<ManagedSession>,
        mut reader: std::pin::Pin<Box<dyn AsyncRead + Send + 'static>>,
        cancellation: CancellationToken,
        control: Arc<dyn SessionTransportControl>,
        transport_generation: u64,
    ) -> JoinHandle<()> {
        let maximum_chunk = self.config.read_chunk_bytes.clamp(1, 1024 * 1024);
        let close_wait = self.config.close_wait;
        let output_recorder = self.output_recorder.clone();
        let manager = self.clone();
        self.runtime.spawn(async move {
            let mut buffer = vec![0_u8; maximum_chunk];
            loop {
                let read = tokio::select! {
                    _ = cancellation.cancelled() => break,
                    result = reader.read(&mut buffer) => result,
                };
                match read {
                    Ok(0) => {
                        if let Some(error) = mark_reader_exit(
                            &session,
                            control.as_ref(),
                            transport_generation,
                            close_wait,
                        )
                        .await
                        {
                            tracing::debug!(
                                session_id = %session.session_id,
                                error = %error,
                                "terminal reader ended without a confirmed exit status"
                            );
                            let lifecycle = session.state.lock().await.info.lifecycle;
                            if lifecycle != SessionLifecycle::Exited {
                                manager
                                    .schedule_auto_reconnect(Arc::clone(&session), error)
                                    .await;
                            }
                        }
                        break;
                    }
                    Err(error) => {
                        mark_reader_failure(&session, transport_generation, error).await;
                        break;
                    }
                    Ok(read) => {
                        let sequence = session
                            .output_sequence
                            .fetch_add(1, Ordering::Relaxed)
                            .wrapping_add(1);
                        if let Some(recorder) = &output_recorder
                            && let OutputRecordResult::Failed {
                                first_missing_sequence,
                                reason,
                            } = recorder.try_record_output(
                                &session.session_id,
                                sequence,
                                &buffer[..read],
                            )
                        {
                            tracing::warn!(
                                session_id = %session.session_id,
                                first_missing_sequence,
                                ?reason,
                                "terminal session log stopped at bounded output queue boundary"
                            );
                        }
                        let (output, reader_diagnostic) = {
                            let mut state = session.state.lock().await;
                            let reader_diagnostic = if resize_diagnostics_enabled()
                                && matches!(&state.info.target, SessionTarget::Ssh(_))
                                && claim_resize_reader_diagnostic_chunk(
                                    &session.resize_diagnostic_reader_budget,
                                ) {
                                Some(ResizeReaderDiagnosticContext {
                                    session_sequence: session.resize_diagnostic_session_sequence,
                                    transport_generation,
                                    resize_generation: session
                                        .resize_diagnostic_generation
                                        .load(Ordering::Acquire),
                                    reader_sequence: sequence,
                                    chunk_bytes: read as u64,
                                })
                            } else {
                                None
                            };
                            let before = reader_diagnostic
                                .map(|_| (state.engine.size(), state.engine.revision()));
                            let output = state.engine.process_output(&buffer[..read]);
                            let after = reader_diagnostic
                                .map(|_| (state.engine.size(), state.engine.revision()));
                            apply_terminal_output_events(&session, &output.events).await;
                            let revision = state.engine.revision();
                            session.changes.send_replace(SessionChange {
                                revision,
                                kind: if output.title_changed.is_some() {
                                    SessionChangeKind::Title
                                } else {
                                    SessionChangeKind::Output
                                },
                            });
                            (output, reader_diagnostic.zip(before).zip(after))
                        };
                        if let Some(((context, before), after)) = reader_diagnostic {
                            log_resize_reader_diagnostic(context, 20, before.0, before.1);
                            log_resize_reader_diagnostic(context, 21, after.0, after.1);
                        }
                        if write_parser_responses(&session, output.pty_writes)
                            .await
                            .is_err()
                        {
                            cancellation.cancel();
                            mark_writer_failure(&session, transport_generation).await;
                            break;
                        }
                        if output.close_requested
                            && let Err(error) =
                                control.close_until(Instant::now() + close_wait).await
                        {
                            tracing::warn!(error = %error, "terminal parser close request failed");
                        }
                    }
                }
            }
        })
    }

    /// 在独立受管 task 中观察 native child；状态确认后只启动非破坏性 release，不结束 reader/writer。
    ///
    /// @author kongweiguang
    fn spawn_child_exit_observer(
        &self,
        session: Arc<ManagedSession>,
        control: Arc<dyn SessionTransportControl>,
        transport_generation: u64,
    ) -> JoinHandle<()> {
        let close_wait = self.config.close_wait;
        self.runtime.spawn(async move {
            match control.observe_child_exit().await {
                Ok(Some(status)) => {
                    let candidate_deadline = Instant::now() + close_wait;
                    let Some(deadline) = publish_native_child_exit(
                        &session,
                        transport_generation,
                        status,
                        candidate_deadline,
                    )
                    .await
                    else {
                        return;
                    };
                    match tokio::time::timeout_at(
                        deadline,
                        control.release_after_child_exit_until(deadline),
                    )
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::debug!(
                            event = "LOCAL_PTY_NATURAL_RELEASE_UNAVAILABLE_OR_FAILED",
                            code = error.public_code().as_str(),
                            "native child exited; output reader retains its owner"
                        ),
                        Err(_) => tracing::warn!(
                            event = "LOCAL_PTY_NATURAL_RELEASE_TIMEOUT",
                            code = "timeout",
                            "native child exited; PTY release owner remains pending"
                        ),
                    }
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    event = "SESSION_NATIVE_EXIT_OBSERVATION_FAILED",
                    code = error.public_code().as_str(),
                    "transport could not confirm a native child exit"
                ),
            }
        })
    }

    /// 查询显式 ID 并只在短 registry 临界区克隆 handle，后续异步操作不占用注册锁。
    ///
    /// @author kongweiguang
    async fn session(&self, session_id: &SessionId) -> AppResult<Arc<ManagedSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .live
            .get(session_id)
            .cloned()
            .ok_or_else(|| AppError::not_found("终端会话不存在"))
    }

    /// 使用桌面组合根提供的绝对 deadline 收拢所有 transport 与日志 writer。
    ///
    /// @author kongweiguang
    pub async fn close_all_until(&self, deadline: Instant) -> AppResult<()> {
        close_all_until(self, deadline).await
    }
}

/// 追踪一个由 manager-owned task 持有的创建流程，调用方 future 被丢弃也不会丢资源 owner。
struct PendingCreationGuard {
    manager: SessionManager,
    cancellation: CancellationToken,
    armed: bool,
}

impl PendingCreationGuard {
    /// 成功写入 registry 后解除自动取消，但仍由 Drop 通知 shutdown 创建流程已结束。
    ///
    /// @author kongweiguang
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingCreationGuard {
    /// Future 中途丢弃时取消未发布连接，并让 close_all 唤醒等待计数器。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
        self.manager.pending_creates.fetch_sub(1, Ordering::AcqRel);
        self.manager.pending_create_changed.notify_waiters();
    }
}

/// caller future 被 abort 或 drop 时只通知 manager-owned 重启任务取消，不直接丢弃其清理 future。
///
/// @author kongweiguang
struct RestartAbandonGuard {
    cancellation: CancellationToken,
}

impl Drop for RestartAbandonGuard {
    /// 将调用方生命周期结束转换成可观察取消信号，让后台任务完成当前 transport 收尾后再返回。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// 未发布或重连中的 transport 清理失败时，保存可再次 close/wait 的唯一控制 owner。
enum PendingCleanupOwner {
    Transport {
        control: Arc<dyn SessionTransportControl>,
        _slot: Option<OwnedSemaphorePermit>,
        identity: Arc<()>,
        _id_reservation: Option<Arc<SessionIdReservation>>,
    },
    Open {
        owner: Arc<dyn SessionOpenCleanupOwner>,
        _slot: Option<OwnedSemaphorePermit>,
        identity: Arc<()>,
        _id_reservation: Option<Arc<SessionIdReservation>>,
    },
    Session {
        session: Arc<ManagedSession>,
        identity: Arc<()>,
        _id_reservation: Option<Arc<SessionIdReservation>>,
    },
}

/// 关闭时可克隆的短期 cleanup 操作；容量 permit 始终留在 map 中直到操作确认成功。
struct PendingCleanupAttempt {
    identity: Arc<()>,
    operation: PendingCleanupOperation,
}

enum PendingCleanupOperation {
    Transport(Arc<dyn SessionTransportControl>),
    Open(Arc<dyn SessionOpenCleanupOwner>),
    Session(Arc<ManagedSession>),
}

impl PendingCleanupOwner {
    /// 克隆控制引用而不移动 permit，使 timeout 或 shutdown future 被丢弃后 map 仍保留资源 owner。
    ///
    /// @author kongweiguang
    fn attempt(&self) -> PendingCleanupAttempt {
        match self {
            Self::Transport {
                control, identity, ..
            } => PendingCleanupAttempt {
                identity: Arc::clone(identity),
                operation: PendingCleanupOperation::Transport(Arc::clone(control)),
            },
            Self::Open {
                owner, identity, ..
            } => PendingCleanupAttempt {
                identity: Arc::clone(identity),
                operation: PendingCleanupOperation::Open(Arc::clone(owner)),
            },
            Self::Session {
                session, identity, ..
            } => PendingCleanupAttempt {
                identity: Arc::clone(identity),
                operation: PendingCleanupOperation::Session(Arc::clone(session)),
            },
        }
    }

    /// 返回条目的不可伪造身份，防止清理成功后误删并发替换的新 owner。
    ///
    /// @author kongweiguang
    fn identity(&self) -> &Arc<()> {
        match self {
            Self::Transport { identity, .. }
            | Self::Open { identity, .. }
            | Self::Session { identity, .. } => identity,
        }
    }
}

impl PendingCleanupAttempt {
    /// 在同一绝对 deadline 内执行一次真实 close/wait/reader join，不消费 manager 保留的 owner。
    ///
    /// @author kongweiguang
    async fn cleanup(self, deadline: Instant) -> AppResult<()> {
        match self.operation {
            PendingCleanupOperation::Transport(control) => {
                close_unpublished_transport(&control, deadline).await
            }
            PendingCleanupOperation::Open(owner) => owner.close_until(deadline).await,
            PendingCleanupOperation::Session(session) => {
                close_managed_session(session, deadline, None, None)
                    .await
                    .map(|_| ())
            }
        }
    }
}

/// 记录 factory 主失败与资源回收失败，同时保持对外稳定的安全文案。
///
/// @author kongweiguang
#[derive(Debug)]
struct OpenCleanupFailure {
    primary: AppError,
    cleanup: AppError,
}

impl std::fmt::Display for OpenCleanupFailure {
    /// 输出稳定错误码而不展开主机、路径或 provider 错误文本。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "open={}; cleanup={}",
            self.primary.public_code().as_str(),
            self.cleanup.public_code().as_str()
        )
    }
}

impl std::error::Error for OpenCleanupFailure {
    /// 保留 factory 原始失败；清理失败也由此类型持有并以安全错误码记录。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

/// 将并发清理失败聚合为一个对调用方可见的安全错误。
///
/// @author kongweiguang
#[derive(Debug)]
struct PendingCleanupRetryFailure {
    pending_count: usize,
    first_failure: AppError,
    failures: Vec<AppError>,
}

impl std::fmt::Display for PendingCleanupRetryFailure {
    /// 只报告待清理数量与稳定错误码，不拼接动态底层错误文案。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} pending cleanup owner(s); first failure={}; additional failures={}",
            self.pending_count,
            self.first_failure.public_code().as_str(),
            self.failures.len()
        )
    }
}

impl std::error::Error for PendingCleanupRetryFailure {
    /// 保留首个具体清理错误；其余错误已逐项安全记录到诊断日志。
    ///
    /// @author kongweiguang
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.first_failure)
    }
}

/// 区分自动恢复、手动重连和运行中主动重启，避免放宽既有 reconnect 状态契约。
///
/// @author kongweiguang
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRecoveryMode {
    AutomaticReconnect,
    ExplicitReconnect,
    ExplicitRestart,
}

impl SessionRecoveryMode {
    /// 用稳定的操作文案区分取消来源，调用方取消后仍由 manager 完成资源收尾。
    ///
    /// @author kongweiguang
    fn cancelled_error(self) -> AppError {
        AppError::cancelled(match self {
            Self::AutomaticReconnect => "自动重连已取消",
            Self::ExplicitReconnect => "终端重连已取消",
            Self::ExplicitRestart => "终端重启已取消",
        })
    }

    /// runtime 关闭时拒绝创建替代 transport，并保留现有会话清理 owner。
    ///
    /// @author kongweiguang
    fn runtime_closing_error(self) -> AppError {
        AppError::conflict(match self {
            Self::AutomaticReconnect => "终端自动重连期间 runtime 正在关闭",
            Self::ExplicitReconnect => "终端重连期间 runtime 正在关闭",
            Self::ExplicitRestart => "终端重启期间 runtime 正在关闭",
        })
    }

    /// 仅显式 restart 可从 Running 开始；自动与手动 reconnect 继续遵循既有事件门禁。
    ///
    /// @author kongweiguang
    fn started_event(self) -> SessionLifecycleEvent {
        match self {
            Self::ExplicitRestart => SessionLifecycleEvent::RestartStarted,
            Self::AutomaticReconnect | Self::ExplicitReconnect => {
                SessionLifecycleEvent::ReconnectStarted
            }
        }
    }

    /// 主动 restart 的 close ACK 已确认资源退役，允许协议没有提供可用退出码。
    ///
    /// @author kongweiguang
    fn accepts_unknown_exit_status(self, previous: SessionLifecycle) -> bool {
        self == Self::ExplicitRestart
            || matches!(
                previous,
                SessionLifecycle::Exited | SessionLifecycle::Failed
            )
    }
}

impl SessionManager {
    /// 把自动重连、显式重连和主动重启共用一条资源发布路径，状态入口仍由 mode 严格区分。
    ///
    /// 旧 transport 的退出码可以因远端断线而未知，但资源退役必须收到 ACK；失败时回到
    /// Failed 并继续持有原 control，避免覆盖仍需清理的资源或留下无法重试的 Starting。
    /// 打开替代 transport 只借用原取消对，不改变锁、deadline 或旧 reader 排空顺序。
    /// Starting 发布前先把旧 generation 放入退役门禁；关闭失败时门禁随旧 owner 保留，成功发布新
    /// generation 后才清除，从而隔离关闭/reader join 竞态里的旧读写失败回执。
    ///
    /// @author kongweiguang
    async fn recover_session_cancellable_mode(
        &self,
        session_id: SessionId,
        caller_cancellation: CancellationToken,
        mode: SessionRecoveryMode,
    ) -> AppResult<SessionInfo> {
        if caller_cancellation.is_cancelled() {
            return Err(mode.cancelled_error());
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(mode.runtime_closing_error());
        }
        let session = self.session(&session_id).await?;
        let deadline = Instant::now() + self.config.close_wait;
        if mode != SessionRecoveryMode::AutomaticReconnect {
            Self::cancel_auto_reconnect_until(&session, deadline).await?;
            session.auto_reconnect.lock().await.attempts_started = 0;
        }
        self.retry_pending_cleanup(&session_id, deadline).await?;
        interrupt_active_shell_command(&session, COMMAND_INTERRUPT_DISCONNECT).await;
        let _operation = tokio::select! {
            _ = caller_cancellation.cancelled() => return Err(mode.cancelled_error()),
            result = tokio::time::timeout_at(deadline, session.operation_gate.lock()) => {
                result.map_err(|_| AppError::infrastructure("等待终端重连操作锁超时"))?
            }
        };
        let _input = tokio::select! {
            _ = caller_cancellation.cancelled() => return Err(mode.cancelled_error()),
            result = tokio::time::timeout_at(deadline, session.input_gate.lock()) => {
                result.map_err(|_| AppError::infrastructure("等待终端输入锁超时"))?
            }
        };
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(mode.runtime_closing_error());
        }
        let retiring_transport_generation = session.transport_generation.load(Ordering::Acquire);
        let previous = {
            // Gate and lifecycle use the same lock order as failure callbacks: retirement first, state second.
            // Installing the generation before `Starting` makes any racing old-reader error either finish
            // before recovery starts or observe retirement and leave the new owner untouched.
            let mut retirement = session.retiring_transport_generation.lock().await;
            let previous_retirement = *retirement;
            *retirement = Some(retiring_transport_generation);
            let mut state = session.state.lock().await;
            let previous = state.info.clone();
            let event = mode.started_event();
            let conflict = match mode {
                SessionRecoveryMode::ExplicitRestart => "当前终端生命周期不允许原位重启",
                SessionRecoveryMode::AutomaticReconnect
                | SessionRecoveryMode::ExplicitReconnect => "当前终端生命周期不允许原位重连",
            };
            if transition_session_lifecycle(&mut state, event).is_err() {
                *retirement = previous_retirement;
                return Err(AppError::conflict(conflict));
            }
            session.changes.send_replace(SessionChange {
                revision: state.engine.revision().wrapping_add(1),
                kind: SessionChangeKind::Lifecycle,
            });
            previous
        };

        let was_disconnected = mode.accepts_unknown_exit_status(previous.lifecycle);
        // 先退役 transport 让旧 reader 自然排空；整个阶段失败前都不替换 control 或取消 reader。
        let retirement = async {
            shutdown_session_writer(&session, deadline).await?;
            let old_control = tokio::time::timeout_at(deadline, session.control.read())
                .await
                .map_err(|_| AppError::infrastructure("等待终端 transport 控制锁超时"))?
                .clone();
            old_control.close_until(deadline).await?;
            wait_for_closed_transport_exit(
                old_control.as_ref(),
                &session.session_id,
                deadline,
                was_disconnected,
            )
            .await?;
            join_reader(&session, deadline).await?;
            join_child_exit_observer(&session, deadline).await?;
            Ok::<(), AppError>(())
        }
        .await;
        if let Err(error) = retirement {
            mark_reconnect_failed(&session).await;
            return Err(error);
        }
        if let Ok(current_cancellation) =
            tokio::time::timeout_at(deadline, session.cancellation.lock()).await
        {
            current_cancellation.cancel();
        }

        if caller_cancellation.is_cancelled() {
            mark_reconnect_failed(&session).await;
            return Err(mode.cancelled_error());
        }
        let cancellation = self.shutdown_cancel.child_token();
        let mut pending = match self.register_pending_creation(cancellation.clone()).await {
            Ok(pending) => pending,
            Err(error) => {
                mark_reconnect_failed(&session).await;
                return Err(error);
            }
        };
        let transport = match self
            .open_transport_cancellable(
                self.factory.as_ref(),
                &previous.target,
                previous.size,
                SessionOpenContext {
                    session_id: session_id.clone(),
                    reservation: None,
                    slot: None,
                    cancellations: SessionOpenCancellation {
                        runtime: cancellation.clone(),
                        caller: caller_cancellation.clone(),
                    },
                },
            )
            .await
        {
            Ok((transport, _open_context)) => transport,
            Err(error) => {
                mark_reconnect_failed(&session).await;
                return Err(error);
            }
        };
        let resized = tokio::select! {
            _ = caller_cancellation.cancelled() => {
                cancellation.cancel();
                let cleanup = self.cleanup_unpublished_transport(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    None,
                    mode.cancelled_error(),
                ).await;
                mark_reconnect_failed(&session).await;
                return Err(cleanup);
            }
            _ = cancellation.cancelled() => {
                let cleanup = self.cleanup_unpublished_transport(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    None,
                    mode.runtime_closing_error(),
                ).await;
                mark_reconnect_failed(&session).await;
                return Err(cleanup);
            }
            result = transport.control.resize(previous.size) => result,
        };
        if let Err(error) = resized {
            let error = self
                .cleanup_unpublished_transport(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    None,
                    error,
                )
                .await;
            mark_reconnect_failed(&session).await;
            return Err(error);
        }

        let publication = tokio::select! {
            _ = caller_cancellation.cancelled() => None,
            _ = cancellation.cancelled() => None,
            guard = self.lifecycle_gate.read() => Some(guard),
        };
        let Some(_publication) = publication else {
            let primary = if caller_cancellation.is_cancelled() {
                mode.cancelled_error()
            } else {
                mode.runtime_closing_error()
            };
            let cleanup = self
                .cleanup_unpublished_transport(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    None,
                    primary,
                )
                .await;
            mark_reconnect_failed(&session).await;
            return Err(cleanup);
        };
        if cancellation.is_cancelled()
            || caller_cancellation.is_cancelled()
            || self.shutting_down.load(Ordering::Acquire)
        {
            drop(_publication);
            let primary = if caller_cancellation.is_cancelled() {
                mode.cancelled_error()
            } else {
                mode.runtime_closing_error()
            };
            let cleanup = self
                .cleanup_unpublished_transport(
                    session_id.clone(),
                    Arc::clone(&transport.control),
                    None,
                    primary,
                )
                .await;
            mark_reconnect_failed(&session).await;
            return Err(cleanup);
        }

        {
            let mut state = session.state.lock().await;
            if cancellation.is_cancelled()
                || caller_cancellation.is_cancelled()
                || self.shutting_down.load(Ordering::Acquire)
            {
                drop(state);
                drop(_publication);
                let primary = if caller_cancellation.is_cancelled() {
                    mode.cancelled_error()
                } else {
                    mode.runtime_closing_error()
                };
                let cleanup = self
                    .cleanup_unpublished_transport(
                        session_id.clone(),
                        Arc::clone(&transport.control),
                        None,
                        primary,
                    )
                    .await;
                mark_reconnect_failed(&session).await;
                return Err(cleanup);
            }
            if let Err(error) = state.engine.reset_for_reconnect() {
                drop(state);
                drop(_publication);
                let error = self
                    .cleanup_unpublished_transport(
                        session_id.clone(),
                        Arc::clone(&transport.control),
                        None,
                        engine_error(error),
                    )
                    .await;
                mark_reconnect_failed(&session).await;
                return Err(error);
            }
            let mut shell = session.shell_integration.lock().await;
            shell.reset_for_reconnect();
            session.changes.send_replace(SessionChange {
                revision: state.engine.revision(),
                kind: SessionChangeKind::Output,
            });
        }
        *session.shell_dialect.write().await = transport.shell_dialect;
        *session.process_id.write().await = transport.process_id;
        *session.shell_integration_enabled.write().await = transport.shell_integration_enabled;
        let new_control = Arc::clone(&transport.control);
        let transport_generation = session
            .transport_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        *session.native_exit_deadline.lock().await = None;
        *session.writer.lock().await = transport.writer;
        session.writer_shutdown.store(false, Ordering::Release);
        *session.control.write().await = Arc::clone(&new_control);
        {
            let mut current_cancellation = session.cancellation.lock().await;
            *current_cancellation = cancellation.clone();
        }
        {
            let mut state = session.state.lock().await;
            if cancellation.is_cancelled()
                || caller_cancellation.is_cancelled()
                || self.shutting_down.load(Ordering::Acquire)
            {
                let _ =
                    transition_session_lifecycle(&mut state, SessionLifecycleEvent::CloseRequested);
                drop(state);
                drop(_publication);
                let primary = if caller_cancellation.is_cancelled() {
                    mode.cancelled_error()
                } else {
                    mode.runtime_closing_error()
                };
                let cleanup = self
                    .cleanup_unpublished_transport(
                        session_id.clone(),
                        Arc::clone(&new_control),
                        None,
                        primary,
                    )
                    .await;
                mark_reconnect_failed(&session).await;
                return Err(cleanup);
            }
            if transition_session_lifecycle(&mut state, SessionLifecycleEvent::ReconnectSucceeded)
                .is_err()
            {
                drop(state);
                drop(_publication);
                let cleanup = self
                    .cleanup_unpublished_transport(
                        session_id.clone(),
                        Arc::clone(&new_control),
                        None,
                        AppError::conflict("终端会话恢复结果已过期，不能复活已关闭会话"),
                    )
                    .await;
                mark_reconnect_failed(&session).await;
                return Err(cleanup);
            }
            state.info.exit_status = None;
            session.changes.send_replace(SessionChange {
                revision: state.engine.revision(),
                kind: SessionChangeKind::Lifecycle,
            });
        }
        {
            // The old reader has been joined before publication; clear only its marker before the new
            // reader is spawned, so a new-generation write failure is never treated as retirement noise.
            let mut retirement = session.retiring_transport_generation.lock().await;
            if *retirement == Some(retiring_transport_generation) {
                *retirement = None;
            }
        }
        let child_exit_task = self.spawn_child_exit_observer(
            Arc::clone(&session),
            Arc::clone(&new_control),
            transport_generation,
        );
        *session.child_exit_task.lock().await = Some(child_exit_task);
        let reader_task = self.spawn_reader(
            Arc::clone(&session),
            transport.reader,
            cancellation,
            new_control,
            transport_generation,
        );
        *session.reader_task.lock().await = Some(reader_task);
        pending.disarm();
        drop(_publication);
        Ok(session.state.lock().await.info.clone())
    }
}

#[async_trait]
impl SessionService for SessionManager {
    /// 通过已注入的多协议 factory 建立唯一终端会话。
    ///
    /// @author kongweiguang
    async fn create(&self, request: CreateSessionRequest) -> AppResult<SessionInfo> {
        SessionManager::create(self, request).await
    }

    /// 将 UI 或 MCP 的取消 token 直接传至 provider，以关闭未发布的 PTY 或远端 channel。
    ///
    /// @author kongweiguang
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        SessionManager::create_cancellable(self, request, cancellation).await
    }

    /// 将内部恢复用的预分配 ID 交给 registry gate 与 manager-owned 创建任务。
    ///
    /// @author kongweiguang
    async fn create_with_session_id(
        &self,
        request: CreateSessionRequest,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        SessionManager::create_with_session_id(self, request, session_id, cancellation).await
    }

    /// 使用调用方的一次性 factory，但复用 manager 的容量、取消、publication 与 cleanup 所有权。
    ///
    /// @author kongweiguang
    async fn create_with_transport_factory(
        &self,
        request: CreateSessionRequest,
        factory: Arc<dyn SessionTransportFactory>,
        cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        SessionManager::create_with_transport_factory(self, request, factory, cancellation).await
    }

    /// 先复制 live handle 快照并释放 registry 锁，再逐个读取异步状态以避免锁跨 await。
    ///
    /// @author kongweiguang
    async fn list(&self, filter: SessionFilter) -> AppResult<Vec<SessionInfo>> {
        let sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .live
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut result = Vec::new();
        for session in sessions {
            let state = session.state.lock().await;
            if filter
                .owner
                .as_ref()
                .is_some_and(|owner| owner != &state.info.owner)
                || filter
                    .target
                    .as_ref()
                    .is_some_and(|target| target != &state.info.target)
            {
                continue;
            }
            result.push(state.info.clone());
        }
        Ok(result)
    }

    /// 返回 transport 与 OSC tracker 实际确认的运行态信息，不猜 shell 类型或启动脚本路径。
    ///
    /// @author kongweiguang
    async fn runtime_metadata(&self, session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        let session = self.session(&session_id).await?;
        let shell_dialect = *session.shell_dialect.read().await;
        let process_id = *session.process_id.read().await;
        let shell_integration_enabled = *session.shell_integration_enabled.read().await;
        let shell = session.shell_integration.lock().await;
        Ok(SessionRuntimeMetadata {
            process_id,
            shell_dialect,
            shell_integration_enabled,
            shell_integration_observed: shell.observed,
            prompt_ready: shell.prompt_ready,
        })
    }

    /// 返回同一 session 中实际 shell markers、owner 与输入 generation 的一致性快照。
    ///
    /// @author kongweiguang
    async fn terminal_command_context(
        &self,
        session_id: SessionId,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session(&session_id).await?;
        Ok(terminal_command_context_for(&session).await)
    }

    /// 原样向单一 writer 队列写协议字节，锁保证同一 session 内调用顺序。
    ///
    /// @author kongweiguang
    async fn write(&self, session_id: SessionId, bytes: Vec<u8>) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        write_session_input_with_receipt(session, bytes)
            .await
            .map(|_| ())
    }

    /// 原子回报 Manager 准入的 shell 前态，并复用 raw write 的唯一有序管道。
    ///
    /// @author kongweiguang
    async fn write_with_receipt(
        &self,
        session_id: SessionId,
        bytes: Vec<u8>,
    ) -> AppResult<TerminalWriteReceipt> {
        let session = self.session(&session_id).await?;
        write_session_input_with_receipt(session, bytes).await
    }

    /// 消费组合根提供的唯一 settings snapshot；关闭时取消每个会话的等待重连 token。
    ///
    /// @author kongweiguang
    async fn set_auto_reconnect_enabled(&self, enabled: bool) -> AppResult<()> {
        self.update_auto_reconnect_setting(enabled).await
    }

    /// 根据当前引擎协商模式编码按键、IME、粘贴、鼠标或焦点事件。
    ///
    /// @author kongweiguang
    async fn write_input(&self, session_id: SessionId, event: TerminalInputEvent) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        let modes = session.state.lock().await.engine.input_modes();
        if let Some(bytes) = encode_input(&event, modes) {
            self.write(session_id, bytes).await?;
        }
        Ok(())
    }

    /// 将精确绑定到可见 UI/Agent owner 的事件写入共享 writer；Agent 输入始终使建议投影失效。
    ///
    /// @author kongweiguang
    async fn write_ui_input(
        &self,
        request: UiTerminalInputRequest,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session(&request.binding.session_id).await?;
        write_ui_input_inner(
            session,
            UiInputWriteRequest {
                binding: request.binding,
                expected_context: request.expected_context,
                projection: request.projection,
                payload: UiInputPayload::Event(request.event),
                command_submission: None,
                require_valid_context: true,
                allow_unbracketed_multiline: false,
            },
        )
        .await
    }

    /// 在唯一 writer gate 内复核当前 UI binding，并将命令正文与 Enter 一次提交。
    ///
    /// 不检查 prompt readiness，以免把命令面板限制为 shell integration 场景；失败可能表示部分投递，调用方不得重放。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session(&request.binding.session_id).await?;
        write_ui_input_inner(
            session,
            UiInputWriteRequest {
                binding: request.binding,
                expected_context: None,
                projection: UiInputProjection::Invalidates,
                payload: UiInputPayload::CommandLine(request.command),
                command_submission: None,
                require_valid_context: false,
                allow_unbracketed_multiline: false,
            },
        )
        .await
    }

    /// 对用户点击的片段复用同一 writer gate 与 Paste+Enter 编码，只放宽片段多行的 bracketed-paste 限制。
    ///
    /// 目标仍必须是当前 Running UI owner 和 generation；不会读取 prompt、OSC marker 或 shell 方言。
    ///
    /// @author kongweiguang
    async fn submit_ui_snippet_command_line(
        &self,
        request: UiSnippetCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session(&request.binding.session_id).await?;
        write_ui_input_inner(
            session,
            UiInputWriteRequest {
                binding: request.binding,
                expected_context: None,
                projection: UiInputProjection::Invalidates,
                payload: UiInputPayload::CommandLine(request.command),
                command_submission: None,
                require_valid_context: false,
                allow_unbracketed_multiline: true,
            },
        )
        .await
    }

    /// 将 UI 自己维护的命令文本与一次 Enter 绑定；无法证明投影时仍发送 Enter，但正文留未知。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<TerminalCommandContext> {
        let session = self.session(&request.input.binding.session_id).await?;
        let command = classify_command_block_text(&request.command);
        write_ui_input_inner(
            session,
            UiInputWriteRequest {
                binding: request.input.binding,
                expected_context: request.input.expected_context,
                projection: request.input.projection,
                payload: UiInputPayload::Event(request.input.event),
                command_submission: Some(command),
                require_valid_context: false,
                allow_unbracketed_multiline: false,
            },
        )
        .await
    }

    /// 与兼容 API 共用同一个 writer gate，并返回 gate 对精确 binding/context 正文关联的证明。
    ///
    /// @author kongweiguang
    async fn write_ui_command_submission_with_receipt(
        &self,
        request: UiTerminalCommandSubmissionRequest,
    ) -> AppResult<UiTerminalCommandSubmissionReceipt> {
        let session = self.session(&request.input.binding.session_id).await?;
        let command = classify_command_block_text(&request.command);
        write_ui_input_inner_with_receipt(
            session,
            UiInputWriteRequest {
                binding: request.input.binding,
                expected_context: request.input.expected_context,
                projection: request.input.projection,
                payload: UiInputPayload::Event(request.input.event),
                command_submission: Some(command),
                require_valid_context: false,
                allow_unbracketed_multiline: false,
            },
        )
        .await
    }

    /// 在唯一 writer 准入点复核 Agent owner/generation，并将已确认多行正文与 Enter 合成一次写入。
    ///
    /// @author kongweiguang
    async fn write_agent_submission(
        &self,
        request: AgentTerminalSubmissionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalWriteReceipt> {
        request.validate()?;
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("Agent 终端提交已取消")),
            result = self.session(&request.session_id) => result?,
        };
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("Agent 终端提交已取消"));
        }
        let _input = session
            .input_gate
            .try_lock()
            .map_err(|_| AppError::conflict("终端正在处理另一条输入"))?;
        let state = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("Agent 终端提交已取消")),
            guard = session.state.lock() => guard,
        };
        let mut shell = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("Agent 终端提交已取消")),
            guard = session.shell_integration.lock() => guard,
        };
        let owner_matches = matches!(
            &state.info.owner,
            SessionOwner::Agent { agent_session_id }
                if agent_session_id == &request.agent_session_id
        );
        if !owner_matches
            || state.info.lifecycle != SessionLifecycle::Running
            || shell.session_generation != request.expected_session_generation
        {
            return Err(AppError::conflict("Agent 终端目标或运行代次已变化"));
        }
        if shell.pending_input_write.is_some() {
            return Err(AppError::conflict("上一条终端输入仍在 writer 中"));
        }

        let modes = state.engine.input_modes();
        if request.text.contains('\n') && !modes.bracketed_paste {
            return Err(AppError::unsupported("目标终端未启用多行安全粘贴模式"));
        }
        let mut bytes = encode_input(&TerminalInputEvent::Paste(request.text), modes)
            .ok_or_else(|| AppError::unsupported("当前终端模式不支持粘贴文本"))?;
        let enter = encode_input(
            &TerminalInputEvent::Key(TerminalKeyEvent {
                key: TerminalKey::Enter,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            }),
            modes,
        )
        .ok_or_else(|| AppError::unsupported("当前终端模式不支持 Enter 输入"))?;
        bytes.extend_from_slice(&enter);

        let shell_dialect = *session.shell_dialect.read().await;
        let shell_integration_enabled = *session.shell_integration_enabled.read().await;
        let input_generation_before = shell.input_generation;
        let admission = TerminalWriteAdmission {
            session_id: request.session_id,
            owner: state.info.owner.clone(),
            lifecycle: state.info.lifecycle,
            session_generation: shell.session_generation,
            prompt_epoch: shell.prompt_epoch,
            input_generation_before,
            accepted_input_generation: input_generation_before.wrapping_add(1),
            shell_dialect,
            shell_integration_enabled,
            shell_integration_observed: shell.observed,
            prompt_ready: shell.prompt_ready,
            prompt_line_empty: shell.prompt_ready && !shell.prompt_line_dirty,
            alternate_screen: modes.alternate_screen,
        };
        let write_cancellation = cancellation.child_token();
        let bytes_written = Arc::new(AtomicUsize::new(0));
        let generation = mark_shell_input(&mut shell, ShellInputOrigin::External);
        shell.pending_input_write = Some(PendingTerminalInputWrite {
            generation,
            cancellation: write_cancellation.clone(),
            bytes_written: Arc::clone(&bytes_written),
            expected_bytes: bytes.len(),
            session_generation: shell.session_generation,
            prompt_epoch: shell.prompt_epoch,
            block_id: None,
            command_finished: false,
            prompt_started_after_command: false,
            prompt_ended_after_command: false,
        });
        drop(shell);
        drop(state);
        drop(_input);

        let result = write_input_bytes(&session, &bytes, &write_cancellation, &bytes_written).await;
        finish_terminal_input_write(&session, generation, result.is_err()).await;
        if result.is_err()
            && cancellation.is_cancelled()
            && bytes_written.load(Ordering::Acquire) == 0
        {
            return Err(AppError::cancelled("Agent 终端提交已取消，未发送正文"));
        }
        result?;
        Ok(TerminalWriteReceipt {
            acknowledged_bytes: bytes.len(),
            admission: Some(admission),
        })
    }

    /// 提供权威有界命令块快照；订阅通知可能合并，消费者必须按会话 ID 重新读此接口。
    ///
    /// @author kongweiguang
    async fn command_block_snapshot(
        &self,
        session_id: SessionId,
        cancellation: &CancellationToken,
    ) -> AppResult<CommandBlockSnapshot> {
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令块快照查询已取消")),
            result = self.session(&session_id) => result?,
        };
        let shell = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令块快照查询已取消")),
            guard = session.shell_integration.lock() => guard,
        };
        Ok(CommandBlockSnapshot {
            session_id,
            session_generation: shell.session_generation,
            revision: shell.command_block_revision,
            blocks: shell.command_blocks.iter().cloned().collect(),
        })
    }

    /// 先验证块仍在有界权威快照中，再由同一 engine 读取对应 capture，不从屏幕网格猜输出。
    ///
    /// @author kongweiguang
    async fn command_block_output(
        &self,
        session_id: SessionId,
        block_id: CommandBlockId,
        max_bytes: usize,
        cancellation: &CancellationToken,
    ) -> AppResult<CommandBlockOutputSnapshot> {
        if max_bytes > MAX_COMMAND_BLOCK_OUTPUT_BYTES {
            return Err(AppError::invalid("命令块输出查询超过字节上限"));
        }
        let session = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令块输出查询已取消")),
            result = self.session(&session_id) => result?,
        };
        let state = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令块输出查询已取消")),
            guard = session.state.lock() => guard,
        };
        let shell = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("命令块输出查询已取消")),
            guard = session.shell_integration.lock() => guard,
        };
        let mut snapshot = CommandBlockOutputSnapshot {
            session_id,
            block_id,
            state: CommandBlockOutputState::NotCaptured,
            capture_revision: None,
            text: None,
            captured_bytes: 0,
            returned_bytes: 0,
            truncated: false,
        };
        if !shell
            .command_blocks
            .iter()
            .any(|block| block.id == block_id)
        {
            if shell.evicted_command_blocks.contains(&block_id) {
                snapshot.state = CommandBlockOutputState::Evicted;
                return Ok(snapshot);
            }
            return Err(AppError::not_found("命令块已超出会话保留范围"));
        }
        match shell.command_block_captures.get(&block_id).copied() {
            Some(CommandBlockCapture::Evicted) => {
                snapshot.state = CommandBlockOutputState::Evicted;
            }
            Some(CommandBlockCapture::Captured(capture_id)) => {
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled("命令块输出查询已取消"));
                }
                let captured = match state.engine.transcript_snapshot(capture_id, max_bytes) {
                    Ok(captured) => captured,
                    Err(TranscriptUnavailableReason::Evicted) => {
                        snapshot.state = CommandBlockOutputState::Evicted;
                        return Ok(snapshot);
                    }
                    Err(TranscriptUnavailableReason::UnknownCapture) => {
                        return Err(AppError::infrastructure("命令块转录关联不属于当前终端引擎"));
                    }
                };
                snapshot.state = match captured.state {
                    TerminalTranscriptState::Capturing => CommandBlockOutputState::Capturing,
                    TerminalTranscriptState::Complete { exit_code } => {
                        CommandBlockOutputState::Complete { exit_code }
                    }
                    TerminalTranscriptState::Incomplete(reason) => {
                        CommandBlockOutputState::Incomplete(match reason {
                            TranscriptIncompleteReason::Superseded => {
                                CommandBlockOutputIncompleteReason::Superseded
                            }
                            TranscriptIncompleteReason::Closed => {
                                CommandBlockOutputIncompleteReason::Closed
                            }
                            TranscriptIncompleteReason::Disconnected => {
                                CommandBlockOutputIncompleteReason::Disconnected
                            }
                            TranscriptIncompleteReason::Reconnected => {
                                CommandBlockOutputIncompleteReason::Reconnected
                            }
                        })
                    }
                };
                snapshot.capture_revision = Some(captured.revision);
                snapshot.text = Some(CommandBlockOutputText::from_capture(captured.text));
                snapshot.captured_bytes = captured.captured_bytes;
                snapshot.returned_bytes = captured.returned_bytes;
                snapshot.truncated = captured.truncated;
            }
            None => {}
        }
        Ok(snapshot)
    }

    /// 先切换 VT 网格，使 resize 等待期间到达的字节按新尺寸解析；只有 transport ACK 后才提交公开尺寸。
    /// 主调整与失败补偿都完整等待 transport 自身回执，并在 operation_gate 内串行；不能 timeout-drop
    /// 一个可能仍有 spawn_blocking native 工作的 future，否则迟到的旧尺寸会越过后续重试生效。
    /// 正常退出时只回滚历史网格，不再碰已结束的 transport； opt-in 诊断只记录尺寸、光标坐标和耗时。
    ///
    /// @author kongweiguang
    async fn resize(&self, session_id: SessionId, size: TerminalSize) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        let _operation =
            tokio::time::timeout(self.config.close_wait, session.operation_gate.lock())
                .await
                .map_err(|_| AppError::infrastructure("等待终端 resize 操作锁超时"))?;
        resize_session_locked(&session, size, self.config.terminal_limits).await
    }

    /// 按可见终端 binding 串行缩放；退出历史只重排 engine，不触碰已经结束的 transport。
    ///
    /// operation/input gate 先于 state/shell 锁获取，重连不能在 binding 校验后替换 generation；
    /// Running 复用 engine-first + transport ACK/补偿路径；结束态和已证明的 Closed 历史仅调整 engine 网格。
    ///
    /// @author kongweiguang
    async fn resize_ui_view(&self, binding: UiSessionBinding, size: TerminalSize) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        let _operation =
            tokio::time::timeout(self.config.close_wait, session.operation_gate.lock())
                .await
                .map_err(|_| AppError::infrastructure("等待终端 resize 操作锁超时"))?;
        let _input = tokio::time::timeout(self.config.close_wait, session.input_gate.lock())
            .await
            .map_err(|_| AppError::infrastructure("等待终端输入锁超时"))?;
        TerminalEngine::validate_size(size, self.config.terminal_limits).map_err(engine_error)?;

        {
            let mut state = session.state.lock().await;
            let shell = session.shell_integration.lock().await;
            if !ui_history_access_is_current(&session, &state, &shell, &binding) {
                return Err(AppError::conflict(
                    "可见终端 resize 目标或 generation 已失效",
                ));
            }

            match state.info.lifecycle {
                SessionLifecycle::Running => {}
                SessionLifecycle::Exited | SessionLifecycle::Failed | SessionLifecycle::Closed => {
                    state.engine.resize(size).map_err(engine_error)?;
                    state.info.size = size;
                    session.changes.send_replace(SessionChange {
                        revision: state.engine.revision(),
                        kind: SessionChangeKind::Resized,
                    });
                    return Ok(());
                }
                _ => return Err(AppError::conflict("终端会话当前不能调整尺寸")),
            }
        }

        resize_session_locked(&session, size, self.config.terminal_limits).await
    }

    /// 返回引擎生成的 typed view snapshot，并按每个 UI view 的 revision 提供 dirty rows。
    ///
    /// @author kongweiguang
    async fn render_snapshot(
        &self,
        session_id: SessionId,
        request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        let session = self.session(&session_id).await?;
        let mut state = session.state.lock().await;
        Ok(state.engine.render_snapshot(request))
    }

    /// Reads a workspace snapshot only when its exact visible binding is still eligible for history access.
    ///
    /// @author kongweiguang
    async fn render_snapshot_ui(
        &self,
        binding: UiSessionBinding,
        request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        let session = self.session(&binding.session_id).await?;
        let mut state = session.state.lock().await;
        let shell = session.shell_integration.lock().await;
        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端快照目标或 generation 已失效"));
        }
        Ok(state.engine.render_snapshot(request))
    }

    /// 让主题切换与 OSC 查询共享 engine 的一份颜色解析表，并发送新 revision 触发所有 view 重绘。
    ///
    /// @author kongweiguang
    async fn set_palette(&self, session_id: SessionId, palette: TerminalPalette) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        let mut state = session.state.lock().await;
        state.engine.set_palette(palette);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 把设置页 scrollback 变更同步到既有引擎；引擎按自身网格预算裁剪而不重新创建 PTY。
    ///
    /// @author kongweiguang
    async fn set_scrollback_limit(&self, session_id: SessionId, lines: usize) -> AppResult<usize> {
        let session = self.session(&session_id).await?;
        let mut state = session.state.lock().await;
        let applied = state.engine.set_scrollback_limit(lines);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(applied)
    }

    /// 返回独立的 bounded plaintext view，不让 MCP 从绘制 runs 反推内容。
    ///
    /// @author kongweiguang
    async fn plaintext_snapshot(
        &self,
        session_id: SessionId,
        request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        let session = self.session(&session_id).await?;
        Ok(session
            .state
            .lock()
            .await
            .engine
            .plaintext_snapshot(request))
    }

    /// 调整共享引擎的显示 offset，不改变 PTY 状态或重建窗口。
    ///
    /// @author kongweiguang
    async fn scroll(&self, session_id: SessionId, request: ScrollRequest) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        let mut state = session.state.lock().await;
        state.engine.scroll(request.lines);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 在持有会话状态与 shell generation 锁期间校验绑定并提交滚动，关闭重连的 TOCTOU 窗口。
    /// 滚动仅访问引擎历史，因此 Exited/Failed 可查看；Starting/Closing/Closed 不接受新 view 操作。
    ///
    /// @author kongweiguang
    async fn scroll_ui(&self, binding: UiSessionBinding, request: ScrollRequest) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        let mut state = session.state.lock().await;
        let shell = session.shell_integration.lock().await;
        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端滚动目标或 generation 已失效"));
        }

        state.engine.scroll(request.lines);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 在相同 session 与 shell generation 锁内跳到绝对历史位置，防止失效 UI 绑定改写新视口。
    ///
    /// offset 由 engine 按当前 history 上限裁剪；快照读取不再写回 viewport，因而该命令是
    /// 唯一绝对定位入口，并可与增量 scroll 请求共用同一可见 owner/lifecycle 契约。
    ///
    /// @author kongweiguang
    async fn scroll_ui_to_offset(&self, binding: UiSessionBinding, offset: usize) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        let mut state = session.state.lock().await;
        let shell = session.shell_integration.lock().await;
        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端跳转目标或 generation 已失效"));
        }

        state.engine.scroll_to_offset(offset);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 设置由 Alacritty selection 模型解释的共享网格选择。
    ///
    /// @author kongweiguang
    async fn set_selection(
        &self,
        session_id: SessionId,
        selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        let session = self.session(&session_id).await?;
        let mut state = session.state.lock().await;
        state.engine.set_selection(selection);
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 在相同 session 锁内校验可见 owner/generation 并选择活动 grid，禁止快照后再写造成的边界竞态。
    ///
    /// @author kongweiguang
    async fn select_all_ui(&self, binding: UiSessionBinding) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        let mut state = session.state.lock().await;
        let shell = session.shell_integration.lock().await;
        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端全选目标或 generation 已失效"));
        }

        state.engine.select_all();
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Output,
        });
        Ok(())
    }

    /// 在 session 与 shell generation 锁内更新可见选择，防止迟到拖选事件写入重连后的新视图。
    ///
    /// 只有带 gesture token 的连续拖动保留 Alacritty anchor；清除操作始终直接清理有效选择，
    /// 而失效 token 被拒绝且不发绘制通知，避免已结束手势复活或覆盖新选择。
    ///
    /// @author kongweiguang
    async fn set_ui_selection(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
    ) -> AppResult<()> {
        self.set_ui_selection_cancellable(binding, selection, gesture_id, CancellationToken::new())
            .await
    }

    /// 在可见 owner、generation 与会话锁都稳定后复查取消 token，再提交终端选区。
    ///
    /// UI 可取消等待中的旧 pointer move；token 在等待锁期间取消时必须在修改 engine 前退出，
    /// 否则迟到 move 可能覆盖新手势。连续有效手势仍由 engine 保留 Alacritty 原 anchor。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        self.set_ui_selection_cancellable_with_revision(
            binding,
            selection,
            gesture_id,
            None,
            cancellation,
        )
        .await
    }

    /// 在同一 engine 锁内提交拖选起点的 revision CAS；后续 Move 沿用已确认的 Alacritty anchor。
    ///
    /// 过期的新手势仍需送入 engine 以登记 token tombstone，禁止其迟到 Move 重建 anchor；
    /// revision mismatch 只回冲突，不发绘制通知或改写原选择。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable_with_revision(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        expected_revision: Option<OutputRevision>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        apply_ui_selection(
            &session,
            &binding,
            selection,
            gesture_id,
            UiSelectionCoordinates::Revision(expected_revision),
            cancellation,
        )
        .await
    }

    /// 在 session generation 锁内用快照 origin 和端点行 witness 映射拖选，确保持续输出后仍命中原内容。
    ///
    /// Engine 同时校验尺寸、grid epoch、历史边界和行文字/宽度/wrap；失效坐标返回冲突并保留确认选区。
    ///
    /// @author kongweiguang
    async fn set_ui_selection_cancellable_in_viewport(
        &self,
        binding: UiSessionBinding,
        selection: Option<TerminalSelection>,
        gesture_id: Option<u64>,
        viewport: SelectionViewport,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let session = self.session(&binding.session_id).await?;
        apply_ui_selection(
            &session,
            &binding,
            selection,
            gesture_id,
            UiSelectionCoordinates::Viewport(viewport),
            cancellation,
        )
        .await
    }

    /// 使用 engine 的 wrap、选择和宽字符实现提取文本。
    ///
    /// @author kongweiguang
    async fn selected_text(&self, session_id: SessionId) -> AppResult<Option<String>> {
        let session = self.session(&session_id).await?;
        Ok(session.state.lock().await.engine.selected_text())
    }

    /// Reads a visible selection under the same ended-history owner/generation proof used by other UI view operations.
    ///
    /// @author kongweiguang
    async fn selected_text_ui(&self, binding: UiSessionBinding) -> AppResult<Option<String>> {
        let session = self.session(&binding.session_id).await?;
        let state = session.state.lock().await;
        let shell = session.shell_integration.lock().await;
        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端复制目标或 generation 已失效"));
        }
        Ok(state.engine.selected_text())
    }

    /// 在 bounded grid 与 scrollback 中执行请求的字面或正则查询。
    ///
    /// @author kongweiguang
    async fn search(
        &self,
        session_id: SessionId,
        request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        self.search_cancellable(session_id, request, CancellationToken::new())
            .await
    }

    /// 等待 session 锁时监听取消，并在引擎的有界扫描检查点中止全历史查询。
    ///
    /// 命中只在完整搜索成功时返回；取消与无效正则分别映射为取消和输入错误，绝不回传部分结果。
    ///
    /// @author kongweiguang
    async fn search_cancellable(
        &self,
        session_id: SessionId,
        request: TerminalSearchRequest,
        cancellation: CancellationToken,
    ) -> AppResult<TerminalSearchResult> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端搜索已取消"));
        }
        let session = self.session(&session_id).await?;
        let state = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("终端搜索已取消"));
            }
            state = session.state.lock() => state,
        };
        state
            .engine
            .search_cancellable(&request, || cancellation.is_cancelled())
            .map_err(|error| match error {
                TerminalEngineError::SearchCancelled => AppError::cancelled("终端搜索已取消"),
                error => AppError::invalid("终端搜索表达式无效").with_source(error),
            })
    }

    /// 在固定 UI binding 下原子重查；预览保留 viewport，显式定位由一次性门提交滚动。
    ///
    /// 搜索、坐标映射与 viewport 更新共用 state 锁；输出、裁剪和 resize 只能发生在本次操作之前或之后，
    /// 因而返回 revision 能与结果坐标一一对应。取消与显式滚动通过 CAS 门线性化，预览不触碰 viewport。
    ///
    /// @author kongweiguang
    async fn search_ui_cancellable(
        &self,
        binding: UiSessionBinding,
        request: UiTerminalSearchRequest,
        cancellation: CancellationToken,
    ) -> AppResult<UiTerminalSearchResult> {
        if cancellation.is_cancelled() || request.commit_gate.is_cancelled() {
            return Err(AppError::cancelled("终端搜索已取消"));
        }
        let session = self.session(&binding.session_id).await?;
        let mut state = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("终端搜索已取消"));
            }
            state = session.state.lock() => state,
        };
        let shell = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("终端搜索已取消"));
            }
            shell = session.shell_integration.lock() => shell,
        };

        if !ui_history_access_is_current(&session, &state, &shell, &binding) {
            return Err(AppError::conflict("可见终端搜索目标或 generation 已失效"));
        }
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("终端搜索已取消"));
        }

        // UI 结果保持正向稳定顺序；方向只决定首次 Reveal 的默认选中位置，后续索引因此不漂移。
        let mut engine_request = request.request.clone();
        engine_request.backwards = false;
        let mut result = state
            .engine
            .search_cancellable(&engine_request, || {
                cancellation.is_cancelled() || request.commit_gate.is_cancelled()
            })
            .map_err(|error| match error {
                TerminalEngineError::SearchCancelled => AppError::cancelled("终端搜索已取消"),
                error => AppError::invalid("终端搜索表达式无效").with_source(error),
            })?;
        if cancellation.is_cancelled() || request.commit_gate.is_cancelled() {
            request.commit_gate.cancel();
            return Err(AppError::cancelled("终端搜索已取消"));
        }

        let match_count = result.matches.len();
        let selected_match = match (request.viewport, match_count) {
            (_, 0) => None,
            (UiTerminalSearchViewport::PreserveViewport, _) => request
                .selected_match
                .map(|index| index.min(match_count - 1)),
            (UiTerminalSearchViewport::Reveal { .. }, _) => Some(
                request
                    .selected_match
                    .unwrap_or_else(|| {
                        if request.request.backwards {
                            match_count - 1
                        } else {
                            result.selected_match.unwrap_or(0)
                        }
                    })
                    .min(match_count - 1),
            ),
        };
        result.selected_match = selected_match;

        let revision_before_scroll = state.engine.revision();
        let mut reveal_safe_margin_reached = None;
        if let (UiTerminalSearchViewport::Reveal { safe_top_rows }, Some(selected_match)) =
            (request.viewport, selected_match)
        {
            let match_line = result.matches[selected_match].range.start.line;
            let viewport = state
                .engine
                .render_snapshot(RenderSnapshotRequest::default());
            let current_offset = i128::try_from(viewport.scrollback_offset).unwrap_or(i128::MAX);
            let visible_row = i128::from(match_line) + current_offset;
            let last_row = i128::from(viewport.size.rows().saturating_sub(1));
            let requested_safe_row = i128::from(safe_top_rows);
            let safe_row = requested_safe_row.min(last_row);
            let desired_offset = if visible_row < safe_row {
                current_offset + (safe_row - visible_row)
            } else if visible_row > last_row {
                current_offset - (visible_row - last_row)
            } else {
                current_offset
            };
            let history_limit = i128::try_from(viewport.history_size).unwrap_or(i128::MAX);
            let target_offset = desired_offset.clamp(0, history_limit) as usize;
            if cancellation.is_cancelled() {
                request.commit_gate.cancel();
                return Err(AppError::cancelled("终端搜索已取消"));
            }
            if !request.commit_gate.try_commit() {
                return Err(AppError::cancelled("终端搜索定位已取消"));
            }
            state.engine.scroll_to_offset(target_offset);
            let actual_row =
                i128::from(match_line) + i128::try_from(target_offset).unwrap_or(i128::MAX);
            reveal_safe_margin_reached = Some(
                requested_safe_row < i128::from(viewport.size.rows())
                    && actual_row >= requested_safe_row
                    && actual_row <= last_row,
            );
        }

        let revision = state.engine.revision();
        if revision != revision_before_scroll {
            session.changes.send_replace(SessionChange {
                revision,
                kind: SessionChangeKind::Output,
            });
        }
        Ok(UiTerminalSearchResult {
            result,
            revision,
            reveal_safe_margin_reached,
        })
    }

    /// 用原有显式 target 重建已退出 transport；pending token 与发布门禁阻止 shutdown 后复活。
    ///
    /// @author kongweiguang
    async fn reconnect_cancellable(
        &self,
        session_id: SessionId,
        caller_cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.recover_session_cancellable_mode(
            session_id,
            caller_cancellation,
            SessionRecoveryMode::ExplicitReconnect,
        )
        .await
    }

    /// 对当前 session 执行原位重启，沿用同一身份并仅在旧资源退役 ACK 后发布新代次。
    ///
    /// @author kongweiguang
    async fn restart_cancellable(
        &self,
        session_id: SessionId,
        caller_cancellation: CancellationToken,
    ) -> AppResult<SessionInfo> {
        if caller_cancellation.is_cancelled() {
            return Err(SessionRecoveryMode::ExplicitRestart.cancelled_error());
        }
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SessionRecoveryMode::ExplicitRestart.runtime_closing_error());
        }

        // caller future 的生命周期短于 transport 清理；manager-owned task 必须活到生命周期收敛。
        let abandoned = CancellationToken::new();
        let _abandonment = RestartAbandonGuard {
            cancellation: abandoned.clone(),
        };
        let pending = self
            .register_pending_creation(self.shutdown_cancel.child_token())
            .await?;
        let publication = self.lifecycle_gate.read().await;
        if self.shutting_down.load(Ordering::Acquire) {
            return Err(SessionRecoveryMode::ExplicitRestart.runtime_closing_error());
        }

        let manager = self.clone();
        let recovery_cancel = CancellationToken::new();
        let worker_caller_cancellation = caller_cancellation.clone();
        let (sender, receiver) = oneshot::channel();
        self.create_tasks.spawn_on(
            async move {
                let cancellation = recovery_cancel.clone();
                let shutdown = manager.shutdown_cancel.clone();
                let watcher = manager.runtime.spawn(async move {
                    tokio::select! {
                        _ = worker_caller_cancellation.cancelled() => cancellation.cancel(),
                        _ = abandoned.cancelled() => cancellation.cancel(),
                        _ = shutdown.cancelled() => cancellation.cancel(),
                    }
                });
                let result = manager
                    .recover_session_cancellable_mode(
                        session_id,
                        recovery_cancel,
                        SessionRecoveryMode::ExplicitRestart,
                    )
                    .await;
                watcher.abort();
                let mut pending = pending;
                if result.is_ok() {
                    pending.disarm();
                }
                let _ = sender.send(result);
                drop(pending);
            },
            &self.runtime,
        );
        drop(publication);

        tokio::select! {
            biased;
            result = receiver => {
                result.map_err(|_| AppError::infrastructure("终端重启任务意外结束"))?
            }
            _ = caller_cancellation.cancelled() => {
                Err(SessionRecoveryMode::ExplicitRestart.cancelled_error())
            }
        }
    }

    /// 关闭指定 session，取消 reader 并等待 transport child/channel 的关闭结果。
    ///
    /// @author kongweiguang
    async fn close(&self, session_id: SessionId) -> AppResult<()> {
        SessionService::close_until(self, session_id, Instant::now() + self.config.close_wait).await
    }

    /// 在共享绝对 deadline 内关闭指定 session；移除时核对 Arc owner，防止迟到的同 ID 关闭删掉新会话。
    ///
    /// @author kongweiguang
    async fn close_until(&self, session_id: SessionId, deadline: Instant) -> AppResult<()> {
        if Instant::now() >= deadline {
            return Err(AppError::infrastructure(
                "关闭终端会话的统一 deadline 已到期",
            ));
        }
        self.retry_pending_cleanup(&session_id, deadline).await?;
        let session = tokio::time::timeout_at(deadline, self.session(&session_id))
            .await
            .map_err(|_| AppError::infrastructure("等待终端会话查找超时"))??;
        close_managed_session(Arc::clone(&session), deadline, None, None).await?;
        if let Some(recorder) = &self.output_recorder
            && let Err(error) =
                finish_output_recording(recorder.as_ref(), &session.session_id, deadline).await
        {
            mark_closed_history_recording_failure(&session).await;
            return Err(error);
        }
        let mut registry = self
            .sessions
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registry
            .live
            .get(&session_id)
            .is_some_and(|current| Arc::ptr_eq(current, &session))
        {
            registry.live.remove(&session_id);
        }
        Ok(())
    }

    /// 先禁止发布并通知全部 transport，再并发收尾且逐条记录错误，避免首条失败跳过后续清理。
    ///
    /// @author kongweiguang
    async fn close_all(&self) -> AppResult<()> {
        SessionManager::close_all_until(self, Instant::now() + self.config.close_wait).await
    }

    /// 给 UI 或 MCP 订阅同一个 session 的合并变化信号。
    ///
    /// @author kongweiguang
    async fn subscribe(&self, session_id: SessionId) -> AppResult<SessionSubscription> {
        Ok(SessionSubscription::from_receiver(
            self.session(&session_id).await?.changes.subscribe(),
        ))
    }

    /// 在干净 prompt 上执行脚本；UI binding 在 operation/input gate 内复验，防止检查后重连换代再注入。
    ///
    /// @author kongweiguang
    async fn execute_shell_command(
        &self,
        request: ShellCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        validate_shell_command_request(&request)?;
        let deadline = Instant::now() + request.timeout;
        if cancellation.is_cancelled() {
            return Ok(shell_command_unknown(
                &request,
                ShellCommandUnknownReason::Cancelled,
            ));
        }
        let session = self.session(&request.session_id).await?;
        let _operation =
            match tokio::time::timeout_at(deadline, session.operation_gate.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    return Ok(shell_command_unknown(
                        &request,
                        ShellCommandUnknownReason::TimedOut,
                    ));
                }
            };
        let _input = match session.input_gate.try_lock() {
            Ok(guard) => guard,
            Err(_) => return Err(AppError::conflict("终端当前有输入写入，命令未注入")),
        };
        let dialect = *session.shell_dialect.read().await;
        let command_line = encode_managed_shell_command(request.command.trim(), dialect)?;

        let command_cancellation = cancellation.child_token();
        let interruption = Arc::new(AtomicU8::new(COMMAND_INTERRUPT_NONE));
        let (mut events, baseline_sequence) = {
            let state = session.state.lock().await;
            if state.info.lifecycle != SessionLifecycle::Running {
                return Ok(shell_command_unknown(
                    &request,
                    ShellCommandUnknownReason::Disconnected,
                ));
            }
            let mut shell = session.shell_integration.lock().await;
            if let Some(binding) = &request.expected_ui_binding
                && (binding.session_id != request.session_id
                    || binding.owner != state.info.owner
                    || binding.session_generation != shell.session_generation)
            {
                return Err(AppError::conflict("终端 UI 绑定已过期，受管命令未注入"));
            }
            if shell
                .active_command
                .as_ref()
                .is_some_and(|active| active.cancellation.is_cancelled())
            {
                shell.active_command = None;
            }
            if shell.active_command.is_some() {
                return Err(AppError::conflict("当前终端已有受管命令"));
            }
            if !shell.observed || !shell.prompt_started || !shell.prompt_ready {
                return Err(AppError::capability_not_supported(
                    "OSC 133 shell prompt tracking",
                ));
            }
            if shell.prompt_line_dirty {
                return Err(AppError::conflict("终端提示符中已有输入，命令未注入"));
            }
            let receiver = session.shell_events.subscribe();
            let baseline = shell.sequence;
            shell.active_command = Some(ActiveShellCommand {
                command_id: request.command_id.clone(),
                cancellation: command_cancellation.clone(),
                interruption: Arc::clone(&interruption),
                injected: true,
                started: false,
            });
            shell.input_generation = shell.input_generation.wrapping_add(1);
            shell.prompt_ready = false;
            shell.prompt_line_dirty = true;
            (receiver, baseline)
        };
        let mut cancel_on_drop = CancelShellCommandOnDrop {
            cancellation: command_cancellation.clone(),
            armed: true,
        };
        let mut changes = session.changes.subscribe();
        let mut command_bytes = command_line.into_bytes();
        command_bytes.push(b'\r');
        let write_result = tokio::time::timeout_at(deadline, async {
            let mut writer = session.writer.lock().await;
            writer
                .write_all(&command_bytes)
                .await
                .map_err(io_transport_error)?;
            writer.flush().await.map_err(io_transport_error)
        })
        .await;
        match write_result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                command_cancellation.cancel();
                tracing::warn!(error = %error, "shell command input transport failed");
                clear_active_shell_command(&session, &request.command_id).await;
                cancel_on_drop.disarm();
                return Ok(shell_command_unknown(
                    &request,
                    ShellCommandUnknownReason::Disconnected,
                ));
            }
            Err(_) => {
                command_cancellation.cancel();
                clear_active_shell_command(&session, &request.command_id).await;
                cancel_on_drop.disarm();
                return Ok(shell_command_unknown(
                    &request,
                    ShellCommandUnknownReason::TimedOut,
                ));
            }
        }

        let outcome = loop {
            tokio::select! {
                _ = cancellation.cancelled() => {
                    break shell_command_unknown(&request, ShellCommandUnknownReason::Cancelled);
                }
                _ = command_cancellation.cancelled() => {
                    let reason = if cancellation.is_cancelled() {
                        ShellCommandUnknownReason::Cancelled
                    } else if interruption.load(Ordering::Acquire) == COMMAND_INTERRUPT_USER_INPUT {
                        ShellCommandUnknownReason::InputInterrupted
                    } else {
                        ShellCommandUnknownReason::Disconnected
                    };
                    break shell_command_unknown(&request, reason);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    let started = session.shell_integration.lock().await.active_command.as_ref()
                        .is_some_and(|active| active.command_id == request.command_id && active.started);
                    let reason = if started {
                        ShellCommandUnknownReason::MissingCompletion
                    } else {
                        ShellCommandUnknownReason::MissingCommandStart
                    };
                    break shell_command_unknown(&request, reason);
                }
                changed = changes.changed() => {
                    if changed.is_err() || session.state.lock().await.info.lifecycle != SessionLifecycle::Running {
                        break shell_command_unknown(&request, ShellCommandUnknownReason::Disconnected);
                    }
                }
                event = events.recv() => match event {
                    Ok(event) if event.sequence > baseline_sequence && event.command_id.as_deref() == Some(request.command_id.as_str()) => {
                        match event.marker {
                            ShellIntegrationMarker::CommandStart => {}
                            ShellIntegrationMarker::CommandFinished { exit_code: Some(exit_code) } => {
                                break ShellCommandOutcome::Finished {
                                    command_id: request.command_id.clone(),
                                    exit_code,
                                };
                            }
                            ShellIntegrationMarker::CommandFinished { exit_code: None } => {
                                break shell_command_unknown(&request, ShellCommandUnknownReason::MissingCompletion);
                            }
                            ShellIntegrationMarker::PromptStart | ShellIntegrationMarker::PromptEnd => {}
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        break shell_command_unknown(&request, ShellCommandUnknownReason::EventLagged);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break shell_command_unknown(&request, ShellCommandUnknownReason::Disconnected);
                    }
                }
            }
        };
        clear_active_shell_command(&session, &request.command_id).await;
        cancel_on_drop.disarm();
        Ok(outcome)
    }
}

/// Caller must hold the session operation gate; this helper preserves the engine-first transport ACK and compensation contract.
///
/// @author kongweiguang
async fn resize_session_locked(
    session: &ManagedSession,
    size: TerminalSize,
    terminal_limits: TerminalLimits,
) -> AppResult<()> {
    TerminalEngine::validate_size(size, terminal_limits).map_err(engine_error)?;
    let control = session.control.read().await.clone();
    let diagnostics_enabled = resize_diagnostics_enabled();
    let native_process_id = if diagnostics_enabled {
        session.process_id.read().await.unwrap_or_default()
    } else {
        0
    };
    let (old_size, diagnostic_context, cursor_before, cursor_after) = {
        let mut state = session.state.lock().await;
        if state.info.lifecycle != SessionLifecycle::Running {
            return Err(AppError::conflict("终端会话当前不能调整尺寸"));
        }
        let old_size = state.info.size;
        let target_kind = SessionTargetKind::from_target(&state.info.target);
        let diagnostics_requested = diagnostics_enabled
            && matches!(
                target_kind,
                SessionTargetKind::Local | SessionTargetKind::Ssh
            );
        let transport_kind = match target_kind {
            SessionTargetKind::Local => 0,
            SessionTargetKind::Ssh => 1,
            _ => 2,
        };
        let reader_sequence_before = session.output_sequence.load(Ordering::Acquire);
        let cursor_before = diagnostics_requested
            .then(|| sample_resize_diagnostic_cursor(&state.engine, reader_sequence_before));
        state.engine.resize(size).map_err(engine_error)?;
        let reader_sequence_after = session.output_sequence.load(Ordering::Acquire);
        let diagnostic_context = diagnostics_requested.then(|| {
            let resize_generation = session
                .resize_diagnostic_generation
                .fetch_add(1, Ordering::Relaxed)
                .saturating_add(1);
            session
                .resize_diagnostic_cpr_generation
                .store(resize_generation, Ordering::Release);
            if target_kind == SessionTargetKind::Ssh {
                reset_resize_reader_diagnostic_budget(&session.resize_diagnostic_reader_budget);
            }
            ResizeDiagnosticContext {
                session_sequence: session.resize_diagnostic_session_sequence,
                transport_generation: session.transport_generation.load(Ordering::Acquire),
                resize_generation,
                native_process_id,
                transport_kind,
                old_size,
                new_size: size,
            }
        });
        let cursor_after = diagnostics_requested
            .then(|| sample_resize_diagnostic_cursor(&state.engine, reader_sequence_after));
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Resized,
        });
        (old_size, diagnostic_context, cursor_before, cursor_after)
    };
    if let (Some(context), Some(before), Some(after)) =
        (diagnostic_context, cursor_before, cursor_after)
    {
        log_resize_engine_diagnostic(context, 0, before);
        log_resize_engine_diagnostic(context, 1, after);
    }

    if let Some(context) = diagnostic_context {
        log_resize_transport_diagnostic(
            context,
            2,
            size,
            None,
            session.output_sequence.load(Ordering::Acquire),
        );
    }
    let primary_started = Instant::now();
    let primary_result = control.resize(size).await;
    if let Some(context) = diagnostic_context {
        log_resize_transport_diagnostic(
            context,
            if primary_result.is_ok() { 3 } else { 4 },
            size,
            Some(primary_started.elapsed()),
            session.output_sequence.load(Ordering::Acquire),
        );
    }
    if let Err(primary_error) = primary_result {
        if let Err(engine_error) = restore_resize_engine(session, old_size).await {
            return Err(AppError::infrastructure_with_source(
                "本地终端网格恢复失败；transport 尺寸未确认",
                engine_error.with_source(primary_error),
            ));
        }
        if resize_lifecycle_is_ended(session.state.lock().await.info.lifecycle) {
            return Err(AppError::conflict("会话已结束，未提交尺寸变更").with_source(primary_error));
        }

        if let Some(context) = diagnostic_context {
            log_resize_transport_diagnostic(
                context,
                5,
                old_size,
                None,
                session.output_sequence.load(Ordering::Acquire),
            );
        }
        let compensation_started = Instant::now();
        let compensation_result = control.resize(old_size).await;
        if let Some(context) = diagnostic_context {
            log_resize_transport_diagnostic(
                context,
                if compensation_result.is_ok() { 6 } else { 7 },
                old_size,
                Some(compensation_started.elapsed()),
                session.output_sequence.load(Ordering::Acquire),
            );
        }
        return match compensation_result {
            Ok(()) => {
                let lifecycle = session.state.lock().await.info.lifecycle;
                if resize_lifecycle_is_ended(lifecycle) {
                    Err(AppError::conflict("会话已结束，未提交尺寸变更").with_source(primary_error))
                } else {
                    Err(AppError::infrastructure_with_source(
                        "终端尺寸调整失败，已恢复到原尺寸",
                        primary_error,
                    ))
                }
            }
            Err(recovery_error) => {
                let lifecycle = session.state.lock().await.info.lifecycle;
                if resize_lifecycle_is_ended(lifecycle) {
                    Err(AppError::conflict("会话已结束，未提交尺寸变更")
                        .with_source(recovery_error.with_source(primary_error)))
                } else {
                    tracing::error!(
                        session_id = %session.session_id,
                        primary_error = %primary_error,
                        recovery_error = %recovery_error,
                        "terminal resize failed and the confirmed transport size could not be restored"
                    );
                    Err(AppError::infrastructure_with_source(
                        "终端尺寸状态未确认；请检查会话后显式重试",
                        recovery_error.with_source(primary_error),
                    ))
                }
            }
        };
    }

    let lifecycle_after_native = {
        let mut state = session.state.lock().await;
        if state.info.lifecycle != SessionLifecycle::Running {
            state.info.lifecycle
        } else {
            state.info.size = size;
            session.changes.send_replace(SessionChange {
                revision: state.engine.revision(),
                kind: SessionChangeKind::Resized,
            });
            SessionLifecycle::Running
        }
    };
    if lifecycle_after_native == SessionLifecycle::Running {
        return Ok(());
    }

    if let Err(engine_error) = restore_resize_engine(session, old_size).await {
        return Err(AppError::infrastructure_with_source(
            "会话状态变化后本地终端网格恢复失败",
            engine_error,
        ));
    }
    let lifecycle_after_restore = session.state.lock().await.info.lifecycle;
    if resize_lifecycle_is_ended(lifecycle_after_restore) {
        return Err(AppError::conflict("会话已结束，未提交尺寸变更"));
    }
    if let Some(context) = diagnostic_context {
        log_resize_transport_diagnostic(
            context,
            5,
            old_size,
            None,
            session.output_sequence.load(Ordering::Acquire),
        );
    }
    let compensation_started = Instant::now();
    let compensation_result = control.resize(old_size).await;
    if let Some(context) = diagnostic_context {
        log_resize_transport_diagnostic(
            context,
            if compensation_result.is_ok() { 6 } else { 7 },
            old_size,
            Some(compensation_started.elapsed()),
            session.output_sequence.load(Ordering::Acquire),
        );
    }
    match compensation_result {
        Ok(()) => {
            let lifecycle = session.state.lock().await.info.lifecycle;
            if resize_lifecycle_is_ended(lifecycle) {
                Err(AppError::conflict("会话已结束，未提交尺寸变更"))
            } else if lifecycle_after_native == SessionLifecycle::Failed
                || lifecycle == SessionLifecycle::Failed
            {
                Err(AppError::infrastructure(
                    "终端传输已发生故障；尺寸未提交，原尺寸补偿已确认",
                ))
            } else {
                Err(AppError::conflict(
                    "终端会话在调整尺寸期间已停止，原尺寸已恢复",
                ))
            }
        }
        Err(recovery_error) => {
            let lifecycle = session.state.lock().await.info.lifecycle;
            if resize_lifecycle_is_ended(lifecycle) {
                Err(AppError::conflict("会话已结束，未提交尺寸变更").with_source(recovery_error))
            } else {
                tracing::error!(
                    session_id = %session.session_id,
                    recovery_error = %recovery_error,
                    "terminal stopped during resize and the confirmed transport size could not be restored"
                );
                Err(AppError::infrastructure_with_source(
                    "终端会话已停止且尺寸状态未确认；请检查会话后显式重试",
                    recovery_error,
                ))
            }
        }
    }
}

/// 会话内可变仿真状态；所有 reader 输出和 UI 请求通过同一锁保持顺序。
struct SessionState {
    engine: TerminalEngine,
    info: SessionInfo,
    /// Only proves read access after a close attempt that began from an ended lifecycle.
    close_history_proof: Option<CloseHistoryProof>,
}

/// Binds post-close history access to the exact ended owner and transport generation.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CloseHistoryProof {
    origin: SessionLifecycle,
    owner: SessionOwner,
    shell_session_generation: u64,
    transport_generation: u64,
    recorder_cleanup_failed: bool,
}

/// 一次 opt-in resize 的关联键和两端尺寸；session 序号不暴露随机 SessionId。
#[derive(Clone, Copy)]
struct ResizeDiagnosticContext {
    session_sequence: u64,
    transport_generation: u64,
    resize_generation: u64,
    native_process_id: u32,
    transport_kind: u64,
    old_size: TerminalSize,
    new_size: TerminalSize,
}

/// resize 边界仅保存 cursor 坐标、尺寸、revision 与 reader 水位，不携带可见文本。
#[derive(Clone, Copy)]
struct ResizeDiagnosticCursor {
    row: u16,
    column: u16,
    revision: u64,
    size: TerminalSize,
    reader_sequence: u64,
}

/// reader 诊断只保存同一会话的数字序号和 chunk 长度，不保存输出字节。
#[derive(Clone, Copy)]
struct ResizeReaderDiagnosticContext {
    session_sequence: u64,
    transport_generation: u64,
    resize_generation: u64,
    reader_sequence: u64,
    chunk_bytes: u64,
}

/// 用 TerminalEngine 的纯 getter 采样可见光标，不改变用户 scrollback viewport 或缓存。
///
/// @author kongweiguang
fn sample_resize_diagnostic_cursor(
    engine: &TerminalEngine,
    reader_sequence: u64,
) -> ResizeDiagnosticCursor {
    let (row, column) = engine.diagnostic_cursor_position();
    ResizeDiagnosticCursor {
        row,
        column,
        revision: engine.revision(),
        size: engine.size(),
        reader_sequence,
    }
}

/// 记录 engine resize 前后数值，不写入行内容、命令、标题或原始 VT 字节。
/// phase 固定为 0/1=engine 前后、2/3/4=主 resize 请求/ACK/错误、5/6/7=补偿请求/ACK/错误；
/// transport_kind 为 0=Local、1=SSH；local PTY adapter 使用 8/9/10 表示 native 调用/ACK/错误，
/// SSH reader 的 20/21 表示有限样本的 process_output 前/后。
///
/// @author kongweiguang
fn log_resize_engine_diagnostic(
    context: ResizeDiagnosticContext,
    phase: u64,
    cursor: ResizeDiagnosticCursor,
) {
    tracing::info!(
        event = "TERMINAL_RESIZE_DIAGNOSTIC",
        phase,
        session_sequence = context.session_sequence,
        transport_generation = context.transport_generation,
        resize_generation = context.resize_generation,
        transport_kind = context.transport_kind,
        native_process_id = context.native_process_id,
        reader_sequence = cursor.reader_sequence,
        engine_columns = u64::from(cursor.size.columns()),
        engine_rows = u64::from(cursor.size.rows()),
        old_columns = u64::from(context.old_size.columns()),
        old_rows = u64::from(context.old_size.rows()),
        old_pixel_width = u64::from(context.old_size.pixel_width()),
        old_pixel_height = u64::from(context.old_size.pixel_height()),
        new_columns = u64::from(context.new_size.columns()),
        new_rows = u64::from(context.new_size.rows()),
        new_pixel_width = u64::from(context.new_size.pixel_width()),
        new_pixel_height = u64::from(context.new_size.pixel_height()),
        cursor_row = u64::from(cursor.row),
        cursor_column = u64::from(cursor.column),
        engine_revision = cursor.revision,
    );
}

/// 记录 session transport 的请求、ACK、错误与补偿目标；错误正文留在调用链，不写入日志。
///
/// @author kongweiguang
fn log_resize_transport_diagnostic(
    context: ResizeDiagnosticContext,
    phase: u64,
    target: TerminalSize,
    elapsed: Option<Duration>,
    reader_sequence: u64,
) {
    let elapsed_ms = elapsed
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or_default();
    tracing::info!(
        event = "TERMINAL_RESIZE_DIAGNOSTIC",
        phase,
        session_sequence = context.session_sequence,
        transport_generation = context.transport_generation,
        resize_generation = context.resize_generation,
        transport_kind = context.transport_kind,
        native_process_id = context.native_process_id,
        reader_sequence,
        old_columns = u64::from(context.old_size.columns()),
        old_rows = u64::from(context.old_size.rows()),
        new_columns = u64::from(context.new_size.columns()),
        new_rows = u64::from(context.new_size.rows()),
        target_columns = u64::from(target.columns()),
        target_rows = u64::from(target.rows()),
        target_pixel_width = u64::from(target.pixel_width()),
        target_pixel_height = u64::from(target.pixel_height()),
        elapsed_present = u64::from(elapsed.is_some()),
        elapsed_ms,
    );
}

/// 记录有限 reader 样本的网格尺寸与 revision；阶段 20/21 分别为处理前后。
///
/// @author kongweiguang
fn log_resize_reader_diagnostic(
    context: ResizeReaderDiagnosticContext,
    phase: u64,
    size: TerminalSize,
    revision: u64,
) {
    tracing::info!(
        event = "TERMINAL_RESIZE_DIAGNOSTIC",
        phase,
        transport_kind = 1_u64,
        session_sequence = context.session_sequence,
        transport_generation = context.transport_generation,
        resize_generation = context.resize_generation,
        reader_sequence = context.reader_sequence,
        chunk_bytes = context.chunk_bytes,
        engine_columns = u64::from(size.columns()),
        engine_rows = u64::from(size.rows()),
        engine_revision = revision,
    );
}

/// 每次 SSH resize 重置固定预算，限制诊断输出为最多 16 个 read chunk。
///
/// @author kongweiguang
fn reset_resize_reader_diagnostic_budget(remaining: &AtomicU64) {
    remaining.store(MAX_RESIZE_DIAGNOSTIC_READER_CHUNKS, Ordering::Release);
}

/// 以 CAS 只领取有限 reader 样本；耗尽后热路径不积累队列或分配诊断事件。
///
/// @author kongweiguang
fn claim_resize_reader_diagnostic_chunk(remaining: &AtomicU64) -> bool {
    remaining
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_sub(1)
        })
        .is_ok()
}

/// CPR 记录只保留坐标和进程内 session/transport/resize 序号，便于关联而不暴露会话 ID。
///
/// @author kongweiguang
fn log_resize_cpr_diagnostic(
    session_sequence: u64,
    transport_generation: u64,
    resize_generation: u64,
    row: u16,
    column: u16,
) {
    tracing::info!(
        event = "TERMINAL_CPR_DIAGNOSTIC",
        session_sequence,
        transport_generation,
        resize_generation,
        cpr_row = u64::from(row),
        cpr_column = u64::from(column),
    );
}

/// 仅识别 engine 生成的完整一基 CSI row;column R 回复；其它 VT 响应一律不采样。
///
/// @author kongweiguang
fn parse_cursor_position_report(bytes: &[u8]) -> Option<(u16, u16)> {
    let coordinates = bytes.strip_prefix(b"\x1b[")?.strip_suffix(b"R")?;
    let separator = coordinates.iter().position(|byte| *byte == b';')?;
    if coordinates[separator + 1..].contains(&b';') {
        return None;
    }
    let row = parse_cpr_coordinate(&coordinates[..separator])?;
    let column = parse_cpr_coordinate(&coordinates[separator + 1..])?;
    Some((row, column))
}

/// 将 CPR 的十进制坐标约束到非零 u16，拒绝额外字符、溢出和非规范响应类型。
///
/// @author kongweiguang
fn parse_cpr_coordinate(digits: &[u8]) -> Option<u16> {
    if digits.is_empty() || digits.iter().any(|digit| !digit.is_ascii_digit()) {
        return None;
    }
    let value = digits.iter().try_fold(0_u32, |value, digit| {
        value
            .checked_mul(10)?
            .checked_add(u32::from(digit.saturating_sub(b'0')))
    })?;
    u16::try_from(value)
        .ok()
        .filter(|coordinate| *coordinate > 0)
}

/// 每次 resize 最多关联一条后续 CPR，避免把之后无关的终端光标查询持续归因给旧尺寸变更。
///
/// @author kongweiguang
fn claim_resize_cpr_diagnostic_generation(pending: &AtomicU64, generation: u64) -> bool {
    generation > 0
        && pending
            .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
}

/// 仅在短 state 锁内把 VT 网格恢复到最后 transport 确认尺寸，并发布新 revision；transport 补偿由调用方决定是否安全。
/// 分离两步是为了自然退出后只恢复历史网格，不对已结束 control 发请求或把正常退出升级成基础设施故障。
///
/// @author kongweiguang
async fn restore_resize_engine(
    session: &ManagedSession,
    confirmed_size: TerminalSize,
) -> AppResult<()> {
    let mut state = session.state.lock().await;
    state.engine.resize(confirmed_size).map_err(engine_error)?;
    session.changes.send_replace(SessionChange {
        revision: state.engine.revision(),
        kind: SessionChangeKind::Resized,
    });
    Ok(())
}

/// 只有 Exited/Closed 才证明 resize 已没有可安全调用的活 transport；Failed 保留真实故障分类。
///
/// @author kongweiguang
fn resize_lifecycle_is_ended(lifecycle: SessionLifecycle) -> bool {
    matches!(
        lifecycle,
        SessionLifecycle::Exited | SessionLifecycle::Closed
    )
}

/// 通过领域状态机推进同一 session incarnation，拒绝关闭后到达的 reader 回执。
///
/// @author kongweiguang
fn transition_session_lifecycle(
    state: &mut SessionState,
    event: SessionLifecycleEvent,
) -> Result<(), kerminal_domain::sessions::SessionLifecycleTransitionError> {
    let next = state.info.lifecycle.transition(event)?;
    if matches!(
        event,
        SessionLifecycleEvent::ReconnectStarted | SessionLifecycleEvent::RestartStarted
    ) {
        state.close_history_proof = None;
    }
    state.info.lifecycle = next;
    Ok(())
}

/// Captures history eligibility before Closing is published; retries preserve proof, while live closes clear it.
///
/// @author kongweiguang
fn prepare_close_history_proof(
    state: &mut SessionState,
    shell_session_generation: Option<u64>,
    transport_generation: u64,
) -> Option<CloseHistoryProof> {
    match state.info.lifecycle {
        SessionLifecycle::Exited | SessionLifecycle::Failed => {
            state.close_history_proof =
                shell_session_generation.map(|generation| CloseHistoryProof {
                    origin: state.info.lifecycle,
                    owner: state.info.owner.clone(),
                    shell_session_generation: generation,
                    transport_generation,
                    recorder_cleanup_failed: false,
                });
        }
        SessionLifecycle::Starting | SessionLifecycle::Running => {
            state.close_history_proof = None;
        }
        SessionLifecycle::Closing | SessionLifecycle::Closed => {}
    }
    state.close_history_proof.clone()
}

/// Centralizes bound history admission; Closed is readable only while its ended-origin cleanup owner remains.
///
/// @author kongweiguang
fn ui_history_access_is_current(
    session: &ManagedSession,
    state: &SessionState,
    shell: &ShellIntegrationState,
    binding: &UiSessionBinding,
) -> bool {
    let visible_owner = matches!(
        &state.info.owner,
        SessionOwner::Ui { .. } | SessionOwner::Agent { .. }
    );
    if !visible_owner
        || state.info.owner != binding.owner
        || shell.session_generation != binding.session_generation
    {
        return false;
    }

    match state.info.lifecycle {
        SessionLifecycle::Running | SessionLifecycle::Exited | SessionLifecycle::Failed => true,
        SessionLifecycle::Closed => state.close_history_proof.as_ref().is_some_and(|proof| {
            matches!(
                proof.origin,
                SessionLifecycle::Exited | SessionLifecycle::Failed
            ) && proof.recorder_cleanup_failed
                && proof.owner == binding.owner
                && proof.shell_session_generation == binding.session_generation
                && proof.transport_generation
                    == session.transport_generation.load(Ordering::Acquire)
        }),
        SessionLifecycle::Starting | SessionLifecycle::Closing => false,
    }
}

/// 同一原子选择提交支持旧 revision CAS 与可随输出重基准的 viewport 两种坐标来源。
///
/// @author kongweiguang
enum UiSelectionCoordinates {
    Revision(Option<OutputRevision>),
    Viewport(SelectionViewport),
}

/// 在可见 owner、session generation 与 engine 锁保持期间提交选择，避免迟到 UI 手势污染新会话。
///
/// 取消检查必须位于锁之后；坐标校验失败不改写确认选择，也不发布绘制 revision。
///
/// @author kongweiguang
async fn apply_ui_selection(
    session: &ManagedSession,
    binding: &UiSessionBinding,
    selection: Option<TerminalSelection>,
    gesture_id: Option<u64>,
    coordinates: UiSelectionCoordinates,
    cancellation: CancellationToken,
) -> AppResult<()> {
    let mut state = session.state.lock().await;
    let shell = session.shell_integration.lock().await;
    if !ui_history_access_is_current(session, &state, &shell, binding) {
        return Err(AppError::conflict("可见终端选择目标或 generation 已失效"));
    }
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("终端选择更新已取消"));
    }

    match (gesture_id, selection) {
        (_, None) => state.engine.set_selection(None),
        (Some(gesture_id), Some(selection)) => {
            let accepted = match coordinates {
                UiSelectionCoordinates::Revision(expected_revision) => state
                    .engine
                    .update_selection_gesture_at_revision(gesture_id, selection, expected_revision),
                UiSelectionCoordinates::Viewport(viewport) => state
                    .engine
                    .update_selection_gesture_in_viewport(gesture_id, selection, viewport),
            };
            if !accepted {
                return Err(AppError::conflict("终端内容或选区已变化，请重新拖选"));
            }
        }
        (None, Some(selection)) => state.engine.set_selection(Some(selection)),
    }

    session.changes.send_replace(SessionChange {
        revision: state.engine.revision(),
        kind: SessionChangeKind::Output,
    });
    Ok(())
}

/// transport 读写、协议状态和合并 watch 的单一共享会话对象。
struct ManagedSession {
    session_id: SessionId,
    transport_generation: AtomicU64,
    /// Old reader failures are ignored only while their generation remains the retained retirement owner.
    retiring_transport_generation: Mutex<Option<u64>>,
    resize_diagnostic_session_sequence: u64,
    resize_diagnostic_generation: AtomicU64,
    resize_diagnostic_cpr_generation: AtomicU64,
    resize_diagnostic_reader_budget: AtomicU64,
    native_exit_deadline: Mutex<Option<Instant>>,
    process_id: RwLock<Option<u32>>,
    shell_integration_enabled: RwLock<bool>,
    shell_dialect: RwLock<RemoteShellDialect>,
    state: Mutex<SessionState>,
    writer: Mutex<std::pin::Pin<Box<dyn AsyncWrite + Send + 'static>>>,
    writer_shutdown: AtomicBool,
    control: RwLock<Arc<dyn SessionTransportControl>>,
    cancellation: Mutex<CancellationToken>,
    changes: watch::Sender<SessionChange>,
    input_gate: Mutex<()>,
    shell_integration: Mutex<ShellIntegrationState>,
    shell_events: broadcast::Sender<ShellRuntimeEvent>,
    child_exit_task: Mutex<Option<JoinHandle<()>>>,
    reader_task: Mutex<Option<JoinHandle<()>>>,
    output_sequence: AtomicU64,
    auto_reconnect: Mutex<AutoReconnectRuntime>,
    auto_reconnect_changed: Notify,
    _slot: OwnedSemaphorePermit,
    operation_gate: Mutex<()>,
}

/// 同一 ManagedSession 唯一持有的重连退避计数、失败通知与 joinable supervisor。
///
/// @author kongweiguang
#[derive(Default)]
struct AutoReconnectRuntime {
    attempts_started: u8,
    cancellation: Option<CancellationToken>,
    task: Option<JoinHandle<()>>,
    pending_failure: Option<AppError>,
}

/// 每条 session 仅保留有限的协议状态和至多一个应用命令租期。
///
/// @author kongweiguang
#[derive(Default)]
struct ShellIntegrationState {
    observed: bool,
    prompt_started: bool,
    prompt_ready: bool,
    prompt_line_dirty: bool,
    external_input_since_prompt: bool,
    ui_input_projection_valid: bool,
    prompt_submission_sent: bool,
    sequence: u64,
    session_generation: u64,
    prompt_epoch: u64,
    input_generation: u64,
    pending_input_write: Option<PendingTerminalInputWrite>,
    command_blocks: VecDeque<CommandBlock>,
    command_block_captures: HashMap<CommandBlockId, CommandBlockCapture>,
    evicted_command_blocks: VecDeque<CommandBlockId>,
    command_block_revision: u64,
    next_command_block_ordinal: u64,
    active_command_block: Option<CommandBlockId>,
    pending_command_submission: Option<PendingCommandBlockSubmission>,
    active_command: Option<ActiveShellCommand>,
}

/// 将应用块映射到当前 TerminalEngine 的不透明转录 ID；不向业务 DTO 暴露 engine 句柄。
///
/// @author kongweiguang
#[derive(Clone, Copy)]
enum CommandBlockCapture {
    Captured(TranscriptCaptureId),
    Evicted,
}

/// 一次已校验但尚未完成 transport 写入的输入意图；仅完整送入的一字节 Enter 可跨 OSC 边界等待原 flush。
///
/// @author kongweiguang
struct PendingTerminalInputWrite {
    generation: u64,
    cancellation: CancellationToken,
    bytes_written: Arc<AtomicUsize>,
    expected_bytes: usize,
    session_generation: u64,
    prompt_epoch: u64,
    block_id: Option<CommandBlockId>,
    command_finished: bool,
    prompt_started_after_command: bool,
    prompt_ended_after_command: bool,
}

/// 只暂存一条与 UI generation 对齐的正文，等 writer flush 和真实 OSC C 两项证据都到齐。
///
/// @author kongweiguang
struct PendingCommandBlockSubmission {
    session_generation: u64,
    prompt_epoch: u64,
    input_generation: u64,
    command: CommandBlockText,
    acknowledged: bool,
    block_id: Option<CommandBlockId>,
}

impl ShellIntegrationState {
    /// 单调推进命令块版本，使合并订阅丢事件后仍可通过快照发现变化。
    ///
    /// @author kongweiguang
    fn bump_command_block_revision(&mut self) {
        self.command_block_revision = self.command_block_revision.wrapping_add(1);
    }

    /// 只终结当前真实 OSC C 对应的 Running 块；查找与状态校验均成功后才推进 revision，不推测退出码。
    ///
    /// @author kongweiguang
    fn mark_active_command_unknown(&mut self, reason: CommandBlockUnknownReason) -> bool {
        let Some(id) = self.active_command_block.take() else {
            return false;
        };
        if let Some(block) = self.command_blocks.iter_mut().find(|block| block.id == id)
            && block.state == CommandBlockState::Running
        {
            block.state = CommandBlockState::Unknown(reason);
            self.bump_command_block_revision();
            return true;
        }
        false
    }

    /// 生命周期退出时丢弃尚未关联的正文候选，并将无 D 证明的 Running 块转成 Unknown。
    ///
    /// @author kongweiguang
    fn end_command_tracking(&mut self, reason: CommandBlockUnknownReason) -> bool {
        self.pending_command_submission = None;
        self.mark_active_command_unknown(reason)
    }

    /// 重连时清除旧 prompt projection，保留递增计数并推进 generation，拒绝旧 UI 结果。
    ///
    /// @author kongweiguang
    fn reset_for_reconnect(&mut self) {
        if let Some(pending) = self.pending_input_write.take() {
            pending.cancellation.cancel();
        }
        self.mark_active_command_unknown(CommandBlockUnknownReason::Reconnected);
        self.pending_command_submission = None;
        let sequence = self.sequence;
        let session_generation = self.session_generation.wrapping_add(1);
        let prompt_epoch = self.prompt_epoch;
        let input_generation = self.input_generation;
        let command_blocks = std::mem::take(&mut self.command_blocks);
        let command_block_captures = std::mem::take(&mut self.command_block_captures);
        let evicted_command_blocks = std::mem::take(&mut self.evicted_command_blocks);
        let command_block_revision = self.command_block_revision.wrapping_add(1);
        let next_command_block_ordinal = self.next_command_block_ordinal;
        *self = Self::default();
        self.sequence = sequence;
        self.session_generation = session_generation;
        self.prompt_epoch = prompt_epoch;
        self.input_generation = input_generation;
        self.command_blocks = command_blocks;
        self.command_block_captures = command_block_captures;
        self.evicted_command_blocks = evicted_command_blocks;
        self.command_block_revision = command_block_revision;
        self.next_command_block_ordinal = next_command_block_ordinal;
    }

    /// OSC C 是唯一命令块起点；只关联同一 prompt/input generation 的 UI 候选正文。
    ///
    /// @author kongweiguang
    fn begin_command_block(&mut self) -> Option<CommandBlockId> {
        self.mark_active_command_unknown(CommandBlockUnknownReason::Superseded);
        self.next_command_block_ordinal = self.next_command_block_ordinal.wrapping_add(1);
        let id = CommandBlockId {
            session_generation: self.session_generation,
            prompt_epoch: self.prompt_epoch,
            ordinal: self.next_command_block_ordinal,
        };
        let candidate_matches = self
            .pending_command_submission
            .as_ref()
            .is_some_and(|pending| {
                pending.session_generation == self.session_generation
                    && pending.prompt_epoch == self.prompt_epoch
                    && pending.input_generation == self.input_generation
                    && pending.block_id.is_none()
            });
        let command = match self.pending_command_submission.as_mut() {
            Some(pending) if candidate_matches => {
                pending.block_id = Some(id);
                if pending.acknowledged {
                    pending.command.clone()
                } else {
                    CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged)
                }
            }
            _ => {
                self.pending_command_submission = None;
                CommandBlockText::Unknown(CommandBlockTextUnknownReason::NotCaptured)
            }
        };
        self.command_blocks.push_back(CommandBlock {
            id,
            command,
            state: CommandBlockState::Running,
        });
        while self.command_blocks.len() > COMMAND_BLOCK_RETENTION_LIMIT {
            if let Some(evicted) = self.command_blocks.pop_front() {
                self.command_block_captures.remove(&evicted.id);
                self.remember_evicted_command_block(evicted.id);
            }
        }
        self.active_command_block = Some(id);
        self.bump_command_block_revision();
        let associated_id = candidate_matches.then_some(id);
        if self
            .pending_command_submission
            .as_ref()
            .is_some_and(|pending| pending.acknowledged)
        {
            self.pending_command_submission = None;
        }
        associated_id
    }

    /// OSC D 只更新当前未结束的 C 块，退出码缺失时保留 Finished(None) 而非伪造成功。
    ///
    /// @author kongweiguang
    fn finish_command_block(&mut self, exit_code: Option<i32>) {
        let Some(id) = self.active_command_block.take() else {
            if self
                .pending_command_submission
                .as_ref()
                .is_some_and(|pending| pending.block_id.is_none())
            {
                self.pending_command_submission = None;
            }
            return;
        };
        if let Some(block) = self.command_blocks.iter_mut().find(|block| block.id == id) {
            block.state = CommandBlockState::Finished { exit_code };
            self.bump_command_block_revision();
        }
    }

    /// 仅保留最近固定数量的失效 block ID，使迟到 UI 查询明确看到 Evicted 而非空成功。
    ///
    /// @author kongweiguang
    fn remember_evicted_command_block(&mut self, id: CommandBlockId) {
        if !self.evicted_command_blocks.contains(&id) {
            self.evicted_command_blocks.push_back(id);
        }
        while self.evicted_command_blocks.len() > COMMAND_BLOCK_RETENTION_LIMIT {
            self.evicted_command_blocks.pop_front();
        }
    }
}

/// 当前租期的 ID 与取消原因只在 manager 内部绑定，不从用户命令字符串推断相关性。
struct ActiveShellCommand {
    command_id: String,
    cancellation: CancellationToken,
    interruption: Arc<AtomicU8>,
    injected: bool,
    started: bool,
}

/// 广播事件携带 monotonic per-session 序列和仅由活跃租期产生的 request ID 关联。
#[derive(Clone, Debug)]
struct ShellRuntimeEvent {
    sequence: u64,
    marker: ShellIntegrationMarker,
    command_id: Option<String>,
}

/// 异步任务被取消或 future 被 drop 时中断当前 lease，防止遗留的成功关联状态。
struct CancelShellCommandOnDrop {
    cancellation: CancellationToken,
    armed: bool,
}

impl CancelShellCommandOnDrop {
    /// 正常完成且已清理 active marker 后解除 drop 取消。
    ///
    /// @author kongweiguang
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CancelShellCommandOnDrop {
    /// future 被 runtime 丢弃时通知等待逻辑，不把仍运行的命令伪装为完成。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        if self.armed {
            self.cancellation.cancel();
        }
    }
}

/// 按引擎提供的有序事件推进 OSC 状态与转录关联；flush ticket 与 capture 仍只归属原命令和 generation。
///
/// @author kongweiguang
async fn apply_terminal_output_events(session: &ManagedSession, events: &[TerminalOutputEvent]) {
    let mut shell = session.shell_integration.lock().await;
    for event in events {
        let (marker, capture_id) = match event {
            TerminalOutputEvent::ShellMarker { marker, capture_id } => (*marker, *capture_id),
            TerminalOutputEvent::TranscriptChanged { .. } => continue,
            TerminalOutputEvent::TranscriptEvicted { capture_id } => {
                for capture in shell.command_block_captures.values_mut() {
                    if matches!(capture, CommandBlockCapture::Captured(id) if id == capture_id) {
                        *capture = CommandBlockCapture::Evicted;
                    }
                }
                continue;
            }
        };
        // 只让完整写入的一字节 Enter 对应的同一命令块 C→D→A→B 保留原 flush ticket。
        let keep_command_write = pending_command_write_accepts_marker(&shell, marker);
        let cancelled_input = shell.pending_input_write.as_ref().is_some_and(|pending| {
            if keep_command_write {
                false
            } else {
                pending.cancellation.cancel();
                true
            }
        });
        if cancelled_input
            && shell
                .pending_command_submission
                .as_ref()
                .is_some_and(|pending| pending.block_id.is_none())
        {
            shell.pending_command_submission = None;
        }
        if shell
            .active_command
            .as_ref()
            .is_some_and(|active| active.cancellation.is_cancelled())
        {
            shell.active_command = None;
        }
        let command_id = match marker {
            ShellIntegrationMarker::PromptStart => {
                shell.mark_active_command_unknown(CommandBlockUnknownReason::Superseded);
                if shell
                    .pending_command_submission
                    .as_ref()
                    .is_some_and(|pending| pending.block_id.is_none())
                {
                    shell.pending_command_submission = None;
                }
                shell.observed = true;
                shell.prompt_started = true;
                shell.prompt_ready = false;
                shell.prompt_line_dirty = false;
                shell.external_input_since_prompt = false;
                shell.ui_input_projection_valid = true;
                shell.prompt_submission_sent = false;
                shell.prompt_epoch = shell.prompt_epoch.wrapping_add(1);
                if keep_command_write && let Some(input) = shell.pending_input_write.as_mut() {
                    input.prompt_started_after_command = true;
                }
                None
            }
            ShellIntegrationMarker::PromptEnd => {
                if shell.prompt_started {
                    shell.prompt_ready = true;
                }
                if keep_command_write && let Some(input) = shell.pending_input_write.as_mut() {
                    input.prompt_ended_after_command = true;
                }
                None
            }
            ShellIntegrationMarker::CommandStart => {
                let submission_block_id = shell.begin_command_block();
                if let (Some(block_id), Some(capture_id)) = (shell.active_command_block, capture_id)
                    && block_id.session_generation == shell.session_generation
                {
                    shell
                        .command_block_captures
                        .insert(block_id, CommandBlockCapture::Captured(capture_id));
                }
                if keep_command_write
                    && let (Some(block_id), Some(input)) =
                        (submission_block_id, shell.pending_input_write.as_mut())
                {
                    input.block_id = Some(block_id);
                }
                shell.prompt_started = false;
                shell.prompt_ready = false;
                shell.prompt_submission_sent = false;
                shell.active_command.as_mut().and_then(|active| {
                    if active.injected && !active.started {
                        active.started = true;
                        Some(active.command_id.clone())
                    } else {
                        None
                    }
                })
            }
            ShellIntegrationMarker::CommandFinished { exit_code } => {
                shell.finish_command_block(exit_code);
                if keep_command_write && let Some(input) = shell.pending_input_write.as_mut() {
                    input.command_finished = true;
                }
                shell.prompt_started = false;
                shell.prompt_ready = false;
                shell
                    .active_command
                    .as_ref()
                    .and_then(|active| active.started.then(|| active.command_id.clone()))
            }
        };
        if cancelled_input {
            // 前缀可能已到达 PTY，因此保守地让新 prompt 重新确认输入状态。
            shell.external_input_since_prompt = true;
            shell.ui_input_projection_valid = false;
            shell.prompt_line_dirty = true;
        }
        shell.sequence = shell.sequence.wrapping_add(1);
        let _ = session.shell_events.send(ShellRuntimeEvent {
            sequence: shell.sequence,
            marker,
            command_id,
        });
    }
}

/// 只保留匹配同一单字节 Enter 与命令块的 flush ticket，拒绝跨 prompt 或无正文候选的输入。
///
/// @author kongweiguang
fn pending_command_write_accepts_marker(
    shell: &ShellIntegrationState,
    marker: ShellIntegrationMarker,
) -> bool {
    let Some(input) = shell.pending_input_write.as_ref() else {
        return false;
    };
    if input.cancellation.is_cancelled()
        || input.expected_bytes != 1
        || input.bytes_written.load(Ordering::Acquire) != input.expected_bytes
    {
        return false;
    }
    let Some(submission) = shell.pending_command_submission.as_ref() else {
        return false;
    };
    if input.generation != submission.input_generation
        || input.session_generation != submission.session_generation
        || input.prompt_epoch != submission.prompt_epoch
    {
        return false;
    }
    match marker {
        ShellIntegrationMarker::CommandStart => {
            input.block_id.is_none()
                && submission.block_id.is_none()
                && input.session_generation == shell.session_generation
                && input.prompt_epoch == shell.prompt_epoch
                && submission.session_generation == shell.session_generation
                && submission.prompt_epoch == shell.prompt_epoch
                && submission.input_generation == shell.input_generation
        }
        ShellIntegrationMarker::CommandFinished { .. } => {
            input.block_id.is_some()
                && input.block_id == shell.active_command_block
                && submission.block_id == input.block_id
                && input.session_generation == shell.session_generation
                && input.prompt_epoch == shell.prompt_epoch
                && !input.command_finished
        }
        ShellIntegrationMarker::PromptStart => {
            input.block_id.is_some()
                && submission.block_id == input.block_id
                && input.command_finished
                && input.session_generation == shell.session_generation
                && input.prompt_epoch == shell.prompt_epoch
                && !input.prompt_started_after_command
        }
        ShellIntegrationMarker::PromptEnd => {
            input.block_id.is_some()
                && submission.block_id == input.block_id
                && input.command_finished
                && input.prompt_started_after_command
                && input.session_generation == shell.session_generation
                && input.prompt_epoch.wrapping_add(1) == shell.prompt_epoch
                && !input.prompt_ended_after_command
                && shell.prompt_started
        }
    }
}

/// 生成可用于 inline suggestion 的有界快照；命令行文本始终只由 UI 的 typed input 维护。
///
/// @author kongweiguang
async fn terminal_command_context_for(session: &ManagedSession) -> TerminalCommandContext {
    let shell_dialect = *session.shell_dialect.read().await;
    let shell_integration_enabled = *session.shell_integration_enabled.read().await;
    let state = session.state.lock().await;
    let shell = session.shell_integration.lock().await;
    terminal_command_context_from_state(
        session,
        &state,
        &shell,
        shell_dialect,
        shell_integration_enabled,
    )
}

/// 从 reader 与 UI 写入共用的短锁快照构造可验证上下文，不读取终端文本。
///
/// @author kongweiguang
fn terminal_command_context_from_state(
    session: &ManagedSession,
    state: &SessionState,
    shell: &ShellIntegrationState,
    shell_dialect: RemoteShellDialect,
    shell_integration_enabled: bool,
) -> TerminalCommandContext {
    let owner = state.info.owner.clone();
    let lifecycle = state.info.lifecycle;
    let alternate_screen = state.engine.input_modes().alternate_screen;
    let active_command = shell
        .active_command
        .as_ref()
        .is_some_and(|active| !active.cancellation.is_cancelled());
    let prompt_ready = shell.prompt_ready;
    let prompt_line_empty = prompt_ready && !shell.prompt_line_dirty;
    let unavailable_reason =
        if !matches!(&owner, kerminal_domain::sessions::SessionOwner::Ui { .. }) {
            Some(TerminalCommandUnavailableReason::NonUiOwner)
        } else if lifecycle != SessionLifecycle::Running {
            Some(TerminalCommandUnavailableReason::SessionNotRunning)
        } else if !shell_integration_enabled {
            Some(TerminalCommandUnavailableReason::ShellIntegrationDisabled)
        } else if !shell.observed {
            Some(TerminalCommandUnavailableReason::ShellIntegrationNotObserved)
        } else if alternate_screen {
            Some(TerminalCommandUnavailableReason::AlternateScreen)
        } else if shell_dialect == RemoteShellDialect::Unknown {
            Some(TerminalCommandUnavailableReason::UnsupportedShellDialect)
        } else if !prompt_ready {
            Some(TerminalCommandUnavailableReason::PromptNotReady)
        } else if shell.pending_input_write.is_some() {
            Some(TerminalCommandUnavailableReason::InputWritePending)
        } else if shell.external_input_since_prompt {
            Some(TerminalCommandUnavailableReason::ExternalInputObserved)
        } else if !shell.ui_input_projection_valid {
            Some(TerminalCommandUnavailableReason::UiInputProjectionInvalidated)
        } else if shell.prompt_submission_sent {
            Some(TerminalCommandUnavailableReason::PromptNotReady)
        } else if active_command {
            Some(TerminalCommandUnavailableReason::ManagedCommandActive)
        } else {
            None
        };

    TerminalCommandContext {
        session_id: session.session_id.clone(),
        owner,
        session_generation: shell.session_generation,
        prompt_epoch: shell.prompt_epoch,
        input_generation: shell.input_generation,
        shell_dialect,
        prompt_ready,
        prompt_line_empty,
        alternate_screen,
        availability: unavailable_reason.map_or(
            TerminalCommandAvailability::Available,
            TerminalCommandAvailability::Unavailable,
        ),
    }
}

#[derive(Clone, Copy)]
enum ShellInputOrigin {
    Ui {
        projection: UiInputProjection,
        submitted: bool,
    },
    External,
}

/// 递增共享输入代数并保留 workflow 空行约束；raw MCP/Agent 输入使 UI 建议上下文失效。
///
/// @author kongweiguang
fn mark_shell_input(shell: &mut ShellIntegrationState, origin: ShellInputOrigin) -> u64 {
    // 任意后续输入会改变候选正文与 PTY wire 顺序的对应关系；专用 UI submit 会在此后重新登记。
    shell.pending_command_submission = None;
    shell.input_generation = shell.input_generation.wrapping_add(1);
    if shell.prompt_started || shell.prompt_ready {
        shell.prompt_line_dirty = true;
    }
    match origin {
        ShellInputOrigin::External => shell.external_input_since_prompt = true,
        ShellInputOrigin::Ui {
            projection,
            submitted,
        } => {
            if projection == UiInputProjection::Invalidates {
                shell.ui_input_projection_valid = false;
            }
            if submitted {
                shell.prompt_submission_sent = true;
            }
        }
    }
    shell.input_generation
}

/// 为有界后台写入建立准入回执，并发布字节游标以区分未发送、部分发送与完整输入。
///
/// @author kongweiguang
async fn write_session_input_with_receipt(
    session: Arc<ManagedSession>,
    bytes: Vec<u8>,
) -> AppResult<TerminalWriteReceipt> {
    let input_gate = match session.input_gate.try_lock() {
        Ok(guard) => guard,
        Err(_) => {
            interrupt_active_shell_command(&session, COMMAND_INTERRUPT_USER_INPUT).await;
            return Err(AppError::conflict("终端正在执行受管命令，输入未发送"));
        }
    };
    let shell_dialect = *session.shell_dialect.read().await;
    let shell_integration_enabled = *session.shell_integration_enabled.read().await;
    let cancellation = CancellationToken::new();
    let bytes_written = Arc::new(AtomicUsize::new(0));
    let (input_generation, admission) = {
        let state = session.state.lock().await;
        if state.info.lifecycle != SessionLifecycle::Running {
            return Err(AppError::conflict("终端会话当前不可写入"));
        }
        if bytes.is_empty() {
            (None, None)
        } else {
            let mut shell = session.shell_integration.lock().await;
            if shell.pending_input_write.is_some() {
                return Err(AppError::conflict("上一条终端输入仍在有界 writer 队列中"));
            }
            let input_generation_before = shell.input_generation;
            let mut admission = TerminalWriteAdmission {
                session_id: session.session_id.clone(),
                owner: state.info.owner.clone(),
                lifecycle: state.info.lifecycle,
                session_generation: shell.session_generation,
                prompt_epoch: shell.prompt_epoch,
                input_generation_before,
                accepted_input_generation: input_generation_before,
                shell_dialect,
                shell_integration_enabled,
                shell_integration_observed: shell.observed,
                prompt_ready: shell.prompt_ready,
                prompt_line_empty: shell.prompt_ready && !shell.prompt_line_dirty,
                alternate_screen: state.engine.input_modes().alternate_screen,
            };
            let session_generation = shell.session_generation;
            let prompt_epoch = shell.prompt_epoch;
            let generation = mark_shell_input(&mut shell, ShellInputOrigin::External);
            admission.accepted_input_generation = generation;
            shell.pending_input_write = Some(PendingTerminalInputWrite {
                generation,
                cancellation: cancellation.clone(),
                bytes_written: Arc::clone(&bytes_written),
                expected_bytes: bytes.len(),
                session_generation,
                prompt_epoch,
                block_id: None,
                command_finished: false,
                prompt_started_after_command: false,
                prompt_ended_after_command: false,
            });
            (Some(generation), Some(admission))
        }
    };
    drop(input_gate);

    let acknowledged_bytes = bytes.len();
    let result = write_input_bytes(&session, &bytes, &cancellation, &bytes_written).await;
    if let Some(generation) = input_generation {
        finish_terminal_input_write(&session, generation, result.is_err()).await;
    }
    result?;
    Ok(TerminalWriteReceipt {
        acknowledged_bytes,
        admission,
    })
}

/// UI 写入在同一 session gate 内完成身份检查、输入代次登记和一次有序 writer 调用。
///
/// 普通输入按需验证建议上下文；命令块提交与命令面板提交跳过可选投影门禁，但仍共享 owner/generation CAS。
/// 命令面板正文和 Enter 先按当前 terminal mode 编码，再作为单次 writer 输入提交。
///
/// @author kongweiguang
async fn write_ui_input_inner(
    session: Arc<ManagedSession>,
    request: UiInputWriteRequest,
) -> AppResult<TerminalCommandContext> {
    write_ui_input_inner_with_receipt(session, request)
        .await
        .map(|receipt| receipt.context)
}

/// 共享 owner/input gate 的实际写路径，并把命令候选与匹配的输入代次作为一次性 receipt 返回。
/// 焦点报告只在绑定校验后写入同一有界 writer，不占用命令行 pending 状态、不改变投影；
/// 若受管命令持有 gate，则允许丢弃这个可重发的瞬态报告，但绝不打断受管命令。
/// 隔离 IME 诊断只比较编码模式、长度与真实 writer 回执，不记录提交正文或资源身份。
///
/// @author kongweiguang
async fn write_ui_input_inner_with_receipt(
    session: Arc<ManagedSession>,
    request: UiInputWriteRequest,
) -> AppResult<UiTerminalCommandSubmissionReceipt> {
    let focus_report = matches!(
        &request.payload,
        UiInputPayload::Event(TerminalInputEvent::Focus(_))
    );
    let input_guard = match session.input_gate.try_lock() {
        Ok(guard) => Some(guard),
        Err(_) if focus_report => None,
        Err(_) => {
            interrupt_active_shell_command(&session, COMMAND_INTERRUPT_USER_INPUT).await;
            return Err(AppError::conflict("终端正在执行受管命令，UI 输入未发送"));
        }
    };

    let shell_dialect = *session.shell_dialect.read().await;
    let shell_integration_enabled = *session.shell_integration_enabled.read().await;
    let state = session.state.lock().await;
    let mut shell = session.shell_integration.lock().await;
    let current = terminal_command_context_from_state(
        &session,
        &state,
        &shell,
        shell_dialect,
        shell_integration_enabled,
    );
    if matches!(&request.payload, UiInputPayload::CommandLine(_))
        && !matches!(
            &request.binding.owner,
            kerminal_domain::sessions::SessionOwner::Ui { .. }
        )
    {
        return Err(AppError::conflict("普通命令行提交仅允许当前可见 UI owner"));
    }
    if !matches!(
        &request.binding.owner,
        kerminal_domain::sessions::SessionOwner::Ui { .. }
            | kerminal_domain::sessions::SessionOwner::Agent { .. }
    ) || request.binding.owner != current.owner
        || request.binding.session_generation != current.session_generation
    {
        return Err(AppError::conflict("可见终端输入目标或 generation 已失效"));
    }
    let expected_context_matches = request
        .expected_context
        .is_some_and(|expected| current.version() == expected);
    if request.require_valid_context
        && request.expected_context.is_some()
        && (!expected_context_matches
            || current.availability != TerminalCommandAvailability::Available
            || request.projection != UiInputProjection::Tracked)
    {
        return Err(AppError::conflict("终端命令建议上下文已变化"));
    }
    if state.info.lifecycle != SessionLifecycle::Running {
        return Err(AppError::conflict("终端会话当前不可写入"));
    }
    if input_guard.is_none() {
        return Ok(UiTerminalCommandSubmissionReceipt {
            context: current,
            command_associated: false,
        });
    }
    if shell.pending_input_write.is_some() {
        if focus_report {
            return Ok(UiTerminalCommandSubmissionReceipt {
                context: current,
                command_associated: false,
            });
        }
        return Err(AppError::conflict("上一条终端输入仍在有界 writer 队列中"));
    }
    let modes = state.engine.input_modes();
    let text_probe_units = if resize_diagnostics_enabled() {
        match &request.payload {
            UiInputPayload::Event(TerminalInputEvent::Text(text)) => {
                Some(text.encode_utf16().count() as u64)
            }
            _ => None,
        }
    } else {
        None
    };
    if let Some(utf16_units) = text_probe_units {
        tracing::info!(
            event = "TERMINAL_IME_DIAGNOSTIC",
            phase = 30_u64,
            count = utf16_units,
            ime_session_generation = request.binding.session_generation,
            kitty_keyboard_flags = modes.kitty_keyboard_flags
        );
    }
    let encoded = match &request.payload {
        UiInputPayload::Event(event) => encode_input(event, modes),
        UiInputPayload::CommandLine(command) => {
            if (command.contains('\n') || command.contains('\r'))
                && !modes.bracketed_paste
                && !request.allow_unbracketed_multiline
            {
                return Err(AppError::unsupported("目标终端未启用多行安全粘贴模式"));
            }
            let mut bytes = encode_input(&TerminalInputEvent::Paste(command.clone()), modes)
                .ok_or_else(|| AppError::unsupported("当前终端模式不支持粘贴文本"))?;
            let enter = encode_input(
                &TerminalInputEvent::Key(TerminalKeyEvent {
                    key: TerminalKey::Enter,
                    phase: KeyPhase::Press,
                    modifiers: InputModifiers::default(),
                }),
                modes,
            )
            .ok_or_else(|| AppError::unsupported("当前终端模式不支持 Enter 输入"))?;
            bytes.extend_from_slice(&enter);
            Some(bytes)
        }
    };
    if let Some(utf16_units) = text_probe_units {
        tracing::info!(
            event = "TERMINAL_IME_DIAGNOSTIC",
            phase = 31_u64,
            count = utf16_units,
            kitty_keyboard_flags = modes.kitty_keyboard_flags,
            ime_accepted = u64::from(encoded.is_some()),
            bytes = encoded.as_ref().map_or(0, |bytes| bytes.len() as u64)
        );
    }
    if focus_report {
        let Some(bytes) = encoded.filter(|bytes| !bytes.is_empty()) else {
            return Ok(UiTerminalCommandSubmissionReceipt {
                context: current,
                command_associated: false,
            });
        };
        let cancellation = CancellationToken::new();
        let bytes_written = AtomicUsize::new(0);
        drop(shell);
        drop(state);
        // 焦点报告为固定的短协议帧；共用输入 gate 与 writer 锁可保持命令顺序，且不注册为用户行编辑。
        write_input_bytes(&session, &bytes, &cancellation, &bytes_written).await?;
        return Ok(UiTerminalCommandSubmissionReceipt {
            context: terminal_command_context_for(&session).await,
            command_associated: false,
        });
    }
    let Some(bytes) = encoded else {
        shell.pending_command_submission = None;
        return Ok(UiTerminalCommandSubmissionReceipt {
            context: current,
            command_associated: false,
        });
    };
    if bytes.is_empty() {
        shell.pending_command_submission = None;
        return Ok(UiTerminalCommandSubmissionReceipt {
            context: current,
            command_associated: false,
        });
    }

    let capture_command = request.command_submission.is_some()
        && expected_context_matches
        && current.availability == TerminalCommandAvailability::Available
        && request.projection == UiInputProjection::Tracked
        && matches!(
            &request.binding.owner,
            kerminal_domain::sessions::SessionOwner::Ui { .. }
        )
        && matches!(
            &request.payload,
            UiInputPayload::Event(event) if is_plain_command_enter(event)
        )
        && bytes.len() == 1
        && (bytes[0] == b'\r' || bytes[0] == b'\n');
    let projection = ui_input_projection_for_write(&request);
    let cancellation = CancellationToken::new();
    let bytes_written = Arc::new(AtomicUsize::new(0));
    let session_generation = shell.session_generation;
    let prompt_epoch = shell.prompt_epoch;
    let generation = mark_shell_input(
        &mut shell,
        ShellInputOrigin::Ui {
            projection,
            submitted: match &request.payload {
                UiInputPayload::Event(event) => input_submits_prompt(event),
                UiInputPayload::CommandLine(_) => true,
            },
        },
    );
    shell.pending_command_submission = if capture_command {
        request
            .command_submission
            .map(|command| PendingCommandBlockSubmission {
                session_generation: shell.session_generation,
                prompt_epoch: shell.prompt_epoch,
                input_generation: generation,
                command,
                acknowledged: false,
                block_id: None,
            })
    } else {
        None
    };
    shell.pending_input_write = Some(PendingTerminalInputWrite {
        generation,
        cancellation: cancellation.clone(),
        bytes_written: Arc::clone(&bytes_written),
        expected_bytes: bytes.len(),
        session_generation,
        prompt_epoch,
        block_id: None,
        command_finished: false,
        prompt_started_after_command: false,
        prompt_ended_after_command: false,
    });
    drop(shell);
    drop(state);
    drop(input_guard);

    let result = write_input_bytes(&session, &bytes, &cancellation, &bytes_written).await;
    if let Some(utf16_units) = text_probe_units {
        tracing::info!(
            event = "TERMINAL_IME_DIAGNOSTIC",
            phase = 32_u64,
            count = utf16_units,
            ime_session_generation = session_generation,
            bytes = bytes.len() as u64,
            ime_accepted = u64::from(result.is_ok())
        );
    }
    finish_terminal_input_write(&session, generation, result.is_err()).await;
    result?;
    Ok(UiTerminalCommandSubmissionReceipt {
        context: terminal_command_context_for(&session).await,
        command_associated: capture_command,
    })
}

/// 可见 UI 输入在统一 writer gate 中表达普通事件或命令面板的一次性提交。
///
/// @author kongweiguang
struct UiInputWriteRequest {
    binding: UiSessionBinding,
    expected_context: Option<TerminalCommandContextVersion>,
    projection: UiInputProjection,
    payload: UiInputPayload,
    command_submission: Option<CommandBlockText>,
    require_valid_context: bool,
    /// 仅片段显式发送使用普通 Paste 的多行语义，不扩大通用命令提交能力。
    allow_unbracketed_multiline: bool,
}

/// 保持普通键盘事件与命令行粘贴 Enter 的编码边界明确，防止 UI 传入已编码字节绕过终端模式。
///
/// @author kongweiguang
enum UiInputPayload {
    Event(TerminalInputEvent),
    CommandLine(String),
}

/// 尊重 Root 在已知行 edit ticket 后选择的 projection；writer 不从可用 context 推断正文可信度。
/// Agent、命令面板、鼠标和焦点事件仍由 adapter 强制失效，IME 预编辑内容不进入此接口。
///
/// @author kongweiguang
fn ui_input_projection_for_write(request: &UiInputWriteRequest) -> UiInputProjection {
    if matches!(
        &request.binding.owner,
        kerminal_domain::sessions::SessionOwner::Agent { .. }
    ) || matches!(
        &request.payload,
        UiInputPayload::CommandLine(_)
            | UiInputPayload::Event(TerminalInputEvent::Mouse(_) | TerminalInputEvent::Focus(_))
    ) {
        UiInputProjection::Invalidates
    } else {
        request.projection
    }
}

/// 逐块写入并在 flush 前发布准确字节游标，使 reader 只关联完整 Enter 而不等待任意输入。
///
/// @author kongweiguang
async fn write_input_bytes(
    session: &ManagedSession,
    bytes: &[u8],
    cancellation: &CancellationToken,
    bytes_written: &AtomicUsize,
) -> AppResult<()> {
    let mut writer = tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            return Err(AppError::conflict("终端输入上下文在写入期间已变化"));
        }
        writer = session.writer.lock() => writer,
    };
    let mut offset = 0;
    while offset < bytes.len() {
        if cancellation.is_cancelled() {
            return Err(input_write_cancelled(offset));
        }
        let written = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(input_write_cancelled(offset));
            }
            result = writer.write(&bytes[offset..]) => match result {
                Ok(written) => written,
                Err(error) if offset > 0 => {
                    return Err(AppError::outcome_unknown_with_source(
                        "终端输入已部分发送，最终结果未知",
                        error,
                    ));
                }
                Err(error) => return Err(io_transport_error(error)),
            },
        };
        if written == 0 {
            return Err(if offset > 0 {
                AppError::outcome_unknown("终端输入已部分发送，最终结果未知")
            } else {
                AppError::infrastructure("终端输入 writer 未接受任何字节")
            });
        }
        offset += written;
        bytes_written.store(offset, Ordering::Release);
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => {
            Err(input_write_cancelled(offset))
        }
        result = writer.flush() => match result {
            Ok(()) => Ok(()),
            Err(error) if offset > 0 => Err(AppError::outcome_unknown_with_source(
                "终端输入已发送但 flush 失败，最终结果未知",
                error,
            )),
            Err(error) => Err(io_transport_error(error)),
        },
    }
}

/// 在没有任何字节进入 transport 时允许安全重试；部分写入后必须报告未知结果。
///
/// @author kongweiguang
fn input_write_cancelled(bytes_written: usize) -> AppError {
    if bytes_written == 0 {
        AppError::conflict("终端输入上下文在写入期间已变化，未发送输入")
    } else {
        AppError::outcome_unknown("终端输入已部分发送，最终结果未知")
    }
}

/// 清除已完成意图；持有 state→shell 锁时按原 generation 关联 ACK，失败分支才取走候选，迟到 ACK 不回写新 prompt。
///
/// @author kongweiguang
async fn finish_terminal_input_write(session: &ManagedSession, generation: u64, failed: bool) {
    let state = session.state.lock().await;
    let mut shell = session.shell_integration.lock().await;
    let mut command_block_changed = false;
    let failed_in_current_prompt = shell
        .pending_input_write
        .as_ref()
        .filter(|pending| pending.generation == generation)
        .is_some_and(|pending| {
            pending.session_generation == shell.session_generation
                && pending.prompt_epoch == shell.prompt_epoch
        });
    if shell
        .pending_input_write
        .as_ref()
        .is_some_and(|pending| pending.generation == generation)
    {
        shell.pending_input_write = None;
        if failed && failed_in_current_prompt {
            shell.external_input_since_prompt = true;
            shell.ui_input_projection_valid = false;
        }
    }
    if shell
        .pending_command_submission
        .as_ref()
        .is_some_and(|pending| pending.input_generation == generation)
    {
        if failed {
            if let Some(pending) = shell
                .pending_command_submission
                .take()
                .filter(|pending| pending.input_generation == generation)
                && let Some(id) = pending.block_id
                && let Some(block) = shell.command_blocks.iter_mut().find(|block| block.id == id)
            {
                block.command =
                    CommandBlockText::Unknown(CommandBlockTextUnknownReason::WriteNotAcknowledged);
                command_block_changed = true;
            }
        } else {
            let attached = shell
                .pending_command_submission
                .as_ref()
                .filter(|pending| pending.input_generation == generation)
                .and_then(|pending| pending.block_id.map(|id| (id, pending.command.clone())));
            if let Some(pending) = shell.pending_command_submission.as_mut()
                && pending.input_generation == generation
            {
                pending.acknowledged = true;
            }
            if let Some((id, command)) = attached {
                if let Some(block) = shell.command_blocks.iter_mut().find(|block| block.id == id) {
                    block.command = command;
                    command_block_changed = true;
                }
                shell.pending_command_submission = None;
            }
        }
    }
    if command_block_changed {
        shell.bump_command_block_revision();
        let revision = state.engine.revision();
        drop(shell);
        drop(state);
        session.changes.send_replace(SessionChange {
            revision,
            kind: SessionChangeKind::Output,
        });
    }
}

/// 只识别终端输入事件本身携带的提交字符，不从回显或屏幕内容推断用户是否执行命令。
///
/// @author kongweiguang
fn input_submits_prompt(event: &TerminalInputEvent) -> bool {
    let contains_submit = |text: &str| text.contains('\r') || text.contains('\n');
    match event {
        TerminalInputEvent::Text(text) | TerminalInputEvent::Paste(text) => contains_submit(text),
        TerminalInputEvent::Key(event) | TerminalInputEvent::KeyWithText { event, .. } => {
            matches!(
                event.key,
                TerminalKey::Enter | TerminalKey::Character('\r') | TerminalKey::Character('\n')
            )
        }
        TerminalInputEvent::Mouse(_) | TerminalInputEvent::Focus(_) => false,
    }
}

/// 在关闭、重连或用户抢占输入时取消活跃命令租期，并记录导致结果未知的原因。
///
/// @author kongweiguang
async fn interrupt_active_shell_command(session: &ManagedSession, reason: u8) {
    let shell = session.shell_integration.lock().await;
    if let Some(active) = &shell.active_command {
        active.interruption.store(reason, Ordering::Release);
        active.cancellation.cancel();
    }
}

/// 只移除匹配的请求 ID，避免较晚结束的旧 caller 清掉后续租期。
///
/// @author kongweiguang
async fn clear_active_shell_command(session: &ManagedSession, command_id: &str) {
    let mut shell = session.shell_integration.lock().await;
    if shell
        .active_command
        .as_ref()
        .is_some_and(|active| active.command_id == command_id)
    {
        shell.active_command = None;
    }
}

/// 保留既有 8000 字符多行工作流契约，同时拒绝无法安全送入 PTY 的控制字符。
///
/// @author kongweiguang
fn validate_shell_command_request(request: &ShellCommandRequest) -> AppResult<()> {
    if request.command_id.trim().is_empty()
        || request.command_id.len() > 128
        || request.command_id.chars().any(char::is_control)
    {
        return Err(AppError::invalid("受管命令标识无效"));
    }
    let command = request.command.trim();
    if command.is_empty()
        || command.chars().count() > 8_000
        || command.len() > 32_000
        || request
            .command
            .bytes()
            .any(|byte| matches!(byte, b'\r' | 0x00 | 0x1b))
    {
        return Err(AppError::invalid("受管命令长度或控制字符无效"));
    }
    if request.timeout.is_zero() || request.timeout > Duration::from_secs(24 * 60 * 60) {
        return Err(AppError::invalid("受管命令等待时限无效"));
    }
    Ok(())
}

/// 用已确认的 shell dialect 将任意 UTF-8 脚本编码为一行，保留当前 shell 的 cwd 与变量作用域。
///
/// @author kongweiguang
fn encode_managed_shell_command(command: &str, dialect: RemoteShellDialect) -> AppResult<String> {
    match dialect {
        RemoteShellDialect::Posix => {
            // Octal encodes every input byte, so quotes, substitutions and LF stay data until eval.
            let mut encoded =
                String::with_capacity(command.len().saturating_mul(4).saturating_add(40));
            encoded.push_str("eval \"$(printf '%b' '");
            for byte in command.as_bytes() {
                use std::fmt::Write as _;
                let _ = write!(encoded, "\\{byte:03o}");
            }
            encoded.push_str("')\"");
            Ok(encoded)
        }
        RemoteShellDialect::PowerShell => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(command.as_bytes());
            Ok(format!(
                "Invoke-Expression ([Text.Encoding]::UTF8.GetString([Convert]::FromBase64String('{encoded}')))"
            ))
        }
        RemoteShellDialect::Fish => Ok(encode_fish_managed_command(command)),
        RemoteShellDialect::Unknown => Err(AppError::capability_not_supported(
            "受管命令需要已验证的 POSIX、Fish 或 PowerShell shell dialect",
        )),
    }
}

/// 将任意 UTF-8 脚本编码为单行 Fish 表达式，并在当前 shell scope 中只 eval 一次。
///
/// 每个 Unicode scalar 都使用 Fish printf 支持的 \UXXXXXXXX 表示；string collect 保留脚本内 LF 为单个 eval 参数。
/// @author kongweiguang
fn encode_fish_managed_command(command: &str) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(command.chars().count().saturating_mul(10));
    for character in command.chars() {
        write!(encoded, "\\U{:08X}", u32::from(character))
            .expect("writing a Fish unicode escape into String cannot fail");
    }
    format!("eval (builtin printf '%b' '{encoded}' | string collect --no-trim-newlines)")
}

/// 所有不能由本租期真实 C/D 证明的结果均保留 request ID 并标为 Unknown。
///
/// @author kongweiguang
fn shell_command_unknown(
    request: &ShellCommandRequest,
    reason: ShellCommandUnknownReason,
) -> ShellCommandOutcome {
    ShellCommandOutcome::Unknown {
        command_id: request.command_id.clone(),
        reason,
    }
}

/// 在 close ACK 后统一解释退出等待结果，避免重连与关闭对同一资源采用不同的回收证明。
///
/// typed remote disconnect 只使退出码未知；其它错误或 deadline 到期都不能证明 worker 已回收。
///
/// @author kongweiguang
async fn wait_for_closed_transport_exit(
    control: &dyn SessionTransportControl,
    session_id: &SessionId,
    deadline: Instant,
    was_disconnected: bool,
) -> AppResult<SessionExitStatus> {
    match tokio::time::timeout_at(deadline, control.wait_exit()).await {
        Ok(Ok(status)) => Ok(status),
        Ok(Err(error)) if was_disconnected && error.is_retryable_remote_disconnect() => {
            tracing::debug!(
                error = %error,
                session_id = %session_id,
                "closed terminal transport has no confirmed remote exit status"
            );
            Ok(SessionExitStatus {
                code: None,
                signal: None,
                success: false,
            })
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(AppError::infrastructure(
            "等待终端退出状态或 transport worker 收尾超时",
        )),
    }
}

/// 关闭单条 transport 并完整 join reader；close_until 已确认清理后允许取消态只表示退出码未知。
/// 其它错误或 deadline 仍失败并保留 registry，不能把未确认资源回收当作成功。
///
/// @author kongweiguang
async fn close_managed_session(
    session: Arc<ManagedSession>,
    deadline: Instant,
    disconnected_before_batch: Option<bool>,
    notified_close: Option<AppResult<()>>,
) -> AppResult<SessionExitStatus> {
    SessionManager::cancel_auto_reconnect_until(&session, deadline).await?;
    tokio::time::timeout_at(
        deadline,
        interrupt_active_shell_command(&session, COMMAND_INTERRUPT_DISCONNECT),
    )
    .await
    .map_err(|_| AppError::infrastructure("等待受管命令中断超时"))?;
    let _operation = tokio::time::timeout_at(deadline, session.operation_gate.lock())
        .await
        .map_err(|_| AppError::infrastructure("等待终端会话操作锁超时"))?;
    let _input = tokio::time::timeout_at(deadline, session.input_gate.lock())
        .await
        .map_err(|_| AppError::infrastructure("等待终端输入锁超时"))?;
    // Read shell generation before taking the state lock so this short proof capture never reverses lock order.
    let shell_session_generation =
        tokio::time::timeout_at(deadline, session.shell_integration.lock())
            .await
            .map_err(|_| AppError::infrastructure("等待终端输入状态锁超时"))?
            .session_generation;
    let transport_generation = session.transport_generation.load(Ordering::Acquire);
    let (was_disconnected, existing_status, close_history_proof) = {
        let mut state = tokio::time::timeout_at(deadline, session.state.lock())
            .await
            .map_err(|_| AppError::infrastructure("等待终端会话状态锁超时"))?;
        if state.info.lifecycle == SessionLifecycle::Closed {
            return Ok(state.info.exit_status.clone().unwrap_or(SessionExitStatus {
                code: None,
                signal: None,
                success: false,
            }));
        }
        state
            .engine
            .invalidate_active_transcript(TranscriptIncompleteReason::Closed);
        let was_disconnected = disconnected_before_batch.unwrap_or_else(|| {
            matches!(
                state.info.lifecycle,
                SessionLifecycle::Exited | SessionLifecycle::Failed
            ) || state.info.exit_status.is_some()
        });
        let existing_status = state.info.exit_status.clone();
        let close_history_proof = prepare_close_history_proof(
            &mut state,
            Some(shell_session_generation),
            transport_generation,
        );
        transition_session_lifecycle(&mut state, SessionLifecycleEvent::CloseRequested)
            .map_err(|_| AppError::conflict("当前终端生命周期不能关闭"))?;
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision().wrapping_add(1),
            kind: SessionChangeKind::Lifecycle,
        });
        (was_disconnected, existing_status, close_history_proof)
    };
    let close_attempt: AppResult<SessionExitStatus> = async {
        let mut shell = tokio::time::timeout_at(deadline, session.shell_integration.lock())
            .await
            .map_err(|_| AppError::infrastructure("等待终端输入状态锁超时"))?;
        let command_block_changed = shell.end_command_tracking(CommandBlockUnknownReason::Closed);
        if let Some(pending) = shell.pending_input_write.take() {
            pending.cancellation.cancel();
        }
        drop(shell);
        if command_block_changed {
            let revision = tokio::time::timeout_at(deadline, session.state.lock())
                .await
                .map_err(|_| AppError::infrastructure("等待终端会话状态锁超时"))?
                .engine
                .revision();
            session.changes.send_replace(SessionChange {
                revision,
                kind: SessionChangeKind::Lifecycle,
            });
        }
        shutdown_session_writer(&session, deadline).await?;
        let control = tokio::time::timeout_at(deadline, session.control.read())
            .await
            .map_err(|_| AppError::infrastructure("等待终端 transport 控制锁超时"))?
            .clone();
        let close_result = match notified_close {
            Some(result) => result,
            None => control.close_until(deadline).await,
        };
        // @author kongweiguang
        // 远端断线只会让退出码不可知；close_until 仍承担 channel 与 reader 清理确认，任何失败都必须保留会话 owner 供重试。
        close_result?;
        let observed_status = match wait_for_closed_transport_exit(
            control.as_ref(),
            &session.session_id,
            deadline,
            was_disconnected,
        )
        .await
        {
            Ok(status) => status,
            Err(error)
                if error.public_code() == kerminal_application::PublicErrorCode::Cancelled =>
            {
                // close_until 已确认 transport 回收；SSH PTY reader 对本地主动 Close 会报告 Cancelled，
                // 此时取消只说明没有可靠退出码，不能把已完成清理的 close 再记成失败。
                tracing::debug!(
                    session_id = %session.session_id,
                    "transport close was confirmed; exit status ended through intentional cancellation"
                );
                SessionExitStatus {
                    code: None,
                    signal: None,
                    success: false,
                }
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = join_reader(&session, deadline).await {
            if let Ok(cancellation) =
                tokio::time::timeout_at(deadline, session.cancellation.lock()).await
            {
                cancellation.cancel();
            }
            return Err(error);
        }
        join_child_exit_observer(&session, deadline).await?;
        let status = existing_status.unwrap_or(observed_status);
        if let Ok(cancellation) =
            tokio::time::timeout_at(deadline, session.cancellation.lock()).await
        {
            cancellation.cancel();
        }
        let mut state = tokio::time::timeout_at(deadline, session.state.lock())
            .await
            .map_err(|_| AppError::infrastructure("等待终端会话最终状态锁超时"))?;
        transition_session_lifecycle(&mut state, SessionLifecycleEvent::CloseSucceeded)
            .map_err(|_| AppError::conflict("终端会话关闭状态已过期"))?;
        state.info.exit_status = Some(status.clone());
        let revision = state.engine.revision().wrapping_add(1);
        session.changes.send_replace(SessionChange {
            revision,
            kind: SessionChangeKind::Closed,
        });
        Ok(status)
    }
    .await;
    if let Err(error) = close_attempt {
        // Only restore an ended history lifecycle if this same owner and transport generation still prove it.
        if let Some(proof) = close_history_proof {
            let mut state = session.state.lock().await;
            if state.info.lifecycle == SessionLifecycle::Closing
                && state.info.owner == proof.owner
                && state.close_history_proof.as_ref() == Some(&proof)
                && session.transport_generation.load(Ordering::Acquire)
                    == proof.transport_generation
                && transition_session_lifecycle(
                    &mut state,
                    SessionLifecycleEvent::CloseFailedToHistory {
                        previous: proof.origin,
                    },
                )
                .is_ok()
            {
                session.changes.send_replace(SessionChange {
                    revision: state.engine.revision().wrapping_add(1),
                    kind: SessionChangeKind::Lifecycle,
                });
            }
        }
        return Err(error);
    }
    close_attempt
}

/// 复制 live handle 后按统一预算收尾；移除时核对 Arc owner，且 registry 锁不跨越 await。
///
/// @author kongweiguang
async fn close_all_until(manager: &SessionManager, deadline: Instant) -> AppResult<()> {
    manager.shutting_down.store(true, Ordering::Release);
    manager.shutdown_cancel.cancel();
    let _shutdown = tokio::time::timeout_at(deadline, manager.shutdown_gate.lock())
        .await
        .map_err(|_| AppError::infrastructure("等待终端全局关闭锁超时"))?;
    if manager.shutdown_complete.load(Ordering::Acquire) {
        return Ok(());
    }

    let publication = tokio::time::timeout_at(deadline, manager.lifecycle_gate.write())
        .await
        .map_err(|_| AppError::infrastructure("等待终端发布门禁关闭超时"))?;
    manager.create_tasks.close();
    drop(publication);

    let mut failures = Vec::new();
    if !manager.wait_for_pending_creates(deadline).await {
        let pending = manager.pending_creates.load(Ordering::Acquire);
        tracing::error!(
            pending,
            "pending terminal creates did not finish before shutdown deadline"
        );
        failures.push(AppError::infrastructure("等待未完成的终端创建清理超时"));
    }
    if tokio::time::timeout_at(deadline, manager.create_tasks.wait())
        .await
        .is_err()
    {
        tracing::error!("manager-owned terminal create tasks remain pending at shutdown deadline");
        failures.push(AppError::infrastructure("等待终端创建任务清理超时"));
    }

    let open_sessions = manager
        .sessions
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .live
        .values()
        .cloned()
        .collect::<Vec<_>>();
    let mut sessions: Vec<(Arc<ManagedSession>, bool)> = Vec::with_capacity(open_sessions.len());
    for session in open_sessions {
        if tokio::time::timeout_at(
            deadline,
            interrupt_active_shell_command(&session, COMMAND_INTERRUPT_DISCONNECT),
        )
        .await
        .is_err()
        {
            failures.push(AppError::infrastructure("等待受管命令中断超时"));
        }
        let shell_session_generation =
            tokio::time::timeout_at(deadline, session.shell_integration.lock())
                .await
                .ok()
                .map(|shell| shell.session_generation);
        let transport_generation = session.transport_generation.load(Ordering::Acquire);
        let was_disconnected = match tokio::time::timeout_at(deadline, session.state.lock()).await {
            Ok(mut state) => {
                let disconnected = matches!(
                    state.info.lifecycle,
                    SessionLifecycle::Exited | SessionLifecycle::Failed
                ) || state.info.exit_status.is_some();
                let _ = prepare_close_history_proof(
                    &mut state,
                    shell_session_generation,
                    transport_generation,
                );
                if state.info.lifecycle != SessionLifecycle::Closed {
                    if transition_session_lifecycle(
                        &mut state,
                        SessionLifecycleEvent::CloseRequested,
                    )
                    .is_err()
                    {
                        failures.push(AppError::conflict("批量关闭遇到已过期的终端生命周期事件"));
                    }
                    session.changes.send_replace(SessionChange {
                        revision: state.engine.revision().wrapping_add(1),
                        kind: SessionChangeKind::Lifecycle,
                    });
                }
                disconnected
            }
            Err(_) => {
                failures.push(AppError::infrastructure("等待终端会话状态锁超时"));
                false
            }
        };
        sessions.push((session, was_disconnected));
    }

    // 先并发停止所有写端并发出 close，之后再逐会话等待退出，防止前一条拖住其余会话。
    let mut notifications = tokio::task::JoinSet::new();
    for (session, _) in &sessions {
        let session = Arc::clone(session);
        notifications.spawn(async move {
            let session_id = session.session_id.clone();
            let mut task_failures = Vec::new();
            if let Err(error) =
                SessionManager::cancel_auto_reconnect_until(&session, deadline).await
            {
                task_failures.push(error);
            }
            if let Err(error) = shutdown_session_writer(&session, deadline).await {
                task_failures.push(error);
            }
            let control = match tokio::time::timeout_at(deadline, session.control.read()).await {
                Ok(control) => Arc::clone(&control),
                Err(_) => {
                    task_failures.push(AppError::infrastructure("等待终端控制锁超时"));
                    return (session_id, task_failures, None);
                }
            };
            let close_result = control.close_until(deadline).await;
            (session_id, task_failures, Some(close_result))
        });
    }

    let mut close_results = HashMap::with_capacity(sessions.len());
    while let Some(result) = notifications.join_next().await {
        match result {
            Ok((session_id, task_failures, close_result)) => {
                failures.extend(task_failures);
                if let Some(close_result) = close_result {
                    close_results.insert(session_id, close_result);
                }
            }
            Err(error) => {
                tracing::error!(error = %error, "terminal close notification task failed");
                failures.push(AppError::infrastructure_with_source(
                    "终端关闭通知任务失败",
                    error,
                ));
            }
        }
    }

    let mut cleanups = tokio::task::JoinSet::new();
    let output_recorder = manager.output_recorder.clone();
    for (session, was_disconnected) in sessions {
        let session_id = session.session_id.clone();
        let close_result = close_results.remove(&session_id);
        let output_recorder = output_recorder.clone();
        cleanups.spawn(async move {
            let result = match close_managed_session(
                Arc::clone(&session),
                deadline,
                Some(was_disconnected),
                close_result,
            )
            .await
            {
                Ok(status) => match output_recorder {
                    Some(recorder) => {
                        match finish_output_recording(recorder.as_ref(), &session_id, deadline)
                            .await
                        {
                            Ok(()) => Ok(status),
                            Err(error) => {
                                mark_closed_history_recording_failure(&session).await;
                                Err(error)
                            }
                        }
                    }
                    None => Ok(status),
                },
                Err(error) => Err(error),
            };
            (session_id, session, result)
        });
    }
    while let Some(result) = cleanups.join_next().await {
        match result {
            Ok((session_id, session, Ok(_))) => {
                let mut registry = manager
                    .sessions
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if registry
                    .live
                    .get(&session_id)
                    .is_some_and(|current| Arc::ptr_eq(current, &session))
                {
                    registry.live.remove(&session_id);
                }
            }
            Ok((session_id, _session, Err(error))) => {
                tracing::error!(session_id = %session_id, error = %error, "terminal session cleanup failed");
                failures.push(error);
            }
            Err(error) => {
                tracing::error!(error = %error, "terminal cleanup task failed");
                failures.push(AppError::infrastructure_with_source(
                    "终端清理任务失败",
                    error,
                ));
            }
        }
    }

    if let Err(error) = manager.retry_all_pending_cleanup(deadline).await {
        failures.push(error);
    }
    let remaining_cleanups = manager.pending_cleanup.lock().await.len();
    if remaining_cleanups > 0 {
        tracing::error!(
            remaining_cleanups,
            "terminal cleanup owners remain after shutdown attempt"
        );
        failures.push(AppError::infrastructure("终端仍有资源处于待回收状态"));
    }

    if let Some(recorder) = &manager.output_recorder {
        match tokio::time::timeout_at(deadline, recorder.shutdown_until(deadline.into_std())).await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::error!(error = %error, "terminal output recorder shutdown failed");
                failures.push(error);
            }
            Err(_) => {
                tracing::error!("terminal output recorder shutdown timed out");
                failures.push(AppError::infrastructure("等待终端日志全局 flush 超时"));
            }
        }
    }

    if failures.is_empty() {
        manager.shutdown_complete.store(true, Ordering::Release);
        Ok(())
    } else {
        Err(AppError::infrastructure_with_source(
            "部分终端会话未能完整关闭",
            SessionCleanupSummary {
                failed_count: failures.len(),
            },
        ))
    }
}

/// 关闭 managed writer 的写半部以便 bounded PTY bridge 能收到 EOF 并完成阻塞任务。
///
/// @author kongweiguang
async fn shutdown_session_writer(session: &ManagedSession, deadline: Instant) -> AppResult<()> {
    if session.writer_shutdown.swap(true, Ordering::AcqRel) {
        return Ok(());
    }
    let mut writer = tokio::time::timeout_at(deadline, session.writer.lock())
        .await
        .map_err(|_| {
            session.writer_shutdown.store(false, Ordering::Release);
            AppError::infrastructure("等待终端 writer 锁超时")
        })?;
    let result = match tokio::time::timeout_at(deadline, writer.shutdown()).await {
        Ok(result) => result.map_err(io_transport_error),
        Err(_) => Err(AppError::infrastructure("关闭终端 writer 超时")),
    };
    if result.is_err() {
        session.writer_shutdown.store(false, Ordering::Release);
    }
    result
}

/// 在关闭 session 时排空原始输出队列并 join 日志 writer，遵守统一 deadline。
///
/// @author kongweiguang
async fn finish_output_recording(
    recorder: &dyn SessionOutputRecorder,
    session_id: &SessionId,
    deadline: Instant,
) -> AppResult<()> {
    let result = tokio::time::timeout_at(
        deadline,
        recorder.finish_session(session_id, deadline.into_std()),
    )
    .await
    .map_err(|_| AppError::infrastructure("等待终端日志 flush 超时"))??;
    if result.active {
        return Err(AppError::conflict("终端日志 writer 关闭后仍处于活动状态"));
    }
    if result.truncated {
        tracing::warn!(
            session_id = %session_id,
            first_missing_sequence = ?result.first_missing_sequence,
            failure_reason = ?result.failure_reason,
            "terminal output recording closed with a known gap"
        );
    }
    Ok(())
}

/// Enables only the exact ended-history view after recorder cleanup is known to have failed; transport state stays Closed.
///
/// @author kongweiguang
async fn mark_closed_history_recording_failure(session: &ManagedSession) {
    let mut state = session.state.lock().await;
    let current_transport_generation = session.transport_generation.load(Ordering::Acquire);
    let proof_matches = state.close_history_proof.as_ref().is_some_and(|proof| {
        state.info.lifecycle == SessionLifecycle::Closed
            && matches!(
                proof.origin,
                SessionLifecycle::Exited | SessionLifecycle::Failed
            )
            && proof.owner == state.info.owner
            && proof.transport_generation == current_transport_generation
    });
    if proof_matches && let Some(proof) = state.close_history_proof.as_mut() {
        proof.recorder_cleanup_failed = true;
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision(),
            kind: SessionChangeKind::Lifecycle,
        });
    }
}

/// 有界回收尚未进入 registry 的 transport，避免取消或容量错误把调用方卡在关闭请求中。
///
/// @author kongweiguang
async fn close_unpublished_transport(
    control: &Arc<dyn SessionTransportControl>,
    deadline: Instant,
) -> AppResult<()> {
    let close = control.close_until(deadline).await;
    let exit = tokio::time::timeout_at(deadline, control.wait_exit()).await;
    if let Err(error) = &close {
        tracing::error!(error = %error, "unpublished terminal transport close failed");
    }
    if let Err(error) = &exit {
        tracing::error!(error = %error, "unpublished terminal transport exit wait failed");
    }
    match (close, exit) {
        (Ok(()), Ok(Ok(_))) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Ok(Err(error))) => Err(error),
        (Ok(()), Err(_)) => Err(AppError::infrastructure("等待未发布终端退出超时")),
    }
}

/// 重连任一步失败时通知稳定 session shell，保留 registry 供显式重试或关闭。
///
/// @author kongweiguang
async fn mark_reconnect_failed(session: &ManagedSession) {
    let mut state = session.state.lock().await;
    if transition_session_lifecycle(&mut state, SessionLifecycleEvent::ReconnectFailed).is_err() {
        tracing::debug!(
            session_id = %session.session_id,
            lifecycle = ?state.info.lifecycle,
            "ignored stale terminal reconnect failure"
        );
        return;
    }
    state
        .engine
        .invalidate_active_transcript(TranscriptIncompleteReason::Disconnected);
    session
        .shell_integration
        .lock()
        .await
        .end_command_tracking(CommandBlockUnknownReason::Disconnected);
    session.changes.send_replace(SessionChange {
        revision: state.engine.revision().wrapping_add(1),
        kind: SessionChangeKind::Lifecycle,
    });
}

/// 等待同一会话的 reader 清理；超时保留 JoinHandle，以便 close/reconnect 可再次收尾。
///
/// @author kongweiguang
async fn join_reader(session: &ManagedSession, deadline: Instant) -> AppResult<()> {
    let mut reader_task = tokio::time::timeout_at(deadline, session.reader_task.lock())
        .await
        .map_err(|_| AppError::infrastructure("等待终端 reader 锁超时"))?;
    let Some(task) = reader_task.as_mut() else {
        return Ok(());
    };
    match tokio::time::timeout_at(deadline, task).await {
        Ok(result) => {
            reader_task.take();
            result.map_err(|error| {
                AppError::infrastructure_with_source("终端 reader task 失败", error)
            })
        }
        Err(_) => Err(AppError::infrastructure("等待终端 reader 清理超时")),
    }
}

/// 批量关闭错误摘要保留失败数量，单条安全错误详情已逐个写入诊断日志。
///
/// @author kongweiguang
#[derive(Debug)]
struct SessionCleanupSummary {
    failed_count: usize,
}

impl std::fmt::Display for SessionCleanupSummary {
    /// 仅在内部 source 链描述失败数量，公开 AppError 文案仍使用固定脱敏内容。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} terminal session cleanup operation(s) failed",
            self.failed_count
        )
    }
}

impl std::error::Error for SessionCleanupSummary {}

/// 将 VT 响应按序写回对应 transport；opt-in 只摘录首个 CPR 坐标，且不能把它当作 native resize 确认。
///
/// @author kongweiguang
async fn write_parser_responses(session: &ManagedSession, writes: Vec<Vec<u8>>) -> AppResult<()> {
    if writes.is_empty() {
        return Ok(());
    }
    let resize_generation = resize_diagnostics_enabled()
        .then(|| session.resize_diagnostic_generation.load(Ordering::Relaxed))
        .filter(|generation| *generation > 0);
    if let Some(resize_generation) = resize_generation {
        let transport_generation = session.transport_generation.load(Ordering::Acquire);
        for bytes in &writes {
            if let Some((row, column)) = parse_cursor_position_report(bytes)
                && claim_resize_cpr_diagnostic_generation(
                    &session.resize_diagnostic_cpr_generation,
                    resize_generation,
                )
            {
                log_resize_cpr_diagnostic(
                    session.resize_diagnostic_session_sequence,
                    transport_generation,
                    resize_generation,
                    row,
                    column,
                );
            }
        }
    }
    let mut writer = session.writer.lock().await;
    for bytes in writes {
        writer.write_all(&bytes).await.map_err(io_transport_error)?;
    }
    writer.flush().await.map_err(io_transport_error)
}

/// 发布与当前 transport generation 匹配的原生退出状态，并只为首次观察建立共享收尾 deadline。
///
/// @author kongweiguang
async fn publish_native_child_exit(
    session: &ManagedSession,
    transport_generation: u64,
    status: SessionExitStatus,
    candidate_deadline: Instant,
) -> Option<Instant> {
    if session.transport_generation.load(Ordering::Acquire) != transport_generation {
        return None;
    }
    let mut state = session.state.lock().await;
    if session.transport_generation.load(Ordering::Acquire) != transport_generation {
        return None;
    }

    let mut changed = false;
    match state.info.lifecycle {
        SessionLifecycle::Running => {
            if transition_session_lifecycle(&mut state, SessionLifecycleEvent::TransportExited)
                .is_err()
            {
                return None;
            }
            changed = true;
        }
        SessionLifecycle::Exited | SessionLifecycle::Failed => {}
        SessionLifecycle::Starting | SessionLifecycle::Closing | SessionLifecycle::Closed => {
            return None;
        }
    }
    if state.info.exit_status.is_none() {
        state.info.exit_status = Some(status);
        changed = true;
    }
    let deadline = {
        let mut stored = session.native_exit_deadline.lock().await;
        *stored.get_or_insert(candidate_deadline)
    };
    if changed {
        session.changes.send_replace(SessionChange {
            revision: state.engine.revision().wrapping_add(1),
            kind: SessionChangeKind::Lifecycle,
        });
    }
    Some(deadline)
}

/// 在同一绝对 deadline 内 join native-exit observer；超时保留 JoinHandle 供关闭或重连重试。
///
/// @author kongweiguang
async fn join_child_exit_observer(session: &ManagedSession, deadline: Instant) -> AppResult<()> {
    let mut observer = tokio::time::timeout_at(deadline, session.child_exit_task.lock())
        .await
        .map_err(|_| AppError::timeout("等待 native child observer 锁"))?;
    let Some(task) = observer.as_mut() else {
        return Ok(());
    };
    match tokio::time::timeout_at(deadline, task).await {
        Ok(Ok(())) => {
            observer.take();
            Ok(())
        }
        Ok(Err(error)) => {
            observer.take();
            Err(AppError::infrastructure_with_source(
                "native child observer task 失败",
                error,
            ))
        }
        Err(_) => Err(AppError::timeout("等待 native child observer 收尾")),
    }
}

/// reader EOF 后使用 native exit 首次建立的同一 deadline 完成输入收口、transport drain 与状态确认。
///
/// @author kongweiguang
async fn mark_reader_exit(
    session: &ManagedSession,
    control: &dyn SessionTransportControl,
    transport_generation: u64,
    close_wait: Duration,
) -> Option<AppError> {
    let fallback_deadline = Instant::now() + close_wait;
    let mut failure = None;
    match tokio::time::timeout_at(fallback_deadline, control.observe_child_exit()).await {
        Ok(Ok(Some(status))) => {
            if let Some(deadline) =
                publish_native_child_exit(session, transport_generation, status, fallback_deadline)
                    .await
            {
                // EOF 已证明管道读端结束；Unsupported 可安全降级到 drain 后显式 Close。
                if let Err(error) = control.release_after_child_exit_until(deadline).await
                    && error.public_code() != kerminal_application::PublicErrorCode::Unsupported
                {
                    tracing::debug!(
                        event = "LOCAL_PTY_NATURAL_RELEASE_RETRY_FAILED",
                        code = error.public_code().as_str(),
                        "reader reached EOF; final worker join still owns PTY cleanup"
                    );
                }
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => {
            failure = Some(error);
        }
        Err(_) => {
            failure = Some(AppError::timeout("等待 native child exit 观察"));
        }
    }
    let deadline = session
        .native_exit_deadline
        .lock()
        .await
        .unwrap_or(fallback_deadline);

    // EOF 表示 writer bridge 不再能被 PTY 读取；只在此处关闭 app 写半部，保留 native writer 到此刻。
    if let Err(error) = shutdown_session_writer(session, deadline).await {
        failure.get_or_insert(error);
    }
    let observed_status = match tokio::time::timeout_at(deadline, control.wait_exit()).await {
        Ok(Ok(status)) => Some(status),
        Ok(Err(error)) => {
            failure.get_or_insert(error);
            None
        }
        Err(_) => {
            failure.get_or_insert_with(|| AppError::timeout("等待终端 drain 和 worker 回收"));
            None
        }
    };
    if let Err(error) = join_child_exit_observer(session, deadline).await {
        failure.get_or_insert(error);
    }

    let mut state = session.state.lock().await;
    match state.info.lifecycle {
        SessionLifecycle::Running => {
            let event = if observed_status.is_some() {
                SessionLifecycleEvent::TransportExited
            } else {
                SessionLifecycleEvent::TransportFailed
            };
            if let Err(stale) = transition_session_lifecycle(&mut state, event) {
                tracing::debug!(?stale, "ignored stale terminal reader EOF transition");
                return failure;
            }
            if let Some(status) = observed_status {
                state.info.exit_status = Some(status);
            }
        }
        SessionLifecycle::Exited | SessionLifecycle::Failed => {
            if state.info.exit_status.is_none() {
                state.info.exit_status = observed_status;
            }
        }
        SessionLifecycle::Starting | SessionLifecycle::Closing | SessionLifecycle::Closed => {
            tracing::debug!(
                lifecycle = ?state.info.lifecycle,
                "ignored stale terminal reader EOF transition"
            );
            return failure;
        }
    }
    state
        .engine
        .invalidate_active_transcript(TranscriptIncompleteReason::Disconnected);
    session
        .shell_integration
        .lock()
        .await
        .end_command_tracking(CommandBlockUnknownReason::Disconnected);
    let revision = state.engine.revision().wrapping_add(1);
    session.changes.send_replace(SessionChange {
        revision,
        kind: SessionChangeKind::Lifecycle,
    });
    failure
}

/// 把当前 transport 的 reader I/O 失败发布为 failed；退役门禁和 generation 一起隔离旧 owner 回执。
///
/// @author kongweiguang
async fn mark_reader_failure(
    session: &ManagedSession,
    transport_generation: u64,
    error: std::io::Error,
) {
    let retirement = session.retiring_transport_generation.lock().await;
    if *retirement == Some(transport_generation)
        || session.transport_generation.load(Ordering::Acquire) != transport_generation
    {
        tracing::debug!(
            session_id = %session.session_id,
            transport_generation,
            "ignored terminal reader failure from stale or retiring transport"
        );
        return;
    }
    let mut state = session.state.lock().await;
    if transition_session_lifecycle(&mut state, SessionLifecycleEvent::TransportFailed).is_err() {
        tracing::debug!(
            session_id = %session.session_id,
            transport_generation,
            lifecycle = ?state.info.lifecycle,
            "ignored stale terminal reader failure"
        );
        return;
    }
    state
        .engine
        .invalidate_active_transcript(TranscriptIncompleteReason::Disconnected);
    session
        .shell_integration
        .lock()
        .await
        .end_command_tracking(CommandBlockUnknownReason::ReaderFailed);
    let revision = state.engine.revision().wrapping_add(1);
    session.changes.send_replace(SessionChange {
        revision,
        kind: SessionChangeKind::Lifecycle,
    });
    tracing::debug!(error = %error, "terminal reader failed");
}

/// parser response 写失败只影响当前 transport；旧 generation 在退役门禁中不得污染 Starting owner。
///
/// @author kongweiguang
async fn mark_writer_failure(session: &ManagedSession, transport_generation: u64) {
    let retirement = session.retiring_transport_generation.lock().await;
    if *retirement == Some(transport_generation)
        || session.transport_generation.load(Ordering::Acquire) != transport_generation
    {
        tracing::debug!(
            session_id = %session.session_id,
            transport_generation,
            "ignored terminal writer failure from stale or retiring transport"
        );
        return;
    }
    let mut state = session.state.lock().await;
    if transition_session_lifecycle(&mut state, SessionLifecycleEvent::TransportFailed).is_err() {
        tracing::debug!(
            session_id = %session.session_id,
            transport_generation,
            lifecycle = ?state.info.lifecycle,
            "ignored stale terminal writer failure"
        );
        return;
    }
    state
        .engine
        .invalidate_active_transcript(TranscriptIncompleteReason::Disconnected);
    session
        .shell_integration
        .lock()
        .await
        .end_command_tracking(CommandBlockUnknownReason::WriterFailed);
    let revision = state.engine.revision().wrapping_add(1);
    session.changes.send_replace(SessionChange {
        revision,
        kind: SessionChangeKind::Lifecycle,
    });
}

/// 将终端网格容量错误转为应用层稳定文案，同时在 source chain 留下原因。
///
/// @author kongweiguang
fn engine_error(error: TerminalEngineError) -> AppError {
    AppError::invalid("终端尺寸或引擎请求超过允许容量").with_source(error)
}

#[cfg(test)]
#[path = "../tests/unit/session_resize_diagnostics.rs"]
mod resize_diagnostic_tests;
