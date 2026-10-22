//! 本地与 SFTP 端点的文件访问和提交适配；传输编排保留在父模块以维持单一生命周期所有者。
//!
//! @author kongweiguang

use std::{io, sync::Arc, time::UNIX_EPOCH};

#[cfg(not(windows))]
use std::time::Duration;

use kerminal_application::{
    AppError, AppResult,
    connections::{ChannelLane, RemoteConnectionLease},
};
use kerminal_domain::{
    files::{FileEndpoint, FileEntryKind, FileRevision, RemotePath},
    identity::HostId,
};
use russh_sftp::{
    client::{Config as SftpClientConfig, SftpSession, error::Error as SftpError},
    protocol::{FileAttributes, OpenFlags, StatusCode},
};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::super::sftp::{RemoteCommitError, RemoteCommitMode, SftpFileProvider};
use super::{
    BoxReader, BoxWriter, STAGING_MARKER_MAX_BYTES, StagingArtifact, StagingDisposition,
    StagingKind, absolute_path, local_kind, local_permissions, map_atomic_commit,
    map_directory_rename_error, map_io_error, remote_kind,
};
use crate::storage::{CommitMode, commit_staged_file};
/// 单端点文件访问通道；Local 不占网络资源，Remote 只持有本次 SFTP subsystem。
#[derive(Clone)]
pub(super) enum EndpointSession {
    Local,
    Remote {
        host_id: HostId,
        session: Arc<SftpSession>,
        lease: RemoteConnectionLease,
    },
}

impl EndpointSession {
    /// 每个端点尝试独立持有 lease；关闭时先释放 SFTP channel，再结束本次 SSH route。
    pub(super) async fn open(
        files: &SftpFileProvider,
        endpoint: &FileEndpoint,
        cancellation: &CancellationToken,
        config: SftpClientConfig,
    ) -> AppResult<Self> {
        match endpoint {
            FileEndpoint::Local { .. } => Ok(Self::Local),
            FileEndpoint::Remote { host_id, .. } => {
                let (session, lease) = files
                    .open_session_with_config(
                        host_id,
                        ChannelLane::Bulk,
                        cancellation.clone(),
                        config,
                    )
                    .await?;
                Ok(Self::Remote {
                    host_id: host_id.clone(),
                    session: Arc::new(session),
                    lease,
                })
            }
            FileEndpoint::Container { .. } => Err(AppError::unsupported("SFTP 传输不处理容器路径")),
        }
    }

    /// 关闭本机无状态端点或远端 route；route 未确认关闭时向调用方保留重试 owner。
    pub(super) async fn close_until(&self, deadline: Instant) -> AppResult<()> {
        if let Self::Remote { session, lease, .. } = self {
            let channel_result = match timeout_at(deadline, session.close()).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(error)) => Err(AppError::infrastructure_with_source(
                    "传输 SFTP 子通道关闭失败",
                    error,
                )),
                Err(_) => Err(AppError::timeout("传输 SFTP 子通道关闭")),
            };
            let lease_result = lease
                .close_until(deadline, "SFTP transfer endpoint completed")
                .await;
            match lease_result {
                // SSH route 已经关闭时，底层传输关闭了全部子通道，单独的 SFTP close 回执不再决定资源存活。
                Ok(()) => {
                    if let Err(error) = channel_result {
                        tracing::debug!(
                            error_code = error.public_code().as_str(),
                            "SSH route 已关闭，忽略已失效子通道的关闭回执"
                        );
                    }
                    Ok(())
                }
                Err(error) => Err(error),
            }
        } else {
            Ok(())
        }
    }

    /// 仅允许由当前端点在独占创建语义下写入 marker，避免覆盖同名外部对象。
    pub(super) async fn create_staging_marker(&self, artifact: &StagingArtifact) -> AppResult<()> {
        let bytes = artifact.marker_bytes()?;
        match (self, &artifact.marker) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .await
                    .map_err(map_io_error)?;
                file.write_all(&bytes).await.map_err(map_io_error)?;
                file.sync_all().await.map_err(map_io_error)
            }
            (
                Self::Remote {
                    host_id, session, ..
                },
                FileEndpoint::Remote {
                    host_id: marker_host,
                    path,
                },
            ) if host_id == marker_host => {
                let mut file = session
                    .open_with_flags(
                        path.as_str(),
                        OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                    )
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                file.write_all(&bytes).await.map_err(map_io_error)?;
                file.sync_all()
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                file.shutdown().await.map_err(map_io_error)
            }
            _ => Err(AppError::unsupported("暂存 marker 与访问通道端点不匹配")),
        }
    }

    /// 比对完整的小型 sidecar 内容，缺失或被替换时拒绝续传和清理。
    pub(super) async fn verify_staging_marker(&self, artifact: &StagingArtifact) -> AppResult<()> {
        let expected = artifact.marker_bytes()?;
        let actual = self.read_staging_marker(&artifact.marker).await?;
        if actual.as_deref() == Some(expected.as_slice()) {
            Ok(())
        } else {
            Err(AppError::conflict("传输暂存 marker 缺失或归属已变化"))
        }
    }

    /// 读取有上限的 marker；外部替换成超大文件时不会无界占用内存。
    async fn read_staging_marker(&self, marker: &FileEndpoint) -> AppResult<Option<Vec<u8>>> {
        match (self, marker) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let file = match fs::File::open(path).await {
                    Ok(file) => file,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                    Err(error) => return Err(map_io_error(error)),
                };
                let mut bytes = Vec::new();
                file.take(STAGING_MARKER_MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(map_io_error)?;
                if bytes.len() as u64 > STAGING_MARKER_MAX_BYTES {
                    return Err(AppError::conflict("传输暂存 marker 大小无效"));
                }
                Ok(Some(bytes))
            }
            (
                Self::Remote {
                    host_id, session, ..
                },
                FileEndpoint::Remote {
                    host_id: marker_host,
                    path,
                },
            ) if host_id == marker_host => {
                let file = match session.open(path.as_str()).await {
                    Ok(file) => file,
                    Err(SftpError::Status(status))
                        if status.status_code == StatusCode::NoSuchFile =>
                    {
                        return Ok(None);
                    }
                    Err(error) => return Err(super::super::sftp::map_sftp_error(error)),
                };
                let mut bytes = Vec::new();
                file.take(STAGING_MARKER_MAX_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .await
                    .map_err(map_io_error)?;
                if bytes.len() as u64 > STAGING_MARKER_MAX_BYTES {
                    return Err(AppError::conflict("传输暂存 marker 大小无效"));
                }
                Ok(Some(bytes))
            }
            _ => Err(AppError::unsupported("暂存 marker 与访问通道端点不匹配")),
        }
    }

    /// 只删除 marker 证明归属且本轮明确创建的暂存对象；删除后核验原路径消失才允许释放持久 owner。
    /// 未知创建或清理结果保留 marker 与 cleanup owner，不凭一次 remove 回执遗忘资源。
    ///
    /// @author kongweiguang
    pub(super) async fn cleanup_owned_artifact(&self, artifact: &StagingArtifact) -> AppResult<()> {
        let expected = artifact.marker_bytes()?;
        let actual_marker = self.read_staging_marker(&artifact.marker).await?;
        let stage_metadata = self.metadata(&artifact.endpoint).await?;
        let Some(actual_marker) = actual_marker else {
            return if stage_metadata.is_none() {
                Ok(())
            } else {
                Err(AppError::conflict("暂存对象缺少 owner marker，拒绝删除"))
            };
        };
        if actual_marker != expected {
            return Err(AppError::conflict("暂存 owner marker 已被替换，拒绝删除"));
        }
        if artifact.disposition == StagingDisposition::Committed {
            if stage_metadata.is_some() {
                return Err(AppError::conflict("已提交暂存路径出现新对象，拒绝清理"));
            }
        } else if let Some(metadata) = stage_metadata {
            if !artifact.stage_owned {
                return Err(AppError::conflict(
                    "暂存对象创建结果未确认，保留 marker 以避免清理外部对象",
                ));
            }
            let expected_kind = match artifact.kind {
                StagingKind::File => FileEntryKind::File,
                StagingKind::Tree => FileEntryKind::Directory,
            };
            if metadata.kind != expected_kind {
                return Err(AppError::conflict("暂存对象类型已变化，拒绝删除"));
            }
            match artifact.kind {
                StagingKind::File => self.remove_staging_file(&artifact.endpoint).await?,
                StagingKind::Tree => self.remove_staging_tree(&artifact.endpoint).await?,
            }
        }
        self.remove_staging_file(&artifact.marker).await?;
        if self.metadata(&artifact.endpoint).await?.is_some()
            || self.metadata(&artifact.marker).await?.is_some()
        {
            return Err(AppError::conflict(
                "暂存清理后原路径仍存在，保留 owner 以待核对",
            ));
        }
        Ok(())
    }

    /// 删除一个普通文件或 symlink 并将明确的缺失当作幂等成功。
    async fn remove_staging_file(&self, endpoint: &FileEndpoint) -> AppResult<()> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => match fs::remove_file(path).await {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(map_io_error(error)),
            },
            (
                Self::Remote {
                    host_id, session, ..
                },
                FileEndpoint::Remote {
                    host_id: endpoint_host,
                    path,
                },
            ) if host_id == endpoint_host => match session.remove_file(path.as_str()).await {
                Ok(()) => Ok(()),
                Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                    Ok(())
                }
                Err(error) => Err(super::super::sftp::map_sftp_error(error)),
            },
            _ => Err(AppError::unsupported("暂存清理端点与访问通道不匹配")),
        }
    }

    /// 不跟随符号链接递归删除已验证的目录树，任何不完整清理都返回给持久 owner。
    async fn remove_staging_tree(&self, root: &FileEndpoint) -> AppResult<()> {
        let mut pending = vec![(root.clone(), false)];
        while let Some((current, visited)) = pending.pop() {
            let Some(metadata) = self.metadata(&current).await? else {
                continue;
            };
            if metadata.kind != FileEntryKind::Directory {
                self.remove_staging_file(&current).await?;
            } else if visited {
                match (self, &current) {
                    (Self::Local, FileEndpoint::Local { path }) => {
                        fs::remove_dir(path).await.map_err(map_io_error)?
                    }
                    (
                        Self::Remote {
                            host_id, session, ..
                        },
                        FileEndpoint::Remote {
                            host_id: endpoint_host,
                            path,
                        },
                    ) if host_id == endpoint_host => session
                        .remove_dir(path.as_str())
                        .await
                        .map_err(super::super::sftp::map_sftp_error)?,
                    _ => return Err(AppError::unsupported("暂存目录端点与访问通道不匹配")),
                }
            } else {
                pending.push((current.clone(), true));
                for (child, _, _, _) in self.list_children(&current).await? {
                    pending.push((child, false));
                }
            }
        }
        Ok(())
    }

    /// 校验清理器持有的 route 确实对应 artifact endpoint，避免跨主机误删同路径文件。
    pub(super) fn owns(&self, endpoint: &FileEndpoint) -> bool {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { .. }) => true,
            (
                Self::Remote { host_id, .. },
                FileEndpoint::Remote {
                    host_id: endpoint_host,
                    ..
                },
            ) => host_id == endpoint_host,
            _ => false,
        }
    }

    /// 返回当前端点是否持有需显式关闭的 SSH/SFTP 资源。
    pub(super) fn has_remote_resources(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    /// 返回不跟随符号链接的 metadata，重试验证和递归遍历都以此区分普通目录项。
    pub(super) async fn metadata(
        &self,
        endpoint: &FileEndpoint,
    ) -> AppResult<Option<ObjectMetadata>> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => match fs::symlink_metadata(path).await {
                Ok(metadata) => Ok(Some(local_metadata(&metadata))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(map_io_error(error)),
            },
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => match session
                .symlink_metadata(path.as_str())
                .await
            {
                Ok(metadata) => Ok(Some(remote_metadata(&metadata))),
                Err(SftpError::Status(status)) if status.status_code == StatusCode::NoSuchFile => {
                    Ok(None)
                }
                Err(error) => Err(super::super::sftp::map_sftp_error(error)),
            },
            _ => Err(AppError::unsupported("文件端点与已打开的访问通道不匹配")),
        }
    }

    /// 为源对象或已存在的目标父目录取得规范路径，供读取范围与目录自包含门禁比较。
    /// 目标最终 basename 不传入此入口，写入及清理仍绑定明确目标叶而不解引用它。
    ///
    /// @author kongweiguang
    pub(super) async fn canonicalize_source(
        &self,
        endpoint: &FileEndpoint,
    ) -> AppResult<FileEndpoint> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => Ok(FileEndpoint::Local {
                path: fs::canonicalize(path).await.map_err(map_io_error)?,
            }),
            (
                Self::Remote {
                    host_id, session, ..
                },
                FileEndpoint::Remote {
                    host_id: endpoint_host,
                    path,
                },
            ) if host_id == endpoint_host => {
                let canonical = session
                    .canonicalize(path.as_str())
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                let canonical = RemotePath::parse(&canonical)
                    .map_err(|_| AppError::invalid("SFTP 规范源路径无效"))?;
                Ok(FileEndpoint::Remote {
                    host_id: host_id.clone(),
                    path: canonical,
                })
            }
            _ => Err(AppError::unsupported("源路径与已打开的访问通道不匹配")),
        }
    }

    /// 列出当前目录的一层普通项，不跟随链接并拒绝无法安全表示的远端名称。
    pub(super) async fn list_children(
        &self,
        endpoint: &FileEndpoint,
    ) -> AppResult<Vec<(FileEndpoint, String, FileEntryKind, u64)>> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let mut reader = fs::read_dir(path).await.map_err(map_io_error)?;
                let mut output = Vec::new();
                while let Some(entry) = reader.next_entry().await.map_err(map_io_error)? {
                    let child_path = entry.path();
                    let metadata = fs::symlink_metadata(&child_path)
                        .await
                        .map_err(map_io_error)?;
                    let name = entry
                        .file_name()
                        .into_string()
                        .map_err(|_| AppError::unsupported("本机目录包含无法表示的文件名"))?;
                    output.push((
                        FileEndpoint::Local { path: child_path },
                        name,
                        local_kind(&metadata),
                        metadata.len(),
                    ));
                }
                Ok(output)
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { host_id, path }) => {
                let entries = session
                    .read_dir(path.as_str())
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                let mut output = Vec::new();
                for entry in entries {
                    let name = entry.file_name();
                    let child_path = path
                        .join_name(&name)
                        .map_err(|_| AppError::invalid("SFTP 目录项名称无效"))?;
                    let metadata = entry.metadata();
                    output.push((
                        FileEndpoint::Remote {
                            host_id: host_id.clone(),
                            path: child_path,
                        },
                        name,
                        remote_kind(metadata.file_type()),
                        metadata.size.unwrap_or_default(),
                    ));
                }
                Ok(output)
            }
            _ => Err(AppError::unsupported("文件端点与已打开的访问通道不匹配")),
        }
    }

    /// 创建普通文件读取流；动态 dispatch 让本机和 SFTP 共用同一块有界复制循环。
    pub(super) async fn open_reader(&self, endpoint: &FileEndpoint) -> AppResult<BoxReader> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                Ok(Box::new(fs::File::open(path).await.map_err(map_io_error)?))
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => Ok(Box::new(
                session
                    .open(path.as_str())
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?,
            )),
            _ => Err(AppError::unsupported("文件端点与已打开的读取通道不匹配")),
        }
    }

    /// 在已准备好的 partial 上写入；create-new 防止撞上未校验文件，续传只打开已有 owner 路径。
    /// Unix 新建文件的 0600 由 Tokio OpenOptions 的内建设置方法完成，不需要额外 std trait。
    pub(super) async fn open_writer(
        &self,
        endpoint: &FileEndpoint,
        create_new: bool,
    ) -> AppResult<BoxWriter> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(create_new);
                #[cfg(unix)]
                if create_new {
                    options.mode(0o600);
                }
                Ok(Box::new(options.open(path).await.map_err(map_io_error)?))
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => {
                let file = if create_new {
                    session
                        .open_with_flags(
                            path.as_str(),
                            OpenFlags::CREATE | OpenFlags::EXCLUDE | OpenFlags::WRITE,
                        )
                        .await
                } else {
                    session
                        .open_with_flags(path.as_str(), OpenFlags::WRITE)
                        .await
                }
                .map_err(super::super::sftp::map_sftp_error)?;
                Ok(Box::new(file))
            }
            _ => Err(AppError::unsupported("文件端点与已打开的写入通道不匹配")),
        }
    }

    /// 设置文件或目录权限；本机 Windows 只映射只读语义，不把 POSIX mode 谎报为完整 ACL。
    pub(super) async fn set_permissions(
        &self,
        endpoint: &FileEndpoint,
        mode: Option<u32>,
    ) -> AppResult<()> {
        let Some(mode) = mode else { return Ok(()) };
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    fs::set_permissions(path, std::fs::Permissions::from_mode(mode & 0o7777))
                        .await
                        .map_err(map_io_error)
                }
                #[cfg(not(unix))]
                {
                    let mut permissions = fs::metadata(path)
                        .await
                        .map_err(map_io_error)?
                        .permissions();
                    permissions.set_readonly(mode & 0o222 == 0);
                    fs::set_permissions(path, permissions)
                        .await
                        .map_err(map_io_error)
                }
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => session
                .set_metadata(
                    path.as_str(),
                    FileAttributes {
                        permissions: Some(mode & 0o7777),
                        ..FileAttributes::empty()
                    },
                )
                .await
                .map_err(super::super::sftp::map_sftp_error),
            _ => Err(AppError::unsupported("文件权限设置端点与访问通道不匹配")),
        }
    }

    /// 在文件数据完成后、最终权限收紧前恢复秒级修改时间；缺失或协议越界由调用方显式处理。
    /// 本机时间写入移入 blocking pool，远端遵从 SFTP v3 的 32 位秒字段边界。
    ///
    /// @author kongweiguang
    pub(super) async fn set_modified_time(
        &self,
        endpoint: &FileEndpoint,
        seconds: u64,
    ) -> AppResult<()> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let path = path.to_path_buf();
                tokio::task::spawn_blocking(move || Self::set_local_modified_time(&path, seconds))
                    .await
                    .map_err(|error| {
                        AppError::infrastructure_with_source("本机修改时间设置任务异常", error)
                    })?
                    .map_err(map_io_error)
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => {
                let mtime = u32::try_from(seconds)
                    .map_err(|_| AppError::invalid("源文件修改时间超出 SFTP 协议范围"))?;
                session
                    .set_metadata(
                        path.as_str(),
                        FileAttributes {
                            mtime: Some(mtime),
                            ..FileAttributes::empty()
                        },
                    )
                    .await
                    .map_err(super::super::sftp::map_sftp_error)
            }
            _ => Err(AppError::unsupported("修改时间端点与访问通道不匹配")),
        }
    }

    /// 同步 partial 文件数据；续传检查点只在 flush/sync 返回后发布。
    pub(super) async fn sync_file(&self, endpoint: &FileEndpoint) -> AppResult<()> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => fs::OpenOptions::new()
                .write(true)
                .open(path)
                .await
                .map_err(map_io_error)?
                .sync_all()
                .await
                .map_err(map_io_error),
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => {
                let mut file = session
                    .open_with_flags(path.as_str(), OpenFlags::WRITE)
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                file.sync_all()
                    .await
                    .map_err(super::super::sftp::map_sftp_error)?;
                file.shutdown().await.map_err(map_io_error)
            }
            _ => Err(AppError::unsupported("文件端点与同步通道不匹配")),
        }
    }

    /// Windows 以最小属性写权限设置时间并支持目录句柄；不复用模板 HANDLE，避免 SDK 句柄类型混用。
    /// 文件 IO 放在调用方 blocking pool，Windows FILETIME 与 Unix 秒间的纪元偏移在边界集中换算。
    /// Win32 路径复用纯词法扩展前缀转换，不 canonicalize，避免转换时主动解引用目标。
    /// 路径式 open 仍受既有 check-then-use 竞态约束，不宣称具备句柄相对 IO 的原子 no-follow 保证。
    ///
    /// @author kongweiguang
    fn set_local_modified_time(path: &std::path::Path, seconds: u64) -> io::Result<()> {
        #[cfg(windows)]
        {
            use std::ptr::null;
            use windows_sys::Win32::{
                Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE},
                Storage::FileSystem::{
                    CreateFileW, FILE_FLAG_BACKUP_SEMANTICS, FILE_SHARE_DELETE, FILE_SHARE_READ,
                    FILE_SHARE_WRITE, FILE_WRITE_ATTRIBUTES, OPEN_EXISTING, SetFileTime,
                },
            };

            const WINDOWS_EPOCH_OFFSET_100NS: u64 = 116_444_736_000_000_000;
            const TICKS_PER_SECOND: u64 = 10_000_000;
            let ticks = seconds
                .checked_mul(TICKS_PER_SECOND)
                .and_then(|value| value.checked_add(WINDOWS_EPOCH_OFFSET_100NS))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "修改时间越界"))?;
            let timestamp = FILETIME {
                dwLowDateTime: ticks as u32,
                dwHighDateTime: (ticks >> 32) as u32,
            };
            let path_wide = crate::storage::windows_api_path(path)?;
            let handle = unsafe {
                CreateFileW(
                    path_wide.as_ptr(),
                    FILE_WRITE_ATTRIBUTES,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS,
                    std::ptr::null_mut(),
                )
            };
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let updated = unsafe { SetFileTime(handle, null(), null(), &timestamp) } != 0;
            let update_error = (!updated).then(io::Error::last_os_error);
            let closed = unsafe { CloseHandle(handle) } != 0;
            match (update_error, closed) {
                (Some(error), _) => Err(error),
                (None, false) => Err(io::Error::last_os_error()),
                (None, true) => Ok(()),
            }
        }
        #[cfg(not(windows))]
        {
            let modified = UNIX_EPOCH
                .checked_add(Duration::from_secs(seconds))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "修改时间越界"))?;
            std::fs::File::open(path)?.set_modified(modified)
        }
    }

    /// 只有 staging 前缀摘要验证通过后才截掉未确认尾部，避免使用文件长度推断可续传偏移。
    pub(super) async fn truncate_file(&self, endpoint: &FileEndpoint, size: u64) -> AppResult<()> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                let file = fs::OpenOptions::new()
                    .write(true)
                    .open(path)
                    .await
                    .map_err(map_io_error)?;
                file.set_len(size).await.map_err(map_io_error)
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => session
                .set_metadata(
                    path.as_str(),
                    FileAttributes {
                        size: Some(size),
                        ..FileAttributes::empty()
                    },
                )
                .await
                .map_err(super::super::sftp::map_sftp_error),
            _ => Err(AppError::unsupported("文件端点与截断通道不匹配")),
        }
    }

    /// 在目标文件系统上建立私有 staging 目录或文件的父级已存在时返回明确结果。
    pub(super) async fn create_directory(&self, endpoint: &FileEndpoint) -> AppResult<()> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => {
                fs::create_dir(path).await.map_err(map_io_error)
            }
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => session
                .create_dir(path.as_str())
                .await
                .map_err(super::super::sftp::map_sftp_error),
            _ => Err(AppError::unsupported("文件端点与已打开的写入通道不匹配")),
        }
    }

    /// 区分明确拒绝与 SFTP 回执丢失；提交期间只能对后者报告结果未知。
    pub(super) async fn create_directory_for_commit(
        &self,
        endpoint: &FileEndpoint,
    ) -> Result<(), CommitFailure> {
        match (self, endpoint) {
            (Self::Local, FileEndpoint::Local { path }) => fs::create_dir(path)
                .await
                .map_err(|error| CommitFailure::Known(map_io_error(error))),
            (Self::Remote { session, .. }, FileEndpoint::Remote { path, .. }) => {
                match session.create_dir(path.as_str()).await {
                    Ok(()) => Ok(()),
                    Err(SftpError::Status(status)) => Err(CommitFailure::Known(
                        super::super::sftp::map_sftp_error(SftpError::Status(status)),
                    )),
                    Err(error) => Err(CommitFailure::Unknown(
                        AppError::outcome_unknown_with_source("SFTP 目录创建结果未知", error),
                    )),
                }
            }
            _ => Err(CommitFailure::Known(AppError::unsupported(
                "目录创建端点与文件传输通道不匹配",
            ))),
        }
    }

    /// 用本机共享原子 helper 或远端 SFTP hardlink/posix-rename 完成最终文件提交。
    pub(super) async fn commit_file(
        &self,
        files: &SftpFileProvider,
        staging: &FileEndpoint,
        destination: &FileEndpoint,
        mode: FileCommitMode,
        cancellation: CancellationToken,
    ) -> Result<(), CommitFailure> {
        match (self, staging, destination) {
            (
                Self::Local,
                FileEndpoint::Local { path: source },
                FileEndpoint::Local { path: target },
            ) => {
                let source = absolute_path(source).map_err(CommitFailure::Known)?;
                let target = absolute_path(target).map_err(CommitFailure::Known)?;
                let commit_mode = match mode {
                    FileCommitMode::CreateNew => CommitMode::CreateNew,
                    FileCommitMode::ReplaceExisting => CommitMode::ReplaceExisting,
                };
                tokio::task::spawn_blocking(move || {
                    commit_staged_file(&source, &target, commit_mode)
                })
                .await
                .map_err(|error| {
                    CommitFailure::Unknown(AppError::outcome_unknown_with_source(
                        "本机文件提交任务结果未知",
                        error,
                    ))
                })?
                .map_err(map_atomic_commit)
            }
            (
                Self::Remote { lease, .. },
                FileEndpoint::Remote {
                    host_id,
                    path: source,
                },
                FileEndpoint::Remote {
                    host_id: target_host,
                    path: target,
                },
            ) if host_id == target_host => {
                let mode = match mode {
                    FileCommitMode::CreateNew => RemoteCommitMode::CreateNew,
                    FileCommitMode::ReplaceExisting => RemoteCommitMode::ReplaceExisting,
                };
                files
                    .commit_staged_file(lease, source, target, mode, cancellation)
                    .await
                    .map_err(|error| match error {
                        RemoteCommitError::CommitUnknown(error) => CommitFailure::Unknown(
                            AppError::outcome_unknown_with_source("SFTP 提交结果未知", error),
                        ),
                        RemoteCommitError::NotCommitted(error) => {
                            CommitFailure::Known(super::super::sftp::map_sftp_error(error))
                        }
                        RemoteCommitError::Unsupported => CommitFailure::Known(
                            AppError::unsupported("服务器不支持安全的文件提交"),
                        ),
                        RemoteCommitError::Application(error) => CommitFailure::Known(error),
                    })
            }
            _ => Err(CommitFailure::Known(AppError::unsupported(
                "原子文件提交必须留在同一文件系统",
            ))),
        }
    }

    /// 提交整棵目录时使用 no-clobber 移动；SFTP 标准 rename 对现存目标由服务器拒绝。
    pub(super) async fn commit_directory(
        &self,
        staging: &FileEndpoint,
        destination: &FileEndpoint,
    ) -> Result<(), CommitFailure> {
        match (self, staging, destination) {
            (
                Self::Local,
                FileEndpoint::Local { path: source },
                FileEndpoint::Local { path: target },
            ) => {
                let source = absolute_path(source).map_err(CommitFailure::Known)?;
                let target = absolute_path(target).map_err(CommitFailure::Known)?;
                tokio::task::spawn_blocking(move || {
                    commit_staged_file(&source, &target, CommitMode::MoveNoReplace)
                })
                .await
                .map_err(|error| {
                    CommitFailure::Unknown(AppError::outcome_unknown_with_source(
                        "本机目录提交任务结果未知",
                        error,
                    ))
                })?
                .map_err(map_atomic_commit)
            }
            (
                Self::Remote { session, .. },
                FileEndpoint::Remote { path: source, .. },
                FileEndpoint::Remote { path: target, .. },
            ) => session
                .rename(source.as_str(), target.as_str())
                .await
                .map_err(map_directory_rename_error),
            _ => Err(CommitFailure::Known(AppError::unsupported(
                "目录提交必须留在同一文件系统",
            ))),
        }
    }
}

#[derive(Clone)]
pub(super) struct ObjectMetadata {
    pub(super) kind: FileEntryKind,
    pub(super) size: u64,
    pub(super) modified: Option<String>,
    pub(super) modified_seconds: Option<u64>,
    pub(super) permissions: Option<u32>,
}

impl ObjectMetadata {
    /// 构造不含内容的基础 revision；完整摘要由文件流读取后补齐。
    pub(super) fn revision(&self) -> FileRevision {
        FileRevision {
            size: self.size,
            modified: self.modified.clone(),
            permissions: self.permissions,
            content_digest: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum FileCommitMode {
    CreateNew,
    ReplaceExisting,
}

pub(super) enum CommitFailure {
    Known(AppError),
    Unknown(AppError),
}

/// 统计本机不跟随链接的元数据并同时保留 revision 精度与可写回的秒级时间。
///
/// @author kongweiguang
fn local_metadata(metadata: &std::fs::Metadata) -> ObjectMetadata {
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    ObjectMetadata {
        kind: local_kind(metadata),
        size: metadata.len(),
        modified: modified.map(|duration| duration.as_nanos().to_string()),
        modified_seconds: modified.map(|duration| duration.as_secs()),
        permissions: local_permissions(metadata),
    }
}

/// 将 SFTP metadata 转成纯领域属性，不在传输状态或 UI 中泄露协议类型。
///
/// @author kongweiguang
fn remote_metadata(metadata: &FileAttributes) -> ObjectMetadata {
    ObjectMetadata {
        kind: remote_kind(metadata.file_type()),
        size: metadata.size.unwrap_or_default(),
        modified: metadata
            .mtime
            .map(|seconds| (u64::from(seconds) * 1_000_000_000).to_string()),
        modified_seconds: metadata.mtime.map(u64::from),
        permissions: metadata.permissions.map(|mode| mode & 0o7777),
    }
}
