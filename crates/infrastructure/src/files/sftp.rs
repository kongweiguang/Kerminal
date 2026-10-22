//! SSH/SFTP 文件系统适配器；连接与认证委托给共享 RemoteConnectionService。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Weak},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ChannelLane, ConnectionIntent, OpenSftpRequest, RemoteConnectionLease,
        RemoteConnectionService, management::HostManagementService,
    },
    files::FileAccessProvider,
};
use kerminal_domain::files::{
    DeleteTargetKind, DirectoryListing, FileEndpoint, FileEntry, FileEntryKind, FilePreview,
    FileRevision, LineEnding, RemotePath, TextDocument, WriteTextRequest, WriteTextResult,
    WriteTextStrategy,
};
use russh_sftp::{
    client::{Config as SftpClientConfig, RawSftpSession, SftpSession, error::Error as SftpError},
    protocol::{FileAttributes, FileType, OpenFlags, Packet, StatusCode},
};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{Mutex, OwnedMutexGuard},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const POSIX_RENAME_EXTENSION: &str = "posix-rename@openssh.com";

/// 远端写锁按主机和路径区分，弱引用使无活跃写入的锁可回收。
type SftpWriteLocks = HashMap<(String, String), Weak<Mutex<()>>>;

/// SSH 文件访问实现共享连接服务，单个 SFTP 子系统结束时不会关闭主机的 SSH lease。
#[derive(Clone)]
pub struct SftpFileProvider {
    connections: Arc<dyn RemoteConnectionService>,
    write_locks: Arc<Mutex<SftpWriteLocks>>,
    host_management: Option<HostManagementService>,
}

impl SftpFileProvider {
    /// 注入唯一 SSH 连接端口，避免文件模块绕过 host capability、vault 或 known_hosts 规则。
    pub fn new(connections: Arc<dyn RemoteConnectionService>) -> Self {
        Self {
            connections,
            write_locks: Arc::new(Mutex::new(HashMap::new())),
            host_management: None,
        }
    }

    /// 文件浏览和 MCP 共用主机级开关；仅限制新 SFTP 操作，不关闭在途任务或 SSH 终端。
    /// 临时外部目标没有保存配置，仍由连接服务的原有 capability 边界校验。
    ///
    /// @author kongweiguang
    pub fn with_host_management(mut self, host_management: HostManagementService) -> Self {
        self.host_management = Some(host_management);
        self
    }

    /// 在共享 SSH lease 上打开拥有式 SFTP channel，批量与交互通道按用途隔离。
    pub(super) async fn open_session(
        &self,
        host_id: &kerminal_domain::identity::HostId,
        lane: ChannelLane,
        cancellation: CancellationToken,
    ) -> AppResult<(SftpSession, RemoteConnectionLease)> {
        self.open_session_with_config(host_id, lane, cancellation, SftpClientConfig::default())
            .await
    }

    /// 在连接之前检查已保存主机的 SFTP 开关，配置读取失败不能降级绕过限制。
    /// 批量请求仍使用入队时冻结的 packet、流水线和请求超时预算。
    ///
    /// @author kongweiguang
    pub(super) async fn open_session_with_config(
        &self,
        host_id: &kerminal_domain::identity::HostId,
        lane: ChannelLane,
        cancellation: CancellationToken,
        config: SftpClientConfig,
    ) -> AppResult<(SftpSession, RemoteConnectionLease)> {
        if let Some(hosts) = &self.host_management {
            let host = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("SFTP 文件操作已取消")),
                host = hosts.load_host(host_id) => host?,
            };
            if host.is_some_and(|host| !host.sftp_options.enabled) {
                return Err(AppError::unsupported(
                    "此主机已关闭 SFTP，请在连接的传输设置中启用后重试",
                ));
            }
        }
        let lease = self
            .connections
            .connect(
                host_id.clone(),
                ConnectionIntent::Sftp,
                cancellation.clone(),
            )
            .await?;
        let channel = match lease
            .open_sftp(OpenSftpRequest { lane }, cancellation)
            .await
        {
            Ok(channel) => channel,
            Err(error) => {
                let _ = lease.close("SFTP channel open failed").await;
                return Err(error);
            }
        };
        match SftpSession::new_with_config(channel, config).await {
            Ok(session) => Ok((session, lease)),
            Err(error) => {
                let _ = lease.close("SFTP session initialization failed").await;
                Err(map_sftp_error(error))
            }
        }
    }

    /// 同一进程内对同一远端路径串行保存，避免两个编辑器互相覆盖；弱引用表不会常驻历史路径。
    async fn write_lock(
        &self,
        host_id: &kerminal_domain::identity::HostId,
        path: &RemotePath,
    ) -> OwnedMutexGuard<()> {
        let key = (host_id.as_str().to_owned(), path.as_str().to_owned());
        let lock = {
            let mut locks = self.write_locks.lock().await;
            locks.retain(|_, lock| lock.strong_count() > 0);
            match locks.get(&key).and_then(Weak::upgrade) {
                Some(lock) => lock,
                None => {
                    let lock = Arc::new(Mutex::new(()));
                    locks.insert(key, Arc::downgrade(&lock));
                    lock
                }
            }
        };
        lock.lock_owned().await
    }

    /// 仅在服务端声明 OpenSSH 原子替换扩展后才允许覆盖，避免退化为删除后 rename。
    async fn replace_remote_file(
        &self,
        lease: &RemoteConnectionLease,
        temporary: &RemotePath,
        destination: &RemotePath,
        cancellation: CancellationToken,
    ) -> Result<(), RemoteCommitError> {
        let channel = lease
            .open_sftp(
                OpenSftpRequest {
                    lane: ChannelLane::Bulk,
                },
                cancellation.clone(),
            )
            .await
            .map_err(RemoteCommitError::Application)?;
        let session = RawSftpSession::new(channel);
        let version = match session.init().await {
            Ok(version) => version,
            Err(error) => {
                let _ = session.close_session();
                return Err(RemoteCommitError::NotCommitted(error));
            }
        };
        if !version.extensions.contains_key(POSIX_RENAME_EXTENSION) {
            let _ = session.close_session();
            return Err(RemoteCommitError::Unsupported);
        }
        if cancellation.is_cancelled() {
            let _ = session.close_session();
            return Err(RemoteCommitError::Application(AppError::cancelled(
                "SFTP 文件提交已取消",
            )));
        }

        let payload = match encode_posix_rename(temporary.as_str(), destination.as_str()) {
            Ok(payload) => payload,
            Err(error) => {
                let _ = session.close_session();
                return Err(RemoteCommitError::Application(error));
            }
        };
        let result = session.extended(POSIX_RENAME_EXTENSION, payload).await;
        let close_result = session.close_session();
        match result {
            Ok(Packet::Status(status)) if status.status_code == StatusCode::Ok => {
                if let Err(error) = close_result {
                    tracing::warn!(error_kind = ?sftp_error_kind(&error), "SFTP 提交后关闭通道失败");
                }
                Ok(())
            }
            Ok(Packet::Status(status)) => {
                Err(RemoteCommitError::NotCommitted(SftpError::Status(status)))
            }
            Ok(_) => Err(RemoteCommitError::CommitUnknown(
                SftpError::UnexpectedPacket,
            )),
            Err(SftpError::Status(status)) => {
                Err(RemoteCommitError::NotCommitted(SftpError::Status(status)))
            }
            Err(error) => Err(RemoteCommitError::CommitUnknown(error)),
        }
    }

    /// 用 SFTP hardlink 做 no-clobber 新建，用 OpenSSH 扩展原子替换；已提交后通道清理失败只告警。
    ///
    /// @author kongweiguang
    pub(super) async fn commit_staged_file(
        &self,
        lease: &RemoteConnectionLease,
        temporary: &RemotePath,
        destination: &RemotePath,
        mode: RemoteCommitMode,
        cancellation: CancellationToken,
    ) -> Result<(), RemoteCommitError> {
        match mode {
            RemoteCommitMode::CreateNew => {
                let channel = lease
                    .open_sftp(
                        OpenSftpRequest {
                            lane: ChannelLane::Bulk,
                        },
                        cancellation.clone(),
                    )
                    .await
                    .map_err(RemoteCommitError::Application)?;
                let session = SftpSession::new(channel)
                    .await
                    .map_err(RemoteCommitError::NotCommitted)?;
                if cancellation.is_cancelled() {
                    let _ = session.close().await;
                    return Err(RemoteCommitError::Application(AppError::cancelled(
                        "SFTP 文件提交已取消",
                    )));
                }
                let result = match session
                    .hardlink(temporary.as_str(), destination.as_str())
                    .await
                {
                    Ok(true) => {
                        cleanup_remote_temporary(&session, temporary.as_str()).await;
                        Ok(())
                    }
                    Ok(false) => Err(RemoteCommitError::Unsupported),
                    Err(SftpError::Status(status)) => {
                        Err(RemoteCommitError::NotCommitted(SftpError::Status(status)))
                    }
                    Err(error) => Err(RemoteCommitError::CommitUnknown(error)),
                };
                let close_result = session.close().await;
                if let Err(error) = close_result
                    && result.is_ok()
                {
                    tracing::warn!(error_kind = ?sftp_error_kind(&error), "SFTP 新建提交后关闭通道失败");
                }
                result
            }
            RemoteCommitMode::ReplaceExisting => {
                self.replace_remote_file(lease, temporary, destination, cancellation)
                    .await
            }
        }
    }
}

/// 远端暂存文件的提交策略；覆盖只能使用明确声明支持的原子替换扩展。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RemoteCommitMode {
    CreateNew,
    ReplaceExisting,
}

/// 提交前连接失败、能力不足、明确拒绝和已发送请求无回执分别处理。
pub(super) enum RemoteCommitError {
    Application(AppError),
    Unsupported,
    NotCommitted(SftpError),
    CommitUnknown(SftpError),
}

#[async_trait]
impl FileAccessProvider for SftpFileProvider {
    /// 只接受保存主机的远端 endpoint，容器文件系统由 operations adapter 单独处理。
    fn supports(&self, endpoint: &FileEndpoint) -> bool {
        matches!(endpoint, FileEndpoint::Remote { .. })
    }

    /// 列出远端目录并把取消传到正在运行的 SFTP 请求，取消后关闭当前子系统通道。
    async fn list_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<DirectoryListing> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = cancellable_read(&cancellation, async {
            let entries = session
                .read_dir(path.as_str())
                .await
                .map_err(map_sftp_error)?;
            let mut output = Vec::new();
            for entry in entries {
                let name = entry.file_name();
                let child = path
                    .join_name(&name)
                    .map_err(|_| AppError::infrastructure("SFTP 目录项名称无效"))?;
                let metadata = entry.metadata();
                let kind = sftp_entry_kind(metadata.file_type());
                let raw = sftp_attribute_line(&name, kind, &metadata);
                let is_hidden = name.starts_with('.');
                output.push(FileEntry {
                    endpoint: FileEndpoint::Remote {
                        host_id: host_id.clone(),
                        path: child,
                    },
                    name,
                    kind,
                    size: metadata.size,
                    modified: metadata
                        .mtime
                        .map(|seconds| (u64::from(seconds) * 1_000_000_000).to_string()),
                    permissions: metadata.permissions.map(|mode| mode & 0o7777),
                    is_hidden,
                    raw: Some(raw),
                });
            }
            output.sort_by(|left, right| {
                let left_directory = left.kind == FileEntryKind::Directory;
                let right_directory = right.kind == FileEntryKind::Directory;
                right_directory
                    .cmp(&left_directory)
                    .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            });
            Ok(DirectoryListing {
                directory: endpoint.clone(),
                entries: output,
            })
        })
        .await;
        finish_session(session, lease, result).await
    }

    /// 通过有界 reader 读取预览；取消立即关闭 SFTP 子通道并返回取消结果。
    async fn preview_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<FilePreview> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = cancellable_read(&cancellation, async {
            let metadata = session
                .metadata(path.as_str())
                .await
                .map_err(map_sftp_error)?;
            if !metadata.file_type().is_file() {
                return Err(AppError::conflict("预览目标不是普通文件"));
            }
            let mut file = session.open(path.as_str()).await.map_err(map_sftp_error)?;
            let mut limited = (&mut file).take(max_bytes.saturating_add(1) as u64);
            let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
            limited
                .read_to_end(&mut bytes)
                .await
                .map_err(map_io_error)?;
            let truncated = bytes.len() > max_bytes;
            bytes.truncate(max_bytes);
            let total_size = metadata.size;
            Ok(FilePreview {
                endpoint: endpoint.clone(),
                binary: is_binary_preview(&bytes, truncated),
                truncated: truncated || total_size.is_some_and(|size| size > max_bytes as u64),
                revision: Some(sftp_revision(&metadata, None)),
                bytes,
                total_size,
            })
        })
        .await;
        finish_session(session, lease, result).await
    }

    /// 文本编辑读取拒绝链接与非 UTF-8 内容；取消读取关闭子通道并保留原 session 状态。
    async fn read_text_file(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        max_bytes: usize,
    ) -> AppResult<TextDocument> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = cancellable_read(&cancellation, async {
            let metadata = session
                .symlink_metadata(path.as_str())
                .await
                .map_err(map_sftp_error)?;
            if metadata.file_type().is_symlink() {
                return Err(AppError::unsupported(
                    "符号链接可预览，但文本编辑需打开其真实文件路径",
                ));
            }
            if !metadata.file_type().is_file() {
                return Err(AppError::conflict("编辑目标不是普通文件"));
            }
            let mut file = session.open(path.as_str()).await.map_err(map_sftp_error)?;
            let mut limited = (&mut file).take(max_bytes.saturating_add(1) as u64);
            let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
            limited
                .read_to_end(&mut bytes)
                .await
                .map_err(map_io_error)?;
            let truncated = bytes.len() > max_bytes;
            bytes.truncate(max_bytes);
            let content = String::from_utf8(bytes.clone())
                .map_err(|_| AppError::unsupported("文件不是 UTF-8 文本，不能作为文本编辑"))?;
            Ok(TextDocument {
                endpoint: endpoint.clone(),
                line_ending: line_ending(&bytes),
                revision: sftp_revision(&metadata, Some(&bytes)),
                readonly: truncated || metadata.permissions.is_some_and(|mode| mode & 0o222 == 0),
                truncated,
                content,
            })
        })
        .await;
        finish_session(session, lease, result).await
    }

    /// 临时文件写入与 revision 复验可取消；原子提交请求一旦发出就等待回执或未知结果。
    async fn write_text_file(
        &self,
        cancellation: CancellationToken,
        request: WriteTextRequest,
    ) -> AppResult<WriteTextResult> {
        let (host_id, path) = remote_endpoint(&request.endpoint)?;
        let _write_guard = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("SFTP 文件写入已取消")),
            guard = self.write_lock(host_id, path) => guard,
        };
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = async {
            let current = tokio::select! {
                biased;
                _ = cancellation.cancelled() => return Err(AppError::cancelled("SFTP 文件写入已取消")),
                result = session.symlink_metadata(path.as_str()) => result,
            };
            let (expected, existing_metadata) = match (&request.strategy, current) {
                (WriteTextStrategy::CreateNew, Ok(_)) => return Err(AppError::conflict("新建目标已经存在")),
                (WriteTextStrategy::CreateNew, Err(SftpError::Status(status)))
                    if status.status_code == StatusCode::NoSuchFile => (None, None),
                (WriteTextStrategy::CreateNew, Err(error)) => return Err(map_sftp_error(error)),
                (WriteTextStrategy::CompareRevision(expected), Ok(metadata))
                    if metadata.file_type().is_symlink() => {
                    return Err(AppError::unsupported("符号链接可预览，但文本编辑需打开其真实文件路径"));
                }
                (WriteTextStrategy::CompareRevision(expected), Ok(metadata))
                    if metadata.file_type().is_file() => (Some(expected), Some(metadata)),
                (WriteTextStrategy::CompareRevision(_), Ok(_)) => {
                    return Err(AppError::conflict("编辑目标不是普通文件"));
                }
                (WriteTextStrategy::CompareRevision(_), Err(error)) => return Err(map_sftp_error(error)),
                (WriteTextStrategy::ExplicitOverwrite, Ok(metadata))
                    if metadata.file_type().is_symlink() => {
                    return Err(AppError::unsupported("符号链接可预览，但文本编辑需打开其真实文件路径"));
                }
                (WriteTextStrategy::ExplicitOverwrite, Ok(metadata))
                    if metadata.file_type().is_file() => (None, Some(metadata)),
                (WriteTextStrategy::ExplicitOverwrite, Ok(_)) => {
                    return Err(AppError::conflict("覆盖目标不是普通文件"));
                }
                (WriteTextStrategy::ExplicitOverwrite, Err(error)) => return Err(map_sftp_error(error)),
            };
            if let (Some(expected), Some(metadata)) = (expected, existing_metadata.as_ref()) {
                let digest = if expected.content_digest.is_some() {
                    Some(read_file_bounded(&session, path.as_str(), expected.size, &cancellation).await?)
                } else {
                    None
                };
                if sftp_revision(metadata, digest.as_deref()) != *expected {
                    return Err(AppError::conflict("文件在编辑期间已被其它操作修改"));
                }
            }

            let temporary = temporary_remote_sibling(path)?;
            let mut file = session
                .open_with_flags(
                    temporary.as_str(),
                    OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                )
                .await
                .map_err(map_sftp_error)?;
            if cancellation.is_cancelled() {
                drop(file);
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(AppError::cancelled("SFTP 文件写入已取消"));
            }
            let write_result = tokio::select! {
                biased;
                _ = cancellation.cancelled() => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "cancelled")),
                result = file.write_all(request.content.as_bytes()) => result,
            };
            if let Err(error) = write_result {
                drop(file);
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(if cancellation.is_cancelled() { AppError::cancelled("SFTP 文件写入已取消") } else { map_io_error(error) });
            }
            if let Err(error) = file.sync_all().await {
                drop(file);
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(map_sftp_error(error));
            }
            if let Err(error) = file.shutdown().await {
                drop(file);
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(map_io_error(error));
            }
            drop(file);
            if cancellation.is_cancelled() {
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(AppError::cancelled("SFTP 文件写入已取消"));
            }

            if let Some(permissions) = existing_metadata.as_ref().and_then(|metadata| metadata.permissions) {
                if let Err(error) = session
                    .set_metadata(
                        temporary.as_str(),
                        FileAttributes { permissions: Some(permissions), ..FileAttributes::empty() },
                    )
                    .await
                {
                    cleanup_remote_temporary(&session, temporary.as_str()).await;
                    return Err(map_sftp_error(error));
                }
                if cancellation.is_cancelled() {
                    cleanup_remote_temporary(&session, temporary.as_str()).await;
                    return Err(AppError::cancelled("SFTP 文件写入已取消"));
                }
            }

            if let Some(expected) = expected {
                let metadata = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        cleanup_remote_temporary(&session, temporary.as_str()).await;
                        return Err(AppError::cancelled("SFTP 文件写入已取消"));
                    }
                    result = session.symlink_metadata(path.as_str()) => result.map_err(map_sftp_error)?,
                };
                if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                    cleanup_remote_temporary(&session, temporary.as_str()).await;
                    return Err(AppError::conflict("编辑目标类型在保存期间发生变化"));
                }
                let digest = if expected.content_digest.is_some() {
                    match read_file_bounded(&session, path.as_str(), expected.size, &cancellation).await {
                        Ok(bytes) => Some(bytes),
                        Err(error) => {
                            cleanup_remote_temporary(&session, temporary.as_str()).await;
                            return Err(error);
                        }
                    }
                } else {
                    None
                };
                if sftp_revision(&metadata, digest.as_deref()) != *expected {
                    cleanup_remote_temporary(&session, temporary.as_str()).await;
                    return Err(AppError::conflict("文件在保存提交前已被其它操作修改"));
                }
            }
            if cancellation.is_cancelled() {
                cleanup_remote_temporary(&session, temporary.as_str()).await;
                return Err(AppError::cancelled("SFTP 文件写入已取消"));
            }

            let strategy = match request.strategy {
                WriteTextStrategy::CreateNew => RemoteCommitMode::CreateNew,
                WriteTextStrategy::CompareRevision(_) | WriteTextStrategy::ExplicitOverwrite => {
                    RemoteCommitMode::ReplaceExisting
                }
            };
            match self
                .commit_staged_file(
                    &lease,
                    &temporary,
                    path,
                    strategy,
                    cancellation.clone(),
                )
                .await
            {
                Ok(()) => {}
                Err(error) => {
                    if !matches!(error, RemoteCommitError::CommitUnknown(_)) {
                        cleanup_remote_temporary(&session, temporary.as_str()).await;
                    }
                    return Err(map_remote_commit_error(error));
                }
            }
            let metadata = session.metadata(path.as_str()).await.map_err(|error| {
                AppError::outcome_unknown_with_source("SFTP 文件已提交但 revision 回读失败", error)
            })?;
            let bytes = request.content.as_bytes();
            Ok(WriteTextResult {
                bytes_written: bytes.len() as u64,
                revision: sftp_revision(&metadata, Some(bytes)),
            })
        }
        .await;
        finish_session(session, lease, result).await
    }

    /// 创建单层目录；请求发出后等待响应，连接中断时保留提交结果未知分类。
    async fn create_directory(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
    ) -> AppResult<()> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = if cancellation.is_cancelled() {
            Err(AppError::cancelled("SFTP 文件操作已取消"))
        } else {
            session
                .create_dir(path.as_str())
                .await
                .map_err(|error| map_sftp_mutation_error(error, "远端目录创建结果未知"))
        };
        finish_session(session, lease, result).await
    }

    /// 仅允许同一 SFTP 主机内重命名；响应丢失时不猜测源和目标的最终状态。
    async fn rename(
        &self,
        cancellation: CancellationToken,
        source: FileEndpoint,
        destination: FileEndpoint,
    ) -> AppResult<()> {
        let (host_id, source_path) = remote_endpoint(&source)?;
        let (destination_host, destination_path) = remote_endpoint(&destination)?;
        if host_id != destination_host {
            return Err(AppError::unsupported("SFTP 重命名必须留在同一主机"));
        }
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = if cancellation.is_cancelled() {
            Err(AppError::cancelled("SFTP 文件操作已取消"))
        } else {
            session
                .rename(source_path.as_str(), destination_path.as_str())
                .await
                .map_err(|error| map_sftp_mutation_error(error, "SFTP 重命名结果未知"))
        };
        finish_session(session, lease, result).await
    }

    /// 通过 SETSTAT 设置 Unix mode；响应丢失时保留结果未知以免重复误判。
    async fn chmod(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        mode: u32,
    ) -> AppResult<()> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = if cancellation.is_cancelled() {
            Err(AppError::cancelled("SFTP 文件操作已取消"))
        } else {
            session
                .set_metadata(
                    path.as_str(),
                    FileAttributes {
                        permissions: Some(mode),
                        ..FileAttributes::empty()
                    },
                )
                .await
                .map_err(|error| map_sftp_mutation_error(error, "SFTP 权限修改结果未知"))
        };
        finish_session(session, lease, result).await
    }

    /// 删除目录只调用 rmdir；请求发出后等待结果，不把远端目录删除扩展为递归。
    async fn delete(
        &self,
        cancellation: CancellationToken,
        endpoint: FileEndpoint,
        kind: DeleteTargetKind,
    ) -> AppResult<()> {
        let (host_id, path) = remote_endpoint(&endpoint)?;
        let (session, lease) = self
            .open_session(host_id, ChannelLane::Interactive, cancellation.clone())
            .await?;
        let result = if cancellation.is_cancelled() {
            Err(AppError::cancelled("SFTP 文件操作已取消"))
        } else {
            match kind {
                DeleteTargetKind::File => session.remove_file(path.as_str()).await,
                DeleteTargetKind::Directory => session.remove_dir(path.as_str()).await,
            }
            .map_err(|error| map_sftp_mutation_error(error, "SFTP 删除结果未知"))
        };
        finish_session(session, lease, result).await
    }
}

/// 从远端 endpoint 提取连接 ID 与 POSIX 路径，阻止容器或本机路径进入 SFTP。
fn remote_endpoint(
    endpoint: &FileEndpoint,
) -> AppResult<(&kerminal_domain::identity::HostId, &RemotePath)> {
    match endpoint {
        FileEndpoint::Remote { host_id, path } => Ok((host_id, path)),
        _ => Err(AppError::unsupported("SFTP 文件适配器仅处理远端主机路径")),
    }
}

/// 映射 SFTP 文件类型并保留 symlink 标记，避免递归文件操作无意跟随链接。
fn sftp_entry_kind(file_type: FileType) -> FileEntryKind {
    match file_type {
        FileType::Dir => FileEntryKind::Directory,
        FileType::File => FileEntryKind::File,
        FileType::Symlink => FileEntryKind::Symlink,
        FileType::Other => FileEntryKind::Other,
    }
}

/// 由服务器实际返回的 SFTP 属性生成稳定 ls 风格行，不冒充服务器未提供的 longname。
///
/// @author kongweiguang
fn sftp_attribute_line(name: &str, kind: FileEntryKind, attributes: &FileAttributes) -> String {
    let mode = attributes
        .permissions
        .map(|value| sftp_permission_string(kind, value & 0o7777))
        .unwrap_or_else(|| format!("{}?????????", file_type_mark(kind)));
    let owner = attributes
        .user
        .clone()
        .or_else(|| attributes.uid.map(|uid| uid.to_string()))
        .unwrap_or_else(|| "?".to_owned());
    let group = attributes
        .group
        .clone()
        .or_else(|| attributes.gid.map(|gid| gid.to_string()))
        .unwrap_or_else(|| "?".to_owned());
    let size = attributes
        .size
        .map(|size| size.to_string())
        .unwrap_or_else(|| "?".to_owned());
    let modified = attributes
        .mtime
        .map(|seconds| format!("@{seconds}"))
        .unwrap_or_else(|| "?".to_owned());
    format!("{mode} ? {owner} {group} {size} {modified} {name}")
}

/// 将实际 SFTP mode 以十位 POSIX 列表格式呈现，未知 mode 不在此处伪造。
///
/// @author kongweiguang
fn sftp_permission_string(kind: FileEntryKind, mode: u32) -> String {
    let mut output = String::with_capacity(10);
    output.push(file_type_mark(kind));
    for (read, write, execute, special) in [
        (0o400, 0o200, 0o100, 0o4000),
        (0o040, 0o020, 0o010, 0o2000),
        (0o004, 0o002, 0o001, 0o1000),
    ] {
        output.push(if mode & read != 0 { 'r' } else { '-' });
        output.push(if mode & write != 0 { 'w' } else { '-' });
        let execute_mark = if mode & special != 0 {
            if mode & execute != 0 {
                if special == 0o1000 { 't' } else { 's' }
            } else if special == 0o1000 {
                'T'
            } else {
                'S'
            }
        } else if mode & execute != 0 {
            'x'
        } else {
            '-'
        };
        output.push(execute_mark);
    }
    output
}

/// 返回 POSIX 列表权限前缀使用的文件类型符号。
///
/// @author kongweiguang
fn file_type_mark(kind: FileEntryKind) -> char {
    match kind {
        FileEntryKind::File => '-',
        FileEntryKind::Directory => 'd',
        FileEntryKind::Symlink => 'l',
        FileEntryKind::Other => '?',
    }
}

/// 将 SFTP 属性转换为稳定 revision；打开编辑时才额外计算内容摘要。
fn sftp_revision(attributes: &FileAttributes, content: Option<&[u8]>) -> FileRevision {
    FileRevision {
        size: attributes.size.unwrap_or_default(),
        modified: attributes
            .mtime
            .map(|seconds| (u64::from(seconds) * 1_000_000_000).to_string()),
        permissions: attributes.permissions.map(|mode| mode & 0o7777),
        content_digest: content.map(sha256_hex),
    }
}

/// 读取 revision 所需的文件内容最多 expected_size+1 字节，避免远端文件增长时越过硬上限。
async fn read_file_bounded(
    session: &SftpSession,
    path: &str,
    expected_size: u64,
    cancellation: &CancellationToken,
) -> AppResult<Vec<u8>> {
    if expected_size > kerminal_application::files::MAX_TEXT_FILE_READ_BYTES as u64 {
        return Err(AppError::unsupported(
            "大于编辑上限的文件不能做内容级 revision 比对",
        ));
    }
    cancellable_read(cancellation, async {
        let mut file = session.open(path).await.map_err(map_sftp_error)?;
        let mut limited = (&mut file).take(expected_size.saturating_add(1));
        let mut bytes = Vec::with_capacity(expected_size as usize);
        limited
            .read_to_end(&mut bytes)
            .await
            .map_err(map_io_error)?;
        if bytes.len() as u64 != expected_size {
            return Err(AppError::conflict("文件在 revision 校验期间已变化"));
        }
        Ok(bytes)
    })
    .await
}

/// 为新远端版本生成同目录唯一暂存路径，根路径或无父目录语义时显式拒绝写入。
///
/// @author kongweiguang
fn temporary_remote_sibling(path: &RemotePath) -> AppResult<RemotePath> {
    let basename = path.as_str().rsplit('/').next().unwrap_or("file");
    let suffix = format!(".{basename}.kerminal-edit-{}.partial", Uuid::new_v4());
    path.parent()
        .and_then(|parent| parent.join_name(&suffix).ok())
        .ok_or_else(|| AppError::invalid("远端文件路径无法创建同目录暂存文件"))
}

/// 编码 OpenSSH posix-rename 扩展要求的两个 SSH string，避免依赖不支持该扩展的高层封装。
///
/// @author kongweiguang
fn encode_posix_rename(source: &str, destination: &str) -> AppResult<Vec<u8>> {
    let mut payload = Vec::with_capacity(source.len() + destination.len() + 8);
    for value in [source, destination] {
        let length = u32::try_from(value.len())
            .map_err(|_| AppError::invalid("远端路径超过协议长度限制"))?;
        payload.extend_from_slice(&length.to_be_bytes());
        payload.extend_from_slice(value.as_bytes());
    }
    Ok(payload)
}

/// 根据 SFTP 扩展是否已发出请求映射提交错误，丢失回执始终成为不可重试的未知结果。
///
/// @author kongweiguang
fn map_remote_commit_error(error: RemoteCommitError) -> AppError {
    match error {
        RemoteCommitError::Application(error) => error,
        RemoteCommitError::Unsupported => AppError::unsupported("服务器不支持安全的原子 SFTP 提交"),
        RemoteCommitError::NotCommitted(error) => map_sftp_error(error),
        RemoteCommitError::CommitUnknown(error) => {
            AppError::outcome_unknown_with_source("SFTP 提交结果未知，请重新读取目标确认", error)
        }
    }
}

/// 尝试清理未提交的远端 partial，清理失败只记录安全错误类别而不覆盖原始失败。
async fn cleanup_remote_temporary(session: &SftpSession, path: &str) {
    if let Err(error) = session.remove_file(path).await {
        tracing::warn!(error_kind = ?sftp_error_kind(&error), "SFTP 临时文件清理失败");
    }
}

/// 把已实现的 SFTP 状态码映射为安全错误类型，其余 transport 错误保留 source 供诊断。
pub(super) fn map_sftp_error(error: SftpError) -> AppError {
    match &error {
        SftpError::Status(status) if status.status_code == StatusCode::NoSuchFile => {
            AppError::not_found("远端文件或目录不存在")
        }
        SftpError::Status(status) if status.status_code == StatusCode::PermissionDenied => {
            AppError::conflict("远端文件权限不足")
        }
        SftpError::Timeout => AppError::timeout("SFTP 请求"),
        _ => AppError::infrastructure_with_source("SFTP 文件操作失败", error),
    }
}

/// 把 channel IO 错误保留在基础设施错误链中，公开文案不泄露路径和主机输入。
fn map_io_error(error: std::io::Error) -> AppError {
    AppError::infrastructure_with_source("SFTP 文件数据流失败", error)
}

/// 关闭当前 SFTP subsystem channel；操作错误优先返回，关闭错误只在成功路径上显式暴露。
async fn finish_session<T>(
    session: SftpSession,
    lease: RemoteConnectionLease,
    result: AppResult<T>,
) -> AppResult<T> {
    let deadline = Instant::now() + Duration::from_secs(5);
    match timeout_at(deadline, session.close()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            tracing::warn!(error_kind = ?sftp_error_kind(&error), "关闭 SFTP 子通道失败")
        }
        Err(_) => tracing::warn!("关闭 SFTP 子通道超过资源回收时限"),
    }
    if lease
        .close_until(deadline, "SFTP operation completed")
        .await
        .is_err()
    {
        tracing::warn!("关闭 SFTP 连接超过资源回收时限");
    }
    result
}

/// 对无副作用的 SFTP 查询响应取消，并让调用方随后关闭本次专属 channel。
///
/// @author kongweiguang
async fn cancellable_read<T>(
    cancellation: &CancellationToken,
    operation: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AppError::cancelled("SFTP 文件读取已取消")),
        result = operation => result,
    }
}

/// SFTP mutation 的明确状态码代表服务端拒绝，传输层错误则可能发生在执行后回执丢失。
///
/// @author kongweiguang
fn map_sftp_mutation_error(error: SftpError, unknown_context: &'static str) -> AppError {
    match &error {
        SftpError::Status(_) => map_sftp_error(error),
        _ => AppError::outcome_unknown_with_source(unknown_context, error),
    }
}

/// 预览截断点只产生 UTF-8 不完整尾序列时仍按文本处理，但不会把该预览送入编辑器。
///
/// @author kongweiguang
fn is_binary_preview(bytes: &[u8], truncated: bool) -> bool {
    if bytes.contains(&0) {
        return true;
    }
    match std::str::from_utf8(bytes) {
        Ok(_) => false,
        Err(error) => !truncated || error.error_len().is_some(),
    }
}

/// 从 SFTP 错误中提取固定类别用于清理诊断，避免记录远端原始 message。
pub(super) fn sftp_error_kind(error: &SftpError) -> &'static str {
    match error {
        SftpError::Status(status) => match status.status_code {
            StatusCode::NoSuchFile => "not_found",
            StatusCode::PermissionDenied => "permission_denied",
            _ => "status",
        },
        SftpError::Timeout => "timeout",
        SftpError::IO(_) => "io",
        SftpError::Limited(_) => "limit",
        SftpError::UnexpectedPacket => "unexpected_packet",
        SftpError::UnexpectedBehavior(_) => "unexpected_behavior",
    }
}

/// 转换 SFTP 秒级 mtime，供编辑器保留同一属性粒度的 revision。
fn line_ending(bytes: &[u8]) -> LineEnding {
    let crlf = bytes.windows(2).filter(|pair| *pair == b"\r\n").count();
    let lf = bytes
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_sub(crlf);
    match (crlf > 0, lf > 0) {
        (true, true) => LineEnding::Mixed,
        (true, false) => LineEnding::Crlf,
        _ => LineEnding::Lf,
    }
}

/// SHA-256 仅用于本地 revision/checkpoint 比对，不暴露远端文件内容。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
#[path = "../../tests/files/sftp_preview.rs"]
mod preview_tests;
