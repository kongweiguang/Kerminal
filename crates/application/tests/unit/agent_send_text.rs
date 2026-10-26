//! Agent 文本提交的恢复门禁与 writer 不重放编排测试。
//!
//! @author kongweiguang

use super::*;
use async_trait::async_trait;
use kerminal_domain::{
    agents::{
        AgentLauncherKey, AgentProvider, AgentProviderSession, AgentSession, AgentSessionScope,
        AgentSessionStatus, AgentTargetBinding,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState,
    },
    sessions::{LocalSessionTarget, SessionExitStatus, SessionOwner, SessionTarget, TerminalSize},
};
use std::{
    collections::{BTreeMap, HashMap},
    ffi::OsString,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult, PreferencesService, PublicErrorCode,
    agents::{
        AgentHistoryMode, AgentHistoryRequest, AgentProviderSessionSnapshot,
        AgentProviderSessionStatus, AgentSessionService, AgentWorkspaceDocument,
        AgentWorkspaceDocumentContent, AgentWorkspaceDocumentKind, AgentWorkspaceRepository,
        AgentWorkspaceRevision, AgentWorkspaceSessionSnapshot, AgentWorkspaceWriteReceipt,
        CreateAgentSessionFromHistoryRequest, RenameAgentSessionTitleRequest,
        SendAgentCommandRequest,
    },
    preferences::{
        ConfigurationGuideStatus, ConfigurationRepository, SettingsRepository, SettingsSnapshot,
        VersionedSettings,
    },
    sessions::{
        CommandBlockSnapshot, CreateSessionRequest, PlainTextSnapshot, PlainTextSnapshotRequest,
        RenderSnapshot, RenderSnapshotRequest, ScrollRequest, SessionDeadline, SessionFilter,
        SessionRuntimeMetadata, SessionService, SessionSubscription, ShellCommandOutcome,
        ShellCommandRequest, TerminalCommandContext, TerminalInputEvent, TerminalPalette,
        TerminalSearchRequest, TerminalSearchResult, TerminalSelection, TerminalWriteReceipt,
        UiCommandLineRequest,
    },
};

const TEST_SESSION_GENERATION: u64 = 42;
const TEST_AGENT_ENDPOINT: &str = "http://127.0.0.1:37657/mcp";

#[derive(Clone, Copy)]
enum CommitBehavior {
    Accept,
    AcceptThenReadCancelled,
    ConflictWithoutCommit,
    UnknownAfterSessionCommit,
}

#[derive(Clone, Copy)]
enum PrepareBehavior {
    Accept,
    AcceptThenCancelCaller,
    ErrorWithoutWrite,
    UnknownAfterWrite,
}

#[derive(Clone, Copy)]
enum ClearBehavior {
    Accept,
    AcceptWithUnknownReadback,
}

struct WorkspaceState {
    sessions: BTreeMap<String, AgentWorkspaceSessionSnapshot>,
    providers: BTreeMap<String, AgentProviderSessionSnapshot>,
    mcp_endpoint_documents: BTreeMap<String, crate::agents::AgentWorkspaceDocumentSnapshot>,
    context_documents: HashMap<
        (String, AgentWorkspaceDocumentKind),
        crate::agents::AgentWorkspaceDocumentSnapshot,
    >,
    first_session_binding_ids: BTreeMap<String, Option<String>>,
    resume_checkpoints: BTreeMap<String, AgentResumeCheckpointSnapshot>,
    next_prepare: PrepareBehavior,
    next_commit: CommitBehavior,
    next_clear: ClearBehavior,
    cancel_caller_after_prepare: Option<CancellationToken>,
    fail_next_checkpoint_read: bool,
    cancel_next_session_read: bool,
}

/// 使用受控内存快照模拟一个 Agent workspace 的读取与一次不确定提交。
struct MemoryAgentWorkspace {
    state: Mutex<WorkspaceState>,
}

impl MemoryAgentWorkspace {
    /// 固定读取基线和下一次写入结果，并记录首次 Session/context 以验证 ID 在 CLI 前落盘。
    ///
    /// @author kongweiguang
    fn new(
        session: AgentSession,
        provider: AgentProviderSession,
        next_commit: CommitBehavior,
    ) -> Self {
        let id = session.agent_session_id.as_str().to_owned();
        let mut sessions = BTreeMap::new();
        sessions.insert(
            id.clone(),
            AgentWorkspaceSessionSnapshot {
                session,
                revision: AgentWorkspaceRevision::Sha256("before-session".to_owned()),
            },
        );
        let mut providers = BTreeMap::new();
        providers.insert(
            id,
            AgentProviderSessionSnapshot {
                status: AgentProviderSessionStatus::Available(provider),
                revision: AgentWorkspaceRevision::Sha256("before-provider".to_owned()),
                path: "C:/fixture/agents/session/provider.toml".to_owned(),
            },
        );
        Self {
            state: Mutex::new(WorkspaceState {
                sessions,
                providers,
                mcp_endpoint_documents: BTreeMap::new(),
                context_documents: HashMap::new(),
                first_session_binding_ids: BTreeMap::new(),
                resume_checkpoints: BTreeMap::new(),
                next_prepare: PrepareBehavior::Accept,
                next_commit,
                next_clear: ClearBehavior::Accept,
                cancel_caller_after_prepare: None,
                fail_next_checkpoint_read: false,
                cancel_next_session_read: false,
            }),
        }
    }

    /// 只在内存 fake 中播种 legacy endpoint 正文，确保兼容测试不接触用户配置。
    ///
    /// @author kongweiguang
    fn seed_mcp_endpoint_context(&self, id: &AgentSessionId, contents: String) {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .mcp_endpoint_documents
            .insert(
                id.as_str().to_owned(),
                crate::agents::AgentWorkspaceDocumentSnapshot {
                    contents: Some(contents),
                    revision: AgentWorkspaceRevision::Sha256("legacy-endpoint".to_owned()),
                },
            );
    }

    /// 返回 fake 实际保存的 endpoint 正文，验证 stop 清理写入了 None 终态。
    ///
    /// @author kongweiguang
    fn mcp_endpoint_context(&self, id: &AgentSessionId) -> Option<String> {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .mcp_endpoint_documents
            .get(id.as_str())
            .and_then(|snapshot| snapshot.contents.clone())
    }

    /// Expose exact committed context bytes so legacy repair can be checked across all three files.
    ///
    /// @author kongweiguang
    fn context_document(
        &self,
        id: &AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
    ) -> Option<String> {
        if kind == AgentWorkspaceDocumentKind::McpEndpointContext {
            return self.mcp_endpoint_context(id);
        }
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .context_documents
            .get(&(id.as_str().to_owned(), kind))
            .and_then(|snapshot| snapshot.contents.clone())
    }

    /// Capture the first session commit because a later post-launch refresh could otherwise hide a late ID repair.
    ///
    /// @author kongweiguang
    fn first_session_binding_id(&self, id: &AgentSessionId) -> Option<Option<String>> {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .first_session_binding_ids
            .get(id.as_str())
            .cloned()
    }

    /// 读取外置测试实际写入的会话记录，确认派生创建没有改动来源身份。
    fn stored_session(&self, id: &AgentSessionId) -> AgentSession {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .sessions
            .get(id.as_str())
            .expect("requested Agent fixture should exist")
            .session
            .clone()
    }

    /// 读取真实 provider document 状态，用于证明派生的新会话没有旧 CLI session ID。
    fn stored_provider(&self, id: &AgentSessionId) -> AgentProviderSessionStatus {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .providers
            .get(id.as_str())
            .expect("requested Agent provider fixture should exist")
            .status
            .clone()
    }

    /// 注入一次 checkpoint prepare 结果，覆盖拒绝与写后回执未知两种无 PTY 路径。
    ///
    /// @author kongweiguang
    fn set_next_prepare(&self, behavior: PrepareBehavior) {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .next_prepare = behavior;
    }

    /// 让 prepare 在 durable 写入后取消原 caller，稳定覆盖 ACK 与取消并发的准入竞态。
    ///
    /// @author kongweiguang
    fn cancel_caller_after_next_prepare(&self, cancellation: CancellationToken) {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .cancel_caller_after_prepare = Some(cancellation);
    }

    /// 注入 clear 成功后的一次性读回故障，确认下一轮显式核对能安全结束 committed pending。
    ///
    /// @author kongweiguang
    fn set_next_clear_readback_unknown(&self) {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .next_clear = ClearBehavior::AcceptWithUnknownReadback;
    }

    /// 返回当前 durable marker 副本，用于验证状态未因重启或替代 owner 被清除。
    ///
    /// @author kongweiguang
    fn resume_checkpoint(&self, id: &AgentSessionId) -> Option<AgentResumeCheckpointSnapshot> {
        self.state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .resume_checkpoints
            .get(id.as_str())
            .cloned()
    }

    /// 向隔离 workspace 加入一条额外历史记录，供筛选与派生用例复用真实服务。
    ///
    /// @author kongweiguang
    fn insert_history_session(&self, session: AgentSession, provider: AgentProviderSession) {
        let id = session.agent_session_id.as_str().to_owned();
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        state.sessions.insert(
            id.clone(),
            AgentWorkspaceSessionSnapshot {
                session,
                revision: AgentWorkspaceRevision::Sha256(format!("fixture-{id}")),
            },
        );
        state.providers.insert(
            id.clone(),
            AgentProviderSessionSnapshot {
                status: AgentProviderSessionStatus::Available(provider),
                revision: AgentWorkspaceRevision::Sha256(format!("provider-{id}")),
                path: format!("C:/fixture/.kerminal/agents/sessions/{id}/provider.toml"),
            },
        );
    }
}

#[async_trait]
impl AgentWorkspaceRepository for MemoryAgentWorkspace {
    /// 返回隔离测试路径，避免测试探测或创建正式 `.kerminal` 目录。
    fn workspace_root(&self) -> String {
        "C:/fixture/.kerminal".to_owned()
    }

    /// 新建历史派生用例返回当前平台的绝对假路径，不创建实际目录或触碰用户数据。
    ///
    /// @author kongweiguang
    async fn create_session_root(
        &self,
        agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<String> {
        Ok(std::env::temp_dir()
            .join("kerminal-agent-history-fixture")
            .join(agent_session_id.as_str())
            .to_string_lossy()
            .into_owned())
    }

    /// 本测试保留所有注入的 fixture，不执行 workspace 回滚。
    async fn rollback_created_session_root(
        &self,
        agent_session_id: AgentSessionId,
    ) -> AppResult<()> {
        let key = agent_session_id.as_str().to_owned();
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        state.sessions.remove(&key);
        state.providers.remove(&key);
        Ok(())
    }

    /// 只返回指定身份和 revision；一次性读回取消模拟提交 ACK 之后存储读回被中断。
    ///
    /// @author kongweiguang
    async fn read_session(
        &self,
        agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceSessionSnapshot> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        if std::mem::take(&mut state.cancel_next_session_read) {
            return Err(AppError::cancelled(
                "simulated post-commit readback cancellation",
            ));
        }
        state
            .sessions
            .get(agent_session_id.as_str())
            .cloned()
            .ok_or_else(|| AppError::not_found("Agent fixture session not found"))
    }

    /// endpoint 初始化只需枚举固定的一个持久会话。
    async fn list_sessions(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceSessionSnapshot>> {
        let state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        Ok(state.sessions.values().cloned().collect())
    }

    /// 返回同一隔离仓储保存的 context 正文，避免测试仅断言内存投影而漏掉实际序列化结果。
    ///
    /// @author kongweiguang
    async fn read_document(
        &self,
        agent_session_id: AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        _cancellation: CancellationToken,
    ) -> AppResult<crate::agents::AgentWorkspaceDocumentSnapshot> {
        if kind == AgentWorkspaceDocumentKind::McpEndpointContext {
            let state = self
                .state
                .lock()
                .expect("Agent workspace test mutex should not be poisoned");
            if let Some(snapshot) = state.mcp_endpoint_documents.get(agent_session_id.as_str()) {
                return Ok(snapshot.clone());
            }
        }
        if let Some(snapshot) = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned")
            .context_documents
            .get(&(agent_session_id.as_str().to_owned(), kind))
            .cloned()
        {
            return Ok(snapshot);
        }
        Ok(crate::agents::AgentWorkspaceDocumentSnapshot {
            contents: None,
            revision: AgentWorkspaceRevision::Missing,
        })
    }

    /// 返回 provider 的同一读取快照，确保恢复判定比较 session/provider 成对状态。
    async fn read_provider_session(
        &self,
        agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentProviderSessionSnapshot> {
        let state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        state
            .providers
            .get(agent_session_id.as_str())
            .cloned()
            .ok_or_else(|| AppError::not_found("Agent fixture provider not found"))
    }

    /// 先按 session/provider 双 revision CAS 持久化恢复意图，unknown 回执保留 marker 并禁止 CLI 创建。
    ///
    /// @author kongweiguang
    async fn prepare_resume_checkpoint(
        &self,
        request: crate::agents::PrepareAgentResumeCheckpointRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<crate::agents::AgentResumeCheckpointSnapshot> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        let behavior = std::mem::replace(&mut state.next_prepare, PrepareBehavior::Accept);
        match behavior {
            PrepareBehavior::ErrorWithoutWrite => {
                return Err(AppError::conflict(
                    "simulated resume checkpoint prepare conflict",
                ));
            }
            PrepareBehavior::Accept
            | PrepareBehavior::AcceptThenCancelCaller
            | PrepareBehavior::UnknownAfterWrite => {}
        }
        let key = request.agent_session_id.as_str().to_owned();
        let session = state
            .sessions
            .get(&key)
            .ok_or_else(|| AppError::not_found("Agent fixture session not found"))?;
        let provider = state
            .providers
            .get(&key)
            .ok_or_else(|| AppError::not_found("Agent fixture provider not found"))?;
        if session.revision != request.base_session_revision
            || provider.revision != request.base_provider_revision
            || state.resume_checkpoints.contains_key(&key)
        {
            return Err(AppError::conflict(
                "simulated resume checkpoint base or marker conflict",
            ));
        }

        let attempt_id = request.attempt_id;
        let prepared = crate::agents::AgentResumeCheckpointSnapshot {
            checkpoint: crate::agents::AgentResumeCheckpoint {
                attempt_id,
                terminal_session_id: request.terminal_session_id,
                base_session_revision: request.base_session_revision,
                base_provider_revision: request.base_provider_revision,
                intended_session_revision: AgentWorkspaceRevision::Sha256(
                    "after-AgentWorkspaceDocumentKind::Session".to_owned(),
                ),
                intended_provider_revision: AgentWorkspaceRevision::Sha256(
                    "after-AgentWorkspaceDocumentKind::Provider".to_owned(),
                ),
                phase: crate::agents::AgentResumeCheckpointPhase::Prepared,
            },
            revision: AgentWorkspaceRevision::Sha256(format!("prepared-{attempt_id}")),
        };
        state.resume_checkpoints.insert(key, prepared.clone());
        if matches!(behavior, PrepareBehavior::AcceptThenCancelCaller) {
            state
                .cancel_caller_after_prepare
                .take()
                .expect("cancellation race test should configure its caller token")
                .cancel();
        }
        if matches!(behavior, PrepareBehavior::UnknownAfterWrite) {
            return Err(AppError::outcome_unknown(
                "simulated checkpoint prepare ACK loss",
            ));
        }
        Ok(prepared)
    }

    /// 只将与 durable Prepared snapshot 完全匹配的原始 typed 意图提交一次，并保留部分提交作为 Unknown。
    ///
    /// @author kongweiguang
    async fn commit_resume_checkpoint(
        &self,
        request: crate::agents::CommitAgentResumeCheckpointRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<crate::agents::AgentResumeCheckpointSnapshot> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        let key = request.agent_session_id.as_str().to_owned();
        if state.resume_checkpoints.get(&key) != Some(&request.prepared) {
            return Err(AppError::conflict(
                "simulated resume checkpoint attempt CAS conflict",
            ));
        }
        let session = state
            .sessions
            .get(&key)
            .ok_or_else(|| AppError::not_found("Agent fixture session not found"))?;
        let provider = state
            .providers
            .get(&key)
            .ok_or_else(|| AppError::not_found("Agent fixture provider not found"))?;
        if session.revision != request.prepared.checkpoint.base_session_revision
            || provider.revision != request.prepared.checkpoint.base_provider_revision
        {
            return Err(AppError::conflict(
                "simulated resume checkpoint base revision changed",
            ));
        }

        let behavior = std::mem::replace(&mut state.next_commit, CommitBehavior::Accept);
        if matches!(behavior, CommitBehavior::ConflictWithoutCommit) {
            return Err(AppError::conflict("simulated workspace CAS conflict"));
        }
        let intended_session_revision = request
            .prepared
            .checkpoint
            .intended_session_revision
            .clone();
        let intended_provider_revision = request
            .prepared
            .checkpoint
            .intended_provider_revision
            .clone();
        state
            .sessions
            .get_mut(&key)
            .expect("session was checked above")
            .session = request.intended_session.clone();
        state
            .sessions
            .get_mut(&key)
            .expect("session was checked above")
            .revision = intended_session_revision;

        if matches!(behavior, CommitBehavior::UnknownAfterSessionCommit) {
            return Err(AppError::outcome_unknown(
                "simulated partial resume workspace transaction",
            ));
        }

        let provider_snapshot = state
            .providers
            .get_mut(&key)
            .expect("provider was checked above");
        provider_snapshot.status = AgentProviderSessionStatus::Available(request.intended_provider);
        provider_snapshot.revision = intended_provider_revision;
        let mut committed = request.prepared;
        committed.checkpoint.phase = crate::agents::AgentResumeCheckpointPhase::Committed;
        committed.revision = AgentWorkspaceRevision::Sha256(format!(
            "committed-{}",
            committed.checkpoint.attempt_id
        ));
        state.resume_checkpoints.insert(key, committed.clone());
        if matches!(behavior, CommitBehavior::AcceptThenReadCancelled) {
            state.cancel_next_session_read = true;
        }
        Ok(committed)
    }

    /// 返回 checkpoint 原始 CAS snapshot；hard-restart 用例复用同一仓储来模拟磁盘留存。
    ///
    /// @author kongweiguang
    async fn read_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<crate::agents::AgentResumeCheckpointSnapshot>> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        if std::mem::take(&mut state.fail_next_checkpoint_read) {
            return Err(AppError::outcome_unknown(
                "simulated checkpoint readback interruption",
            ));
        }
        Ok(state
            .resume_checkpoints
            .get(agent_session_id.as_str())
            .cloned())
    }

    /// 对齐真实仓储：marker CAS 外再按 phase 对两份文档 revision 做锁内清理 CAS。
    ///
    /// @author kongweiguang
    async fn clear_resume_checkpoint(
        &self,
        agent_session_id: AgentSessionId,
        expected: crate::agents::AgentResumeCheckpointSnapshot,
        _cancellation: CancellationToken,
    ) -> AppResult<()> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        if state.resume_checkpoints.get(agent_session_id.as_str()) != Some(&expected) {
            return Err(AppError::conflict(
                "simulated resume checkpoint clear CAS conflict",
            ));
        }
        let session = state
            .sessions
            .get(agent_session_id.as_str())
            .ok_or_else(|| AppError::not_found("Agent fixture session not found"))?;
        let provider = state
            .providers
            .get(agent_session_id.as_str())
            .ok_or_else(|| AppError::not_found("Agent fixture provider not found"))?;
        let checkpoint = &expected.checkpoint;
        let (expected_session_revision, expected_provider_revision) = match checkpoint.phase {
            crate::agents::AgentResumeCheckpointPhase::Prepared => (
                &checkpoint.base_session_revision,
                &checkpoint.base_provider_revision,
            ),
            crate::agents::AgentResumeCheckpointPhase::Committed => (
                &checkpoint.intended_session_revision,
                &checkpoint.intended_provider_revision,
            ),
        };
        if &session.revision != expected_session_revision
            || &provider.revision != expected_provider_revision
        {
            return Err(AppError::conflict(
                "simulated resume checkpoint clear document revision CAS conflict",
            ));
        }
        state.resume_checkpoints.remove(agent_session_id.as_str());
        if matches!(state.next_clear, ClearBehavior::AcceptWithUnknownReadback) {
            state.next_clear = ClearBehavior::Accept;
            state.fail_next_checkpoint_read = true;
        }
        Ok(())
    }

    /// 单独 provider 更新不属于本 fixture；恢复只覆盖 session/provider 同批 journal 写入。
    ///
    /// @author kongweiguang
    async fn write_provider_session(
        &self,
        _agent_session_id: AgentSessionId,
        _provider_session: AgentProviderSession,
        _expected_revision: AgentWorkspaceRevision,
        _cancellation: CancellationToken,
    ) -> AppResult<AgentWorkspaceWriteReceipt> {
        unused_port()
    }

    /// 注入一次冲突、部分提交或 ACK 后读回取消，区分未提交与已经越过提交边界的结果。
    ///
    /// @author kongweiguang
    async fn write_documents(
        &self,
        agent_session_id: AgentSessionId,
        documents: Vec<AgentWorkspaceDocument>,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        let mut state = self
            .state
            .lock()
            .expect("Agent workspace test mutex should not be poisoned");
        let behavior = std::mem::replace(&mut state.next_commit, CommitBehavior::Accept);
        match behavior {
            CommitBehavior::ConflictWithoutCommit => {
                Err(AppError::conflict("simulated workspace CAS conflict"))
            }
            CommitBehavior::UnknownAfterSessionCommit => {
                apply_documents(&mut state, agent_session_id.as_str(), &documents, false);
                Err(AppError::outcome_unknown(
                    "simulated partial workspace transaction",
                ))
            }
            CommitBehavior::AcceptThenReadCancelled => {
                let receipts =
                    apply_documents(&mut state, agent_session_id.as_str(), &documents, true);
                state.cancel_next_session_read = true;
                Ok(receipts)
            }
            CommitBehavior::Accept => Ok(apply_documents(
                &mut state,
                agent_session_id.as_str(),
                &documents,
                true,
            )),
        }
    }
}

/// 按文档类型推进隔离快照并留存首次 Session 写入与受管 context，验证稳定身份落盘时序。
///
/// @author kongweiguang
fn apply_documents(
    state: &mut WorkspaceState,
    agent_session_id: &str,
    documents: &[AgentWorkspaceDocument],
    include_provider: bool,
) -> Vec<AgentWorkspaceWriteReceipt> {
    let mut receipts = Vec::with_capacity(documents.len());
    for document in documents {
        let revision = AgentWorkspaceRevision::Sha256(format!("after-{:?}", document.kind));
        match (&document.kind, &document.content) {
            (
                AgentWorkspaceDocumentKind::Session,
                AgentWorkspaceDocumentContent::Session(session),
            ) => {
                state
                    .first_session_binding_ids
                    .entry(agent_session_id.to_owned())
                    .or_insert_with(|| {
                        session
                            .target
                            .as_ref()
                            .and_then(|binding| binding.binding_id.clone())
                    });
                state
                    .sessions
                    .entry(agent_session_id.to_owned())
                    .and_modify(|snapshot| {
                        snapshot.session = session.as_ref().clone();
                        snapshot.revision = revision.clone();
                    })
                    .or_insert_with(|| AgentWorkspaceSessionSnapshot {
                        session: session.as_ref().clone(),
                        revision: revision.clone(),
                    });
            }
            (
                AgentWorkspaceDocumentKind::Provider,
                AgentWorkspaceDocumentContent::Provider(provider),
            ) if include_provider => {
                state
                    .providers
                    .entry(agent_session_id.to_owned())
                    .and_modify(|snapshot| {
                        snapshot.status = AgentProviderSessionStatus::Available(provider.clone());
                        snapshot.revision = revision.clone();
                    })
                    .or_insert_with(|| AgentProviderSessionSnapshot {
                        status: AgentProviderSessionStatus::Available(provider.clone()),
                        revision: revision.clone(),
                        path: format!(
                            "C:/fixture/.kerminal/agents/sessions/{agent_session_id}/provider.toml"
                        ),
                    });
            }
            (
                AgentWorkspaceDocumentKind::McpEndpointContext,
                AgentWorkspaceDocumentContent::Text(contents),
            ) if include_provider => {
                state.mcp_endpoint_documents.insert(
                    agent_session_id.to_owned(),
                    crate::agents::AgentWorkspaceDocumentSnapshot {
                        contents: Some(contents.clone()),
                        revision: revision.clone(),
                    },
                );
            }
            (
                kind @ (AgentWorkspaceDocumentKind::TargetBindingContext
                | AgentWorkspaceDocumentKind::TerminalSnapshotContext),
                AgentWorkspaceDocumentContent::Text(contents),
            ) if include_provider => {
                state.context_documents.insert(
                    (agent_session_id.to_owned(), *kind),
                    crate::agents::AgentWorkspaceDocumentSnapshot {
                        contents: Some(contents.clone()),
                        revision: revision.clone(),
                    },
                );
            }
            _ => {}
        }
        receipts.push(AgentWorkspaceWriteReceipt {
            kind: document.kind,
            revision,
        });
    }
    receipts
}

/// 复用实际记录查询与 endpoint stop-sync，确认当前全局地址可读并在停止时清成 None。
///
/// @author kongweiguang
#[tokio::test]
async fn current_global_endpoint_is_readable_and_stop_sync_clears_it() {
    let agent_session_id = AgentSessionId::new();
    let session = active_agent(agent_session_id.clone(), AgentSessionStatus::Active);
    let endpoint = "http://127.0.0.1:37657/mcp".to_owned();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        session,
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let stored = serde_json::json!({
        "schemaVersion": 1,
        "agentSessionId": agent_session_id.as_str(),
        "endpoint": endpoint.clone(),
        "transport": "streamable-http",
        "toolsOnly": true,
        "generatedAt": "2026-10-19T00:00:00Z"
    });
    workspace.seed_mcp_endpoint_context(
        &agent_session_id,
        serde_json::to_string(&stored).expect("fixture context should serialize"),
    );
    let terminal_session_id = SessionId::new();
    let sessions = Arc::new(MemorySessions::new(
        vec![terminal_info(
            terminal_session_id,
            SessionOwner::Agent {
                agent_session_id: agent_session_id.clone(),
            },
            kerminal_domain::sessions::SessionLifecycle::Running,
        )],
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions);

    let before = service
        .record_projection(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("current global endpoint should be queryable");
    assert_eq!(
        before
            .mcp_endpoint
            .as_ref()
            .and_then(|context| context.endpoint.as_deref()),
        Some(endpoint.as_str())
    );

    service
        .configure_mcp_endpoint(None, CancellationToken::new())
        .await
        .expect("stop sync should clear a valid endpoint owned by this Agent");

    let after = service
        .record_projection(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("cleared endpoint context should remain queryable");
    assert_eq!(
        after
            .mcp_endpoint
            .as_ref()
            .and_then(|context| context.endpoint.as_deref()),
        None
    );
    let saved = workspace
        .mcp_endpoint_context(&agent_session_id)
        .expect("stop sync should commit the endpoint context");
    let saved: serde_json::Value =
        serde_json::from_str(&saved).expect("committed context should be valid JSON");
    assert!(saved["endpoint"].is_null());
}

/// 普通 0.4 reader 拒绝旧 snake_case 身份头并给出迁移诊断，不将其当作当前文档读取。
///
/// @author kongweiguang
#[tokio::test]
async fn record_projection_rejects_legacy_snake_case_context_headers() {
    let agent_session_id = AgentSessionId::new();
    let session = active_agent(agent_session_id.clone(), AgentSessionStatus::Active);
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        session,
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let legacy = serde_json::json!({
        "schema_version": 1,
        "agent_session_id": agent_session_id.as_str(),
        "endpoint": null,
        "transport": "streamable-http",
        "toolsOnly": true,
        "generatedAt": "2026-10-19T00:00:00Z"
    });
    workspace.seed_mcp_endpoint_context(
        &agent_session_id,
        serde_json::to_string(&legacy).expect("synthetic legacy context should serialize"),
    );
    let sessions = Arc::new(MemorySessions::new(
        vec![terminal_info(
            SessionId::new(),
            SessionOwner::Agent {
                agent_session_id: agent_session_id.clone(),
            },
            kerminal_domain::sessions::SessionLifecycle::Running,
        )],
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace, sessions);

    let projection = service
        .record_projection(agent_session_id, CancellationToken::new())
        .await
        .expect("a malformed optional context is reported as a diagnostic");

    assert!(projection.mcp_endpoint.is_none());
    assert!(projection.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "invalidMcpEndpoint"
            && diagnostic.message.contains("one-time data migration")
    }));
}

/// 旧 scoped endpoint 在迁移前仍失败闭合，停止同步不能绕过校验并覆盖原文档。
///
/// @author kongweiguang
#[tokio::test]
async fn stop_sync_preserves_an_unmigrated_scoped_endpoint_for_recovery() {
    let agent_session_id = AgentSessionId::new();
    let other_agent_session_id = AgentSessionId::new();
    let session = active_agent(agent_session_id.clone(), AgentSessionStatus::Active);
    let endpoint = format!("http://127.0.0.1:37657/mcp/agents/{other_agent_session_id}");
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        session,
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let stored = serde_json::json!({
        "schemaVersion": 1,
        "agentSessionId": agent_session_id.as_str(),
        "endpoint": endpoint.clone(),
        "transport": "streamable-http",
        "toolsOnly": true,
        "generatedAt": "2026-10-19T00:00:00Z"
    });
    workspace.seed_mcp_endpoint_context(
        &agent_session_id,
        serde_json::to_string(&stored).expect("fixture context should serialize"),
    );
    let sessions = Arc::new(MemorySessions::new(
        vec![terminal_info(
            SessionId::new(),
            SessionOwner::Agent {
                agent_session_id: agent_session_id.clone(),
            },
            kerminal_domain::sessions::SessionLifecycle::Running,
        )],
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions);

    assert!(
        service
            .configure_mcp_endpoint(None, CancellationToken::new())
            .await
            .is_err()
    );
    let saved = workspace
        .mcp_endpoint_context(&agent_session_id)
        .expect("invalid endpoint context must remain available for repair");
    let saved: serde_json::Value =
        serde_json::from_str(&saved).expect("retained context should remain valid JSON");
    assert_eq!(saved["endpoint"].as_str(), Some(endpoint.as_str()));
}

/// 配置读取在 builtin provider resume 中不参与路径选择，测试若意外访问则失败闭合。
struct UnusedConfigurationRepository;

#[async_trait]
impl SettingsRepository for UnusedConfigurationRepository {
    /// 恢复用例使用 provider.toml 快照，不读取当前 selected launcher 设置。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        Ok(VersionedSettings {
            settings: SettingsSnapshot::default(),
            revision: None,
        })
    }

    /// 本用例只读配置，不允许测试假仓储伪造设置保存成功。
    async fn save_settings(
        &self,
        _settings: SettingsSnapshot,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        unused_port()
    }
}

#[async_trait]
impl ConfigurationRepository for UnusedConfigurationRepository {
    /// 仅满足 PreferencesService 构造约束；此恢复路径不改配置状态。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 本测试不读取文件化设置。
    async fn read(&self, _path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        unused_port()
    }

    /// 本测试不写入正式配置文件。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        unused_port()
    }

    /// 保留默认指南状态，不把设置指南纳入 Agent 恢复测试。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

struct MemorySessions {
    sessions: Mutex<Vec<kerminal_domain::sessions::SessionInfo>>,
    create_count: AtomicUsize,
    created_session_ids: Mutex<Vec<SessionId>>,
    generation: u64,
    writes: AtomicUsize,
    written_text: Mutex<Vec<String>>,
    writer_unknown: bool,
}

impl MemorySessions {
    /// 固定 live owner 和 generation，并记录到唯一 Agent writer 的提交次数。
    ///
    /// @author kongweiguang
    fn new(
        sessions: Vec<kerminal_domain::sessions::SessionInfo>,
        generation: u64,
        writer_unknown: bool,
    ) -> Self {
        Self {
            sessions: Mutex::new(sessions),
            create_count: AtomicUsize::new(0),
            created_session_ids: Mutex::new(Vec::new()),
            generation,
            writes: AtomicUsize::new(0),
            written_text: Mutex::new(Vec::new()),
            writer_unknown,
        }
    }

    /// 返回唯一 Agent writer 调用数，验证拒绝路径从未触达输入边界。
    fn write_count(&self) -> usize {
        self.writes.load(Ordering::SeqCst)
    }

    /// 返回 writer 收到的精确确认文本，用于锁定多行内容未被应用层重写。
    fn submitted_text(&self) -> Vec<String> {
        self.written_text
            .lock()
            .expect("Agent writer test mutex should not be poisoned")
            .clone()
    }

    /// 返回 PTY manager 的真实 create 调用数，验证 prepare 前不触发任何 provider 副作用。
    ///
    /// @author kongweiguang
    fn create_count(&self) -> usize {
        self.create_count.load(Ordering::SeqCst)
    }

    /// 返回被测试 session adapter 接受的 IDs，锁定 checkpoint 与 PTY 使用同一预分配身份。
    ///
    /// @author kongweiguang
    fn created_session_ids(&self) -> Vec<SessionId> {
        self.created_session_ids
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .clone()
    }
}

#[async_trait]
impl SessionService for MemorySessions {
    /// 既有 session 用例不会通过这个假端口创建另一个 transport。
    async fn create(
        &self,
        _request: CreateSessionRequest,
    ) -> AppResult<kerminal_domain::sessions::SessionInfo> {
        unused_port()
    }

    /// resume 真实路径在这里创建唯一 Agent owner，并返回有界测试终端信息。
    ///
    /// @author kongweiguang
    async fn create_cancellable(
        &self,
        request: CreateSessionRequest,
        cancellation: CancellationToken,
    ) -> AppResult<kerminal_domain::sessions::SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("Agent resume fixture was cancelled"));
        }
        let session = kerminal_domain::sessions::SessionInfo {
            session_id: SessionId::new(),
            target: request.target,
            owner: request.owner,
            size: request.size,
            lifecycle: kerminal_domain::sessions::SessionLifecycle::Running,
            exit_status: None,
        };
        self.create_count.fetch_add(1, Ordering::SeqCst);
        self.created_session_ids
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .push(session.session_id.clone());
        self.sessions
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .push(session.clone());
        Ok(session)
    }

    /// 只接受 service 预分配的 exact session ID，使 fake 覆盖 marker-before-PTY 的所有权契约。
    ///
    /// @author kongweiguang
    async fn create_with_session_id(
        &self,
        request: CreateSessionRequest,
        session_id: SessionId,
        cancellation: CancellationToken,
    ) -> AppResult<kerminal_domain::sessions::SessionInfo> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("Agent resume fixture was cancelled"));
        }
        let session = kerminal_domain::sessions::SessionInfo {
            session_id: session_id.clone(),
            target: request.target,
            owner: request.owner,
            size: request.size,
            lifecycle: kerminal_domain::sessions::SessionLifecycle::Running,
            exit_status: None,
        };
        self.create_count.fetch_add(1, Ordering::SeqCst);
        self.created_session_ids
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .push(session_id);
        self.sessions
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .push(session.clone());
        Ok(session)
    }

    /// endpoint 与 Agent use case 仅查询固定 live session 列表。
    async fn list(
        &self,
        filter: SessionFilter,
    ) -> AppResult<Vec<kerminal_domain::sessions::SessionInfo>> {
        let sessions = self
            .sessions
            .lock()
            .expect("Agent session test mutex should not be poisoned")
            .clone();
        Ok(sessions
            .into_iter()
            .filter(|session| {
                filter
                    .owner
                    .as_ref()
                    .is_none_or(|owner| &session.owner == owner)
                    && filter
                        .target
                        .as_ref()
                        .is_none_or(|target| &session.target == target)
            })
            .collect())
    }

    /// 本用例不读取进程 PID 或 shell integration 元数据。
    async fn runtime_metadata(&self, _session_id: SessionId) -> AppResult<SessionRuntimeMetadata> {
        unused_port()
    }

    /// 此测试不通过 raw bytes 写入终端。
    async fn write(&self, _session_id: SessionId, _bytes: Vec<u8>) -> AppResult<()> {
        unused_port()
    }

    /// 不匹配的 UI 输入通道不能冒充 Agent 已确认提交。
    async fn write_input(
        &self,
        _session_id: SessionId,
        _event: TerminalInputEvent,
    ) -> AppResult<()> {
        unused_port()
    }

    /// UI 命令行提交必须经过绑定与 generation 准入；此 fake 不模拟该生产写入边界。
    ///
    /// @author kongweiguang
    async fn submit_ui_command_line(
        &self,
        _request: UiCommandLineRequest,
    ) -> AppResult<TerminalCommandContext> {
        Err(AppError::unsupported(
            "UI command-line submission is unavailable for this fake",
        ))
    }

    /// application orchestration 不改终端尺寸。
    async fn resize(&self, _session_id: SessionId, _size: TerminalSize) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不构造 GPUI 渲染快照。
    async fn render_snapshot(
        &self,
        _session_id: SessionId,
        _request: RenderSnapshotRequest,
    ) -> AppResult<RenderSnapshot> {
        unused_port()
    }

    /// 本测试不更改终端颜色。
    async fn set_palette(
        &self,
        _session_id: SessionId,
        _palette: TerminalPalette,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不更改 scrollback 限额。
    async fn set_scrollback_limit(
        &self,
        _session_id: SessionId,
        _lines: usize,
    ) -> AppResult<usize> {
        unused_port()
    }

    /// 本用例不从 scrollback 读取文本。
    async fn plaintext_snapshot(
        &self,
        _session_id: SessionId,
        _request: PlainTextSnapshotRequest,
    ) -> AppResult<PlainTextSnapshot> {
        unused_port()
    }

    /// 本测试不滚动终端视口。
    async fn scroll(&self, _session_id: SessionId, _request: ScrollRequest) -> AppResult<()> {
        unused_port()
    }

    /// 此 fake 不维护可见终端视口，避免测试误把普通滚动当作绑定滚动。
    ///
    /// @author kongweiguang
    async fn scroll_ui(
        &self,
        _binding: crate::sessions::UiSessionBinding,
        _request: ScrollRequest,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不变更文本选择。
    async fn set_selection(
        &self,
        _session_id: SessionId,
        _selection: Option<TerminalSelection>,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不提取当前选择文本。
    async fn selected_text(&self, _session_id: SessionId) -> AppResult<Option<String>> {
        unused_port()
    }

    /// 本测试不搜索 scrollback。
    async fn search(
        &self,
        _session_id: SessionId,
        _request: TerminalSearchRequest,
    ) -> AppResult<TerminalSearchResult> {
        unused_port()
    }

    /// 本用例不重连远程 session。
    async fn reconnect_cancellable(
        &self,
        _session_id: SessionId,
        _cancellation: CancellationToken,
    ) -> AppResult<kerminal_domain::sessions::SessionInfo> {
        unused_port()
    }

    /// Agent session 关闭属于生命周期测试范围之外。
    async fn close(&self, _session_id: SessionId) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不触发带 deadline 的终端清理。
    async fn close_until(
        &self,
        _session_id: SessionId,
        _deadline: SessionDeadline,
    ) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不关闭应用级终端集合。
    async fn close_all(&self) -> AppResult<()> {
        unused_port()
    }

    /// 本测试不运行 session watch 消费循环。
    async fn subscribe(&self, _session_id: SessionId) -> AppResult<SessionSubscription> {
        unused_port()
    }

    /// 本测试不执行另一类受管 shell 命令。
    async fn execute_shell_command(
        &self,
        _request: ShellCommandRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<ShellCommandOutcome> {
        unused_port()
    }

    /// 提供带 session generation 的真实命令块查询边界供 Agent 用例复核。
    async fn command_block_snapshot(
        &self,
        session_id: SessionId,
        _cancellation: &CancellationToken,
    ) -> AppResult<CommandBlockSnapshot> {
        Ok(CommandBlockSnapshot {
            session_id,
            session_generation: self.generation,
            revision: 1,
            blocks: Vec::new(),
        })
    }

    /// 只在应用完成状态核对后计数提交；未知 ACK 返回 typed error 且不在 service 内重放。
    async fn write_agent_submission(
        &self,
        request: crate::sessions::AgentTerminalSubmissionRequest,
        _cancellation: CancellationToken,
    ) -> AppResult<TerminalWriteReceipt> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        self.written_text
            .lock()
            .expect("Agent writer test mutex should not be poisoned")
            .push(request.text.clone());
        if self.writer_unknown {
            return Err(AppError::outcome_unknown(
                "simulated terminal write ACK loss",
            ));
        }
        Ok(TerminalWriteReceipt {
            acknowledged_bytes: request.text.len(),
            admission: None,
        })
    }
}

/// 以当前格式的显式全局归属构造隔离 Agent，不读取机器上的 CLI 或用户配置。
///
/// @author kongweiguang
fn active_agent(agent_session_id: AgentSessionId, status: AgentSessionStatus) -> AgentSession {
    let session_root = format!("C:/fixture/agents/sessions/{}", agent_session_id);
    let launch = kerminal_domain::agents::AgentSessionLaunch {
        command_label: "Codex".to_owned(),
        shell: "codex".to_owned(),
        args: Vec::new(),
        cwd: session_root.clone(),
    };
    AgentSession::restore(
        1,
        agent_session_id,
        AgentProvider::Codex,
        None,
        "test agent".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        status,
        "C:/fixture/.kerminal".to_owned(),
        session_root,
        launch,
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("Agent fixture must satisfy the domain invariants")
}

/// 为 builtin Codex 固定有效恢复元数据，避免解析当前 selected/custom 配置。
fn codex_provider_session() -> AgentProviderSession {
    AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Codex,
        provider_session_id: None,
        resume_command: Some("codex resume --last".to_owned()),
        resume_supported: true,
        last_resume_at: None,
    }
}

/// 为 Custom 历史提供与原启动定义相符的 provider 文件快照。
///
/// @author kongweiguang
fn custom_provider_session() -> AgentProviderSession {
    AgentProviderSession {
        schema_version: 1,
        provider: AgentProvider::Custom,
        provider_session_id: Some("old-custom-session".to_owned()),
        resume_command: Some("custom-agent --continue old-custom-session".to_owned()),
        resume_supported: true,
        last_resume_at: Some("2026-10-16T00:00:00Z".to_owned()),
    }
}

/// 构造一个可独立派生的 Custom 历史，不接触真实 CLI 或用户安装路径。
///
/// @author kongweiguang
fn custom_history_agent(agent_session_id: AgentSessionId) -> AgentSession {
    let session_root = format!("C:/fixture/.kerminal/agents/sessions/{agent_session_id}");
    AgentSession::restore(
        1,
        agent_session_id.clone(),
        AgentProvider::Custom,
        Some(
            AgentLauncherKey::new(
                format!("custom:{}", agent_session_id.as_str()),
                AgentProvider::Custom,
            )
            .expect("canonical fixture ID produces a valid custom key"),
        ),
        "custom history".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        "2026-10-17T00:00:00Z".to_owned(),
        AgentSessionStatus::Archived,
        "C:/fixture/.kerminal".to_owned(),
        session_root.clone(),
        kerminal_domain::agents::AgentSessionLaunch {
            command_label: "Custom CLI".to_owned(),
            shell: "C:/fixture/bin/custom-agent.cmd".to_owned(),
            args: vec!["--profile".to_owned(), "review".to_owned()],
            cwd: session_root,
        },
        Some(AgentSessionScope::Global),
        Some(AgentTargetBinding::unbound(2)),
    )
    .expect("custom history fixture must satisfy domain invariants")
}

/// 在 restore 校验后固定历史 scope 与 target 身份，避免测试绕过领域不变量。
///
/// @author kongweiguang
fn agent_with_history_binding(
    agent_session_id: AgentSessionId,
    status: AgentSessionStatus,
    target: AgentTargetBinding,
) -> AgentSession {
    let base = active_agent(agent_session_id, status);
    AgentSession::restore(
        base.schema_version,
        base.agent_session_id,
        base.agent_id,
        base.launcher_key,
        base.title,
        base.created_at,
        base.updated_at,
        base.status,
        base.workspace_root,
        base.session_root,
        base.launch,
        Some(AgentSessionScope::Global),
        Some(target),
    )
    .expect("history binding fixture must satisfy domain invariants")
}

/// 构造可用于历史筛选的 stale target；它只作为稳定身份，不要求活动终端存在。
///
/// @author kongweiguang
fn stale_target(reference: &str) -> AgentTargetBinding {
    let mut binding = AgentTargetBinding::unbound(3);
    binding.terminal_session_id = Some(SessionId::new());
    binding.target_ref = Some(reference.to_owned());
    binding.live_status = kerminal_domain::agents::AgentTargetLiveStatus::Stale;
    binding
}

/// 创建仅用于恢复与发送用例的服务，workspace/session ports 都留在隔离内存中。
fn agent_service(
    workspace: Arc<MemoryAgentWorkspace>,
    sessions: Arc<MemorySessions>,
) -> AgentSessionService {
    let preferences = Arc::new(PreferencesService::new(Arc::new(
        UnusedConfigurationRepository,
    )));
    AgentSessionService::new(sessions, workspace, preferences)
}

/// 构造 owner、lifecycle 与目标均显式的终端记录，防止测试绕过身份字段。
fn terminal_info(
    session_id: SessionId,
    owner: SessionOwner,
    lifecycle: kerminal_domain::sessions::SessionLifecycle,
) -> kerminal_domain::sessions::SessionInfo {
    kerminal_domain::sessions::SessionInfo {
        session_id,
        target: SessionTarget::Local(LocalSessionTarget {
            program: OsString::from("codex"),
            args: Vec::new(),
            working_directory: Some(PathBuf::from("C:/fixture/agents/sessions")),
            environment: BTreeMap::new(),
        }),
        owner,
        size: TerminalSize::default(),
        lifecycle,
        exit_status: None::<SessionExitStatus>,
    }
}

/// 创建 generation 可控的在线 Agent writer fixture，初始状态不含 pending 恢复。
fn submission_fixture(
    status: AgentSessionStatus,
    owner_matches: bool,
    generation: u64,
    writer_unknown: bool,
) -> (
    AgentSessionService,
    Arc<MemorySessions>,
    AgentSessionId,
    SessionId,
) {
    let agent_session_id = AgentSessionId::new();
    let terminal_session_id = SessionId::new();
    let terminal_owner = SessionOwner::Agent {
        agent_session_id: if owner_matches {
            agent_session_id.clone()
        } else {
            AgentSessionId::new()
        },
    };
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(agent_session_id.clone(), status),
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let sessions = Arc::new(MemorySessions::new(
        vec![terminal_info(
            terminal_session_id.clone(),
            terminal_owner,
            kerminal_domain::sessions::SessionLifecycle::Running,
        )],
        generation,
        writer_unknown,
    ));
    (
        agent_service(workspace, sessions.clone()),
        sessions,
        agent_session_id,
        terminal_session_id,
    )
}

/// 生成可直接提交的多行文本请求，固定 AgentId、TerminalId 与预览 generation。
fn submission_request(
    agent_session_id: AgentSessionId,
    terminal_session_id: SessionId,
    generation: u64,
    text: &str,
) -> SendAgentCommandRequest {
    SendAgentCommandRequest {
        agent_session_id,
        terminal_session_id,
        expected_session_generation: generation,
        text: text.to_owned(),
    }
}

/// 标记本测试不会调用的 application port，意外走到时立即失败。
///
/// @author kongweiguang
fn unused_port<T>() -> AppResult<T> {
    Err(AppError::unsupported("unused Agent submission test port"))
}

/// CAS 冲突后旧 session 与新 CLI 并存时必须阻止提交；完整读回后同一 owner 才可提交一次。
///
/// @author kongweiguang
#[tokio::test]
async fn pending_resume_blocks_then_readback_allows_one_exact_submission() {
    let agent_session_id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(agent_session_id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let resumed = service
        .resume(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("resume keeps the created CLI owner after a workspace CAS conflict");
    let terminal_session_id = resumed
        .terminal
        .expect("resumed CLI remains attached to the real owner")
        .session_id;
    assert_eq!(
        service
            .pending_resume(&agent_session_id)
            .await
            .expect("same resume attempt remains pending")
            .create_admission
            .state(),
        CreateAdmission::MayHaveStarted,
        "only an admitted create can reconcile a running owner"
    );
    assert_eq!(
        sessions.created_session_ids(),
        vec![terminal_session_id.clone()],
        "the PTY must use the ID reserved in the durable checkpoint"
    );
    assert_eq!(
        workspace
            .resume_checkpoint(&agent_session_id)
            .expect("pending marker remains durable")
            .checkpoint
            .terminal_session_id,
        terminal_session_id
    );
    assert_eq!(
        resumed
            .resume_recovery
            .as_ref()
            .map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Pending)
    );

    let rename_error = service
        .rename_title(
            RenameAgentSessionTitleRequest {
                agent_session_id: agent_session_id.clone(),
                expected_title: resumed.session.title.clone(),
                title: "must wait for readback".to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("pending recovery must freeze session metadata writes");
    assert_eq!(rename_error.public_code(), PublicErrorCode::Conflict);
    let binding_error = service
        .bind_preferred_target(
            agent_session_id.clone(),
            AgentTargetBinding::unbound(1),
            CancellationToken::new(),
        )
        .await
        .expect_err("pending recovery must freeze target rebinding");
    assert_eq!(binding_error.public_code(), PublicErrorCode::Conflict);

    let text = "show status\nready\tconfirmed";
    let pending = service
        .send_command_block_text(
            submission_request(
                agent_session_id.clone(),
                terminal_session_id.clone(),
                TEST_SESSION_GENERATION,
                text,
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("unconfirmed resume must block Agent input");
    assert_eq!(pending.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(sessions.write_count(), 0);

    let reconciled = service
        .reconcile_resume(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("readback should retry only the original workspace transaction");
    assert!(reconciled.resume_recovery.is_none());
    assert_eq!(
        reconciled
            .terminal
            .as_ref()
            .expect("reconciliation must retain the exact owner")
            .session_id,
        terminal_session_id
    );
    assert_eq!(
        sessions
            .list(SessionFilter::default())
            .await
            .expect("live Agent sessions should remain readable")
            .len(),
        1,
        "reconciliation must not create another CLI terminal"
    );
    assert!(service.pending_resume(&agent_session_id).await.is_none());

    let receipt = service
        .send_command_block_text(
            submission_request(
                agent_session_id.clone(),
                terminal_session_id,
                TEST_SESSION_GENERATION,
                text,
            ),
            CancellationToken::new(),
        )
        .await
        .expect("only the confirmed original Agent owner may submit after reconciliation");
    assert_eq!(receipt.acknowledged_bytes, text.len());
    assert_eq!(sessions.write_count(), 1);
    assert_eq!(sessions.submitted_text(), vec![text.to_owned()]);
    assert!(service.pending_resume(&agent_session_id).await.is_none());
}

/// 写盘已 ACK 后读回取消必须返回待核对 owner，不能把真实已启动会话伪装为取消成功。
///
/// @author kongweiguang
#[tokio::test]
async fn post_commit_readback_cancellation_retains_owner_until_explicit_reconciliation() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Stale),
        codex_provider_session(),
        CommitBehavior::AcceptThenReadCancelled,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let resumed = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("an ACKed resume cannot become a cancellation-only result");
    let owner = resumed
        .terminal
        .expect("the ACKed CLI owner remains visible")
        .session_id;
    assert_eq!(
        resumed.resume_recovery.map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );
    assert_eq!(
        workspace.stored_session(&id).status,
        AgentSessionStatus::Active
    );

    let checked = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("readback can confirm the already committed transaction");
    assert!(checked.resume_recovery.is_none());
    assert_eq!(
        checked.terminal.expect("same CLI remains owned").session_id,
        owner
    );
    assert!(service.pending_resume(&id).await.is_none());
    assert_eq!(
        sessions
            .list(SessionFilter::default())
            .await
            .expect("read live owners")
            .len(),
        1,
        "reconciliation must never create another CLI"
    );
}

/// 原 owner 停止后缺少 Running owner 证明时保持 Unknown，任何恢复入口都不得启动第二个 CLI。
///
/// @author kongweiguang
#[tokio::test]
async fn stopped_pending_owner_reconciles_without_starting_another_cli() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let pending = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("conflict retains original CLI owner");
    let owner = pending.terminal.expect("original owner").session_id;
    sessions.sessions.lock().expect("session registry")[0].lifecycle = SessionLifecycle::Exited;

    let checked = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("stopped owner leaves the durable transaction unresolved");
    assert_eq!(
        checked
            .resume_recovery
            .as_ref()
            .map(|recovery| (recovery.status, recovery.terminal_session_id.clone())),
        Some((AgentResumeRecoveryStatus::Unknown, owner.clone()))
    );
    assert_eq!(checked.session.status, AgentSessionStatus::Active);
    assert_eq!(
        checked.terminal.expect("history owner remains").session_id,
        owner
    );
    assert_eq!(sessions.sessions.lock().expect("session registry").len(), 1);
    assert!(matches!(
        workspace.stored_provider(&id),
        AgentProviderSessionStatus::Available(provider) if provider.last_resume_at.is_none()
    ));
    assert!(workspace.resume_checkpoint(&id).is_some());

    let resumed = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("explicit resume also honors the unresolved checkpoint");
    assert_eq!(
        resumed
            .resume_recovery
            .as_ref()
            .map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );
    assert_eq!(
        resumed.terminal.expect("historical owner").session_id,
        owner
    );
    assert_eq!(sessions.create_count(), 1);
    assert!(workspace.resume_checkpoint(&id).is_some());
}

/// Prepare 拒绝或 ACK 未知时不得开启 PTY；未知 marker 必须阻断后续普通 resume。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_prepare_failure_or_unknown_never_creates_a_terminal() {
    for behavior in [
        PrepareBehavior::ErrorWithoutWrite,
        PrepareBehavior::UnknownAfterWrite,
    ] {
        let id = AgentSessionId::new();
        let workspace = Arc::new(MemoryAgentWorkspace::new(
            active_agent(id.clone(), AgentSessionStatus::Active),
            codex_provider_session(),
            CommitBehavior::Accept,
        ));
        workspace.set_next_prepare(behavior);
        let sessions = Arc::new(MemorySessions::new(
            Vec::new(),
            TEST_SESSION_GENERATION,
            false,
        ));
        let service = agent_service(workspace.clone(), sessions.clone());
        *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

        let error = service
            .resume(id.clone(), CancellationToken::new())
            .await
            .expect_err("prepare has to complete before any PTY provider open");
        assert_eq!(sessions.create_count(), 0);
        assert!(sessions.created_session_ids().is_empty());
        match behavior {
            PrepareBehavior::ErrorWithoutWrite => {
                assert_eq!(error.public_code(), PublicErrorCode::Conflict);
                assert!(workspace.resume_checkpoint(&id).is_none());
            }
            PrepareBehavior::UnknownAfterWrite => {
                assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
                let marker = workspace
                    .resume_checkpoint(&id)
                    .expect("unknown prepare ACK keeps durable marker");
                assert_eq!(
                    marker.checkpoint.phase,
                    AgentResumeCheckpointPhase::Prepared
                );
                let blocked = service
                    .resume(id.clone(), CancellationToken::new())
                    .await
                    .expect("unknown marker projects as unresolved instead of reopening CLI");
                assert_eq!(
                    blocked
                        .resume_recovery
                        .as_ref()
                        .map(|recovery| recovery.status),
                    Some(AgentResumeRecoveryStatus::Unknown)
                );
                assert_eq!(sessions.create_count(), 0);
                assert_eq!(workspace.resume_checkpoint(&id), Some(marker));
            }
            PrepareBehavior::Accept | PrepareBehavior::AcceptThenCancelCaller => {
                unreachable!("test only injects non-accept behavior")
            }
        }
    }
}

/// prepare 后取消保持 NotAdmitted；显式核对只清理原事务，之后的用户继续操作才启动 CLI。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_cancellation_after_prepare_clears_before_explicit_retry() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    workspace.set_next_prepare(PrepareBehavior::AcceptThenCancelCaller);
    let caller = CancellationToken::new();
    workspace.cancel_caller_after_next_prepare(caller.clone());
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let error = service
        .resume(id.clone(), caller)
        .await
        .expect_err("cancellation after prepare must not cross the PTY admission boundary");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(sessions.create_count(), 0);
    let marker = workspace
        .resume_checkpoint(&id)
        .expect("Prepared marker remains durable");
    assert_eq!(
        marker.checkpoint.phase,
        AgentResumeCheckpointPhase::Prepared
    );
    assert_eq!(
        service
            .pending_resume(&id)
            .await
            .expect("RAM attempt retained")
            .create_admission
            .state(),
        CreateAdmission::NotAdmitted
    );

    let historical_owner = SessionId::new();
    sessions
        .sessions
        .lock()
        .expect("session registry")
        .push(terminal_info(
            historical_owner.clone(),
            SessionOwner::Agent {
                agent_session_id: id.clone(),
            },
            SessionLifecycle::Exited,
        ));
    let checked = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("reconcile confirms unchanged bases and absence without opening a CLI");
    assert!(checked.resume_recovery.is_none());
    assert!(workspace.resume_checkpoint(&id).is_none());
    assert!(service.pending_resume(&id).await.is_none());
    assert_eq!(sessions.create_count(), 0);

    sessions.sessions.lock().expect("session registry").clear();
    let resumed = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("a separate explicit resume may create after prestart cleanup");
    assert!(resumed.resume_recovery.is_none());
    assert_eq!(sessions.create_count(), 1);
    assert_eq!(sessions.created_session_ids().len(), 1);
    assert_ne!(sessions.created_session_ids()[0], historical_owner);
}
/// Session 或 provider 的任一基线改变，都阻止清理 NotAdmitted checkpoint。
///
/// @author kongweiguang
#[tokio::test]
async fn not_admitted_reconciliation_rejects_each_changed_base_revision() {
    for changed_session in [true, false] {
        let id = AgentSessionId::new();
        let workspace = Arc::new(MemoryAgentWorkspace::new(
            active_agent(id.clone(), AgentSessionStatus::Active),
            codex_provider_session(),
            CommitBehavior::Accept,
        ));
        workspace.set_next_prepare(PrepareBehavior::AcceptThenCancelCaller);
        let caller = CancellationToken::new();
        workspace.cancel_caller_after_next_prepare(caller.clone());
        let sessions = Arc::new(MemorySessions::new(
            Vec::new(),
            TEST_SESSION_GENERATION,
            false,
        ));
        let service = agent_service(workspace.clone(), sessions.clone());
        *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
        assert_eq!(
            service
                .resume(id.clone(), caller)
                .await
                .unwrap_err()
                .public_code(),
            PublicErrorCode::Cancelled
        );
        let marker = workspace.resume_checkpoint(&id).expect("Prepared marker");
        {
            let mut state = workspace.state.lock().expect("workspace registry");
            if changed_session {
                state
                    .sessions
                    .get_mut(id.as_str())
                    .expect("session document")
                    .revision =
                    AgentWorkspaceRevision::Sha256("external-session-revision".to_owned());
            } else {
                state
                    .providers
                    .get_mut(id.as_str())
                    .expect("provider document")
                    .revision =
                    AgentWorkspaceRevision::Sha256("external-provider-revision".to_owned());
            }
        }
        let checked = service
            .reconcile_resume(id.clone(), CancellationToken::new())
            .await
            .expect("a third revision remains unknown");
        assert_eq!(
            checked.resume_recovery.map(|recovery| recovery.status),
            Some(AgentResumeRecoveryStatus::Unknown)
        );
        assert_eq!(workspace.resume_checkpoint(&id), Some(marker));
        assert!(service.pending_resume(&id).await.is_some());
        assert_eq!(sessions.create_count(), 0);
    }
}

/// 精确预留 ID 或其他仍活动的 Agent owner 都阻止 NotAdmitted checkpoint 清理。
///
/// @author kongweiguang
#[tokio::test]
async fn not_admitted_reconciliation_rejects_reserved_id_or_live_agent_owner() {
    for exact_reserved_id in [true, false] {
        let id = AgentSessionId::new();
        let workspace = Arc::new(MemoryAgentWorkspace::new(
            active_agent(id.clone(), AgentSessionStatus::Active),
            codex_provider_session(),
            CommitBehavior::Accept,
        ));
        workspace.set_next_prepare(PrepareBehavior::AcceptThenCancelCaller);
        let caller = CancellationToken::new();
        workspace.cancel_caller_after_next_prepare(caller.clone());
        let sessions = Arc::new(MemorySessions::new(
            Vec::new(),
            TEST_SESSION_GENERATION,
            false,
        ));
        let service = agent_service(workspace.clone(), sessions.clone());
        *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
        assert_eq!(
            service
                .resume(id.clone(), caller)
                .await
                .unwrap_err()
                .public_code(),
            PublicErrorCode::Cancelled
        );
        let marker = workspace.resume_checkpoint(&id).expect("Prepared marker");
        sessions
            .sessions
            .lock()
            .expect("session registry")
            .push(terminal_info(
                if exact_reserved_id {
                    marker.checkpoint.terminal_session_id.clone()
                } else {
                    SessionId::new()
                },
                SessionOwner::Agent {
                    agent_session_id: id.clone(),
                },
                if exact_reserved_id {
                    SessionLifecycle::Exited
                } else {
                    SessionLifecycle::Running
                },
            ));
        let checked = service
            .reconcile_resume(id.clone(), CancellationToken::new())
            .await
            .expect("a present reserved ID or active owner is not proof of non-creation");
        assert_eq!(
            checked.resume_recovery.map(|recovery| recovery.status),
            Some(AgentResumeRecoveryStatus::Unknown)
        );
        assert_eq!(workspace.resume_checkpoint(&id), Some(marker));
        assert_eq!(sessions.create_count(), 0);
    }
}

/// Clear ACK 后读回失败先保持 Unknown；后续显式核对仅凭同 RAM NotAdmitted 证据安全收敛。
///
/// @author kongweiguang
#[tokio::test]
async fn not_admitted_clear_readback_failure_reconciles_without_starting_cli() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    workspace.set_next_clear_readback_unknown();
    workspace.set_next_prepare(PrepareBehavior::AcceptThenCancelCaller);
    let caller = CancellationToken::new();
    workspace.cancel_caller_after_next_prepare(caller.clone());
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    assert_eq!(
        service
            .resume(id.clone(), caller)
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Cancelled
    );

    let first = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("clear ACK readback interruption remains unknown");
    assert_eq!(
        first.resume_recovery.map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );
    assert!(workspace.resume_checkpoint(&id).is_none());
    assert!(service.pending_resume(&id).await.is_some());

    let second = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("same RAM NotAdmitted transaction can finish after absence is read back");
    assert!(second.resume_recovery.is_none());
    assert!(service.pending_resume(&id).await.is_none());
    assert!(workspace.resume_checkpoint(&id).is_none());
    assert_eq!(sessions.create_count(), 0);
}

/// Clear ACK 后的短暂读回失败只报告 Unknown；下一次显式核对可凭原 revision 与 owner 安全收尾。
///
/// @author kongweiguang
#[tokio::test]
async fn committed_resume_clear_readback_failure_recovers_without_opening_another_cli() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Stale),
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    workspace.set_next_clear_readback_unknown();
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let first = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("a completed CLI remains visible when marker readback is transiently unknown");
    let owner = first
        .terminal
        .expect("exact created owner remains attached")
        .session_id;
    assert_eq!(
        first.resume_recovery.map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );
    assert!(workspace.resume_checkpoint(&id).is_none());
    assert_eq!(sessions.create_count(), 1);
    assert_eq!(
        service
            .pending_resume(&id)
            .await
            .expect("committed RAM intent remains available for explicit readback")
            .checkpoint
            .checkpoint
            .phase,
        AgentResumeCheckpointPhase::Committed
    );

    let reconciled = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("marker absence plus intended documents and exact owner can finish readback");
    assert!(reconciled.resume_recovery.is_none());
    assert_eq!(
        reconciled.terminal.expect("same exact owner").session_id,
        owner
    );
    assert!(service.pending_resume(&id).await.is_none());
    assert!(workspace.resume_checkpoint(&id).is_none());
    assert_eq!(sessions.create_count(), 1);
    assert_eq!(sessions.created_session_ids().len(), 1);
}

/// 新进程没有原 RAM typed 意图且 registry 没有 exact owner 时，保留 marker 并拒绝启动或重放。
///
/// @author kongweiguang
#[tokio::test]
async fn hard_restart_without_exact_owner_keeps_prepared_checkpoint_unknown() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let original_sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let first_service = agent_service(workspace.clone(), original_sessions.clone());
    *first_service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let pending = first_service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("failed commit leaves prepared intent and actual owner");
    let marker = workspace
        .resume_checkpoint(&id)
        .expect("prepared marker remains persisted");
    assert_eq!(original_sessions.create_count(), 1);
    assert_eq!(
        original_sessions.created_session_ids(),
        vec![marker.checkpoint.terminal_session_id.clone()]
    );
    assert_eq!(
        pending
            .resume_recovery
            .as_ref()
            .map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Pending)
    );

    let restarted_sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let restarted_service = agent_service(workspace.clone(), restarted_sessions.clone());
    *restarted_service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let resume = restarted_service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("normal resume must consult durable state before opening another CLI");
    let reconcile = restarted_service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("hard-restart reconciliation without exact owner remains unknown");
    for view in [resume, reconcile] {
        assert_eq!(
            view.resume_recovery
                .as_ref()
                .map(|recovery| (recovery.status, recovery.terminal_session_id.clone())),
            Some((
                AgentResumeRecoveryStatus::Unknown,
                marker.checkpoint.terminal_session_id.clone()
            ))
        );
    }
    assert_eq!(restarted_sessions.create_count(), 0);
    assert!(restarted_sessions.created_session_ids().is_empty());
    assert_eq!(workspace.resume_checkpoint(&id), Some(marker));
}

/// 外部修改产生第三种基线时保持 Unknown 和原 owner，不能用恢复重试覆盖用户的新元数据。
///
/// @author kongweiguang
#[tokio::test]
async fn reconciliation_preserves_external_session_changes_and_original_owner() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let pending = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("pending original owner");
    let owner = pending.terminal.expect("original owner").session_id;
    {
        let mut state = workspace.state.lock().expect("workspace registry");
        let snapshot = state.sessions.get_mut(id.as_str()).expect("Agent record");
        snapshot.session.title = "external title must survive".to_owned();
        snapshot.revision = AgentWorkspaceRevision::Sha256("external-revision".to_owned());
    }

    let checked = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("unrecognized baseline stays visible without being overwritten");
    assert_eq!(
        checked
            .resume_recovery
            .map(|recovery| (recovery.status, recovery.terminal_session_id)),
        Some((AgentResumeRecoveryStatus::Unknown, owner.clone()))
    );
    assert_eq!(checked.terminal.expect("same CLI").session_id, owner);
    assert_eq!(
        workspace.stored_session(&id).title,
        "external title must survive"
    );
    assert_eq!(sessions.sessions.lock().expect("session registry").len(), 1);
    assert!(service.pending_resume(&id).await.is_some());
    let rename = service
        .rename_title(
            RenameAgentSessionTitleRequest {
                agent_session_id: id.clone(),
                expected_title: "external title must survive".to_owned(),
                title: "cannot overwrite an unknown recovery".to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown recovery freezes title changes too");
    assert_eq!(rename.public_code(), PublicErrorCode::Conflict);
    let bind = service
        .bind_preferred_target(
            id.clone(),
            AgentTargetBinding::unbound(1),
            CancellationToken::new(),
        )
        .await
        .expect_err("unknown recovery freezes target changes too");
    assert_eq!(bind.public_code(), PublicErrorCode::Conflict);
    assert_eq!(
        workspace.stored_session(&id).title,
        "external title must survive"
    );
}

/// Starting 和 Closing 仍占有资源，不能被历史 Exited 条目遮挡后触发第二个 CLI。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_rejects_existing_starting_or_closing_owner() {
    for lifecycle in [SessionLifecycle::Starting, SessionLifecycle::Closing] {
        let id = AgentSessionId::new();
        let live_id = SessionId::new();
        let workspace = Arc::new(MemoryAgentWorkspace::new(
            active_agent(id.clone(), AgentSessionStatus::Stale),
            codex_provider_session(),
            CommitBehavior::Accept,
        ));
        let sessions = Arc::new(MemorySessions::new(
            vec![
                terminal_info(
                    SessionId::new(),
                    SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    },
                    SessionLifecycle::Exited,
                ),
                terminal_info(
                    live_id.clone(),
                    SessionOwner::Agent {
                        agent_session_id: id.clone(),
                    },
                    lifecycle,
                ),
            ],
            TEST_SESSION_GENERATION,
            false,
        ));
        let service = agent_service(workspace, sessions.clone());
        *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
        let error = service
            .resume(id.clone(), CancellationToken::new())
            .await
            .expect_err("pending lifecycle cannot be replaced by another CLI");
        assert_eq!(error.public_code(), PublicErrorCode::Conflict);
        let registry = sessions.sessions.lock().expect("session registry");
        assert_eq!(registry.len(), 2);
        assert_eq!(
            find_agent_terminal(&registry, &id)
                .expect("live owner")
                .session_id,
            live_id
        );
    }
}

/// 原 owner 已结束而另一个 owner 存活时，不把新 CLI 当成本事务，也不自动启动第三个。
///
/// @author kongweiguang
#[tokio::test]
async fn reconciliation_retains_original_identity_when_live_owner_was_replaced() {
    let id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let pending = service
        .resume(id.clone(), CancellationToken::new())
        .await
        .expect("pending original CLI");
    let original = pending.terminal.expect("original owner").session_id;
    let replacement = SessionId::new();
    {
        let mut registry = sessions.sessions.lock().expect("session registry");
        registry[0].lifecycle = SessionLifecycle::Exited;
        registry.push(terminal_info(
            replacement.clone(),
            SessionOwner::Agent {
                agent_session_id: id.clone(),
            },
            SessionLifecycle::Running,
        ));
    }

    let checked = service
        .reconcile_resume(id.clone(), CancellationToken::new())
        .await
        .expect("replaced owner must stay an explicit unknown result");
    assert_eq!(
        checked
            .resume_recovery
            .map(|recovery| (recovery.status, recovery.terminal_session_id)),
        Some((AgentResumeRecoveryStatus::Unknown, original.clone()))
    );
    assert_eq!(
        checked.terminal.expect("actual current owner").session_id,
        replacement
    );
    assert_eq!(sessions.sessions.lock().expect("session registry").len(), 2);
    assert!(service.pending_resume(&id).await.is_some());
    assert_eq!(
        workspace
            .resume_checkpoint(&id)
            .expect("replaced owner cannot clear the durable attempt")
            .checkpoint
            .terminal_session_id,
        original
    );
}

/// session 已部分落盘而 provider 未同步时返回 Unknown，不能将仍运行的 CLI 当作已确认。
#[tokio::test]
async fn unknown_partial_resume_blocks_submission_without_touching_writer() {
    let agent_session_id = AgentSessionId::new();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        active_agent(agent_session_id.clone(), AgentSessionStatus::Active),
        codex_provider_session(),
        CommitBehavior::UnknownAfterSessionCommit,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace, sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let resumed = service
        .resume(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("resume preserves owner when only one document is confirmed");
    let terminal_session_id = resumed
        .terminal
        .expect("resume retains its actual terminal owner")
        .session_id;
    assert_eq!(
        resumed
            .resume_recovery
            .as_ref()
            .map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );

    let reconciled = service
        .reconcile_resume(agent_session_id.clone(), CancellationToken::new())
        .await
        .expect("reconciliation should retain a mixed session/provider result");
    assert_eq!(
        reconciled
            .resume_recovery
            .as_ref()
            .map(|recovery| recovery.status),
        Some(AgentResumeRecoveryStatus::Unknown)
    );
    assert_eq!(
        reconciled
            .terminal
            .as_ref()
            .expect("unknown result must retain its original terminal owner")
            .session_id,
        terminal_session_id
    );
    assert_eq!(
        sessions
            .list(SessionFilter::default())
            .await
            .expect("live Agent sessions should remain readable")
            .len(),
        1,
        "unknown reconciliation must not start another CLI"
    );

    let error = service
        .send_command_block_text(
            submission_request(
                agent_session_id,
                terminal_session_id,
                TEST_SESSION_GENERATION,
                "inspect\nresult",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("mixed session/provider state must remain blocked");
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(sessions.write_count(), 0);
}

/// 只有 Active、精确 Agent owner 和当前 generation 可到达 writer；其它状态均零写入。
#[tokio::test]
async fn archived_stale_foreign_owner_and_stale_generation_never_reach_writer() {
    let (archived, archived_writer, archived_id, archived_terminal) = submission_fixture(
        AgentSessionStatus::Archived,
        true,
        TEST_SESSION_GENERATION,
        false,
    );
    let archived_error = archived
        .send_command_block_text(
            submission_request(
                archived_id,
                archived_terminal,
                TEST_SESSION_GENERATION,
                "visible text",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("archived records are read-only");
    assert_eq!(archived_error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(archived_writer.write_count(), 0);

    let (stale, stale_writer, stale_id, stale_terminal) = submission_fixture(
        AgentSessionStatus::Stale,
        true,
        TEST_SESSION_GENERATION,
        false,
    );
    let stale_error = stale
        .send_command_block_text(
            submission_request(
                stale_id,
                stale_terminal,
                TEST_SESSION_GENERATION,
                "visible text",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("stale records cannot accept Agent text");
    assert_eq!(stale_error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(stale_writer.write_count(), 0);

    let (foreign, foreign_writer, foreign_id, foreign_terminal) = submission_fixture(
        AgentSessionStatus::Active,
        false,
        TEST_SESSION_GENERATION,
        false,
    );
    // fixture 使用独立业务 ID；foreign owner 与请求 ID 不相同，必须在 writer 前拒绝。
    let foreign_error = foreign
        .send_command_block_text(
            submission_request(
                foreign_id,
                foreign_terminal,
                TEST_SESSION_GENERATION,
                "visible text",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("a different Agent cannot use this runtime owner");
    assert_eq!(foreign_error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(foreign_writer.write_count(), 0);

    let (generation, generation_writer, generation_id, generation_terminal) = submission_fixture(
        AgentSessionStatus::Active,
        true,
        TEST_SESSION_GENERATION + 1,
        false,
    );
    let generation_error = generation
        .send_command_block_text(
            submission_request(
                generation_id,
                generation_terminal,
                TEST_SESSION_GENERATION,
                "visible text",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("stale preview generation must be refreshed before writing");
    assert_eq!(generation_error.public_code(), PublicErrorCode::Conflict);
    assert_eq!(generation_writer.write_count(), 0);
}

/// writer ACK 未知透传给调用方且本用例只发起一次提交，禁止 service 自动重放。
#[tokio::test]
async fn unknown_writer_ack_is_returned_without_retry() {
    let (service, sessions, agent_session_id, terminal_session_id) = submission_fixture(
        AgentSessionStatus::Active,
        true,
        TEST_SESSION_GENERATION,
        true,
    );

    let error = service
        .send_command_block_text(
            submission_request(
                agent_session_id,
                terminal_session_id,
                TEST_SESSION_GENERATION,
                "confirmed text",
            ),
            CancellationToken::new(),
        )
        .await
        .expect_err("terminal ACK uncertainty must remain visible to the caller");
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(sessions.write_count(), 1);
    assert_eq!(sessions.submitted_text(), vec!["confirmed text".to_owned()]);
}

/// 当前目标仅在候选存在时显示匹配历史；全部与归档视图保持独立列表及准确计数。
///
/// @author kongweiguang
#[tokio::test]
async fn history_modes_filter_by_target_and_keep_archived_count() {
    let current_target = stale_target("local://terminal-a");
    let other_target = stale_target("local://terminal-b");
    let current = agent_with_history_binding(
        AgentSessionId::new(),
        AgentSessionStatus::Stale,
        current_target.clone(),
    );
    let other = agent_with_history_binding(
        AgentSessionId::new(),
        AgentSessionStatus::Stale,
        other_target,
    );
    let archived = custom_history_agent(AgentSessionId::new());
    let current_id = current.agent_session_id.clone();
    let other_id = other.agent_session_id.clone();
    let archived_id = archived.agent_session_id.clone();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        current,
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    workspace.insert_history_session(other, codex_provider_session());
    workspace.insert_history_session(archived, custom_provider_session());
    let service = agent_service(
        workspace,
        Arc::new(MemorySessions::new(
            Vec::new(),
            TEST_SESSION_GENERATION,
            false,
        )),
    );

    let current_view = service
        .history(
            AgentHistoryRequest {
                scope: AgentSessionScope::Global,
                target_candidate: Some(current_target),
                mode: AgentHistoryMode::CurrentTarget,
            },
            CancellationToken::new(),
        )
        .await
        .expect("current target history is a read-only projection");
    assert_eq!(current_view.visible_session_ids, vec![current_id.clone()]);
    assert_eq!(current_view.current_target_count, 1);
    assert_eq!(current_view.all_count, 2);
    assert_eq!(current_view.archived_count, 1);

    let no_candidate_view = service
        .history(
            AgentHistoryRequest {
                scope: AgentSessionScope::Global,
                target_candidate: None,
                mode: AgentHistoryMode::CurrentTarget,
            },
            CancellationToken::new(),
        )
        .await
        .expect("without a new-session target the current-target view is empty");
    assert!(no_candidate_view.visible_session_ids.is_empty());
    assert_eq!(no_candidate_view.current_target_count, 0);
    assert_eq!(no_candidate_view.all_count, 2);
    assert_eq!(no_candidate_view.archived_count, 1);

    let all_view = service
        .history(
            AgentHistoryRequest {
                scope: AgentSessionScope::Global,
                target_candidate: None,
                mode: AgentHistoryMode::All,
            },
            CancellationToken::new(),
        )
        .await
        .expect("all mode includes non-archived sessions across target candidates");
    assert!(all_view.visible_session_ids.contains(&current_id));
    assert!(all_view.visible_session_ids.contains(&other_id));
    assert!(!all_view.visible_session_ids.contains(&archived_id));
    assert_eq!(all_view.visible_session_ids.len(), 2);

    let archived_view = service
        .history(
            AgentHistoryRequest {
                scope: AgentSessionScope::Global,
                target_candidate: None,
                mode: AgentHistoryMode::Archived,
            },
            CancellationToken::new(),
        )
        .await
        .expect("archive view includes retained archived records only");
    assert_eq!(archived_view.visible_session_ids, vec![archived_id]);
    assert_eq!(archived_view.archived_count, 1);
}

/// 标题更新以旧值做 CAS；冲突不会覆盖当前记录，且相同草稿可在读取后重试。
///
/// @author kongweiguang
#[tokio::test]
async fn title_rename_conflict_preserves_stored_value_and_can_retry() {
    let session = active_agent(AgentSessionId::new(), AgentSessionStatus::Stale);
    let id = session.agent_session_id.clone();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        session,
        codex_provider_session(),
        CommitBehavior::ConflictWithoutCommit,
    ));
    let service = agent_service(
        workspace.clone(),
        Arc::new(MemorySessions::new(
            Vec::new(),
            TEST_SESSION_GENERATION,
            false,
        )),
    );

    let failed = service
        .rename_title(
            RenameAgentSessionTitleRequest {
                agent_session_id: id.clone(),
                expected_title: "test agent".to_owned(),
                title: "  reviewed title  ".to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("concurrent workspace edit must surface a CAS conflict");
    assert_eq!(failed.public_code(), PublicErrorCode::Conflict);
    assert_eq!(workspace.stored_session(&id).title, "test agent");

    let renamed = service
        .rename_title(
            RenameAgentSessionTitleRequest {
                agent_session_id: id.clone(),
                expected_title: "test agent".to_owned(),
                title: "  reviewed title  ".to_owned(),
            },
            CancellationToken::new(),
        )
        .await
        .expect("the same caller draft can retry against its still-current expected title");
    assert_eq!(renamed.session.title, "reviewed title");
    assert_eq!(workspace.stored_session(&id).title, "reviewed title");
}

/// 从 Custom 历史新建一条新身份，验证新 target ID 在首次 workspace 提交时已存在。
///
/// @author kongweiguang
#[tokio::test]
async fn create_from_history_keeps_custom_launch_but_allocates_fresh_session_state() {
    let source = custom_history_agent(AgentSessionId::new());
    let source_id = source.agent_session_id.clone();
    let source_root = source.session_root.clone();
    let source_launch = source.launch.clone();
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        source,
        custom_provider_session(),
        CommitBehavior::Accept,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());
    let requested_target = stale_target("local://explicit-target-before-start");
    let requested_generation = 7;
    let mut requested_target = requested_target;
    requested_target.generation = requested_generation;

    let created = service
        .create_from_history(
            CreateAgentSessionFromHistoryRequest {
                source_agent_session_id: source_id.clone(),
                title: Some("new independent chat".to_owned()),
                scope: AgentSessionScope::Global,
                target: requested_target.clone(),
                initial_palette: None,
            },
            CancellationToken::new(),
        )
        .await
        .expect("Custom history launch settings can start an isolated new session");
    let new_id = created.session.agent_session_id.clone();
    let stored_source = workspace.stored_session(&source_id);
    let stored_new = workspace.stored_session(&new_id);
    let new_provider = workspace.stored_provider(&new_id);
    let new_terminal = created
        .terminal
        .expect("new Agent owns a fresh terminal session");

    assert_ne!(new_id, source_id);
    assert_eq!(stored_source.session_root, source_root);
    assert_eq!(stored_source.launch, source_launch);
    assert_ne!(stored_new.session_root, source_root);
    assert_eq!(stored_new.launch.command_label, source_launch.command_label);
    assert_eq!(stored_new.launch.shell, source_launch.shell);
    assert_eq!(stored_new.launch.args, source_launch.args);
    assert_eq!(stored_new.launch.cwd, stored_new.session_root);
    assert_eq!(stored_new.title, "new independent chat");
    let new_binding = stored_new
        .target
        .as_ref()
        .expect("explicit target remains attached to the new history-derived session");
    let binding_id = new_binding
        .binding_id
        .as_deref()
        .filter(|binding_id| !binding_id.trim().is_empty())
        .expect("application assigns the first stable binding identity");
    let mut expected_target = requested_target.clone();
    expected_target.binding_id = Some(binding_id.to_owned());
    assert_eq!(new_binding, &expected_target);
    assert_eq!(
        workspace.first_session_binding_id(&new_id),
        Some(Some(binding_id.to_owned())),
        "the initial persisted Session must already carry the ID before CLI creation"
    );
    let first_target_context = workspace
        .context_document(&new_id, AgentWorkspaceDocumentKind::TargetBindingContext)
        .expect("initial target context was persisted");
    let first_target_context: serde_json::Value =
        serde_json::from_str(&first_target_context).expect("target context is valid JSON");
    assert_eq!(
        first_target_context["binding"]["bindingId"], binding_id,
        "first target context must agree with the first Session write"
    );
    assert!(matches!(
        new_provider,
        AgentProviderSessionStatus::Available(AgentProviderSession {
            provider_session_id: None,
            resume_command: None,
            resume_supported: false,
            ..
        })
    ));
    assert_eq!(
        new_terminal.owner,
        SessionOwner::Agent {
            agent_session_id: new_id.clone()
        }
    );
    assert_eq!(
        sessions
            .list(SessionFilter::default())
            .await
            .expect("new terminal remains in the isolated registry")
            .len(),
        1
    );
}

/// Old ended histories gain one durable binding ID without selecting a replacement target or starting a CLI.
///
/// @author kongweiguang
#[tokio::test]
async fn target_context_repairs_legacy_binding_id_once_and_keeps_all_contexts_aligned() {
    let id = AgentSessionId::new();
    let mut target = stale_target("ssh://ended-host");
    target.pane_id = Some(PaneId::parse("pane-ended").expect("valid fixture pane ID"));
    target.tab_id = Some(TabId::parse("tab-ended").expect("valid fixture tab ID"));
    target.binding_id = None;
    let original_target = target.clone();
    let old_target_id = target
        .terminal_session_id
        .clone()
        .expect("legacy target retains its exact terminal identity");
    let session = agent_with_history_binding(id.clone(), AgentSessionStatus::Stale, target);
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        session,
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions.clone());
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let first = service
        .target_context(id.clone(), 1024, CancellationToken::new())
        .await
        .expect("legacy context should be repaired using the session revision CAS");
    let first_binding_id = first
        .target
        .binding
        .binding_id
        .clone()
        .expect("resolved target projection has a stable ID");
    assert!(!first_binding_id.trim().is_empty());
    assert_eq!(
        first.target.status,
        kerminal_domain::agents::AgentTargetLiveStatus::Stale
    );
    assert_eq!(
        first.target.binding.terminal_session_id.as_ref(),
        Some(&old_target_id),
        "a missing old terminal stays stale and is never rebound"
    );
    let mut expected_target = original_target;
    expected_target.binding_id = Some(first_binding_id.clone());
    assert_eq!(
        first.target.binding, expected_target,
        "repair may add only the binding ID, preserving the original target relation"
    );
    assert!(first.snapshot.is_none());
    assert_eq!(
        first
            .record
            .target_binding
            .as_ref()
            .map(|context| context.binding.binding_id.as_deref()),
        Some(Some(first_binding_id.as_str()))
    );

    let endpoint_context = workspace
        .context_document(&id, AgentWorkspaceDocumentKind::McpEndpointContext)
        .expect("endpoint context was persisted");
    let target_context = workspace
        .context_document(&id, AgentWorkspaceDocumentKind::TargetBindingContext)
        .expect("target context was persisted");
    let terminal_context = workspace
        .context_document(&id, AgentWorkspaceDocumentKind::TerminalSnapshotContext)
        .expect("terminal snapshot context was persisted");
    let endpoint_context: serde_json::Value =
        serde_json::from_str(&endpoint_context).expect("endpoint context is valid JSON");
    let target_context: serde_json::Value =
        serde_json::from_str(&target_context).expect("target context is valid JSON");
    let terminal_context: serde_json::Value =
        serde_json::from_str(&terminal_context).expect("terminal context is valid JSON");
    assert_eq!(endpoint_context["agentSessionId"], id.as_str());
    assert_eq!(target_context["agentSessionId"], id.as_str());
    assert_eq!(terminal_context["agentSessionId"], id.as_str());
    assert_eq!(target_context["binding"]["bindingId"], first_binding_id);
    assert_eq!(
        target_context["binding"]["targetTerminalSessionId"],
        old_target_id.as_str()
    );
    assert_eq!(
        terminal_context["targetTerminalSessionId"],
        old_target_id.as_str()
    );

    let second = service
        .target_context(id.clone(), 1024, CancellationToken::new())
        .await
        .expect("repeated context reads reuse the repaired binding");
    assert_eq!(
        second.target.binding.binding_id.as_deref(),
        Some(first_binding_id.as_str())
    );
    assert_eq!(
        workspace
            .stored_session(&id)
            .target
            .as_ref()
            .and_then(|binding| binding.binding_id.as_deref()),
        Some(first_binding_id.as_str())
    );
    assert_eq!(
        sessions.create_count(),
        0,
        "context reads never start a CLI"
    );
    assert!(sessions.created_session_ids().is_empty());
}

/// Same-generation replays preserve identity; older positive generations are rejected without changing the binding.
///
/// @author kongweiguang
#[tokio::test]
async fn target_binding_retry_keeps_identity_and_rejects_older_generation() {
    let id = AgentSessionId::new();
    let mut existing = stale_target("local://target-a");
    let current_generation = 9;
    existing.generation = current_generation;
    assert!(
        current_generation >= 2,
        "the older fixture generation must stay positive"
    );
    existing.binding_id = Some("binding-id-stable-9".to_owned());
    let workspace = Arc::new(MemoryAgentWorkspace::new(
        agent_with_history_binding(id.clone(), AgentSessionStatus::Stale, existing.clone()),
        codex_provider_session(),
        CommitBehavior::Accept,
    ));
    let sessions = Arc::new(MemorySessions::new(
        Vec::new(),
        TEST_SESSION_GENERATION,
        false,
    ));
    let service = agent_service(workspace.clone(), sessions);
    *service.endpoint.write().await = Some(TEST_AGENT_ENDPOINT.to_owned());

    let replay = service
        .bind_preferred_target(
            id.clone(),
            {
                let mut binding = existing.clone();
                binding.binding_id = None;
                binding
            },
            CancellationToken::new(),
        )
        .await
        .expect("same relation can be retried after an uncertain caller result");
    assert_eq!(
        replay
            .session
            .target
            .as_ref()
            .and_then(|binding| binding.binding_id.as_deref()),
        Some("binding-id-stable-9")
    );
    assert_eq!(
        workspace
            .stored_session(&id)
            .target
            .as_ref()
            .and_then(|binding| binding.binding_id.as_deref()),
        Some("binding-id-stable-9")
    );

    let expected_current = existing.clone();
    let mut stale_generation = existing;
    stale_generation.binding_id = None;
    stale_generation.generation = current_generation - 1;
    assert!(stale_generation.generation > 0);
    assert!(stale_generation.generation < current_generation);
    let error = service
        .bind_preferred_target(id.clone(), stale_generation, CancellationToken::new())
        .await
        .expect_err("a lower generation cannot overwrite the current relation");
    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
    assert_eq!(
        error.public_message(),
        "Agent target binding generation is stale"
    );
    let stored = workspace.stored_session(&id);
    assert_eq!(stored.target.as_ref(), Some(&expected_current));
    assert_eq!(
        stored.target.as_ref().map(|binding| binding.generation),
        Some(current_generation)
    );
    assert_eq!(
        stored
            .target
            .as_ref()
            .and_then(|binding| binding.binding_id.as_deref()),
        Some("binding-id-stable-9")
    );
}
