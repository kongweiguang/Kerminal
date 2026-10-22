//! Agent 会话用例编排；工作区 I/O 和终端运行时分别通过 application ports 注入。
//!
//! @author kongweiguang

use async_trait::async_trait;
use serde::Deserialize;
use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    path::Path,
    sync::{
        Arc, RwLock as StdRwLock, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use kerminal_domain::{
    agents::{
        AgentDomainError, AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSession,
        AgentSessionScope, AgentSessionStatus, AgentTargetBinding, AgentTargetLiveStatus,
    },
    identity::AgentSessionId,
    sessions::{
        LocalSessionTarget, SessionInfo, SessionLifecycle, SessionOwner, SessionTarget,
        TerminalSize,
    },
};
use tokio::{
    sync::{Mutex, MutexGuard, RwLock},
    task::JoinSet,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;
use zeroize::Zeroizing;

use crate::{
    AppError, AppResult, PreferencesService, ProfileService,
    mcp_runtime::McpRuntimeControl,
    sessions::{
        AgentTerminalSubmissionRequest, CreateSessionRequest, MAX_AGENT_TERMINAL_SUBMISSION_BYTES,
        PlainTextSnapshot, PlainTextSnapshotRequest, SessionFilter, SessionService,
        logging::redact_terminal_text,
    },
};

use super::{
    AgentHistoryProjection, AgentHistoryRequest, AgentLaunchResolver, AgentLauncherCatalog,
    AgentMcpEndpointContextProjection, AgentProviderSessionSnapshot, AgentProviderSessionStatus,
    AgentResumeCapability, AgentResumeRecovery, AgentResumeRecoveryStatus, AgentService,
    AgentSessionContextPathsProjection, AgentSessionLaunchProjection, AgentSessionPathsProjection,
    AgentSessionRecordDiagnostic, AgentSessionRecordProjection,
    AgentSessionRecordSessionProjection, AgentSessionView,
    AgentTargetBindingContextBindingProjection, AgentTargetBindingContextProjection,
    AgentTargetContext, AgentTargetResolution, AgentTerminalContextProjection,
    AgentTerminalSnapshotRequest, AgentTextPreview, AgentTextSubmissionReceipt,
    AgentWorkspaceDocumentKind, AgentWorkspaceRepository, AgentWorkspaceRevision,
    AgentWorkspaceSessionSnapshot, CreateAgentSessionFromHistoryRequest, CreateAgentSessionRequest,
    CreateSelectedAgentSessionRequest, RenameAgentSessionTitleRequest, ResolvedAgentLauncher,
    SendAgentCommandRequest,
    launch::initial_provider_session,
    repository::{
        AgentResumeCheckpointPhase, AgentResumeCheckpointSnapshot,
        CommitAgentResumeCheckpointRequest, PrepareAgentResumeCheckpointRequest,
    },
    workspace_documents::{
        ProviderDocumentWrite, SessionDocumentWrite, WorkspaceSnapshotInput,
        compose_workspace_documents,
    },
};

const DEFAULT_SNAPSHOT_BYTES: usize = 24 * 1024;
const MAX_SNAPSHOT_BYTES: usize = 1024 * 1024;
const AGENT_CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// 持有启动确认后的原 owner 和两种可验证磁盘状态，供重试先读回而不是重复启动 CLI。
#[derive(Clone)]
struct PendingAgentResume {
    recovery: AgentResumeRecovery,
    checkpoint: AgentResumeCheckpointSnapshot,
    resumed_session: AgentSession,
    resumed_provider: AgentProviderSession,
    create_admission: ResumeCreateAdmission,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CreateAdmission {
    NotAdmitted,
    MayHaveStarted,
}

/// 每次 durable attempt 独有的 RAM 准入证据；共享原子位使同步 PTY 调用前能无 await 记录边界。
#[derive(Clone)]
struct ResumeCreateAdmission {
    agent_session_id: AgentSessionId,
    attempt_id: Uuid,
    terminal_session_id: kerminal_domain::identity::SessionId,
    may_have_started: Arc<AtomicBool>,
}

impl ResumeCreateAdmission {
    /// 将准入位绑定到 checkpoint 的固定 attempt 和预留终端 ID，重启后不会伪造该内存证据。
    ///
    /// @author kongweiguang
    fn new(agent_session_id: AgentSessionId, checkpoint: &AgentResumeCheckpointSnapshot) -> Self {
        Self {
            agent_session_id,
            attempt_id: checkpoint.checkpoint.attempt_id,
            terminal_session_id: checkpoint.checkpoint.terminal_session_id.clone(),
            may_have_started: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 读取当前 attempt 的准入阶段；false 只证明本进程尚未进入 provider create 调用。
    ///
    /// @author kongweiguang
    fn state(&self) -> CreateAdmission {
        if self.may_have_started.load(Ordering::Acquire) {
            CreateAdmission::MayHaveStarted
        } else {
            CreateAdmission::NotAdmitted
        }
    }

    /// 仅接受同一 Agent 与预留 SessionId，并在 create future 首次执行前同步线性化为可能已启动。
    ///
    /// @author kongweiguang
    fn mark_may_have_started(
        &self,
        agent_session_id: &AgentSessionId,
        terminal_session_id: &kerminal_domain::identity::SessionId,
    ) -> bool {
        if &self.agent_session_id != agent_session_id
            || &self.terminal_session_id != terminal_session_id
        {
            return false;
        }
        self.may_have_started.store(true, Ordering::Release);
        true
    }

    /// 将 RAM 准入证据与 durable checkpoint 精确配对，禁止旧 attempt 复用新的恢复事务。
    ///
    /// @author kongweiguang
    fn matches_checkpoint(
        &self,
        agent_session_id: &AgentSessionId,
        checkpoint: &AgentResumeCheckpointSnapshot,
    ) -> bool {
        &self.agent_session_id == agent_session_id
            && self.attempt_id == checkpoint.checkpoint.attempt_id
            && self.terminal_session_id == checkpoint.checkpoint.terminal_session_id
    }
}

/// 仅在目录创建成功后构造，绑定本次独占的 ID 与 root；异步回滚始终由用例显式等待。
///
/// @author kongweiguang
struct CreatedAgentWorkspace<'a> {
    id: &'a AgentSessionId,
    root: String,
}

/// Agent 生命周期用例实现；只编排业务，不承担路径解析、磁盘恢复或 PTY 所有权。
pub struct AgentSessionService {
    pub(super) sessions: Arc<dyn SessionService>,
    pub(super) workspace: Arc<dyn AgentWorkspaceRepository>,
    pub(super) launch_resolver: AgentLaunchResolver,
    endpoint: RwLock<Option<String>>,
    mcp_runtime: StdRwLock<Option<Weak<dyn McpRuntimeControl>>>,
    operation_gate: Mutex<()>,
    pending_resumes: RwLock<BTreeMap<String, PendingAgentResume>>,
    shutdown: CancellationToken,
    shutting_down: AtomicBool,
    shutdown_complete: AtomicBool,
}

impl AgentSessionService {
    /// 复用桌面组合根的 PreferencesService、SessionService 和安全工作区仓储。
    pub fn new(
        sessions: Arc<dyn SessionService>,
        workspace: Arc<dyn AgentWorkspaceRepository>,
        preferences: Arc<PreferencesService>,
    ) -> Self {
        Self {
            sessions,
            workspace,
            launch_resolver: AgentLaunchResolver::new(preferences),
            endpoint: RwLock::new(None),
            mcp_runtime: StdRwLock::new(None),
            operation_gate: Mutex::new(()),
            pending_resumes: RwLock::new(BTreeMap::new()),
            shutdown: CancellationToken::new(),
            shutting_down: AtomicBool::new(false),
            shutdown_complete: AtomicBool::new(false),
        }
    }

    /// 将桌面同一 ProfileService 注入 launcher resolver，使 Windows shell 命令遵循实际 profile。
    pub fn with_profile_service(mut self, profile_service: Arc<ProfileService>) -> Self {
        self.launch_resolver = self.launch_resolver.with_profile_service(profile_service);
        self
    }

    /// 同步注入同一个桌面 MCP runtime 的弱引用，避免 Agent 与 runtime controller 形成 Arc 环。
    pub fn set_mcp_runtime(&self, runtime: Weak<dyn McpRuntimeControl>) -> AppResult<()> {
        let mut current = self
            .mcp_runtime
            .write()
            .map_err(|_| AppError::infrastructure("Agent MCP runtime reference is unavailable"))?;
        *current = Some(runtime);
        Ok(())
    }

    /// 兼容组合根的 endpoint 设置入口；通过同一 workspace CAS 用例同步活动 Agent context。
    pub async fn set_mcp_endpoint(&self, endpoint: &str) -> AppResult<()> {
        self.configure_mcp_endpoint(Some(endpoint.to_owned()), CancellationToken::new())
            .await
    }

    /// 在 Agent 操作门内按 workspace CAS 同步 endpoint；部分失败时不发布可供新会话使用的地址。
    pub async fn configure_mcp_endpoint(
        &self,
        endpoint: Option<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        if let Some(endpoint) = endpoint.as_deref() {
            validate_mcp_endpoint(endpoint)?;
        }
        let _gate = self
            .acquire_operation(&cancellation, "Agent MCP endpoint update was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent MCP endpoint update was cancelled")?;

        // 先撤销旧值，避免停止后或部分同步失败时新会话拿到已关闭的 listener 地址。
        *self.endpoint.write().await = None;
        self.sync_live_agent_endpoints(endpoint.as_deref(), &cancellation)
            .await?;
        if let Some(endpoint) = endpoint {
            *self.endpoint.write().await = Some(endpoint);
        }
        Ok(())
    }

    /// 只更新正在运行的 Active Agent workspace；归档、stale 及自定义 AGENTS 文件均不触碰。
    async fn sync_live_agent_endpoints(
        &self,
        endpoint: Option<&str>,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        let sessions = self
            .await_cancellable(cancellation, |token| self.workspace.list_sessions(token))
            .await?;
        let terminals = self.sessions.list(SessionFilter::default()).await?;
        let live_agent_ids = terminals
            .iter()
            .filter(|terminal| terminal.lifecycle == SessionLifecycle::Running)
            .filter_map(|terminal| match &terminal.owner {
                SessionOwner::Agent { agent_session_id } => {
                    Some(agent_session_id.as_str().to_owned())
                }
                _ => None,
            })
            .collect::<std::collections::BTreeSet<_>>();
        let mut first_error = None;

        for snapshot in sessions.into_iter().filter(|snapshot| {
            snapshot.session.status == AgentSessionStatus::Active
                && live_agent_ids.contains(snapshot.session.agent_session_id.as_str())
        }) {
            self.ensure_request_active(cancellation, "Agent MCP endpoint update was cancelled")?;
            let id = snapshot.session.agent_session_id.clone();
            let document = self
                .await_cancellable(cancellation, |token| {
                    self.workspace.read_document(
                        id.clone(),
                        AgentWorkspaceDocumentKind::McpEndpointContext,
                        token,
                    )
                })
                .await;
            let document = match document {
                Ok(document) => document,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            if let Some(contents) = document.contents.as_deref() {
                let stored = match serde_json::from_str::<StoredMcpEndpointContext>(contents) {
                    Ok(stored) => stored,
                    Err(error) => {
                        first_error.get_or_insert_with(|| {
                            AppError::configuration_corrupt("Agent MCP endpoint context")
                                .with_source(error)
                        });
                        continue;
                    }
                };
                if let Err(error) = stored.validate_for(&snapshot.session) {
                    first_error.get_or_insert(error);
                    continue;
                }
            }
            let document = match super::workspace_documents::serialize_mcp_endpoint_document(
                &snapshot.session,
                endpoint,
                document.revision,
            ) {
                Ok(document) => document,
                Err(error) => {
                    first_error.get_or_insert(error);
                    continue;
                }
            };
            let id = snapshot.session.agent_session_id.clone();
            if let Err(error) = self
                .await_cancellable(cancellation, |token| {
                    self.workspace.write_documents(id, vec![document], token)
                })
                .await
            {
                first_error.get_or_insert(error);
            }
        }

        if let Some(error) = first_error {
            Err(error)
        } else {
            Ok(())
        }
    }

    /// 在进入 Agent operation gate 前启动/核验同一个 MCP controller，避免 controller 回调死锁。
    pub(super) async fn ensure_mcp_runtime_started(
        &self,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        let weak_runtime = self
            .mcp_runtime
            .read()
            .map_err(|_| AppError::infrastructure("Agent MCP runtime reference is unavailable"))?
            .clone();
        let runtime =
            match weak_runtime {
                Some(runtime) => Some(runtime.upgrade().ok_or_else(|| {
                    AppError::unsupported("MCP runtime controller is unavailable")
                })?),
                None => None,
            };
        if let Some(runtime) = runtime {
            runtime.start(cancellation.clone()).await?;
        }
        self.endpoint().await.map(|_| ())
    }

    /// 同步关闭新建/恢复入口并通知正在执行的端口操作协作取消。
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        self.shutdown.cancel();
    }

    /// 使用独立便捷预算关闭 Agent CLI；桌面退出应调用共享绝对 deadline 版本。
    pub async fn shutdown(&self) -> AppResult<()> {
        self.shutdown_until(Instant::now() + AGENT_CLOSE_TIMEOUT)
            .await
    }

    /// 停止 intake、等待当前用例收束，并并行关闭所有属于 Agent 的终端。
    pub async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        self.begin_shutdown();
        if self.shutdown_complete.load(Ordering::Acquire) {
            return Ok(());
        }
        let _gate = timeout_at(deadline, self.operation_gate.lock())
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("Agent shutdown deadline elapsed", error)
            })?;
        if self.shutdown_complete.load(Ordering::Acquire) {
            return Ok(());
        }

        let sessions = timeout_at(deadline, self.sessions.list(SessionFilter::default()))
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("Agent shutdown deadline elapsed", error)
            })??;
        let mut closing = JoinSet::new();
        for session in sessions.into_iter().filter(|session| {
            matches!(session.owner, SessionOwner::Agent { .. })
                && !matches!(session.lifecycle, SessionLifecycle::Closed)
        }) {
            let service = Arc::clone(&self.sessions);
            closing.spawn(async move {
                let id = session.session_id;
                let result = service.close_until(id.clone(), deadline).await;
                (id, result)
            });
        }
        let mut first_error = None;
        while let Some(result) = closing.join_next().await {
            match result {
                Ok((_, Ok(()))) => {}
                Ok((_, Err(error))) if first_error.is_none() => first_error = Some(error),
                Err(error) if first_error.is_none() => {
                    first_error = Some(AppError::infrastructure_with_source(
                        "Agent terminal close task failed",
                        error,
                    ));
                }
                _ => {}
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }
        self.shutdown_complete.store(true, Ordering::Release);
        Ok(())
    }

    /// 按当前已验证 selected/custom 定义新会话；后续 resume 独立读取已保存 provider 能力。
    ///
    /// @author kongweiguang
    pub async fn create_selected(
        &self,
        request: CreateSelectedAgentSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        self.ensure_mcp_runtime_started(&cancellation).await?;
        let _gate = self
            .acquire_operation(&cancellation, "Agent creation was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent creation was cancelled")?;
        let endpoint = self.endpoint().await?;
        let launcher = self
            .launch_resolver
            .resolve_for_key(request.launcher_key.as_ref(), &cancellation)
            .await?;
        self.ensure_request_active(&cancellation, "Agent creation was cancelled")?;
        request.target.validate().map_err(domain_error)?;
        self.validate_target_binding(&request.target, &cancellation)
            .await?;

        let id = AgentSessionId::new();
        let root = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.create_session_root(id.clone(), token)
            })
            .await?;
        let workspace = CreatedAgentWorkspace { id: &id, root };
        let mut terminal = None;
        let created = self
            .create_session_after_root(
                workspace,
                request,
                launcher,
                &endpoint,
                &cancellation,
                &mut terminal,
            )
            .await;
        match created {
            Ok(view) => Ok(view),
            Err(error) => Err(self.rollback_failed_creation(id, terminal, error).await),
        }
    }

    /// 消费已成功创建的 ID/root 绑定；terminal 回滚槽留在调用方，错误不能丢失已启动 owner。
    ///
    /// @author kongweiguang
    async fn create_session_after_root(
        &self,
        workspace: CreatedAgentWorkspace<'_>,
        request: CreateSelectedAgentSessionRequest,
        launcher: ResolvedAgentLauncher,
        endpoint: &str,
        cancellation: &CancellationToken,
        terminal: &mut Option<SessionInfo>,
    ) -> AppResult<AgentSessionView> {
        let CreatedAgentWorkspace { id, root } = workspace;
        if !Path::new(&root).is_absolute() {
            return Err(AppError::invalid(
                "Agent session workspace must be an absolute path",
            ));
        }
        let launch = launcher.clone().into_session_launch(root.clone())?;
        let title = match request.title {
            Some(title) if !title.trim().is_empty() => title,
            Some(_) => return Err(AppError::invalid("Agent title is required")),
            None => default_title(launcher.provider),
        };
        let timestamp = current_timestamp();
        let session = AgentSession::new(
            id.clone(),
            launcher.provider,
            Some(launcher.launcher_key),
            title,
            timestamp,
            self.workspace.workspace_root(),
            root,
            launch,
            Some(request.scope),
            Some(request.target),
        )
        .map_err(domain_error)?;
        self.start_created_session(session, endpoint, cancellation, terminal)
            .await
    }

    /// 首次持久化前固定实际 target 身份，使启动前 context 和后续返回值一致。
    ///
    /// @author kongweiguang
    pub(super) async fn start_created_session(
        &self,
        mut session: AgentSession,
        endpoint: &str,
        cancellation: &CancellationToken,
        terminal: &mut Option<SessionInfo>,
    ) -> AppResult<AgentSessionView> {
        self.ensure_request_active(cancellation, "Agent creation was cancelled")?;
        ensure_session_target_binding_id(&mut session);
        let provider_session = initial_provider_session(session.agent_id);
        let mut session_revision = self
            .persist_workspace_with_provider(
                SessionDocumentWrite {
                    session: &session,
                    expected_revision: AgentWorkspaceRevision::Missing,
                },
                Some(ProviderDocumentWrite {
                    provider: &provider_session,
                    expected_revision: AgentWorkspaceRevision::Missing,
                }),
                endpoint,
                None,
                WorkspaceSnapshotInput {
                    target_session_id: None,
                    output: "",
                    max_bytes: DEFAULT_SNAPSHOT_BYTES,
                    truncated: false,
                    redacted: false,
                },
                cancellation,
            )
            .await?;

        let created = self
            .start_agent_terminal(
                &session,
                &session.launch,
                endpoint,
                None,
                None,
                cancellation,
            )
            .await?;
        *terminal = Some(created.clone());
        let (resolution, target) = self
            .resolve_target_inner(&mut session, &mut session_revision, cancellation)
            .await?;
        let (snapshot, redacted) = self.capture_target_snapshot(target.as_ref()).await?;
        let output = snapshot
            .as_ref()
            .map(|snapshot| snapshot.text.as_str())
            .unwrap_or_default();
        self.persist_workspace(
            SessionDocumentWrite {
                session: &session,
                expected_revision: session_revision,
            },
            endpoint,
            Some(&created),
            WorkspaceSnapshotInput {
                target_session_id: None,
                output,
                max_bytes: DEFAULT_SNAPSHOT_BYTES,
                truncated: snapshot.as_ref().is_some_and(|snapshot| snapshot.truncated),
                redacted,
            },
            cancellation,
        )
        .await?;
        let _ = resolution;
        Ok(AgentSessionView {
            resume_capability: project_resume_capability(
                &session,
                &AgentProviderSessionStatus::Available(provider_session),
            ),
            session,
            terminal: Some(created),
            resume_recovery: None,
        })
    }

    /// 关闭部分创建的终端后再回滚新目录；清理失败升级为未知结果，不能向界面误报已取消。
    ///
    /// @author kongweiguang
    pub(super) async fn rollback_failed_creation(
        &self,
        id: AgentSessionId,
        terminal: Option<SessionInfo>,
        mut error: AppError,
    ) -> AppError {
        if let Some(terminal) = terminal
            && let Err(cleanup_error) = self
                .sessions
                .close_until(terminal.session_id, Instant::now() + AGENT_CLOSE_TIMEOUT)
                .await
        {
            return AppError::outcome_unknown_with_source(
                "Agent creation cleanup is incomplete; verify the existing session before retrying",
                error.with_source(cleanup_error),
            );
        }
        if error.public_code() != crate::PublicErrorCode::OutcomeUnknown
            && let Err(cleanup_error) = self.workspace.rollback_created_session_root(id).await
        {
            error = AppError::outcome_unknown_with_source(
                "Agent creation workspace cleanup is incomplete; verify it before retrying",
                error.with_source(cleanup_error),
            );
        }
        error
    }

    /// 列出持久会话并根据真实 terminal registry 将退出的 Active CLI 标记为 Stale。
    /// 恢复能力是可选投影，普通元数据读取故障不能让整份历史列表不可用。
    pub async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent list was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent list was cancelled")?;
        let mut sessions = self
            .await_cancellable(&cancellation, |token| self.workspace.list_sessions(token))
            .await?;
        self.ensure_request_active(&cancellation, "Agent list was cancelled")?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let mut views = Vec::with_capacity(sessions.len());
        for snapshot in &mut sessions {
            self.ensure_request_active(&cancellation, "Agent list was cancelled")?;
            let terminal = find_agent_terminal(&live, &snapshot.session.agent_session_id);
            let resume_recovery = self
                .resume_recovery_for_view(&snapshot.session, terminal.as_ref(), &cancellation)
                .await?;
            if resume_recovery.is_none() {
                self.reconcile_cli_state(snapshot, terminal.as_ref(), &cancellation)
                    .await?;
            }
            let resume_capability = self
                .read_resume_capability(&snapshot.session, &cancellation)
                .await?;
            views.push(AgentSessionView {
                session: snapshot.session.clone(),
                terminal,
                resume_recovery,
                resume_capability,
            });
        }
        Ok(views)
    }

    /// 读取指定 AgentSessionId 并仅关联归属字段完全匹配的运行态 terminal。
    /// 可选恢复能力无法确认时仍返回会话详情，由 UI 隐藏继续入口。
    pub async fn get(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent lookup was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent lookup was cancelled")?;
        let mut snapshot = self.read_session(id.clone(), &cancellation).await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        let resume_recovery = self
            .resume_recovery_for_view(&snapshot.session, terminal.as_ref(), &cancellation)
            .await?;
        if resume_recovery.is_none() {
            self.reconcile_cli_state(&mut snapshot, terminal.as_ref(), &cancellation)
                .await?;
        }
        let resume_capability = self
            .read_resume_capability(&snapshot.session, &cancellation)
            .await?;
        Ok(AgentSessionView {
            session: snapshot.session,
            terminal,
            resume_recovery,
            resume_capability,
        })
    }

    /// 在同一操作门内读取记录和恢复状态；不暴露 launch 环境值，也不隐藏未确认的 CLI owner。
    pub async fn record_projection(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionRecordProjection> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent record lookup was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent record lookup was cancelled")?;
        let mut stored = self.read_session(id.clone(), &cancellation).await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        let provider_snapshot = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await?;
        let recovery_before_target_resolution = self
            .resume_recovery_for_view(&stored.session, terminal.as_ref(), &cancellation)
            .await?;
        if recovery_before_target_resolution.is_none() {
            self.reconcile_cli_state(&mut stored, terminal.as_ref(), &cancellation)
                .await?;
            let (_resolution, _target) = self
                .resolve_target_inner(&mut stored.session, &mut stored.revision, &cancellation)
                .await?;
        }
        let resume_recovery = if recovery_before_target_resolution.is_some() {
            recovery_before_target_resolution
        } else {
            self.resume_recovery_for_view(&stored.session, terminal.as_ref(), &cancellation)
                .await?
        };
        self.project_session_record(
            &stored.session,
            provider_snapshot,
            resume_recovery,
            &cancellation,
        )
        .await
    }

    /// 显式按 provider 持久恢复能力继续会话；普通 checkpoint 固化 target ID，未决恢复仍严格沿用原意图。
    ///
    /// @author kongweiguang
    pub async fn resume(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        self.ensure_mcp_runtime_started(&cancellation).await?;
        let _gate = self
            .acquire_operation(&cancellation, "Agent resume was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent resume was cancelled")?;
        // A durable checkpoint is the first authority for this path: it prevents a restart or a
        // lost in-memory Pending entry from turning an uncertain attempt into a second CLI.
        let checkpoint = self.read_resume_checkpoint(&id, &cancellation).await?;
        let stored = self.read_session(id.clone(), &cancellation).await?;
        let session = stored.session.clone();
        let pending = self.pending_resume(&id).await;
        if let Some(checkpoint) = checkpoint {
            return self
                .reconcile_durable_resume_checkpoint(
                    &id,
                    checkpoint,
                    pending,
                    stored,
                    &cancellation,
                )
                .await;
        }
        if let Some(pending) = pending {
            if let Some(view) = self
                .confirm_absent_committed_resume(&id, &pending, &stored, &cancellation)
                .await?
            {
                return Ok(view);
            }
            let live = self.sessions.list(SessionFilter::default()).await?;
            let terminal = find_agent_terminal(&live, &id);
            return Ok(AgentSessionView {
                resume_capability: self.read_resume_capability(&session, &cancellation).await?,
                session,
                terminal,
                resume_recovery: Some(AgentResumeRecovery {
                    terminal_session_id: pending.recovery.terminal_session_id,
                    status: AgentResumeRecoveryStatus::Unknown,
                }),
            });
        }
        if session.status == AgentSessionStatus::Archived {
            return Err(AppError::conflict("Archived Agent session cannot resume"));
        }
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        if let Some(terminal) = terminal
            && terminal_has_live_owner(&terminal)
        {
            if terminal.lifecycle != SessionLifecycle::Running {
                return Err(AppError::conflict(
                    "Agent CLI is starting or closing; wait for the existing owner before resuming",
                ));
            }
            let recovery =
                (session.status != AgentSessionStatus::Active).then(|| AgentResumeRecovery {
                    terminal_session_id: terminal.session_id.clone(),
                    status: AgentResumeRecoveryStatus::Unknown,
                });
            let resume_capability = self.read_resume_capability(&session, &cancellation).await?;
            return Ok(AgentSessionView {
                session,
                terminal: Some(terminal),
                resume_recovery: recovery,
                resume_capability,
            });
        }

        let endpoint = self.endpoint().await?;
        let provider_snapshot = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await?;
        let mut provider_session =
            provider_session_for_resume(&provider_snapshot.status, session.agent_id)?;
        let launch = self
            .launch_resolver
            .resolve_resume_launch(&provider_session, session.session_root.clone())
            .await?;
        let resumed_at = current_timestamp();
        let mut resumed_session = session.clone();
        ensure_session_target_binding_id(&mut resumed_session);
        resumed_session
            .mark_resumed(launch.clone(), resumed_at.clone())
            .map_err(domain_error)?;
        provider_session.last_resume_at = Some(resumed_at);

        let attempt_id = Uuid::new_v4();
        let terminal_session_id = kerminal_domain::identity::SessionId::new();
        let prepared = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.prepare_resume_checkpoint(
                    PrepareAgentResumeCheckpointRequest {
                        agent_session_id: id.clone(),
                        attempt_id,
                        terminal_session_id: terminal_session_id.clone(),
                        base_session_revision: stored.revision.clone(),
                        base_provider_revision: provider_snapshot.revision.clone(),
                        intended_session: resumed_session.clone(),
                        intended_provider: provider_session.clone(),
                    },
                    token,
                )
            })
            .await?;
        let pending = PendingAgentResume {
            recovery: AgentResumeRecovery {
                terminal_session_id: terminal_session_id.clone(),
                status: AgentResumeRecoveryStatus::Pending,
            },
            checkpoint: prepared.clone(),
            resumed_session: resumed_session.clone(),
            resumed_provider: provider_session.clone(),
            create_admission: ResumeCreateAdmission::new(id.clone(), &prepared),
        };
        self.remember_pending_resume(&id, pending.clone()).await;
        // prepare may complete after caller cancellation; recheck before any PTY side effect.
        self.ensure_request_active(&cancellation, "Agent resume was cancelled")?;

        let created = match self
            .start_agent_terminal(
                &session,
                &launch,
                &endpoint,
                Some(terminal_session_id.clone()),
                Some(pending.create_admission.clone()),
                &cancellation,
            )
            .await
        {
            Ok(created) => created,
            Err(error) => {
                if pending.create_admission.state() == CreateAdmission::NotAdmitted
                    && error.public_code() == crate::PublicErrorCode::Cancelled
                {
                    return Err(error);
                }
                // A failed open has no safe implicit cleanup receipt; clear only if closing this
                // exact reserved ID succeeds and the checkpoint deletion is read back as absent.
                if pending.create_admission.state() == CreateAdmission::MayHaveStarted
                    && self
                        .sessions
                        .close_until(
                            terminal_session_id.clone(),
                            Instant::now() + AGENT_CLOSE_TIMEOUT,
                        )
                        .await
                        .is_ok()
                    && self
                        .clear_resume_checkpoint_confirmed(&id, &prepared, &cancellation)
                        .await
                {
                    self.clear_pending_resume(&id).await;
                    return Err(error);
                }
                self.set_pending_resume_status(&id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                let live = self.sessions.list(SessionFilter::default()).await?;
                return Ok(AgentSessionView {
                    resume_capability: project_resume_capability(
                        &session,
                        &provider_snapshot.status,
                    ),
                    session,
                    terminal: find_agent_terminal(&live, &id),
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                });
            }
        };
        if created.session_id != terminal_session_id
            || created.owner
                != (SessionOwner::Agent {
                    agent_session_id: id.clone(),
                })
            || created.lifecycle != SessionLifecycle::Running
        {
            let _ = self
                .sessions
                .close_until(
                    created.session_id.clone(),
                    Instant::now() + AGENT_CLOSE_TIMEOUT,
                )
                .await;
            self.set_pending_resume_status(&id, AgentResumeRecoveryStatus::Unknown)
                .await;
            return Ok(AgentSessionView {
                resume_capability: project_resume_capability(&session, &provider_snapshot.status),
                session,
                terminal: Some(created),
                resume_recovery: Some(AgentResumeRecovery {
                    terminal_session_id,
                    status: AgentResumeRecoveryStatus::Unknown,
                }),
            });
        }

        let committed = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.commit_resume_checkpoint(
                    CommitAgentResumeCheckpointRequest {
                        agent_session_id: id.clone(),
                        prepared: prepared.clone(),
                        intended_session: resumed_session.clone(),
                        intended_provider: provider_session.clone(),
                    },
                    token,
                )
            })
            .await;
        let committed = match committed {
            Ok(committed)
                if committed.checkpoint.attempt_id == prepared.checkpoint.attempt_id
                    && committed.checkpoint.terminal_session_id
                        == prepared.checkpoint.terminal_session_id
                    && committed.checkpoint.base_session_revision
                        == prepared.checkpoint.base_session_revision
                    && committed.checkpoint.base_provider_revision
                        == prepared.checkpoint.base_provider_revision
                    && committed.checkpoint.intended_session_revision
                        == prepared.checkpoint.intended_session_revision
                    && committed.checkpoint.intended_provider_revision
                        == prepared.checkpoint.intended_provider_revision
                    && committed.checkpoint.phase == AgentResumeCheckpointPhase::Committed =>
            {
                committed
            }
            Ok(_) => {
                self.set_pending_resume_status(&id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                return Ok(AgentSessionView {
                    resume_capability: project_resume_capability(
                        &session,
                        &provider_snapshot.status,
                    ),
                    session,
                    terminal: Some(created),
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                });
            }
            Err(error) => {
                let status = recovery_status_for_error(&error);
                self.set_pending_resume_status(&id, status).await;
                return Ok(AgentSessionView {
                    resume_capability: project_resume_capability(
                        &session,
                        &provider_snapshot.status,
                    ),
                    session,
                    terminal: Some(created),
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id,
                        status,
                    }),
                });
            }
        };
        let mut committed_pending = pending;
        committed_pending.checkpoint = committed.clone();
        self.remember_pending_resume(&id, committed_pending.clone())
            .await;
        self.confirm_committed_resume_checkpoint(
            &id,
            committed,
            Some(&committed_pending),
            &session,
            &provider_snapshot.status,
            &cancellation,
        )
        .await
    }

    /// 优先核对 durable 原事务；存在未决 checkpoint 时缺少 Running exact owner 仍保持 Unknown。
    ///
    /// @author kongweiguang
    pub async fn reconcile_resume(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        self.ensure_mcp_runtime_started(&cancellation).await?;
        let _gate = self
            .acquire_operation(&cancellation, "Agent recovery reconciliation was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent recovery reconciliation was cancelled")?;

        // Read the durable marker before session lifecycle or RAM state so a restarted process
        // cannot infer that an absent owner means the previous create/close was safely settled.
        let checkpoint = self.read_resume_checkpoint(&id, &cancellation).await?;
        let mut stored = self.read_session(id.clone(), &cancellation).await?;
        let pending = self.pending_resume(&id).await;
        if let Some(checkpoint) = checkpoint {
            return self
                .reconcile_durable_resume_checkpoint(
                    &id,
                    checkpoint,
                    pending,
                    stored,
                    &cancellation,
                )
                .await;
        }
        if let Some(pending) = pending {
            if let Some(view) = self
                .confirm_absent_committed_resume(&id, &pending, &stored, &cancellation)
                .await?
            {
                return Ok(view);
            }
            let live = self.sessions.list(SessionFilter::default()).await?;
            let terminal = find_agent_terminal(&live, &id);
            let resume_capability = self
                .read_resume_capability(&stored.session, &cancellation)
                .await?;
            return Ok(AgentSessionView {
                session: stored.session,
                terminal,
                resume_recovery: Some(AgentResumeRecovery {
                    terminal_session_id: pending.recovery.terminal_session_id,
                    status: AgentResumeRecoveryStatus::Unknown,
                }),
                resume_capability,
            });
        }
        if stored.session.status == AgentSessionStatus::Archived {
            return Err(AppError::conflict(
                "Archived Agent session has no pending resume to reconcile",
            ));
        }

        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        let resume_recovery = self
            .resume_recovery_for_view(&stored.session, terminal.as_ref(), &cancellation)
            .await?;
        let resume_capability = self
            .read_resume_capability(&stored.session, &cancellation)
            .await?;
        if resume_recovery.is_none() {
            self.reconcile_cli_state(&mut stored, terminal.as_ref(), &cancellation)
                .await?;
        }
        Ok(AgentSessionView {
            session: stored.session,
            terminal,
            resume_recovery,
            resume_capability,
        })
    }

    /// 先关闭 CLI 并确认资源收尾，再保存归档状态，避免持久状态伪装成已关闭。
    ///
    /// @author kongweiguang
    pub async fn archive(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent archive was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent archive was cancelled")?;
        if self
            .read_resume_checkpoint(&id, &cancellation)
            .await?
            .is_some()
            || self.pending_resume(&id).await.is_some()
        {
            return Err(AppError::conflict(
                "Agent resume recovery must be reconciled before archiving",
            ));
        }
        let endpoint = self.endpoint().await?;
        let stored = self.read_session(id.clone(), &cancellation).await?;
        let resume_capability = self
            .read_resume_capability(&stored.session, &cancellation)
            .await?;
        let mut session = stored.session;
        let session_revision = stored.revision;
        if let Some(terminal) = self.find_agent_terminal(&id).await? {
            self.ensure_request_active(&cancellation, "Agent archive was cancelled")?;
            self.sessions
                .close_until(terminal.session_id, Instant::now() + AGENT_CLOSE_TIMEOUT)
                .await?;
        }
        session.archive(current_timestamp());
        self.persist_workspace(
            SessionDocumentWrite {
                session: &session,
                expected_revision: session_revision,
            },
            &endpoint,
            None,
            WorkspaceSnapshotInput {
                target_session_id: None,
                output: "",
                max_bytes: DEFAULT_SNAPSHOT_BYTES,
                truncated: false,
                redacted: false,
            },
            &cancellation,
        )
        .await?;
        Ok(AgentSessionView {
            session,
            terminal: None,
            resume_recovery: None,
            resume_capability,
        })
    }

    /// 仅显式更新目标 binding；同代同目标重放沿用 ID，低代次冲突且失效目标不自动改绑。
    ///
    /// @author kongweiguang
    pub async fn bind_preferred_target(
        &self,
        id: AgentSessionId,
        mut binding: AgentTargetBinding,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent target binding was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent target binding was cancelled")?;
        let stored = self.read_session(id.clone(), &cancellation).await?;
        self.ensure_resume_recovery_resolved_before_mutation(&stored.session, &cancellation)
            .await?;
        let resume_capability = self
            .read_resume_capability(&stored.session, &cancellation)
            .await?;
        let mut session = stored.session;
        ensure_target_binding_id(&mut binding, session.target.as_ref());
        self.validate_target_binding(&binding, &cancellation)
            .await?;
        let endpoint = self.endpoint().await?;
        let mut session_revision = stored.revision;
        session
            .bind_preferred_target(binding, current_timestamp())
            .map_err(domain_error)?;
        let (resolution, target) = self
            .resolve_target_inner(&mut session, &mut session_revision, &cancellation)
            .await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        let (snapshot, redacted) = self.capture_target_snapshot(target.as_ref()).await?;
        self.persist_workspace(
            SessionDocumentWrite {
                session: &session,
                expected_revision: session_revision,
            },
            &endpoint,
            terminal.as_ref(),
            WorkspaceSnapshotInput {
                target_session_id: None,
                output: snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.text.as_str())
                    .unwrap_or_default(),
                max_bytes: DEFAULT_SNAPSHOT_BYTES,
                truncated: snapshot.as_ref().is_some_and(|snapshot| snapshot.truncated),
                redacted,
            },
            &cancellation,
        )
        .await?;
        let _ = resolution;
        let resume_recovery = self
            .resume_recovery_for_view(&session, terminal.as_ref(), &cancellation)
            .await?;
        Ok(AgentSessionView {
            session,
            terminal,
            resume_recovery,
            resume_capability,
        })
    }

    /// 返回绑定的实际运行状态；无绑定、stale 或 closed 均不枚举其它终端补位。
    ///
    /// @author kongweiguang
    pub async fn resolve_preferred_target(
        &self,
        id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent target resolution was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent target resolution was cancelled")?;
        let stored = self.read_session(id, &cancellation).await?;
        self.ensure_resume_recovery_resolved_before_mutation(&stored.session, &cancellation)
            .await?;
        let mut session = stored.session;
        let mut session_revision = stored.revision;
        let (resolution, target) = self
            .resolve_target_inner(&mut session, &mut session_revision, &cancellation)
            .await?;
        let endpoint = self.endpoint().await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &session.agent_session_id);
        let (snapshot, redacted) = self.capture_target_snapshot(target.as_ref()).await?;
        self.persist_workspace(
            SessionDocumentWrite {
                session: &session,
                expected_revision: session_revision,
            },
            &endpoint,
            terminal.as_ref(),
            WorkspaceSnapshotInput {
                target_session_id: None,
                output: snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.text.as_str())
                    .unwrap_or_default(),
                max_bytes: DEFAULT_SNAPSHOT_BYTES,
                truncated: snapshot.as_ref().is_some_and(|snapshot| snapshot.truncated),
                redacted,
            },
            &cancellation,
        )
        .await?;
        Ok(resolution)
    }

    /// 按指定上限读取脱敏 terminal snapshot，并保持失效目标与 Agent CLI 状态互相独立。
    /// 复用同一 provider 快照投影恢复能力，避免上下文与会话列表对能力作不同判断。
    ///
    /// @author kongweiguang
    pub async fn target_context(
        &self,
        id: AgentSessionId,
        max_snapshot_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent context request was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent context request was cancelled")?;
        if max_snapshot_bytes == 0 || max_snapshot_bytes > MAX_SNAPSHOT_BYTES {
            return Err(AppError::invalid(
                "Agent snapshot limit must be between 1 and 1048576",
            ));
        }
        let mut stored = self.read_session(id.clone(), &cancellation).await?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &id);
        let provider_snapshot = self
            .await_cancellable(&cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await?;
        let recovery_before_target_resolution = self
            .resume_recovery_for_view(&stored.session, terminal.as_ref(), &cancellation)
            .await?;
        if recovery_before_target_resolution.is_none() {
            self.reconcile_cli_state(&mut stored, terminal.as_ref(), &cancellation)
                .await?;
        }
        let mut session = stored.session.clone();
        let mut session_revision = stored.revision;
        let (target, resolved) = if recovery_before_target_resolution.is_some() {
            let binding = session
                .target
                .clone()
                .unwrap_or_else(|| AgentTargetBinding::unbound(0));
            (
                AgentTargetResolution {
                    agent_session_id: session.agent_session_id.clone(),
                    status: binding.live_status,
                    binding,
                    resolved_session_id: None,
                    observed_at_ms: current_epoch_millis()?,
                },
                None,
            )
        } else {
            self.resolve_target_inner(&mut session, &mut session_revision, &cancellation)
                .await?
        };
        let resume_recovery = if recovery_before_target_resolution.is_some() {
            recovery_before_target_resolution
        } else {
            self.resume_recovery_for_view(&session, terminal.as_ref(), &cancellation)
                .await?
        };
        let (snapshot, redacted, captured_bytes) = self
            .capture_target_snapshot_with_limit(resolved.as_ref(), max_snapshot_bytes)
            .await?;
        if resume_recovery.is_none() {
            let endpoint = self.endpoint().await?;
            self.persist_workspace(
                SessionDocumentWrite {
                    session: &session,
                    expected_revision: session_revision,
                },
                &endpoint,
                terminal.as_ref(),
                WorkspaceSnapshotInput {
                    target_session_id: None,
                    output: snapshot
                        .as_ref()
                        .map(|snapshot| snapshot.text.as_str())
                        .unwrap_or_default(),
                    max_bytes: max_snapshot_bytes,
                    truncated: snapshot.as_ref().is_some_and(|snapshot| snapshot.truncated),
                    redacted,
                },
                &cancellation,
            )
            .await?;
        }
        let resume_capability = project_resume_capability(&session, &provider_snapshot.status);
        let record = self
            .project_session_record(&session, provider_snapshot, resume_recovery, &cancellation)
            .await?;
        Ok(AgentTargetContext {
            session: AgentSessionView {
                session,
                terminal,
                resume_recovery: record.resume_recovery.clone(),
                resume_capability,
            },
            record,
            target,
            snapshot,
            captured_bytes,
            redacted,
        })
    }

    /// 为指定 Agent 生成短生命周期的脱敏文本预览；只验证会话存在，不解析目标或写入 workspace。
    ///
    /// @author kongweiguang
    pub async fn preview_text(
        &self,
        agent_session_id: AgentSessionId,
        text: String,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent text preview was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent text preview was cancelled")?;
        let stored = self.read_session(agent_session_id, &cancellation).await?;
        if stored.session.status == AgentSessionStatus::Archived {
            return Err(AppError::conflict(
                "Archived Agent session cannot preview text",
            ));
        }
        self.ensure_request_active(&cancellation, "Agent text preview was cancelled")?;
        project_agent_text_preview(text, max_bytes)
    }

    /// 在 Agent operation gate 内先回读未决恢复，再核对运行 owner/generation 后提交单次 Paste+Enter。
    ///
    /// @author kongweiguang
    pub async fn send_command_block_text(
        &self,
        request: SendAgentCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt> {
        let SendAgentCommandRequest {
            agent_session_id,
            terminal_session_id,
            expected_session_generation,
            text,
        } = request;
        let submission = AgentTerminalSubmissionRequest {
            session_id: terminal_session_id.clone(),
            agent_session_id: agent_session_id.clone(),
            expected_session_generation,
            text,
        };
        submission.validate()?;
        validate_agent_text_is_reviewed(&submission.text)?;

        let _gate = self
            .acquire_operation(&cancellation, "Agent text submission was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent text submission was cancelled")?;
        let stored = self
            .read_session(agent_session_id.clone(), &cancellation)
            .await?;
        if stored.session.status != AgentSessionStatus::Active {
            return Err(AppError::conflict("Agent session is not active"));
        }

        let live = self.sessions.list(SessionFilter::default()).await?;
        let current_agent_terminal = find_agent_terminal(&live, &agent_session_id);
        if let Some(recovery) = self
            .resume_recovery_for_view(
                &stored.session,
                current_agent_terminal.as_ref(),
                &cancellation,
            )
            .await?
        {
            let context = match recovery.status {
                AgentResumeRecoveryStatus::Pending => {
                    "Agent resume metadata is pending confirmation; query the session before sending"
                }
                AgentResumeRecoveryStatus::Unknown => {
                    "Agent resume outcome is unknown; query the session before sending"
                }
            };
            return Err(AppError::outcome_unknown(context));
        }

        let terminal = live
            .iter()
            .find(|candidate| candidate.session_id == terminal_session_id)
            .ok_or_else(|| AppError::not_found("Agent CLI session not found"))?;
        if terminal.lifecycle != SessionLifecycle::Running
            || terminal.owner
                != (SessionOwner::Agent {
                    agent_session_id: agent_session_id.clone(),
                })
        {
            return Err(AppError::conflict(
                "Specified terminal is not the running CLI for this Agent",
            ));
        }

        let snapshot = self
            .sessions
            .command_block_snapshot(terminal_session_id.clone(), &cancellation)
            .await?;
        if snapshot.session_id != terminal_session_id
            || snapshot.session_generation != expected_session_generation
        {
            return Err(AppError::conflict(
                "Agent CLI session generation changed; refresh the preview before sending",
            ));
        }
        self.ensure_request_active(&cancellation, "Agent text submission was cancelled")?;

        // 进入 writer 后不 select/drop Future；adapter 负责区分未写入取消与 ACK 未知，调用方不得重放。
        let receipt = self
            .sessions
            .write_agent_submission(submission, cancellation)
            .await?;
        Ok(AgentTextSubmissionReceipt {
            acknowledged_bytes: receipt.acknowledged_bytes,
        })
    }

    /// 将真实输出二次脱敏后写入快照；恢复未核对前冻结同批 session 写入，防止改变原事务基线。
    ///
    /// @author kongweiguang
    pub async fn record_terminal_snapshot(
        &self,
        request: AgentTerminalSnapshotRequest,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent snapshot write was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent snapshot write was cancelled")?;
        if request.max_bytes == 0
            || request.max_bytes > MAX_SNAPSHOT_BYTES
            || request.output.len() > request.max_bytes
        {
            return Err(AppError::invalid(
                "Agent snapshot exceeds its declared byte limit",
            ));
        }
        let stored = self
            .read_session(request.agent_session_id.clone(), &cancellation)
            .await?;
        self.ensure_resume_recovery_resolved_before_mutation(&stored.session, &cancellation)
            .await?;
        let session = stored.session;
        let session_revision = stored.revision;
        if session.status != AgentSessionStatus::Active {
            return Err(AppError::conflict("Agent session is not active"));
        }
        let live = self.sessions.list(SessionFilter::default()).await?;
        if !find_agent_terminal(&live, &request.agent_session_id)
            .as_ref()
            .is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        {
            return Err(AppError::conflict("Agent CLI session is not running"));
        }
        let target = live
            .iter()
            .find(|terminal| terminal.session_id == request.target_session_id)
            .filter(|terminal| {
                terminal.lifecycle == SessionLifecycle::Running
                    && !matches!(terminal.owner, SessionOwner::Agent { .. })
            })
            .ok_or_else(|| AppError::not_found("Terminal snapshot target not found"))?;
        let (safe, newly_redacted) = redact_terminal_text(&request.output);
        let (output, truncated) = truncate_utf8(&safe, request.max_bytes);
        let redacted = request.redacted || newly_redacted;
        let endpoint = self.endpoint().await?;
        let agent_terminal = find_agent_terminal(&live, &request.agent_session_id);
        // 指定目标只写入 snapshot 元数据，不修改 Agent 的首选绑定或触发跨 session fallback。
        self.persist_workspace(
            SessionDocumentWrite {
                session: &session,
                expected_revision: session_revision,
            },
            &endpoint,
            agent_terminal.as_ref(),
            WorkspaceSnapshotInput {
                target_session_id: Some(&target.session_id),
                output: &output,
                max_bytes: request.max_bytes,
                truncated: request.truncated || truncated,
                redacted,
            },
            &cancellation,
        )
        .await?;
        Ok(())
    }

    /// 读取一个准确 id 的会话并在存储边界拒绝未来 schema 或损坏记录。
    pub(super) async fn read_session(
        &self,
        id: AgentSessionId,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentWorkspaceSessionSnapshot> {
        self.await_cancellable(cancellation, |token| self.workspace.read_session(id, token))
            .await
    }

    /// 从固定仓储边界读取安全记录；provider 快照同时用于恢复核对，损坏文档只产生稳定诊断。
    async fn project_session_record(
        &self,
        session: &AgentSession,
        provider_snapshot: AgentProviderSessionSnapshot,
        resume_recovery: Option<AgentResumeRecovery>,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentSessionRecordProjection> {
        self.ensure_request_active(cancellation, "Agent record lookup was cancelled")?;
        let mut diagnostics = Vec::new();
        let provider = match &provider_snapshot.status {
            AgentProviderSessionStatus::Available(provider) => Some(provider.clone()),
            AgentProviderSessionStatus::Missing => {
                diagnostics.push(record_diagnostic(
                    Some(provider_snapshot.path.clone()),
                    "invalidProvider",
                    "Provider metadata is missing",
                ));
                None
            }
            AgentProviderSessionStatus::Malformed => {
                diagnostics.push(record_diagnostic(
                    Some(provider_snapshot.path.clone()),
                    "invalidProvider",
                    "Provider metadata is malformed",
                ));
                None
            }
            AgentProviderSessionStatus::UnsupportedSchemaVersion(version) => {
                diagnostics.push(record_diagnostic(
                    Some(provider_snapshot.path.clone()),
                    "invalidProvider",
                    &format!("Provider schema version {version} is unsupported"),
                ));
                None
            }
            AgentProviderSessionStatus::ProviderMismatch => {
                diagnostics.push(record_diagnostic(
                    Some(provider_snapshot.path.clone()),
                    "invalidProvider",
                    "Provider metadata does not match the Agent session",
                ));
                None
            }
        };

        let target_binding = self
            .read_record_context::<StoredTargetBindingContext>(
                session,
                AgentWorkspaceDocumentKind::TargetBindingContext,
                cancellation,
                &mut diagnostics,
                "invalidTargetBinding",
                "Target binding context is malformed",
            )
            .await?
            .map(StoredTargetBindingContext::into_projection);
        let mcp_endpoint = self
            .read_record_context::<StoredMcpEndpointContext>(
                session,
                AgentWorkspaceDocumentKind::McpEndpointContext,
                cancellation,
                &mut diagnostics,
                "invalidMcpEndpoint",
                "MCP endpoint context is malformed",
            )
            .await?
            .map(StoredMcpEndpointContext::into_projection);

        let root = Path::new(&session.session_root);
        let context = root.join("context");
        let paths = AgentSessionPathsProjection {
            workspace_root: session.workspace_root.clone(),
            session_root: session.session_root.clone(),
            session_toml: root.join("session.toml").to_string_lossy().into_owned(),
            provider_toml: provider_snapshot.path,
            context: AgentSessionContextPathsProjection {
                target_binding_json: context
                    .join("target-binding.json")
                    .to_string_lossy()
                    .into_owned(),
                mcp_endpoint_json: context
                    .join("mcp-endpoint.json")
                    .to_string_lossy()
                    .into_owned(),
                terminal_snapshot_json: context
                    .join("terminal-snapshot.json")
                    .to_string_lossy()
                    .into_owned(),
                workspace_snapshot_json: context
                    .join("workspace-snapshot.json")
                    .to_string_lossy()
                    .into_owned(),
            },
        };
        Ok(AgentSessionRecordProjection {
            session: AgentSessionRecordSessionProjection {
                schema_version: session.schema_version,
                agent_session_id: session.agent_session_id.to_string(),
                agent_id: session.agent_id,
                launcher_key: session
                    .launcher_key
                    .as_ref()
                    .map(|key| key.as_str().to_owned()),
                title: session.title.clone(),
                created_at: session.created_at.clone(),
                updated_at: session.updated_at.clone(),
                status: session.status,
                workspace_root: session.workspace_root.clone(),
                session_root: session.session_root.clone(),
                launch: AgentSessionLaunchProjection {
                    command_label: session.launch.command_label.clone(),
                    shell: session.launch.shell.clone(),
                    args: session.launch.args.clone(),
                    cwd: session.launch.cwd.clone(),
                },
                scope: session.scope.clone(),
                target: session.target.clone(),
            },
            provider,
            resume_recovery,
            target_binding,
            mcp_endpoint,
            paths,
            diagnostics,
        })
    }

    /// 只解码应用生成的固定 context 文档；缺失保持 None，损坏记录为诊断且绝不拼默认值。
    async fn read_record_context<T: for<'de> Deserialize<'de>>(
        &self,
        session: &AgentSession,
        kind: AgentWorkspaceDocumentKind,
        cancellation: &CancellationToken,
        diagnostics: &mut Vec<AgentSessionRecordDiagnostic>,
        diagnostic_code: &'static str,
        diagnostic_message: &'static str,
    ) -> AppResult<Option<T>> {
        let snapshot = self
            .await_cancellable(cancellation, |token| {
                self.workspace
                    .read_document(session.agent_session_id.clone(), kind, token)
            })
            .await?;
        let Some(contents) = snapshot.contents else {
            return Ok(None);
        };
        let value = match serde_json::from_str::<serde_json::Value>(&contents) {
            Ok(value) => value,
            Err(error) => {
                tracing::debug!(%error, ?kind, "Agent record context cannot be decoded");
                diagnostics.push(record_diagnostic(
                    Some(context_path(session, kind)),
                    diagnostic_code,
                    diagnostic_message,
                ));
                return Ok(None);
            }
        };
        if value.as_object().is_some_and(|object| {
            object.contains_key("schema_version") || object.contains_key("agent_session_id")
        }) {
            diagnostics.push(record_diagnostic(
                Some(context_path(session, kind)),
                diagnostic_code,
                "Agent context uses a legacy header; run the one-time data migration",
            ));
            return Ok(None);
        }
        let schema_version = value
            .get("schemaVersion")
            .and_then(serde_json::Value::as_u64);
        let document_session_id = value
            .get("agentSessionId")
            .and_then(serde_json::Value::as_str);
        if schema_version != Some(1)
            || document_session_id != Some(session.agent_session_id.as_str())
        {
            diagnostics.push(record_diagnostic(
                Some(context_path(session, kind)),
                diagnostic_code,
                "Agent context does not match the current session schema or identity",
            ));
            return Ok(None);
        }
        match serde_json::from_value::<T>(value) {
            Ok(document) => Ok(Some(document)),
            Err(error) => {
                tracing::debug!(%error, ?kind, "Agent record context cannot be decoded");
                diagnostics.push(record_diagnostic(
                    Some(context_path(session, kind)),
                    diagnostic_code,
                    diagnostic_message,
                ));
                Ok(None)
            }
        }
    }

    /// 只从统一 SessionService 列表中匹配 Agent owner，不维护重复的 terminal ID 缓存。
    async fn find_agent_terminal(&self, id: &AgentSessionId) -> AppResult<Option<SessionInfo>> {
        let live = self.sessions.list(SessionFilter::default()).await?;
        Ok(find_agent_terminal(&live, id))
    }

    /// 创建前验证 Ready binding 确实指向正在运行的 UI terminal。
    pub(super) async fn validate_target_binding(
        &self,
        binding: &AgentTargetBinding,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        binding.validate().map_err(domain_error)?;
        if binding.live_status != AgentTargetLiveStatus::Ready {
            return Ok(());
        }
        let id = binding
            .terminal_session_id
            .as_ref()
            .ok_or_else(|| AppError::invalid("ready Agent target requires a session id"))?;
        let live = self.sessions.list(SessionFilter::default()).await?;
        self.ensure_request_active(cancellation, "Agent target binding was cancelled")?;
        let target = live
            .into_iter()
            .find(|session| &session.session_id == id)
            .ok_or_else(|| AppError::not_found("Agent preferred target not found"))?;
        if target.lifecycle != SessionLifecycle::Running
            || !matches!(target.owner, SessionOwner::Ui { .. })
        {
            return Err(AppError::conflict(
                "Agent preferred target is not a running UI terminal",
            ));
        }
        Ok(())
    }

    /// 用精确 SessionId 解析 target 状态；旧绑定只补稳定身份并经原 revision CAS 写回。
    async fn resolve_target_inner(
        &self,
        session: &mut AgentSession,
        session_revision: &mut AgentWorkspaceRevision,
        cancellation: &CancellationToken,
    ) -> AppResult<(AgentTargetResolution, Option<SessionInfo>)> {
        self.ensure_request_active(cancellation, "Agent target resolution was cancelled")?;
        let binding_identity_changed = session
            .target
            .as_mut()
            .is_some_and(|binding| ensure_target_binding_id(binding, None));
        if binding_identity_changed {
            *session_revision = self
                .persist_session_only(session, session_revision.clone(), cancellation)
                .await?;
        }
        let observed_at_ms = current_epoch_millis()?;
        let binding = session
            .target
            .clone()
            .unwrap_or_else(|| AgentTargetBinding::unbound(0));
        let Some(requested_id) = binding.terminal_session_id.clone() else {
            return Ok((
                AgentTargetResolution {
                    agent_session_id: session.agent_session_id.clone(),
                    binding: binding.clone(),
                    resolved_session_id: None,
                    status: binding.live_status,
                    observed_at_ms,
                },
                None,
            ));
        };
        if binding.live_status != AgentTargetLiveStatus::Ready {
            return Ok((
                AgentTargetResolution {
                    agent_session_id: session.agent_session_id.clone(),
                    binding: binding.clone(),
                    resolved_session_id: None,
                    status: binding.live_status,
                    observed_at_ms,
                },
                None,
            ));
        }

        let live = self.sessions.list(SessionFilter::default()).await?;
        let found = live
            .into_iter()
            .find(|terminal| terminal.session_id == requested_id);
        let (status, resolved) = match found {
            Some(terminal)
                if terminal.lifecycle == SessionLifecycle::Running
                    && matches!(terminal.owner, SessionOwner::Ui { .. }) =>
            {
                (AgentTargetLiveStatus::Ready, Some(terminal))
            }
            Some(terminal)
                if matches!(
                    terminal.lifecycle,
                    SessionLifecycle::Exited | SessionLifecycle::Closed | SessionLifecycle::Failed
                ) =>
            {
                (AgentTargetLiveStatus::Closed, None)
            }
            _ => (AgentTargetLiveStatus::Stale, None),
        };
        if status != AgentTargetLiveStatus::Ready
            && binding.live_status == AgentTargetLiveStatus::Ready
        {
            session
                .mark_target_stale(status, current_timestamp())
                .map_err(domain_error)?;
            *session_revision = self
                .persist_session_only(session, session_revision.clone(), cancellation)
                .await?;
        }
        let next_binding = session.target.clone().unwrap_or(binding);
        Ok((
            AgentTargetResolution {
                agent_session_id: session.agent_session_id.clone(),
                binding: next_binding,
                resolved_session_id: resolved
                    .as_ref()
                    .map(|terminal| terminal.session_id.clone()),
                status,
                observed_at_ms,
            },
            resolved,
        ))
    }

    /// 由 engine 的受限文本快照接口读取并脱敏，不从渲染数据推导文本。
    async fn capture_target_snapshot(
        &self,
        target: Option<&SessionInfo>,
    ) -> AppResult<(Option<PlainTextSnapshot>, bool)> {
        let (snapshot, redacted, _) = self
            .capture_target_snapshot_with_limit(target, DEFAULT_SNAPSHOT_BYTES)
            .await?;
        Ok((snapshot, redacted))
    }

    /// 按字节上限读取同一目标的纯文本，敏感替换后再次裁剪，准确报告截断状态。
    async fn capture_target_snapshot_with_limit(
        &self,
        target: Option<&SessionInfo>,
        max_bytes: usize,
    ) -> AppResult<(Option<PlainTextSnapshot>, bool, usize)> {
        let Some(target) = target else {
            return Ok((None, false, 0));
        };
        let mut snapshot = self
            .sessions
            .plaintext_snapshot(
                target.session_id.clone(),
                PlainTextSnapshotRequest {
                    include_scrollback: true,
                    max_lines: 2000,
                    max_bytes: Some(max_bytes),
                },
            )
            .await?;
        let captured_bytes = snapshot.text.len();
        let (safe, redacted) = redact_terminal_text(&snapshot.text);
        let (text, truncated_by_redaction) = truncate_utf8(&safe, max_bytes);
        if truncated_by_redaction {
            snapshot.line_count = text.lines().count();
        }
        snapshot.text = text;
        snapshot.truncated |= truncated_by_redaction;
        Ok((Some(snapshot), redacted, captured_bytes))
    }

    /// durable marker 优先于 RAM 与 terminal 列表；缺少精确恢复材料时只展示 Unknown 并冻结写入。
    ///
    /// @author kongweiguang
    pub(super) async fn resume_recovery_for_view(
        &self,
        session: &AgentSession,
        terminal: Option<&SessionInfo>,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<AgentResumeRecovery>> {
        if let Some(checkpoint) = self
            .read_resume_checkpoint(&session.agent_session_id, cancellation)
            .await?
        {
            return Ok(Some(AgentResumeRecovery {
                terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                status: AgentResumeRecoveryStatus::Unknown,
            }));
        }
        let pending = self.pending_resume(&session.agent_session_id).await;
        if let Some(pending) = pending {
            self.set_pending_resume_status(
                &session.agent_session_id,
                AgentResumeRecoveryStatus::Unknown,
            )
            .await;
            return Ok(Some(AgentResumeRecovery {
                terminal_session_id: pending.recovery.terminal_session_id,
                status: AgentResumeRecoveryStatus::Unknown,
            }));
        }
        Ok(terminal
            .filter(|terminal| {
                terminal_has_live_owner(terminal) && session.status != AgentSessionStatus::Active
            })
            .map(|terminal| AgentResumeRecovery {
                terminal_session_id: terminal.session_id.clone(),
                status: AgentResumeRecoveryStatus::Unknown,
            }))
    }

    /// 读取恢复能力失败只降级该字段；调用方取消或应用关闭仍按原查询语义返回错误。
    ///
    /// @author kongweiguang
    pub(super) async fn read_resume_capability(
        &self,
        session: &AgentSession,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentResumeCapability> {
        self.ensure_request_active(
            cancellation,
            "Agent resume capability projection was cancelled",
        )?;
        let provider = self
            .await_cancellable(cancellation, |token| {
                self.workspace
                    .read_provider_session(session.agent_session_id.clone(), token)
            })
            .await
            .map(|provider| provider.status);
        let capability = project_resume_capability_result(session, provider)?;
        self.ensure_request_active(
            cancellation,
            "Agent resume capability projection was cancelled",
        )?;
        Ok(capability)
    }

    /// 共享恢复判定后再允许改写 Agent 元数据，避免与原 session/provider 事务竞争。
    ///
    /// @author kongweiguang
    pub(super) async fn ensure_resume_recovery_resolved_before_mutation(
        &self,
        session: &AgentSession,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        let live = self.sessions.list(SessionFilter::default()).await?;
        let terminal = find_agent_terminal(&live, &session.agent_session_id);
        if self
            .resume_recovery_for_view(session, terminal.as_ref(), cancellation)
            .await?
            .is_some()
        {
            return Err(AppError::conflict(
                "Agent resume recovery must be reconciled before metadata can change",
            ));
        }
        Ok(())
    }

    /// 优先读取当前 typed checkpoint；仓储错误不能退化为“没有待核对恢复”。
    ///
    /// @author kongweiguang
    async fn read_resume_checkpoint(
        &self,
        id: &AgentSessionId,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
        self.await_cancellable(cancellation, |token| {
            self.workspace.read_resume_checkpoint(id.clone(), token)
        })
        .await
    }

    /// 仅在 checkpoint 的 exact attempt、base CAS、RAM typed 意图和 Running owner 全部吻合时重试原 commit。
    ///
    /// @author kongweiguang
    async fn reconcile_durable_resume_checkpoint(
        &self,
        id: &AgentSessionId,
        checkpoint: AgentResumeCheckpointSnapshot,
        mut pending: Option<PendingAgentResume>,
        current: AgentWorkspaceSessionSnapshot,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let provider = match self
            .await_cancellable(cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await
        {
            Ok(provider) => provider,
            Err(error) if error.public_code() == crate::PublicErrorCode::Cancelled => {
                return Err(error);
            }
            Err(error) => {
                tracing::debug!(%error, "Agent resume checkpoint provider read needs another check");
                self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                return Ok(AgentSessionView {
                    resume_capability: AgentResumeCapability::MetadataUnavailable,
                    session: current.session,
                    terminal: None,
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                });
            }
        };
        let live = match self.sessions.list(SessionFilter::default()).await {
            Ok(live) => live,
            Err(error) => {
                tracing::debug!(%error, "Agent resume checkpoint owner read needs another check");
                self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                return Ok(AgentSessionView {
                    resume_capability: project_resume_capability(
                        &current.session,
                        &provider.status,
                    ),
                    session: current.session,
                    terminal: None,
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                });
            }
        };
        let terminal = find_agent_terminal(&live, id);
        let exact_owner = live.iter().find(|terminal| {
            terminal.session_id == checkpoint.checkpoint.terminal_session_id
                && terminal.owner
                    == (SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    })
        });
        let owner_running =
            exact_owner.is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running);
        let attempt_matches = pending.as_ref().is_some_and(|pending| {
            pending.checkpoint == checkpoint
                && checkpoint.checkpoint.phase == AgentResumeCheckpointPhase::Prepared
                && pending.create_admission.matches_checkpoint(id, &checkpoint)
                && pending.create_admission.state() == CreateAdmission::MayHaveStarted
        });

        // 只有本进程能证明 create 从未准入时，才能把“无 owner”解释为尚未启动并清理原事务。
        let not_admitted_attempt = pending.as_ref().is_some_and(|pending| {
            pending.checkpoint == checkpoint
                && checkpoint.checkpoint.phase == AgentResumeCheckpointPhase::Prepared
                && pending.create_admission.matches_checkpoint(id, &checkpoint)
                && pending.create_admission.state() == CreateAdmission::NotAdmitted
        });
        let exact_reserved_session_exists = live
            .iter()
            .any(|item| item.session_id == checkpoint.checkpoint.terminal_session_id);
        let has_live_agent_owner = live.iter().any(|item| {
            item.owner
                == (SessionOwner::Agent {
                    agent_session_id: id.clone(),
                })
                && matches!(
                    item.lifecycle,
                    SessionLifecycle::Starting
                        | SessionLifecycle::Running
                        | SessionLifecycle::Closing
                )
        });
        if not_admitted_attempt
            && current.revision == checkpoint.checkpoint.base_session_revision
            && provider.revision == checkpoint.checkpoint.base_provider_revision
            && !exact_reserved_session_exists
            && !has_live_agent_owner
            && self
                .clear_resume_checkpoint_confirmed(id, &checkpoint, cancellation)
                .await
        {
            self.clear_pending_resume(id).await;
            return Ok(AgentSessionView {
                resume_capability: project_resume_capability(&current.session, &provider.status),
                session: current.session,
                terminal,
                resume_recovery: None,
            });
        }

        if checkpoint.checkpoint.phase == AgentResumeCheckpointPhase::Prepared
            && attempt_matches
            && owner_running
            && current.revision == checkpoint.checkpoint.base_session_revision
            && provider.revision == checkpoint.checkpoint.base_provider_revision
        {
            let pending = pending
                .as_mut()
                .expect("matching attempt requires its original typed resume intent");
            let committed = self
                .await_cancellable(cancellation, |token| {
                    self.workspace.commit_resume_checkpoint(
                        CommitAgentResumeCheckpointRequest {
                            agent_session_id: id.clone(),
                            prepared: checkpoint.clone(),
                            intended_session: pending.resumed_session.clone(),
                            intended_provider: pending.resumed_provider.clone(),
                        },
                        token,
                    )
                })
                .await;
            match committed {
                Ok(committed)
                    if committed.checkpoint.attempt_id == checkpoint.checkpoint.attempt_id
                        && committed.checkpoint.terminal_session_id
                            == checkpoint.checkpoint.terminal_session_id
                        && committed.checkpoint.base_session_revision
                            == checkpoint.checkpoint.base_session_revision
                        && committed.checkpoint.base_provider_revision
                            == checkpoint.checkpoint.base_provider_revision
                        && committed.checkpoint.intended_session_revision
                            == checkpoint.checkpoint.intended_session_revision
                        && committed.checkpoint.intended_provider_revision
                            == checkpoint.checkpoint.intended_provider_revision
                        && committed.checkpoint.phase == AgentResumeCheckpointPhase::Committed =>
                {
                    pending.checkpoint = committed.clone();
                    self.remember_pending_resume(id, pending.clone()).await;
                    return self
                        .confirm_committed_resume_checkpoint(
                            id,
                            committed,
                            Some(pending),
                            &current.session,
                            &provider.status,
                            cancellation,
                        )
                        .await;
                }
                Ok(_) => {}
                Err(error) => {
                    let status = recovery_status_for_error(&error);
                    self.set_pending_resume_status(id, status).await;
                    return Ok(AgentSessionView {
                        resume_capability: project_resume_capability(
                            &current.session,
                            &provider.status,
                        ),
                        session: current.session,
                        terminal,
                        resume_recovery: Some(AgentResumeRecovery {
                            terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                            status,
                        }),
                    });
                }
            }
        }

        if checkpoint.checkpoint.phase == AgentResumeCheckpointPhase::Committed {
            return self
                .confirm_committed_resume_checkpoint(
                    id,
                    checkpoint,
                    pending.as_ref(),
                    &current.session,
                    &provider.status,
                    cancellation,
                )
                .await;
        }

        self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
            .await;
        Ok(AgentSessionView {
            resume_capability: project_resume_capability(&current.session, &provider.status),
            session: current.session,
            terminal,
            resume_recovery: Some(AgentResumeRecovery {
                terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                status: AgentResumeRecoveryStatus::Unknown,
            }),
        })
    }

    /// 提交后按 checkpoint hash 和 exact Running owner 核对；Unknown 不会因进程缺席而被清除。
    ///
    /// @author kongweiguang
    async fn confirm_committed_resume_checkpoint(
        &self,
        id: &AgentSessionId,
        checkpoint: AgentResumeCheckpointSnapshot,
        pending: Option<&PendingAgentResume>,
        fallback_session: &AgentSession,
        fallback_provider: &AgentProviderSessionStatus,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentSessionView> {
        let current = match self.read_session(id.clone(), cancellation).await {
            Ok(current) => current,
            Err(error) => {
                tracing::debug!(%error, "Agent resume session hash readback is unknown");
                self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                let terminal = self
                    .sessions
                    .list(SessionFilter::default())
                    .await
                    .ok()
                    .and_then(|live| find_agent_terminal(&live, id));
                return Ok(AgentSessionView {
                    session: fallback_session.clone(),
                    terminal,
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                    resume_capability: project_resume_capability(
                        fallback_session,
                        fallback_provider,
                    ),
                });
            }
        };
        let provider = match self
            .await_cancellable(cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await
        {
            Ok(provider) => provider,
            Err(error) => {
                tracing::debug!(%error, "Agent resume provider hash readback is unknown");
                self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                return Ok(AgentSessionView {
                    session: current.session,
                    terminal: None,
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                    resume_capability: AgentResumeCapability::MetadataUnavailable,
                });
            }
        };
        let live = match self.sessions.list(SessionFilter::default()).await {
            Ok(live) => live,
            Err(error) => {
                tracing::debug!(%error, "Agent resume exact owner readback is unknown");
                self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
                    .await;
                return Ok(AgentSessionView {
                    resume_capability: project_resume_capability(
                        &current.session,
                        &provider.status,
                    ),
                    session: current.session,
                    terminal: None,
                    resume_recovery: Some(AgentResumeRecovery {
                        terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                        status: AgentResumeRecoveryStatus::Unknown,
                    }),
                });
            }
        };
        let terminal = find_agent_terminal(&live, id);
        let exact_owner_running = live.iter().any(|terminal| {
            terminal.session_id == checkpoint.checkpoint.terminal_session_id
                && terminal.owner
                    == (SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    })
                && terminal.lifecycle == SessionLifecycle::Running
        });
        let intended_revisions_match = current.revision
            == checkpoint.checkpoint.intended_session_revision
            && provider.revision == checkpoint.checkpoint.intended_provider_revision;
        let intended_objects_match = pending.is_none_or(|pending| {
            current.session == pending.resumed_session
                && matches!(
                    &provider.status,
                    AgentProviderSessionStatus::Available(actual)
                        if actual == &pending.resumed_provider
                )
        });
        if checkpoint.checkpoint.phase == AgentResumeCheckpointPhase::Committed
            && intended_revisions_match
            && intended_objects_match
            && exact_owner_running
            && self
                .clear_resume_checkpoint_confirmed(id, &checkpoint, cancellation)
                .await
        {
            self.clear_pending_resume(id).await;
            return Ok(AgentSessionView {
                resume_capability: project_resume_capability(&current.session, &provider.status),
                session: current.session,
                terminal,
                resume_recovery: None,
            });
        }

        self.set_pending_resume_status(id, AgentResumeRecoveryStatus::Unknown)
            .await;
        Ok(AgentSessionView {
            resume_capability: project_resume_capability(&current.session, &provider.status),
            session: current.session,
            terminal,
            resume_recovery: Some(AgentResumeRecovery {
                terminal_session_id: checkpoint.checkpoint.terminal_session_id,
                status: AgentResumeRecoveryStatus::Unknown,
            }),
        })
    }

    /// Clear 必须同时有成功回执和 marker 缺失读回；仅看到缺失不能覆盖 CAS 冲突。
    ///
    /// @author kongweiguang
    async fn clear_resume_checkpoint_confirmed(
        &self,
        id: &AgentSessionId,
        expected: &AgentResumeCheckpointSnapshot,
        cancellation: &CancellationToken,
    ) -> bool {
        let cleared = self
            .await_cancellable(cancellation, |token| {
                self.workspace
                    .clear_resume_checkpoint(id.clone(), expected.clone(), token)
            })
            .await;
        if let Err(error) = cleared {
            tracing::debug!(%error, "Agent resume checkpoint clear was not confirmed");
            return false;
        }
        matches!(
            self.read_resume_checkpoint(id, cancellation).await,
            Ok(None)
        )
    }

    /// Marker 缺失时，committed 核对原意图与 exact Running owner；NotAdmitted 仅凭本进程准入证据收敛。
    ///
    /// @author kongweiguang
    async fn confirm_absent_committed_resume(
        &self,
        id: &AgentSessionId,
        pending: &PendingAgentResume,
        stored: &AgentWorkspaceSessionSnapshot,
        cancellation: &CancellationToken,
    ) -> AppResult<Option<AgentSessionView>> {
        let checkpoint = &pending.checkpoint.checkpoint;
        let prestart_not_admitted = checkpoint.phase == AgentResumeCheckpointPhase::Prepared
            && pending
                .create_admission
                .matches_checkpoint(id, &pending.checkpoint)
            && pending.create_admission.state() == CreateAdmission::NotAdmitted;
        if checkpoint.phase != AgentResumeCheckpointPhase::Committed && !prestart_not_admitted {
            return Ok(None);
        }
        self.ensure_request_active(cancellation, "Agent recovery reconciliation was cancelled")?;
        let provider = match self
            .await_cancellable(cancellation, |token| {
                self.workspace.read_provider_session(id.clone(), token)
            })
            .await
        {
            Ok(provider) => provider,
            Err(error) if error.public_code() == crate::PublicErrorCode::Cancelled => {
                return Err(error);
            }
            Err(error) => {
                tracing::debug!(%error, "Absent Agent resume checkpoint provider verification is unknown");
                return Ok(None);
            }
        };
        let live = match self.sessions.list(SessionFilter::default()).await {
            Ok(live) => live,
            Err(error) if error.public_code() == crate::PublicErrorCode::Cancelled => {
                return Err(error);
            }
            Err(error) => {
                tracing::debug!(%error, "Absent Agent resume checkpoint owner verification is unknown");
                return Ok(None);
            }
        };
        if prestart_not_admitted {
            let exact_reserved_session_exists = live
                .iter()
                .any(|terminal| terminal.session_id == checkpoint.terminal_session_id);
            let has_live_agent_owner = live.iter().any(|terminal| {
                terminal.owner
                    == (SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    })
                    && matches!(
                        terminal.lifecycle,
                        SessionLifecycle::Starting
                            | SessionLifecycle::Running
                            | SessionLifecycle::Closing
                    )
            });
            if stored.revision == checkpoint.base_session_revision
                && provider.revision == checkpoint.base_provider_revision
                && !exact_reserved_session_exists
                && !has_live_agent_owner
            {
                // Marker absence is safe only with this process-local NotAdmitted proof and both untouched bases.
                self.ensure_request_active(
                    cancellation,
                    "Agent recovery reconciliation was cancelled",
                )?;
                self.clear_pending_resume(id).await;
                return Ok(Some(AgentSessionView {
                    resume_capability: project_resume_capability(&stored.session, &provider.status),
                    session: stored.session.clone(),
                    terminal: find_agent_terminal(&live, id),
                    resume_recovery: None,
                }));
            }
            return Ok(None);
        }
        if checkpoint.phase != AgentResumeCheckpointPhase::Committed {
            return Ok(None);
        }
        let checkpoint = &pending.checkpoint.checkpoint;
        let intended_documents_match = stored.revision == checkpoint.intended_session_revision
            && stored.session == pending.resumed_session
            && provider.revision == checkpoint.intended_provider_revision
            && matches!(
                &provider.status,
                AgentProviderSessionStatus::Available(actual)
                    if actual == &pending.resumed_provider
            );
        let exact_owner = live.iter().find(|terminal| {
            terminal.session_id == checkpoint.terminal_session_id
                && terminal.owner
                    == (SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    })
                && terminal.lifecycle == SessionLifecycle::Running
        });
        let Some(exact_owner) = exact_owner else {
            return Ok(None);
        };
        if !intended_documents_match {
            return Ok(None);
        }
        self.ensure_request_active(cancellation, "Agent recovery reconciliation was cancelled")?;
        self.clear_pending_resume(id).await;
        Ok(Some(AgentSessionView {
            resume_capability: project_resume_capability(&stored.session, &provider.status),
            session: stored.session.clone(),
            terminal: Some(exact_owner.clone()),
            resume_recovery: None,
        }))
    }

    /// 读取唯一恢复尝试副本；这个 map 只保存 ACK 后尚未完成状态读回的 owner。
    ///
    /// @author kongweiguang
    async fn pending_resume(&self, id: &AgentSessionId) -> Option<PendingAgentResume> {
        self.pending_resumes.read().await.get(id.as_str()).cloned()
    }

    /// 先保留已 ACK 的 terminal 身份，再尝试写盘，取消和 CAS 错误都不能丢失恢复引用。
    ///
    /// @author kongweiguang
    async fn remember_pending_resume(&self, id: &AgentSessionId, pending: PendingAgentResume) {
        self.pending_resumes
            .write()
            .await
            .insert(id.as_str().to_owned(), pending);
    }

    /// 更新可见恢复分类；CAS 明确冲突仍可重试，读回或提交不明时提升为 Unknown。
    ///
    /// @author kongweiguang
    async fn set_pending_resume_status(
        &self,
        id: &AgentSessionId,
        status: AgentResumeRecoveryStatus,
    ) {
        if let Some(pending) = self.pending_resumes.write().await.get_mut(id.as_str()) {
            pending.recovery.status = status;
        }
    }

    /// 只有 session/provider 与原 terminal 已核对后才释放 pending owner。
    ///
    /// @author kongweiguang
    async fn clear_pending_resume(&self, id: &AgentSessionId) {
        self.pending_resumes.write().await.remove(id.as_str());
    }

    /// 将聚合的原始版本与快照捕获一起提交；无 provider 更新时保留已有恢复元数据。
    ///
    /// @author kongweiguang
    async fn persist_workspace(
        &self,
        session_write: SessionDocumentWrite<'_>,
        endpoint: &str,
        agent_terminal: Option<&SessionInfo>,
        snapshot: WorkspaceSnapshotInput<'_>,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentWorkspaceRevision> {
        self.persist_workspace_with_provider(
            session_write,
            None,
            endpoint,
            agent_terminal,
            snapshot,
            cancellation,
        )
        .await
    }

    /// Session/provider 保持同批 CAS；组合阶段借用正文，提交后仍等待实际回执并返回新版本。
    ///
    /// @author kongweiguang
    async fn persist_workspace_with_provider(
        &self,
        session_write: SessionDocumentWrite<'_>,
        provider_update: Option<ProviderDocumentWrite<'_>>,
        endpoint: &str,
        agent_terminal: Option<&SessionInfo>,
        snapshot: WorkspaceSnapshotInput<'_>,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentWorkspaceRevision> {
        let agent_session_id = session_write.session.agent_session_id.clone();
        let documents = self
            .await_cancellable(cancellation, |token| {
                compose_workspace_documents(
                    self.workspace.as_ref(),
                    session_write,
                    provider_update,
                    endpoint,
                    agent_terminal,
                    snapshot,
                    token,
                )
            })
            .await?;
        self.ensure_request_active(cancellation, "Agent workspace update was cancelled")?;
        let receipts = self
            .await_cancellable(cancellation, |token| {
                self.workspace
                    .write_documents(agent_session_id, documents, token)
            })
            .await?;
        receipts
            .into_iter()
            .find(|receipt| receipt.kind == AgentWorkspaceDocumentKind::Session)
            .map(|receipt| receipt.revision)
            .ok_or_else(|| {
                AppError::infrastructure("Agent workspace commit omitted session revision")
            })
    }

    /// 持久化仅发生状态变化的聚合，目标失效不重写启动快照或任意用户文档。
    pub(super) async fn persist_session_only(
        &self,
        session: &AgentSession,
        expected_revision: AgentWorkspaceRevision,
        cancellation: &CancellationToken,
    ) -> AppResult<AgentWorkspaceRevision> {
        let document =
            super::workspace_documents::serialize_session_document(session, expected_revision);
        let receipts = self
            .await_cancellable(cancellation, |token| {
                self.workspace.write_documents(
                    session.agent_session_id.clone(),
                    vec![document],
                    token,
                )
            })
            .await?;
        receipts
            .into_iter()
            .find(|receipt| receipt.kind == AgentWorkspaceDocumentKind::Session)
            .map(|receipt| receipt.revision)
            .ok_or_else(|| {
                AppError::infrastructure("Agent workspace commit omitted session revision")
            })
    }

    /// 按冻结 launch 启动后台 PTY；resume 先核对 caller token，再以 attempt 原子标记紧邻 create 的副作用边界。
    ///
    /// @author kongweiguang
    async fn start_agent_terminal(
        &self,
        session: &AgentSession,
        launch: &kerminal_domain::agents::AgentSessionLaunch,
        endpoint: &str,
        preallocated_session_id: Option<kerminal_domain::identity::SessionId>,
        resume_admission: Option<ResumeCreateAdmission>,
        cancellation: &CancellationToken,
    ) -> AppResult<SessionInfo> {
        self.ensure_request_active(cancellation, "Agent CLI launch was cancelled")?;
        let mut environment = std::collections::BTreeMap::new();
        environment.insert(
            OsString::from("KERMINAL_AGENT_SESSION_ID"),
            OsString::from(session.agent_session_id.as_str()),
        );
        environment.insert(
            OsString::from("KERMINAL_MCP_ENDPOINT"),
            OsString::from(endpoint),
        );
        environment.insert(
            OsString::from("KERMINAL_WORKSPACE_ROOT"),
            OsString::from(&session.workspace_root),
        );
        environment.insert(
            OsString::from("KERMINAL_AGENT_SESSION_ROOT"),
            OsString::from(&session.session_root),
        );
        let request = CreateSessionRequest {
            target: SessionTarget::Local(LocalSessionTarget {
                program: OsString::from(&launch.shell),
                args: launch.args.iter().map(OsString::from).collect(),
                working_directory: Some(launch.cwd.clone().into()),
                environment,
            }),
            owner: SessionOwner::Agent {
                agent_session_id: session.agent_session_id.clone(),
            },
            size: TerminalSize::default(),
        };
        self.await_cancellable(cancellation, |token| async move {
            // The executor token is fresh, so mirror any already-set caller/shutdown state before
            // passing it to a synchronous provider implementation.
            if cancellation.is_cancelled() || self.shutdown.is_cancelled() {
                token.cancel();
            }
            if token.is_cancelled() {
                return Err(AppError::cancelled("Agent CLI launch was cancelled"));
            }
            match (preallocated_session_id, resume_admission) {
                (Some(session_id), Some(admission)) => {
                    if !admission.mark_may_have_started(&session.agent_session_id, &session_id) {
                        return Err(AppError::conflict(
                            "Agent resume create admission does not match its reserved owner",
                        ));
                    }
                    self.sessions
                        .create_with_session_id(request, session_id, token)
                        .await
                }
                (None, None) => self.sessions.create_cancellable(request, token).await,
                _ => Err(AppError::infrastructure(
                    "Agent resume create admission lost its reserved session ID",
                )),
            }
        })
        .await
    }

    /// 根据当前真实 SessionInfo 持久化 Active→Stale 转换；无正在运行进程时不伪装 Active。
    pub(super) async fn reconcile_cli_state(
        &self,
        snapshot: &mut AgentWorkspaceSessionSnapshot,
        terminal: Option<&SessionInfo>,
        cancellation: &CancellationToken,
    ) -> AppResult<()> {
        if snapshot.session.status == AgentSessionStatus::Active
            && !terminal.is_some_and(|terminal| terminal.lifecycle == SessionLifecycle::Running)
        {
            snapshot.session.mark_cli_stale(current_timestamp());
            snapshot.revision = self
                .persist_session_only(&snapshot.session, snapshot.revision.clone(), cancellation)
                .await?;
        }
        Ok(())
    }

    /// 返回 endpoint 并拒绝 server 尚未启动或应用已关闭时的 workspace 初始化。
    pub(super) async fn endpoint(&self) -> AppResult<String> {
        self.ensure_accepting()?;
        self.endpoint
            .read()
            .await
            .clone()
            .ok_or_else(|| AppError::conflict("Agent MCP endpoint is not ready"))
    }

    /// 端口操作通过一个子令牌同时接受调用方取消和应用关闭通知；取消后仍 await 清理结果。
    pub(super) async fn await_cancellable<'a, T, F, Fut>(
        &'a self,
        request: &'a CancellationToken,
        operation: F,
    ) -> AppResult<T>
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = AppResult<T>> + Send + 'a,
        T: Send + 'a,
    {
        let token = CancellationToken::new();
        let mut future = Box::pin(operation(token.clone()));
        tokio::select! {
            biased;
            result = &mut future => result,
            _ = request.cancelled() => {
                token.cancel();
                future.await
            }
            _ = self.shutdown.cancelled() => {
                token.cancel();
                future.await
            }
        }
    }

    /// 在所有状态变更入口拒绝关闭中的新业务操作。
    fn ensure_accepting(&self) -> AppResult<()> {
        if self.shutting_down.load(Ordering::Acquire) {
            Err(AppError::cancelled("Agent runtime is shutting down"))
        } else {
            Ok(())
        }
    }

    /// 等待共享 operation gate 时响应 caller 或应用关闭；持有 gate 后提交不由 select 丢弃。
    pub(super) async fn acquire_operation<'a>(
        &'a self,
        cancellation: &CancellationToken,
        cancelled_message: &'static str,
    ) -> AppResult<MutexGuard<'a, ()>> {
        self.ensure_accepting()?;
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AppError::cancelled(cancelled_message)),
            _ = self.shutdown.cancelled() => Err(AppError::cancelled("Agent runtime is shutting down")),
            guard = self.operation_gate.lock() => Ok(guard),
        }
    }

    /// 在不可逆文件提交或 terminal close 前检查取消状态，避免返回陈旧成功。
    pub(super) fn ensure_request_active(
        &self,
        cancellation: &CancellationToken,
        cancelled_message: &'static str,
    ) -> AppResult<()> {
        self.ensure_accepting()?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled(cancelled_message));
        }
        Ok(())
    }
}

#[async_trait]
impl AgentService for AgentSessionService {
    /// 由 MCP controller 在 workspace 更新前调用，保证 create/resume 只见到已同步的 listener 地址。
    async fn configure_mcp_endpoint(
        &self,
        endpoint: Option<String>,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        AgentSessionService::configure_mcp_endpoint(self, endpoint, cancellation).await
    }

    /// 通过共享设置用例读取真实 Launcher 目录，不探测或推断 CLI 安装状态。
    ///
    /// @author kongweiguang
    async fn launcher_catalog(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        self.ensure_request_active(&cancellation, "Agent launcher catalog was cancelled")?;
        self.launch_resolver.launcher_catalog(&cancellation).await
    }

    /// 与新建共用操作门，串行化设置选择和启动解析；CAS 冲突仍由 UI 显式重试。
    ///
    /// @author kongweiguang
    async fn select_launcher(
        &self,
        key: AgentLauncherKey,
        cancellation: CancellationToken,
    ) -> AppResult<AgentLauncherCatalog> {
        let _gate = self
            .acquire_operation(&cancellation, "Agent launcher selection was cancelled")
            .await?;
        self.ensure_request_active(&cancellation, "Agent launcher selection was cancelled")?;
        self.launch_resolver
            .select_launcher(key, &cancellation)
            .await
    }

    /// 将 UI 与 MCP 的用户意图请求交给应用用例，由用例解析当前设置并固化启动快照。
    async fn create(
        &self,
        request: CreateAgentSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        self.create_selected(request, cancellation).await
    }

    /// 列出持久 Agent 会话，并从唯一 SessionService 关联真实 CLI 运行态。
    async fn list(&self, cancellation: CancellationToken) -> AppResult<Vec<AgentSessionView>> {
        AgentSessionService::list(self, cancellation).await
    }

    /// 将历史过滤和计数留在 application，UI 不复制聚合归属判定。
    async fn history(
        &self,
        request: AgentHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentHistoryProjection> {
        AgentSessionService::history(self, request, cancellation).await
    }

    /// 从可信历史 launch snapshot 建立新身份，provider 内置项按当前有效启动器新建。
    async fn create_from_history(
        &self,
        request: CreateAgentSessionFromHistoryRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::create_from_history(self, request, cancellation).await
    }

    /// CAS 改名并回传包含真实 terminal 状态的 Agent view。
    async fn rename_title(
        &self,
        request: RenameAgentSessionTitleRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::rename_title(self, request, cancellation).await
    }

    /// 读取一个稳定 AgentSessionId，未找到时保留 application 的 not-found 语义。
    async fn get(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::get(self, agent_session_id, cancellation).await
    }

    /// 为跨边界 Agent 查询提供经过校验且排除环境变量秘密的记录视图。
    async fn record_projection(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionRecordProjection> {
        AgentSessionService::record_projection(self, agent_session_id, cancellation).await
    }

    /// 显式恢复 session.toml 中持久化的 launch 快照，不重新读取当前 launcher 设置。
    async fn resume(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::resume(self, agent_session_id, cancellation).await
    }

    /// 将只核对请求路由到专用用例，保证 UI 不会把核对误做成第二次 CLI 启动。
    ///
    /// @author kongweiguang
    async fn reconcile_resume(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::reconcile_resume(self, agent_session_id, cancellation).await
    }

    /// 关闭 CLI 并提交归档状态；提交结果未知时保留底层 typed error。
    async fn archive(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::archive(self, agent_session_id, cancellation).await
    }

    /// 只在明确调用时写入首选目标绑定，绝不因 UI Tab 切换自动触发。
    async fn bind_preferred_target(
        &self,
        agent_session_id: AgentSessionId,
        binding: AgentTargetBinding,
        cancellation: CancellationToken,
    ) -> AppResult<AgentSessionView> {
        AgentSessionService::bind_preferred_target(self, agent_session_id, binding, cancellation)
            .await
    }

    /// 返回保存绑定的真实状态；失效 ID 不会由其他 session 代替。
    async fn resolve_preferred_target(
        &self,
        agent_session_id: AgentSessionId,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetResolution> {
        AgentSessionService::resolve_preferred_target(self, agent_session_id, cancellation).await
    }

    /// 读取目标上下文和有界终端快照，重用应用用例的 redaction 与持久化边界。
    async fn target_context(
        &self,
        agent_session_id: AgentSessionId,
        max_snapshot_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTargetContext> {
        AgentSessionService::target_context(
            self,
            agent_session_id,
            max_snapshot_bytes,
            cancellation,
        )
        .await
    }

    /// 通过同一 Agent application service 创建临时脱敏预览，不访问目标终端或落盘。
    ///
    /// @author kongweiguang
    async fn preview_text(
        &self,
        agent_session_id: AgentSessionId,
        text: String,
        max_bytes: usize,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextPreview> {
        AgentSessionService::preview_text(self, agent_session_id, text, max_bytes, cancellation)
            .await
    }

    /// 将确认正文交给唯一 SessionService 原子提交，不把 writer ACK 描述成 Agent 执行成功。
    ///
    /// @author kongweiguang
    async fn send_command_block_text(
        &self,
        request: SendAgentCommandRequest,
        cancellation: CancellationToken,
    ) -> AppResult<AgentTextSubmissionReceipt> {
        AgentSessionService::send_command_block_text(self, request, cancellation).await
    }

    /// 将可信来源的脱敏快照写入本 Agent 的固定 context 文件。
    async fn record_terminal_snapshot(
        &self,
        request: AgentTerminalSnapshotRequest,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        AgentSessionService::record_terminal_snapshot(self, request, cancellation).await
    }
}

/// 只接受 MCP server 真实 loopback HTTP endpoint，禁止写入外部控制地址。
///
/// @author kongweiguang
fn validate_mcp_endpoint(endpoint: &str) -> AppResult<()> {
    let parsed = parse_loopback_mcp_endpoint(endpoint)?;
    if parsed.path() != "/mcp" {
        return Err(AppError::invalid(
            "Agent MCP endpoint must use loopback HTTP /mcp",
        ));
    }
    Ok(())
}

/// 只接受现行全局 MCP 路径，旧 session-scoped 地址由一次性迁移清理。
///
/// @author kongweiguang
fn parse_loopback_mcp_endpoint(endpoint: &str) -> AppResult<url::Url> {
    let parsed = url::Url::parse(endpoint)
        .map_err(|_| AppError::invalid("Agent MCP endpoint is invalid"))?;
    if parsed.scheme() != "http"
        || parsed.host_str() != Some("127.0.0.1")
        || parsed.port().is_none()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(AppError::invalid(
            "Agent MCP endpoint must use loopback HTTP without credentials or query data",
        ));
    }
    Ok(parsed)
}

/// 优先选运行中的 owner，再选仍在启动或关闭的 owner，避免历史记录遮挡清理所有权。
///
/// @author kongweiguang
pub(super) fn find_agent_terminal(
    sessions: &[SessionInfo],
    id: &AgentSessionId,
) -> Option<SessionInfo> {
    sessions
        .iter()
        .find(|session| {
            matches!(
                &session.owner,
                SessionOwner::Agent { agent_session_id } if agent_session_id == id
            ) && session.lifecycle == SessionLifecycle::Running
        })
        .or_else(|| {
            sessions.iter().find(|session| {
                matches!(
                    &session.owner,
                    SessionOwner::Agent { agent_session_id } if agent_session_id == id
                ) && terminal_has_live_owner(session)
            })
        })
        .or_else(|| {
            sessions.iter().find(|session| {
                matches!(
                    &session.owner,
                    SessionOwner::Agent { agent_session_id } if agent_session_id == id
                )
            })
        })
        .cloned()
}

/// Starting/Closing 仍可能拥有进程、管道和清理责任，只有终态允许后续启动新的 CLI。
///
/// @author kongweiguang
fn terminal_has_live_owner(terminal: &SessionInfo) -> bool {
    matches!(
        terminal.lifecycle,
        SessionLifecycle::Starting | SessionLifecycle::Running | SessionLifecycle::Closing
    )
}

/// 给有实际目标的非 Unbound 绑定补稳定身份；同代同目标重放保留当前 ID。
///
/// UUID 只在 application 持久化边界生成，避免 Root/MCP 候选缺 ID 时每次读取都临时造值。
///
/// @author kongweiguang
fn ensure_target_binding_id(
    binding: &mut AgentTargetBinding,
    current: Option<&AgentTargetBinding>,
) -> bool {
    if binding.live_status == AgentTargetLiveStatus::Unbound
        || binding.terminal_session_id.is_none()
    {
        return false;
    }

    if let Some(current_id) = current
        .filter(|current| {
            current.generation == binding.generation && current.points_to_same_session(binding)
        })
        .and_then(|current| current.binding_id.as_deref())
        .filter(|binding_id| !binding_id.trim().is_empty())
    {
        if binding.binding_id.as_deref() != Some(current_id) {
            binding.binding_id = Some(current_id.to_owned());
            return true;
        }
        return false;
    }

    if binding
        .binding_id
        .as_deref()
        .is_some_and(|binding_id| !binding_id.trim().is_empty())
    {
        return false;
    }

    binding.binding_id = Some(Uuid::new_v4().to_string());
    true
}

/// 复用绑定归一化规则，保证首次 session.toml 写入和 resume checkpoint 使用真实稳定 ID。
///
/// @author kongweiguang
fn ensure_session_target_binding_id(session: &mut AgentSession) -> bool {
    session
        .target
        .as_mut()
        .is_some_and(|binding| ensure_target_binding_id(binding, None))
}

/// 仅从匹配且静态合法的 provider 元数据投影恢复能力，不据 provider 名称推断默认值。
///
/// @author kongweiguang
pub(super) fn project_resume_capability(
    session: &AgentSession,
    status: &AgentProviderSessionStatus,
) -> AgentResumeCapability {
    let AgentProviderSessionStatus::Available(provider) = status else {
        return AgentResumeCapability::MetadataUnavailable;
    };
    if provider.provider != session.agent_id || provider.validate().is_err() {
        return AgentResumeCapability::MetadataUnavailable;
    }
    if !provider.resume_supported {
        return AgentResumeCapability::Unsupported;
    }
    if super::launch::validated_resume_command(provider).is_ok() {
        AgentResumeCapability::Supported
    } else {
        AgentResumeCapability::MetadataUnavailable
    }
}

/// 投影的存储故障只隐藏可选能力；取消仍向上返回，避免列表或详情伪报成功。
///
/// @author kongweiguang
pub(super) fn project_resume_capability_result(
    session: &AgentSession,
    status: AppResult<AgentProviderSessionStatus>,
) -> AppResult<AgentResumeCapability> {
    match status {
        Ok(status) => Ok(project_resume_capability(session, &status)),
        Err(error) if error.public_code() == crate::PublicErrorCode::Cancelled => Err(error),
        Err(_) => Ok(AgentResumeCapability::MetadataUnavailable),
    }
}

/// 运行时只接受迁移后的当前 provider 文件；缺失、损坏、未来 schema 和错配都显式失败。
fn provider_session_for_resume(
    status: &AgentProviderSessionStatus,
    expected_provider: AgentProvider,
) -> AppResult<AgentProviderSession> {
    let provider = match status {
        AgentProviderSessionStatus::Available(provider)
            if provider.provider == expected_provider =>
        {
            provider.clone()
        }
        AgentProviderSessionStatus::Available(_) | AgentProviderSessionStatus::ProviderMismatch => {
            return Err(AppError::configuration_corrupt("Agent provider metadata"));
        }
        AgentProviderSessionStatus::Missing => {
            return Err(AppError::configuration_corrupt("Agent provider metadata"));
        }
        AgentProviderSessionStatus::Malformed
        | AgentProviderSessionStatus::UnsupportedSchemaVersion(_) => {
            return Err(AppError::configuration_corrupt("Agent provider metadata"));
        }
    };
    provider.validate().map_err(|error| {
        AppError::configuration_corrupt("Agent provider metadata").with_source(error)
    })?;
    Ok(provider)
}

/// 将确定 CAS 冲突与未知提交结果分开，供 UI/MCP 显示“待确认”或“结果未知”。
///
/// @author kongweiguang
fn recovery_status_for_error(error: &AppError) -> AgentResumeRecoveryStatus {
    if error.public_code() == crate::PublicErrorCode::Conflict {
        AgentResumeRecoveryStatus::Pending
    } else {
        AgentResumeRecoveryStatus::Unknown
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTargetBindingContext {
    schema_version: u32,
    agent_session_id: String,
    #[serde(default)]
    scope: Option<AgentSessionScope>,
    binding: StoredTargetBinding,
    #[serde(default)]
    agent_terminal: Option<StoredAgentTerminalContext>,
    generated_at: String,
}

impl StoredTargetBindingContext {
    /// 只把已通过 schema/session 身份验证的固定文档字段投影给查询层。
    fn into_projection(self) -> AgentTargetBindingContextProjection {
        AgentTargetBindingContextProjection {
            schema_version: self.schema_version,
            agent_session_id: self.agent_session_id,
            scope: self.scope,
            binding: self.binding.into_projection(),
            agent_terminal: self
                .agent_terminal
                .map(StoredAgentTerminalContext::into_projection),
            generated_at: self.generated_at,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredTargetBinding {
    #[serde(default)]
    binding_id: Option<String>,
    generation: u64,
    status: StoredTargetBindingStatus,
    stale: bool,
    #[serde(default)]
    pane_id: Option<String>,
    #[serde(default)]
    tab_id: Option<String>,
    #[serde(default)]
    target_terminal_session_id: Option<String>,
    #[serde(default)]
    target_ref: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    shell: Option<String>,
}

impl StoredTargetBinding {
    /// 显式保留旧 target-binding context 的 JSON 字段语义，不序列化领域绑定对象。
    fn into_projection(self) -> AgentTargetBindingContextBindingProjection {
        AgentTargetBindingContextBindingProjection {
            binding_id: self.binding_id,
            generation: self.generation,
            status: self.status.as_str().to_owned(),
            stale: self.stale,
            pane_id: self.pane_id,
            tab_id: self.tab_id,
            target_terminal_session_id: self.target_terminal_session_id,
            target_ref: self.target_ref,
            cwd: self.cwd,
            shell: self.shell,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum StoredTargetBindingStatus {
    Ready,
    Stale,
    Closed,
    Unbound,
}

impl StoredTargetBindingStatus {
    /// 只接受当前与旧 workspace 共同使用的状态名，拒绝把未知字符串透传到 MCP。
    fn as_str(&self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Stale => "stale",
            Self::Closed => "closed",
            Self::Unbound => "unbound",
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredAgentTerminalContext {
    #[serde(default)]
    session_id: Option<String>,
    status: String,
}

impl StoredAgentTerminalContext {
    /// 暴露应用实际写入的 Agent CLI 运行快照，不从当前 UI 或首选 target 推测。
    fn into_projection(self) -> AgentTerminalContextProjection {
        AgentTerminalContextProjection {
            session_id: self.session_id,
            status: self.status,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredMcpEndpointContext {
    schema_version: u32,
    agent_session_id: String,
    #[serde(default)]
    endpoint: Option<String>,
    transport: String,
    tools_only: bool,
    generated_at: String,
}

impl StoredMcpEndpointContext {
    /// 运行时只读当前全局 endpoint；旧 scoped endpoint 必须先由数据迁移清理。
    ///
    /// @author kongweiguang
    fn validate_for(&self, session: &AgentSession) -> AppResult<()> {
        if self.schema_version != 1
            || self.agent_session_id != session.agent_session_id.as_str()
            || self.transport != "streamable-http"
            || !self.tools_only
        {
            return Err(AppError::configuration_corrupt(
                "Agent MCP endpoint context",
            ));
        }
        if let Some(endpoint) = self.endpoint.as_deref() {
            validate_mcp_endpoint(endpoint)?;
        }
        if self.generated_at.trim().is_empty() {
            return Err(AppError::configuration_corrupt(
                "Agent MCP endpoint context",
            ));
        }
        Ok(())
    }

    /// 从 endpoint 文件读取实际绑定地址，保留 MCP 启动前显式缺失的状态。
    fn into_projection(self) -> AgentMcpEndpointContextProjection {
        AgentMcpEndpointContextProjection {
            schema_version: self.schema_version,
            agent_session_id: self.agent_session_id,
            endpoint: self.endpoint,
            transport: self.transport,
            tools_only: self.tools_only,
            generated_at: self.generated_at,
        }
    }
}

/// 用固定路径构造可恢复的非致命诊断，不把 serde 错误正文或用户文件内容外传。
fn record_diagnostic(
    path: Option<String>,
    code: &str,
    message: &str,
) -> AgentSessionRecordDiagnostic {
    AgentSessionRecordDiagnostic {
        path,
        code: code.to_owned(),
        message: message.to_owned(),
        line: None,
        column: None,
    }
}

/// 仅为受支持的 Agent context 文件生成定位路径，不允许文档内容指定任意路径。
fn context_path(session: &AgentSession, kind: AgentWorkspaceDocumentKind) -> String {
    let name = match kind {
        AgentWorkspaceDocumentKind::McpEndpointContext => "mcp-endpoint.json",
        AgentWorkspaceDocumentKind::TargetBindingContext => "target-binding.json",
        AgentWorkspaceDocumentKind::TerminalSnapshotContext => "terminal-snapshot.json",
        AgentWorkspaceDocumentKind::WorkspaceSnapshotContext => "workspace-snapshot.json",
        _ => "unknown-context.json",
    };
    Path::new(&session.session_root)
        .join("context")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

/// 读取真实 wall-clock Unix 毫秒，避免把系统时钟异常伪装成更新于 epoch 的绑定。
fn current_epoch_millis() -> AppResult<u64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| {
            AppError::infrastructure_with_source("System clock is before Unix epoch", error)
        })?;
    u64::try_from(elapsed.as_millis()).map_err(|error| {
        AppError::infrastructure_with_source("System clock is out of range", error)
    })
}

/// 为缺省标题保留 provider 稳定显示名，不把 shell command 或路径暴露到 UI 标题。
fn default_title(provider: AgentProvider) -> String {
    match provider {
        AgentProvider::Codex => "Codex".to_owned(),
        AgentProvider::Claude => "Claude Code".to_owned(),
        AgentProvider::Pi => "Pi".to_owned(),
        AgentProvider::Custom => "Custom Agent".to_owned(),
    }
}

/// 领域规则错误沿用统一 Invalid 分类，同时避免公开持久化路径或底层错误内容。
///
/// @author kongweiguang
pub(super) fn domain_error(error: AgentDomainError) -> AppError {
    AppError::invalid(match error {
        AgentDomainError::EmptyTitle => "Agent title is required",
        AgentDomainError::EmptyTimestamp => "Agent timestamp is invalid",
        AgentDomainError::EmptyWorkspacePath => "Agent workspace path is required",
        AgentDomainError::InvalidLauncherKey => "Agent launcher key is invalid",
        AgentDomainError::InvalidLaunchSpec(_) => "Agent launch spec is invalid",
        AgentDomainError::UnknownProvider(_) => "Agent provider is unsupported",
        AgentDomainError::StaleBindingGeneration { .. } => {
            "Agent target binding generation is stale"
        }
        AgentDomainError::BindingGenerationReused(_) => {
            "Agent target binding generation was reused"
        }
        AgentDomainError::PreferredTargetUnavailable => "Agent preferred target is unavailable",
        AgentDomainError::InvalidTargetInvalidationStatus => {
            "Agent target invalidation status is invalid"
        }
        AgentDomainError::TargetBindingMissing => "Agent target binding is missing",
        AgentDomainError::UnsupportedSchemaVersion(_) => "Agent session schema is unsupported",
    })
}

/// 为 CLI 添加受管 endpoint 与 workspace 标识时拒绝用户 launch env 覆盖保留 key。
/// 安全截断 UTF-8 文本并报告是否丢弃了超出预算的尾部。
fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_owned(), false);
    }
    let mut boundary = max_bytes;
    while !value.is_char_boundary(boundary) {
        boundary -= 1;
    }
    (value[..boundary].to_owned(), true)
}

/// 限制可视化文本内容只能包含 LF、TAB 和普通字符，避免终端控制序列混入确认正文。
///
/// @author kongweiguang
fn validate_agent_text_controls(text: &str) -> AppResult<()> {
    if text
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\t'))
    {
        return Err(AppError::invalid(
            "Agent text may contain only line feed and tab control characters",
        ));
    }
    Ok(())
}

/// 发送端不再改写用户确认的正文；若中央规则仍发现秘密形态，则要求重新展示脱敏预览。
///
/// @author kongweiguang
fn validate_agent_text_is_reviewed(text: &str) -> AppResult<()> {
    let (_, redacted_again) = redact_terminal_text(text);
    if redacted_again {
        return Err(AppError::invalid(
            "Agent text changed during safety review; preview it again before sending",
        ));
    }
    Ok(())
}

/// 统一以中央脱敏规则生成有界预览；先拒绝控制字符，再按 UTF-8 边界裁剪，绝不静默改写确认文本。
///
/// @author kongweiguang
fn project_agent_text_preview(text: String, max_bytes: usize) -> AppResult<AgentTextPreview> {
    if max_bytes == 0 || max_bytes > MAX_AGENT_TERMINAL_SUBMISSION_BYTES {
        return Err(AppError::invalid(
            "Agent text preview limit must be between 1 and 32768",
        ));
    }
    validate_agent_text_controls(&text)?;
    let source = Zeroizing::new(text);
    let (safe, redacted) = redact_terminal_text(source.as_str());
    let (text, truncated) = truncate_utf8(safe.as_str(), max_bytes);
    Ok(AgentTextPreview {
        text,
        redacted,
        truncated,
    })
}

/// 生成 Unix epoch 秒级时间戳，兼容现有 session.toml 的字符串字段。
pub(super) fn current_timestamp() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_owned())
}

#[cfg(test)]
#[path = "../../tests/unit/agent_resume_service.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/agent_send_text.rs"]
mod send_text_tests;
