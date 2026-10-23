//! SFTP upload data path.
//! @author kongweiguang

use super::*;

pub(in crate::services::sftp_service) async fn upload_directory(
    sftp: &SftpSession,
    local_path: &Path,
    remote_path: &str,
    progress: &TransferProgress,
    settings: SftpRuntimeSettings,
    conflict_policy: SftpTransferConflictPolicy,
) -> AppResult<()> {
    let total = calculate_local_directory_bytes(local_path).await?;
    progress.set_total_bytes(total);
    let Some(remote_root) = prepare_task_remote_directory_root(
        sftp,
        local_path,
        remote_path,
        conflict_policy,
        progress,
    )
    .await?
    else {
        progress.add_bytes(total);
        return Ok(());
    };
    let mut stack = vec![(local_path.to_path_buf(), remote_root)];
    while let Some((local_dir, remote_dir)) = stack.pop() {
        progress.ensure_not_cancelled()?;
        if let Err(error) = sftp.create_dir(remote_dir.clone()).await {
            if !remote_create_conflict_confirmed(sftp, &remote_dir, &error, true).await {
                return Err(native_sftp_error(error));
            }
        }
        let mut entries = fs::read_dir(&local_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            progress.ensure_not_cancelled()?;
            let metadata = entry.metadata().await?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let remote_child = join_remote_path(&remote_dir, &name);
            if metadata.is_dir() {
                stack.push((entry.path(), remote_child));
            } else if metadata.is_file() {
                upload_file(
                    sftp,
                    &entry.path(),
                    &remote_child,
                    progress,
                    settings,
                    conflict_policy,
                    false,
                )
                .await?;
            }
        }
    }
    Ok(())
}

/// 上传只在写入 ACK 后累计进度；提交前重读源，避免原文件传输途中改变。
pub(in crate::services::sftp_service) async fn upload_file(
    sftp: &SftpSession,
    local_path: &Path,
    remote_path: &str,
    progress: &TransferProgress,
    _settings: SftpRuntimeSettings,
    conflict_policy: SftpTransferConflictPolicy,
    set_total: bool,
) -> AppResult<()> {
    progress.ensure_not_cancelled()?;
    let metadata = fs::metadata(local_path).await?;
    let source_identity = local_source_fingerprint(local_path).await?;
    let checkpoint_key = recovery_key(&local_path.to_string_lossy(), remote_path);
    if set_total {
        progress.set_total_bytes(metadata.len());
    }
    let mut local_file = fs::File::open(local_path).await?;
    let Some(mut remote_target) = prepare_remote_reliable_write_target_for_source(
        sftp,
        remote_path,
        conflict_policy,
        &source_identity,
        Some(&progress.recovery_checkpoints()),
        &checkpoint_key,
    )
    .await?
    else {
        credit_skipped_or_committed(progress, &checkpoint_key, metadata.len(), set_total)?;
        return Ok(());
    };
    if remote_target.offset > 0 {
        local_file
            .seek(SeekFrom::Start(remote_target.offset))
            .await
            .map_err(io_sftp_error)?;
        // 同一任务自动恢复保留既有进度；人工后继任务由 registry 从可信 checkpoint 初始化。
        if set_total {
            progress.set_confirmed_bytes(remote_target.offset);
        }
    }
    let checkpoints = progress.recovery_checkpoints();
    let mut hasher = verify_resume(
        &mut local_file,
        sftp,
        &remote_target.partial_path,
        remote_target.offset,
        &checkpoints,
        &checkpoint_key,
        progress,
    )
    .await?;
    progress.mark_phase("transferring", Some(remote_target.final_path.clone()));
    let mut first_write_failed = false;
    let first_copy = copy_confirmed(
        &mut local_file,
        &mut remote_target.file,
        remote_target.offset,
        &mut hasher,
        &checkpoints,
        &checkpoint_key,
        progress,
        true,
        &mut first_write_failed,
    )
    .await;
    if let Err(first_error) = first_copy {
        if !first_write_failed || remote_target.offset != 0 || progress.is_cancelled() {
            return Err(first_error);
        }
        // 首个探测块只有 1 字节，flush 返回失败后没有乱序 ACK；旧句柄先释放再检查 partial。
        drop(remote_target.file);
        let Some(mut retried_target) = retry_empty_remote_partial_after_first_write(
            sftp,
            &remote_target.final_path,
            &remote_target.partial_path,
            &source_identity,
            &checkpoints,
            &checkpoint_key,
        )
        .await?
        else {
            return Err(first_error);
        };
        local_file
            .seek(SeekFrom::Start(0))
            .await
            .map_err(io_sftp_error)?;
        hasher = Sha256::new();
        let mut ignored_write_failure = false;
        if let Err(retry_error) = copy_confirmed(
            &mut local_file,
            &mut retried_target.file,
            0,
            &mut hasher,
            &checkpoints,
            &checkpoint_key,
            progress,
            true,
            &mut ignored_write_failure,
        )
        .await
        {
            drop(retried_target.file);
            if ignored_write_failure {
                let _ =
                    cleanup_empty_owned_remote_partial(sftp, &retried_target.partial_path).await;
            }
            return Err(retry_error);
        }
        remote_target = retried_target;
    }
    remote_target.file.shutdown().await.map_err(io_sftp_error)?;
    if !source_identity_matches(
        &source_identity,
        &local_source_fingerprint(local_path).await?,
    ) {
        return Err(AppError::Sftp(
            "提交前源文件身份已变化，保留 partial".into(),
        ));
    }
    let mut source_again = fs::File::open(local_path).await?;
    if sha256_hex_digest(&hash_prefix(&mut source_again, metadata.len(), progress).await?)
        != sha256_hex_digest(&hasher)
    {
        return Err(AppError::Sftp(
            "提交前源文件内容已变化，保留 partial".into(),
        ));
    }
    progress.ensure_not_cancelled()?;
    progress.mark_phase("committing", Some(remote_target.final_path.clone()));
    progress.note_network_wait("commitResponse");
    commit_remote_reliable_write_target(
        sftp,
        &remote_target.final_path,
        &remote_target.partial_path,
        metadata.len(),
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
