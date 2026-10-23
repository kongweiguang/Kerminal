//! SFTP v3 覆盖提交：先直接 rename，明确拒绝时以同目录备份兼容。
//! @author kongweiguang

use super::*;
use crate::services::sftp_service::TransferProgress;
use russh_sftp::{client::error::Error as SftpError, protocol::StatusCode};

pub(in crate::services::sftp_service) struct RemoteCommitContext<'a> {
    pub progress: &'a TransferProgress,
    pub checkpoints: &'a RecoveryCheckpoints,
    pub key: &'a str,
    pub expected_sha256: String,
    pub allow_overwrite: bool,
}

impl RemoteCommitContext<'_> {
    /// 在每次可能改变正式路径的请求前登记恢复路径，监督器丢弃 future 仍能给出人工核对依据。
    fn remember(
        &self,
        final_path: &str,
        partial_path: &str,
        backup: Option<&str>,
    ) -> AppResult<()> {
        self.checkpoints
            .lock()
            .map_err(|_| AppError::Sftp("checkpoint 锁定失败".into()))?
            .remember_commit_attempt(
                self.key,
                backup.map(str::to_owned),
                self.expected_sha256.clone(),
            );
        self.progress.set_commit_recovery_detail(format!(
            "目标: {final_path}\npartial: {partial_path}{}",
            backup
                .map(|path| format!("\nbackup: {path}"))
                .unwrap_or_default()
        ));
        Ok(())
    }

    /// 仅在服务端明确完成提交或回滚后清理 journal；未知结果必须留下恢复信息。
    fn resolved(&self) {
        if let Ok(mut state) = self.checkpoints.lock() {
            state.clear_commit_backup(self.key);
        }
        self.progress.clear_commit_recovery_detail();
    }

    /// 网络失败不能解释为 rename 未发生；停用自动/人工重试，避免覆盖尚待核对的结果。
    fn unknown(&self, message: impl Into<String>) -> AppError {
        self.progress.note_commit_outcome_uncertain();
        AppError::Sftp(message.into())
    }
}

/// 连接状态和坏回执无法证明服务器拒绝了写操作，其余错误 STATUS 才能用于安全回滚。
fn definite_rejection(error: &SftpError) -> bool {
    matches!(error, SftpError::Status(status) if !matches!(status.status_code,
        StatusCode::Ok | StatusCode::NoConnection | StatusCode::ConnectionLost | StatusCode::BadMessage))
}

/// LSTAT 避免跟随符号链接；只有明确不存在才能把路径用于备份或无覆盖回滚。
async fn entry_metadata(
    sftp: &SftpSession,
    path: &str,
) -> Result<Option<russh_sftp::client::fs::Metadata>, SftpError> {
    match sftp.symlink_metadata(path.to_owned()).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if is_no_such_file_error(&error) => Ok(None),
        Err(error) => Err(error),
    }
}

/// SFTP v3 允许省略权限或仅返回读写位；缺少类型位不能当成特殊文件，已知目录/链接仍须拒绝。
fn is_known_non_regular(metadata: &russh_sftp::client::fs::Metadata) -> bool {
    metadata
        .permissions
        .is_some_and(|mode| mode & 0o170000 != 0)
        && !metadata.file_type().is_file()
}

/// UUID 名称不依赖原文件名长度；同时确认未占用，避免误用历史备份。
async fn allocate_backup(sftp: &SftpSession, final_path: &str) -> AppResult<String> {
    for _ in 0..8 {
        let path = join_remote_path(
            &remote_parent_path(final_path),
            &format!(".{}.kerminal-backup", uuid::Uuid::new_v4().simple()),
        );
        if entry_metadata(sftp, &path)
            .await
            .map_err(|error| remote_write_error("检查备份路径（LSTAT）", error))?
            .is_none()
        {
            return Ok(path);
        }
    }
    Err(AppError::Sftp(
        "无法分配不冲突的上传备份路径；原文件与 partial 保留".into(),
    ))
}

/// 核对任务持有的 partial 后优先直接提交；缺省类型属性兼容直接 rename，备份事务仍要求普通文件证明。
pub(in crate::services::sftp_service) async fn commit_remote_reliable_write_target(
    sftp: &SftpSession,
    final_path: &str,
    partial_path: &str,
    expected_bytes: u64,
    context: RemoteCommitContext<'_>,
) -> AppResult<()> {
    let partial = entry_metadata(sftp, partial_path)
        .await
        .map_err(|error| remote_write_error("核对上传临时文件（LSTAT）", error))?
        .ok_or_else(|| AppError::Sftp("上传 partial 已丢失，正式文件未改".into()))?;
    if is_known_non_regular(&partial) {
        return Err(AppError::Sftp(
            "上传临时路径不是普通文件；正式文件未改，partial 保留".into(),
        ));
    }
    if partial.size != Some(expected_bytes) {
        return Err(AppError::Sftp(format!(
            "可靠上传 size 确认失败: expected {expected_bytes} bytes, got {:?}; partial 保留",
            partial.size
        )));
    }
    // 普通 rename 也可能替换符号链接；提交前再次核对类型和非覆盖策略，缩小外部写入竞态窗口。
    if let Some(metadata) = entry_metadata(sftp, final_path)
        .await
        .map_err(|error| remote_write_error("检查上传目标（LSTAT）", error))?
    {
        if is_known_non_regular(&metadata) {
            return Err(AppError::Sftp(
                "上传目标不是普通文件；原目标与 partial 保留".into(),
            ));
        }
        if !context.allow_overwrite {
            return Err(AppError::Sftp(
                "上传目标已被占用；原文件与 partial 保留，请重新选择冲突策略".into(),
            ));
        }
    }
    context.remember(final_path, partial_path, None)?;
    let error = match sftp
        .rename(partial_path.to_owned(), final_path.to_owned())
        .await
    {
        Ok(()) => {
            context.resolved();
            return Ok(());
        }
        Err(error) if !definite_rejection(&error) => {
            return Err(context.unknown(format!("提交上传文件回执未确认: {error}")))
        }
        Err(error) => error,
    };
    let supports_fallback = matches!(&error, SftpError::Status(status) if matches!(status.status_code, StatusCode::Failure | StatusCode::OpUnsupported));
    if !context.allow_overwrite || !supports_fallback {
        context.resolved();
        return Err(remote_write_error("提交上传文件（RENAME）", error));
    }
    let original = entry_metadata(sftp, final_path)
        .await
        .map_err(|error| remote_write_error("检查覆盖目标（LSTAT）", error))?;
    if !original
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_file())
    {
        context.resolved();
        return Err(AppError::Sftp(format!(
            "服务器拒绝提交；无法确认目标为可兼容覆盖的普通文件，原目标与 partial 保留: {error}"
        )));
    }
    let backup = allocate_backup(sftp, final_path).await?;
    context.remember(final_path, partial_path, Some(&backup))?;
    if let Err(error) = sftp.rename(final_path.to_owned(), backup.clone()).await {
        if !definite_rejection(&error) {
            return Err(context.unknown(format!("备份原文件回执未确认: {error}")));
        }
        context.resolved();
        return Err(remote_write_error(
            "备份原文件（RENAME）；原文件与 partial 保留",
            error,
        ));
    }
    // 备份后目标应不存在；发现外部进程抢占路径时保留备份，避免继续覆盖它。
    match entry_metadata(sftp, final_path).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(context.unknown("备份后目标被其他文件占用；backup 与 partial 保留"))
        }
        Err(error) => return Err(context.unknown(format!("备份后无法确认目标路径: {error}"))),
    }
    match sftp
        .rename(partial_path.to_owned(), final_path.to_owned())
        .await
    {
        Ok(()) => {
            // 新文件已被明确提交；备份清理失败只留下可恢复副本，不能把成功变成可重试上传。
            context
                .checkpoints
                .lock()
                .map_err(|_| context.unknown("上传已提交，但 checkpoint 锁定失败"))?
                .mark_committed(context.key);
            match tokio::time::timeout(
                std::time::Duration::from_millis(500),
                sftp.remove_file(backup.clone()),
            )
            .await
            {
                Ok(Ok(())) => {}
                result => tauri_plugin_log::log::warn!(
                    "上传已提交，备份清理未确认；保留路径 {backup}: {result:?}"
                ),
            }
            context.resolved();
            Ok(())
        }
        Err(error) if !definite_rejection(&error) => Err(context.unknown(format!(
            "替换上传文件回执未确认，backup 与 partial 保留: {error}"
        ))),
        Err(error) => rollback(sftp, final_path, &backup, &context, error).await,
    }
}

/// 仅在替换明确失败且目标仍空闲时回滚，任何不确定性都保留三条路径而非猜测成功。
async fn rollback(
    sftp: &SftpSession,
    final_path: &str,
    backup: &str,
    context: &RemoteCommitContext<'_>,
    commit_error: SftpError,
) -> AppResult<()> {
    match entry_metadata(sftp, final_path).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(context.unknown(format!(
                "替换失败且目标已被占用，不能安全回滚；backup 与 partial 保留: {commit_error}"
            )))
        }
        Err(error) => return Err(context.unknown(format!("替换失败，无法确认回滚目标: {error}"))),
    }
    match sftp.rename(backup.to_owned(), final_path.to_owned()).await {
        Ok(()) => {
            context.resolved();
            context.progress.note_commit_rollback_confirmed();
            Err(AppError::Sftp(format!(
                "替换上传文件失败，原文件已恢复；partial 保留，可重试: {commit_error}"
            )))
        }
        Err(error) => Err(context.unknown(format!(
            "替换失败且原文件回滚未完成；backup 与 partial 保留: {error}"
        ))),
    }
}
