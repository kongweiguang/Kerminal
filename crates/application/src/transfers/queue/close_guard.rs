//! 关闭工作区来源前读取真实队列 owner 与恢复孤儿，避免 UI 缓存承担生命周期判断。
//!
//! @author kongweiguang

use std::collections::HashSet;

use kerminal_domain::files::{TransferFailureKind, TransferSummary};
use tokio_util::sync::CancellationToken;

use super::{TransferRecord, TransferService, is_terminal};
use crate::{AppError, AppResult, transfers::TransferOrigin};

impl TransferService {
    /// 同时核对 live worker 与恢复孤儿；普通终态历史可关闭，尚未确认收尾的 owner 必须保留入口。
    ///
    /// @author kongweiguang
    pub async fn origin_has_close_blockers(
        &self,
        origin: &TransferOrigin,
        cancellation: CancellationToken,
    ) -> AppResult<bool> {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输关闭状态核对已取消"));
        }
        let recovery = self
            .list_recovery_for_origin(origin, cancellation.clone())
            .await?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输关闭状态核对已取消"));
        }

        let records = self.inner.records.lock().await;
        let live_records = records
            .values()
            .filter(|record| record.origin.as_ref() == Some(origin))
            .collect::<Vec<_>>();
        let live_ids = live_records
            .iter()
            .map(|record| record.summary().id)
            .collect::<HashSet<_>>();
        if live_records
            .iter()
            .any(|record| live_transfer_blocks_close(record, self))
        {
            return Ok(true);
        }

        Ok(recovery
            .iter()
            .any(|record| recovery_orphan_blocks_close(record, &live_ids)))
    }
}

/// 把队列 owner 的真实结束事实与面板可见的待办状态合并判定，防止终态快照早于 worker join。
///
/// @author kongweiguang
fn live_transfer_blocks_close(record: &TransferRecord, service: &TransferService) -> bool {
    let summary = record.summary();
    let worker_finished = record.attempt_finished
        && !record.worker_join_pending
        && record
            .worker
            .as_ref()
            .is_none_or(|worker| worker.is_finished());
    transfer_summary_blocks_close(
        &summary,
        worker_finished,
        service.is_drag_staging_pending_delivery(&summary.id),
    )
}

/// 只对仍需用户处理的摘要阻止关闭；可续传取消、未知提交和清理 owner 与传输面板保持同义。
///
/// @author kongweiguang
fn transfer_summary_blocks_close(
    summary: &TransferSummary,
    worker_finished: bool,
    pending_delivery: bool,
) -> bool {
    !worker_finished
        || !is_terminal(summary.status)
        || summary.cleanup_pending
        || summary.failure_kind == Some(TransferFailureKind::CommitUnknown)
        || summary.resumable
        || pending_delivery
}

/// live 记录同 ID 时以其运行态为准，避免成功回执尚未清理的持久事实把正常关闭误判为待办。
///
/// @author kongweiguang
fn recovery_orphan_blocks_close(
    record: &super::super::TransferRecoveryRecord,
    live_ids: &HashSet<kerminal_domain::identity::TransferId>,
) -> bool {
    !live_ids.contains(&record.id)
        && (!record.archived
            || record.cleanup_pending
            || record.checkpoint.is_some()
            || !record.staging_ownership.is_empty()
            || record.drag_staging)
}

#[cfg(test)]
#[path = "../../../tests/unit/transfer_close_guard.rs"]
mod tests;
