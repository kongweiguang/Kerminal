//! SFTP 会话级传输 I/O helper。
//!
//! @author kongweiguang

use sha2::{Digest, Sha256};
use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
};

use russh_sftp::{client::SftpSession, protocol::FileType};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt},
};

use crate::{
    error::{AppError, AppResult},
    models::sftp::SftpTransferConflictPolicy,
};

use super::{
    backend::{io_sftp_error, native_sftp_error, SftpRuntimeSettings},
    is_already_exists_error, is_ambiguous_sftp_failure, is_no_such_file_error,
    transfer_paths::join_remote_path,
    TransferProgress,
};

enum RemoteReadFallback {
    File(String),
    Directory(String),
}

mod reliable_io;
pub(super) use reliable_io::*;
mod download;
mod remote_copy;
mod upload;
pub(in crate::services::sftp_service) use download::*;
pub(in crate::services::sftp_service) use remote_copy::*;
pub(in crate::services::sftp_service) use upload::*;
pub(super) mod checkpoint;
pub(super) use checkpoint::*;
pub(super) type RecoveryCheckpointHolder = RecoveryCheckpoints;

/// 目录每个文件独立登记可信断点，不能按文件名后缀猜测 partial 是否属于本任务。
/// 只有 flush 确认整个有界批次后才推进断点，首字节单独探测可安全重开零确认任务。
#[allow(clippy::too_many_arguments)]
async fn copy_confirmed<R, W>(
    reader: &mut R,
    writer: &mut W,
    mut offset: u64,
    hasher: &mut Sha256,
    checkpoints: &RecoveryCheckpoints,
    key: &str,
    progress: &TransferProgress,
    probe_first_write: bool,
    first_write_failed: &mut bool,
) -> AppResult<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buffer = vec![0; DOWNLOAD_READ_CHUNK_BYTES];
    loop {
        progress.ensure_not_cancelled()?;
        progress.note_network_wait("readResponse");
        let limit = if probe_first_write && offset == 0 {
            1
        } else {
            buffer.len()
        };
        let count = reader
            .read(&mut buffer[..limit])
            .await
            .map_err(io_sftp_error)?;
        progress.clear_network_wait();
        if count == 0 {
            break;
        }
        progress.refresh_activity();
        progress.note_network_wait("writeConfirmation");
        if let Err(error) = writer.write_all(&buffer[..count]).await {
            *first_write_failed = probe_first_write && offset == 0;
            return Err(io_sftp_error(error));
        }
        if let Err(error) = writer.flush().await {
            *first_write_failed = probe_first_write && offset == 0;
            return Err(io_sftp_error(error));
        }
        progress.clear_network_wait();
        hasher.update(&buffer[..count]);
        offset += count as u64;
        checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
            .record_confirmed_batch_with_prefix(key, offset, sha256_hex_digest(hasher));
        progress.add_bytes(count as u64);
    }
    Ok(())
}

/// 恢复时读取完整确认区，既校验源又校验 partial；读块只刷新活动时间，不重复累计字节。
async fn hash_prefix<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    mut length: u64,
    progress: &TransferProgress,
) -> AppResult<Sha256> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; DOWNLOAD_READ_CHUNK_BYTES];
    while length > 0 {
        progress.ensure_not_cancelled()?;
        let limit = length.min(buffer.len() as u64) as usize;
        let count = reader
            .read(&mut buffer[..limit])
            .await
            .map_err(io_sftp_error)?;
        if count == 0 {
            return Err(AppError::Sftp("断点校验前缀提前结束".into()));
        }
        hasher.update(&buffer[..count]);
        length -= count as u64;
        progress.refresh_activity();
    }
    Ok(hasher)
}

/// 两侧完整前缀必须与同一 ACK 摘要一致；源读指针停在续传位置。
async fn verify_resume<R: tokio::io::AsyncRead + AsyncSeekExt + Unpin>(
    source: &mut R,
    sftp: &SftpSession,
    partial: &str,
    offset: u64,
    checkpoints: &RecoveryCheckpoints,
    key: &str,
    progress: &TransferProgress,
) -> AppResult<Sha256> {
    progress.mark_phase("verifying", None);
    source
        .seek(SeekFrom::Start(0))
        .await
        .map_err(io_sftp_error)?;
    let hasher = hash_prefix(source, offset, progress).await?;
    if offset > 0 {
        let mut target = sftp.open(partial).await.map_err(native_sftp_error)?;
        let target_hash = hash_prefix(&mut target, offset, progress).await?;
        verify_digest(checkpoints, key, offset, &hasher, &target_hash)?;
    }
    Ok(hasher)
}

/// 严格要求摘要覆盖整个确认前缀，拒绝旧的不完整或错配记录。
fn verify_digest(
    checkpoints: &RecoveryCheckpoints,
    key: &str,
    offset: u64,
    source: &Sha256,
    target: &Sha256,
) -> AppResult<()> {
    let record = checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
        .snapshot(key)
        .ok_or_else(|| AppError::Sftp("缺少可信 checkpoint".into()))?;
    if record.source.prefix_length != offset
        || record.source.prefix_sha256 != sha256_hex_digest(source)
        || record.source.prefix_sha256 != sha256_hex_digest(target)
    {
        return Err(AppError::Sftp("断点完整前缀校验失败，保留 partial".into()));
    }
    Ok(())
}

/// 目录重试重建总量并沿用固定目标根目录，防止 rename 生成新的副本。
async fn resolve_remote_read_fallback(
    sftp: &SftpSession,
    remote_path: &str,
) -> Option<RemoteReadFallback> {
    if let Some(directory_path) = resolve_remote_directory_path(sftp, remote_path).await {
        return Some(RemoteReadFallback::Directory(directory_path));
    }

    let target_path = resolve_remote_link_target_path(sftp, remote_path).await?;
    if let Some(directory_path) = resolve_remote_directory_path(sftp, &target_path).await {
        return Some(RemoteReadFallback::Directory(directory_path));
    }
    Some(RemoteReadFallback::File(target_path))
}

/// 顶层文件请求先用 STAT 识别目录，兼容允许 OPEN 目录句柄的 SFTP 服务端。
async fn resolve_file_request_directory(sftp: &SftpSession, remote_path: &str) -> Option<String> {
    let metadata = sftp.metadata(remote_path.to_owned()).await.ok()?;
    if !metadata.is_dir() {
        return None;
    }

    resolve_remote_directory_path(sftp, remote_path)
        .await
        .or_else(|| Some(remote_path.to_owned()))
}

async fn resolve_remote_directory_path(sftp: &SftpSession, remote_path: &str) -> Option<String> {
    if sftp.read_dir(remote_path.to_owned()).await.is_ok() {
        return Some(remote_path.to_owned());
    }

    if let Some(target_path) = resolve_remote_link_target_path(sftp, remote_path).await {
        if sftp.read_dir(target_path.clone()).await.is_ok() {
            return Some(target_path);
        }
    }

    let metadata = sftp.metadata(remote_path.to_owned()).await.ok()?;
    if !metadata.is_dir() {
        return None;
    }

    let canonical_path =
        normalize_remote_fallback_path(&sftp.canonicalize(remote_path).await.ok()?);
    if canonical_path != normalize_remote_fallback_path(remote_path)
        && sftp.read_dir(canonical_path.clone()).await.is_ok()
    {
        return Some(canonical_path);
    }

    None
}

async fn resolve_remote_link_target_path(sftp: &SftpSession, remote_path: &str) -> Option<String> {
    let target = sftp.read_link(remote_path.to_owned()).await.ok()?;
    resolve_remote_link_target(remote_path, &target)
}

fn resolve_remote_link_target(link_path: &str, target: &str) -> Option<String> {
    let target = normalize_remote_fallback_path(target);
    if target.is_empty() {
        return None;
    }
    if target.starts_with('/') {
        return Some(target);
    }
    Some(join_remote_path(&remote_parent_path(link_path), &target))
}

fn remote_parent_path(path: &str) -> String {
    let path = normalize_remote_fallback_path(path);
    let path = path.trim_end_matches('/');
    match path.rfind('/') {
        Some(0) | None => "/".to_owned(),
        Some(index) => path[..index].to_owned(),
    }
}

fn normalize_remote_fallback_path(path: &str) -> String {
    let mut normalized = path.trim().replace('\\', "/");
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

/// 每个文件独立记录可信断点，目录枚举不依赖文件名后缀判断所有权。
/// 已提交文件在恢复任务中已计入进度；这里只计入冲突策略真正跳过的新文件。
fn credit_skipped_or_committed(
    progress: &TransferProgress,
    key: &str,
    source_size: u64,
    single_file: bool,
) -> AppResult<()> {
    let checkpoints = progress.recovery_checkpoints();
    let state = checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?;
    let committed = state.record_committed(key);
    let confirmed = state.confirmed_bytes();
    drop(state);
    if committed {
        if single_file {
            progress.set_confirmed_bytes(confirmed);
        }
    } else {
        progress.add_bytes(source_size);
    }
    Ok(())
}

async fn ensure_remote_directory(sftp: &SftpSession, remote_path: &str) -> AppResult<()> {
    if let Err(error) = sftp.create_dir(remote_path.to_owned()).await {
        if !remote_create_conflict_confirmed(sftp, remote_path, &error, true).await {
            return Err(native_sftp_error(error));
        }
    }
    Ok(())
}

/// 目录级 rename 只选择一次实际目标，防止恢复时换号后把已完成子文件复制第二遍。
async fn prepare_task_remote_directory_root(
    sftp: &SftpSession,
    local_path: &Path,
    requested_remote_path: &str,
    conflict_policy: SftpTransferConflictPolicy,
    progress: &TransferProgress,
) -> AppResult<Option<String>> {
    let key = recovery_key(&local_path.to_string_lossy(), requested_remote_path);
    let checkpoints = progress.recovery_checkpoints();
    let remembered = checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
        .directory(&key);
    if let Some(root) = remembered {
        ensure_remote_directory(sftp, &root).await?;
        return Ok(Some(root));
    }
    let chosen =
        prepare_remote_directory_root(sftp, requested_remote_path, conflict_policy).await?;
    if let Some(root) = &chosen {
        checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
            .remember_directory(key, root.clone());
    }
    Ok(chosen)
}

/// 本地目录级 rename 也必须固定，避免下载重试改写到新的编号目录。
async fn prepare_task_local_directory_root(
    remote_path: &str,
    requested_local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    progress: &TransferProgress,
) -> AppResult<Option<PathBuf>> {
    let key = recovery_key(remote_path, &requested_local_path.to_string_lossy());
    let checkpoints = progress.recovery_checkpoints();
    let remembered = checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
        .directory(&key);
    if let Some(root) = remembered {
        fs::create_dir_all(&root).await?;
        return Ok(Some(PathBuf::from(root)));
    }
    let chosen = prepare_local_directory_root(requested_local_path, conflict_policy).await?;
    if let Some(root) = &chosen {
        checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
            .remember_directory(key, root.to_string_lossy().into_owned());
    }
    Ok(chosen)
}

async fn prepare_remote_directory_root(
    sftp: &SftpSession,
    remote_path: &str,
    conflict_policy: SftpTransferConflictPolicy,
) -> AppResult<Option<String>> {
    match conflict_policy {
        SftpTransferConflictPolicy::Overwrite => {
            ensure_remote_directory(sftp, remote_path).await?;
            Ok(Some(remote_path.to_owned()))
        }
        SftpTransferConflictPolicy::Skip => match sftp.create_dir(remote_path.to_owned()).await {
            Ok(()) => Ok(Some(remote_path.to_owned())),
            Err(error) => {
                if remote_create_conflict_confirmed(sftp, remote_path, &error, true).await {
                    Ok(None)
                } else {
                    Err(native_sftp_error(error))
                }
            }
        },
        SftpTransferConflictPolicy::Rename => {
            for candidate in remote_conflict_candidates(remote_path).take(1000) {
                match sftp.create_dir(candidate.clone()).await {
                    Ok(()) => return Ok(Some(candidate)),
                    Err(error) => {
                        if remote_create_conflict_confirmed(sftp, &candidate, &error, true).await {
                            continue;
                        }
                        return Err(native_sftp_error(error));
                    }
                }
            }
            Err(AppError::Sftp(format!(
                "无法为远程目标生成不冲突的目录名: {remote_path}"
            )))
        }
    }
}

pub(super) async fn prepare_local_directory_root(
    local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
) -> AppResult<Option<PathBuf>> {
    match conflict_policy {
        SftpTransferConflictPolicy::Overwrite => {
            fs::create_dir_all(local_path).await?;
            Ok(Some(local_path.to_path_buf()))
        }
        SftpTransferConflictPolicy::Skip if fs::try_exists(local_path).await? => Ok(None),
        SftpTransferConflictPolicy::Skip => {
            fs::create_dir_all(local_path).await?;
            Ok(Some(local_path.to_path_buf()))
        }
        SftpTransferConflictPolicy::Rename => {
            for candidate in local_conflict_candidates(local_path).take(1000) {
                match fs::create_dir(&candidate).await {
                    Ok(()) => return Ok(Some(candidate)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(AppError::Sftp(format!(
                "无法为本地目标生成不冲突的目录名: {}",
                local_path.display()
            )))
        }
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::{
        verify_digest, RecoveryCheckpointRecord, RecoveryCheckpoints, RecoverySourceFingerprint,
        Sha256,
    };
    use sha2::Digest;
    use std::sync::{Arc, Mutex};

    /// 即使大小、修改时间都不变，源或 partial 任一已确认字节变化也必须拒绝续传。
    #[test]
    fn full_confirmed_prefix_rejects_source_or_partial_corruption() {
        let checkpoints: RecoveryCheckpoints =
            Arc::new(Mutex::new(super::TransferRecoveryState::new()));
        let mut expected = Sha256::new();
        expected.update(b"abcd");
        checkpoints.lock().expect("checkpoint lock").upsert(
            "item",
            RecoveryCheckpointRecord {
                actual_target: "target".into(),
                partial_path: "target.kerminal-part".into(),
                confirmed_offset: 4,
                source: RecoverySourceFingerprint {
                    size: 8,
                    mtime_seconds: Some(123),
                    prefix_sha256: super::sha256_hex_digest(&expected),
                    prefix_length: 4,
                },
                committed: false,
                backup_path: None,
                expected_sha256: None,
            },
        );
        let mut changed_source = Sha256::new();
        changed_source.update(b"abce");
        let mut changed_partial = Sha256::new();
        changed_partial.update(b"abcf");
        assert!(verify_digest(&checkpoints, "item", 4, &expected, &expected).is_ok());
        assert!(verify_digest(&checkpoints, "item", 4, &changed_source, &expected).is_err());
        assert!(verify_digest(&checkpoints, "item", 4, &expected, &changed_partial).is_err());
    }
}
