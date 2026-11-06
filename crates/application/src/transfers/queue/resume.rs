//! 同记录续传的准入、worker 所有权和写前持久化。
//!
//! @author kongweiguang

use kerminal_domain::{
    files::{TransferFailureKind, TransferKind, TransferSummary},
    identity::TransferId,
};
use tokio::time::{Instant, timeout_at};
use tokio_util::sync::CancellationToken;

use super::{
    ShutdownWorkerOwner, TransferRecord, TransferService, external_host_for_request, is_terminal,
    publish, valid_checkpoint,
};
use crate::{AppError, AppResult, platform::external_launch::ExternalTargetLease};

impl TransferService {
    /// 以原记录恢复可信断点，避免暂停和继续产生新的历史项目。
    ///
    /// @author kongweiguang
    pub async fn resume(&self, id: &TransferId) -> AppResult<TransferSummary> {
        self.resume_inner(id, None).await
    }

    /// 临时主机必须由当前调用方重新提供同一 scope，不能借用已失效的目标。
    ///
    /// @author kongweiguang
    pub async fn resume_with_external_target(
        &self,
        id: &TransferId,
        lease: &ExternalTargetLease,
    ) -> AppResult<TransferSummary> {
        self.resume_inner(id, Some(lease)).await
    }

    /// 等待原 worker 真正退出后再持久化和启动；共享句柄容器跨调用取消保留所有权。
    ///
    /// @author kongweiguang
    async fn resume_inner(
        &self,
        id: &TransferId,
        external_lease: Option<&ExternalTargetLease>,
    ) -> AppResult<TransferSummary> {
        let deadline = Instant::now() + self.inner.options.shutdown_timeout;
        let _shutdown = timeout_at(deadline, self.inner.shutdown_lock.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输恢复锁"))?;
        let _intake = timeout_at(deadline, self.inner.intake_lock.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输入队锁"))?;
        self.ensure_open()?;
        let mut workers = timeout_at(deadline, self.inner.shutdown_workers.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输 worker 所有权锁"))?;
        {
            let mut records = timeout_at(deadline, self.inner.records.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输记录锁"))?;
            let record = records
                .get_mut(id)
                .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
            validate_resume_target(record, external_lease)?;
            if record.drag_staging {
                return Err(AppError::conflict("拖出准备已结束，请重新拖动以创建新任务"));
            }
            if !is_terminal(record.task.status()) {
                return Ok(record.summary());
            }
            validate_resume_record(record)?;
            let owned_count = workers
                .iter()
                .filter(|owner| worker_owner_id(owner) == id)
                .count();
            if owned_count > 1 || (owned_count != 0 && record.worker.is_some()) {
                return Err(AppError::infrastructure("传输 worker 所有权重复"));
            }
            if let Some(handle) = record.worker.take() {
                workers.push(ShutdownWorkerOwner::Running {
                    id: id.clone(),
                    handle,
                });
                record.worker_join_pending = true;
            } else if owned_count != 0 {
                record.worker_join_pending = true;
            }
        }

        if let Some(index) = workers
            .iter()
            .position(|owner| worker_owner_id(owner) == id)
        {
            let joined = match &mut workers[index] {
                ShutdownWorkerOwner::Running { handle, .. } => {
                    match timeout_at(deadline, &mut *handle).await {
                        Ok(result) => result,
                        Err(_) if handle.is_finished() => (&mut *handle).await,
                        Err(_) => {
                            return Err(AppError::timeout("原传输 worker 仍在收尾，句柄已保留"));
                        }
                    }
                }
                ShutdownWorkerOwner::JoinFailed { .. } => {
                    return Err(AppError::infrastructure("原传输 worker 异常退出，不能续传"));
                }
            };
            if let Err(error) = joined {
                let cancelled = error.is_cancelled();
                let panicked = error.is_panic();
                workers[index] = ShutdownWorkerOwner::JoinFailed {
                    id: id.clone(),
                    cancelled,
                    panicked,
                };
                self.record_worker_join_failure_until(id, cancelled, panicked, deadline)
                    .await?;
                return Err(AppError::infrastructure("原传输 worker 异常退出，不能续传"));
            }
            workers.remove(index);
        }

        let mut records = timeout_at(deadline, self.inner.records.lock())
            .await
            .map_err(|_| AppError::timeout("等待续传记录锁"))?;
        let active_count = records
            .values()
            .filter(|record| !is_terminal(record.task.status()))
            .count();
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        record.worker_join_pending = false;
        validate_resume_target(record, external_lease)?;
        validate_resume_record(record)?;
        if !record.attempt_finished {
            return Err(AppError::conflict("原传输 worker 未确认退出，不能续传"));
        }
        if active_count >= self.inner.options.max_queued + self.inner.options.max_concurrent {
            return Err(AppError::conflict("传输队列已达到容量上限"));
        }
        let checkpoint = record
            .checkpoint
            .clone()
            .ok_or_else(|| AppError::conflict("传输缺少可信断点"))?;
        let mut task = record.task.clone();
        task.resume_from_checkpoint(
            checkpoint.confirmed_bytes,
            Some(checkpoint.source_revision.size),
        )
        .map_err(|_| AppError::conflict("该传输结果不能从断点继续"))?;
        let retained_lease = external_lease
            .map(ExternalTargetLease::retain)
            .transpose()?;
        let mut resumed = TransferRecord {
            task,
            origin: record.origin.clone(),
            drag_staging: record.drag_staging,
            drag_evidence_pending: record.drag_evidence_pending,
            checkpoint: Some(checkpoint),
            staging_ownership: record.staging_ownership.clone(),
            sftp_policy: record.sftp_policy,
            source_sftp: record.source_sftp.clone(),
            destination_sftp: record.destination_sftp.clone(),
            phase: None,
            effective_idle_timeout_seconds: record.effective_idle_timeout_seconds,
            speed_bytes_per_second: 0,
            last_progress_at: None,
            progress_sample: None,
            external_target_lease: retained_lease,
            cleanup_owner: None,
            cancellation: CancellationToken::new(),
            commit_finalized: false,
            no_changes_finalized: false,
            attempt_finished: false,
            worker: None,
            worker_join_pending: false,
            sequence: record.sequence,
        };
        timeout_at(deadline, self.persist_recovery_record(&resumed))
            .await
            .map_err(|_| AppError::timeout("保存续传恢复材料超时"))??;
        let service = self.clone();
        let worker_id = id.clone();
        resumed.worker = Some(self.inner.runtime.spawn(async move {
            service.run_worker(worker_id).await;
        }));
        let summary = resumed.summary();
        *record = resumed;
        publish(&self.inner.events);
        Ok(summary)
    }
}

/// 先校验目标身份再检查活动状态，防止调用方用另一个临时主机查询或启动续传。
///
/// @author kongweiguang
fn validate_resume_target(
    record: &TransferRecord,
    lease: Option<&ExternalTargetLease>,
) -> AppResult<()> {
    match (external_host_for_request(record.task.request())?, lease) {
        (Some(_), None) => Err(AppError::conflict("临时外部目标已释放，请重新接入后续传")),
        (Some(host_id), Some(lease)) if lease.host_id() != &host_id => {
            Err(AppError::invalid("续传目标与当前临时外部连接不匹配"))
        }
        (None, Some(_)) => Err(AppError::invalid("续传请求没有使用临时外部目标")),
        _ => Ok(()),
    }
}

/// 只接受与已登记暂存 owner 完全一致的断点，防止用不可信路径或提交未知状态覆盖目标。
///
/// @author kongweiguang
fn validate_resume_record(record: &TransferRecord) -> AppResult<()> {
    if record.task.failure_kind() == Some(TransferFailureKind::CommitUnknown) {
        return Err(AppError::outcome_unknown("提交结果未确认，不能安全续传"));
    }
    if record.cleanup_owner.is_some() {
        return Err(AppError::conflict("传输资源尚未确认释放，不能安全续传"));
    }
    if record.task.retry_successor().is_some() {
        return Err(AppError::conflict("该传输已有重试后继，不能再次续传"));
    }
    if !record.task.resumable() || record.task.request().kind != TransferKind::File {
        return Err(AppError::conflict("该传输结果不能安全续传"));
    }
    let checkpoint = record
        .checkpoint
        .as_ref()
        .ok_or_else(|| AppError::conflict("传输缺少可信断点"))?;
    if record.task.actual_destination() != Some(&checkpoint.actual_destination)
        || !valid_checkpoint(
            checkpoint,
            checkpoint.confirmed_bytes,
            record.task.actual_destination(),
        )
        || !record.staging_ownership.iter().any(|ownership| {
            ownership.staging_owner == checkpoint.staging_owner
                && ownership.staging_token == checkpoint.staging_token
                && ownership.staging_destination == checkpoint.staging_destination
                && ownership.actual_destination == checkpoint.actual_destination
        })
    {
        return Err(AppError::conflict("传输断点与暂存所有权不一致"));
    }
    Ok(())
}

/// 同一个 ID 在共享容器中只能拥有一个待回收 worker。
///
/// @author kongweiguang
fn worker_owner_id(owner: &ShutdownWorkerOwner) -> &TransferId {
    match owner {
        ShutdownWorkerOwner::Running { id, .. } | ShutdownWorkerOwner::JoinFailed { id, .. } => id,
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/transfer_resume.rs"]
mod tests;
