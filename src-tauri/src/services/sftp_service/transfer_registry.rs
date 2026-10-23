//! SFTP transfer registry facade methods.
//!
//! @author kongweiguang

use super::*;

pub(super) const RECENT_COMPLETED_TRANSFER_LIMIT: usize = 200;
pub(super) const RECENT_COMPLETED_TRANSFER_SECONDS: u64 = 24 * 60 * 60;

impl SftpService {
    /// 后继任务继承服务端保存的原始参数，不接受调用者重写目标来绕过断点身份校验。
    pub fn retry_transfer(
        &self,
        paths: &KerminalPaths,
        request: SftpTransferRetryRequest,
    ) -> AppResult<SftpTransferSummary> {
        self.retry_transfer_with_events(paths, request, None)
    }

    /// 窗口事件与普通入队复用，避免 UI 与 MCP 形成不同的恢复状态机。
    pub fn retry_transfer_for_window(
        &self,
        paths: &KerminalPaths,
        request: SftpTransferRetryRequest,
        window: Window,
    ) -> AppResult<SftpTransferSummary> {
        self.retry_transfer_with_events(paths, request, Some(TransferEventEmitter::new(window)))
    }

    /// 只允许普通文件传输的终态重试；复合操作没有足够检查点，不能静默重放副作用。
    fn retry_transfer_with_events(
        &self,
        paths: &KerminalPaths,
        request: SftpTransferRetryRequest,
        emitter: Option<TransferEventEmitter>,
    ) -> AppResult<SftpTransferSummary> {
        let summary = {
            let transfers = self.transfers()?;
            let task = transfers
                .get(&request.transfer_id)
                .ok_or_else(|| AppError::NotFound("SFTP 传输任务不存在".to_owned()))?;
            if !transfer_matches_scope(&task.summary, request.view_scope.as_deref()) {
                return Err(AppError::NotFound("SFTP 传输任务不属于当前视图".to_owned()));
            }
            if let Some(successor) = task
                .summary
                .successor_id
                .as_ref()
                .and_then(|id| transfers.get(id))
            {
                return Ok(successor.summary.clone());
            }
            let s = &task.summary;
            if !matches!(
                s.status,
                SftpTransferStatus::Failed | SftpTransferStatus::Canceled
            ) || !s.retryable
                || s.kind != SftpTransferKind::File
                || !matches!(
                    s.operation,
                    SftpTransferOperation::Upload | SftpTransferOperation::Download
                )
            {
                return Err(AppError::Sftp("该任务不能继续传输".to_owned()));
            }
            s.clone()
        };
        self.enqueue_transfer_with_recovery(
            paths,
            SftpManagedTransferRequest {
                host_id: summary.host_id,
                local_path: summary.local_path,
                remote_path: summary.remote_path,
                direction: summary.direction,
                kind: summary.kind,
                conflict_policy: summary
                    .conflict_policy
                    .unwrap_or(crate::models::sftp::SftpTransferConflictPolicy::Overwrite),
                view_scope: summary.view_scope,
                idle_timeout_seconds: Some(summary.idle_timeout_seconds),
            },
            emitter,
            Some(request.transfer_id),
        )
    }
    /// 列出传输任务。
    pub fn list_transfers(&self) -> AppResult<Vec<SftpTransferSummary>> {
        self.list_transfers_for_scope(SftpTransferScopeRequest::default())
    }

    /// 按前端视图 scope 列出传输任务。
    pub fn list_transfers_for_scope(
        &self,
        request: SftpTransferScopeRequest,
    ) -> AppResult<Vec<SftpTransferSummary>> {
        let mut transfers = self.transfers()?;
        prune_completed_transfers(&mut transfers, unix_timestamp());
        let mut summaries = transfers
            .values()
            .filter(|task| transfer_matches_scope(&task.summary, request.view_scope.as_deref()))
            .map(|task| task.summary.clone())
            .collect::<Vec<_>>();
        summaries.sort_by_key(|summary| summary.created_at);
        Ok(summaries)
    }

    /// 取消传输任务。
    pub fn cancel_transfer(
        &self,
        request: SftpTransferCancelRequest,
    ) -> AppResult<SftpTransferSummary> {
        self.cancel_transfer_with_events(request, None)
    }

    /// 取消传输任务，并向当前窗口推送状态更新。
    pub fn cancel_transfer_for_window(
        &self,
        request: SftpTransferCancelRequest,
        window: Window,
    ) -> AppResult<SftpTransferSummary> {
        self.cancel_transfer_with_events(request, Some(TransferEventEmitter::new(window)))
    }

    /// 先固化取消意图再唤醒监督器；提交阶段保留 phase，确保取消仍会等待提交/回滚回执。
    fn cancel_transfer_with_events(
        &self,
        request: SftpTransferCancelRequest,
        event_emitter: Option<TransferEventEmitter>,
    ) -> AppResult<SftpTransferSummary> {
        let mut transfers = self.transfers()?;
        let Some(task) = transfers.get_mut(&request.transfer_id) else {
            return Err(AppError::NotFound(format!(
                "SFTP 传输任务不存在: {}",
                request.transfer_id
            )));
        };
        if !transfer_matches_scope(&task.summary, request.view_scope.as_deref()) {
            return Err(AppError::NotFound(format!(
                "SFTP 传输任务不属于当前视图: {}",
                request.transfer_id
            )));
        }

        // 终态是不可逆结果；迟到的取消不能覆盖成功、失败或已取消任务。
        if is_completed_transfer_status(task.summary.status) {
            return Ok(task.summary.clone());
        }

        task.cancel_requested.store(true, Ordering::SeqCst);
        // 单任务只有一个活跃监督器；notify_one 会保存尚未注册 waiter 时的取消信号。
        task.cancel_notify.notify_one();
        let cancel_requested_at = unix_timestamp_millis();
        tauri_plugin_log::log::info!(
            "sftp transfer cancel transferId={} cancelRequestedAt={cancel_requested_at}",
            task.summary.id
        );
        task.summary.cancel_requested = true;
        task.summary.updated_at = unix_timestamp();
        if task.summary.status == SftpTransferStatus::Queued {
            task.summary.status = SftpTransferStatus::Canceled;
            task.summary.phase = Some("canceled".to_owned());
            task.summary.retryable = matches!(
                task.summary.operation,
                SftpTransferOperation::Upload | SftpTransferOperation::Download
            ) && task.summary.kind == SftpTransferKind::File;
        } else {
            if task.summary.phase.as_deref() != Some("committing") {
                task.summary.phase = Some("canceling".to_owned());
            }
            task.summary.speed_bytes_per_second = 0;
        }
        let summary = task.summary.clone();
        if is_completed_transfer_status(summary.status) {
            prune_completed_transfers(&mut transfers, unix_timestamp());
        }
        drop(transfers);
        if let Some(emitter) = &event_emitter {
            emitter.emit(&summary, true);
        }
        Ok(summary)
    }

    /// 清理已经完成的传输任务。
    pub fn clear_completed_transfers(&self) -> AppResult<Vec<SftpTransferSummary>> {
        self.clear_completed_transfers_for_scope(SftpTransferScopeRequest::default())
    }

    /// 按前端视图 scope 清理已经完成的传输任务。
    pub fn clear_completed_transfers_for_scope(
        &self,
        request: SftpTransferScopeRequest,
    ) -> AppResult<Vec<SftpTransferSummary>> {
        let mut transfers = self.transfers()?;
        transfers.retain(|_, task| {
            !transfer_matches_scope(&task.summary, request.view_scope.as_deref())
                || !matches!(
                    task.summary.status,
                    SftpTransferStatus::Succeeded
                        | SftpTransferStatus::Failed
                        | SftpTransferStatus::Canceled
                )
        });
        prune_completed_transfers(&mut transfers, unix_timestamp());
        let mut summaries = transfers
            .values()
            .filter(|task| transfer_matches_scope(&task.summary, request.view_scope.as_deref()))
            .map(|task| task.summary.clone())
            .collect::<Vec<_>>();
        summaries.sort_by_key(|summary| summary.created_at);
        Ok(summaries)
    }
}

pub(super) fn transfer_matches_scope(
    summary: &SftpTransferSummary,
    view_scope: Option<&str>,
) -> bool {
    view_scope
        .map(|scope| summary.view_scope.as_deref() == Some(scope))
        .unwrap_or(true)
}

pub(super) fn prune_completed_transfers(
    transfers: &mut HashMap<String, TransferTask>,
    now: u64,
) -> usize {
    let prune_ids = completed_transfer_prune_ids(transfers.values().map(|task| &task.summary), now);
    let pruned = prune_ids.len();
    for id in prune_ids {
        transfers.remove(&id);
    }
    pruned
}

pub(super) fn completed_transfer_prune_ids<'a>(
    summaries: impl IntoIterator<Item = &'a SftpTransferSummary>,
    now: u64,
) -> HashSet<String> {
    let min_updated_at = now.saturating_sub(RECENT_COMPLETED_TRANSFER_SECONDS);
    let mut prune_ids = HashSet::new();
    let mut recent_completed = Vec::new();

    for summary in summaries {
        if !is_completed_transfer_status(summary.status) {
            continue;
        }
        if summary.updated_at < min_updated_at {
            prune_ids.insert(summary.id.clone());
            continue;
        }
        recent_completed.push((summary.updated_at, summary.created_at, summary.id.clone()));
    }

    if recent_completed.len() > RECENT_COMPLETED_TRANSFER_LIMIT {
        recent_completed.sort_by(|left, right| {
            left.0
                .cmp(&right.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.cmp(&right.2))
        });
        let excess_completed = recent_completed.len() - RECENT_COMPLETED_TRANSFER_LIMIT;
        for (_, _, id) in recent_completed.into_iter().take(excess_completed) {
            prune_ids.insert(id);
        }
    }

    prune_ids
}

pub(super) fn is_completed_transfer_status(status: SftpTransferStatus) -> bool {
    matches!(
        status,
        SftpTransferStatus::Succeeded | SftpTransferStatus::Failed | SftpTransferStatus::Canceled
    )
}
