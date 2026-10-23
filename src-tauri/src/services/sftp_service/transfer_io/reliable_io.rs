//! 可靠传输的 partial、resume、冲突与提交语义。
//! @author kongweiguang

use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
};

use russh_sftp::{
    client::{fs::File as SftpFile, SftpSession},
    protocol::OpenFlags,
};
use sha2::{Digest, Sha256};
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
};

use super::checkpoint::*;

mod local;
mod recovery;
mod remote;
mod remote_commit;
pub(in crate::services::sftp_service) use local::*;
pub(in crate::services::sftp_service) use recovery::*;
pub(in crate::services::sftp_service) use remote::*;
pub(in crate::services::sftp_service) use remote_commit::*;

use crate::{
    error::{AppError, AppResult},
    models::sftp::SftpTransferConflictPolicy,
};

use super::{
    io_sftp_error, is_already_exists_error, is_ambiguous_sftp_failure, is_no_such_file_error,
    join_remote_path, native_sftp_error, remote_parent_path,
};

const RELIABLE_PARTIAL_SUFFIX: &str = ".kerminal-part";
pub(in crate::services::sftp_service) const DOWNLOAD_READ_CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::sftp_service) enum ReliableWriteDecision {
    SkipExistingFinal,
    ChooseRenamedFinal,
    Fresh,
    Resume { offset: u64 },
    CommitExistingPartial,
    RestartPartial,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::services::sftp_service) enum ReliableSizeConfirmation {
    Verified,
    Mismatch { expected: u64, actual: u64 },
}

pub(in crate::services::sftp_service) struct PreparedLocalReliableWriteTarget {
    pub(in crate::services::sftp_service) final_path: PathBuf,
    pub(in crate::services::sftp_service) partial_path: PathBuf,
    pub(in crate::services::sftp_service) offset: u64,
    pub(in crate::services::sftp_service) file: fs::File,
}

pub(in crate::services::sftp_service) struct PreparedRemoteReliableWriteTarget {
    pub(in crate::services::sftp_service) final_path: String,
    pub(in crate::services::sftp_service) partial_path: String,
    pub(in crate::services::sftp_service) offset: u64,
    pub(in crate::services::sftp_service) file: SftpFile,
}

pub(in crate::services::sftp_service) fn reliable_partial_file_name(name: &str) -> String {
    let trimmed = name.trim();
    let name = if trimmed.is_empty() { "file" } else { trimmed };
    format!("{name}{RELIABLE_PARTIAL_SUFFIX}")
}

pub(in crate::services::sftp_service) fn reliable_remote_partial_path(final_path: &str) -> String {
    let trimmed = final_path.trim().trim_end_matches('/');
    let name = trimmed.rsplit('/').next().unwrap_or("file");
    join_remote_path(
        &remote_parent_path(final_path),
        &reliable_partial_file_name(name),
    )
}

pub(in crate::services::sftp_service) fn reliable_local_partial_path(final_path: &Path) -> PathBuf {
    let name = final_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let partial_name = reliable_partial_file_name(name);
    final_path
        .parent()
        .map(|parent| parent.join(&partial_name))
        .unwrap_or_else(|| PathBuf::from(partial_name))
}

pub(in crate::services::sftp_service) fn plan_reliable_write(
    conflict_policy: SftpTransferConflictPolicy,
    final_exists: bool,
    source_bytes: u64,
    partial_bytes: Option<u64>,
) -> ReliableWriteDecision {
    if final_exists {
        match conflict_policy {
            SftpTransferConflictPolicy::Skip => return ReliableWriteDecision::SkipExistingFinal,
            SftpTransferConflictPolicy::Rename => {
                return ReliableWriteDecision::ChooseRenamedFinal;
            }
            SftpTransferConflictPolicy::Overwrite => {}
        }
    }

    match partial_bytes {
        None | Some(0) if source_bytes > 0 => ReliableWriteDecision::Fresh,
        None => ReliableWriteDecision::Fresh,
        Some(partial_bytes) if partial_bytes < source_bytes => ReliableWriteDecision::Resume {
            offset: partial_bytes,
        },
        Some(partial_bytes) if partial_bytes == source_bytes => {
            ReliableWriteDecision::CommitExistingPartial
        }
        Some(_) => ReliableWriteDecision::RestartPartial,
    }
}

pub(in crate::services::sftp_service) fn confirm_reliable_write_size(
    expected: u64,
    actual: u64,
) -> ReliableSizeConfirmation {
    if expected == actual {
        ReliableSizeConfirmation::Verified
    } else {
        ReliableSizeConfirmation::Mismatch { expected, actual }
    }
}

pub(in crate::services::sftp_service) async fn remote_create_conflict_confirmed(
    sftp: &SftpSession,
    remote_path: &str,
    error: &russh_sftp::client::error::Error,
    require_directory: bool,
) -> bool {
    if is_already_exists_error(error) {
        return true;
    }
    if !is_ambiguous_sftp_failure(error) {
        return false;
    }
    match sftp.metadata(remote_path.to_owned()).await {
        Ok(metadata) => !require_directory || metadata.is_dir(),
        Err(_) => false,
    }
}

/// 在具体请求处保留操作名称；同一个权限状态不能掩盖 STAT、OPEN 或断点截断的差别。
fn remote_write_error(operation: &str, error: russh_sftp::client::error::Error) -> AppError {
    AppError::Sftp(format!("{operation}失败: {error}"))
}

/// 只有明确不存在才按新目标处理，权限错误保留 STAT 阶段，避免误走创建或覆盖。
async fn remote_path_exists(sftp: &SftpSession, remote_path: &str) -> AppResult<bool> {
    match sftp.metadata(remote_path.to_owned()).await {
        Ok(_) => Ok(true),
        Err(error) if is_no_such_file_error(&error) => Ok(false),
        Err(error) => Err(remote_write_error("检查正式目标文件（STAT）", error)),
    }
}

/// 为真实源文件准备远端 partial；新任务隔离历史残留，同任务恢复只信任已登记的路径和偏移。
pub(in crate::services::sftp_service) async fn prepare_remote_reliable_write_target_for_source(
    sftp: &SftpSession,
    remote_path: &str,
    conflict_policy: SftpTransferConflictPolicy,
    source: &RecoverySourceFingerprint,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedRemoteReliableWriteTarget>> {
    prepare_remote_target_with_checkpoint(
        sftp,
        remote_path,
        conflict_policy,
        source.size,
        Some(source),
        checkpoints,
        checkpoint_key,
    )
    .await
}

/// 恢复优先锁定首次实际目标；正式文件若已完整提交则用 SHA 对账，避免回执丢失后重复上传。
pub(in crate::services::sftp_service) async fn open_local_write_target(
    local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    skipped_bytes: u64,
) -> AppResult<Option<fs::File>> {
    match conflict_policy {
        SftpTransferConflictPolicy::Overwrite => fs::File::create(local_path)
            .await
            .map(Some)
            .map_err(Into::into),
        SftpTransferConflictPolicy::Skip => {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(local_path)
                .await
            {
                Ok(file) => Ok(Some(file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let _ = skipped_bytes;
                    Ok(None)
                }
                Err(error) => Err(error.into()),
            }
        }
        SftpTransferConflictPolicy::Rename => {
            for candidate in local_conflict_candidates(local_path).take(1000) {
                match fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(candidate)
                    .await
                {
                    Ok(file) => return Ok(Some(file)),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            Err(AppError::Sftp(format!(
                "无法为本地目标生成不冲突的文件名: {}",
                local_path.display()
            )))
        }
    }
}

pub(in crate::services::sftp_service) fn remote_conflict_candidates(
    remote_path: &str,
) -> impl Iterator<Item = String> + '_ {
    std::iter::once(remote_path.to_owned()).chain((1..).map(move |index| {
        let parent = remote_parent_path(remote_path);
        let name = remote_path
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or(remote_path);
        join_remote_path(&parent, &numbered_candidate_name(name, index))
    }))
}

pub(in crate::services::sftp_service) fn local_conflict_candidates(
    local_path: &Path,
) -> impl Iterator<Item = PathBuf> + '_ {
    std::iter::once(local_path.to_path_buf()).chain((1..).map(move |index| {
        let name = local_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let candidate_name = numbered_candidate_name(name, index);
        local_path
            .parent()
            .map(|parent| parent.join(&candidate_name))
            .unwrap_or_else(|| PathBuf::from(candidate_name))
    }))
}

pub(in crate::services::sftp_service) fn numbered_candidate_name(
    name: &str,
    index: usize,
) -> String {
    let trimmed = name.trim();
    let name = if trimmed.is_empty() { "file" } else { trimmed };
    let Some(dot_index) = name.rfind('.') else {
        return format!("{name} ({index})");
    };
    if dot_index == 0 {
        return format!("{name} ({index})");
    }
    let (stem, extension) = name.split_at(dot_index);
    format!("{stem} ({index}){extension}")
}

pub(in crate::services::sftp_service) async fn calculate_local_directory_bytes(
    path: &Path,
) -> AppResult<u64> {
    let mut total = 0_u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let mut entries = fs::read_dir(&dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let metadata = entry.metadata().await?;
            if metadata.is_dir() {
                stack.push(entry.path());
            } else if metadata.is_file() {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::{
        commit_local_reliable_write_target, empty_prefix_digest, hex_digest,
        prepare_local_reliable_write_target_for_source, safe_first_write_retry_checkpoint,
    };
    use crate::{
        models::sftp::SftpTransferConflictPolicy,
        services::sftp_service::transfer_io::checkpoint::{
            new_recovery_checkpoints, RecoveryCheckpointRecord, RecoverySourceFingerprint,
        },
    };
    use sha2::{Digest, Sha256};

    /// 首写兼容重试只接受零确认的同一文件；非空断点和已提交项绝不能重新截断。
    #[test]
    fn first_write_retry_requires_trusted_zero_checkpoint() {
        let source = RecoverySourceFingerprint {
            size: 8,
            mtime_seconds: Some(123),
            prefix_sha256: empty_prefix_digest(),
            prefix_length: 0,
        };
        let mut record = RecoveryCheckpointRecord {
            actual_target: "/target".into(),
            partial_path: "/target.kerminal-part".into(),
            confirmed_offset: 0,
            source: source.clone(),
            committed: false,
            backup_path: None,
            expected_sha256: None,
        };
        assert!(safe_first_write_retry_checkpoint(
            &record,
            "/target",
            "/target.kerminal-part",
            &source,
        ));
        record.confirmed_offset = 1;
        assert!(!safe_first_write_retry_checkpoint(
            &record,
            "/target",
            "/target.kerminal-part",
            &source,
        ));
        record.confirmed_offset = 0;
        record.committed = true;
        assert!(!safe_first_write_retry_checkpoint(
            &record,
            "/target",
            "/target.kerminal-part",
            &source,
        ));
        record.committed = false;
        assert!(!safe_first_write_retry_checkpoint(
            &record,
            "/other",
            "/target.kerminal-part",
            &source,
        ));
    }

    /// 已存在正式文件也只能经同目录原子替换，不能先删除旧文件。
    #[tokio::test]
    async fn local_commit_replaces_existing_final_atomically() {
        let directory = tempfile::tempdir().expect("temp directory");
        let final_path = directory.path().join("target.txt");
        let partial_path = directory.path().join("target.txt.kerminal-part");
        tokio::fs::write(&final_path, b"old")
            .await
            .expect("old final");
        tokio::fs::write(&partial_path, b"new contents")
            .await
            .expect("partial");

        commit_local_reliable_write_target(&final_path, &partial_path, 12)
            .await
            .expect("commit");

        assert_eq!(
            tokio::fs::read(&final_path).await.expect("final"),
            b"new contents"
        );
        assert!(!tokio::fs::try_exists(&partial_path)
            .await
            .expect("partial state"));
    }

    /// 大小不符时保持两个文件原样，方便用户重试或检查故障残留。
    #[tokio::test]
    async fn local_commit_mismatch_preserves_final_and_partial() {
        let directory = tempfile::tempdir().expect("temp directory");
        let final_path = directory.path().join("target.txt");
        let partial_path = directory.path().join("target.txt.kerminal-part");
        tokio::fs::write(&final_path, b"old")
            .await
            .expect("old final");
        tokio::fs::write(&partial_path, b"short")
            .await
            .expect("partial");

        assert!(
            commit_local_reliable_write_target(&final_path, &partial_path, 9)
                .await
                .is_err()
        );

        assert_eq!(tokio::fs::read(&final_path).await.expect("final"), b"old");
        assert_eq!(
            tokio::fs::read(&partial_path).await.expect("partial"),
            b"short"
        );
    }

    /// 提交回执未知时完整哈希可识别已替换的正式文件，重试不会再打开写句柄。
    #[tokio::test]
    async fn retry_reconciles_already_committed_local_file() {
        let directory = tempfile::tempdir().expect("temp directory");
        let final_path = directory.path().join("target.txt");
        let contents = b"already committed";
        tokio::fs::write(&final_path, contents)
            .await
            .expect("final");
        let source = RecoverySourceFingerprint {
            size: contents.len() as u64,
            mtime_seconds: Some(123),
            prefix_sha256: hex_digest(&Sha256::digest(contents)),
            prefix_length: contents.len() as u64,
        };
        let checkpoints = new_recovery_checkpoints();
        checkpoints.lock().expect("checkpoint lock").upsert(
            "item",
            RecoveryCheckpointRecord {
                actual_target: final_path.to_string_lossy().into_owned(),
                partial_path: directory
                    .path()
                    .join("target.txt.kerminal-part")
                    .to_string_lossy()
                    .into_owned(),
                confirmed_offset: contents.len() as u64,
                source: source.clone(),
                committed: false,
                backup_path: None,
                expected_sha256: None,
            },
        );

        let prepared = prepare_local_reliable_write_target_for_source(
            &final_path,
            SftpTransferConflictPolicy::Overwrite,
            &source,
            Some(&checkpoints),
            "item",
        )
        .await
        .expect("reconcile");
        assert!(prepared.is_none());
        assert!(checkpoints
            .lock()
            .expect("checkpoint lock")
            .record_committed("item"));
        assert_eq!(tokio::fs::read(&final_path).await.expect("final"), contents);
    }

    /// 新连接先裁掉未确认的乱序尾部，再从可信偏移续写；原正式文件一直不受影响。
    #[tokio::test]
    async fn retry_truncates_unconfirmed_partial_tail() {
        let directory = tempfile::tempdir().expect("temp directory");
        let final_path = directory.path().join("target.txt");
        let partial_path = directory.path().join("target.txt.kerminal-part");
        tokio::fs::write(&final_path, b"original")
            .await
            .expect("final");
        tokio::fs::write(&partial_path, b"abcdXX")
            .await
            .expect("partial");
        let source = RecoverySourceFingerprint {
            size: 8,
            mtime_seconds: Some(123),
            prefix_sha256: hex_digest(&Sha256::digest(b"abcd")),
            prefix_length: 4,
        };
        let checkpoints = new_recovery_checkpoints();
        checkpoints.lock().expect("checkpoint lock").upsert(
            "item",
            RecoveryCheckpointRecord {
                actual_target: final_path.to_string_lossy().into_owned(),
                partial_path: partial_path.to_string_lossy().into_owned(),
                confirmed_offset: 4,
                source: source.clone(),
                committed: false,
                backup_path: None,
                expected_sha256: None,
            },
        );

        let target = prepare_local_reliable_write_target_for_source(
            &final_path,
            SftpTransferConflictPolicy::Overwrite,
            &source,
            Some(&checkpoints),
            "item",
        )
        .await
        .expect("resume")
        .expect("target");
        assert_eq!(target.offset, 4);
        drop(target);
        assert_eq!(
            tokio::fs::read(&partial_path).await.expect("partial"),
            b"abcd"
        );
        assert_eq!(
            tokio::fs::read(&final_path).await.expect("final"),
            b"original"
        );
        use tokio::io::AsyncWriteExt;
        let mut resumed = tokio::fs::OpenOptions::new()
            .append(true)
            .open(&partial_path)
            .await
            .expect("reopen");
        resumed.write_all(b"efgh").await.expect("remaining bytes");
        resumed.sync_all().await.expect("sync");
        drop(resumed);
        commit_local_reliable_write_target(&final_path, &partial_path, 8)
            .await
            .expect("commit");
        let final_bytes = tokio::fs::read(&final_path).await.expect("final");
        assert_eq!(final_bytes, b"abcdefgh");
        assert_eq!(
            hex_digest(&Sha256::digest(&final_bytes)),
            hex_digest(&Sha256::digest(b"abcdefgh"))
        );
    }

    /// 修改时间变化说明源身份不再可信；重试拒绝并保留正式文件与 partial。
    #[tokio::test]
    async fn retry_rejects_changed_source_without_touching_files() {
        let directory = tempfile::tempdir().expect("temp directory");
        let final_path = directory.path().join("target.txt");
        let partial_path = directory.path().join("target.txt.kerminal-part");
        tokio::fs::write(&final_path, b"original")
            .await
            .expect("final");
        tokio::fs::write(&partial_path, b"abcdXX")
            .await
            .expect("partial");
        let source = RecoverySourceFingerprint {
            size: 8,
            mtime_seconds: Some(123),
            prefix_sha256: hex_digest(&Sha256::digest(b"abcd")),
            prefix_length: 4,
        };
        let checkpoints = new_recovery_checkpoints();
        checkpoints.lock().expect("checkpoint lock").upsert(
            "item",
            RecoveryCheckpointRecord {
                actual_target: final_path.to_string_lossy().into_owned(),
                partial_path: partial_path.to_string_lossy().into_owned(),
                confirmed_offset: 4,
                source: source.clone(),
                committed: false,
                backup_path: None,
                expected_sha256: None,
            },
        );
        let changed_source = RecoverySourceFingerprint {
            mtime_seconds: Some(124),
            ..source
        };

        assert!(prepare_local_reliable_write_target_for_source(
            &final_path,
            SftpTransferConflictPolicy::Overwrite,
            &changed_source,
            Some(&checkpoints),
            "item",
        )
        .await
        .is_err());
        assert_eq!(
            tokio::fs::read(&partial_path).await.expect("partial"),
            b"abcdXX"
        );
        assert_eq!(
            tokio::fs::read(&final_path).await.expect("final"),
            b"original"
        );
    }
}
