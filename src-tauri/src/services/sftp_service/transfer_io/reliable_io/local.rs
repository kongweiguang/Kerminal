//! local reliable transfer helpers.
//! @author kongweiguang

use super::*;

pub(in crate::services::sftp_service) async fn prepare_local_reliable_write_target(
    local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    source_bytes: u64,
) -> AppResult<Option<PreparedLocalReliableWriteTarget>> {
    prepare_local_target_with_checkpoint(local_path, conflict_policy, source_bytes, None, None, "")
        .await
}

/// 为真实源文件准备本地 partial；身份或前缀不匹配时拒绝错误续传。
pub(in crate::services::sftp_service) async fn prepare_local_reliable_write_target_for_source(
    local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    source: &RecoverySourceFingerprint,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedLocalReliableWriteTarget>> {
    prepare_local_target_with_checkpoint(
        local_path,
        conflict_policy,
        source.size,
        Some(source),
        checkpoints,
        checkpoint_key,
    )
    .await
}

/// 本地恢复也绑定最初目标与源身份，提交回执未知时先比较正式文件完整摘要。
async fn prepare_local_target_with_checkpoint(
    local_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    source_bytes: u64,
    source: Option<&RecoverySourceFingerprint>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedLocalReliableWriteTarget>> {
    if let Some(parent) = local_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    if let (Some(source), Some(checkpoints)) = (source, checkpoints) {
        let existing = checkpoints
            .lock()
            .ok()
            .and_then(|state| state.snapshot(checkpoint_key));
        if let Some(record) = existing {
            if !source_identity_matches(source, &record.source) {
                return Err(AppError::Sftp(
                    "可靠传输断点校验失败：源文件大小或修改时间已变化".to_owned(),
                ));
            }
            if record.committed {
                return Ok(None);
            }
            if local_final_matches_checkpoint(&record).await? {
                checkpoints
                    .lock()
                    .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
                    .mark_committed(checkpoint_key);
                return Ok(None);
            }
            if source.mtime_seconds.is_none() {
                return Err(AppError::Sftp("缺少源修改时间，不能安全恢复".into()));
            }
            let actual_target = record.actual_target.clone();
            if conflict_policy != SftpTransferConflictPolicy::Overwrite
                && fs::try_exists(&actual_target).await?
            {
                return Err(AppError::Sftp(
                    "恢复目标已被其他文件占用，保留 partial".into(),
                ));
            }
            return prepare_selected_local_reliable_write_target(
                Path::new(&actual_target),
                conflict_policy,
                fs::try_exists(Path::new(&record.actual_target)).await?,
                source_bytes,
                Some(source),
                Some(record),
                Some(checkpoints),
                checkpoint_key,
            )
            .await;
        }
    }
    match conflict_policy {
        SftpTransferConflictPolicy::Overwrite => {
            prepare_selected_local_reliable_write_target(
                local_path,
                conflict_policy,
                fs::try_exists(local_path).await?,
                source_bytes,
                source,
                None,
                checkpoints,
                checkpoint_key,
            )
            .await
        }
        SftpTransferConflictPolicy::Skip if fs::try_exists(local_path).await? => Ok(None),
        SftpTransferConflictPolicy::Skip => {
            prepare_selected_local_reliable_write_target(
                local_path,
                conflict_policy,
                false,
                source_bytes,
                source,
                None,
                checkpoints,
                checkpoint_key,
            )
            .await
        }
        SftpTransferConflictPolicy::Rename => {
            for candidate in local_conflict_candidates(local_path).take(1000) {
                if fs::try_exists(&candidate).await? {
                    continue;
                }
                if local_partial_bytes(&reliable_local_partial_path(&candidate))
                    .await?
                    .is_some()
                    && source.is_some()
                {
                    continue;
                }
                return prepare_selected_local_reliable_write_target(
                    &candidate,
                    conflict_policy,
                    false,
                    source_bytes,
                    source,
                    None,
                    checkpoints,
                    checkpoint_key,
                )
                .await;
            }
            Err(AppError::Sftp(format!(
                "无法为本地目标生成不冲突的文件名: {}",
                local_path.display()
            )))
        }
    }
}

/// 仅允许任务内可信断点续写，未知 partial 不用文件大小猜测来源或安全偏移。
#[allow(clippy::too_many_arguments)]
async fn prepare_selected_local_reliable_write_target(
    final_path: &Path,
    conflict_policy: SftpTransferConflictPolicy,
    final_exists: bool,
    source_bytes: u64,
    source: Option<&RecoverySourceFingerprint>,
    existing_checkpoint: Option<RecoveryCheckpointRecord>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedLocalReliableWriteTarget>> {
    let partial_path = reliable_local_partial_path(final_path);
    let partial_bytes = local_partial_bytes(&partial_path).await?;
    let checkpoint = existing_checkpoint.or_else(|| {
        (source.is_some() && partial_bytes.is_none()).then(|| RecoveryCheckpointRecord {
            actual_target: final_path.to_string_lossy().into_owned(),
            partial_path: partial_path.to_string_lossy().into_owned(),
            confirmed_offset: 0,
            source: source.cloned().expect("source is present"),
            committed: false,
            backup_path: None,
            expected_sha256: None,
        })
    });
    let resume_offset = match (source, partial_bytes, checkpoint.as_ref()) {
        (Some(source), Some(bytes), Some(checkpoint)) => {
            if bytes > source.size {
                return Err(AppError::Sftp(
                    "可靠传输断点校验失败：partial 大小超过源文件".to_owned(),
                ));
            }
            if !source_identity_matches(source, &checkpoint.source) {
                return Err(AppError::Sftp(
                    "可靠传输断点校验失败：源文件身份或前缀已变化".to_owned(),
                ));
            }
            if checkpoint.confirmed_offset > bytes {
                return Err(AppError::Sftp(
                    "可靠传输断点校验失败：确认偏移超过 partial 实际长度".to_owned(),
                ));
            }
            Some(checkpoint.confirmed_offset)
        }
        (Some(_), Some(_), None) => {
            return Err(AppError::Sftp(
                "可靠传输无法恢复：partial 没有同一任务的可信 checkpoint".to_owned(),
            ));
        }
        (Some(_), None, Some(record)) if record.confirmed_offset > 0 => {
            return Err(AppError::Sftp(
                "已确认 partial 已丢失，不能重新猜测恢复".into(),
            ))
        }
        (Some(_), None, _) => Some(0),
        (None, bytes, _) => bytes,
    };
    let decision = if source.is_some() {
        match (final_exists, conflict_policy, resume_offset) {
            (true, SftpTransferConflictPolicy::Skip, _) => ReliableWriteDecision::SkipExistingFinal,
            (true, SftpTransferConflictPolicy::Rename, Some(offset)) => {
                ReliableWriteDecision::Resume { offset }
            }
            (true, SftpTransferConflictPolicy::Rename, None) => {
                ReliableWriteDecision::ChooseRenamedFinal
            }
            (_, _, Some(offset)) if offset == source_bytes => {
                ReliableWriteDecision::CommitExistingPartial
            }
            (_, _, Some(offset)) if offset < source_bytes => {
                ReliableWriteDecision::Resume { offset }
            }
            (_, _, Some(_)) => ReliableWriteDecision::RestartPartial,
            _ => ReliableWriteDecision::Fresh,
        }
    } else {
        plan_reliable_write(conflict_policy, final_exists, source_bytes, partial_bytes)
    };
    match decision {
        ReliableWriteDecision::SkipExistingFinal => Ok(None),
        ReliableWriteDecision::ChooseRenamedFinal => Err(AppError::Sftp(format!(
            "无法为本地目标生成不冲突的文件名: {}",
            final_path.display()
        ))),
        ReliableWriteDecision::Fresh | ReliableWriteDecision::RestartPartial => {
            open_local_reliable_partial_target(
                final_path,
                partial_path,
                0,
                true,
                source,
                checkpoints,
                checkpoint_key,
            )
            .await
            .map(Some)
        }
        ReliableWriteDecision::Resume { offset } => open_local_reliable_partial_target(
            final_path,
            partial_path,
            offset,
            false,
            source,
            checkpoints,
            checkpoint_key,
        )
        .await
        .map(Some),
        ReliableWriteDecision::CommitExistingPartial => open_local_reliable_partial_target(
            final_path,
            partial_path,
            source_bytes,
            false,
            source,
            checkpoints,
            checkpoint_key,
        )
        .await
        .map(Some),
    }
}

async fn local_partial_bytes(partial_path: &Path) -> AppResult<Option<u64>> {
    match fs::metadata(partial_path).await {
        Ok(metadata) => Ok(Some(metadata.len())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// 先截去未确认尾部再 seek 到确认点；失败仍留下非空 partial 供用户检查或重试。
async fn open_local_reliable_partial_target(
    final_path: &Path,
    partial_path: PathBuf,
    offset: u64,
    truncate: bool,
    source: Option<&RecoverySourceFingerprint>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<PreparedLocalReliableWriteTarget> {
    if let Some(parent) = partial_path.parent() {
        fs::create_dir_all(parent).await?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(truncate)
        .open(&partial_path)
        .await?;
    if !truncate {
        file.set_len(offset).await?;
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset)).await?;
    }
    let checkpoint = source.map(|source| RecoveryCheckpointRecord {
        actual_target: final_path.to_string_lossy().into_owned(),
        partial_path: partial_path.to_string_lossy().into_owned(),
        confirmed_offset: offset,
        source: source.clone(),
        committed: false,
        backup_path: None,
        expected_sha256: None,
    });
    if let (Some(checkpoint), Some(checkpoints)) = (&checkpoint, checkpoints) {
        checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("可靠传输 checkpoint 状态锁定失败".to_owned()))?
            .upsert(checkpoint_key, checkpoint.clone());
    }
    Ok(PreparedLocalReliableWriteTarget {
        final_path: final_path.to_path_buf(),
        partial_path,
        offset,
        file,
    })
}

/// 仅在 partial 大小吻合后执行同目录原子替换；失败时保留 partial 与原正式文件。
pub(in crate::services::sftp_service) async fn commit_local_reliable_write_target(
    final_path: &Path,
    partial_path: &Path,
    expected_bytes: u64,
) -> AppResult<()> {
    let actual_bytes = fs::metadata(partial_path).await?.len();
    match confirm_reliable_write_size(expected_bytes, actual_bytes) {
        ReliableSizeConfirmation::Verified => {}
        ReliableSizeConfirmation::Mismatch { expected, actual } => {
            return Err(AppError::Sftp(format!(
                "可靠下载 size 确认失败: expected {expected} bytes, got {actual} bytes"
            )));
        }
    }

    let partial = partial_path.to_path_buf();
    let final_target = final_path.to_path_buf();
    tokio::task::spawn_blocking(move || persist_partial_atomically(&partial, &final_target))
        .await
        .map_err(|error| AppError::Sftp(format!("原子提交任务失败: {error}")))??;
    Ok(())
}

/// Unix rename 在同一目录完成原子替换；partial 与正式文件始终处于同一目录。
#[cfg(not(windows))]
fn persist_partial_atomically(partial: &Path, final_target: &Path) -> std::io::Result<()> {
    std::fs::rename(partial, final_target)
}

/// Windows 对已存在文件使用 ReplaceFileW，避免先删正式文件产生不可恢复的空窗。
/// 暂时被索引器占用时仅短暂重试；失败保持 partial 供下次继续。
#[cfg(windows)]
fn persist_partial_atomically(partial: &Path, final_target: &Path) -> std::io::Result<()> {
    use std::{
        os::windows::ffi::OsStrExt,
        ptr, thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, ReplaceFileW, MOVEFILE_WRITE_THROUGH, REPLACEFILE_WRITE_THROUGH,
    };

    /// Win32 文件替换 API 要求 NUL 结尾 UTF-16 路径，不能使用有损字符串转换。
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    let source = wide(partial);
    let target = wide(final_target);
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let result = if final_target.exists() {
            // SAFETY: 两个 UTF-16 路径在调用期间有效，均以 NUL 结尾；可选指针为空。
            unsafe {
                ReplaceFileW(
                    target.as_ptr(),
                    source.as_ptr(),
                    ptr::null(),
                    REPLACEFILE_WRITE_THROUGH,
                    ptr::null(),
                    ptr::null(),
                )
            }
        } else {
            // SAFETY: 两个 UTF-16 路径在调用期间有效，均以 NUL 结尾。
            unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), MOVEFILE_WRITE_THROUGH) }
        };
        if result != 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if !partial.is_file()
            || !matches!(
                error.raw_os_error(),
                Some(5 | 32 | 33 | 80 | 183 | 1175 | 1176 | 1177)
            )
            || Instant::now() >= deadline
        {
            return Err(error);
        }
        thread::sleep(Duration::from_millis(20));
    }
}
