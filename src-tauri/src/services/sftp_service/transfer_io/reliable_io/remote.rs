//! remote reliable transfer helpers.
//! @author kongweiguang

use super::*;

/// 恢复使用已登记的实际路径；新任务的正式文件冲突策略不受历史临时文件影响。
pub(in crate::services::sftp_service) async fn prepare_remote_target_with_checkpoint(
    sftp: &SftpSession,
    remote_path: &str,
    conflict_policy: SftpTransferConflictPolicy,
    source_bytes: u64,
    source: Option<&RecoverySourceFingerprint>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedRemoteReliableWriteTarget>> {
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
            if remote_final_matches_checkpoint(sftp, &record).await? {
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
                && remote_path_exists(sftp, &actual_target).await?
            {
                return Err(AppError::Sftp(
                    "恢复目标已被其他文件占用，保留 partial".into(),
                ));
            }
            return prepare_selected_remote_reliable_write_target(
                sftp,
                &actual_target,
                conflict_policy,
                remote_path_exists(sftp, &record.actual_target).await?,
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
            prepare_selected_remote_reliable_write_target(
                sftp,
                remote_path,
                conflict_policy,
                remote_path_exists(sftp, remote_path).await?,
                source_bytes,
                source,
                None,
                checkpoints,
                checkpoint_key,
            )
            .await
        }
        SftpTransferConflictPolicy::Skip if remote_path_exists(sftp, remote_path).await? => {
            Ok(None)
        }
        SftpTransferConflictPolicy::Skip => {
            prepare_selected_remote_reliable_write_target(
                sftp,
                remote_path,
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
            for candidate in remote_conflict_candidates(remote_path).take(1000) {
                if remote_path_exists(sftp, &candidate).await? {
                    continue;
                }
                return prepare_selected_remote_reliable_write_target(
                    sftp,
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
                "无法为远程目标生成不冲突的文件名: {remote_path}"
            )))
        }
    }
}

/// 只有同任务 checkpoint 能提供可信偏移和临时路径；孤儿文件保留并隔离到新路径上传。
/// 全新文件用排他创建避免检查与 OPEN 之间覆盖其它任务，恢复仍须裁掉未确认尾部。
#[allow(clippy::too_many_arguments)]
async fn prepare_selected_remote_reliable_write_target(
    sftp: &SftpSession,
    final_path: &str,
    conflict_policy: SftpTransferConflictPolicy,
    final_exists: bool,
    source_bytes: u64,
    source: Option<&RecoverySourceFingerprint>,
    existing_checkpoint: Option<RecoveryCheckpointRecord>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedRemoteReliableWriteTarget>> {
    let mut partial_path = existing_checkpoint
        .as_ref()
        .map(|record| record.partial_path.clone())
        .unwrap_or_else(|| reliable_remote_partial_path(final_path));
    let mut partial_bytes = remote_partial_bytes(sftp, &partial_path).await?;
    if source.is_some() && existing_checkpoint.is_none() && partial_bytes.is_some() {
        partial_path = join_remote_path(
            &remote_parent_path(final_path),
            &format!(
                ".{}{RELIABLE_PARTIAL_SUFFIX}",
                uuid::Uuid::new_v4().simple()
            ),
        );
        // 固定长度的同目录名称不挤占长文件名上限；CREATE|EXCLUDE 原子占用，绝不复用历史长度。
        partial_bytes = None;
    }
    let checkpoint = existing_checkpoint.or_else(|| {
        (source.is_some() && partial_bytes.is_none()).then(|| RecoveryCheckpointRecord {
            actual_target: final_path.to_owned(),
            partial_path: partial_path.clone(),
            confirmed_offset: 0,
            source: source.cloned().expect("source is present"),
            committed: false,
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
            return Err(AppError::Sftp(format!(
                "无法恢复上传：临时文件“{partial_path}”缺少本任务的断点记录。请重新添加上传任务；现有文件已保留。"
            )));
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
            (_, _, Some(0)) if partial_bytes.is_none() => ReliableWriteDecision::Fresh,
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
            "无法为远程目标生成不冲突的文件名: {final_path}"
        ))),
        ReliableWriteDecision::Fresh | ReliableWriteDecision::RestartPartial => {
            open_remote_reliable_partial_target(
                sftp,
                final_path,
                partial_path,
                0,
                OpenFlags::CREATE
                    | OpenFlags::WRITE
                    | if source.is_some() && partial_bytes.is_none() {
                        OpenFlags::EXCLUDE
                    } else {
                        OpenFlags::TRUNCATE
                    },
                source,
                checkpoints,
                checkpoint_key,
            )
            .await
            .map(Some)
        }
        ReliableWriteDecision::Resume { offset } => open_remote_reliable_partial_target(
            sftp,
            final_path,
            partial_path,
            offset,
            OpenFlags::CREATE | OpenFlags::WRITE,
            source,
            checkpoints,
            checkpoint_key,
        )
        .await
        .map(Some),
        ReliableWriteDecision::CommitExistingPartial => open_remote_reliable_partial_target(
            sftp,
            final_path,
            partial_path,
            source_bytes,
            OpenFlags::CREATE | OpenFlags::WRITE,
            source,
            checkpoints,
            checkpoint_key,
        )
        .await
        .map(Some),
    }
}

/// 单独标识临时文件的 STAT，便于区分目录不可访问与旧断点文件不可访问。
async fn remote_partial_bytes(sftp: &SftpSession, partial_path: &str) -> AppResult<Option<u64>> {
    match sftp.metadata(partial_path.to_owned()).await {
        Ok(metadata) => Ok(metadata.size),
        Err(error) if is_no_such_file_error(&error) => Ok(None),
        Err(error) => Err(remote_write_error("检查上传临时文件（STAT）", error)),
    }
}

/// 首个单字节 WRITE 的失败回执已消耗且 partial 仍为 0 时，才允许同会话重开句柄。
/// 非零 partial、缺失 checkpoint 或已提交记录都不能被这一兼容重试截断。
pub(in crate::services::sftp_service) async fn retry_empty_remote_partial_after_first_write(
    sftp: &SftpSession,
    final_path: &str,
    partial_path: &str,
    source: &RecoverySourceFingerprint,
    checkpoints: &RecoveryCheckpoints,
    checkpoint_key: &str,
) -> AppResult<Option<PreparedRemoteReliableWriteTarget>> {
    let record = checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
        .snapshot(checkpoint_key);
    let Some(record) = record else {
        return Ok(None);
    };
    if !safe_first_write_retry_checkpoint(&record, final_path, partial_path, source)
        || remote_partial_bytes(sftp, partial_path).await? != Some(0)
    {
        return Ok(None);
    }
    open_remote_reliable_partial_target(
        sftp,
        final_path,
        partial_path.to_owned(),
        0,
        OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
        Some(source),
        Some(checkpoints),
        checkpoint_key,
    )
    .await
    .map(Some)
}

/// 重试门槛只接受同一源、目标且零确认的记录；先前的确认偏移或提交结果不能被覆盖。
pub(in crate::services::sftp_service) fn safe_first_write_retry_checkpoint(
    record: &RecoveryCheckpointRecord,
    final_path: &str,
    partial_path: &str,
    source: &RecoverySourceFingerprint,
) -> bool {
    !record.committed
        && record.confirmed_offset == 0
        && record.source.prefix_length == 0
        && record.source.prefix_sha256 == empty_prefix_digest()
        && record.actual_target == final_path
        && record.partial_path == partial_path
        && source_identity_matches(source, &record.source)
}

/// 两次零确认失败且旧写句柄已释放后，只清理本任务的空 partial，非空断点保留。
pub(in crate::services::sftp_service) async fn cleanup_empty_owned_remote_partial(
    sftp: &SftpSession,
    partial_path: &str,
) -> bool {
    if remote_partial_bytes(sftp, partial_path).await.ok() != Some(Some(0)) {
        return false;
    }
    sftp.remove_file(partial_path.to_owned()).await.is_ok()
}

/// 排他创建的新文件与已确认可截断的空文件都不依赖 FSETSTAT；恢复句柄才裁掉未确认尾部。
#[allow(clippy::too_many_arguments)]
async fn open_remote_reliable_partial_target(
    sftp: &SftpSession,
    final_path: &str,
    partial_path: String,
    offset: u64,
    flags: OpenFlags,
    source: Option<&RecoverySourceFingerprint>,
    checkpoints: Option<&RecoveryCheckpoints>,
    checkpoint_key: &str,
) -> AppResult<PreparedRemoteReliableWriteTarget> {
    let starts_empty = flags.intersects(OpenFlags::EXCLUDE | OpenFlags::TRUNCATE);
    let mut file = sftp
        .open_with_flags(partial_path.clone(), flags)
        .await
        .map_err(|error| {
            remote_write_error(
                if starts_empty {
                    "创建上传临时文件（OPEN）"
                } else {
                    "打开上传断点文件（OPEN）"
                },
                error,
            )
        })?;
    if !starts_empty {
        file.set_metadata(russh_sftp::protocol::FileAttributes {
            size: Some(offset),
            ..Default::default()
        })
        .await
        .map_err(|error| remote_write_error("裁掉未确认上传尾部（FSETSTAT）", error))?;
    }
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))
            .await
            .map_err(io_sftp_error)?;
    }
    let checkpoint = source.map(|source| RecoveryCheckpointRecord {
        actual_target: final_path.to_owned(),
        partial_path: partial_path.clone(),
        confirmed_offset: offset,
        source: source.clone(),
        committed: false,
    });
    if let (Some(checkpoint), Some(checkpoints)) = (&checkpoint, checkpoints) {
        checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("可靠传输 checkpoint 状态锁定失败".to_owned()))?
            .upsert(checkpoint_key, checkpoint.clone());
    }
    Ok(PreparedRemoteReliableWriteTarget {
        final_path: final_path.to_owned(),
        partial_path,
        offset,
        file,
    })
}

/// 大小确认后才改正式路径；服务器不能原子替换已存在目标时保留原文件和 partial。
pub(in crate::services::sftp_service) async fn commit_remote_reliable_write_target(
    sftp: &SftpSession,
    final_path: &str,
    partial_path: &str,
    expected_bytes: u64,
) -> AppResult<()> {
    let actual_bytes = sftp
        .metadata(partial_path.to_owned())
        .await
        .map_err(|error| remote_write_error("核对上传临时文件大小（STAT）", error))?
        .size
        .unwrap_or(0);
    match confirm_reliable_write_size(expected_bytes, actual_bytes) {
        ReliableSizeConfirmation::Verified => {}
        ReliableSizeConfirmation::Mismatch { expected, actual } => {
            return Err(AppError::Sftp(format!(
                "可靠上传 size 确认失败: expected {expected} bytes, got {actual} bytes"
            )));
        }
    }

    // 直接请求服务器执行 rename；支持同路径原子替换的实现可完成 overwrite。
    // 服务器拒绝已有目标时保持两侧文件原样，绝不先删除正式文件。
    match sftp
        .rename(partial_path.to_owned(), final_path.to_owned())
        .await
    {
        Ok(()) => Ok(()),
        Err(error) if remote_create_conflict_confirmed(sftp, final_path, &error, false).await => {
            // 不能 remove(final) 再 rename：网络中断发生在 remove 之后会丢失用户原文件。
            // 当前依赖没有公开 posix-rename 扩展入口，因此遇到已存在目标安全拒绝，保留 partial。
            Err(AppError::Sftp(
                "服务器不支持原子覆盖；原文件未改，partial 保留".to_owned(),
            ))
        }
        Err(error) => Err(remote_write_error("提交上传文件（RENAME）", error)),
    }
}
