//! 本机与 SFTP 之间的有界传输后端，队列、幂等重试和生命周期仍由 application 管理。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    connections::{RemoteConnectionService, management::HostSftpOptions},
    transfers::{
        SftpTransferPolicySnapshot, TransferBackend, TransferCheckpoint, TransferCleanupState,
        TransferExecutionContext, TransferExecutionError, TransferExecutionFailure,
        TransferExecutionResult, TransferPhase, TransferProgressReporter, TransferProgressUpdate,
        TransferRecoveryInspection, TransferRecoveryRecord, TransferResourceCleanup,
        TransferStagingOwnership,
    },
};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FileEntryKind, FileRevision, TransferFailureKind,
        TransferKind, TransferRequest,
    },
    identity::TransferId,
};
use russh_sftp::{
    client::{Config as SftpClientConfig, error::Error as SftpError},
    protocol::FileType,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, AsyncWrite, AsyncWriteExt, SeekFrom},
    time::Instant,
};
use uuid::Uuid;

use super::sftp::SftpFileProvider;
use crate::storage::AtomicCommitError;

mod endpoint_session;
use self::endpoint_session::{CommitFailure, EndpointSession, FileCommitMode, ObjectMetadata};

const COPY_BUFFER_BYTES: usize = 64 * 1024;
const CHECKPOINT_INTERVAL_BYTES: u64 = 4 * 1024 * 1024;
const STAGING_MARKER_MAX_BYTES: u64 = 1024;
const STAGING_MARKER_SCHEMA_VERSION: u8 = 2;
const MAX_SOURCE_DIRECTORY_DEPTH: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EffectiveSftpOptions {
    preserve_mtime: bool,
    follow_source_symlinks: bool,
}

/// 从冻结的端点策略一次性选出本轮行为：mtime 由目标主机决定，follow 只由源主机语义决定。
/// 本机→远端时远端 host 配置仍控制“本轮从本机源跟随”；容器不进入此 SFTP 后端。
///
/// @author kongweiguang
fn effective_sftp_options(
    source: &FileEndpoint,
    destination: &FileEndpoint,
    source_sftp: Option<&HostSftpOptions>,
    destination_sftp: Option<&HostSftpOptions>,
) -> EffectiveSftpOptions {
    let follow_options = match (source, destination) {
        (FileEndpoint::Local { .. }, FileEndpoint::Remote { .. }) => destination_sftp,
        (FileEndpoint::Remote { .. }, FileEndpoint::Local { .. })
        | (FileEndpoint::Remote { .. }, FileEndpoint::Remote { .. }) => source_sftp,
        _ => None,
    };
    let mtime_options = match destination {
        FileEndpoint::Remote { .. } => destination_sftp,
        FileEndpoint::Local { .. } => source_sftp,
        FileEndpoint::Container { .. } => None,
    };
    EffectiveSftpOptions {
        preserve_mtime: mtime_options.is_none_or(|options| options.preserve_mtime),
        follow_source_symlinks: follow_options
            .is_some_and(|options| options.follow_source_symlinks),
    }
}

/// 将入队快照映射到 SFTP client 的写流水线、单包大小和请求超时，不影响已运行任务。
/// 锁定库的三个字段均显式取值，避免默认配置掩盖快照策略；请求超时仍优先采用本次覆盖值。
///
/// @author kongweiguang
fn sftp_client_config(
    policy: SftpTransferPolicySnapshot,
    request_idle_timeout_seconds: Option<u16>,
) -> SftpClientConfig {
    SftpClientConfig {
        max_packet_len: policy.packet_bytes(),
        max_concurrent_writes: policy.pipeline_depth(),
        request_timeout_secs: request_idle_timeout_seconds
            .map(u64::from)
            .unwrap_or_else(|| policy.idle_timeout().as_secs()),
    }
}

trait AsyncReadSeek: AsyncRead + AsyncSeek + Send + Unpin {}
impl<T> AsyncReadSeek for T where T: AsyncRead + AsyncSeek + Send + Unpin {}

trait AsyncWriteSeek: AsyncWrite + AsyncSeek + Send + Unpin {}
impl<T> AsyncWriteSeek for T where T: AsyncWrite + AsyncSeek + Send + Unpin {}

type BoxReader = Box<dyn AsyncReadSeek>;
type BoxWriter = Box<dyn AsyncWriteSeek>;

/// 内部传输错误仅在失败路径盒装，避免 checkpoint 与清理 owner 放大异步状态机；后端 trait 边界会原样解箱。
type CompactTransferResult<T> = Result<T, Box<TransferExecutionError>>;

/// 单次流式复制的具名上下文，避免位置参数在续传、目录复制与最终提交间错位。
struct StreamTransferPlan<'a> {
    context: &'a TransferExecutionContext,
    source_session: &'a EndpointSession,
    destination_session: &'a EndpointSession,
    source: &'a FileEndpoint,
    destination_stage: &'a FileEndpoint,
    staging_token: String,
    staging_tracker: &'a StagingCleanupTracker,
    public_destination: &'a FileEndpoint,
    /// 树内子项由已登记根覆盖；独立文件必须绑定真正的同目录提交目标，不能借用 UI 进度目录。
    recovery_destination: Option<&'a FileEndpoint>,
    offset: u64,
    hasher: Sha256,
    base_confirmed: u64,
    source_revision: Option<&'a FileRevision>,
    resume_existing: bool,
    report_progress: bool,
    allow_checkpoint: bool,
}

/// 目录子文件复制共享同一树级进度与 staging owner，参数必须作为一个操作快照一起传递。
///
/// @author kongweiguang
struct CopyFileIntoTreePlan<'a> {
    context: &'a TransferExecutionContext,
    source_session: &'a EndpointSession,
    destination_session: &'a EndpointSession,
    source: &'a FileEndpoint,
    destination_in_tree: &'a FileEndpoint,
    public_destination: &'a FileEndpoint,
    total_bytes: &'a mut u64,
    staging_tracker: &'a StagingCleanupTracker,
    preserve_mtime: bool,
}

/// 目录合并的源权限、进度、目标和清理跟踪组成单次目录合并的参数快照。
/// 目录项仍逐项提交，计划结构不改变部分提交语义。
///
/// @author kongweiguang
struct MergeStagedDirectoryPlan<'a> {
    context: &'a TransferExecutionContext,
    destination_session: &'a EndpointSession,
    staging: &'a FileEndpoint,
    destination: &'a FileEndpoint,
    total_bytes: u64,
    staging_tracker: &'a StagingCleanupTracker,
    source_directory_modes: &'a HashMap<FileEndpoint, Option<u32>>,
    source_directory_mtimes: &'a HashMap<FileEndpoint, Option<u64>>,
    preserve_mtime: bool,
}

/// 共享远程连接端口的本机/SFTP 传输组合实现；容器端点由 operations backend 独立接入同一队列。
#[derive(Clone)]
pub struct SftpTransferBackend {
    files: SftpFileProvider,
}

/// 暂存对象和协议资源共用一个清理 owner，避免错误路径只关闭 SSH 却遗失 partial。
struct SftpTransferCleanup {
    sessions: Vec<EndpointSession>,
    staging: StagingCleanupTracker,
}

#[async_trait::async_trait]
impl TransferResourceCleanup for SftpTransferCleanup {
    /// 清理所有独立暂存项；失败项保留原 endpoint lease，其他无待清理对象的 session 仍按同一期限关闭。
    async fn cleanup_until(&self, deadline: Instant) -> AppResult<()> {
        let mut first_error = self
            .staging
            .cleanup_until(&self.sessions, deadline)
            .await
            .err();
        let retained_sessions = self.staging.pending_session_indices(&self.sessions);
        for (index, session) in self.sessions.iter().enumerate() {
            if retained_sessions.contains(&index) {
                continue;
            }
            if let Err(error) = session.close_until(deadline).await {
                if first_error.is_none() {
                    first_error = Some(error);
                } else {
                    tracing::warn!(
                        error_code = error.public_code().as_str(),
                        "另一个独立 SFTP endpoint session 关闭仍待重试"
                    );
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

/// 每个暂存项都有独立 sidecar marker；不能仅凭随机文件名推断删除权限。
#[derive(Clone)]
struct StagingCleanupTracker {
    entries: Arc<Mutex<Vec<StagingArtifact>>>,
}

impl Default for StagingCleanupTracker {
    /// 每次执行尝试独立记录暂存对象，避免不同任务的失败清理互相影响。
    ///
    /// @author kongweiguang
    fn default() -> Self {
        Self {
            entries: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum StagingDisposition {
    Remove,
    Preserve,
    Committed,
}

#[derive(Clone, Copy)]
enum StagingKind {
    File,
    Tree,
}

#[derive(Clone)]
struct StagingArtifact {
    owner: TransferId,
    token: String,
    endpoint: FileEndpoint,
    marker: FileEndpoint,
    kind: StagingKind,
    stage_owned: bool,
    disposition: StagingDisposition,
    recovery_ownership: Option<TransferStagingOwnership>,
}

/// 目录合并使用后序目录帧，在子项完成后才应用新目录权限；已有目录不带权限写入。
struct DirectoryMergeFrame {
    source: FileEndpoint,
    destination: FileEndpoint,
    created_permissions: Option<u32>,
    created_modified_seconds: Option<u64>,
    postorder: bool,
}

#[derive(Serialize)]
struct StagingMarker<'a> {
    schema_version: u8,
    owner: &'a str,
    token: &'a str,
    endpoint_sha256: String,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
enum StagingEndpoint<'a> {
    Local { path: Vec<u8> },
    Remote { host_id: &'a str, path: &'a str },
}

impl StagingArtifact {
    /// 通过随机 token 派生同目录 marker，payload 同时绑定任务 owner 和完整端点。
    ///
    /// @author kongweiguang
    fn new(
        endpoint: FileEndpoint,
        owner: TransferId,
        token: String,
        kind: StagingKind,
    ) -> AppResult<Self> {
        if !valid_staging_token(&token) {
            return Err(AppError::invalid("暂存 token 格式无效"));
        }
        let marker = staging_marker_sibling(&endpoint, &token)?;
        Ok(Self {
            owner,
            token,
            endpoint,
            marker,
            kind,
            stage_owned: false,
            disposition: StagingDisposition::Remove,
            recovery_ownership: None,
        })
    }

    /// 暂存随机路径先取得持久化回执才允许创建；回执未知时不制造新的 marker 或数据对象。
    ///
    /// @author kongweiguang
    async fn register_recovery(
        &mut self,
        context: &TransferExecutionContext,
        actual_destination: &FileEndpoint,
    ) -> AppResult<()> {
        let ownership = self.recovery_record(actual_destination);
        context.progress.register_staging(ownership.clone()).await?;
        self.recovery_ownership = Some(ownership);
        Ok(())
    }

    /// 恢复事实绑定真实文件目标而非展示进度的目录根，确保 merge 子项仍满足同目录提交约束。
    ///
    /// @author kongweiguang
    fn recovery_record(&self, actual_destination: &FileEndpoint) -> TransferStagingOwnership {
        TransferStagingOwnership {
            staging_destination: self.endpoint.clone(),
            actual_destination: actual_destination.clone(),
            staging_owner: self.owner.clone(),
            staging_token: self.token.clone(),
        }
    }

    /// 将完整原生 endpoint 身份先摘要，再写固定长度 marker，避免长路径制造不可读的 sidecar。
    fn marker_bytes(&self) -> AppResult<Vec<u8>> {
        let endpoint = marker_endpoint(&self.endpoint)?;
        let endpoint_bytes = serde_json::to_vec(&endpoint).map_err(|error| {
            AppError::infrastructure_with_source("暂存 endpoint 摘要失败", error)
        })?;
        serde_json::to_vec(&StagingMarker {
            schema_version: STAGING_MARKER_SCHEMA_VERSION,
            owner: self.owner.as_str(),
            token: &self.token,
            endpoint_sha256: sha256_hex(&endpoint_bytes),
        })
        .map_err(|error| AppError::infrastructure_with_source("暂存 marker 序列化失败", error))
    }
}

/// 以无损端点身份编码 marker，不能使用 lossy Unicode 让不同本机路径共用清理权。
fn marker_endpoint(endpoint: &FileEndpoint) -> AppResult<StagingEndpoint<'_>> {
    match endpoint {
        FileEndpoint::Local { path } => Ok(StagingEndpoint::Local {
            path: local_path_identity(path),
        }),
        FileEndpoint::Remote { host_id, path } => Ok(StagingEndpoint::Remote {
            host_id: host_id.as_str(),
            path: path.as_str(),
        }),
        FileEndpoint::Container { .. } => Err(AppError::unsupported("SFTP marker 不接受容器端点")),
    }
}

/// 将本机路径编码为平台原生字符串单元，确保 marker 校验覆盖完整路径身份。
fn local_path_identity(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        path.to_string_lossy().as_bytes().to_vec()
    }
}

impl StagingCleanupTracker {
    /// 清理前冻结已登记事实；清理后内存项可能退役，不能再从剩余对象倒推完整释放集合。
    ///
    /// @author kongweiguang
    fn recovery_ownerships(&self) -> Vec<TransferStagingOwnership> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter_map(|artifact| artifact.recovery_ownership.clone())
            .collect()
    }

    /// 仅当前 execute 的控制 receiver 存活时释放已核验消失的暂存事实；后续 cleanup owner 不持通道。
    /// journal 回执失败保留仓储证据，但不把已物理释放的资源伪装为仍持有 SSH lease。
    ///
    /// @author kongweiguang
    async fn release_cleaned_recovery_ownerships(
        &self,
        progress: &TransferProgressReporter,
        ownerships: &[TransferStagingOwnership],
        deadline: Instant,
    ) {
        for ownership in ownerships {
            if self.contains(&ownership.staging_destination) {
                continue;
            }
            let result =
                tokio::time::timeout_at(deadline, progress.release_staging(ownership.clone()))
                    .await;
            if !matches!(result, Ok(Ok(()))) {
                tracing::warn!("已清理暂存的持久化释放回执未确认，保留恢复事实供核对");
            }
        }
    }

    /// 登记对象后再创建 marker，使 marker 写入不完整时也不会遗失待核查路径。
    fn register(&self, artifact: StagingArtifact) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(artifact);
    }

    /// 判断本轮是否已经登记该路径，确保一次尝试不会重复生成或覆盖 marker。
    fn contains(&self, endpoint: &FileEndpoint) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|artifact| artifact.endpoint == *endpoint)
    }

    /// 仅识别本轮登记的精确 sidecar endpoint，避免目录 merge 把内部 marker 当用户文件复制。
    ///
    /// @author kongweiguang
    fn is_marker_endpoint(&self, endpoint: &FileEndpoint) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .any(|artifact| artifact.marker == *endpoint)
    }

    /// 标记原子 rename 已收到明确回执；清理只能移除 sidecar，不能触碰正式目标。
    fn mark_committed(&self, endpoint: &FileEndpoint) {
        if let Some(artifact) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .find(|artifact| artifact.endpoint == *endpoint)
        {
            artifact.disposition = StagingDisposition::Committed;
        }
    }

    /// 在原子发布目录树前退役已提交子文件 marker；root marker 持续证明整棵 staging 的所有权。
    ///
    /// @author kongweiguang
    async fn retire_committed_file_markers_before_tree_publish(
        &self,
        session: &EndpointSession,
        tree_root: &FileEndpoint,
    ) -> AppResult<()> {
        let (root_artifact, children) = {
            let entries = self
                .entries
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let root_artifact = entries
                .iter()
                .find(|artifact| {
                    matches!(artifact.kind, StagingKind::Tree) && artifact.endpoint == *tree_root
                })
                .cloned()
                .ok_or_else(|| AppError::conflict("目录发布缺少 root 暂存 owner"))?;
            if !root_artifact.stage_owned || root_artifact.disposition != StagingDisposition::Remove
            {
                return Err(AppError::conflict("目录 root 暂存所有权尚未确认"));
            }

            let mut children = Vec::new();
            for artifact in entries.iter().filter(|artifact| {
                artifact.owner == root_artifact.owner && matches!(artifact.kind, StagingKind::File)
            }) {
                if !artifact.marker.same_filesystem(tree_root)
                    || !is_same_or_descendant(tree_root, &artifact.marker)
                {
                    return Err(AppError::conflict(
                        "目录子文件 owner 不属于待发布的 staging 树",
                    ));
                }
                if artifact.disposition != StagingDisposition::Committed || !artifact.stage_owned {
                    return Err(AppError::conflict(
                        "目录 staging 中存在未确认提交的文件暂存",
                    ));
                }
                children.push(artifact.clone());
            }
            (root_artifact, children)
        };

        // 先验证唯一覆盖整树的 owner，随后才可移除会随目录 rename 的子文件 marker。
        session.verify_staging_marker(&root_artifact).await?;
        let Some(root_metadata) = session.metadata(tree_root).await? else {
            return Err(AppError::conflict("目录 staging 在发布前消失"));
        };
        if root_metadata.kind != FileEntryKind::Directory {
            return Err(AppError::conflict("目录 staging 在发布前改变了类型"));
        }

        for artifact in children {
            // cleanup_owned_artifact 会校验 token/endpoint 摘要，并确认已提交的 temporary 不存在。
            session.cleanup_owned_artifact(&artifact).await?;
            self.remove_entry(&artifact.marker);
        }
        Ok(())
    }

    /// 只有文件 create-new 或目录 create-dir 的成功回执能确认本次尝试拥有数据路径。
    fn mark_stage_created(&self, endpoint: &FileEndpoint) {
        if let Some(artifact) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .find(|artifact| artifact.endpoint == *endpoint)
        {
            artifact.stage_owned = true;
        }
    }

    /// 可信续传依赖原暂存对象与 marker，清理 owner 只释放连接而不删除它们。
    fn preserve_checkpoint(&self, checkpoint: &TransferCheckpoint) {
        if let Some(artifact) = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
            .find(|artifact| artifact.endpoint == checkpoint.staging_destination)
        {
            artifact.disposition = StagingDisposition::Preserve;
        }
    }

    /// commitUnknown 时各 staging 的提交边界不确定，因此不删除任何关联文件。
    fn preserve_all(&self) {
        for artifact in self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter_mut()
        {
            if artifact.disposition != StagingDisposition::Committed {
                artifact.disposition = StagingDisposition::Preserve;
            }
        }
    }

    /// 清理所有可确认属于本任务的对象；消失核验成功后才退役物理 owner，持久事实单独处理。
    /// marker 缺失但对象仍在时拒绝删除，物理回执未知时保留同一资源供重试。
    ///
    /// @author kongweiguang
    async fn cleanup_until(
        &self,
        sessions: &[EndpointSession],
        deadline: Instant,
    ) -> AppResult<()> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let mut first_error = None;

        // 目录权限可能因发布失败而已变成只读；先验证根 owner，再从根到叶恢复清理所需的写权限。
        for artifact in entries.iter().filter(|artifact| {
            matches!(artifact.kind, StagingKind::Tree)
                && artifact.stage_owned
                && artifact.disposition == StagingDisposition::Remove
        }) {
            let result = match sessions
                .iter()
                .find(|session| session.owns(&artifact.endpoint))
            {
                Some(session) => tokio::time::timeout_at(
                    deadline,
                    make_owned_staging_tree_writable(session, artifact),
                )
                .await
                .map_err(|_| AppError::timeout("传输暂存权限恢复超过关闭期限"))
                .and_then(|result| result),
                None => Err(AppError::infrastructure("目录暂存权限恢复缺少匹配端点资源")),
            };
            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                } else {
                    tracing::warn!(
                        error_code = error.public_code().as_str(),
                        "另一个目录暂存权限恢复仍待重试"
                    );
                }
            }
        }

        for artifact in entries.into_iter().rev() {
            if artifact.disposition == StagingDisposition::Preserve {
                continue;
            }
            let result = match sessions
                .iter()
                .find(|session| session.owns(&artifact.endpoint))
            {
                Some(session) => {
                    tokio::time::timeout_at(deadline, session.cleanup_owned_artifact(&artifact))
                        .await
                        .map_err(|_| AppError::timeout("传输暂存清理超过关闭期限"))
                        .and_then(|result| result)
                }
                None => Err(AppError::infrastructure("暂存清理缺少匹配端点资源")),
            };
            match result {
                Ok(()) => self.remove_entry(&artifact.marker),
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    } else {
                        tracing::warn!(
                            error_code = error.public_code().as_str(),
                            "另一个独立暂存对象清理仍待重试"
                        );
                    }
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 返回仍需要原始远端 route 才能清理的 session，禁止按可能已变更的 HostId 重连。
    fn pending_session_indices(&self, sessions: &[EndpointSession]) -> Vec<usize> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut indices = Vec::new();
        for artifact in entries
            .iter()
            .filter(|artifact| artifact.disposition != StagingDisposition::Preserve)
        {
            if let Some(index) = sessions
                .iter()
                .position(|session| session.owns(&artifact.endpoint))
                && !indices.contains(&index)
            {
                indices.push(index);
            }
        }
        indices
    }

    /// 仅移除相同 marker 对应的 journal 项，重试期间其它对象仍由 owner 保持。
    fn remove_entry(&self, marker: &FileEndpoint) {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .retain(|artifact| artifact.marker != *marker);
    }
}

/// 验证树根 marker 后按父先子后重置已拥有 staging 目录权限，供 cleanup owner 安全重试。
///
/// @author kongweiguang
async fn make_owned_staging_tree_writable(
    session: &EndpointSession,
    artifact: &StagingArtifact,
) -> AppResult<()> {
    session.verify_staging_marker(artifact).await?;
    let Some(root_metadata) = session.metadata(&artifact.endpoint).await? else {
        return Ok(());
    };
    if root_metadata.kind != FileEntryKind::Directory {
        return Err(AppError::conflict("目录暂存对象类型已变化，拒绝恢复权限"));
    }

    let mut pending = vec![artifact.endpoint.clone()];
    while let Some(directory) = pending.pop() {
        let Some(metadata) = session.metadata(&directory).await? else {
            continue;
        };
        if metadata.kind != FileEntryKind::Directory {
            continue;
        }
        session.set_permissions(&directory, Some(0o700)).await?;
        for (child, _, kind, _) in session.list_children(&directory).await? {
            if kind == FileEntryKind::Directory {
                pending.push(child);
            }
        }
    }
    Ok(())
}

impl SftpTransferBackend {
    /// 复用文件浏览器的连接服务，保证 host profile、凭据、known_hosts 与 SFTP-only 能力门禁一致。
    pub fn new(connections: Arc<dyn RemoteConnectionService>) -> Self {
        Self {
            files: SftpFileProvider::new(connections),
        }
    }

    /// 执行前端点校验、稳定目标选择和可靠传输/提交边界；源链接预检遵循入队冻结的源端策略。
    /// 每轮结束只关闭本轮 SFTP channel；链接仍须经后续规范化、范围与目标类型校验。
    /// 盒装内部错误以缩小 future 的失败分支体积，不改变失败类别或携带的恢复与清理状态。
    ///
    /// @author kongweiguang
    async fn execute_with_sessions(
        &self,
        context: &TransferExecutionContext,
        source_session: &EndpointSession,
        destination_session: &EndpointSession,
        staging: &StagingCleanupTracker,
    ) -> CompactTransferResult<TransferExecutionResult> {
        let request = &context.request;
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(
                None,
                context.actual_destination.clone(),
            )));
        }
        let source_policy = effective_sftp_options(
            &request.source,
            &request.destination,
            context.source_sftp.as_ref(),
            context.destination_sftp.as_ref(),
        );
        let source_metadata = source_session
            .metadata(&request.source)
            .await
            .map_err(|error| transfer_error(error, None, context.actual_destination.clone()))?
            .ok_or_else(|| transfer_error(AppError::not_found("传输源不存在"), None, None))?;
        validate_transfer_source(
            request.kind,
            &source_metadata,
            source_policy.follow_source_symlinks,
        )
        .map_err(|error| transfer_error(error, None, context.actual_destination.clone()))?;

        let destination = match context.actual_destination.clone() {
            Some(destination) => destination,
            None => choose_destination(
                destination_session,
                &request.destination,
                request.conflict_policy,
            )
            .await
            .map_err(|error| transfer_error(error, None, None))?,
        };
        let destination_metadata = destination_session
            .metadata(&destination)
            .await
            .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        validate_destination(
            request.kind,
            destination_metadata.as_ref(),
            request.conflict_policy,
        )
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;

        let directory_copy_enters_source = if request.kind == TransferKind::Directory
            && request.source.same_filesystem(&destination)
        {
            if is_same_or_descendant(&request.source, &destination) {
                true
            } else {
                match (&request.source, &destination) {
                    (
                        FileEndpoint::Local { path: source },
                        FileEndpoint::Local { path: target },
                    ) => local_destination_resolves_within_source(
                        source,
                        target,
                        &context.cancellation,
                    )
                    .await
                    .map_err(|error| transfer_error(error, None, Some(destination.clone())))?,
                    _ => false,
                }
            }
        } else {
            false
        };
        if directory_copy_enters_source {
            return Err(Box::new(transfer_error(
                AppError::invalid("目录不能复制到自身或其子目录"),
                None,
                Some(destination),
            )));
        }

        let initial_bytes = context
            .resume_from
            .as_ref()
            .map(|checkpoint| checkpoint.confirmed_bytes)
            .unwrap_or(0);
        let total = (request.kind == TransferKind::File).then_some(source_metadata.size);
        context
            .progress
            .begin_transfer(total)
            .await
            .map_err(|error| {
                transfer_error(
                    error,
                    context.resume_from.clone(),
                    Some(destination.clone()),
                )
            })?;
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: initial_bytes,
            total_bytes: total,
            phase: TransferPhase::Preparing,
            actual_destination: Some(destination.clone()),
            checkpoint: context.resume_from.clone(),
            ..TransferProgressUpdate::default()
        });

        if destination_metadata.is_some() && request.conflict_policy == ConflictPolicy::Skip {
            if context.resume_from.is_some() || initial_bytes != 0 {
                return Err(Box::new(transfer_error(
                    AppError::conflict("续传目标已出现冲突，未覆盖现有文件"),
                    None,
                    Some(destination),
                )));
            }
            return Ok(TransferExecutionResult {
                actual_destination: destination,
                total_bytes: None,
                skipped: true,
                unchanged: false,
            });
        }

        match request.kind {
            TransferKind::File => {
                self.transfer_file(
                    context,
                    source_session,
                    destination_session,
                    destination,
                    staging,
                )
                .await
            }
            TransferKind::Directory => {
                self.transfer_directory(
                    context,
                    source_session,
                    destination_session,
                    destination,
                    destination_metadata.is_some(),
                    staging,
                )
                .await
            }
        }
    }

    /// 先验证完整源 revision，再流式写入受 owner 绑定的同目录 partial；每个检查点均可安全截断后续尾部。
    /// 单文件暂存先可靠登记再写入；仅失败时盒装完整状态，保留 checkpoint 而缩小 future 结果体积。
    ///
    /// @author kongweiguang
    async fn transfer_file(
        &self,
        context: &TransferExecutionContext,
        source_session: &EndpointSession,
        destination_session: &EndpointSession,
        destination: FileEndpoint,
        staging_tracker: &StagingCleanupTracker,
    ) -> CompactTransferResult<TransferExecutionResult> {
        let request = &context.request;
        let options = effective_sftp_options(
            &request.source,
            &request.destination,
            context.source_sftp.as_ref(),
            context.destination_sftp.as_ref(),
        );
        let source_scope = source_file_scope(
            source_session,
            &request.source,
            options.follow_source_symlinks,
        )
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        let (source, source_before) = resolve_source_object(
            source_session,
            &request.source,
            options.follow_source_symlinks,
            source_scope.as_ref(),
        )
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        if source_before.kind != FileEntryKind::File {
            return Err(Box::new(transfer_error(
                AppError::conflict("文件传输源不是普通文件"),
                None,
                Some(destination),
            )));
        }

        let resume = context.resume_from.as_ref();
        let staging_token = resume
            .map(|checkpoint| checkpoint.staging_token.clone())
            .unwrap_or_else(|| Uuid::new_v4().simple().to_string());
        let resume_offset = resume
            .map(|checkpoint| checkpoint.confirmed_bytes)
            .unwrap_or(0);
        let (source_revision, source_hash, source_prefix_digest) = hash_source(
            source_session,
            &source,
            &source_before,
            resume_offset,
            resume_offset,
            context,
            &destination,
        )
        .await?;
        if let Some(checkpoint) = resume
            && (checkpoint.source_revision != source_revision
                || checkpoint.actual_destination != destination
                || !checkpoint_owner_matches(
                    &checkpoint.staging_destination,
                    &checkpoint.staging_owner,
                )
                || checkpoint.confirmed_prefix_sha256 != source_prefix_digest
                || checkpoint.confirmed_bytes > source_revision.size)
        {
            return Err(Box::new(transfer_error(
                AppError::conflict("传输源或可信断点已变化，不能继续续传"),
                None,
                Some(destination),
            )));
        }

        let staging = if let Some(checkpoint) = resume {
            validate_staging_checkpoint(
                destination_session,
                checkpoint,
                &destination,
                context,
                &source_prefix_digest,
            )
            .await?
        } else {
            create_staging_sibling(&destination, &context.transfer_id)
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?
        };

        let (copied_bytes, final_checkpoint) = match self
            .stream_file(StreamTransferPlan {
                context,
                source_session,
                destination_session,
                source: &source,
                destination_stage: &staging,
                staging_token,
                staging_tracker,
                public_destination: &destination,
                recovery_destination: Some(&destination),
                offset: resume_offset,
                hasher: source_hash,
                base_confirmed: resume_offset,
                source_revision: Some(&source_revision),
                resume_existing: resume.is_some(),
                report_progress: true,
                allow_checkpoint: true,
            })
            .await
        {
            Ok(result) => result,
            Err(error) => return Err(error),
        };
        if copied_bytes.saturating_add(resume_offset) != source_revision.size
            || final_checkpoint.as_ref().is_none_or(|checkpoint| {
                checkpoint.confirmed_bytes != source_revision.size
                    || checkpoint.source_revision.content_digest != source_revision.content_digest
            })
        {
            return Err(Box::new(transfer_error(
                AppError::conflict("传输字节与预检源 revision 不一致"),
                None,
                Some(destination),
            )));
        }
        let Some(final_checkpoint) = final_checkpoint else {
            return Err(Box::new(transfer_error(
                AppError::infrastructure("传输完成但未形成可信提交状态"),
                None,
                Some(destination),
            )));
        };
        if let Err(error) = apply_source_modified_time(
            destination_session,
            &staging,
            &source_before,
            options.preserve_mtime,
        )
        .await
        {
            return Err(Box::new(transfer_error(
                error,
                Some(final_checkpoint),
                Some(destination),
            )));
        }
        if let Err(error) = destination_session
            .set_permissions(&staging, source_before.permissions)
            .await
        {
            return Err(Box::new(transfer_error(
                error,
                Some(final_checkpoint),
                Some(destination),
            )));
        }
        if let Err(error) =
            confirm_source_revision(source_session, &source, &source_before, &source_revision).await
        {
            return Err(Box::new(transfer_error(error, None, Some(destination))));
        }
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(
                Some(final_checkpoint),
                Some(destination),
            )));
        }
        let mode = match request.conflict_policy {
            ConflictPolicy::Overwrite => match destination_session
                .metadata(&destination)
                .await
                .map_err(|error| {
                    transfer_error(
                        error,
                        Some(final_checkpoint.clone()),
                        Some(destination.clone()),
                    )
                })? {
                Some(metadata) if metadata.kind == FileEntryKind::File => {
                    FileCommitMode::ReplaceExisting
                }
                Some(_) => {
                    return Err(Box::new(transfer_error(
                        AppError::conflict("覆盖目标不再是普通文件"),
                        None,
                        Some(destination),
                    )));
                }
                None => FileCommitMode::CreateNew,
            },
            ConflictPolicy::Rename | ConflictPolicy::Skip => FileCommitMode::CreateNew,
        };
        context
            .progress
            .begin_commit(
                destination.clone(),
                source_revision.size,
                Some(source_revision.size),
                Some(final_checkpoint.clone()),
            )
            .await
            .map_err(|error| {
                transfer_error(
                    error,
                    Some(final_checkpoint.clone()),
                    Some(destination.clone()),
                )
            })?;
        match destination_session
            .commit_file(
                &self.files,
                &staging,
                &destination,
                mode,
                context.cancellation.clone(),
            )
            .await
        {
            Ok(()) => {
                staging_tracker.mark_committed(&staging);
                context
                    .progress
                    .record_committed_entry(source_revision.size)
                    .await
                    .map_err(|error| unknown_commit(error, Some(destination.clone())))?;
                context
                    .progress
                    .finish_commit()
                    .await
                    .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                Ok(TransferExecutionResult {
                    actual_destination: destination,
                    total_bytes: Some(source_revision.size),
                    skipped: false,
                    unchanged: false,
                })
            }
            Err(CommitFailure::Unknown(error)) => {
                Err(Box::new(unknown_commit(error, Some(destination))))
            }
            Err(CommitFailure::Known(error)) => {
                context
                    .progress
                    .confirm_no_change()
                    .await
                    .map_err(|acknowledgment| {
                        transfer_error(
                            acknowledgment,
                            Some(final_checkpoint.clone()),
                            Some(destination.clone()),
                        )
                    })?;
                Err(Box::new(transfer_error(
                    error,
                    Some(final_checkpoint),
                    Some(destination),
                )))
            }
        }
    }

    /// 目录先完整复制到同级隐藏 staging，未冲突目标以 no-clobber rename 发布，已有目录按文件原子合并。
    /// 规范源根与目标真实父目录先做 containment 校验；目标叶不规范化，且该门禁早于 stage 所有权登记。
    /// 根暂存创建前先持久登记所有权，目录内子项沿用该根记录，树外 merge 暂存仍独立登记。
    /// 根目录 rename 会连同 staging 中的文件 sidecar 一起发布，因此发布前必须先校验并退役已提交的子文件 marker；root marker 保留至最终提交结果明确。
    /// 内部失败以盒装形式传播，不跳过目录项、回执、权限应用或原子发布步骤。
    ///
    /// @author kongweiguang
    async fn transfer_directory(
        &self,
        context: &TransferExecutionContext,
        source_session: &EndpointSession,
        destination_session: &EndpointSession,
        destination: FileEndpoint,
        merge_existing: bool,
        staging_tracker: &StagingCleanupTracker,
    ) -> CompactTransferResult<TransferExecutionResult> {
        if context.resume_from.is_some() {
            return Err(Box::new(transfer_error(
                AppError::unsupported("目录传输尚无可信的多文件恢复清单"),
                None,
                Some(destination),
            )));
        }
        let options = effective_sftp_options(
            &context.request.source,
            &context.request.destination,
            context.source_sftp.as_ref(),
            context.destination_sftp.as_ref(),
        );
        let (source_root, source_root_metadata) = resolve_source_object(
            source_session,
            &context.request.source,
            options.follow_source_symlinks,
            None,
        )
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        if source_root_metadata.kind != FileEntryKind::Directory {
            return Err(Box::new(transfer_error(
                AppError::conflict("目录传输源不是普通目录"),
                None,
                Some(destination),
            )));
        }
        ensure_directory_destination_outside_source(
            source_session,
            destination_session,
            &source_root,
            &destination,
        )
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        let source_scope = options.follow_source_symlinks.then(|| source_root.clone());
        let staging = create_staging_sibling(&destination, &context.transfer_id)
            .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        let staging_token = Uuid::new_v4().simple().to_string();
        let mut root_artifact = StagingArtifact::new(
            staging.clone(),
            context.transfer_id.clone(),
            staging_token,
            StagingKind::Tree,
        )
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        root_artifact
            .register_recovery(context, &destination)
            .await
            .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
        staging_tracker.register(root_artifact.clone());
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(None, Some(destination))));
        }
        let mut total_bytes = 0_u64;
        let mut source_directory_modes = HashMap::new();
        let mut source_directory_mtimes = HashMap::new();
        let mut staging_directories_postorder = Vec::new();
        let stage_result = async {
            destination_session
                .create_staging_marker(&root_artifact)
                .await
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
            destination_session
                .create_directory(&staging)
                .await
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
            staging_tracker.mark_stage_created(&staging);
            destination_session
                .set_permissions(&staging, Some(0o700))
                .await
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
            let mut pending = vec![(source_root.clone(), staging.clone(), false, 0_usize)];
            let mut visited_source_directories = HashSet::new();
            while let Some((source_directory, staging_directory, visited, depth)) = pending.pop() {
                if context.cancellation.is_cancelled() {
                    return Err(Box::new(cancelled(None, Some(destination.clone()))));
                }
                if visited {
                    let source_metadata = source_session
                        .metadata(&source_directory)
                        .await
                        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?
                        .ok_or_else(|| {
                            transfer_error(
                                AppError::not_found("目录传输源在读取期间消失"),
                                None,
                                Some(destination.clone()),
                            )
                        })?;
                    if source_metadata.kind != FileEntryKind::Directory {
                        return Err(Box::new(transfer_error(
                            AppError::conflict("目录传输源在遍历期间改变了类型"),
                            None,
                            Some(destination.clone()),
                        )));
                    }
                    source_directory_modes
                        .insert(staging_directory.clone(), source_metadata.permissions);
                    source_directory_mtimes
                        .insert(staging_directory.clone(), source_metadata.modified_seconds);
                    staging_directories_postorder.push(staging_directory.clone());
                    continue;
                }
                register_source_directory(
                    &mut visited_source_directories,
                    &source_directory,
                    depth,
                )
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                pending.push((
                    source_directory.clone(),
                    staging_directory.clone(),
                    true,
                    depth,
                ));
                let entries = source_session
                    .list_children(&source_directory)
                    .await
                    .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                for (listed_child, name, listed_kind, size) in entries {
                    let (source_child, kind) = if listed_kind == FileEntryKind::Symlink {
                        if !options.follow_source_symlinks {
                            return Err(Box::new(transfer_error(
                                AppError::unsupported("目录传输的源符号链接跟随未启用"),
                                None,
                                Some(destination.clone()),
                            )));
                        }
                        let (resolved, metadata) = resolve_source_object(
                            source_session,
                            &listed_child,
                            true,
                            source_scope.as_ref(),
                        )
                        .await
                        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                        (resolved, metadata.kind)
                    } else {
                        (listed_child, listed_kind)
                    };
                    let staging_child = sibling_child(&staging_directory, &name)
                        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                    match kind {
                        FileEntryKind::Directory => {
                            destination_session
                                .create_directory(&staging_child)
                                .await
                                .map_err(|error| {
                                    transfer_error(error, None, Some(destination.clone()))
                                })?;
                            destination_session
                                .set_permissions(&staging_child, Some(0o700))
                                .await
                                .map_err(|error| {
                                    transfer_error(error, None, Some(destination.clone()))
                                })?;
                            pending.push((source_child, staging_child, false, depth + 1));
                        }
                        FileEntryKind::File => {
                            self.copy_file_into_tree(CopyFileIntoTreePlan {
                                context,
                                source_session,
                                destination_session,
                                source: &source_child,
                                destination_in_tree: &staging_child,
                                public_destination: &destination,
                                total_bytes: &mut total_bytes,
                                staging_tracker,
                                preserve_mtime: options.preserve_mtime,
                            })
                            .await?;
                            let _ = size;
                        }
                        FileEntryKind::Symlink => {
                            return Err(Box::new(transfer_error(
                                AppError::conflict("规范化后的源目录项仍是符号链接"),
                                None,
                                Some(destination.clone()),
                            )));
                        }
                        FileEntryKind::Other => {
                            return Err(Box::new(transfer_error(
                                AppError::unsupported("目录包含当前不支持的特殊文件"),
                                None,
                                Some(destination.clone()),
                            )));
                        }
                    }
                }
            }
            Ok::<(), Box<TransferExecutionError>>(())
        }
        .await;

        stage_result?;
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(None, Some(destination))));
        }
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: total_bytes,
            total_bytes: Some(total_bytes),
            phase: TransferPhase::Verifying,
            actual_destination: Some(destination.clone()),
            ..TransferProgressUpdate::default()
        });
        if merge_existing {
            let merge_result = self
                .merge_staged_directory(MergeStagedDirectoryPlan {
                    context,
                    destination_session,
                    staging: &staging,
                    destination: &destination,
                    total_bytes,
                    staging_tracker,
                    source_directory_modes: &source_directory_modes,
                    source_directory_mtimes: &source_directory_mtimes,
                    preserve_mtime: options.preserve_mtime,
                })
                .await;
            let changed = match merge_result {
                Ok(changed) => changed,
                Err(CommitFailure::Unknown(error)) => {
                    return Err(Box::new(unknown_commit(error, Some(destination))));
                }
                Err(CommitFailure::Known(error)) => {
                    return Err(Box::new(transfer_error(error, None, Some(destination))));
                }
            };
            let barrier = if changed {
                context.progress.finish_commit().await
            } else {
                context.progress.finish_no_changes().await
            };
            if let Err(error) = barrier {
                return Err(Box::new(transfer_error(error, None, Some(destination))));
            }
            Ok(TransferExecutionResult {
                actual_destination: destination,
                total_bytes: Some(total_bytes),
                skipped: false,
                unchanged: !changed,
            })
        } else {
            staging_tracker
                .retire_committed_file_markers_before_tree_publish(destination_session, &staging)
                .await
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
            if context.cancellation.is_cancelled() {
                return Err(Box::new(cancelled(None, Some(destination.clone()))));
            }
            context
                .progress
                .begin_commit(destination.clone(), total_bytes, Some(total_bytes), None)
                .await
                .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
            if let Err(error) = apply_staging_directory_modes(
                destination_session,
                &staging_directories_postorder,
                &source_directory_modes,
                &source_directory_mtimes,
                options.preserve_mtime,
            )
            .await
            {
                if let Err(restore_error) = restore_staging_directory_modes(
                    destination_session,
                    &staging_directories_postorder,
                )
                .await
                {
                    tracing::warn!(
                        error_code = restore_error.public_code().as_str(),
                        "目录权限应用失败后，暂存权限恢复仍待清理 owner 重试"
                    );
                }
                context
                    .progress
                    .confirm_no_change()
                    .await
                    .map_err(|acknowledgment| {
                        transfer_error(acknowledgment, None, Some(destination.clone()))
                    })?;
                return Err(Box::new(transfer_error(error, None, Some(destination))));
            }
            match destination_session
                .commit_directory(&staging, &destination)
                .await
            {
                Ok(()) => {
                    staging_tracker.mark_committed(&staging);
                    context
                        .progress
                        .record_committed_entry(total_bytes)
                        .await
                        .map_err(|error| unknown_commit(error, Some(destination.clone())))?;
                    context
                        .progress
                        .finish_commit()
                        .await
                        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
                    Ok(TransferExecutionResult {
                        actual_destination: destination,
                        total_bytes: Some(total_bytes),
                        skipped: false,
                        unchanged: false,
                    })
                }
                Err(CommitFailure::Unknown(error)) => {
                    Err(Box::new(unknown_commit(error, Some(destination))))
                }
                Err(CommitFailure::Known(error)) => {
                    if let Err(restore_error) = restore_staging_directory_modes(
                        destination_session,
                        &staging_directories_postorder,
                    )
                    .await
                    {
                        tracing::warn!(
                            error_code = restore_error.public_code().as_str(),
                            "目录原子发布确认未发生后，暂存权限恢复仍待清理 owner 重试"
                        );
                    }
                    context
                        .progress
                        .confirm_no_change()
                        .await
                        .map_err(|acknowledgment| {
                            transfer_error(acknowledgment, None, Some(destination.clone()))
                        })?;
                    Err(Box::new(transfer_error(error, None, Some(destination))))
                }
            }
        }
    }

    /// 把目录中的单个普通文件写入私有 staging 子项，摘要阶段沿用整树已确认字节作为进度基数。
    /// 使用同一份树级操作计划避免进度引用与 cleanup owner 在多参数调用中错位。
    ///
    /// @author kongweiguang
    async fn copy_file_into_tree(
        &self,
        plan: CopyFileIntoTreePlan<'_>,
    ) -> CompactTransferResult<()> {
        let CopyFileIntoTreePlan {
            context,
            source_session,
            destination_session,
            source,
            destination_in_tree,
            public_destination,
            total_bytes,
            staging_tracker,
            preserve_mtime,
        } = plan;
        let metadata = source_session
            .metadata(source)
            .await
            .map_err(|error| transfer_error(error, None, Some(public_destination.clone())))?
            .ok_or_else(|| {
                transfer_error(
                    AppError::not_found("目录中的源文件不存在"),
                    None,
                    Some(public_destination.clone()),
                )
            })?;
        if metadata.kind != FileEntryKind::File {
            return Err(Box::new(transfer_error(
                AppError::conflict("目录项在传输期间改变了类型"),
                None,
                Some(public_destination.clone()),
            )));
        }
        let (source_revision, source_hash, _) = hash_source(
            source_session,
            source,
            &metadata,
            0,
            *total_bytes,
            context,
            public_destination,
        )
        .await?;
        let temporary = temporary_sibling(destination_in_tree)
            .map_err(|error| transfer_error(error, None, Some(public_destination.clone())))?;
        let copied = self
            .stream_file(StreamTransferPlan {
                context,
                source_session,
                destination_session,
                source,
                destination_stage: &temporary,
                staging_token: Uuid::new_v4().simple().to_string(),
                staging_tracker,
                public_destination,
                recovery_destination: None,
                offset: 0,
                hasher: source_hash,
                base_confirmed: *total_bytes,
                source_revision: Some(&source_revision),
                resume_existing: false,
                report_progress: true,
                allow_checkpoint: false,
            })
            .await;
        match copied {
            Ok((bytes, _)) => {
                *total_bytes = total_bytes.saturating_add(bytes);
                if let Err(error) = apply_source_modified_time(
                    destination_session,
                    &temporary,
                    &metadata,
                    preserve_mtime,
                )
                .await
                {
                    return Err(Box::new(transfer_error(
                        error,
                        None,
                        Some(public_destination.clone()),
                    )));
                }
                if let Err(error) = destination_session
                    .set_permissions(&temporary, metadata.permissions)
                    .await
                {
                    return Err(Box::new(transfer_error(
                        error,
                        None,
                        Some(public_destination.clone()),
                    )));
                }
                match destination_session
                    .commit_file(
                        &self.files,
                        &temporary,
                        destination_in_tree,
                        FileCommitMode::CreateNew,
                        context.cancellation.clone(),
                    )
                    .await
                {
                    Ok(()) => {
                        staging_tracker.mark_committed(&temporary);
                        Ok(())
                    }
                    Err(CommitFailure::Known(error)) => Err(Box::new(transfer_error(
                        error,
                        None,
                        Some(public_destination.clone()),
                    ))),
                    Err(CommitFailure::Unknown(error)) => Err(Box::new(unknown_commit(
                        error,
                        Some(public_destination.clone()),
                    ))),
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 将 staging 树按项合并，逐项记录服务端回执；仅新目录恢复源 mtime/权限，已有目录元数据不覆盖。
    /// 参数计划仅收纳单次目录合并的输入快照；目录项逐项提交，失败时保留既有部分提交语义。
    ///
    /// @author kongweiguang
    async fn merge_staged_directory(
        &self,
        plan: MergeStagedDirectoryPlan<'_>,
    ) -> Result<bool, CommitFailure> {
        let MergeStagedDirectoryPlan {
            context,
            destination_session,
            staging,
            destination,
            total_bytes,
            staging_tracker,
            source_directory_modes,
            source_directory_mtimes,
            preserve_mtime,
        } = plan;
        let mut pending = vec![DirectoryMergeFrame {
            source: staging.clone(),
            destination: destination.clone(),
            created_permissions: None,
            created_modified_seconds: None,
            postorder: false,
        }];
        let mut changed = false;
        while let Some(frame) = pending.pop() {
            if frame.postorder {
                if let Some(seconds) = frame.created_modified_seconds {
                    destination_session
                        .set_modified_time(&frame.destination, seconds)
                        .await
                        .map_err(CommitFailure::Known)?;
                }
                if let Some(mode) = frame.created_permissions {
                    destination_session
                        .set_permissions(&frame.destination, Some(mode))
                        .await
                        .map_err(CommitFailure::Known)?;
                }
                continue;
            }
            if context.cancellation.is_cancelled() {
                return Err(CommitFailure::Known(AppError::cancelled("目录合并已取消")));
            }
            let entries = destination_session
                .list_children(&frame.source)
                .await
                .map_err(CommitFailure::Known)?;
            if context.cancellation.is_cancelled() {
                return Err(CommitFailure::Known(AppError::cancelled("目录合并已取消")));
            }
            pending.push(DirectoryMergeFrame {
                source: frame.source.clone(),
                destination: frame.destination.clone(),
                created_permissions: frame.created_permissions,
                created_modified_seconds: frame.created_modified_seconds,
                postorder: true,
            });
            for (source_child, name, kind, size) in entries {
                if context.cancellation.is_cancelled() {
                    return Err(CommitFailure::Known(AppError::cancelled("目录合并已取消")));
                }
                if staging_tracker.is_marker_endpoint(&source_child) {
                    continue;
                }
                let destination_child =
                    sibling_child(&frame.destination, &name).map_err(CommitFailure::Known)?;
                match kind {
                    FileEntryKind::Directory => {
                        let source_metadata = destination_session
                            .metadata(&source_child)
                            .await
                            .map_err(CommitFailure::Known)?
                            .ok_or_else(|| {
                                CommitFailure::Known(AppError::not_found(
                                    "目录暂存项在合并期间消失",
                                ))
                            })?;
                        if source_metadata.kind != FileEntryKind::Directory {
                            return Err(CommitFailure::Known(AppError::conflict(
                                "目录暂存项在合并期间改变了类型",
                            )));
                        }
                        match destination_session.metadata(&destination_child).await {
                            Ok(Some(metadata)) if metadata.kind == FileEntryKind::Directory => {
                                pending.push(DirectoryMergeFrame {
                                    source: source_child,
                                    destination: destination_child,
                                    created_permissions: None,
                                    created_modified_seconds: None,
                                    postorder: false,
                                });
                                continue;
                            }
                            Ok(Some(_)) => {
                                return Err(CommitFailure::Known(AppError::conflict(
                                    "目录合并目标类型不匹配",
                                )));
                            }
                            Ok(None) => {}
                            Err(error) => return Err(CommitFailure::Known(error)),
                        }
                        let created_permissions = source_directory_modes
                            .get(&source_child)
                            .copied()
                            .ok_or_else(|| {
                                CommitFailure::Known(AppError::conflict(
                                    "目录权限快照缺少已遍历的暂存目录",
                                ))
                            })?;
                        let created_modified_seconds = if preserve_mtime {
                            Some(
                                source_directory_mtimes
                                    .get(&source_child)
                                    .copied()
                                    .flatten()
                                    .ok_or_else(|| {
                                        CommitFailure::Known(AppError::unsupported(
                                            "源目录未提供可保留的修改时间",
                                        ))
                                    })?,
                            )
                        } else {
                            None
                        };
                        context
                            .progress
                            .begin_commit(destination.clone(), total_bytes, Some(total_bytes), None)
                            .await
                            .map_err(CommitFailure::Known)?;
                        match destination_session
                            .create_directory_for_commit(&destination_child)
                            .await
                        {
                            Ok(()) => {
                                context.progress.record_committed_entry(0).await.map_err(
                                    |error| {
                                        CommitFailure::Unknown(
                                            AppError::outcome_unknown_with_source(
                                                "目录项已创建但提交回执未被队列确认",
                                                error,
                                            ),
                                        )
                                    },
                                )?;
                                changed = true;
                            }
                            Err(CommitFailure::Known(error)) => {
                                context
                                    .progress
                                    .confirm_no_change()
                                    .await
                                    .map_err(CommitFailure::Known)?;
                                return Err(CommitFailure::Known(error));
                            }
                            Err(CommitFailure::Unknown(error)) => {
                                return Err(CommitFailure::Unknown(error));
                            }
                        }
                        pending.push(DirectoryMergeFrame {
                            source: source_child,
                            destination: destination_child,
                            created_permissions,
                            created_modified_seconds,
                            postorder: false,
                        });
                    }
                    FileEntryKind::File => {
                        let metadata = destination_session
                            .metadata(&source_child)
                            .await
                            .map_err(CommitFailure::Known)?
                            .ok_or_else(|| {
                                CommitFailure::Known(AppError::not_found("目录暂存文件不存在"))
                            })?;
                        let exists = destination_session
                            .metadata(&destination_child)
                            .await
                            .map_err(CommitFailure::Known)?;
                        if exists
                            .as_ref()
                            .is_some_and(|item| item.kind != FileEntryKind::File)
                        {
                            return Err(CommitFailure::Known(AppError::conflict(
                                "目录合并文件目标类型不匹配",
                            )));
                        }
                        let temporary =
                            temporary_sibling(&destination_child).map_err(CommitFailure::Known)?;
                        let copied = match self
                            .stream_file(StreamTransferPlan {
                                context,
                                source_session: destination_session,
                                destination_session,
                                source: &source_child,
                                destination_stage: &temporary,
                                staging_token: Uuid::new_v4().simple().to_string(),
                                staging_tracker,
                                public_destination: destination,
                                recovery_destination: Some(&destination_child),
                                offset: 0,
                                hasher: Sha256::new(),
                                base_confirmed: 0,
                                source_revision: None,
                                resume_existing: false,
                                report_progress: false,
                                allow_checkpoint: false,
                            })
                            .await
                        {
                            Ok(copied) => copied,
                            Err(error) => match *error {
                                TransferExecutionError::Failed(_) => {
                                    return Err(CommitFailure::Known(AppError::infrastructure(
                                        "目录暂存文件复制失败",
                                    )));
                                }
                                TransferExecutionError::Cancelled { .. } => {
                                    return Err(CommitFailure::Known(AppError::cancelled(
                                        "目录合并已取消",
                                    )));
                                }
                            },
                        };
                        if copied.0 != metadata.size || copied.0 != size {
                            return Err(CommitFailure::Known(AppError::conflict(
                                "目录暂存文件在合并期间发生变化",
                            )));
                        }
                        if preserve_mtime {
                            let modified_seconds = metadata.modified_seconds.ok_or_else(|| {
                                CommitFailure::Known(AppError::unsupported(
                                    "目录暂存文件缺少可保留的修改时间",
                                ))
                            })?;
                            destination_session
                                .set_modified_time(&temporary, modified_seconds)
                                .await
                                .map_err(CommitFailure::Known)?;
                        }
                        let replacement_permissions = match exists.as_ref() {
                            Some(existing) => Some(existing.permissions.ok_or_else(|| {
                                CommitFailure::Known(AppError::unsupported(
                                    "现有远端文件未提供权限，无法安全保留后覆盖",
                                ))
                            })?),
                            None => metadata.permissions,
                        };
                        if let Err(error) = destination_session
                            .set_permissions(&temporary, replacement_permissions)
                            .await
                        {
                            return Err(CommitFailure::Known(error));
                        }
                        if context.cancellation.is_cancelled() {
                            return Err(CommitFailure::Known(AppError::cancelled(
                                "目录合并已取消",
                            )));
                        }
                        if let Err(error) = context
                            .progress
                            .begin_commit(destination.clone(), total_bytes, Some(total_bytes), None)
                            .await
                        {
                            return Err(CommitFailure::Known(error));
                        }
                        let mode = if exists.is_some() {
                            FileCommitMode::ReplaceExisting
                        } else {
                            FileCommitMode::CreateNew
                        };
                        match destination_session
                            .commit_file(
                                &self.files,
                                &temporary,
                                &destination_child,
                                mode,
                                context.cancellation.clone(),
                            )
                            .await
                        {
                            Ok(()) => {
                                staging_tracker.mark_committed(&temporary);
                                context
                                    .progress
                                    .record_committed_entry(metadata.size)
                                    .await
                                    .map_err(|error| {
                                        CommitFailure::Unknown(
                                            AppError::outcome_unknown_with_source(
                                                "目录文件已提交但回执未被队列确认",
                                                error,
                                            ),
                                        )
                                    })?;
                                changed = true;
                            }
                            Err(CommitFailure::Known(error)) => {
                                context
                                    .progress
                                    .confirm_no_change()
                                    .await
                                    .map_err(CommitFailure::Known)?;
                                return Err(CommitFailure::Known(error));
                            }
                            Err(CommitFailure::Unknown(error)) => {
                                return Err(CommitFailure::Unknown(error));
                            }
                        }
                    }
                    FileEntryKind::Symlink | FileEntryKind::Other => {
                        return Err(CommitFailure::Known(AppError::unsupported(
                            "目录合并遇到不支持的特殊文件",
                        )));
                    }
                }
            }
        }
        Ok(changed)
    }

    /// 按 64 KiB 有界块复制；SFTP 写确认和本机 flush 后才推进字节及可信前缀摘要。
    /// 独立暂存先取得持久化 owner 回执；树内子项沿用已登记根，不按展示用目录推断提交目标。
    /// 错误只在内部结果中盒装，取消携带的 checkpoint 与 staging 身份原样交还外层清理状态机。
    ///
    /// @author kongweiguang
    async fn stream_file(
        &self,
        plan: StreamTransferPlan<'_>,
    ) -> CompactTransferResult<(u64, Option<TransferCheckpoint>)> {
        let StreamTransferPlan {
            context,
            source_session,
            destination_session,
            source,
            destination_stage,
            staging_token,
            staging_tracker,
            public_destination,
            recovery_destination,
            offset,
            mut hasher,
            base_confirmed,
            source_revision,
            resume_existing,
            report_progress,
            allow_checkpoint,
        } = plan;
        let staging_owner = context
            .resume_from
            .as_ref()
            .map(|checkpoint| checkpoint.staging_owner.clone())
            .unwrap_or_else(|| context.transfer_id.clone());
        let mut staging_artifact = StagingArtifact::new(
            destination_stage.clone(),
            staging_owner.clone(),
            staging_token.clone(),
            StagingKind::File,
        )
        .map_err(|error| {
            transfer_error(
                error,
                context.resume_from.clone(),
                Some(public_destination.clone()),
            )
        })?;
        if let Some(actual_destination) = recovery_destination {
            staging_artifact
                .register_recovery(context, actual_destination)
                .await
                .map_err(|error| {
                    transfer_error(
                        error,
                        context.resume_from.clone(),
                        Some(public_destination.clone()),
                    )
                })?;
        }
        if resume_existing {
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(context.resume_from.clone(), Some(public_destination.clone()))));
                }
                result = destination_session.verify_staging_marker(&staging_artifact) => {
                    result.map_err(|error| transfer_error(
                        error,
                        context.resume_from.clone(),
                        Some(public_destination.clone()),
                    ))?;
                }
            }
            if !staging_tracker.contains(destination_stage) {
                let mut staging_artifact = staging_artifact;
                staging_artifact.stage_owned = true;
                staging_tracker.register(staging_artifact);
            }
        } else {
            if !staging_tracker.contains(destination_stage) {
                staging_tracker.register(staging_artifact.clone());
                if context.cancellation.is_cancelled() {
                    return Err(Box::new(cancelled(
                        context.resume_from.clone(),
                        Some(public_destination.clone()),
                    )));
                }
                destination_session
                    .create_staging_marker(&staging_artifact)
                    .await
                    .map_err(|error| {
                        transfer_error(error, None, Some(public_destination.clone()))
                    })?;
            }
        }
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(
                context.resume_from.clone(),
                Some(public_destination.clone()),
            )));
        }
        let mut reader = source_session
            .open_reader(source)
            .await
            .map_err(|error| transfer_error(error, None, Some(public_destination.clone())))?;
        if offset > 0 {
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(
                        context.resume_from.clone(),
                        Some(public_destination.clone()),
                    )));
                }
                result = reader.seek(SeekFrom::Start(offset)) => {
                    result.map_err(|error| transfer_error(
                        map_io_error(error), None, Some(public_destination.clone()),
                    ))?;
                }
            }
        }
        let mut writer = tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                return Err(Box::new(cancelled(
                    context.resume_from.clone(),
                    Some(public_destination.clone()),
                )));
            }
            result = destination_session.open_writer(destination_stage, !resume_existing) => {
                result.map_err(|error| transfer_error(error, None, Some(public_destination.clone())))?
            }
        };
        if !resume_existing {
            staging_tracker.mark_stage_created(destination_stage);
        }
        if !resume_existing
            && let Err(error) = destination_session
                .set_permissions(destination_stage, Some(0o600))
                .await
        {
            drop(writer);
            return Err(Box::new(transfer_error(
                error,
                None,
                Some(public_destination.clone()),
            )));
        }
        if offset > 0 {
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(
                        context.resume_from.clone(),
                        Some(public_destination.clone()),
                    )));
                }
                result = writer.seek(SeekFrom::Start(offset)) => {
                    result.map_err(|error| transfer_error(
                        map_io_error(error), None, Some(public_destination.clone()),
                    ))?;
                }
            }
        }

        let packet_bytes = context
            .sftp_policy
            .map(|policy| policy.packet_bytes() as usize)
            .unwrap_or(COPY_BUFFER_BYTES);
        let mut buffer = vec![0_u8; packet_bytes];
        let mut copied = 0_u64;
        let mut confirmed = offset;
        let mut last_checkpoint_at = offset;
        let mut checkpoint = context.resume_from.clone();
        loop {
            if context.cancellation.is_cancelled() {
                drop(writer);
                return Err(Box::new(cancelled(
                    checkpoint,
                    Some(public_destination.clone()),
                )));
            }
            let read = tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
                }
                result = reader.read(&mut buffer) => result.map_err(|error| {
                    transfer_error(
                        map_io_error(error),
                        checkpoint.clone(),
                        Some(public_destination.clone()),
                    )
                })?,
            };
            if read == 0 {
                break;
            }
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
                }
                result = writer.write_all(&buffer[..read]) => result.map_err(|error| {
                    transfer_error(
                        map_io_error(error),
                        checkpoint.clone(),
                        Some(public_destination.clone()),
                    )
                })?,
            }
            tokio::select! {
                biased;
                _ = context.cancellation.cancelled() => {
                    return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
                }
                result = writer.flush() => result.map_err(|error| {
                    transfer_error(
                        map_io_error(error),
                        checkpoint.clone(),
                        Some(public_destination.clone()),
                    )
                })?,
            }
            hasher.update(&buffer[..read]);
            copied = copied.saturating_add(read as u64);
            confirmed = confirmed.saturating_add(read as u64);

            let checkpoint_due = allow_checkpoint
                && source_revision.is_some_and(|revision| {
                    confirmed.saturating_sub(last_checkpoint_at) >= CHECKPOINT_INTERVAL_BYTES
                        || confirmed == revision.size
                });
            if checkpoint_due && let Some(revision) = source_revision {
                tokio::select! {
                    biased;
                    _ = context.cancellation.cancelled() => {
                        return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
                    }
                    result = destination_session.sync_file(destination_stage) => result.map_err(|error| {
                        transfer_error(
                            error,
                            checkpoint.clone(),
                            Some(public_destination.clone()),
                        )
                    })?,
                }
                checkpoint = Some(TransferCheckpoint {
                    source_revision: revision.clone(),
                    confirmed_bytes: confirmed,
                    confirmed_prefix_sha256: sha256_hex(hasher.clone().finalize().as_slice()),
                    staging_owner: staging_owner.clone(),
                    staging_destination: destination_stage.clone(),
                    staging_token: staging_token.clone(),
                    actual_destination: public_destination.clone(),
                });
                last_checkpoint_at = confirmed;
            }
            if report_progress {
                context.progress.report(TransferProgressUpdate {
                    confirmed_bytes: base_confirmed.saturating_add(copied),
                    total_bytes: if context.request.kind == TransferKind::File {
                        source_revision.map(|revision| revision.size)
                    } else {
                        None
                    },
                    phase: TransferPhase::Transferring,
                    actual_destination: Some(public_destination.clone()),
                    checkpoint: checkpoint.clone(),
                    ..TransferProgressUpdate::default()
                });
            }
        }
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
            }
            result = writer.flush() => result.map_err(|error| {
                transfer_error(
                    map_io_error(error),
                    checkpoint.clone(),
                    Some(public_destination.clone()),
                )
            })?,
        }
        drop(writer);
        tokio::select! {
            biased;
            _ = context.cancellation.cancelled() => {
                return Err(Box::new(cancelled(checkpoint.clone(), Some(public_destination.clone()))));
            }
            result = destination_session.sync_file(destination_stage) => result.map_err(|error| {
                transfer_error(error, checkpoint.clone(), Some(public_destination.clone()))
            })?,
        }
        if let Some(revision) = source_revision.filter(|_| allow_checkpoint)
            && confirmed == revision.size
        {
            checkpoint = Some(TransferCheckpoint {
                source_revision: revision.clone(),
                confirmed_bytes: confirmed,
                confirmed_prefix_sha256: sha256_hex(hasher.clone().finalize().as_slice()),
                staging_owner,
                staging_destination: destination_stage.clone(),
                staging_token,
                actual_destination: public_destination.clone(),
            });
        }
        if let Some(revision) = source_revision
            && (confirmed != revision.size
                || revision.content_digest.as_ref().is_some_and(|expected| {
                    sha256_hex(hasher.clone().finalize().as_slice()) != expected.as_str()
                }))
        {
            return Err(Box::new(transfer_error(
                AppError::conflict("传输流内容与预检源摘要不一致"),
                checkpoint,
                Some(public_destination.clone()),
            )));
        }
        Ok((copied, checkpoint))
    }
}

#[async_trait::async_trait]
impl TransferBackend for SftpTransferBackend {
    /// 复用同一条受限流式后端处理本机与 SSH/SFTP 组合；容器仍由独立后端接入队列。
    ///
    /// @author kongweiguang
    fn supports(&self, request: &TransferRequest) -> bool {
        let local_or_remote = |endpoint: &FileEndpoint| {
            matches!(
                endpoint,
                FileEndpoint::Local { .. } | FileEndpoint::Remote { .. }
            )
        };
        local_or_remote(&request.source) && local_or_remote(&request.destination)
    }

    /// 仅含远端 endpoint 的请求申请 SFTP 策略配额，本机复制只占普通队列并发许可。
    ///
    /// @author kongweiguang
    fn uses_sftp_policy(&self, request: &TransferRequest) -> bool {
        self.supports(request)
            && (matches!(request.source, FileEndpoint::Remote { .. })
                || matches!(request.destination, FileEndpoint::Remote { .. }))
    }

    /// 只核对未知提交的原实际文件目标；没有完整源摘要时保留未知，绝不续传、重放或清理。
    /// 目录合并可能已经逐项提交，当前单文件摘要不足以证明整棵树，因此显式返回 Insufficient。
    /// 重试继任任务可沿用原暂存 owner；可信身份来自原检查点与命名绑定，而不是当前队列 ID。
    ///
    /// @author kongweiguang
    async fn inspect_recovery(
        &self,
        record: &TransferRecoveryRecord,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> AppResult<TransferRecoveryInspection> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输恢复核对已取消"));
        }
        let Some((destination, expected_sha256, expected_size)) = recovery_file_evidence(record)
        else {
            return Ok(TransferRecoveryInspection::Insufficient);
        };
        if !self.supports(&record.request) {
            return Ok(TransferRecoveryInspection::Insufficient);
        }

        let mut config = SftpClientConfig::default();
        if let Some(timeout) = record.request.idle_timeout_seconds {
            config.request_timeout_secs = u64::from(timeout);
        }
        let session =
            EndpointSession::open(&self.files, destination, &cancellation, config).await?;
        let inspection = inspect_recovery_destination(
            &session,
            destination,
            expected_size,
            expected_sha256,
            &cancellation,
        )
        .await;
        let cleanup = session
            .close_until(Instant::now() + Duration::from_secs(5))
            .await;
        match (inspection, cleanup) {
            (Ok(inspection), Ok(())) => Ok(inspection),
            (Err(error), Ok(())) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup_error)) => {
                tracing::warn!(
                    inspection_error_code = error.public_code().as_str(),
                    error_code = cleanup_error.public_code().as_str(),
                    "恢复核对失败后 SFTP endpoint 关闭仍待连接服务处理"
                );
                Err(cleanup_error)
            }
        }
    }

    /// 每个队列尝试独立建立端点连接，返回前释放本次 SFTP channel 与其专属 SSH lease。
    /// 纯本机复制使用默认 client 参数，绝不为占位配置打开 SSH route。
    /// 内部传输 helper 的盒装错误在此解箱，保证 application trait 与清理/未知提交分类契约不变。
    /// 暂存物理清理与 journal 释放回执分离；仅本轮 receiver 存活时补释放，不能让后续 owner 卡死在旧通道。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let requires_sftp_policy = matches!(context.request.source, FileEndpoint::Remote { .. })
            || matches!(context.request.destination, FileEndpoint::Remote { .. });
        let policy = context.sftp_policy;
        if requires_sftp_policy && policy.is_none() {
            return Err(transfer_error(
                AppError::infrastructure("SFTP 传输缺少入队时策略快照"),
                context.resume_from.clone(),
                context.actual_destination.clone(),
            ));
        }
        let request_idle_timeout = context.request.idle_timeout_seconds;
        let source_config = policy
            .map(|snapshot| sftp_client_config(snapshot, request_idle_timeout))
            .unwrap_or_default();
        let source_session = EndpointSession::open(
            &self.files,
            &context.request.source,
            &context.cancellation,
            source_config,
        )
        .await
        .map_err(|error| {
            transfer_error(
                error,
                context.resume_from.clone(),
                context.actual_destination.clone(),
            )
        })?;
        let destination_config = policy
            .map(|snapshot| sftp_client_config(snapshot, request_idle_timeout))
            .unwrap_or_default();
        let destination_session = match EndpointSession::open(
            &self.files,
            &context.request.destination,
            &context.cancellation,
            destination_config,
        )
        .await
        {
            Ok(session) => session,
            Err(error) => {
                let deadline = context
                    .shutdown_deadline()
                    .unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
                let source_close = source_session.close_until(deadline).await;
                let failure = transfer_error(
                    error,
                    context.resume_from.clone(),
                    context.actual_destination.clone(),
                );
                if let Err(close_error) = source_close {
                    tracing::error!(
                        error_code = close_error.public_code().as_str(),
                        "打开目标端点失败后仍未确认源连接关闭"
                    );
                    let cleanup = pending_cleanup_for_sessions(&[&source_session])
                        // 本机端点没有 route/子通道 owner；其关闭路径本身是无状态成功。
                        .unwrap_or(TransferCleanupState::Released);
                    return Err(with_pending_cleanup(failure, cleanup));
                }
                return Err(failure);
            }
        };
        let staging = StagingCleanupTracker::default();
        let outcome = self
            .execute_with_sessions(&context, &source_session, &destination_session, &staging)
            .await
            .map_err(|error| *error);
        preserve_staging_for_outcome(&staging, &outcome);
        let deadline = context
            .shutdown_deadline()
            .unwrap_or_else(|| Instant::now() + Duration::from_secs(5));
        let cleanup_owner = Arc::new(SftpTransferCleanup {
            sessions: vec![source_session.clone(), destination_session.clone()],
            staging: staging.clone(),
        });
        let recovery_ownerships = staging.recovery_ownerships();
        let cleanup_result = cleanup_owner.cleanup_until(deadline).await;
        staging
            .release_cleaned_recovery_ownerships(&context.progress, &recovery_ownerships, deadline)
            .await;
        if cleanup_result.is_ok() {
            return outcome;
        }
        let cleanup = TransferCleanupState::pending(cleanup_owner.clone());
        match outcome {
            Ok(result) => match context
                .progress
                .retain_cleanup_owner(cleanup_owner.clone())
                .await
            {
                Ok(()) => Ok(result),
                Err(_) => Err(TransferExecutionError::Failed(
                    TransferExecutionFailure::new(
                        TransferFailureKind::Internal,
                        false,
                        false,
                        None,
                        Some(result.actual_destination),
                        cleanup,
                    ),
                )),
            },
            Err(outcome) => Err(with_pending_cleanup(outcome, cleanup)),
        }
    }
}

/// 只有冻结策略明确允许源端跟随时才把链接交给后续规范化；解析后的类型与范围仍需再次验证。
///
/// @author kongweiguang
fn validate_transfer_source(
    kind: TransferKind,
    source: &ObjectMetadata,
    follow_source_symlinks: bool,
) -> AppResult<()> {
    match (kind, source.kind) {
        (TransferKind::File, FileEntryKind::File)
        | (TransferKind::Directory, FileEntryKind::Directory) => Ok(()),
        (_, FileEntryKind::Symlink) if follow_source_symlinks => Ok(()),
        (_, FileEntryKind::Symlink) => Err(AppError::unsupported("传输不跟随符号链接")),
        _ => Err(AppError::conflict("传输源类型与请求不匹配")),
    }
}

/// 冲突策略只作用于顶层目标，更新目录时拒绝把文件和目录互相覆盖。
/// 显式匹配允许的目标类型，保持 Skip 与 Overwrite 对文件和目录的校验相同。
///
/// @author kongweiguang
fn validate_destination(
    kind: TransferKind,
    destination: Option<&ObjectMetadata>,
    policy: ConflictPolicy,
) -> AppResult<()> {
    let Some(destination) = destination else {
        return Ok(());
    };
    if policy != ConflictPolicy::Overwrite && policy != ConflictPolicy::Skip {
        return Ok(());
    }
    let valid = matches!(
        (kind, destination.kind),
        (TransferKind::File, FileEntryKind::File)
            | (TransferKind::Directory, FileEntryKind::Directory)
    );
    if valid {
        Ok(())
    } else {
        Err(AppError::conflict("传输目标类型与源类型不匹配"))
    }
}

/// 依据稳定目标和显式冲突策略选名；Rename 候选必须与请求目标并列，不能把文件端点当作父目录。
/// 目标名只选择一次，后继重试沿用队列保存的端点；最终发布仍依赖 no-clobber 原子提交处理竞态。
///
/// @author kongweiguang
async fn choose_destination(
    session: &EndpointSession,
    requested: &FileEndpoint,
    policy: ConflictPolicy,
) -> AppResult<FileEndpoint> {
    let Some(_) = session.metadata(requested).await? else {
        return Ok(requested.clone());
    };
    match policy {
        ConflictPolicy::Skip | ConflictPolicy::Overwrite => Ok(requested.clone()),
        ConflictPolicy::Rename => {
            let name = endpoint_name(requested)?;
            let parent = endpoint_parent(requested)?;
            let (stem, extension) = split_extension(&name);
            for index in 1..=10_000_u32 {
                let suffix = if extension.is_empty() {
                    format!("{stem} ({index})")
                } else {
                    format!("{stem} ({index}).{extension}")
                };
                let candidate = sibling_child(&parent, &suffix)?;
                if session.metadata(&candidate).await?.is_none() {
                    return Ok(candidate);
                }
            }
            Err(AppError::conflict("无法为传输目标选择稳定的新名称"))
        }
    }
}

/// 逐块计算完整源摘要，并在续传偏移处保留 SHA 状态；目录项使用累计字节且不把单文件大小当整树总量。
///
/// @author kongweiguang
async fn hash_source(
    session: &EndpointSession,
    endpoint: &FileEndpoint,
    metadata: &ObjectMetadata,
    prefix_len: u64,
    progress_base: u64,
    context: &TransferExecutionContext,
    destination: &FileEndpoint,
) -> CompactTransferResult<(FileRevision, Sha256, String)> {
    if prefix_len > metadata.size {
        return Err(Box::new(transfer_error(
            AppError::conflict("续传断点超过源文件大小"),
            None,
            Some(destination.clone()),
        )));
    }
    let mut reader = session
        .open_reader(endpoint)
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
    let mut full = Sha256::new();
    let mut prefix = Sha256::new();
    let mut prefix_state = Sha256::new();
    let mut prefix_saved = prefix_len == 0;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    let mut bytes = 0_u64;
    loop {
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(
                context.resume_from.clone(),
                Some(destination.clone()),
            )));
        }
        let read = reader.read(&mut buffer).await.map_err(|error| {
            transfer_error(
                map_io_error(error),
                context.resume_from.clone(),
                Some(destination.clone()),
            )
        })?;
        if read == 0 {
            break;
        }
        let remaining_prefix = prefix_len.saturating_sub(bytes).min(read as u64) as usize;
        if remaining_prefix > 0 {
            prefix.update(&buffer[..remaining_prefix]);
        }
        full.update(&buffer[..read]);
        bytes = bytes.saturating_add(read as u64);
        if !prefix_saved && bytes >= prefix_len {
            prefix_state = prefix.clone();
            prefix_saved = true;
        }
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: progress_base,
            total_bytes: (context.request.kind == TransferKind::File).then_some(metadata.size),
            phase: TransferPhase::Verifying,
            actual_destination: Some(destination.clone()),
            checkpoint: context.resume_from.clone(),
            ..TransferProgressUpdate::default()
        });
    }
    if bytes != metadata.size || !prefix_saved {
        return Err(Box::new(transfer_error(
            AppError::conflict("传输源在读取 revision 时发生变化"),
            None,
            Some(destination.clone()),
        )));
    }
    let mut revision = metadata.revision();
    revision.content_digest = Some(sha256_hex(full.finalize().as_slice()));
    Ok((
        revision,
        prefix_state,
        sha256_hex(prefix.finalize().as_slice()),
    ))
}

/// 校验 checkpoint 的 owner、同目录目标、源摘要和 partial 前缀；只修剪摘要已确认后的尾部。
/// 仅在全部信任条件成立后才访问 partial，盒装错误仍完整携带恢复上下文供外层分类。
///
/// @author kongweiguang
async fn validate_staging_checkpoint(
    session: &EndpointSession,
    checkpoint: &TransferCheckpoint,
    destination: &FileEndpoint,
    context: &TransferExecutionContext,
    source_prefix_digest: &str,
) -> CompactTransferResult<FileEndpoint> {
    let staging = &checkpoint.staging_destination;
    if !staging.same_filesystem(destination)
        || !same_parent_directory(staging, destination)
        || staging.same_object(destination)
        || checkpoint.actual_destination != *destination
        || checkpoint.staging_owner.as_str()
            != context
                .resume_from
                .as_ref()
                .map(|item| item.staging_owner.as_str())
                .unwrap_or("")
        || !checkpoint_owner_matches(staging, &checkpoint.staging_owner)
        || !valid_staging_token(&checkpoint.staging_token)
        || checkpoint.confirmed_prefix_sha256 != source_prefix_digest
    {
        return Err(Box::new(transfer_error(
            AppError::conflict("传输断点未绑定当前目标"),
            None,
            Some(destination.clone()),
        )));
    }
    let artifact = StagingArtifact::new(
        staging.clone(),
        checkpoint.staging_owner.clone(),
        checkpoint.staging_token.clone(),
        StagingKind::File,
    )
    .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
    session
        .verify_staging_marker(&artifact)
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
    let metadata = session
        .metadata(staging)
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?
        .ok_or_else(|| {
            transfer_error(
                AppError::conflict("可信传输暂存文件已不存在"),
                None,
                Some(destination.clone()),
            )
        })?;
    if metadata.kind != FileEntryKind::File || metadata.size < checkpoint.confirmed_bytes {
        return Err(Box::new(transfer_error(
            AppError::conflict("可信传输暂存文件类型或长度无效"),
            None,
            Some(destination.clone()),
        )));
    }
    let digest = hash_prefix(
        session,
        staging,
        checkpoint.confirmed_bytes,
        context,
        destination,
    )
    .await?;
    if digest != checkpoint.confirmed_prefix_sha256 {
        return Err(Box::new(transfer_error(
            AppError::conflict("传输暂存文件前缀校验失败"),
            None,
            Some(destination.clone()),
        )));
    }
    session
        .truncate_file(staging, checkpoint.confirmed_bytes)
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
    Ok(staging.clone())
}

/// 分块读取指定前缀并校验短读；不把服务端 partial 长度直接当作可信断点。
/// 校验与源 revision 共用紧凑错误通道，短读仍失败关闭，不接受服务端长度作为可信前缀。
///
/// @author kongweiguang
async fn hash_prefix(
    session: &EndpointSession,
    endpoint: &FileEndpoint,
    length: u64,
    context: &TransferExecutionContext,
    destination: &FileEndpoint,
) -> CompactTransferResult<String> {
    let mut reader = session
        .open_reader(endpoint)
        .await
        .map_err(|error| transfer_error(error, None, Some(destination.clone())))?;
    let mut hasher = Sha256::new();
    let mut remaining = length;
    let mut buffer = vec![0_u8; COPY_BUFFER_BYTES];
    while remaining > 0 {
        if context.cancellation.is_cancelled() {
            return Err(Box::new(cancelled(
                context.resume_from.clone(),
                Some(destination.clone()),
            )));
        }
        let limit = buffer.len().min(remaining as usize);
        let read = reader.read(&mut buffer[..limit]).await.map_err(|error| {
            transfer_error(map_io_error(error), None, Some(destination.clone()))
        })?;
        if read == 0 {
            return Err(Box::new(transfer_error(
                AppError::conflict("传输暂存文件前缀读取不完整"),
                None,
                Some(destination.clone()),
            )));
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: context
                .resume_from
                .as_ref()
                .map(|item| item.confirmed_bytes)
                .unwrap_or(0),
            phase: TransferPhase::Verifying,
            actual_destination: Some(destination.clone()),
            checkpoint: context.resume_from.clone(),
            ..TransferProgressUpdate::default()
        });
    }
    Ok(sha256_hex(hasher.finalize().as_slice()))
}

/// 读取源的基础属性并检查完整哈希在传输读取中未变化。
async fn confirm_source_revision(
    session: &EndpointSession,
    source: &FileEndpoint,
    initial: &ObjectMetadata,
    expected: &FileRevision,
) -> AppResult<()> {
    let current = session
        .metadata(source)
        .await?
        .ok_or_else(|| AppError::not_found("传输源在提交前消失"))?;
    let digest_is_valid = expected.content_digest.as_ref().is_some_and(|digest| {
        digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    });
    if current.revision() != initial.revision() || !digest_is_valid {
        return Err(AppError::conflict("传输源属性在提交前发生变化"));
    }
    Ok(())
}

/// 只读核对沿用原暂存的 owner 身份，继任任务 ID 不会重写既有路径归属。
/// 仍须同时证明原 owner 命名、token、同目录实际目标和完整摘要，不能把继任 ID 当作可信证据。
///
/// @author kongweiguang
fn recovery_file_evidence(record: &TransferRecoveryRecord) -> Option<(&FileEndpoint, &str, u64)> {
    if record.archived || !record.commit_unknown || record.request.kind != TransferKind::File {
        return None;
    }
    let checkpoint = record.checkpoint.as_ref()?;
    let expected_sha256 = checkpoint.source_revision.content_digest.as_deref()?;
    if !checkpoint_owner_matches(&checkpoint.staging_destination, &checkpoint.staging_owner)
        || !valid_staging_token(&checkpoint.staging_token)
        || checkpoint.confirmed_bytes != checkpoint.source_revision.size
        || expected_sha256.len() != 64
        || !expected_sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !checkpoint
            .confirmed_prefix_sha256
            .eq_ignore_ascii_case(expected_sha256)
    {
        return None;
    }
    let destination = record.actual_destination.as_ref()?;
    let same_staging_parent = match (
        endpoint_parent(&checkpoint.staging_destination),
        endpoint_parent(destination),
    ) {
        (Ok(staging_parent), Ok(destination_parent)) => staging_parent == destination_parent,
        _ => false,
    };
    let same_requested_parent = match (
        endpoint_parent(&record.request.destination),
        endpoint_parent(destination),
    ) {
        (Ok(requested_parent), Ok(actual_parent)) => requested_parent == actual_parent,
        _ => false,
    };
    if &checkpoint.actual_destination != destination
        || !checkpoint.staging_destination.same_filesystem(destination)
        || checkpoint.staging_destination.same_object(destination)
        || !record.request.destination.same_filesystem(destination)
        || !same_staging_parent
        || !same_requested_parent
        || !matches!(
            destination,
            FileEndpoint::Local { .. } | FileEndpoint::Remote { .. }
        )
    {
        return None;
    }
    Some((
        destination,
        expected_sha256,
        checkpoint.source_revision.size,
    ))
}

/// 仅以 lstat、真实目标字节哈希和读后 revision 组成只读恢复证据，不把匹配改写为成功状态。
/// 链接目标和变动中的对象均不能证明原提交结果；哈希按固定缓冲区流式读取以限制内存。
///
/// @author kongweiguang
async fn inspect_recovery_destination(
    session: &EndpointSession,
    destination: &FileEndpoint,
    expected_size: u64,
    expected_sha256: &str,
    cancellation: &tokio_util::sync::CancellationToken,
) -> AppResult<TransferRecoveryInspection> {
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("传输恢复核对已取消"));
    }
    let Some(before) = session.metadata(destination).await? else {
        return Ok(TransferRecoveryInspection::DestinationMissing);
    };
    if before.kind != FileEntryKind::File || before.size != expected_size {
        return Ok(TransferRecoveryInspection::DestinationDiffers);
    }
    let mut reader = match session.open_reader(destination).await {
        Ok(reader) => reader,
        Err(error) => {
            if cancellation.is_cancelled() {
                return Err(AppError::cancelled("传输恢复核对已取消"));
            }
            return if session.metadata(destination).await?.is_none() {
                Ok(TransferRecoveryInspection::DestinationMissing)
            } else {
                Err(error)
            };
        }
    };
    let Some(opened) = session.metadata(destination).await? else {
        return Ok(TransferRecoveryInspection::DestinationMissing);
    };
    if opened.kind != FileEntryKind::File || opened.revision() != before.revision() {
        return Ok(TransferRecoveryInspection::DestinationDiffers);
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; COPY_BUFFER_BYTES];
    loop {
        let read = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("传输恢复核对已取消"));
            }
            read = reader.read(&mut buffer) => read.map_err(map_io_error)?,
        };
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("传输恢复核对已取消"));
    }
    let Some(after) = session.metadata(destination).await? else {
        return Ok(TransferRecoveryInspection::DestinationMissing);
    };
    if after.kind != FileEntryKind::File || after.revision() != before.revision() {
        return Ok(TransferRecoveryInspection::DestinationDiffers);
    }
    let observed_sha256 = sha256_hex(hasher.finalize().as_slice());
    if observed_sha256.eq_ignore_ascii_case(expected_sha256) {
        Ok(TransferRecoveryInspection::DestinationMatches)
    } else {
        Ok(TransferRecoveryInspection::DestinationDiffers)
    }
}

/// 在原子发布前按后序恢复源目录 mtime 与权限，确保子项完成后设置时间且收紧权限前可写。
///
/// @author kongweiguang
async fn apply_staging_directory_modes(
    session: &EndpointSession,
    postorder: &[FileEndpoint],
    source_modes: &HashMap<FileEndpoint, Option<u32>>,
    source_mtimes: &HashMap<FileEndpoint, Option<u64>>,
    preserve_mtime: bool,
) -> AppResult<()> {
    for directory in postorder {
        let mode = source_modes
            .get(directory)
            .copied()
            .ok_or_else(|| AppError::conflict("目录权限快照缺少已遍历的暂存目录"))?;
        if preserve_mtime {
            let modified_seconds = source_mtimes
                .get(directory)
                .copied()
                .flatten()
                .ok_or_else(|| AppError::unsupported("源目录未提供可保留的修改时间"))?;
            session
                .set_modified_time(directory, modified_seconds)
                .await?;
        }
        session.set_permissions(directory, mode).await?;
    }
    Ok(())
}

/// 发布前失败时从根到叶恢复 staging 的写权限，确保清理 owner 仍能递归回收暂存树。
///
/// @author kongweiguang
async fn restore_staging_directory_modes(
    session: &EndpointSession,
    postorder: &[FileEndpoint],
) -> AppResult<()> {
    let mut first_error = None;
    for directory in postorder.iter().rev() {
        if let Err(error) = session.set_permissions(directory, Some(0o700)).await {
            if first_error.is_none() {
                first_error = Some(error);
            } else {
                tracing::warn!(
                    error_code = error.public_code().as_str(),
                    "目录暂存的另一个权限恢复仍待重试"
                );
            }
        }
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// 冲突目录合并时逐项写成私有 sibling，再按无覆盖/替换策略原子发布。
fn sibling_child(parent: &FileEndpoint, name: &str) -> AppResult<FileEndpoint> {
    match parent {
        FileEndpoint::Local { path } => Ok(FileEndpoint::Local {
            path: path.join(name),
        }),
        FileEndpoint::Remote { host_id, path } => Ok(FileEndpoint::Remote {
            host_id: host_id.clone(),
            path: path
                .join_name(name)
                .map_err(|_| AppError::invalid("远端文件名无效"))?,
        }),
        FileEndpoint::Container { .. } => Err(AppError::unsupported("SFTP 传输不接受容器端点")),
    }
}

/// 暂存名不复用用户文件名，避免长 UTF-8 basename 再次突破远端组件长度限制；owner 与随机 nonce 固定恢复身份。
///
/// @author kongweiguang
fn create_staging_sibling(
    destination: &FileEndpoint,
    owner: &TransferId,
) -> AppResult<FileEndpoint> {
    endpoint_name(destination)?;
    let owner = safe_owner(owner.as_str());
    let staging_name = format!(
        ".kerminal-transfer-{owner}-{}.partial",
        Uuid::new_v4().simple()
    );
    let parent = endpoint_parent(destination)?;
    sibling_child(&parent, &staging_name)
}

/// 为目录内部文件生成短随机 sibling，避免用户 basename 过长并保持每次写入独立。
///
/// @author kongweiguang
fn temporary_sibling(destination: &FileEndpoint) -> AppResult<FileEndpoint> {
    endpoint_name(destination)?;
    let staging_name = format!(".kerminal-file-{}.partial", Uuid::new_v4().simple());
    sibling_child(&endpoint_parent(destination)?, &staging_name)
}

/// 用不可变 token 派生短 sidecar 名称，使 marker 与暂存数据同目录且不受源文件名长度影响。
///
/// @author kongweiguang
fn staging_marker_sibling(endpoint: &FileEndpoint, token: &str) -> AppResult<FileEndpoint> {
    if !valid_staging_token(token) {
        return Err(AppError::invalid("暂存 token 格式无效"));
    }
    endpoint_name(endpoint)?;
    sibling_child(
        &endpoint_parent(endpoint)?,
        &format!(".kerminal-owner-{token}.marker"),
    )
}

/// 只接纳 32 位小写十六进制 nonce，阻止 checkpoint 字段注入路径片段。
fn valid_staging_token(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 从完整命名形状校验 staging owner 与随机 nonce，拒绝前缀相似或旧孤儿路径冒充当前检查点。
///
/// @author kongweiguang
fn checkpoint_owner_matches(staging: &FileEndpoint, owner: &TransferId) -> bool {
    let prefix = format!(".kerminal-transfer-{}-", safe_owner(owner.as_str()));
    endpoint_name(staging).is_ok_and(|name| {
        name.strip_prefix(&prefix)
            .and_then(|nonce| nonce.strip_suffix(".partial"))
            .is_some_and(valid_staging_token)
    })
}

/// 将本机路径转换成 UTF-8 单段名称，不能安全传给远端协议的名称会明确拒绝。
fn endpoint_name(endpoint: &FileEndpoint) -> AppResult<String> {
    match endpoint {
        FileEndpoint::Local { path } => path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
            .ok_or_else(|| AppError::invalid("文件路径缺少可表示的名称")),
        FileEndpoint::Remote { path, .. } => path
            .as_str()
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty() && *name != ".")
            .map(str::to_owned)
            .ok_or_else(|| AppError::invalid("远端文件路径缺少名称")),
        FileEndpoint::Container { .. } => Err(AppError::unsupported("SFTP 传输不接受容器端点")),
    }
}

/// 返回端点同目录父路径，远端始终按 POSIX slash 规则而非宿主 Windows Path 解析。
fn endpoint_parent(endpoint: &FileEndpoint) -> AppResult<FileEndpoint> {
    match endpoint {
        FileEndpoint::Local { path } => path
            .parent()
            .map(|parent| FileEndpoint::Local {
                path: parent.to_path_buf(),
            })
            .ok_or_else(|| AppError::invalid("本机目标没有可用父目录")),
        FileEndpoint::Remote { host_id, path } => path
            .parent()
            .map(|parent| FileEndpoint::Remote {
                host_id: host_id.clone(),
                path: parent,
            })
            .ok_or_else(|| AppError::invalid("远端目标没有可用父目录")),
        FileEndpoint::Container { .. } => Err(AppError::unsupported("SFTP 传输不接受容器端点")),
    }
}

/// 比较已规范化的源路径范围；远端按 POSIX 组件边界判断，避免 `/root/a` 错收 `/root/ab`。
/// 此判断只约束源端解引用；目标路径从不经过 canonicalize，避免写入或删除跟随目标链接。
///
/// @author kongweiguang
fn endpoint_path_is_within(root: &FileEndpoint, candidate: &FileEndpoint) -> bool {
    match (root, candidate) {
        (FileEndpoint::Local { path: root }, FileEndpoint::Local { path: candidate }) => {
            candidate == root || candidate.starts_with(root)
        }
        (
            FileEndpoint::Remote {
                host_id: root_host,
                path: root,
            },
            FileEndpoint::Remote {
                host_id: candidate_host,
                path: candidate,
            },
        ) if root_host == candidate_host => {
            let root = root.as_str().trim_end_matches('/');
            let candidate = candidate.as_str();
            candidate == root
                || if root.is_empty() {
                    candidate.starts_with('/')
                } else {
                    candidate
                        .strip_prefix(root)
                        .is_some_and(|suffix| suffix.starts_with('/'))
                }
        }
        _ => false,
    }
}

/// 防止目录源的规范路径与目标父目录别名相交，并在任何 staging 所有权登记或写入前失败。
/// 只规范化已存在的目标父目录，再原样拼接最终 basename；目标叶始终不跟随，避免改变目标链接语义。
///
/// @author kongweiguang
async fn ensure_directory_destination_outside_source(
    source_session: &EndpointSession,
    destination_session: &EndpointSession,
    source_root: &FileEndpoint,
    destination: &FileEndpoint,
) -> AppResult<()> {
    if !source_root.same_filesystem(destination) {
        return Ok(());
    }

    let canonical_source_root = source_session.canonicalize_source(source_root).await?;
    let destination_parent = endpoint_parent(destination)?;
    let canonical_destination_parent = destination_session
        .canonicalize_source(&destination_parent)
        .await?;
    let canonical_destination =
        sibling_child(&canonical_destination_parent, &endpoint_name(destination)?)?;
    if canonical_source_root.same_filesystem(&canonical_destination)
        && endpoint_path_is_within(&canonical_source_root, &canonical_destination)
    {
        return Err(AppError::invalid("目录不能复制到自身或其子目录"));
    }
    Ok(())
}

/// 统一处理源端 no-follow/canonicalize、类型复核和范围校验；目标 endpoint 不传入此函数。
/// 关闭跟随时保留原有 lstat 语义并拒绝链接，开启时使用解析后的同一 endpoint 做后续哈希与读取。
///
/// @author kongweiguang
async fn resolve_source_object(
    session: &EndpointSession,
    source: &FileEndpoint,
    follow_symlinks: bool,
    allowed_root: Option<&FileEndpoint>,
) -> AppResult<(FileEndpoint, ObjectMetadata)> {
    let original = session
        .metadata(source)
        .await?
        .ok_or_else(|| AppError::not_found("传输源不存在"))?;
    if original.kind == FileEntryKind::Symlink && !follow_symlinks {
        return Err(AppError::unsupported(
            "源路径是符号链接，当前传输策略不跟随",
        ));
    }
    let resolved = if follow_symlinks {
        session.canonicalize_source(source).await?
    } else {
        source.clone()
    };
    let metadata = if follow_symlinks {
        session
            .metadata(&resolved)
            .await?
            .ok_or_else(|| AppError::not_found("符号链接目标不存在"))?
    } else {
        original
    };
    if metadata.kind == FileEntryKind::Symlink {
        return Err(AppError::conflict("规范源路径仍指向符号链接，拒绝递归解析"));
    }
    if let Some(root) = allowed_root
        && !endpoint_path_is_within(root, &resolved)
    {
        return Err(AppError::unsupported("符号链接目标超出所选源目录范围"));
    }
    Ok((resolved, metadata))
}

/// 文件源的跟随范围以用户选中文件的父目录为根，不能借单文件 symlink 读取任意外部路径。
///
/// @author kongweiguang
async fn source_file_scope(
    session: &EndpointSession,
    source: &FileEndpoint,
    follow_symlinks: bool,
) -> AppResult<Option<FileEndpoint>> {
    if !follow_symlinks {
        return Ok(None);
    }
    session
        .canonicalize_source(&endpoint_parent(source)?)
        .await
        .map(Some)
}

/// SFTP 协议只提供秒级 mtime；默认策略开启时缺少源时间必须显式失败，不能静默伪称保留。
///
/// @author kongweiguang
async fn apply_source_modified_time(
    session: &EndpointSession,
    destination: &FileEndpoint,
    source_metadata: &ObjectMetadata,
    preserve: bool,
) -> AppResult<()> {
    if !preserve {
        return Ok(());
    }
    let seconds = source_metadata
        .modified_seconds
        .ok_or_else(|| AppError::unsupported("源文件系统未提供可保留的修改时间"))?;
    session.set_modified_time(destination, seconds).await
}

/// 目录只接受范围内尚未访问的规范路径，并在入栈前限制深度以截断循环和极端树结构。
///
/// @author kongweiguang
fn register_source_directory(
    visited: &mut HashSet<FileEndpoint>,
    directory: &FileEndpoint,
    depth: usize,
) -> AppResult<()> {
    if depth > MAX_SOURCE_DIRECTORY_DEPTH {
        return Err(AppError::unsupported("目录传输超过最大源目录深度"));
    }
    if !visited.insert(directory.clone()) {
        return Err(AppError::conflict("源目录中存在循环或重复的符号链接目标"));
    }
    Ok(())
}

/// 比较来源目录和目标目录的路径身份，规整本机 `.`/`..` 后阻止把目录复制进自身。
/// 远端路径继续按 POSIX 语义比较，不借用 Windows 宿主机的路径解释。
///
/// @author kongweiguang
fn is_same_or_descendant(source: &FileEndpoint, destination: &FileEndpoint) -> bool {
    match (source, destination) {
        (FileEndpoint::Local { path: source }, FileEndpoint::Local { path: destination }) => {
            let source = normalize_local_path(source);
            let destination = normalize_local_path(destination);
            local_path_starts_with(&destination, &source)
        }
        (
            FileEndpoint::Remote { path: source, .. },
            FileEndpoint::Remote {
                path: destination, ..
            },
        ) => {
            let source = source.as_str().trim_end_matches('/');
            let destination = destination.as_str().trim_end_matches('/');
            destination == source
                || destination
                    .strip_prefix(source)
                    .is_some_and(|suffix| suffix.starts_with('/'))
        }
        _ => false,
    }
}

/// 通过真实文件系统路径再检查本机目录目标，拦截符号链接或 junction 将目标别名导回源目录。
/// 缺失的目标尾段会追加到最近的 canonical 祖先；验证发生在 staging 创建前，但标准路径 API
/// 无法阻止外部进程随后替换重解析点，因此仍存在 check-then-use 竞态，完整防护需要句柄相对的 no-follow 创建。
///
/// @author kongweiguang
async fn local_destination_resolves_within_source(
    source: &Path,
    destination: &Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> AppResult<bool> {
    let canonical_source = canonical_local_path_for_containment(source, cancellation).await?;
    let canonical_destination =
        canonical_local_path_for_containment(destination, cancellation).await?;
    Ok(local_path_starts_with(
        &canonical_destination,
        &canonical_source,
    ))
}

/// 为可能尚未创建的目标解析最长存在祖先，再按路径组件补回目标尾段以保留链接别名语义。
///
/// @author kongweiguang
async fn canonical_local_path_for_containment(
    path: &Path,
    cancellation: &tokio_util::sync::CancellationToken,
) -> AppResult<PathBuf> {
    let mut probe = absolute_path(path)?;
    let mut missing_tail = Vec::new();
    loop {
        let canonicalized = tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                return Err(AppError::cancelled("目录目标路径检查已取消"));
            }
            result = tokio::fs::canonicalize(&probe) => result,
        };
        match canonicalized {
            Ok(mut canonical) => {
                for component in missing_tail.iter().rev() {
                    canonical.push(component);
                }
                if cancellation.is_cancelled() {
                    return Err(AppError::cancelled("目录目标路径检查已取消"));
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let entry = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return Err(AppError::cancelled("目录目标路径检查已取消"));
                    }
                    result = tokio::fs::symlink_metadata(&probe) => result,
                };
                match entry {
                    // dangling symlink/junction 的目标无法验证，必须 fail closed，不能把它当普通缺失目录。
                    Ok(_) => return Err(AppError::invalid("目录目标包含无法解析的链接")),
                    Err(metadata_error) if metadata_error.kind() == io::ErrorKind::NotFound => {}
                    Err(metadata_error) => return Err(map_io_error(metadata_error)),
                }

                let Some(name) = probe.file_name() else {
                    return Err(map_io_error(error));
                };
                if name == "." || name == ".." {
                    return Err(AppError::invalid("目录目标路径包含无法安全解析的父目录段"));
                }
                missing_tail.push(name.to_os_string());
                let Some(parent) = probe.parent() else {
                    return Err(map_io_error(error));
                };
                probe = parent.to_path_buf();
            }
            Err(error) => return Err(map_io_error(error)),
        }
    }
}

/// 以当前平台的 Path components 折叠本机路径中的当前目录和可抵消的父目录段。
///
/// @author kongweiguang
fn normalize_local_path(path: &Path) -> PathBuf {
    use std::path::Component;

    let mut normalized = PathBuf::new();
    let rooted = path.has_root();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.file_name().is_some() {
                    normalized.pop();
                } else if !rooted {
                    normalized.push("..");
                }
            }
        }
    }
    normalized
}

/// 使用组件边界判断本机父路径；Windows 文件系统的路径大小写按不敏感语义处理。
///
/// @author kongweiguang
fn local_path_starts_with(candidate: &Path, parent: &Path) -> bool {
    #[cfg(windows)]
    {
        let candidate = candidate.to_string_lossy().to_lowercase();
        let parent = parent.to_string_lossy().to_lowercase();
        candidate == parent
            || candidate
                .strip_prefix(&parent)
                .is_some_and(|suffix| suffix.starts_with('\\') || suffix.starts_with('/'))
    }
    #[cfg(not(windows))]
    {
        candidate.starts_with(parent)
    }
}

/// 比较同目录 staging 约束时本机用 Path、远端用 RemotePath，禁止跨平台路径语义混用。
fn same_parent_directory(left: &FileEndpoint, right: &FileEndpoint) -> bool {
    match (left, right) {
        (FileEndpoint::Local { path: left }, FileEndpoint::Local { path: right }) => {
            left.parent() == right.parent()
        }
        (
            FileEndpoint::Remote {
                host_id: left_host,
                path: left,
            },
            FileEndpoint::Remote {
                host_id: right_host,
                path: right,
            },
        ) => left_host == right_host && left.parent() == right.parent(),
        _ => false,
    }
}

/// Rename 候选在最后一个非隐藏扩展名前插入序号，名称只在初次入队时选择一次。
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index + 1..]),
        _ => (name, ""),
    }
}

/// owner 仅来自内部随机 TransferId；转成文件名允许字符以避免 opaque ID 被误作路径段。
fn safe_owner(owner: &str) -> String {
    owner
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// 暂存提交 helper 要求绝对路径，且路径解析只发生在本机端点边界。
fn absolute_path(path: &Path) -> AppResult<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .map_err(map_io_error)
    }
}

/// 仅当关闭失败的 endpoint 仍持有远端 route 时，才生成真实可重试的 cleanup owner。
fn pending_cleanup_for_sessions(sessions: &[&EndpointSession]) -> Option<TransferCleanupState> {
    let retained = sessions
        .iter()
        .filter(|session| session.has_remote_resources())
        .map(|session| (*session).clone())
        .collect::<Vec<_>>();
    (!retained.is_empty()).then(|| {
        TransferCleanupState::pending(Arc::new(SftpTransferCleanup {
            sessions: retained,
            staging: StagingCleanupTracker::default(),
        }))
    })
}

/// 仅保留可信断点或提交未知关联的 partial，普通失败仍由 owner 安全清理。
fn preserve_staging_for_outcome(
    staging: &StagingCleanupTracker,
    outcome: &Result<TransferExecutionResult, TransferExecutionError>,
) {
    match outcome {
        Ok(_) => {}
        Err(TransferExecutionError::Cancelled {
            checkpoint: Some(checkpoint),
            ..
        }) => staging.preserve_checkpoint(checkpoint),
        Err(TransferExecutionError::Cancelled {
            checkpoint: None, ..
        }) => {}
        Err(TransferExecutionError::Failed(failure))
            if failure.kind() == TransferFailureKind::CommitUnknown =>
        {
            staging.preserve_all();
        }
        Err(TransferExecutionError::Failed(failure)) if failure.resumable() => {
            if let Some(checkpoint) = failure.checkpoint() {
                staging.preserve_checkpoint(checkpoint);
            }
        }
        Err(TransferExecutionError::Failed(_)) => {}
    }
}

/// 资源关闭失败后，取消不再代表已清理；将其转为带 owner 的失败以保留重试入口。
fn with_pending_cleanup(
    error: TransferExecutionError,
    cleanup: TransferCleanupState,
) -> TransferExecutionError {
    let failure = match error {
        TransferExecutionError::Failed(failure) => TransferExecutionFailure::new(
            failure.kind(),
            failure.retryable(),
            failure.resumable(),
            failure.checkpoint().cloned(),
            failure.actual_destination().cloned(),
            cleanup,
        ),
        TransferExecutionError::Cancelled {
            checkpoint,
            actual_destination,
        } => TransferExecutionFailure::new(
            TransferFailureKind::Internal,
            false,
            false,
            checkpoint,
            actual_destination,
            cleanup,
        ),
    };
    TransferExecutionError::Failed(failure)
}

/// 对文件冲突采用稳定可移植分类，网络故障有可信断点时可重试，提交未知永不可重试。
fn transfer_error(
    error: AppError,
    checkpoint: Option<TransferCheckpoint>,
    actual_destination: Option<FileEndpoint>,
) -> TransferExecutionError {
    if error.public_code() == PublicErrorCode::Cancelled {
        return cancelled(checkpoint, actual_destination);
    }
    let safe_message = error.public_message();
    let kind = match error.public_code() {
        PublicErrorCode::Conflict => TransferFailureKind::Conflict,
        PublicErrorCode::Unsupported | PublicErrorCode::CapabilityNotSupported => {
            TransferFailureKind::Unsupported
        }
        PublicErrorCode::Invalid | PublicErrorCode::NotFound => TransferFailureKind::InvalidSource,
        PublicErrorCode::Timeout => TransferFailureKind::IdleTimeout,
        PublicErrorCode::Infrastructure
        | PublicErrorCode::AuthenticationFailed
        | PublicErrorCode::HostKeyUnknown
        | PublicErrorCode::HostKeyChanged
        | PublicErrorCode::HostKeyTrustMismatch => TransferFailureKind::Connection,
        _ => TransferFailureKind::Internal,
    };
    let retryable = matches!(
        kind,
        TransferFailureKind::Connection | TransferFailureKind::IdleTimeout
    ) || checkpoint.is_some();
    TransferExecutionError::Failed(
        TransferExecutionFailure::new(
            kind,
            retryable,
            checkpoint.is_some(),
            checkpoint,
            actual_destination,
            TransferCleanupState::Released,
        )
        .with_safe_message(safe_message),
    )
}

/// 把本机原子 helper 的提交回执分类保留下来，sync-after-rename 失败必须成为 CommitUnknown。
fn map_atomic_commit(error: AtomicCommitError) -> CommitFailure {
    match error {
        AtomicCommitError::NotCommitted(error) => CommitFailure::Known(map_io_error(error)),
        AtomicCommitError::CommitUnknown(error) => CommitFailure::Unknown(
            AppError::outcome_unknown_with_source("本机文件提交结果未知", error),
        ),
    }
}

/// 远端目录 rename 返回明确 SFTP status 才能判定未提交；transport 断开意味着结果未知。
fn map_directory_rename_error(error: SftpError) -> CommitFailure {
    match error {
        SftpError::Status(status) => {
            CommitFailure::Known(super::sftp::map_sftp_error(SftpError::Status(status)))
        }
        error => CommitFailure::Unknown(AppError::outcome_unknown_with_source(
            "SFTP 目录提交结果未知",
            error,
        )),
    }
}

/// commitUnknown 禁止 retry 并丢弃 checkpoint；调用方保留 staging 供人工回读目标时核查。
fn unknown_commit(
    error: AppError,
    actual_destination: Option<FileEndpoint>,
) -> TransferExecutionError {
    let failure = TransferExecutionFailure::new(
        TransferFailureKind::CommitUnknown,
        false,
        false,
        None,
        actual_destination,
        TransferCleanupState::Released,
    );
    tracing::warn!(
        error_code = error.public_code().as_str(),
        "传输提交结果未知"
    );
    TransferExecutionError::Failed(failure)
}

/// 取消只保留上次已发布且前缀已确认的 checkpoint，队列释放 worker 后才允许恢复。
fn cancelled(
    checkpoint: Option<TransferCheckpoint>,
    actual_destination: Option<FileEndpoint>,
) -> TransferExecutionError {
    TransferExecutionError::Cancelled {
        checkpoint,
        actual_destination,
    }
}

/// 按来源目标类型返回领域文件条目类型。
fn local_kind(metadata: &std::fs::Metadata) -> FileEntryKind {
    let kind = metadata.file_type();
    if kind.is_symlink() {
        FileEntryKind::Symlink
    } else if kind.is_dir() {
        FileEntryKind::Directory
    } else if kind.is_file() {
        FileEntryKind::File
    } else {
        FileEntryKind::Other
    }
}

/// 将 SFTP entry 类型映射到统一传输安全判断。
fn remote_kind(kind: FileType) -> FileEntryKind {
    match kind {
        FileType::File => FileEntryKind::File,
        FileType::Dir => FileEntryKind::Directory,
        FileType::Symlink => FileEntryKind::Symlink,
        FileType::Other => FileEntryKind::Other,
    }
}

/// Unix 保留源 mode；Windows 映射为保守可移植权限，目录始终保有 traversal 位。
///
/// @author kongweiguang
fn local_permissions(metadata: &std::fs::Metadata) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Some(metadata.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let readonly = metadata.permissions().readonly();
        Some(if metadata.is_dir() {
            if readonly { 0o555 } else { 0o755 }
        } else if readonly {
            0o444
        } else {
            0o644
        })
    }
}

/// 将操作系统 I/O 错误转换成脱敏的应用错误，路径仍保留在内部 source 链。
fn map_io_error(error: io::Error) -> AppError {
    match error.kind() {
        io::ErrorKind::NotFound => AppError::not_found("本机文件或目录不存在"),
        io::ErrorKind::AlreadyExists => AppError::conflict("本机传输目标已经存在"),
        io::ErrorKind::PermissionDenied => AppError::conflict("本机文件权限不足"),
        _ => AppError::infrastructure_with_source("本机文件传输失败", error),
    }
}

/// SHA-256 仅保存十六进制摘要到可信 checkpoint，不暴露文件内容。
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
#[path = "../../tests/unit/private_transfer.rs"]
mod private_tests;
