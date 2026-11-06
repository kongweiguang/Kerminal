//! idle 取消回执中的断点必须先通过归属与持久化门禁。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FileRevision, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus,
    },
    identity::TransferId,
};
use tokio::{
    runtime::Handle,
    sync::Notify,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use super::super::{
    TransferBackend, TransferCheckpoint, TransferCleanupState, TransferExecutionContext,
    TransferExecutionError, TransferExecutionFailure, TransferExecutionResult, TransferPhase,
    TransferProgressUpdate, TransferQueueOptions, TransferResourceCleanup, TransferService,
    TransferStagingOwnership,
};
use crate::AppResult;

#[derive(Clone, Copy)]
enum ReceiptMode {
    Valid,
    Advance,
    OlderCheckpoint,
    CancelRace,
    InvalidDigest,
    WrongOwner,
    WrongDestination,
    PendingCleanup,
    FailureAdvance,
    FailureConflict,
    FailureInvalidSource,
}

struct IdleBackend {
    mode: ReceiptMode,
    attempts: AtomicUsize,
    received: Mutex<Vec<TransferCheckpoint>>,
    resumed: Notify,
    release: Notify,
    idle_cancelled: Notify,
    release_cancel_result: Notify,
}

struct Cleanup;

#[async_trait]
impl TransferResourceCleanup for Cleanup {
    /// 受控清理只在 shutdown 时确认释放，便于观察失败回执保留 owner。
    async fn cleanup_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

#[async_trait]
impl TransferBackend for IdleBackend {
    /// 测试后端只由独立 fixture 使用，不接触真实文件系统。
    fn supports(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 首轮受控返回首次或更新断点，第二轮保留闸门供测试检查恢复材料。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let attempt = self.attempts.fetch_add(1, Ordering::SeqCst);
        let destination = context.request.destination.clone();
        context.progress.begin_transfer(Some(100)).await.unwrap();
        if attempt > 0 {
            let checkpoint = context.resume_from.clone().expect("recovery checkpoint");
            context.progress.report(TransferProgressUpdate {
                confirmed_bytes: checkpoint.confirmed_bytes,
                total_bytes: Some(100),
                phase: TransferPhase::Preparing,
                actual_destination: Some(destination.clone()),
                checkpoint: Some(checkpoint.clone()),
                activity_sequence: 1,
            });
            self.received.lock().unwrap().push(checkpoint);
            self.resumed.notify_one();
            self.release.notified().await;
            context
                .progress
                .begin_commit(destination.clone(), 100, Some(100), None)
                .await
                .unwrap();
            context.progress.record_committed_entry(100).await.unwrap();
            context.progress.finish_commit().await.unwrap();
            return Ok(TransferExecutionResult {
                actual_destination: destination,
                total_bytes: Some(100),
                skipped: false,
                unchanged: false,
            });
        }

        let ownership = TransferStagingOwnership {
            staging_destination: FileEndpoint::Local {
                path: ".target.partial".into(),
            },
            actual_destination: destination.clone(),
            staging_owner: context.transfer_id.clone(),
            staging_token: "a".repeat(32),
        };
        context
            .progress
            .register_staging(ownership.clone())
            .await
            .unwrap();
        let mut checkpoint = TransferCheckpoint {
            source_revision: FileRevision {
                size: 100,
                modified: None,
                permissions: None,
                content_digest: Some("b".repeat(64)),
            },
            confirmed_bytes: 60,
            confirmed_prefix_sha256: "c".repeat(64),
            staging_owner: ownership.staging_owner,
            staging_destination: ownership.staging_destination,
            staging_token: ownership.staging_token,
            actual_destination: destination.clone(),
        };
        let previous_checkpoint = if matches!(
            self.mode,
            ReceiptMode::Advance
                | ReceiptMode::OlderCheckpoint
                | ReceiptMode::FailureAdvance
                | ReceiptMode::FailureConflict
                | ReceiptMode::FailureInvalidSource
        ) {
            let mut previous = checkpoint.clone();
            previous.confirmed_bytes = 30;
            if matches!(
                self.mode,
                ReceiptMode::FailureAdvance
                    | ReceiptMode::FailureConflict
                    | ReceiptMode::FailureInvalidSource
            ) {
                previous.confirmed_prefix_sha256 = "d".repeat(64);
            }
            Some(previous)
        } else {
            None
        };
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: 40,
            total_bytes: Some(100),
            phase: TransferPhase::Transferring,
            actual_destination: Some(destination.clone()),
            checkpoint: previous_checkpoint,
            activity_sequence: 1,
        });
        context.cancellation.cancelled().await;
        if matches!(
            self.mode,
            ReceiptMode::CancelRace
                | ReceiptMode::FailureAdvance
                | ReceiptMode::FailureConflict
                | ReceiptMode::FailureInvalidSource
        ) {
            self.idle_cancelled.notify_one();
            self.release_cancel_result.notified().await;
        }
        if matches!(self.mode, ReceiptMode::PendingCleanup) {
            return Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    true,
                    false,
                    None,
                    Some(destination),
                    TransferCleanupState::pending(Arc::new(Cleanup)),
                ),
            ));
        }
        if matches!(self.mode, ReceiptMode::FailureAdvance) {
            return Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    true,
                    true,
                    Some(checkpoint),
                    Some(destination),
                    TransferCleanupState::Released,
                ),
            ));
        }
        if matches!(
            self.mode,
            ReceiptMode::FailureConflict | ReceiptMode::FailureInvalidSource
        ) {
            let kind = if matches!(self.mode, ReceiptMode::FailureConflict) {
                TransferFailureKind::Conflict
            } else {
                TransferFailureKind::InvalidSource
            };
            return Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    kind,
                    false,
                    false,
                    None,
                    Some(destination),
                    TransferCleanupState::Released,
                ),
            ));
        }
        let mut returned_destination = destination;
        match self.mode {
            ReceiptMode::InvalidDigest => checkpoint.confirmed_prefix_sha256 = "invalid".into(),
            ReceiptMode::WrongOwner => checkpoint.staging_owner = TransferId::new(),
            ReceiptMode::OlderCheckpoint => checkpoint.confirmed_bytes = 30,
            ReceiptMode::WrongDestination => {
                returned_destination = FileEndpoint::Local {
                    path: "other.bin".into(),
                };
            }
            ReceiptMode::Valid
            | ReceiptMode::Advance
            | ReceiptMode::CancelRace
            | ReceiptMode::PendingCleanup
            | ReceiptMode::FailureAdvance
            | ReceiptMode::FailureConflict
            | ReceiptMode::FailureInvalidSource => {}
        }
        Err(TransferExecutionError::Cancelled {
            checkpoint: Some(checkpoint),
            actual_destination: Some(returned_destination),
        })
    }
}

/// 用一秒 idle 和独立仓储触发真实队列超时，避免修改 Tokio 全局时间。
fn fixture(mode: ReceiptMode) -> (TransferService, Arc<IdleBackend>, TransferRequest) {
    let backend = Arc::new(IdleBackend {
        mode,
        attempts: AtomicUsize::new(0),
        received: Mutex::new(Vec::new()),
        resumed: Notify::new(),
        release: Notify::new(),
        idle_cancelled: Notify::new(),
        release_cancel_result: Notify::new(),
    });
    let options = TransferQueueOptions {
        idle_timeout: Duration::from_secs(1),
        recovery_delay: Duration::from_millis(1),
        ..TransferQueueOptions::default()
    };
    let service = TransferService::new(vec![backend.clone()], Handle::current(), options).unwrap();
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: "source.bin".into(),
        },
        destination: FileEndpoint::Local {
            path: "target.bin".into(),
        },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    (service, backend, request)
}

/// 查询权威任务状态并设定期限，避免以固定延迟推测 worker 是否收尾。
async fn wait_status(service: &TransferService, id: &TransferId, status: TransferStatus) {
    timeout(Duration::from_secs(5), async {
        while service.get(id).await.unwrap().status != status {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("transfer status deadline");
}

/// 最后同步可能领先可合并进度帧，自动续传前仍须记录真实字节与完整断点。
#[tokio::test]
async fn idle_checkpoint_from_cancel_receipt_is_persisted_and_resumed() {
    let mut tasks = Vec::new();
    for (mode, checkpoint_bytes, progress_bytes) in [
        (ReceiptMode::Valid, 60, 60),
        (ReceiptMode::Advance, 60, 60),
        (ReceiptMode::OlderCheckpoint, 30, 30),
    ] {
        let (service, backend, request) = fixture(mode);
        let id = service.enqueue(request).await.unwrap().id;
        tasks.push((service, backend, id, checkpoint_bytes, progress_bytes));
    }
    for (service, backend, id, checkpoint_bytes, progress_bytes) in tasks {
        timeout(Duration::from_secs(5), backend.resumed.notified())
            .await
            .expect("automatic resume deadline");
        let checkpoint = backend.received.lock().unwrap()[0].clone();
        assert_eq!(checkpoint.confirmed_bytes, checkpoint_bytes);
        assert_eq!(checkpoint.staging_owner, id);
        let summary = service.get(&id).await.unwrap();
        assert_eq!(summary.bytes_transferred, progress_bytes);
        assert!(summary.automatic_recovery_used);
        let saved = service
            .inner
            .recovery_repository
            .list(CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(saved[0].id, id);
        assert_eq!(saved[0].checkpoint, Some(checkpoint));
        backend.release.notify_one();
        wait_status(&service, &id, TransferStatus::Succeeded).await;
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
        service.shutdown().await.unwrap();
    }
}

/// 失败回执的最终断点可能领先 watch，恢复前必须持久化并交给下一轮后端。
///
/// @author kongweiguang
#[tokio::test]
async fn idle_checkpoint_from_resumable_failure_is_persisted_and_resumed() {
    let (service, backend, request) = fixture(ReceiptMode::FailureAdvance);
    let id = service.enqueue(request).await.unwrap().id;
    timeout(Duration::from_secs(5), backend.idle_cancelled.notified())
        .await
        .expect("idle cancellation deadline");
    {
        let records = service.inner.records.lock().await;
        let checkpoint = records.get(&id).unwrap().checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.confirmed_bytes, 30);
        assert_eq!(checkpoint.confirmed_prefix_sha256, "d".repeat(64));
    }
    backend.release_cancel_result.notify_one();
    timeout(Duration::from_secs(5), backend.resumed.notified())
        .await
        .expect("automatic resume deadline");
    let checkpoint = backend.received.lock().unwrap()[0].clone();
    assert_eq!(checkpoint.confirmed_bytes, 60);
    assert_eq!(checkpoint.confirmed_prefix_sha256, "c".repeat(64));
    assert_eq!(checkpoint.staging_owner, id);
    let summary = service.get(&id).await.unwrap();
    assert_eq!(summary.bytes_transferred, 60);
    assert!(summary.automatic_recovery_used);
    let saved = service
        .inner
        .recovery_repository
        .list(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(saved[0].id, id);
    assert_eq!(saved[0].checkpoint, Some(checkpoint));
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    service.shutdown().await.unwrap();
}

/// 内容冲突或源失效会撤销旧断点，用户同时取消时也不能显示可续传。
///
/// @author kongweiguang
#[tokio::test]
async fn idle_failure_revokes_previous_checkpoint_without_automatic_recovery() {
    for (mode, kind, user_cancelled) in [
        (
            ReceiptMode::FailureConflict,
            TransferFailureKind::Conflict,
            false,
        ),
        (
            ReceiptMode::FailureInvalidSource,
            TransferFailureKind::InvalidSource,
            false,
        ),
        (
            ReceiptMode::FailureConflict,
            TransferFailureKind::Conflict,
            true,
        ),
    ] {
        let (service, backend, request) = fixture(mode);
        let id = service.enqueue(request).await.unwrap().id;
        timeout(Duration::from_secs(5), backend.idle_cancelled.notified())
            .await
            .expect("idle cancellation deadline");
        {
            let records = service.inner.records.lock().await;
            let checkpoint = records.get(&id).unwrap().checkpoint.as_ref().unwrap();
            assert_eq!(checkpoint.confirmed_bytes, 30);
        }
        if user_cancelled {
            assert_eq!(
                service.cancel(&id).await.unwrap().status,
                TransferStatus::Cancelling
            );
        }
        backend.release_cancel_result.notify_one();
        let status = if user_cancelled {
            TransferStatus::Cancelled
        } else {
            TransferStatus::Failed
        };
        wait_status(&service, &id, status).await;
        let summary = service.get(&id).await.unwrap();
        if !user_cancelled {
            assert_eq!(summary.failure_kind, Some(kind));
        }
        assert!(!summary.resumable);
        assert!(!summary.automatic_recovery_used);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        service.shutdown().await.unwrap();
    }
}

/// idle 取消后、回执前收到用户取消时，必须保留新断点并终止自动重连。
///
/// @author kongweiguang
#[tokio::test]
async fn idle_checkpoint_user_cancel_keeps_late_checkpoint_and_can_resume() {
    let (service, backend, request) = fixture(ReceiptMode::CancelRace);
    let id = service.enqueue(request).await.unwrap().id;
    timeout(Duration::from_secs(5), backend.idle_cancelled.notified())
        .await
        .expect("idle cancellation deadline");
    assert_eq!(
        service.cancel(&id).await.unwrap().status,
        TransferStatus::Cancelling
    );
    backend.release_cancel_result.notify_one();
    wait_status(&service, &id, TransferStatus::Cancelled).await;
    let cancelled = service.get(&id).await.unwrap();
    assert!(cancelled.resumable);
    assert_eq!(cancelled.bytes_transferred, 60);
    assert!(!cancelled.automatic_recovery_used);
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .inner
            .recovery_repository
            .list(CancellationToken::new())
            .await
            .unwrap()[0]
            .checkpoint
            .as_ref()
            .unwrap()
            .confirmed_bytes,
        60
    );
    assert_eq!(service.resume(&id).await.unwrap().id, id);
    timeout(Duration::from_secs(5), backend.resumed.notified())
        .await
        .expect("manual resume deadline");
    assert_eq!(backend.received.lock().unwrap()[0].confirmed_bytes, 60);
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.unwrap();
}

/// 假摘要、错误 owner 和不同目标均不得使无断点的停滞任务进入自动续传。
#[tokio::test]
async fn idle_checkpoint_rejects_untrusted_cancel_receipts() {
    let mut tasks = Vec::new();
    for mode in [
        ReceiptMode::InvalidDigest,
        ReceiptMode::WrongOwner,
        ReceiptMode::WrongDestination,
    ] {
        let (service, backend, request) = fixture(mode);
        let id = service.enqueue(request).await.unwrap().id;
        tasks.push((service, backend, id));
    }
    for (service, backend, id) in tasks {
        wait_status(&service, &id, TransferStatus::Failed).await;
        let summary = service.get(&id).await.unwrap();
        assert_eq!(summary.failure_kind, Some(TransferFailureKind::IdleTimeout));
        assert!(!summary.resumable);
        assert_eq!(summary.bytes_transferred, 40);
        assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
        service.shutdown().await.unwrap();
    }
}

/// idle 收到待清理失败必须先保留原 cleanup owner，不能转成自动恢复。
#[tokio::test]
async fn idle_checkpoint_preserves_pending_cleanup_failure() {
    let (service, backend, request) = fixture(ReceiptMode::PendingCleanup);
    let id = service.enqueue(request).await.unwrap().id;
    wait_status(&service, &id, TransferStatus::Failed).await;
    let summary = service.get(&id).await.unwrap();
    assert_eq!(summary.failure_kind, Some(TransferFailureKind::Connection));
    assert!(summary.cleanup_pending);
    assert!(!summary.automatic_recovery_used);
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}
