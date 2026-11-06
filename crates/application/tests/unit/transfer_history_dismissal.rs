//! 面板清理的耐久归档、来源隔离及部分取消回归。
//!
//! @author kongweiguang

use super::*;
use crate::transfers::{
    InMemoryTransferRecoveryRepository, TransferRecoveryPage, TransferRecoveryRecord,
    TransferRecoveryRepository,
};

/// 原暂停任务产生后继后应成为普通历史，不能留下既无法继续又始终不能清理的旧卡片。
///
/// @author kongweiguang
#[test]
fn retry_successor_releases_predecessor_pause_attention() {
    let (_, mut record, _, _) = cancelling_checkpoint_record();
    record.task.confirm_cancelled(true).unwrap();
    assert!(record.summary().resumable);
    record.task.set_retry_successor(TransferId::new()).unwrap();
    assert!(!record.summary().resumable);
    assert!(super::super::is_removable_history_record(&record));
}

/// 已结束的活任务可保留证据归档；孤儿只有无待办证据才可收起，未知提交与暂停继续可见。
///
/// @author kongweiguang
#[tokio::test]
async fn dismissal_archives_safe_history_and_preserves_unresolved_evidence() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let origin = TransferOrigin::SharedWorkbench;
    let (safe_id, mut safe) = terminal_record(TransferFailureKind::Connection, 1);
    safe.origin = Some(origin.clone());
    let ownership = TransferStagingOwnership {
        staging_destination: FileEndpoint::Local {
            path: "stage.partial".into(),
        },
        actual_destination: safe.task.request().destination.clone(),
        staging_owner: safe_id.clone(),
        staging_token: "a".repeat(32),
    };
    safe.staging_ownership = vec![ownership.clone()];
    service.persist_recovery_record(&safe).await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(safe_id.clone(), safe);

    let (orphan_id, mut orphan) = terminal_record(TransferFailureKind::Connection, 2);
    orphan.origin = Some(origin.clone());
    service.persist_recovery_record(&orphan).await.unwrap();

    let (unknown_id, mut unknown) = terminal_record(TransferFailureKind::CommitUnknown, 3);
    unknown.origin = Some(origin.clone());
    service.persist_recovery_record(&unknown).await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(unknown_id.clone(), unknown);

    let (paused_id, mut paused, _, _) = cancelling_checkpoint_record();
    paused.task.confirm_cancelled(true).unwrap();
    paused.origin = Some(origin.clone());
    service.persist_recovery_record(&paused).await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(paused_id.clone(), paused);

    let (pending_id, mut pending) = terminal_record(TransferFailureKind::Connection, 4);
    pending.origin = Some(origin.clone());
    service.persist_recovery_record(&pending).await.unwrap();
    let mut pending_saved = service
        .inner
        .recovery_repository
        .list(CancellationToken::new())
        .await
        .unwrap()
        .into_iter()
        .find(|saved| saved.id == pending_id)
        .unwrap();
    pending_saved.cleanup_pending = true;
    service
        .inner
        .recovery_repository
        .upsert(pending_saved, CancellationToken::new())
        .await
        .unwrap();

    let (other_id, mut other) = terminal_record(TransferFailureKind::Connection, 5);
    other.origin = Some(TransferOrigin::TerminalPane(PaneId::new()));
    service.persist_recovery_record(&other).await.unwrap();

    service
        .dismiss_completed_for_origin(&origin, CancellationToken::new())
        .await
        .unwrap();
    assert!(service.get(&safe_id).await.is_err());
    assert!(service.get(&paused_id).await.is_ok());
    assert!(service.get(&unknown_id).await.is_ok());
    let remaining = service
        .inner
        .recovery_repository
        .list(CancellationToken::new())
        .await
        .unwrap();
    assert!(
        !remaining
            .iter()
            .any(|saved| saved.id == safe_id || saved.id == orphan_id)
    );
    for id in [&paused_id, &unknown_id, &pending_id, &other_id] {
        assert!(remaining.iter().any(|saved| &saved.id == id));
    }
    let archived = service
        .list_archived_recovery(None, 10, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(archived.records.len(), 2);
    assert_eq!(
        archived
            .records
            .iter()
            .find(|saved| saved.id == safe_id)
            .unwrap()
            .staging_ownership,
        vec![ownership]
    );
}

struct CancelAfterArchive {
    inner: InMemoryTransferRecoveryRepository,
    cancel_once: AtomicBool,
    error_once: AtomicBool,
}

#[async_trait]
impl TransferRecoveryRepository for CancelAfterArchive {
    /// 复用真实内存仓储语义，仅在归档确认后注入取消。
    /// @author kongweiguang
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.inner.list(cancellation).await
    }

    /// 不更改保存行为，避免测试替身掩盖不可变归档约束。
    /// @author kongweiguang
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        self.inner.upsert(record, cancellation).await
    }

    /// 删除仍服从仓储的取消门禁。
    /// @author kongweiguang
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.inner.remove(id, cancellation).await
    }

    /// 固定成功写入后的取消或未知回执，覆盖冷档已写但活动文件收敛失败的边界。
    /// @author kongweiguang
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.inner.archive(id, cancellation.clone()).await?;
        if self.cancel_once.swap(false, Ordering::AcqRel) {
            cancellation.cancel();
        }
        if self.error_once.swap(false, Ordering::AcqRel) {
            return Err(AppError::outcome_unknown("fixture archive receipt lost"));
        }
        Ok(())
    }

    /// 测试通过正式冷档投影核实证据保存。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        self.inner.list_archived(cursor, limit, cancellation).await
    }
}

/// 第一项归档确认便移除活卡片；中途取消保留未归档任务，下一次可接续且不重开冷档。
///
/// @author kongweiguang
#[tokio::test]
async fn dismissal_cancellation_keeps_only_uncommitted_live_records() {
    let (base, _, _, _) = service_for(BackendMode::NoChanges);
    let service = TransferService::new_with_recovery_repository(
        base.inner.backends.to_vec(),
        Handle::current(),
        base.inner.options.clone(),
        base.inner.sftp_policy.clone(),
        Arc::new(CancelAfterArchive {
            inner: InMemoryTransferRecoveryRepository::default(),
            cancel_once: AtomicBool::new(true),
            error_once: AtomicBool::new(false),
        }),
        None,
    )
    .unwrap();
    let origin = TransferOrigin::SharedWorkbench;
    for sequence in 1..=2 {
        let (id, mut record) = terminal_record(TransferFailureKind::Connection, sequence);
        record.origin = Some(origin.clone());
        service.persist_recovery_record(&record).await.unwrap();
        service.inner.records.lock().await.insert(id, record);
    }
    let error = service
        .dismiss_completed_for_origin(&origin, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    let live = service.list_for_origin(&origin).await;
    assert_eq!(live.len(), 1);
    let archived = service
        .list_archived_recovery(None, 10, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(archived.records.len(), 1);
    assert_ne!(live[0].id, archived.records[0].id);
    service
        .dismiss_completed_for_origin(&origin, CancellationToken::new())
        .await
        .unwrap();
    assert!(service.list_for_origin(&origin).await.is_empty());
    assert!(
        service
            .list_recovery_for_origin(&origin, CancellationToken::new())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        service
            .list_archived_recovery(None, 10, CancellationToken::new())
            .await
            .unwrap()
            .records
            .len(),
        2
    );
}

/// 归档提交后丢失回执不得再续传旧 ID；原材料保留，普通重试改用新 ID 避开不可变冷档。
///
/// @author kongweiguang
#[tokio::test]
async fn dismissal_unknown_archive_receipt_revokes_only_live_resume() {
    let (base, _, _, _) = service_for(BackendMode::NoChanges);
    let service = TransferService::new_with_recovery_repository(
        base.inner.backends.to_vec(),
        Handle::current(),
        base.inner.options.clone(),
        base.inner.sftp_policy.clone(),
        Arc::new(CancelAfterArchive {
            inner: InMemoryTransferRecoveryRepository::default(),
            cancel_once: AtomicBool::new(false),
            error_once: AtomicBool::new(true),
        }),
        None,
    )
    .unwrap();
    let origin = TransferOrigin::SharedWorkbench;
    let (id, mut record, checkpoint, _) = cancelling_checkpoint_record();
    let mut request = record.task.request().clone();
    request.source = FileEndpoint::remote(HostId::new(), "/source.bin").unwrap();
    let mut task = TransferTask::queued(id.clone(), request).unwrap();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(checkpoint.source_revision.size))
        .unwrap();
    task.select_actual_destination(checkpoint.actual_destination.clone())
        .unwrap();
    task.record_progress(
        checkpoint.confirmed_bytes,
        Some(checkpoint.source_revision.size),
    )
    .unwrap();
    task.fail(
        TransferFailureKind::Connection,
        "fixture failed".into(),
        true,
        true,
    )
    .unwrap();
    record.task = task;
    record.origin = Some(origin.clone());
    service.persist_recovery_record(&record).await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .insert(id.clone(), record);
    assert!(service.get(&id).await.unwrap().resumable);

    let error = service
        .dismiss_completed_for_origin(&origin, CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert!(!service.get(&id).await.unwrap().resumable);
    assert!(service.resume(&id).await.is_err());
    let archived = service
        .list_archived_recovery(None, 10, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(archived.records.len(), 1);
    assert_eq!(archived.records[0].checkpoint, Some(checkpoint));
    let retried = service.retry(&id).await.expect("new ID remains retryable");
    assert_ne!(retried.id, id);
    service
        .dismiss_completed_for_origin(&origin, CancellationToken::new())
        .await
        .unwrap();
    assert!(service.get(&id).await.is_err());
}
