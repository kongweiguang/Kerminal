//! Agent 会话文件存储与可恢复的受限文档提交。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    fs::{self as sync_fs, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
};

#[cfg(not(windows))]
use std::fs::File;

use kerminal_application::{
    AppError, AppResult,
    agents::{
        AgentProviderSessionSnapshot, AgentProviderSessionStatus, AgentResumeCheckpoint,
        AgentResumeCheckpointPhase, AgentResumeCheckpointSnapshot, AgentWorkspaceDocumentKind,
        AgentWorkspaceRevision, AgentWorkspaceWriteReceipt, CommitAgentResumeCheckpointRequest,
        PrepareAgentResumeCheckpointRequest,
    },
};
use kerminal_domain::{
    agents::{
        AgentLaunchPermissionMode, AgentLauncherKey, AgentProvider, AgentProviderSession,
        AgentSession, AgentSessionLaunch, AgentSessionScope, AgentSessionStatus,
        AgentTargetBinding, AgentTargetLiveStatus,
    },
    identity::{AgentSessionId, PaneId, SessionId, TabId},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::fs;
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, StorageCoordinator, commit_staged_file};

const SESSION_DIRECTORY: &str = "agents/sessions";
const JOURNAL_FILE: &str = ".agent-workspace-transaction.json";
const JOURNAL_SCHEMA_VERSION: u32 = 1;
const RESUME_CHECKPOINT_FILE: &str = ".agent-resume-checkpoint.json";
const RESUME_CHECKPOINT_SCHEMA_VERSION: u32 = 1;

/// 将一个会话工作区限制在 Kerminal 配置根内。
#[derive(Clone)]
pub(super) struct AgentSessionStore {
    workspace_root: PathBuf,
    sessions_root: PathBuf,
    storage: StorageCoordinator,
}

/// 已校验的固定 kind、目标路径、正文与读取时 CAS 基线。
pub(super) struct AgentWorkspaceFile {
    pub kind: AgentWorkspaceFileKind,
    pub path: PathBuf,
    pub contents: Vec<u8>,
    pub expected_revision: AgentWorkspaceRevision,
}

/// 内部文档类型让恢复 checkpoint 复用受限 journal，而不伪装成用户可编辑的 workspace 文档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum AgentWorkspaceFileKind {
    Document(AgentWorkspaceDocumentKind),
    ResumeCheckpoint,
}

/// checkpoint 文件只保留 attempt 身份与原始字节 revision，不复制 session/provider 启动数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AgentResumeCheckpointFile {
    schema_version: u32,
    agent_session_id: AgentSessionId,
    attempt_id: Uuid,
    terminal_session_id: SessionId,
    base_session_revision: Option<String>,
    base_provider_revision: Option<String>,
    intended_session_revision: String,
    intended_provider_revision: String,
    phase: AgentResumeCheckpointFilePhase,
}

/// 磁盘阶段只含两种可核对事实；Unknown 留在业务投影，不序列化为误导性的稳定状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum AgentResumeCheckpointFilePhase {
    Prepared,
    Committed,
}

impl AgentResumeCheckpointFile {
    /// 仅把已经由 typed encoder 计算的 revision 写入 marker，禁止持久化任何恢复命令或正文。
    ///
    /// @author kongweiguang
    fn from_checkpoint(
        agent_session_id: &AgentSessionId,
        checkpoint: AgentResumeCheckpoint,
    ) -> AppResult<Self> {
        Ok(Self {
            schema_version: RESUME_CHECKPOINT_SCHEMA_VERSION,
            agent_session_id: agent_session_id.clone(),
            attempt_id: checkpoint.attempt_id,
            terminal_session_id: checkpoint.terminal_session_id,
            base_session_revision: checkpoint_revision_hash(
                &checkpoint.base_session_revision,
                true,
            )?,
            base_provider_revision: checkpoint_revision_hash(
                &checkpoint.base_provider_revision,
                true,
            )?,
            intended_session_revision: checkpoint_revision_hash(
                &checkpoint.intended_session_revision,
                false,
            )?
            .ok_or_else(|| AppError::invalid("Agent resume session intent is missing"))?,
            intended_provider_revision: checkpoint_revision_hash(
                &checkpoint.intended_provider_revision,
                false,
            )?
            .ok_or_else(|| AppError::invalid("Agent resume provider intent is missing"))?,
            phase: match checkpoint.phase {
                AgentResumeCheckpointPhase::Prepared => AgentResumeCheckpointFilePhase::Prepared,
                AgentResumeCheckpointPhase::Committed => AgentResumeCheckpointFilePhase::Committed,
            },
        })
    }

    /// 把磁盘 schema 与固定路径会话归属校验后转换成无正文公开快照。
    ///
    /// @author kongweiguang
    fn to_checkpoint(&self, agent_session_id: &AgentSessionId) -> AppResult<AgentResumeCheckpoint> {
        if self.schema_version != RESUME_CHECKPOINT_SCHEMA_VERSION
            || self.agent_session_id != *agent_session_id
            || !valid_sha256(&self.intended_session_revision)
            || !valid_sha256(&self.intended_provider_revision)
        {
            return Err(AppError::configuration_corrupt("Agent resume checkpoint"));
        }
        Ok(AgentResumeCheckpoint {
            attempt_id: self.attempt_id,
            terminal_session_id: self.terminal_session_id.clone(),
            base_session_revision: checkpoint_revision(self.base_session_revision.as_deref())?,
            base_provider_revision: checkpoint_revision(self.base_provider_revision.as_deref())?,
            intended_session_revision: AgentWorkspaceRevision::Sha256(
                self.intended_session_revision.clone(),
            ),
            intended_provider_revision: AgentWorkspaceRevision::Sha256(
                self.intended_provider_revision.clone(),
            ),
            phase: match self.phase {
                AgentResumeCheckpointFilePhase::Prepared => AgentResumeCheckpointPhase::Prepared,
                AgentResumeCheckpointFilePhase::Committed => AgentResumeCheckpointPhase::Committed,
            },
        })
    }

    /// 比较请求和同一 attempt 的 marker 内容，不能只凭随机 UUID 将不同意图当作幂等重试。
    ///
    /// @author kongweiguang
    fn matches_snapshot(&self, snapshot: &AgentResumeCheckpointSnapshot) -> bool {
        self.attempt_id == snapshot.checkpoint.attempt_id
            && self.terminal_session_id == snapshot.checkpoint.terminal_session_id
            && checkpoint_revision_hash(&snapshot.checkpoint.base_session_revision, true).ok()
                == Some(self.base_session_revision.clone())
            && checkpoint_revision_hash(&snapshot.checkpoint.base_provider_revision, true).ok()
                == Some(self.base_provider_revision.clone())
            && checkpoint_revision_hash(&snapshot.checkpoint.intended_session_revision, false).ok()
                == Some(Some(self.intended_session_revision.clone()))
            && checkpoint_revision_hash(&snapshot.checkpoint.intended_provider_revision, false).ok()
                == Some(Some(self.intended_provider_revision.clone()))
            && matches!(
                (self.phase, snapshot.checkpoint.phase),
                (
                    AgentResumeCheckpointFilePhase::Prepared,
                    AgentResumeCheckpointPhase::Prepared
                ) | (
                    AgentResumeCheckpointFilePhase::Committed,
                    AgentResumeCheckpointPhase::Committed
                )
            )
    }
}

/// 把 Missing 与 SHA-256 revision 显式映射到可序列化字段，拒绝携带任意或格式错误值。
///
/// @author kongweiguang
fn checkpoint_revision_hash(
    revision: &AgentWorkspaceRevision,
    allow_missing: bool,
) -> AppResult<Option<String>> {
    match revision {
        AgentWorkspaceRevision::Missing if allow_missing => Ok(None),
        AgentWorkspaceRevision::Sha256(hash) if valid_sha256(hash) => Ok(Some(hash.clone())),
        _ => Err(AppError::invalid(
            "Agent resume checkpoint revision is invalid",
        )),
    }
}

/// 从 checkpoint 的可选 hash 恢复 CAS 语义；任何非 SHA-256 内容都视为损坏状态。
///
/// @author kongweiguang
fn checkpoint_revision(value: Option<&str>) -> AppResult<AgentWorkspaceRevision> {
    match value {
        None => Ok(AgentWorkspaceRevision::Missing),
        Some(hash) if valid_sha256(hash) => Ok(AgentWorkspaceRevision::Sha256(hash.to_owned())),
        Some(_) => Err(AppError::configuration_corrupt("Agent resume checkpoint")),
    }
}

/// 以稳定 JSON 编码极小 metadata marker；私有键只含 ID、阶段与已验证摘要。
///
/// @author kongweiguang
fn encode_resume_checkpoint_file(file: &AgentResumeCheckpointFile) -> AppResult<Vec<u8>> {
    serde_json::to_vec(file).map_err(|error| {
        AppError::infrastructure_with_source("Agent resume checkpoint encoding failed", error)
    })
}

/// 验证固定 checkpoint schema、会话归属和所有哈希后生成与原字节绑定的 snapshot。
///
/// @author kongweiguang
fn decode_resume_checkpoint_snapshot(
    bytes: &[u8],
    agent_session_id: &AgentSessionId,
) -> AppResult<AgentResumeCheckpointSnapshot> {
    let file = serde_json::from_slice::<AgentResumeCheckpointFile>(bytes).map_err(|error| {
        AppError::configuration_corrupt("Agent resume checkpoint").with_source(error)
    })?;
    let checkpoint = file.to_checkpoint(agent_session_id)?;
    Ok(AgentResumeCheckpointSnapshot {
        checkpoint,
        revision: revision_for_bytes(bytes),
    })
}

/// 只从固定 checkpoint 文件读取并解码，不把损坏或未来 schema 当作 absence。
///
/// @author kongweiguang
fn read_resume_checkpoint_snapshot(
    root: &Path,
    session_root: &Path,
    agent_session_id: &AgentSessionId,
) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
    let path = session_root.join(RESUME_CHECKPOINT_FILE);
    read_optional_file(root, session_root, &path)?
        .map(|bytes| decode_resume_checkpoint_snapshot(&bytes, agent_session_id))
        .transpose()
}

/// session.toml 的显式边界 DTO；禁止未知根字段绕过 schema 版本校验。
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionFile {
    schema_version: u32,
    agent_session_id: AgentSessionId,
    agent_id: AgentProvider,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launcher_key: Option<AgentLauncherKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    permission_mode: Option<AgentLaunchPermissionMode>,
    title: String,
    created_at: String,
    updated_at: String,
    status: AgentSessionStatus,
    workspace_root: String,
    session_root: String,
    launch: AgentSessionLaunch,
    scope: AgentSessionScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target: Option<AgentSessionTargetFile>,
}

impl AgentSessionFile {
    /// 反序列化后仍经领域恢复入口校验 schema、状态和 launch spec。
    fn restore(self) -> AppResult<AgentSession> {
        let permission_mode = self.permission_mode;
        let session = AgentSession::restore(
            self.schema_version,
            self.agent_session_id,
            self.agent_id,
            self.launcher_key,
            self.title,
            self.created_at,
            self.updated_at,
            self.status,
            self.workspace_root,
            self.session_root,
            self.launch,
            Some(self.scope),
            self.target.map(AgentTargetBinding::from),
        )
        .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
        if permission_mode.is_some_and(|mode| mode != session.permission_mode) {
            return Err(AppError::configuration_corrupt(
                "Agent session permission mode does not match launch arguments",
            ));
        }
        Ok(session)
    }

    /// 当前格式必须保存归属；拒绝把合法的短暂未绑定领域对象写成不可恢复文件。
    fn from_session(session: AgentSession) -> AppResult<Self> {
        let scope = session.scope.clone().ok_or_else(|| {
            AppError::invalid("Agent session scope is required before persistence")
        })?;
        Ok(Self {
            schema_version: session.schema_version,
            agent_session_id: session.agent_session_id,
            agent_id: session.agent_id,
            launcher_key: session.launcher_key,
            permission_mode: Some(session.permission_mode),
            title: session.title,
            created_at: session.created_at,
            updated_at: session.updated_at,
            status: session.status,
            workspace_root: session.workspace_root,
            session_root: session.session_root,
            launch: session.launch,
            scope,
            target: session.target.map(AgentSessionTargetFile::from),
        })
    }
}

/// 由唯一当前边界 DTO 编码 session 文件，保证 application 不复制持久化字段契约。
pub(super) fn encode_agent_session_document(session: &AgentSession) -> AppResult<Vec<u8>> {
    let document = AgentSessionFile::from_session(session.clone())?;
    toml::to_string_pretty(&document)
        .map(String::into_bytes)
        .map_err(|error| {
            AppError::infrastructure_with_source("Agent session encoding failed", error)
        })
}

/// 在基础设施持久化边界校验并编码 provider.toml，避免业务层直接依赖 TOML 表层级。
pub(super) fn encode_agent_provider_document(
    provider: &AgentProviderSession,
) -> AppResult<Vec<u8>> {
    provider.validate().map_err(|error| {
        AppError::invalid("Agent provider session schema is not supported").with_source(error)
    })?;
    toml::to_string_pretty(provider)
        .map(String::into_bytes)
        .map_err(|error| {
            AppError::infrastructure_with_source("Agent provider session encoding failed", error)
        })
}

/// 当前 session.toml target 的严格边界 DTO；旧字段别名只在一次性迁移中处理。
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AgentSessionTargetFile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    binding_id: Option<String>,
    #[serde(default, rename = "binding_generation")]
    generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pane_id: Option<PaneId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tab_id: Option<TabId>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "target_terminal_session_id"
    )]
    terminal_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    shell: Option<String>,
    live_status: AgentTargetLiveStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_seen_at: Option<String>,
}

impl From<AgentSessionTargetFile> for AgentTargetBinding {
    /// 将规范持久字段映射到领域绑定，完整保留 generation 与显式终端身份。
    ///
    /// @author kongweiguang
    fn from(target: AgentSessionTargetFile) -> Self {
        Self {
            binding_id: target.binding_id,
            generation: target.generation,
            pane_id: target.pane_id,
            tab_id: target.tab_id,
            terminal_session_id: target.terminal_session_id,
            target_ref: target.target_ref,
            target_kind: target.target_kind,
            cwd: target.cwd,
            shell: target.shell,
            live_status: target.live_status,
            last_seen_at: target.last_seen_at,
        }
    }
}

impl From<AgentTargetBinding> for AgentSessionTargetFile {
    /// 将领域目标映射到当前持久化字段，保留稳定绑定版本号。
    fn from(target: AgentTargetBinding) -> Self {
        Self {
            binding_id: target.binding_id,
            generation: target.generation,
            pane_id: target.pane_id,
            tab_id: target.tab_id,
            terminal_session_id: target.terminal_session_id,
            target_ref: target.target_ref,
            target_kind: target.target_kind,
            cwd: target.cwd,
            shell: target.shell,
            live_status: target.live_status,
            last_seen_at: target.last_seen_at,
        }
    }
}

impl AgentSessionStore {
    /// 把正式配置根绑定到固定 sessions 子目录，避免会话路径由调用方拼接。
    pub(super) fn new(workspace_root: PathBuf) -> Self {
        let storage = StorageCoordinator::new(workspace_root.clone());
        Self::with_storage_coordinator(workspace_root, storage)
    }

    /// 共用应用已装配的用户数据锁，使 Agent journal 与配置、vault 等持久化写入串行。
    pub(super) fn with_storage_coordinator(
        workspace_root: PathBuf,
        storage: StorageCoordinator,
    ) -> Self {
        let sessions_root = workspace_root.join(SESSION_DIRECTORY);
        Self {
            workspace_root,
            sessions_root,
            storage,
        }
    }

    /// 返回配置根的稳定路径表示，供 session 元数据持久化。
    pub(super) fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    /// 返回经过路径组件校验的会话目录，不接受分隔符或相对路径 ID。
    pub(super) fn session_root(&self, id: &AgentSessionId) -> AppResult<PathBuf> {
        validate_path_id(id)?;
        Ok(self.sessions_root.join(id.as_str()))
    }

    /// 创建全新 session 目录；UUID 碰撞按冲突处理，不复用已有用户内容。
    pub(super) async fn create_session_root(&self, id: &AgentSessionId) -> AppResult<PathBuf> {
        let path = self.session_root(id)?;
        fs::create_dir_all(&self.workspace_root)
            .await
            .map_err(io_error)?;
        let canonical_root = fs::canonicalize(&self.workspace_root)
            .await
            .map_err(io_error)?;
        self.create_contained_parent(&path, &canonical_root).await?;
        fs::create_dir(&path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                AppError::conflict("Agent session directory already exists")
            } else {
                io_error(error)
            }
        })?;
        Ok(path)
    }

    /// 只回滚本次独占创建的目录；删除前先恢复 journal，恢复冲突时保留全部证据。
    pub(super) async fn remove_created_session_root(&self, id: &AgentSessionId) -> AppResult<()> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.remove_created_session_root_blocking(&id))
            .await
            .map_err(join_error)?
    }

    /// 按路径安全检查读取并恢复一个 session.toml。
    pub(super) async fn read_session(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<(AgentSession, AgentWorkspaceRevision)> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.read_session_blocking(&id))
            .await
            .map_err(join_error)?
    }

    /// 读取 provider.toml 及其 session 归属，坏版本分类返回而不自动修复文件。
    pub(super) async fn read_provider_session(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<AgentProviderSessionSnapshot> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.read_provider_session_blocking(&id))
            .await
            .map_err(join_error)?
    }

    /// 在任何 CLI/PTY 创建前原子写入单文件 checkpoint，并在同一存储锁内核对 session/provider 基线。
    ///
    /// @author kongweiguang
    pub(super) async fn prepare_resume_checkpoint(
        &self,
        request: PrepareAgentResumeCheckpointRequest,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.prepare_resume_checkpoint_blocking(request))
            .await
            .map_err(join_error)?
    }

    /// 按原 attempt typed 意图复用 workspace journal，Committed marker 始终为该事务最后一项。
    ///
    /// @author kongweiguang
    pub(super) async fn commit_resume_checkpoint(
        &self,
        request: CommitAgentResumeCheckpointRequest,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.commit_resume_checkpoint_blocking(request))
            .await
            .map_err(join_error)?
    }

    /// 在同一锁内先恢复现有 journal，再读取当前受管 checkpoint 与其原始字节 revision。
    ///
    /// @author kongweiguang
    pub(super) async fn read_resume_checkpoint(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.read_resume_checkpoint_blocking(&id))
            .await
            .map_err(join_error)?
    }

    /// 只按 snapshot 的 attempt 与原始 checkpoint revision 做 CAS 删除，不自行推断 terminal 是否安全关闭。
    ///
    /// @author kongweiguang
    pub(super) async fn clear_resume_checkpoint(
        &self,
        id: &AgentSessionId,
        expected: AgentResumeCheckpointSnapshot,
    ) -> AppResult<()> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.clear_resume_checkpoint_blocking(&id, &expected))
            .await
            .map_err(join_error)?
    }

    /// 受同一 storage lock 保护地恢复待提交 journal，再读取正文和精确磁盘 revision。
    pub(super) async fn read_document(
        &self,
        id: &AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        path: PathBuf,
    ) -> AppResult<(Option<String>, AgentWorkspaceRevision)> {
        let store = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || store.read_document_blocking(&id, kind, &path))
            .await
            .map_err(join_error)?
    }

    /// 枚举已保存会话并逐一执行 schema 恢复校验，损坏文件不会静默变成空历史。
    pub(super) async fn list_sessions(
        &self,
    ) -> AppResult<Vec<(AgentSession, AgentWorkspaceRevision)>> {
        if let Err(error) = self
            .ensure_directory_is_contained(&self.sessions_root)
            .await
        {
            if error.public_code() == kerminal_application::PublicErrorCode::NotFound {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        let mut entries = match fs::read_dir(&self.sessions_root).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(io_error(error)),
        };
        let mut sessions = Vec::new();
        while let Some(entry) = entries.next_entry().await.map_err(io_error)? {
            let entry_type = entry.file_type().await.map_err(io_error)?;
            if entry_type.is_symlink() {
                return Err(AppError::configuration_corrupt("Agent session directory"));
            }
            if !entry_type.is_dir() {
                continue;
            }
            let Some(raw_id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(id) = AgentSessionId::parse(&raw_id) else {
                continue;
            };
            if validate_path_id(&id).is_err() {
                continue;
            }
            match self.read_session(&id).await {
                Ok((session, revision)) => sessions.push((session, revision)),
                Err(error)
                    if error.public_code() == kerminal_application::PublicErrorCode::NotFound =>
                {
                    return Err(AppError::configuration_corrupt("Agent session"));
                }
                Err(error) => return Err(error),
            }
        }
        sessions.sort_by(|left, right| right.0.updated_at.cmp(&left.0.updated_at));
        Ok(sessions)
    }

    /// 在单一阻塞闭包内加共享锁、恢复旧事务并执行可恢复的 journal 提交。
    pub(super) async fn write_files(
        &self,
        id: AgentSessionId,
        files: Vec<AgentWorkspaceFile>,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        let store = self.clone();
        tokio::task::spawn_blocking(move || store.write_files_blocking(&id, files))
            .await
            .map_err(join_error)?
    }

    /// 在同一共享锁内先解决待恢复提交，再删除只由本次创建的会话目录。
    fn remove_created_session_root_blocking(&self, id: &AgentSessionId) -> AppResult<()> {
        let Some(root) = self.canonical_root_if_present()? else {
            return Ok(());
        };
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        match sync_fs::symlink_metadata(&session_root) {
            Ok(_) => ensure_existing_session_directory(&root, &session_root)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                return Err(io_error_with_context(
                    "Agent session directory inspection failed",
                    error,
                ));
            }
        }
        recover_workspace_transaction(&root, &session_root, id)?;
        sync_fs::remove_dir_all(&session_root)
            .map_err(|error| io_error_with_context("Agent workspace rollback failed", error))?;
        if let Some(parent) = session_root.parent() {
            sync_directory(parent).map_err(|error| {
                AppError::outcome_unknown_with_source(
                    "Agent workspace rollback durability is unresolved",
                    error,
                )
            })?;
        }
        Ok(())
    }

    /// 原始 session 读取必须与 journal 恢复在同一锁内完成，revision 对应返回的字节。
    fn read_session_blocking(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<(AgentSession, AgentWorkspaceRevision)> {
        let root = self
            .canonical_root_if_present()?
            .ok_or_else(|| AppError::not_found("Agent session not found"))?;
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, id)?;
        let metadata_path = session_root.join("session.toml");
        let bytes = read_required_file(&root, &session_root, &metadata_path)?;
        let revision = revision_for_bytes(&bytes);
        let text = std::str::from_utf8(&bytes)
            .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
        let file = toml::from_str::<AgentSessionFile>(text)
            .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
        let session = file.restore()?;
        if session.agent_session_id != *id {
            return Err(AppError::configuration_corrupt("Agent session"));
        }
        Ok((session, revision))
    }

    /// 在同一锁内读取 session 与 provider，确保状态、路径和 revision 指向同一磁盘快照。
    fn read_provider_session_blocking(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<AgentProviderSessionSnapshot> {
        let root = self
            .canonical_root_if_present()?
            .ok_or_else(|| AppError::not_found("Agent session not found"))?;
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, id)?;

        let session_path = session_root.join("session.toml");
        let session_bytes = read_required_file(&root, &session_root, &session_path)?;
        let session_text = std::str::from_utf8(&session_bytes)
            .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
        let session_file = toml::from_str::<AgentSessionFile>(session_text)
            .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
        let session = session_file.restore()?;
        if session.agent_session_id != *id {
            return Err(AppError::configuration_corrupt("Agent session"));
        }

        let path = session_root.join("provider.toml");
        let Some(bytes) = read_optional_file(&root, &session_root, &path)? else {
            return Ok(AgentProviderSessionSnapshot {
                status: AgentProviderSessionStatus::Missing,
                revision: AgentWorkspaceRevision::Missing,
                path: path.to_string_lossy().into_owned(),
            });
        };
        let revision = revision_for_bytes(&bytes);
        let status = match std::str::from_utf8(&bytes) {
            Err(_) => AgentProviderSessionStatus::Malformed,
            Ok(text) => classify_provider_session(text, &session.agent_id),
        };
        Ok(AgentProviderSessionSnapshot {
            status,
            revision,
            path: path.to_string_lossy().into_owned(),
        })
    }

    /// 文档快照采用同一锁恢复事务并读取原始内容，避免正文与 CAS revision 来自不同版本。
    fn read_document_blocking(
        &self,
        id: &AgentSessionId,
        kind: AgentWorkspaceDocumentKind,
        path: &Path,
    ) -> AppResult<(Option<String>, AgentWorkspaceRevision)> {
        let Some(root) = self.canonical_root_if_present()? else {
            return Ok((None, AgentWorkspaceRevision::Missing));
        };
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        match sync_fs::symlink_metadata(&session_root) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok((None, AgentWorkspaceRevision::Missing));
            }
            Err(error) => {
                return Err(io_error_with_context(
                    "Agent session directory inspection failed",
                    error,
                ));
            }
        }
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, id)?;
        validate_document_path(&session_root, kind, path)?;
        let Some(bytes) = read_optional_file(&root, &session_root, path)? else {
            return Ok((None, AgentWorkspaceRevision::Missing));
        };
        let revision = revision_for_bytes(&bytes);
        let text = String::from_utf8(bytes).map_err(|error| {
            AppError::configuration_corrupt("Agent document").with_source(error)
        })?;
        Ok((Some(text), revision))
    }

    /// 锁内核对 base revisions 并以 CreateNew 安装短 metadata marker；未知落盘结果不允许启动 CLI。
    ///
    /// @author kongweiguang
    fn prepare_resume_checkpoint_blocking(
        &self,
        request: PrepareAgentResumeCheckpointRequest,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let root = self.canonical_root_for_write()?;
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(&request.agent_session_id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, &request.agent_session_id)?;

        let session_path = session_root.join("session.toml");
        let session_bytes = read_required_file(&root, &session_root, &session_path)?;
        let session_revision = revision_for_bytes(&session_bytes);
        let provider_path = session_root.join("provider.toml");
        let provider_bytes = read_optional_file(&root, &session_root, &provider_path)?;
        let provider_revision = provider_bytes
            .as_deref()
            .map(revision_for_bytes)
            .unwrap_or(AgentWorkspaceRevision::Missing);
        if session_revision != request.base_session_revision
            || provider_revision != request.base_provider_revision
        {
            return Err(AppError::conflict(
                "Agent resume metadata changed before its checkpoint was prepared",
            ));
        }

        let mut intended = vec![
            AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Session),
                path: session_path,
                contents: encode_agent_session_document(&request.intended_session)?,
                expected_revision: request.base_session_revision.clone(),
            },
            AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Provider),
                path: provider_path,
                contents: encode_agent_provider_document(&request.intended_provider)?,
                expected_revision: request.base_provider_revision.clone(),
            },
        ];
        normalize_and_validate_agent_documents(
            &root,
            &session_root,
            &request.agent_session_id,
            &mut intended,
        )?;
        let intended_session_revision = revision_for_bytes(&intended[0].contents);
        let intended_provider_revision = revision_for_bytes(&intended[1].contents);
        let checkpoint_file = AgentResumeCheckpointFile::from_checkpoint(
            &request.agent_session_id,
            AgentResumeCheckpoint {
                attempt_id: request.attempt_id,
                terminal_session_id: request.terminal_session_id,
                base_session_revision: request.base_session_revision.clone(),
                base_provider_revision: request.base_provider_revision.clone(),
                intended_session_revision,
                intended_provider_revision,
                phase: AgentResumeCheckpointPhase::Prepared,
            },
        )?;
        let checkpoint_bytes = encode_resume_checkpoint_file(&checkpoint_file)?;
        let checkpoint_path = session_root.join(RESUME_CHECKPOINT_FILE);

        if let Some(existing) = read_optional_file(&root, &session_root, &checkpoint_path)? {
            let snapshot = decode_resume_checkpoint_snapshot(&existing, &request.agent_session_id)?;
            if checkpoint_file.matches_snapshot(&snapshot)
                && snapshot.checkpoint.phase == AgentResumeCheckpointPhase::Prepared
            {
                return Ok(snapshot);
            }
            return Err(AppError::conflict(
                "Another Agent resume checkpoint already exists",
            ));
        }

        ensure_parent_sync(&root, &session_root, &checkpoint_path)?;
        let stage = stage_path_for(&checkpoint_path, &request.attempt_id)?;
        if let Some(existing_stage) = read_optional_file(&root, &session_root, &stage)? {
            if existing_stage != checkpoint_bytes {
                return Err(AppError::conflict(
                    "Agent resume checkpoint retry does not match its prepared staging file",
                ));
            }
        } else {
            write_new_synced_file(&stage, &checkpoint_bytes).map_err(|error| {
                io_error_with_context("Agent resume checkpoint staging failed", error)
            })?;
            if let Some(parent) = stage.parent() {
                sync_directory(parent).map_err(|error| {
                    io_error_with_context(
                        "Agent resume checkpoint staging durability failed",
                        error,
                    )
                })?;
            }
        }
        let latest_session =
            read_required_file(&root, &session_root, &session_root.join("session.toml"))?;
        let latest_provider =
            read_optional_file(&root, &session_root, &session_root.join("provider.toml"))?
                .as_deref()
                .map(revision_for_bytes)
                .unwrap_or(AgentWorkspaceRevision::Missing);
        if revision_for_bytes(&latest_session) != request.base_session_revision
            || latest_provider != request.base_provider_revision
        {
            remove_stage_if_matching(&root, &session_root, &stage, &sha256_hex(&checkpoint_bytes))?;
            return Err(AppError::conflict(
                "Agent resume metadata changed before its checkpoint was prepared",
            ));
        }
        match commit_staged_file(&stage, &checkpoint_path, CommitMode::CreateNew) {
            Ok(()) => {}
            Err(error @ AtomicCommitError::CommitUnknown(_)) => {
                return Err(AppError::outcome_unknown_with_source(
                    "Agent resume checkpoint durability is unknown; CLI startup must not continue",
                    error,
                ));
            }
            Err(AtomicCommitError::NotCommitted(error)) => {
                if let Err(cleanup) = remove_prepared_stage(&stage) {
                    return Err(AppError::outcome_unknown_with_source(
                        "Agent resume checkpoint staging cleanup is unresolved",
                        cleanup,
                    ));
                }
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    return Err(AppError::conflict(
                        "Another Agent resume checkpoint already exists",
                    ));
                }
                return Err(io_error_with_context(
                    "Agent resume checkpoint commit failed",
                    error,
                ));
            }
        }
        let committed = read_optional_file(&root, &session_root, &checkpoint_path)
            .map_err(|error| {
                AppError::outcome_unknown_with_source(
                    "Agent resume checkpoint commit requires readback before CLI startup",
                    error,
                )
            })?
            .ok_or_else(|| {
                AppError::outcome_unknown(
                    "Agent resume checkpoint commit requires readback before CLI startup",
                )
            })?;
        if committed != checkpoint_bytes {
            return Err(AppError::outcome_unknown(
                "Agent resume checkpoint differs from its prepared intent",
            ));
        }
        decode_resume_checkpoint_snapshot(&committed, &request.agent_session_id)
    }

    /// 只提交与 durable Prepared attempt 完全匹配的 typed 意图，且在同一锁内 CAS 原始 session/provider。
    ///
    /// @author kongweiguang
    fn commit_resume_checkpoint_blocking(
        &self,
        request: CommitAgentResumeCheckpointRequest,
    ) -> AppResult<AgentResumeCheckpointSnapshot> {
        let root = self.canonical_root_for_write()?;
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(&request.agent_session_id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, &request.agent_session_id)?;
        let current =
            read_resume_checkpoint_snapshot(&root, &session_root, &request.agent_session_id)?
                .ok_or_else(|| AppError::conflict("Agent resume checkpoint no longer exists"))?;
        if current != request.prepared
            || current.checkpoint.phase != AgentResumeCheckpointPhase::Prepared
        {
            return Err(AppError::conflict(
                "Agent resume checkpoint changed before commit",
            ));
        }

        let session_bytes = encode_agent_session_document(&request.intended_session)?;
        let provider_bytes = encode_agent_provider_document(&request.intended_provider)?;
        let mut committed_checkpoint = current.checkpoint.clone();
        committed_checkpoint.phase = AgentResumeCheckpointPhase::Committed;
        let checkpoint_file = AgentResumeCheckpointFile::from_checkpoint(
            &request.agent_session_id,
            committed_checkpoint.clone(),
        )?;
        let checkpoint_bytes = encode_resume_checkpoint_file(&checkpoint_file)?;
        let files = vec![
            AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Session),
                path: session_root.join("session.toml"),
                contents: session_bytes,
                expected_revision: current.checkpoint.base_session_revision.clone(),
            },
            AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Provider),
                path: session_root.join("provider.toml"),
                contents: provider_bytes,
                expected_revision: current.checkpoint.base_provider_revision.clone(),
            },
            AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::ResumeCheckpoint,
                path: session_root.join(RESUME_CHECKPOINT_FILE),
                contents: checkpoint_bytes.clone(),
                expected_revision: current.revision,
            },
        ];
        self.write_files_locked(
            &root,
            &session_root,
            &request.agent_session_id,
            files,
            Some(current.checkpoint.attempt_id),
        )?;
        let committed =
            read_resume_checkpoint_snapshot(&root, &session_root, &request.agent_session_id)
                .map_err(|error| {
                    AppError::outcome_unknown_with_source(
                        "Agent resume checkpoint commit requires durable readback",
                        error,
                    )
                })?
                .ok_or_else(|| {
                    AppError::outcome_unknown(
                        "Agent resume checkpoint commit requires durable readback",
                    )
                })?;
        let expected_revision = revision_for_bytes(&checkpoint_bytes);
        if committed.checkpoint != committed_checkpoint || committed.revision != expected_revision {
            return Err(AppError::outcome_unknown(
                "Agent resume checkpoint commit differs from its typed intent",
            ));
        }
        Ok(committed)
    }

    /// 读取与清除共享的受锁 marker 解码，坏数据必须报告而不能伪装成“无待核对恢复”。
    ///
    /// @author kongweiguang
    fn read_resume_checkpoint_blocking(
        &self,
        id: &AgentSessionId,
    ) -> AppResult<Option<AgentResumeCheckpointSnapshot>> {
        let Some(root) = self.canonical_root_if_present()? else {
            return Ok(None);
        };
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        match sync_fs::symlink_metadata(&session_root) {
            Ok(_) => ensure_existing_session_directory(&root, &session_root)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(io_error_with_context(
                    "Agent session directory inspection failed",
                    error,
                ));
            }
        }
        recover_workspace_transaction(&root, &session_root, id)?;
        read_resume_checkpoint_snapshot(&root, &session_root, id)
    }

    /// 对当前 marker 执行 attempt 与原始 revision 双重 CAS，再同步目录删除结果供 application 核验。
    ///
    /// @author kongweiguang
    fn clear_resume_checkpoint_blocking(
        &self,
        id: &AgentSessionId,
        expected: &AgentResumeCheckpointSnapshot,
    ) -> AppResult<()> {
        let Some(root) = self.canonical_root_if_present()? else {
            return Err(AppError::conflict(
                "Agent resume checkpoint no longer exists",
            ));
        };
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, id)?;
        let current = read_resume_checkpoint_snapshot(&root, &session_root, id)?
            .ok_or_else(|| AppError::conflict("Agent resume checkpoint no longer exists"))?;
        if &current != expected || current.checkpoint.attempt_id != expected.checkpoint.attempt_id {
            return Err(AppError::conflict(
                "Agent resume checkpoint changed before cleanup",
            ));
        }
        let session_path = session_root.join("session.toml");
        let session_bytes = read_optional_file(&root, &session_root, &session_path)?
            .ok_or_else(|| AppError::conflict("Agent session changed before checkpoint cleanup"))?;
        let provider_path = session_root.join("provider.toml");
        let provider_revision = read_optional_file(&root, &session_root, &provider_path)?
            .as_deref()
            .map(revision_for_bytes)
            .unwrap_or(AgentWorkspaceRevision::Missing);
        let (expected_session_revision, expected_provider_revision) =
            match expected.checkpoint.phase {
                AgentResumeCheckpointPhase::Prepared => (
                    &expected.checkpoint.base_session_revision,
                    &expected.checkpoint.base_provider_revision,
                ),
                AgentResumeCheckpointPhase::Committed => (
                    &expected.checkpoint.intended_session_revision,
                    &expected.checkpoint.intended_provider_revision,
                ),
            };
        if &revision_for_bytes(&session_bytes) != expected_session_revision
            || &provider_revision != expected_provider_revision
        {
            return Err(AppError::conflict(
                "Agent session or provider changed before checkpoint cleanup",
            ));
        }
        let path = session_root.join(RESUME_CHECKPOINT_FILE);
        sync_fs::remove_file(&path).map_err(|error| {
            AppError::outcome_unknown_with_source(
                "Agent resume checkpoint cleanup is unresolved",
                error,
            )
        })?;
        sync_directory(&session_root).map_err(|error| {
            AppError::outcome_unknown_with_source(
                "Agent resume checkpoint cleanup durability is unresolved",
                error,
            )
        })
    }

    /// 在一个阻塞线程闭包内持锁完成 CAS、staging、journal 持久化和幂等前滚。
    /// before 哈希只取原始快照；未通过 CAS 不写 journal，也不启动 staging。
    ///
    /// @author kongweiguang
    fn write_files_blocking(
        &self,
        id: &AgentSessionId,
        files: Vec<AgentWorkspaceFile>,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let root = self.canonical_root_for_write()?;
        let _lock = self.storage.lock().map_err(storage_lock_error)?;
        let session_root = self.session_root(id)?;
        ensure_existing_session_directory(&root, &session_root)?;
        recover_workspace_transaction(&root, &session_root, id)?;
        self.write_files_locked(&root, &session_root, id, files, None)
    }

    /// 仅供已持有 storage lock 且已解决旧事务的调用者复用同一 journal/CAS 提交路径。
    ///
    /// @author kongweiguang
    fn write_files_locked(
        &self,
        root: &Path,
        session_root: &Path,
        id: &AgentSessionId,
        mut files: Vec<AgentWorkspaceFile>,
        resume_attempt_id: Option<Uuid>,
    ) -> AppResult<Vec<AgentWorkspaceWriteReceipt>> {
        if files.is_empty() {
            return Ok(Vec::new());
        }
        normalize_and_validate_agent_documents(root, session_root, id, &mut files)?;
        let mut kinds = HashSet::with_capacity(files.len());
        let mut entries = Vec::with_capacity(files.len());
        let mut contents = Vec::with_capacity(files.len());
        for file in files {
            if !kinds.insert(file.kind) {
                return Err(AppError::invalid(
                    "Agent workspace transaction contains a duplicate document kind",
                ));
            }
            validate_workspace_file_path(session_root, file.kind, &file.path)?;
            ensure_parent_sync(root, session_root, &file.path)?;
            let before = read_optional_file(root, session_root, &file.path)?;
            let actual_revision = before
                .as_deref()
                .map(sha256_revision)
                .unwrap_or(AgentWorkspaceRevision::Missing);
            if actual_revision != file.expected_revision {
                return Err(AppError::conflict(
                    "Agent workspace document changed since it was read",
                ));
            }
            let after_hash = sha256_hex(&file.contents);
            let stage = stage_path_for(&file.path, &Uuid::new_v4())?;
            entries.push(WorkspaceTransactionEntry {
                kind: workspace_file_kind_id(file.kind).to_owned(),
                target: relative_to_session(session_root, &file.path)?,
                staging: relative_to_session(session_root, &stage)?,
                before_sha256: before.as_deref().map(sha256_hex),
                after_sha256: after_hash,
            });
            contents.push(file.contents);
        }
        if let Some(attempt_id) = resume_attempt_id {
            Self::validate_resume_checkpoint_transaction(id, attempt_id, &entries, &contents)?;
        } else if entries
            .iter()
            .any(|entry| entry.kind == "resume_checkpoint")
        {
            return Err(AppError::invalid(
                "Agent resume checkpoint must commit with its matching attempt",
            ));
        }

        let mut journal = WorkspaceTransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_session_id: id.clone(),
            phase: WorkspaceTransactionPhase::Preparing,
            resume_attempt_id,
            entries,
        };
        write_journal(session_root, &journal, CommitMode::CreateNew)?;
        for (entry, bytes) in journal.entries.iter().zip(contents.iter()) {
            let stage = session_root.join(&entry.staging);
            if let Err(error) = write_new_synced_file(&stage, bytes) {
                return Err(io_error_with_context(
                    "Agent workspace staging failed",
                    error,
                ));
            }
            if let Some(parent) = stage.parent() {
                sync_directory(parent).map_err(|error| {
                    io_error_with_context("Agent workspace staging durability failed", error)
                })?;
            }
        }
        journal.phase = WorkspaceTransactionPhase::Prepared;
        write_journal(session_root, &journal, CommitMode::ReplaceExisting)?;
        recover_workspace_transaction(root, session_root, id)?;

        let mut receipts = Vec::with_capacity(journal.entries.len());
        for entry in journal.entries {
            if let Some(AgentWorkspaceFileKind::Document(kind)) =
                parse_workspace_file_kind(&entry.kind)
            {
                receipts.push(AgentWorkspaceWriteReceipt {
                    kind,
                    revision: AgentWorkspaceRevision::Sha256(entry.after_sha256),
                });
            }
        }
        Ok(receipts)
    }

    /// resume 三文档事务只接受原 attempt 的 Prepared 派生内容，确保 Committed marker 最后且能校验 intended bytes。
    ///
    /// @author kongweiguang
    fn validate_resume_checkpoint_transaction(
        id: &AgentSessionId,
        attempt_id: Uuid,
        entries: &[WorkspaceTransactionEntry],
        contents: &[Vec<u8>],
    ) -> AppResult<()> {
        if entries.len() != 3
            || contents.len() != 3
            || entries[0].kind != "session"
            || entries[1].kind != "provider"
            || entries[2].kind != "resume_checkpoint"
        {
            return Err(AppError::invalid(
                "Agent resume transaction must commit session, provider, then checkpoint",
            ));
        }
        let file =
            serde_json::from_slice::<AgentResumeCheckpointFile>(&contents[2]).map_err(|error| {
                AppError::invalid("Agent resume checkpoint intent is invalid").with_source(error)
            })?;
        let checkpoint = file.to_checkpoint(id)?;
        if checkpoint.attempt_id != attempt_id
            || checkpoint.phase != AgentResumeCheckpointPhase::Committed
            || checkpoint.intended_session_revision != revision_for_bytes(&contents[0])
            || checkpoint.intended_provider_revision != revision_for_bytes(&contents[1])
            || checkpoint.base_session_revision
                != checkpoint_revision(entries[0].before_sha256.as_deref())?
            || checkpoint.base_provider_revision
                != checkpoint_revision(entries[1].before_sha256.as_deref())?
        {
            return Err(AppError::invalid(
                "Agent resume checkpoint intent does not match its transaction",
            ));
        }
        Ok(())
    }

    /// 仅将实际用户数据根 canonicalize；读取不存在的配置根不能意外创建空目录。
    fn canonical_root_if_present(&self) -> AppResult<Option<PathBuf>> {
        match sync_fs::canonicalize(&self.workspace_root) {
            Ok(root) => Ok(Some(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error_with_context(
                "Agent workspace root inspection failed",
                error,
            )),
        }
    }

    /// 写入前建立数据根并返回 canonical identity，后续路径检查统一使用该根。
    fn canonical_root_for_write(&self) -> AppResult<PathBuf> {
        sync_fs::create_dir_all(&self.workspace_root).map_err(|error| {
            io_error_with_context("Agent workspace root creation failed", error)
        })?;
        sync_fs::canonicalize(&self.workspace_root)
            .map_err(|error| io_error_with_context("Agent workspace root inspection failed", error))
    }

    /// 校验现有目录的 canonical path，避免读历史会话时跟随配置根内的外部目录链接。
    async fn ensure_directory_is_contained(&self, path: &Path) -> AppResult<()> {
        fs::create_dir_all(&self.workspace_root)
            .await
            .map_err(io_error)?;
        let canonical_root = fs::canonicalize(&self.workspace_root)
            .await
            .map_err(io_error)?;
        let canonical_path = fs::canonicalize(path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                AppError::not_found("Agent session directory not found")
            } else {
                io_error(error)
            }
        })?;
        if !canonical_path.starts_with(canonical_root) {
            return Err(AppError::invalid(
                "Agent sessions directory escapes the workspace",
            ));
        }
        Ok(())
    }

    /// 比较 canonical 父目录与 workspace 根，阻断自定义链接目录向外写文件。
    async fn ensure_parent_is_contained_from(
        &self,
        path: &Path,
        canonical_root: &Path,
    ) -> AppResult<()> {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::invalid("Agent document has no parent"))?;
        let canonical_parent = fs::canonicalize(parent).await.map_err(io_error)?;
        if !canonical_parent.starts_with(canonical_root) {
            return Err(AppError::invalid(
                "Agent document parent escapes the workspace",
            ));
        }
        Ok(())
    }

    /// 逐段创建缺失目录并在每一步拒绝符号链接，避免 create_dir_all 沿链接越出数据根。
    async fn create_contained_parent(&self, path: &Path, canonical_root: &Path) -> AppResult<()> {
        let parent = path
            .parent()
            .ok_or_else(|| AppError::invalid("Agent document has no parent"))?;
        let relative = parent
            .strip_prefix(&self.workspace_root)
            .map_err(|_| AppError::invalid("Agent document path escapes the workspace"))?;
        let mut current = self.workspace_root.clone();
        for component in relative.components() {
            let Component::Normal(segment) = component else {
                return Err(AppError::invalid("Agent document path is not normalized"));
            };
            current.push(segment);
            match fs::symlink_metadata(&current).await {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(AppError::invalid(
                        "Agent workspace contains an unsafe path component",
                    ));
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    fs::create_dir(&current).await.map_err(io_error)?;
                }
                Err(error) => return Err(io_error(error)),
            }
        }
        self.ensure_parent_is_contained_from(path, canonical_root)
            .await
    }
}

/// 持久化 journal 只存固定路径与哈希，不重复写入包含 launch env 的文档正文。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceTransactionJournal {
    schema_version: u32,
    agent_session_id: AgentSessionId,
    phase: WorkspaceTransactionPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resume_attempt_id: Option<Uuid>,
    entries: Vec<WorkspaceTransactionEntry>,
}

/// Preparing 只允许清理其登记的 staging；Prepared 才能按 CAS 顺序前滚目标文件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum WorkspaceTransactionPhase {
    Preparing,
    Prepared,
}

/// 每项 journal 绑定固定文档类型、兄弟 staging 路径及提交前后的原始字节哈希。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceTransactionEntry {
    kind: String,
    target: String,
    staging: String,
    before_sha256: Option<String>,
    after_sha256: String,
}

/// 只允许安全单路径组件，opaque domain ID 本身仍保留其兼容语义。
fn validate_path_id(id: &AgentSessionId) -> AppResult<()> {
    let value = id.as_str();
    let mut components = Path::new(value).components();
    if value.trim().is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value.contains(':')
        || value.chars().any(char::is_control)
        || !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
    {
        return Err(AppError::invalid(
            "Agent session id is not a safe path component",
        ));
    }
    Ok(())
}

/// 在锁内幂等恢复一个 session journal；第三方内容不匹配时保留 journal 与 staging。
///
/// @author kongweiguang
fn recover_workspace_transaction(
    root: &Path,
    session_root: &Path,
    id: &AgentSessionId,
) -> AppResult<()> {
    let journal_path = session_root.join(JOURNAL_FILE);
    let metadata = match sync_fs::symlink_metadata(&journal_path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(io_error_with_context(
                "Agent transaction journal read failed",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::configuration_corrupt(
            "Agent workspace transaction journal",
        ));
    }
    let bytes = sync_fs::read(&journal_path)
        .map_err(|error| io_error_with_context("Agent transaction journal read failed", error))?;
    let journal =
        serde_json::from_slice::<WorkspaceTransactionJournal>(&bytes).map_err(|error| {
            AppError::configuration_corrupt("Agent workspace transaction journal")
                .with_source(error)
        })?;
    validate_journal(&journal, session_root, id)?;

    if journal.phase == WorkspaceTransactionPhase::Prepared && journal.resume_attempt_id.is_some() {
        validate_resume_checkpoint_journal_intent(root, session_root, id, &journal)?;
    }

    if journal.phase == WorkspaceTransactionPhase::Preparing {
        for entry in &journal.entries {
            let (kind, stage) = validated_entry_paths(session_root, entry)?;
            let _ = kind;
            remove_prepared_stage(&stage).map_err(|error| {
                AppError::outcome_unknown_with_source(
                    "Agent workspace preparing recovery cleanup is pending",
                    error,
                )
            })?;
        }
        remove_journal(session_root, &journal_path)?;
        return Ok(());
    }

    for entry in &journal.entries {
        let (kind, target, stage) = validated_entry_paths_with_target(session_root, entry)?;
        let current = read_optional_file(root, session_root, &target)?;
        let current_revision = current
            .as_deref()
            .map(sha256_revision)
            .unwrap_or(AgentWorkspaceRevision::Missing);
        let before_revision = entry
            .before_sha256
            .as_deref()
            .map(|hash| AgentWorkspaceRevision::Sha256(hash.to_owned()))
            .unwrap_or(AgentWorkspaceRevision::Missing);
        let after_revision = AgentWorkspaceRevision::Sha256(entry.after_sha256.clone());

        if kind == AgentWorkspaceFileKind::ResumeCheckpoint {
            validate_resume_checkpoint_journal_entry(
                root,
                session_root,
                id,
                &journal,
                entry,
                current.as_deref(),
            )?;
        }

        if current_revision == after_revision {
            verify_staging_after_hash(root, session_root, &stage, &entry.after_sha256)?;
            sync_committed_file(&target)?;
            continue;
        }
        if current_revision != before_revision {
            return Err(AppError::conflict(
                "Agent workspace transaction conflicts with an external document edit; recovery files were retained",
            ));
        }

        let staged = read_optional_file(root, session_root, &stage)?.ok_or_else(|| {
            AppError::outcome_unknown(
                "Agent workspace transaction is pending but a staged document is missing",
            )
        })?;
        if sha256_hex(&staged) != entry.after_sha256 {
            return Err(AppError::configuration_corrupt(
                "Agent workspace transaction staging file",
            ));
        }
        let mode = if entry.before_sha256.is_some() {
            CommitMode::ReplaceExisting
        } else {
            CommitMode::CreateNew
        };
        if let Err(error) = commit_staged_file(&stage, &target, mode) {
            return Err(AppError::outcome_unknown_with_source(
                "Agent workspace transaction commit is unresolved; recovery files were retained",
                error,
            ));
        }
        let committed = read_optional_file(root, session_root, &target)?
            .is_some_and(|bytes| sha256_hex(&bytes) == entry.after_sha256);
        if !committed {
            return Err(AppError::outcome_unknown(
                "Agent workspace transaction commit requires recovery",
            ));
        }
        let _ = kind;
    }

    for entry in &journal.entries {
        let (_, stage) = validated_entry_paths(session_root, entry)?;
        remove_stage_if_matching(root, session_root, &stage, &entry.after_sha256)?;
    }
    remove_journal(session_root, &journal_path)?;
    Ok(())
}

/// 写入或替换 journal 时也用固定兄弟 staging，提交未知时绝不继续改用户文档。
fn write_journal(
    session_root: &Path,
    journal: &WorkspaceTransactionJournal,
    mode: CommitMode,
) -> AppResult<()> {
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        AppError::infrastructure_with_source(
            "Agent transaction journal serialization failed",
            error,
        )
    })?;
    let stage = journal_stage_path(session_root, &Uuid::new_v4());
    write_new_synced_file(&stage, &bytes).map_err(|error| {
        io_error_with_context("Agent transaction journal staging failed", error)
    })?;
    match commit_staged_file(&stage, &session_root.join(JOURNAL_FILE), mode) {
        Ok(()) => Ok(()),
        Err(error @ AtomicCommitError::CommitUnknown(_)) => {
            Err(AppError::outcome_unknown_with_source(
                "Agent transaction journal durability is unknown; no further document commit was started",
                error,
            ))
        }
        Err(AtomicCommitError::NotCommitted(error)) => {
            if let Err(cleanup) = remove_prepared_stage(&stage) {
                return Err(AppError::infrastructure_with_source(
                    "Agent transaction journal staging cleanup failed",
                    cleanup,
                ));
            }
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                Err(AppError::conflict(
                    "Agent workspace transaction journal already exists",
                ))
            } else {
                Err(io_error_with_context(
                    "Agent transaction journal commit failed",
                    error,
                ))
            }
        }
    }
}

/// 校验 journal schema、会话归属、固定 document kind 与哈希格式后才允许恢复文件。
///
/// @author kongweiguang
fn validate_journal(
    journal: &WorkspaceTransactionJournal,
    session_root: &Path,
    id: &AgentSessionId,
) -> AppResult<()> {
    if journal.schema_version != JOURNAL_SCHEMA_VERSION
        || journal.agent_session_id != *id
        || journal.entries.is_empty()
    {
        return Err(AppError::configuration_corrupt(
            "Agent workspace transaction journal",
        ));
    }
    let mut kinds = HashSet::with_capacity(journal.entries.len());
    for entry in &journal.entries {
        let kind = parse_workspace_file_kind(&entry.kind).ok_or_else(|| {
            AppError::configuration_corrupt("Agent workspace transaction journal")
        })?;
        if !kinds.insert(kind)
            || entry.target != relative_workspace_file_path_string(kind)
            || !valid_sha256(&entry.after_sha256)
            || entry
                .before_sha256
                .as_deref()
                .is_some_and(|hash| !valid_sha256(hash))
        {
            return Err(AppError::configuration_corrupt(
                "Agent workspace transaction journal",
            ));
        }
        let _ = validated_entry_paths(session_root, entry)?;
    }
    let has_checkpoint = journal
        .entries
        .iter()
        .any(|entry| entry.kind == "resume_checkpoint");
    match journal.resume_attempt_id {
        Some(_) => {
            if journal.entries.len() != 3
                || journal.entries[0].kind != "session"
                || journal.entries[1].kind != "provider"
                || journal.entries[2].kind != "resume_checkpoint"
            {
                return Err(AppError::configuration_corrupt(
                    "Agent workspace transaction journal",
                ));
            }
        }
        None if has_checkpoint => {
            return Err(AppError::configuration_corrupt(
                "Agent workspace transaction journal",
            ));
        }
        None => {}
    }
    Ok(())
}

/// 校验 resume journal 的 intended hashes 与三份 after bytes 精确一致，防止前滚重构后的参数覆盖外部写入。
///
/// @author kongweiguang
fn validate_resume_checkpoint_journal_intent(
    root: &Path,
    session_root: &Path,
    id: &AgentSessionId,
    journal: &WorkspaceTransactionJournal,
) -> AppResult<()> {
    let attempt_id = journal
        .resume_attempt_id
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    let session_bytes = read_entry_after_bytes(root, session_root, &journal.entries[0])?;
    let provider_bytes = read_entry_after_bytes(root, session_root, &journal.entries[1])?;
    let checkpoint_bytes = read_entry_after_bytes(root, session_root, &journal.entries[2])?;
    let checkpoint_file = serde_json::from_slice::<AgentResumeCheckpointFile>(&checkpoint_bytes)
        .map_err(|error| {
            AppError::configuration_corrupt("Agent resume checkpoint").with_source(error)
        })?;
    let checkpoint = checkpoint_file.to_checkpoint(id)?;
    if checkpoint.attempt_id != attempt_id
        || checkpoint.phase != AgentResumeCheckpointPhase::Committed
        || checkpoint.intended_session_revision != revision_for_bytes(&session_bytes)
        || checkpoint.intended_provider_revision != revision_for_bytes(&provider_bytes)
        || checkpoint.base_session_revision
            != checkpoint_revision(journal.entries[0].before_sha256.as_deref())?
        || checkpoint.base_provider_revision
            != checkpoint_revision(journal.entries[1].before_sha256.as_deref())?
    {
        return Err(AppError::configuration_corrupt(
            "Agent resume checkpoint transaction intent",
        ));
    }
    Ok(())
}

/// 从已提交目标或登记 staging 取出 after bytes；缺失时结果未知，不把它推断为已清理。
///
/// @author kongweiguang
fn read_entry_after_bytes(
    root: &Path,
    session_root: &Path,
    entry: &WorkspaceTransactionEntry,
) -> AppResult<Vec<u8>> {
    let (_, target, stage) = validated_entry_paths_with_target(session_root, entry)?;
    if let Some(bytes) = read_optional_file(root, session_root, &target)?
        && sha256_hex(&bytes) == entry.after_sha256
    {
        return Ok(bytes);
    }
    if let Some(bytes) = read_optional_file(root, session_root, &stage)?
        && sha256_hex(&bytes) == entry.after_sha256
    {
        return Ok(bytes);
    }
    Err(AppError::outcome_unknown(
        "Agent workspace transaction after data is missing or changed",
    ))
}

/// Journal 恢复前核验 checkpoint stage/target 仍属于同 attempt 的 Committed marker。
///
/// @author kongweiguang
fn validate_resume_checkpoint_journal_entry(
    root: &Path,
    session_root: &Path,
    id: &AgentSessionId,
    journal: &WorkspaceTransactionJournal,
    entry: &WorkspaceTransactionEntry,
    current: Option<&[u8]>,
) -> AppResult<()> {
    let attempt_id = journal
        .resume_attempt_id
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    let (_, target, stage) = validated_entry_paths_with_target(session_root, entry)?;
    let current_revision = current
        .map(sha256_revision)
        .unwrap_or(AgentWorkspaceRevision::Missing);
    let after_revision = AgentWorkspaceRevision::Sha256(entry.after_sha256.clone());
    let bytes = if current_revision == after_revision {
        current.map(ToOwned::to_owned).ok_or_else(|| {
            AppError::outcome_unknown("Agent resume checkpoint target cannot be read back")
        })?
    } else {
        read_optional_file(root, session_root, &stage)?.ok_or_else(|| {
            AppError::outcome_unknown(
                "Agent resume checkpoint staging is missing; recovery is unresolved",
            )
        })?
    };
    if sha256_hex(&bytes) != entry.after_sha256 {
        return Err(AppError::configuration_corrupt(
            "Agent resume checkpoint transaction staging",
        ));
    }
    let file = serde_json::from_slice::<AgentResumeCheckpointFile>(&bytes).map_err(|error| {
        AppError::configuration_corrupt("Agent resume checkpoint").with_source(error)
    })?;
    let checkpoint = file.to_checkpoint(id)?;
    if checkpoint.attempt_id != attempt_id
        || checkpoint.phase != AgentResumeCheckpointPhase::Committed
        || target != session_root.join(RESUME_CHECKPOINT_FILE)
    {
        return Err(AppError::configuration_corrupt(
            "Agent resume checkpoint transaction identity",
        ));
    }
    Ok(())
}

/// 将 journal entry 的相对 target/staging 解析回受限 session 目录内的固定路径。
///
/// @author kongweiguang
fn validated_entry_paths(
    session_root: &Path,
    entry: &WorkspaceTransactionEntry,
) -> AppResult<(AgentWorkspaceFileKind, PathBuf)> {
    let (kind, _, stage) = validated_entry_paths_with_target(session_root, entry)?;
    Ok((kind, stage))
}

/// 只接受 `kind` 白名单对应目标和 UUID 命名的同目录 staging，不解释任意 journal 路径。
///
/// @author kongweiguang
fn validated_entry_paths_with_target(
    session_root: &Path,
    entry: &WorkspaceTransactionEntry,
) -> AppResult<(AgentWorkspaceFileKind, PathBuf, PathBuf)> {
    let kind = parse_workspace_file_kind(&entry.kind)
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    let target_relative = PathBuf::from(relative_workspace_file_path_string(kind));
    if entry.target != relative_workspace_file_path_string(kind) {
        return Err(AppError::configuration_corrupt(
            "Agent workspace transaction journal",
        ));
    }
    let staging_relative = PathBuf::from(&entry.staging);
    validate_normal_relative_path(&staging_relative)?;
    let target = session_root.join(&target_relative);
    let staging = session_root.join(&staging_relative);
    let target_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    let staging_name = staging
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    let prefix = format!("{target_name}.");
    let nonce = staging_name
        .strip_prefix(&prefix)
        .and_then(|name| name.strip_suffix(".agent-stage"))
        .and_then(|nonce| Uuid::parse_str(nonce).ok())
        .ok_or_else(|| AppError::configuration_corrupt("Agent workspace transaction journal"))?;
    if staging_relative.parent() != target_relative.parent()
        || relative_to_session(session_root, &stage_path_for(&target, &nonce)?)? != entry.staging
    {
        return Err(AppError::configuration_corrupt(
            "Agent workspace transaction journal",
        ));
    }
    Ok((kind, target, staging))
}

/// 只在完整提交后的 Prepared journal 中清理生成的 staging 文件。
fn verify_staging_after_hash(
    root: &Path,
    session_root: &Path,
    stage: &Path,
    expected_hash: &str,
) -> AppResult<()> {
    if let Some(bytes) = read_optional_file(root, session_root, stage)?
        && sha256_hex(&bytes) != expected_hash
    {
        return Err(AppError::conflict(
            "Agent workspace recovery staging file was externally changed",
        ));
    }
    Ok(())
}

/// 删除已提交 transaction 的残留 staging 前再次验证正文哈希，外部文件不会被误删。
fn remove_stage_if_matching(
    root: &Path,
    session_root: &Path,
    stage: &Path,
    expected_hash: &str,
) -> AppResult<()> {
    let Some(bytes) = read_optional_file(root, session_root, stage)? else {
        return Ok(());
    };
    if sha256_hex(&bytes) != expected_hash {
        return Err(AppError::conflict(
            "Agent workspace recovery staging file was externally changed",
        ));
    }
    remove_prepared_stage(stage).map_err(|error| {
        AppError::outcome_unknown_with_source("Agent workspace recovery cleanup is pending", error)
    })
}

/// 清除 Preparing 阶段登记的部分 staging；目标文档尚未触碰，异常时保留 journal 重试恢复。
fn remove_prepared_stage(stage: &Path) -> std::io::Result<()> {
    match sync_fs::symlink_metadata(stage) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "agent stage path is not a regular file",
            ))
        }
        Ok(_) => {
            sync_fs::remove_file(stage)?;
            if let Some(parent) = stage.parent() {
                sync_directory(parent)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// 删除固定 journal 并同步 session 目录；失败时调用方保留恢复态或返回未知结果。
fn remove_journal(session_root: &Path, journal_path: &Path) -> AppResult<()> {
    sync_fs::remove_file(journal_path).map_err(|error| {
        AppError::outcome_unknown_with_source("Agent transaction cleanup is pending", error)
    })?;
    sync_directory(session_root).map_err(|error| {
        AppError::outcome_unknown_with_source(
            "Agent transaction journal removal result is unknown",
            error,
        )
    })
}

/// 读文件只接受固定 session 子树内的普通文件，缺失父目录按 Missing revision 返回。
fn read_optional_file(root: &Path, session_root: &Path, path: &Path) -> AppResult<Option<Vec<u8>>> {
    if !ensure_existing_parent_sync(root, session_root, path)? {
        return Ok(None);
    }
    let metadata = match sync_fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(io_error_with_context(
                "Agent document inspection failed",
                error,
            ));
        }
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::configuration_corrupt("Agent document"));
    }
    let canonical = sync_fs::canonicalize(path)
        .map_err(|error| io_error_with_context("Agent document path inspection failed", error))?;
    let canonical_session = sync_fs::canonicalize(session_root)
        .map_err(|error| io_error_with_context("Agent session path inspection failed", error))?;
    if !canonical.starts_with(root) || !canonical.starts_with(canonical_session) {
        return Err(AppError::invalid("Agent document path escapes its session"));
    }
    sync_fs::read(path)
        .map(Some)
        .map_err(|error| io_error_with_context("Agent document read failed", error))
}

/// 缺失的 session.toml 与损坏文件区分处理，读取 revision 与正文始终来自同一字节快照。
fn read_required_file(root: &Path, session_root: &Path, path: &Path) -> AppResult<Vec<u8>> {
    read_optional_file(root, session_root, path)?
        .ok_or_else(|| AppError::not_found("Agent session not found"))
}

/// 读取 session 根时逐段拒绝链接或非目录，并确认 canonical path 留在用户数据根内。
fn ensure_existing_session_directory(root: &Path, session_root: &Path) -> AppResult<()> {
    let metadata = sync_fs::symlink_metadata(session_root).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AppError::not_found("Agent session directory not found")
        } else {
            io_error_with_context("Agent session directory inspection failed", error)
        }
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::configuration_corrupt("Agent session directory"));
    }
    let canonical = sync_fs::canonicalize(session_root).map_err(|error| {
        io_error_with_context("Agent session directory inspection failed", error)
    })?;
    if !canonical.starts_with(root) {
        return Err(AppError::invalid(
            "Agent session directory escapes workspace",
        ));
    }
    Ok(())
}

/// 在读取时验证每一级既有父目录，发现缺失时不创建用户数据目录。
fn ensure_existing_parent_sync(root: &Path, session_root: &Path, path: &Path) -> AppResult<bool> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("Agent document has no parent"))?;
    let relative = parent
        .strip_prefix(session_root)
        .map_err(|_| AppError::invalid("Agent document path escapes its session"))?;
    let mut current = session_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(AppError::invalid("Agent document path is not normalized"));
        };
        current.push(name);
        let metadata = match sync_fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(io_error_with_context(
                    "Agent document path inspection failed",
                    error,
                ));
            }
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(AppError::configuration_corrupt("Agent document path"));
        }
        let canonical = sync_fs::canonicalize(&current).map_err(|error| {
            io_error_with_context("Agent document path inspection failed", error)
        })?;
        let canonical_session = sync_fs::canonicalize(session_root).map_err(|error| {
            io_error_with_context("Agent session path inspection failed", error)
        })?;
        if !canonical.starts_with(root) || !canonical.starts_with(canonical_session) {
            return Err(AppError::invalid("Agent document path escapes its session"));
        }
    }
    Ok(true)
}

/// 为待写固定文档逐段创建父目录并拒绝符号链接，不使用会跟随链接的 create_dir_all。
fn ensure_parent_sync(root: &Path, session_root: &Path, path: &Path) -> AppResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| AppError::invalid("Agent document has no parent"))?;
    let relative = parent
        .strip_prefix(session_root)
        .map_err(|_| AppError::invalid("Agent document path escapes its session"))?;
    let mut current = session_root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(AppError::invalid("Agent document path is not normalized"));
        };
        current.push(name);
        let created = match sync_fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AppError::configuration_corrupt("Agent document path"));
            }
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                sync_fs::create_dir(&current).map_err(|error| {
                    io_error_with_context("Agent document directory creation failed", error)
                })?;
                true
            }
            Err(error) => {
                return Err(io_error_with_context(
                    "Agent document path inspection failed",
                    error,
                ));
            }
        };
        if created && let Some(parent) = current.parent() {
            sync_directory(parent).map_err(|error| {
                io_error_with_context("Agent document directory durability failed", error)
            })?;
        }
        let canonical = sync_fs::canonicalize(&current).map_err(|error| {
            io_error_with_context("Agent document path inspection failed", error)
        })?;
        let canonical_session = sync_fs::canonicalize(session_root).map_err(|error| {
            io_error_with_context("Agent session path inspection failed", error)
        })?;
        if !canonical.starts_with(root) || !canonical.starts_with(canonical_session) {
            return Err(AppError::invalid("Agent document path escapes its session"));
        }
    }
    Ok(())
}

/// 确认适配器提交目标严格等于该 document kind 的固定相对路径。
fn validate_document_path(
    session_root: &Path,
    kind: AgentWorkspaceDocumentKind,
    path: &Path,
) -> AppResult<()> {
    if path != session_root.join(relative_document_path(kind).iter().collect::<PathBuf>()) {
        return Err(AppError::invalid(
            "Agent document path is not registered for its kind",
        ));
    }
    Ok(())
}

/// 验证内部文件种类映射到的唯一固定目标，防止 checkpoint 作为任意路径写入。
///
/// @author kongweiguang
fn validate_workspace_file_path(
    session_root: &Path,
    kind: AgentWorkspaceFileKind,
    path: &Path,
) -> AppResult<()> {
    match kind {
        AgentWorkspaceFileKind::Document(kind) => validate_document_path(session_root, kind, path),
        AgentWorkspaceFileKind::ResumeCheckpoint
            if path == session_root.join(RESUME_CHECKPOINT_FILE) =>
        {
            Ok(())
        }
        AgentWorkspaceFileKind::ResumeCheckpoint => Err(AppError::invalid(
            "Agent resume checkpoint path is not registered",
        )),
    }
}

/// 先读最小 schema 字段，以便未来版本即使新增字段也保留为可诊断的未来格式。
fn classify_provider_session(
    contents: &str,
    expected_provider: &AgentProvider,
) -> AgentProviderSessionStatus {
    let envelope = match toml::from_str::<toml::Value>(contents) {
        Ok(value) => value,
        Err(_) => return AgentProviderSessionStatus::Malformed,
    };
    let Some(schema_version) = envelope
        .get("schema_version")
        .and_then(toml::Value::as_integer)
        .and_then(|version| u32::try_from(version).ok())
    else {
        return AgentProviderSessionStatus::Malformed;
    };
    if schema_version != 1 {
        return AgentProviderSessionStatus::UnsupportedSchemaVersion(schema_version);
    }

    let provider = match toml::from_str::<AgentProviderSession>(contents) {
        Ok(provider) => provider,
        Err(_) => return AgentProviderSessionStatus::Malformed,
    };
    if provider.validate().is_err() {
        return AgentProviderSessionStatus::UnsupportedSchemaVersion(provider.schema_version);
    }
    if &provider.provider != expected_provider {
        return AgentProviderSessionStatus::ProviderMismatch;
    }
    AgentProviderSessionStatus::Available(provider)
}

/// 校验 provider/session 成对语义，并将 session 按当前持久字段重新编码。
/// 未提供 provider 更新时仍拒绝改变已有 provider 归属，且校验在 staging 前完成。
///
/// @author kongweiguang
fn normalize_and_validate_agent_documents(
    root: &Path,
    session_root: &Path,
    id: &AgentSessionId,
    files: &mut [AgentWorkspaceFile],
) -> AppResult<()> {
    let session_index = files.iter().position(|file| {
        file.kind == AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Session)
    });
    let provider_index = files.iter().position(|file| {
        file.kind == AgentWorkspaceFileKind::Document(AgentWorkspaceDocumentKind::Provider)
    });
    if session_index.is_none() && provider_index.is_none() {
        return Ok(());
    }

    let existing_session_path = session_root.join("session.toml");
    let existing_session_bytes = read_optional_file(root, session_root, &existing_session_path)?;
    let existing_session = existing_session_bytes
        .as_deref()
        .map(parse_agent_session_file)
        .transpose()?
        .map(|session| ensure_session_id(session, id))
        .transpose()?;

    let proposed_session = if let Some(index) = session_index {
        let file = files
            .get_mut(index)
            .ok_or_else(|| AppError::infrastructure("Agent session document index changed"))?;
        let parsed = parse_agent_session_file(&file.contents)?;
        let session = ensure_session_id(parsed.clone(), id)?;
        file.contents = encode_agent_session_document(&session)?;
        Some(session)
    } else {
        None
    };

    let existing_provider_path = session_root.join("provider.toml");
    let existing_provider_bytes = read_optional_file(root, session_root, &existing_provider_path)?;
    let existing_provider = existing_provider_bytes
        .as_deref()
        .map(parse_provider_session_file)
        .transpose()?;
    if let Some(provider) = &existing_provider {
        let session = existing_session.as_ref().ok_or_else(|| {
            AppError::configuration_corrupt("Agent provider session relationship")
        })?;
        if provider.provider != session.agent_id {
            return Err(AppError::configuration_corrupt(
                "Agent provider session relationship",
            ));
        }
    }

    let proposed_provider = if let Some(index) = provider_index {
        let file = files
            .get(index)
            .ok_or_else(|| AppError::infrastructure("Agent provider document index changed"))?;
        Some(parse_provider_session_file(&file.contents)?)
    } else {
        None
    };
    let effective_session = proposed_session.as_ref().or(existing_session.as_ref());
    if let Some(provider) = &proposed_provider {
        let session = effective_session.ok_or_else(|| {
            AppError::configuration_corrupt("Agent provider session relationship")
        })?;
        if provider.provider != session.agent_id {
            return Err(AppError::invalid(
                "Agent provider metadata does not match its session",
            ));
        }
    }
    if let (Some(session), None, Some(provider)) = (
        proposed_session.as_ref(),
        proposed_provider.as_ref(),
        existing_provider.as_ref(),
    ) && provider.provider != session.agent_id
    {
        return Err(AppError::invalid(
            "Agent session provider cannot change without matching provider metadata",
        ));
    }
    Ok(())
}

/// 只从 session 边界 DTO 恢复领域聚合，拒绝损坏、未知字段和未来 schema。
fn parse_agent_session_file(contents: &[u8]) -> AppResult<AgentSession> {
    let text = std::str::from_utf8(contents)
        .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
    let file = toml::from_str::<AgentSessionFile>(text)
        .map_err(|error| AppError::configuration_corrupt("Agent session").with_source(error))?;
    file.restore()
}

/// Provider 写入必须是完整的 schema 1 文档，未知或未来格式不能作为可替换默认值。
fn parse_provider_session_file(contents: &[u8]) -> AppResult<AgentProviderSession> {
    let text = std::str::from_utf8(contents).map_err(|error| {
        AppError::configuration_corrupt("Agent provider session").with_source(error)
    })?;
    let provider = toml::from_str::<AgentProviderSession>(text).map_err(|error| {
        AppError::configuration_corrupt("Agent provider session").with_source(error)
    })?;
    provider.validate().map_err(|error| {
        AppError::configuration_corrupt("Agent provider session").with_source(error)
    })?;
    Ok(provider)
}

/// 持久 session 主键必须与受管目录一致，避免跨目录文档被复制后重绑定身份。
fn ensure_session_id(session: AgentSession, id: &AgentSessionId) -> AppResult<AgentSession> {
    if session.agent_session_id != *id {
        return Err(AppError::configuration_corrupt("Agent session"));
    }
    Ok(session)
}

/// 固定 session 文档表是外部路径输入与受管 workspace 写入之间的唯一映射。
pub(super) fn relative_document_path(kind: AgentWorkspaceDocumentKind) -> &'static [&'static str] {
    match kind {
        AgentWorkspaceDocumentKind::Session => &["session.toml"],
        AgentWorkspaceDocumentKind::Provider => &["provider.toml"],
        AgentWorkspaceDocumentKind::AgentInstructions => &["AGENTS.md"],
        AgentWorkspaceDocumentKind::ClaudeInstructions => &["CLAUDE.md"],
        AgentWorkspaceDocumentKind::CodexConfig => &[".codex", "config.toml"],
        AgentWorkspaceDocumentKind::ClaudeMcpConfig => &[".mcp.json"],
        AgentWorkspaceDocumentKind::PiMcpConfig => &[".pi", "mcp.json"],
        AgentWorkspaceDocumentKind::McpEndpointContext => &["context", "mcp-endpoint.json"],
        AgentWorkspaceDocumentKind::TargetBindingContext => &["context", "target-binding.json"],
        AgentWorkspaceDocumentKind::TerminalSnapshotContext => {
            &["context", "terminal-snapshot.json"]
        }
        AgentWorkspaceDocumentKind::WorkspaceSnapshotContext => {
            &["context", "workspace-snapshot.json"]
        }
    }
}

/// 生成 journal 中平台无关的相对路径写法，确保 Windows 与 Unix 读取同一契约。
fn relative_document_path_string(kind: AgentWorkspaceDocumentKind) -> String {
    relative_document_path(kind).join("/")
}

/// 把 document kind 映射为稳定 journal 字符串，不依赖 Rust enum Debug 名称。
fn document_kind_id(kind: AgentWorkspaceDocumentKind) -> &'static str {
    match kind {
        AgentWorkspaceDocumentKind::Session => "session",
        AgentWorkspaceDocumentKind::Provider => "provider",
        AgentWorkspaceDocumentKind::AgentInstructions => "agent_instructions",
        AgentWorkspaceDocumentKind::ClaudeInstructions => "claude_instructions",
        AgentWorkspaceDocumentKind::CodexConfig => "codex_config",
        AgentWorkspaceDocumentKind::ClaudeMcpConfig => "claude_mcp_config",
        AgentWorkspaceDocumentKind::PiMcpConfig => "pi_mcp_config",
        AgentWorkspaceDocumentKind::McpEndpointContext => "mcp_endpoint_context",
        AgentWorkspaceDocumentKind::TargetBindingContext => "target_binding_context",
        AgentWorkspaceDocumentKind::TerminalSnapshotContext => "terminal_snapshot_context",
        AgentWorkspaceDocumentKind::WorkspaceSnapshotContext => "workspace_snapshot_context",
    }
}

/// 为内部 checkpoint 提供不暴露给 workspace 文档 API 的固定 journal 身份。
///
/// @author kongweiguang
fn workspace_file_kind_id(kind: AgentWorkspaceFileKind) -> &'static str {
    match kind {
        AgentWorkspaceFileKind::Document(kind) => document_kind_id(kind),
        AgentWorkspaceFileKind::ResumeCheckpoint => "resume_checkpoint",
    }
}

/// 将 journal 类型映射到固定受管相对路径，checkpoint 不会复用用户可编辑文档名。
///
/// @author kongweiguang
fn relative_workspace_file_path_string(kind: AgentWorkspaceFileKind) -> String {
    match kind {
        AgentWorkspaceFileKind::Document(kind) => relative_document_path_string(kind),
        AgentWorkspaceFileKind::ResumeCheckpoint => RESUME_CHECKPOINT_FILE.to_owned(),
    }
}

/// 只接受当前受管文档和专用 checkpoint 两组 journal 身份，拒绝解释未知类型为路径。
///
/// @author kongweiguang
fn parse_workspace_file_kind(value: &str) -> Option<AgentWorkspaceFileKind> {
    if value == "resume_checkpoint" {
        Some(AgentWorkspaceFileKind::ResumeCheckpoint)
    } else {
        parse_document_kind(value).map(AgentWorkspaceFileKind::Document)
    }
}

/// 只接受上面明确列出的 kind，未知 journal 值不会被解释成文件路径。
fn parse_document_kind(value: &str) -> Option<AgentWorkspaceDocumentKind> {
    Some(match value {
        "session" => AgentWorkspaceDocumentKind::Session,
        "provider" => AgentWorkspaceDocumentKind::Provider,
        "agent_instructions" => AgentWorkspaceDocumentKind::AgentInstructions,
        "claude_instructions" => AgentWorkspaceDocumentKind::ClaudeInstructions,
        "codex_config" => AgentWorkspaceDocumentKind::CodexConfig,
        "claude_mcp_config" => AgentWorkspaceDocumentKind::ClaudeMcpConfig,
        "pi_mcp_config" => AgentWorkspaceDocumentKind::PiMcpConfig,
        "mcp_endpoint_context" => AgentWorkspaceDocumentKind::McpEndpointContext,
        "target_binding_context" => AgentWorkspaceDocumentKind::TargetBindingContext,
        "terminal_snapshot_context" => AgentWorkspaceDocumentKind::TerminalSnapshotContext,
        "workspace_snapshot_context" => AgentWorkspaceDocumentKind::WorkspaceSnapshotContext,
        _ => return None,
    })
}

/// 固定 kind 名称和经验证的 UUID 共同限制 journal staging 文件名。
fn stage_path_for(target: &Path, nonce: &Uuid) -> AppResult<PathBuf> {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::invalid("Agent document filename is invalid"))?;
    Ok(target.with_file_name(format!("{name}.{nonce}.agent-stage")))
}

/// Journal 本身也只使用固定目标名和受限随机兄弟文件名。
fn journal_stage_path(session_root: &Path, nonce: &Uuid) -> PathBuf {
    session_root.join(format!("{JOURNAL_FILE}.{nonce}.agent-stage"))
}

/// 仅接受不含绝对、父目录或当前目录组件的相对 journal 路径。
fn validate_normal_relative_path(path: &Path) -> AppResult<()> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(AppError::configuration_corrupt(
            "Agent workspace transaction journal",
        ));
    }
    Ok(())
}

/// 生成受限制 staging 文件，并在 Linux/macOS 上使用仅 owner 可读写的默认权限。
fn write_new_synced_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.sync_all()
}

/// 把路径转为受控 session 相对名，拒绝非 UTF-8 或越界路径进入恢复 journal。
fn relative_to_session(session_root: &Path, path: &Path) -> AppResult<String> {
    let relative = path
        .strip_prefix(session_root)
        .map_err(|_| AppError::invalid("Agent staging path escapes its session"))?;
    validate_normal_relative_path(relative)?;
    relative
        .components()
        .map(|component| match component {
            Component::Normal(value) => value
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| AppError::invalid("Agent staging path is not UTF-8")),
            _ => Err(AppError::invalid("Agent staging path is not normalized")),
        })
        .collect::<AppResult<Vec<_>>>()
        .map(|parts| parts.join("/"))
}

/// 原始字节 SHA-256 用于 CAS 与恢复判定，不能按 TOML/JSON 重新序列化后比较。
fn revision_for_bytes(bytes: &[u8]) -> AgentWorkspaceRevision {
    AgentWorkspaceRevision::Sha256(sha256_hex(bytes))
}

/// 提取 revision 中的哈希表示，调用处保证输入来自同一原始字节快照。
fn sha256_revision(bytes: &[u8]) -> AgentWorkspaceRevision {
    revision_for_bytes(bytes)
}

/// 生成固定小写十六进制摘要，避免新增 hash 编码依赖。
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Journal 哈希字段只接受 64 位小写 SHA-256 字符串。
fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 让首次 journal 安装和中间目录同步使用同一平台策略。
fn sync_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        File::open(path)?.sync_all()
    }
    #[cfg(windows)]
    {
        let _ = path;
        Ok(())
    }
    #[cfg(not(any(unix, windows)))]
    {
        File::open(path)?.sync_all()
    }
}

/// 提交后再次持久化目标；Windows 必须取写句柄且不改原文件权限，失败时保留恢复 journal。
fn sync_committed_file(path: &Path) -> AppResult<()> {
    #[cfg(windows)]
    let file = OpenOptions::new().write(true).open(path);
    #[cfg(not(windows))]
    let file = File::open(path);

    file.and_then(|file| file.sync_all())
        .and_then(|()| path.parent().map(sync_directory).unwrap_or_else(|| Ok(())))
        .map_err(|error| {
            AppError::outcome_unknown_with_source(
                "Agent workspace commit durability is unresolved",
                error,
            )
        })
}

/// 把共享数据根的锁错误压到安全 application 错误，并保留 typed 原因供诊断。
fn storage_lock_error(error: crate::storage::StorageLockError) -> AppError {
    AppError::infrastructure_with_source("Agent workspace storage lock failed", error)
}

/// 后台阻塞任务 panic/取消时不伪装成磁盘提交成功。
fn join_error(error: tokio::task::JoinError) -> AppError {
    AppError::infrastructure_with_source("Agent workspace storage task failed", error)
}

/// 将操作系统文件错误收敛到稳定的 application error，不外泄用户路径。
fn io_error_with_context(context: &'static str, error: std::io::Error) -> AppError {
    AppError::infrastructure_with_source(context, error)
}

/// 将操作系统文件错误收敛到稳定的 application error，不外泄用户路径。
fn io_error(error: std::io::Error) -> AppError {
    io_error_with_context("Agent workspace file operation failed", error)
}

#[cfg(test)]
#[path = "../../tests/support/agent_store_transaction_unit.rs"]
mod transaction_tests;
