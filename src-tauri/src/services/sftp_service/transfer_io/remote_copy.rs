//! SFTP remote copy data path.
//! @author kongweiguang

use super::*;

pub(in crate::services::sftp_service) async fn copy_remote_directory_between_sessions(
    source_sftp: &SftpSession,
    source_remote_path: &str,
    target_sftp: &SftpSession,
    target_remote_path: &str,
    progress: &TransferProgress,
    settings: SftpRuntimeSettings,
    conflict_policy: SftpTransferConflictPolicy,
) -> AppResult<()> {
    let Some(target_root) =
        prepare_remote_directory_root(target_sftp, target_remote_path, conflict_policy).await?
    else {
        return Ok(());
    };
    let mut stack = vec![(source_remote_path.to_owned(), target_root)];
    while let Some((source_dir, target_dir)) = stack.pop() {
        progress.ensure_not_cancelled()?;
        ensure_remote_directory(target_sftp, &target_dir).await?;
        let entries = source_sftp
            .read_dir(source_dir.clone())
            .await
            .map_err(native_sftp_error)?;
        for entry in entries {
            progress.ensure_not_cancelled()?;
            let name = entry.file_name();
            let source_child = entry.path();
            let target_child = join_remote_path(&target_dir, &name);
            match entry.file_type() {
                FileType::Dir => stack.push((source_child, target_child)),
                FileType::File | FileType::Symlink => {
                    if let Some(size) = entry.metadata().size {
                        progress.add_total_bytes(size);
                    }
                    copy_remote_file_between_sessions(
                        source_sftp,
                        &source_child,
                        target_sftp,
                        &target_child,
                        progress,
                        settings,
                        conflict_policy,
                        false,
                    )
                    .await?;
                }
                FileType::Other => {}
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
/// 跨主机复制也按确认前缀推进，源变化或目标校验失败都不发布正式路径。
pub(in crate::services::sftp_service) async fn copy_remote_file_between_sessions(
    source_sftp: &SftpSession,
    source_remote_path: &str,
    target_sftp: &SftpSession,
    target_remote_path: &str,
    progress: &TransferProgress,
    settings: SftpRuntimeSettings,
    conflict_policy: SftpTransferConflictPolicy,
    set_total: bool,
) -> AppResult<()> {
    progress.ensure_not_cancelled()?;
    if set_total {
        if let Some(directory_path) =
            resolve_file_request_directory(source_sftp, source_remote_path).await
        {
            return Box::pin(copy_remote_directory_between_sessions(
                source_sftp,
                &directory_path,
                target_sftp,
                target_remote_path,
                progress,
                settings,
                conflict_policy,
            ))
            .await;
        }
    }
    let mut source_file = match source_sftp.open(source_remote_path).await {
        Ok(source_file) => source_file,
        Err(open_error) => {
            let Some(fallback) =
                resolve_remote_read_fallback(source_sftp, source_remote_path).await
            else {
                return Err(native_sftp_error(open_error));
            };
            match fallback {
                RemoteReadFallback::Directory(directory_path) => {
                    return Box::pin(copy_remote_directory_between_sessions(
                        source_sftp,
                        &directory_path,
                        target_sftp,
                        target_remote_path,
                        progress,
                        settings,
                        conflict_policy,
                    ))
                    .await;
                }
                RemoteReadFallback::File(file_path) => source_sftp
                    .open(file_path)
                    .await
                    .map_err(native_sftp_error)?,
            }
        }
    };
    if set_total {
        if let Ok(metadata) = source_file.metadata().await {
            progress.set_total_bytes(metadata.size.unwrap_or(0));
        }
    }
    let source_metadata = source_file.metadata().await.map_err(native_sftp_error)?;
    let source_size = source_metadata.size.unwrap_or(0);
    let source_identity = remote_source_fingerprint(&mut source_file, &source_metadata).await?;
    let checkpoint_key = recovery_key(source_remote_path, target_remote_path);
    let Some(mut target) = prepare_remote_reliable_write_target_for_source(
        target_sftp,
        target_remote_path,
        conflict_policy,
        &source_identity,
        Some(&progress.recovery_checkpoints()),
        &checkpoint_key,
    )
    .await?
    else {
        credit_skipped_or_committed(progress, &checkpoint_key, source_size, set_total)?;
        return Ok(());
    };
    if target.offset > 0 {
        source_file
            .seek(SeekFrom::Start(target.offset))
            .await
            .map_err(io_sftp_error)?;
        // 远端复制任务可能含多个文件，旧偏移已计入任务汇总。
    }
    let checkpoints = progress.recovery_checkpoints();
    let mut hasher = verify_resume(
        &mut source_file,
        target_sftp,
        &target.partial_path,
        target.offset,
        &checkpoints,
        &checkpoint_key,
        progress,
    )
    .await?;
    progress.mark_phase("transferring", Some(target.final_path.clone()));
    copy_confirmed(
        &mut source_file,
        &mut target.file,
        target.offset,
        &mut hasher,
        &checkpoints,
        &checkpoint_key,
        progress,
        false,
        &mut false,
    )
    .await?;
    target.file.shutdown().await.map_err(io_sftp_error)?;
    let final_metadata = source_file.metadata().await.map_err(native_sftp_error)?;
    if final_metadata.size != source_metadata.size || final_metadata.mtime != source_metadata.mtime
    {
        return Err(AppError::Sftp(
            "提交前远端源文件身份已变化，保留 partial".into(),
        ));
    }
    source_file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(io_sftp_error)?;
    if sha256_hex_digest(&hash_prefix(&mut source_file, source_size, progress).await?)
        != sha256_hex_digest(&hasher)
    {
        return Err(AppError::Sftp(
            "提交前远端源文件内容已变化，保留 partial".into(),
        ));
    }
    source_file.shutdown().await.map_err(io_sftp_error)?;
    progress.ensure_not_cancelled()?;
    progress.mark_phase("committing", Some(target.final_path.clone()));
    progress.note_network_wait("commitResponse");
    commit_remote_reliable_write_target(
        target_sftp,
        &target.final_path,
        &target.partial_path,
        source_size,
        RemoteCommitContext {
            progress,
            checkpoints: &checkpoints,
            key: &checkpoint_key,
            expected_sha256: sha256_hex_digest(&hasher),
            allow_overwrite: conflict_policy == SftpTransferConflictPolicy::Overwrite,
        },
    )
    .await?;
    progress.clear_network_wait();
    checkpoints
        .lock()
        .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
        .mark_committed(&checkpoint_key);
    Ok(())
}
