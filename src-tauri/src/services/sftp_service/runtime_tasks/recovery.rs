//! 传输恢复与最终结果收敛，统一约束普通任务和复合任务的重放边界。
//! @author kongweiguang

use std::{future::Future, time::Duration};
use tokio::time::sleep;

use super::*;

/// 单文件 checkpoint 可收敛确定的晚到错误；unknown 标记必须始终优先于旧 checkpoint。
pub(super) fn committed_transfer_error_is_complete(
    kind: SftpTransferKind,
    all_committed: bool,
    commit_outcome_uncertain: bool,
) -> bool {
    kind == SftpTransferKind::File && all_committed && !commit_outcome_uncertain
}

/// 执行一段受 watchdog 保护的 SFTP 工作，并对首次无进度故障做一次同 ID 重连恢复。
///
/// `operation` 每次调用都会重新创建底层连接 future；旧 future 在 watchdog 返回时已被
/// 丢弃，因此恢复不会继续复用已经不再产生确认的 SFTP stream。排队和恢复等待期间仍监听
/// 取消，最终失败只由这里写入，避免首次 idle 事件污染 UI 的终态通知。
pub(super) async fn run_managed_transfer_with_recovery<F, Fut>(
    progress: &TransferProgress,
    settings: SftpRuntimeSettings,
    limiter: Arc<TransferLimiter>,
    host_id: String,
    allow_recovery: bool,
    mut operation: F,
) -> AppResult<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AppResult<()>>,
{
    let mut recovery_attempt = 0_u8;
    loop {
        let permit = limiter
            .acquire(host_id.clone(), settings, progress.clone())
            .await?;
        progress.mark_running();
        progress.mark_phase("connecting", None);
        let result =
            run_with_idle_watchdog(progress, settings.idle_timeout_seconds, operation()).await;
        // watchdog 已丢弃旧 I/O；等待恢复前先释放槽，让其他排队任务获得执行机会。
        drop(permit);
        if result.is_ok() {
            return result;
        }

        let idle_timeout = progress.take_idle_timeout_pending();
        if should_retry_after_idle_timeout(
            idle_timeout,
            allow_recovery,
            progress.has_safe_recovery_checkpoint(),
            progress.commit_outcome_uncertain(),
            recovery_attempt,
            progress.is_cancelled(),
        ) {
            recovery_attempt = 1;
            progress.begin_recovery(recovery_attempt);
            if progress.is_cancelled() {
                return Err(AppError::Sftp("传输已取消".to_owned()));
            }
            tokio::select! {
                _ = sleep(Duration::from_secs(2)) => {}
                _ = progress.cancel_notified() => {
                    return Err(AppError::Sftp("传输已取消".to_owned()));
                }
            }
            if progress.is_cancelled() {
                return Err(AppError::Sftp("传输已取消".to_owned()));
            }
            progress.reset_after_recovery();
            continue;
        }

        if idle_timeout {
            progress.fail_idle_timeout(settings.idle_timeout_seconds);
        }
        return result;
    }
}

/// 未知提交结果优先于 partial checkpoint；重试可能覆盖已经安装的正式文件。
fn should_retry_after_idle_timeout(
    idle_timeout: bool,
    allow_recovery: bool,
    has_safe_checkpoint: bool,
    commit_outcome_uncertain: bool,
    recovery_attempt: u8,
    cancelled: bool,
) -> bool {
    idle_timeout
        && allow_recovery
        && has_safe_checkpoint
        && !commit_outcome_uncertain
        && recovery_attempt == 0
        && !cancelled
}

/// 无检查点的复合操作不允许自动重放，只把 watchdog 原因写为稳定终态。
pub(super) async fn run_transfer_with_recovery<F, Fut>(
    progress: &TransferProgress,
    idle_timeout_seconds: u64,
    mut operation: F,
) -> AppResult<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = AppResult<()>>,
{
    let result = run_with_idle_watchdog(progress, idle_timeout_seconds, operation()).await;
    if progress.take_idle_timeout_pending() {
        progress.fail_idle_timeout(idle_timeout_seconds);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{
        committed_transfer_error_is_complete, should_retry_after_idle_timeout, SftpTransferKind,
    };

    /// 仅确定的单文件完成记录可收敛晚到错误；目录子集和 unknown 状态都不能报成功。
    #[test]
    fn only_a_committed_file_can_turn_late_error_into_success() {
        assert!(committed_transfer_error_is_complete(
            SftpTransferKind::File,
            true,
            false,
        ));
        assert!(!committed_transfer_error_is_complete(
            SftpTransferKind::Directory,
            true,
            false,
        ));
        assert!(!committed_transfer_error_is_complete(
            SftpTransferKind::File,
            false,
            false,
        ));
        assert!(!committed_transfer_error_is_complete(
            SftpTransferKind::File,
            true,
            true,
        ));
    }

    /// Unknown commit blocks automatic recovery even when the old partial checkpoint is otherwise safe.
    #[test]
    fn uncertain_commit_disables_idle_recovery() {
        assert!(!should_retry_after_idle_timeout(
            true, true, true, true, 0, false,
        ));
        assert!(should_retry_after_idle_timeout(
            true, true, true, false, 0, false,
        ));
        assert!(!should_retry_after_idle_timeout(
            true, true, true, false, 1, false,
        ));
    }
}
