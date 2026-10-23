//! Transfer progress lifecycle and recovery state transitions.
//! @author kongweiguang

use super::*;
use crate::services::sftp_service::{transfer_io, transfer_registry};

const COMMIT_UNKNOWN_ERROR: &str = "提交结果未确认，请核对目标文件";

impl TransferProgress {
    /// 同步调用复用进度 API，但不注册队列摘要或窗口事件。
    pub(in crate::services::sftp_service) fn detached() -> Self {
        Self {
            transfer_id: None,
            transfers: None,
            cancel_requested: Arc::new(AtomicBool::new(false)),
            cancel_notify: Arc::new(Notify::new()),
            recovery: transfer_io::new_recovery_checkpoints(),
            idle_timeout_pending: Arc::new(AtomicBool::new(false)),
            commit_outcome_uncertain: Arc::new(AtomicBool::new(false)),
            commit_rollback_confirmed: Arc::new(AtomicBool::new(false)),
            commit_recovery_detail: Arc::new(Mutex::new(None)),
            last_activity_at_ms: Arc::new(AtomicU64::new(unix_timestamp_millis())),
            diagnostic_operation: Arc::new(Mutex::new("queued")),
            event_emitter: None,
        }
    }

    /// 每个队列任务共享取消、断点和诊断状态，自动恢复沿用同一实例与任务 ID。
    pub(in crate::services::sftp_service) fn tracked(
        transfer_id: String,
        transfers: Arc<Mutex<HashMap<String, TransferTask>>>,
        cancel_requested: Arc<AtomicBool>,
        cancel_notify: Arc<Notify>,
        recovery: RecoveryCheckpointHolder,
        event_emitter: Option<TransferEventEmitter>,
    ) -> Self {
        Self {
            transfer_id: Some(transfer_id),
            transfers: Some(transfers),
            cancel_requested,
            cancel_notify,
            recovery,
            idle_timeout_pending: Arc::new(AtomicBool::new(false)),
            commit_outcome_uncertain: Arc::new(AtomicBool::new(false)),
            commit_rollback_confirmed: Arc::new(AtomicBool::new(false)),
            commit_recovery_detail: Arc::new(Mutex::new(None)),
            last_activity_at_ms: Arc::new(AtomicU64::new(unix_timestamp_millis())),
            diagnostic_operation: Arc::new(Mutex::new("queued")),
            event_emitter,
        }
    }

    pub(in crate::services::sftp_service) fn is_cancelled(&self) -> bool {
        self.cancel_requested.load(Ordering::SeqCst)
    }

    /// 原子位与有记忆的单消费者通知配合，消除发出取消后才注册 waiter 的丢唤醒窗口。
    /// 每个传输阶段同一时间只有一个监督器等待取消；其余同步边界读取原子位。
    pub(in crate::services::sftp_service) async fn cancel_notified(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            self.cancel_notify.notified().await;
        }
    }

    pub(in crate::services::sftp_service) fn ensure_not_cancelled(&self) -> AppResult<()> {
        if self.is_cancelled() {
            return Err(AppError::Sftp("传输已取消".to_owned()));
        }
        Ok(())
    }

    /// 为本地中转子步骤创建共享取消与活动时钟的轻量进度句柄。
    ///
    /// 中转下载不应重复累计到用户可见总进度，但它确实在传输字节；共享时钟避免长文件在
    /// 临时落盘阶段被误判为无进度。
    pub(in crate::services::sftp_service) fn detached_child(&self) -> Self {
        Self {
            transfer_id: None,
            transfers: None,
            cancel_requested: self.cancel_requested.clone(),
            cancel_notify: self.cancel_notify.clone(),
            recovery: self.recovery.clone(),
            idle_timeout_pending: self.idle_timeout_pending.clone(),
            commit_outcome_uncertain: self.commit_outcome_uncertain.clone(),
            commit_rollback_confirmed: self.commit_rollback_confirmed.clone(),
            commit_recovery_detail: self.commit_recovery_detail.clone(),
            last_activity_at_ms: self.last_activity_at_ms.clone(),
            diagnostic_operation: self.diagnostic_operation.clone(),
            event_emitter: None,
        }
    }

    /// 在连接、传输、提交等实际工作边界重置活动时间。
    ///
    /// 排队阶段不会调用此方法，因此等待并发槽不会消耗无进度预算；只有已获取槽位的任务
    /// 才开始被 watchdog 观察。
    pub(in crate::services::sftp_service) fn refresh_activity(&self) {
        self.last_activity_at_ms
            .store(unix_timestamp_millis(), Ordering::SeqCst);
    }

    /// I/O 只记录脱敏操作名；库内 pending request 数不可得，诊断明确记为 unavailable。
    pub(in crate::services::sftp_service) fn note_network_wait(&self, operation: &'static str) {
        if let Ok(mut current) = self.diagnostic_operation.lock() {
            *current = operation;
        }
    }

    /// 收到确认后清除等待点，避免下次连接或提交停滞沿用旧写入诊断。
    pub(in crate::services::sftp_service) fn clear_network_wait(&self) {
        if let Ok(mut current) = self.diagnostic_operation.lock() {
            *current = "active";
        }
    }

    /// 单行脱敏记录用于区分连接、写确认、读响应和提交停滞；不包含目标路径或凭据。
    pub(in crate::services::sftp_service) fn log_stall_diagnostic(&self, reason: &'static str) {
        let operation = self
            .diagnostic_operation
            .lock()
            .map(|value| *value)
            .unwrap_or("unavailable");
        let (last_confirmed_at, recovery_attempt) = self
            .with_summary(|summary| (summary.last_progress_at, summary.recovery_attempt))
            .unwrap_or((None, 0));
        let transfer_id = self.transfer_id.as_deref().unwrap_or("detached");
        let cancel_observed_at = (reason == "cancelRequested").then(unix_timestamp_millis);
        tauri_plugin_log::log::warn!(
            "sftp transfer stall transferId={transfer_id} reason={reason} operation={operation} lastConfirmedAt={last_confirmed_at:?} pendingRequests=unavailable cancelObservedAt={cancel_observed_at:?} recoveryAttempt={recovery_attempt}"
        );
    }

    /// 判断后台任务是否已连续超过阈值而没有字节或阶段推进。
    pub(in crate::services::sftp_service) fn idle_timeout_elapsed(
        &self,
        idle_timeout_seconds: u64,
        now_ms: u64,
    ) -> bool {
        idle_timeout_elapsed(
            self.last_activity_at_ms.load(Ordering::SeqCst),
            idle_timeout_seconds,
            now_ms,
        )
    }

    /// 返回任务是否已由 watchdog 写入稳定的无进度失败状态。
    pub(in crate::services::sftp_service) fn failed_with_idle_timeout(&self) -> bool {
        self.with_summary(|summary| {
            summary.failure_kind == Some(crate::models::sftp::SftpTransferFailureKind::IdleTimeout)
        })
        .unwrap_or(false)
    }

    /// 读取并清除本次 watchdog 事件；首次事件只允许 supervisor 进入恢复，不写终态。
    pub(in crate::services::sftp_service) fn take_idle_timeout_pending(&self) -> bool {
        self.idle_timeout_pending.swap(false, Ordering::SeqCst)
    }

    /// 把连接/传输重新建立前的短暂阶段投影为可观察的恢复中状态。
    pub(in crate::services::sftp_service) fn begin_recovery(&self, attempt: u8) {
        self.refresh_activity();
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Running;
            summary.phase = Some("recovering".to_owned());
            summary.recovery_attempt = attempt;
            summary.retryable = matches!(
                summary.operation,
                crate::models::sftp::SftpTransferOperation::Upload
                    | crate::models::sftp::SftpTransferOperation::Download
            ) && summary.kind == crate::models::sftp::SftpTransferKind::File;
            summary.speed_bytes_per_second = 0;
            summary.updated_at = unix_timestamp();
        });
    }

    /// 让传输重新从连接阶段计时，同时保留已确认 checkpoint 和同一个任务 ID。
    pub(in crate::services::sftp_service) fn reset_after_recovery(&self) {
        self.refresh_activity();
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Running;
            summary.phase = Some("connecting".to_owned());
            summary.speed_bytes_per_second = 0;
            summary.updated_at = unix_timestamp();
        });
    }

    /// 仅记录无进度故障；恢复资格由 supervisor 在关闭旧连接后判断，避免首次事件
    /// 提前投影成 failed 或 recovering，造成界面闪烁与错误通知。
    pub(in crate::services::sftp_service) fn note_idle_timeout(&self, _idle_timeout_seconds: u64) {
        self.idle_timeout_pending.store(true, Ordering::SeqCst);
    }

    /// 自动恢复只接受可靠写入层登记过源身份的断点；未知 partial 不能凭文件长度猜偏移。
    pub(in crate::services::sftp_service) fn has_safe_recovery_checkpoint(&self) -> bool {
        self.recovery
            .lock()
            .map(|state| state.has_safe_recovery_checkpoint())
            .unwrap_or(false)
    }

    /// 提交成功可早于取消回调到达；以可靠写入层的提交记录决定最终成功状态。
    pub(in crate::services::sftp_service) fn all_committed(&self) -> bool {
        self.recovery
            .lock()
            .map(|state| state.all_committed())
            .unwrap_or(false)
    }

    /// 远端 rename 可能已执行但回执丢失；这个位只用于阻止把未知结果谎称“已取消”。
    pub(in crate::services::sftp_service) fn commit_outcome_uncertain(&self) -> bool {
        self.commit_outcome_uncertain.load(Ordering::SeqCst)
    }

    /// 事务 helper 可能在 future 被丢弃后仍需保留路径，用共享字段向最终任务状态回传。
    pub(in crate::services::sftp_service) fn set_commit_recovery_detail(
        &self,
        detail: impl Into<String>,
    ) {
        let detail = detail.into();
        if detail.trim().is_empty() {
            return;
        }
        if let Ok(mut current) = self.commit_recovery_detail.lock() {
            *current = Some(detail);
        }
    }

    /// 提交已确认或回滚已确认后清除临时路径，避免终态展示已失效的恢复指引。
    pub(in crate::services::sftp_service) fn clear_commit_recovery_detail(&self) {
        if let Ok(mut current) = self.commit_recovery_detail.lock() {
            *current = None;
        }
    }

    /// 事务无法唯一确认或取消确认窗口超时时置位，阻止自动重传覆盖未知目标状态。
    pub(in crate::services::sftp_service) fn note_commit_outcome_uncertain(&self) {
        self.commit_outcome_uncertain.store(true, Ordering::SeqCst);
    }

    /// 只有后端收到完整回滚确认后才能开放单文件上传/下载的人工重试。
    pub(in crate::services::sftp_service) fn note_commit_rollback_confirmed(&self) {
        self.commit_rollback_confirmed.store(true, Ordering::SeqCst);
    }

    /// 未知提交结果禁止自动重传，并保留 helper 错误及预登记路径供用户核对/恢复。
    pub(in crate::services::sftp_service) fn fail_commit_outcome_uncertain(
        &self,
        error: impl Into<String>,
    ) {
        let error = error.into();
        let recovery_detail = self
            .commit_recovery_detail
            .lock()
            .ok()
            .and_then(|detail| detail.clone());
        let error = commit_unknown_error_message(&error, recovery_detail.as_deref());
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Failed;
            summary.failure_kind =
                Some(crate::models::sftp::SftpTransferFailureKind::CommitUnknown);
            summary.error = Some(error);
            summary.phase = Some("failed".to_owned());
            summary.retryable = false;
            summary.resumable = false;
            summary.speed_bytes_per_second = 0;
            summary.updated_at = unix_timestamp();
        });
    }

    /// 返回共享 checkpoint holder，供可靠 I/O 在重连后继续使用确认偏移。
    pub(in crate::services::sftp_service) fn recovery_checkpoints(
        &self,
    ) -> RecoveryCheckpointHolder {
        self.recovery.clone()
    }

    pub(in crate::services::sftp_service) fn mark_running(&self) {
        self.refresh_activity();
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Running;
            summary.phase = Some("running".to_owned());
            summary.updated_at = unix_timestamp();
        });
    }

    /// 阶段变更重置活动时间；连接等待的诊断名与真实状态同步，避免沿用上轮 ACK 位置。
    pub(in crate::services::sftp_service) fn mark_phase(
        &self,
        phase: impl Into<String>,
        current_item: Option<String>,
    ) {
        self.refresh_activity();
        let phase = phase.into();
        if phase == "connecting" {
            self.note_network_wait("connecting");
        } else {
            self.clear_network_wait();
        }
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Running;
            summary.phase = Some(phase);
            summary.current_item = current_item;
            summary.updated_at = unix_timestamp();
        });
    }

    pub(in crate::services::sftp_service) fn set_total_bytes(&self, total_bytes: u64) {
        self.update_summary(true, |summary| {
            summary.total_bytes = Some(total_bytes);
            summary.updated_at = unix_timestamp();
        });
    }

    pub(in crate::services::sftp_service) fn add_total_bytes(&self, bytes: u64) {
        self.update_summary(false, |summary| {
            summary.total_bytes = Some(summary.total_bytes.unwrap_or(0).saturating_add(bytes));
            summary.updated_at = unix_timestamp();
        });
    }

    /// 进度只在 I/O 已确认字节后增加，并同步投影可信断点状态；跳过文件不会伪称可续传。
    pub(in crate::services::sftp_service) fn add_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.refresh_activity();
        let now_ms = unix_timestamp_millis();
        let resumable = self
            .recovery
            .lock()
            .map(|state| state.has_resumable())
            .unwrap_or(false);
        self.update_task(false, |task| {
            task.summary.bytes_transferred = task.summary.bytes_transferred.saturating_add(bytes);
            task.summary.speed_bytes_per_second =
                task.speed.update(task.summary.bytes_transferred, now_ms);
            task.summary.last_progress_at = Some(now_ms);
            task.summary.resumable = resumable;
            task.summary.updated_at = unix_timestamp();
        });
    }

    /// 用可靠 I/O 给出的绝对确认偏移更新 UI 进度，恢复连接后不会重复累加旧偏移。
    pub(in crate::services::sftp_service) fn set_confirmed_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.refresh_activity();
        let now_ms = unix_timestamp_millis();
        let resumable = self
            .recovery
            .lock()
            .map(|state| state.has_resumable())
            .unwrap_or(false);
        self.update_task(false, |task| {
            if bytes > task.summary.bytes_transferred {
                task.summary.bytes_transferred = bytes;
                task.summary.last_progress_at = Some(now_ms);
                task.summary.resumable = resumable;
                task.summary.speed_bytes_per_second =
                    task.speed.update(task.summary.bytes_transferred, now_ms);
            }
            task.summary.updated_at = unix_timestamp();
        });
    }

    /// 提交确认后固化成功终态并撤掉恢复入口；迟到取消因终态门禁不会覆盖。
    pub(in crate::services::sftp_service) fn succeed(&self) {
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Succeeded;
            summary.error = None;
            summary.failure_kind = None;
            summary.phase = Some("done".to_owned());
            summary.current_item = None;
            summary.speed_bytes_per_second = 0;
            summary.retryable = false;
            summary.resumable = false;
            summary.updated_at = unix_timestamp();
        });
    }

    /// 取消保留 partial 与断点，并只对单文件任务开放服务端参数继承的人工重试。
    pub(in crate::services::sftp_service) fn cancel(&self) {
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Canceled;
            summary.cancel_requested = true;
            summary.failure_kind = None;
            summary.phase = Some("canceled".to_owned());
            summary.current_item = None;
            summary.speed_bytes_per_second = 0;
            summary.retryable = matches!(
                summary.operation,
                crate::models::sftp::SftpTransferOperation::Upload
                    | crate::models::sftp::SftpTransferOperation::Download
            ) && summary.kind == crate::models::sftp::SftpTransferKind::File;
            summary.updated_at = unix_timestamp();
        });
    }

    /// 将普通错误映射为终态；只有 watchdog 留下的明确 idle 标志才使用脱敏错误。
    pub(in crate::services::sftp_service) fn fail(&self, error: impl Into<String>) {
        if self.take_idle_timeout_pending() {
            self.fail_idle_timeout(u64::from(
                self.with_summary(|summary| summary.idle_timeout_seconds)
                    .unwrap_or(180),
            ));
            return;
        }
        let rollback_confirmed = self.commit_rollback_confirmed.load(Ordering::SeqCst);
        let commit_outcome_uncertain = self.commit_outcome_uncertain();
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Failed;
            summary.error = Some(error.into());
            summary.failure_kind = Some(crate::models::sftp::SftpTransferFailureKind::Other);
            summary.phase = Some("failed".to_owned());
            summary.speed_bytes_per_second = 0;
            summary.retryable = is_retryable_after_commit_rollback(
                rollback_confirmed,
                commit_outcome_uncertain,
                summary.operation,
                summary.kind,
            );
            summary.updated_at = unix_timestamp();
        });
    }

    /// 以不含路径、凭据或底层库文本的结构化语义结束无进度任务。
    ///
    /// partial 文件故意不在这里清理：可靠写入层仅在空 partial 或成功原子提交后清理，
    /// 这样用户点击继续传输时可以从确认偏移量恢复。
    pub(in crate::services::sftp_service) fn fail_idle_timeout(&self, idle_timeout_seconds: u64) {
        self.update_summary(true, |summary| {
            summary.status = SftpTransferStatus::Failed;
            summary.error = Some(idle_timeout_message(idle_timeout_seconds));
            summary.failure_kind = Some(crate::models::sftp::SftpTransferFailureKind::IdleTimeout);
            summary.phase = Some("failed".to_owned());
            summary.current_item = None;
            summary.speed_bytes_per_second = 0;
            summary.retryable = matches!(
                summary.operation,
                crate::models::sftp::SftpTransferOperation::Upload
                    | crate::models::sftp::SftpTransferOperation::Download
            ) && summary.kind == crate::models::sftp::SftpTransferKind::File;
            summary.updated_at = unix_timestamp();
        });
    }

    fn update_summary(&self, force_event: bool, update: impl FnOnce(&mut SftpTransferSummary)) {
        self.update_task(force_event, |task| update(&mut task.summary));
    }

    fn update_task(&self, force_event: bool, update: impl FnOnce(&mut TransferTask)) {
        let (Some(transfer_id), Some(transfers)) = (&self.transfer_id, &self.transfers) else {
            return;
        };
        let next_summary = if let Ok(mut transfers) = transfers.lock() {
            let next_summary = if let Some(task) = transfers.get_mut(transfer_id) {
                if !transfer_registry::is_completed_transfer_status(task.summary.status) {
                    update(task);
                }
                if transfer_registry::is_completed_transfer_status(task.summary.status) {
                    task.summary.speed_bytes_per_second = 0;
                    task.speed
                        .reset(task.summary.bytes_transferred, unix_timestamp_millis());
                }
                Some(task.summary.clone())
            } else {
                None
            };
            if next_summary.as_ref().is_some_and(|summary| {
                transfer_registry::is_completed_transfer_status(summary.status)
            }) {
                transfer_registry::prune_completed_transfers(&mut transfers, unix_timestamp());
            }
            next_summary
        } else {
            None
        };
        if let (Some(summary), Some(emitter)) = (next_summary, &self.event_emitter) {
            emitter.emit(&summary, force_event);
        }
    }

    pub(super) fn with_summary<T>(&self, map: impl FnOnce(&SftpTransferSummary) -> T) -> Option<T> {
        let (Some(transfer_id), Some(transfers)) = (&self.transfer_id, &self.transfers) else {
            return None;
        };
        transfers
            .lock()
            .ok()
            .and_then(|transfers| transfers.get(transfer_id).map(|task| map(&task.summary)))
    }

    /// 等待只改变速度/阶段，不伪造 lastProgressAt 或刷新 watchdog 活动时间。
    pub(super) fn mark_waiting_if_stalled(&self, now_ms: u64) {
        if now_ms.saturating_sub(self.last_activity_at_ms.load(Ordering::SeqCst)) < 5_000 {
            return;
        }
        self.update_task(false, |task| {
            task.summary.speed_bytes_per_second = 0;
            task.speed.reset(task.summary.bytes_transferred, now_ms);
            if matches!(
                task.summary.phase.as_deref(),
                Some("transferring" | "uploading" | "downloading")
            ) {
                task.summary.phase = Some("waiting".to_owned());
            }
        });
    }
}

/// 普通错误不能开放重试；只接受已确认回滚、确定结果和单文件上传/下载。
fn is_retryable_after_commit_rollback(
    rollback_confirmed: bool,
    commit_outcome_uncertain: bool,
    operation: crate::models::sftp::SftpTransferOperation,
    kind: crate::models::sftp::SftpTransferKind,
) -> bool {
    rollback_confirmed
        && !commit_outcome_uncertain
        && matches!(
            operation,
            crate::models::sftp::SftpTransferOperation::Upload
                | crate::models::sftp::SftpTransferOperation::Download
        )
        && kind == crate::models::sftp::SftpTransferKind::File
}

/// Helper 或超时错误继续显示，预登记的恢复路径只在错误尚未包含时追加。
fn commit_unknown_error_message(error: &str, recovery_detail: Option<&str>) -> String {
    let error = error.trim();
    let mut message = if error.is_empty() {
        COMMIT_UNKNOWN_ERROR.to_owned()
    } else {
        error.to_owned()
    };
    if let Some(detail) = recovery_detail
        .map(str::trim)
        .filter(|detail| !detail.is_empty() && !message.contains(detail))
    {
        message.push_str("；恢复信息：");
        message.push_str(detail);
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{
        commit_unknown_error_message, is_retryable_after_commit_rollback, TransferProgress,
        COMMIT_UNKNOWN_ERROR,
    };
    use crate::models::sftp::{SftpTransferKind, SftpTransferOperation};
    use std::sync::atomic::Ordering;

    /// Child I/O handles share recovery metadata and rollback proof with the task publishing its terminal state.
    #[test]
    fn detached_child_keeps_commit_recovery_detail_shared() {
        let parent = TransferProgress::detached();
        let child = parent.detached_child();

        child.set_commit_recovery_detail("backup=/target.bak");
        child.note_commit_rollback_confirmed();

        let detail = parent
            .commit_recovery_detail
            .lock()
            .expect("commit detail lock")
            .clone();
        assert_eq!(detail.as_deref(), Some("backup=/target.bak"));
        assert!(parent.commit_rollback_confirmed.load(Ordering::SeqCst));
    }

    /// Future 超时丢弃时没有 helper 返回值，预登记的恢复路径仍必须进入最终摘要。
    #[test]
    fn unknown_commit_keeps_registered_recovery_detail() {
        assert_eq!(
            commit_unknown_error_message(
                COMMIT_UNKNOWN_ERROR,
                Some("backup=/target.bak; partial=/target.part")
            ),
            "提交结果未确认，请核对目标文件；恢复信息：backup=/target.bak; partial=/target.part",
        );
    }

    /// Helper 已在错误中说明恢复路径时避免重复拼接同一信息。
    #[test]
    fn unknown_commit_does_not_duplicate_error_detail() {
        assert_eq!(
            commit_unknown_error_message(
                "提交结果不明：backup=/target.bak",
                Some("backup=/target.bak")
            ),
            "提交结果不明：backup=/target.bak",
        );
    }

    /// 只有已确认的单文件传输回滚开放人工重试，未知结果和普通错误仍封锁。
    #[test]
    fn only_confirmed_single_file_rollback_is_retryable() {
        assert!(is_retryable_after_commit_rollback(
            true,
            false,
            SftpTransferOperation::Upload,
            SftpTransferKind::File,
        ));
        assert!(is_retryable_after_commit_rollback(
            true,
            false,
            SftpTransferOperation::Download,
            SftpTransferKind::File,
        ));
        assert!(!is_retryable_after_commit_rollback(
            false,
            false,
            SftpTransferOperation::Upload,
            SftpTransferKind::File,
        ));
        assert!(!is_retryable_after_commit_rollback(
            true,
            true,
            SftpTransferOperation::Upload,
            SftpTransferKind::File,
        ));
        assert!(!is_retryable_after_commit_rollback(
            true,
            false,
            SftpTransferOperation::Upload,
            SftpTransferKind::Directory,
        ));
        assert!(!is_retryable_after_commit_rollback(
            true,
            false,
            SftpTransferOperation::RemoteCopy,
            SftpTransferKind::File,
        ));
    }
}
