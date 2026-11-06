//! 同记录续传的状态、所有权与写前门禁回归。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FileRevision, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus,
    },
    identity::{HostId, TransferId},
};
use tokio::{
    sync::Notify,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use super::super::{
    InMemoryTransferRecoveryRepository, SftpTransferPolicySnapshot, SftpTransferPolicyStore,
    ShutdownWorkerOwner, TransferBackend, TransferCheckpoint, TransferCleanupState,
    TransferExecutionContext, TransferExecutionError, TransferExecutionFailure, TransferPhase,
    TransferProgressUpdate, TransferQueueOptions, TransferRecoveryRecord,
    TransferRecoveryRepository, TransferResourceCleanup, TransferService, TransferStagingOwnership,
};
use crate::{
    AppError, AppResult, PublicErrorCode,
    platform::external_launch::{ExternalTargetLease, ExternalTargetLeaseOwner},
    preferences::SftpPerformanceSettings,
    transfers::{TransferExecutionResult, TransferOrigin, TransferRecoveryPage},
};

struct ResumeBackend {
    attempts: AtomicUsize,
    initial_digest: Option<String>,
    initial_checkpoint_bytes: u64,
    first_started: Notify,
    continued: Notify,
    release: Notify,
    resumed: Mutex<
        Vec<(
            TransferId,
            TransferCheckpoint,
            Option<SftpTransferPolicySnapshot>,
        )>,
    >,
}

#[async_trait]
impl TransferBackend for ResumeBackend {
    /// 受控后端只验证队列约束，不执行任何真实文件或网络操作。
    fn supports(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 固定使用 SFTP 策略以验证设置变化不会替换原任务快照。
    fn uses_sftp_policy(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 首轮发布可核对暂存并等待暂停；续传在确认全部字节后提供完整摘要。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let destination = FileEndpoint::Local {
            path: "target (1).bin".into(),
        };
        context
            .progress
            .begin_transfer(Some(100))
            .await
            .expect("begin transfer");
        if let Some(checkpoint) = context.resume_from.clone() {
            self.resumed.lock().expect("test mutex").push((
                context.transfer_id.clone(),
                checkpoint.clone(),
                context.sftp_policy,
            ));
            self.continued.notify_one();
            tokio::select! {
                _ = self.release.notified() => {}
                _ = context.cancellation.cancelled() => return Err(TransferExecutionError::Cancelled {
                    checkpoint: context.resume_from,
                    actual_destination: Some(destination),
                }),
            }
            context.progress.report(TransferProgressUpdate {
                confirmed_bytes: 100,
                total_bytes: Some(100),
                phase: TransferPhase::Transferring,
                actual_destination: Some(destination.clone()),
                checkpoint: None,
                activity_sequence: 1,
            });
            let final_digest = checkpoint
                .source_revision
                .content_digest
                .clone()
                .unwrap_or_else(|| {
                    if checkpoint.confirmed_bytes == 100 {
                        checkpoint.confirmed_prefix_sha256.clone()
                    } else {
                        "d".repeat(64)
                    }
                });
            let mut final_checkpoint = checkpoint;
            final_checkpoint.confirmed_bytes = 100;
            final_checkpoint.confirmed_prefix_sha256 = final_digest.clone();
            final_checkpoint.source_revision.content_digest = Some(final_digest);
            context
                .progress
                .begin_commit(destination.clone(), 100, Some(100), Some(final_checkpoint))
                .await
                .expect("commit permit");
            context
                .progress
                .record_committed_entry(100)
                .await
                .expect("commit receipt");
            context
                .progress
                .finish_commit()
                .await
                .expect("commit barrier");
            return Ok(TransferExecutionResult {
                actual_destination: destination,
                total_bytes: Some(100),
                skipped: false,
                unchanged: false,
            });
        }
        let checkpoint = TransferCheckpoint {
            source_revision: FileRevision {
                size: 100,
                modified: None,
                permissions: None,
                content_digest: self.initial_digest.clone(),
            },
            confirmed_bytes: self.initial_checkpoint_bytes,
            confirmed_prefix_sha256: "c".repeat(64),
            staging_owner: context.transfer_id.clone(),
            staging_destination: FileEndpoint::Local {
                path: ".target.partial".into(),
            },
            staging_token: "a".repeat(32),
            actual_destination: destination.clone(),
        };
        context
            .progress
            .register_staging(TransferStagingOwnership {
                staging_destination: checkpoint.staging_destination.clone(),
                actual_destination: destination.clone(),
                staging_owner: checkpoint.staging_owner.clone(),
                staging_token: checkpoint.staging_token.clone(),
            })
            .await
            .expect("register staging");
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: self.initial_checkpoint_bytes,
            total_bytes: Some(100),
            phase: TransferPhase::Transferring,
            actual_destination: Some(destination.clone()),
            checkpoint: Some(checkpoint.clone()),
            activity_sequence: 1,
        });
        self.first_started.notify_one();
        context.cancellation.cancelled().await;
        Err(TransferExecutionError::Cancelled {
            checkpoint: Some(checkpoint),
            actual_destination: Some(destination),
        })
    }
}

#[derive(Default)]
struct SwitchableRepository {
    inner: InMemoryTransferRecoveryRepository,
    fail_writes: AtomicBool,
}

#[async_trait]
impl TransferRecoveryRepository for SwitchableRepository {
    /// 读取仍交给真实内存实现，以隔离写入门禁的失败场景。
    async fn list(&self, token: CancellationToken) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.inner.list(token).await
    }
    /// 在测试指定时拒绝完整保存，确保失败不会启动新 worker。
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        token: CancellationToken,
    ) -> AppResult<()> {
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("受控恢复写入失败"));
        }
        self.inner.upsert(record, token).await
    }
    /// 删除沿用内存仓储，避免替身改变生命周期语义。
    async fn remove(&self, id: &TransferId, token: CancellationToken) -> AppResult<()> {
        self.inner.remove(id, token).await
    }
    /// 显式代理归档接口，避免替身屏蔽业务约束。
    async fn archive(&self, id: &TransferId, token: CancellationToken) -> AppResult<()> {
        self.inner.archive(id, token).await
    }
    /// 显式代理归档分页，保持仓储契约完整。
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        token: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        self.inner.list_archived(cursor, limit, token).await
    }
}

/// 每项测试拥有独立的后端闸门和仓储，不触碰用户配置或共享目录。
///
/// @author kongweiguang
fn fixture(
    shutdown_timeout: Duration,
) -> (
    TransferService,
    Arc<ResumeBackend>,
    Arc<SwitchableRepository>,
    TransferRequest,
) {
    fixture_with_checkpoint(shutdown_timeout, Some("b".repeat(64)), 40)
}

/// 独立指定初始摘要及进度，覆盖旧断点和到达文件末尾但尚未读到 EOF 的断点。
///
/// @author kongweiguang
fn fixture_with_checkpoint(
    shutdown_timeout: Duration,
    initial_digest: Option<String>,
    initial_checkpoint_bytes: u64,
) -> (
    TransferService,
    Arc<ResumeBackend>,
    Arc<SwitchableRepository>,
    TransferRequest,
) {
    let backend = Arc::new(ResumeBackend {
        attempts: AtomicUsize::new(0),
        initial_digest,
        initial_checkpoint_bytes,
        first_started: Notify::new(),
        continued: Notify::new(),
        release: Notify::new(),
        resumed: Mutex::new(Vec::new()),
    });
    let repository = Arc::new(SwitchableRepository::default());
    let options = TransferQueueOptions {
        shutdown_timeout,
        ..TransferQueueOptions::default()
    };
    let service = TransferService::new_with_recovery_repository(
        vec![backend.clone()],
        tokio::runtime::Handle::current(),
        options,
        SftpTransferPolicyStore::new(SftpPerformanceSettings::default()).expect("policy"),
        repository.clone(),
        None,
    )
    .expect("queue");
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: "source.bin".into(),
        },
        destination: FileEndpoint::Local {
            path: "target.bin".into(),
        },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Rename,
        idle_timeout_seconds: None,
    };
    (service, backend, repository, request)
}

/// 等待特定真实状态，避免以广播时间或固定延迟推断后台线程是否执行。
///
/// @author kongweiguang
async fn wait_status(service: &TransferService, id: &TransferId, status: TransferStatus) {
    timeout(Duration::from_secs(3), async {
        loop {
            if service.get(id).await.expect("record").status == status {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("status deadline");
}

/// 确认指定断点已经进入队列后再取消，避免测试调度覆盖真实的进度确认边界。
///
/// @author kongweiguang
async fn pause(
    service: &TransferService,
    backend: &ResumeBackend,
    request: TransferRequest,
    lease: Option<&ExternalTargetLease>,
) -> TransferId {
    let task = match lease {
        Some(lease) => service.enqueue_with_external_target(request, lease).await,
        None => service.enqueue(request).await,
    }
    .expect("enqueue");
    backend.first_started.notified().await;
    timeout(Duration::from_secs(3), async {
        loop {
            if service
                .get(&task.id)
                .await
                .expect("record")
                .bytes_transferred
                == backend.initial_checkpoint_bytes
            {
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("checkpoint deadline");
    service.cancel(&task.id).await.expect("cancel");
    wait_status(service, &task.id, TransferStatus::Cancelled).await;
    task.id
}

/// 没有完整摘要的部分断点和 EOF 前完整长度断点均须保留、续传并在提交时补全摘要。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_accepts_checkpoints_without_complete_source_digest() {
    for confirmed_bytes in [40, 100] {
        let (service, backend, repository, request) =
            fixture_with_checkpoint(Duration::from_secs(2), None, confirmed_bytes);
        let id = pause(&service, &backend, request, None).await;
        assert!(service.get(&id).await.unwrap().resumable);
        let saved = repository.list(CancellationToken::new()).await.unwrap();
        let checkpoint = saved[0].checkpoint.as_ref().unwrap();
        assert_eq!(checkpoint.confirmed_bytes, confirmed_bytes);
        assert_eq!(checkpoint.source_revision.content_digest, None);

        assert_eq!(service.resume(&id).await.unwrap().id, id);
        timeout(Duration::from_secs(2), backend.continued.notified())
            .await
            .expect("resume deadline");
        let resumed = backend.resumed.lock().unwrap()[0].1.clone();
        assert_eq!(resumed, *checkpoint);
        backend.release.notify_one();
        wait_status(&service, &id, TransferStatus::Succeeded).await;
        service.shutdown().await.unwrap();
    }
}

/// 旧完整摘要、元数据和同一偏移前缀一经确认不得被进度或回执替换。
///
/// @author kongweiguang
#[tokio::test]
async fn checkpoint_updates_cannot_replace_confirmed_evidence() {
    let (service, backend, _, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    {
        let mut records = service.inner.records.lock().await;
        let record = records.get_mut(&id).unwrap();
        let original = record.checkpoint.clone().unwrap();
        assert!(super::super::checkpoint_can_advance(record, &original, 40));

        let mut candidates = Vec::new();
        let mut downgraded = original.clone();
        downgraded.source_revision.content_digest = None;
        candidates.push(downgraded);
        let mut replaced = original.clone();
        replaced.source_revision.content_digest = Some("d".repeat(64));
        candidates.push(replaced);
        let mut changed_metadata = original.clone();
        changed_metadata.source_revision.modified = Some("different".into());
        candidates.push(changed_metadata);
        let mut changed_owner = original.clone();
        changed_owner.staging_owner = TransferId::new();
        candidates.push(changed_owner);
        let mut changed_token = original.clone();
        changed_token.staging_token = "f".repeat(32);
        candidates.push(changed_token);
        let mut changed_prefix = original.clone();
        changed_prefix.confirmed_prefix_sha256 = "d".repeat(64);
        candidates.push(changed_prefix);
        let mut regressed = original.clone();
        regressed.confirmed_bytes = 39;
        candidates.push(regressed);
        for candidate in candidates {
            assert!(!super::super::checkpoint_can_advance(
                record, &candidate, 40
            ));
        }

        record
            .checkpoint
            .as_mut()
            .unwrap()
            .source_revision
            .content_digest = None;
        assert!(!super::super::checkpoint_can_advance(record, &original, 40));
    }
    service.shutdown().await.unwrap();
}

/// 续传尚未启动时再取消必须保留原断点，等待该轮 worker 退出后仍可继续。
///
/// @author kongweiguang
#[tokio::test]
async fn cancelling_queued_resume_keeps_checkpoint_for_next_resume() {
    let (service, backend, _, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let checkpoint = service
        .inner
        .records
        .lock()
        .await
        .get(&id)
        .unwrap()
        .checkpoint
        .clone()
        .unwrap();

    let (queued, cancelled) = tokio::task::unconstrained(async {
        let queued = service.resume(&id).await.expect("first resume");
        let cancelled = service.cancel(&id).await.expect("queued cancellation");
        (queued, cancelled)
    })
    .await;
    assert_eq!(queued.id, id);
    assert_eq!(queued.status, TransferStatus::Queued);
    assert!(!queued.retryable && !queued.resumable);
    assert_eq!(cancelled.id, id);
    assert_eq!(cancelled.status, TransferStatus::Cancelled);
    assert!(cancelled.retryable && cancelled.resumable);
    assert_eq!(
        (cancelled.bytes_transferred, cancelled.total_bytes),
        (40, Some(100))
    );
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .inner
            .records
            .lock()
            .await
            .get(&id)
            .unwrap()
            .checkpoint,
        Some(checkpoint.clone())
    );

    let resumed = service.resume(&id).await.expect("second resume");
    assert_eq!(resumed.id, id);
    assert_eq!(resumed.status, TransferStatus::Queued);
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .expect("resumed worker deadline");
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    assert_eq!(backend.resumed.lock().unwrap()[0].1, checkpoint);
    assert_eq!(service.list().await.len(), 1);
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.expect("join");
}

/// 并发继续只启动一轮，且原 ID、顺序、源配置与暂存 owner 始终不变。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_preserves_identity_checkpoint_and_frozen_policy_under_concurrency() {
    let (service, backend, repository, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let (sequence, checkpoint, policy) = {
        let mut records = service.inner.records.lock().await;
        let record = records.get_mut(&id).expect("paused record");
        record.origin = Some(TransferOrigin::SharedWorkbench);
        (
            record.sequence,
            record.checkpoint.clone().unwrap(),
            record.sftp_policy,
        )
    };
    service
        .inner
        .sftp_policy
        .update(SftpPerformanceSettings {
            idle_timeout_seconds: 240,
            ..SftpPerformanceSettings::default()
        })
        .unwrap();
    let (first, second) = tokio::join!(service.resume(&id), service.resume(&id));
    assert_eq!(first.unwrap().id, id);
    assert_eq!(second.unwrap().id, id);
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .unwrap();
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 2);
    let observed = backend.resumed.lock().unwrap().clone();
    assert_eq!(observed, vec![(id.clone(), checkpoint.clone(), policy)]);
    assert_eq!(
        service
            .inner
            .records
            .lock()
            .await
            .get(&id)
            .unwrap()
            .sequence,
        sequence
    );
    assert_eq!(service.list().await.len(), 1);
    assert_eq!(
        service
            .list_for_origin(&TransferOrigin::SharedWorkbench)
            .await
            .len(),
        1
    );
    let saved = repository.list(CancellationToken::new()).await.unwrap();
    assert_eq!(saved[0].origin, Some(TransferOrigin::SharedWorkbench));
    service.cancel(&id).await.unwrap();
    wait_status(&service, &id, TransferStatus::Cancelled).await;
    assert_eq!(service.resume(&id).await.unwrap().id, id);
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .unwrap();
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 3);
    assert_eq!(
        backend.resumed.lock().unwrap().last().unwrap().1,
        checkpoint
    );
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.expect("join");
}

/// 超时和调用 future 中止均保留同一旧句柄；迟到回执结束后才可启动新一轮。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_retains_delayed_worker_across_timeout_and_caller_cancellation() {
    let (service, backend, _, request) = fixture(Duration::from_millis(180));
    let id = pause(&service, &backend, request, None).await;
    let previous = service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker
        .take()
        .unwrap();
    previous.await.unwrap();
    let gate = Arc::new(Notify::new());
    let worker_gate = gate.clone();
    let worker_service = service.clone();
    let worker_id = id.clone();
    let late = tokio::spawn(async move {
        worker_gate.notified().await;
        worker_service
            .finish_failure(
                &worker_id,
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    false,
                    false,
                    None,
                    None,
                    TransferCleanupState::Released,
                ),
            )
            .await;
    });
    service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker = Some(late);
    assert_eq!(
        service.resume(&id).await.unwrap_err().public_code(),
        PublicErrorCode::Timeout
    );
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    let resume_service = service.clone();
    let resume_id = id.clone();
    let waiting = tokio::spawn(async move { resume_service.resume(&resume_id).await });
    sleep(Duration::from_millis(20)).await;
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    assert!(service.inner.shutdown_workers.lock().await.iter().any(|owner| matches!(owner, ShutdownWorkerOwner::Running { id: owner_id, .. } if owner_id == &id)));
    gate.notify_one();
    assert_eq!(service.resume(&id).await.unwrap().id, id);
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .unwrap();
    assert_eq!(
        service.get(&id).await.unwrap().status,
        TransferStatus::Transferring
    );
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.unwrap();
}

/// 写前持久化失败保留原终态和断点，显式重试成功前不能创建新 worker。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_persistence_failure_leaves_original_record_unchanged() {
    let (service, backend, repository, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let previous = service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker
        .take()
        .unwrap();
    previous.await.unwrap();
    let before = service.get(&id).await.unwrap();
    repository.fail_writes.store(true, Ordering::SeqCst);
    assert!(service.resume(&id).await.is_err());
    assert_eq!(service.get(&id).await.unwrap(), before);
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    repository.fail_writes.store(false, Ordering::SeqCst);
    assert_eq!(service.resume(&id).await.unwrap().id, id);
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .unwrap();
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.unwrap();
}

struct PendingCleanup;

#[async_trait]
impl TransferResourceCleanup for PendingCleanup {
    /// 清理替身保留 owner 身份，测试只验证续传入口的拒绝。
    async fn cleanup_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 清理未完成、断点损坏和已有后继分别拒绝续传，原执行次数保持不变。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_rejects_pending_cleanup_untrusted_checkpoint_and_successor() {
    let (service, backend, _, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let previous = service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker
        .take()
        .unwrap();
    previous.await.unwrap();
    {
        let mut records = service.inner.records.lock().await;
        records.get_mut(&id).unwrap().cleanup_owner = Some(Arc::new(PendingCleanup));
    }
    assert!(service.resume(&id).await.is_err());
    {
        let mut records = service.inner.records.lock().await;
        let record = records.get_mut(&id).unwrap();
        record.cleanup_owner = None;
        record.checkpoint.as_mut().unwrap().staging_token = "invalid".into();
    }
    assert!(service.resume(&id).await.is_err());
    {
        let mut records = service.inner.records.lock().await;
        let record = records.get_mut(&id).unwrap();
        record.checkpoint.as_mut().unwrap().staging_token = "a".repeat(32);
        record.task.set_retry_successor(TransferId::new()).unwrap();
    }
    assert!(service.resume(&id).await.is_err());
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    service.shutdown().await.unwrap();
}

/// join 失败的 owner 需持续保留，后续续传和关闭都不能把异常当作已回收。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_retains_worker_join_failure() {
    let (service, backend, _, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let previous = service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker
        .take()
        .unwrap();
    previous.await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker = Some(tokio::spawn(async {
        panic!("受控 worker 退出异常");
    }));
    assert_eq!(
        service.resume(&id).await.unwrap_err().public_code(),
        PublicErrorCode::Infrastructure
    );
    assert_eq!(
        service.resume(&id).await.unwrap_err().public_code(),
        PublicErrorCode::Infrastructure
    );
    assert!(service.inner.shutdown_workers.lock().await.iter().any(|owner| matches!(owner, ShutdownWorkerOwner::JoinFailed { id: owner_id, .. } if owner_id == &id)));
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    assert!(service.shutdown().await.is_err());
}

/// 没有句柄也没有完成回执时不能假定旧线程已经退出。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_rejects_missing_worker_completion_evidence() {
    let (service, backend, _, request) = fixture(Duration::from_secs(2));
    let id = pause(&service, &backend, request, None).await;
    let previous = service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .worker
        .take()
        .unwrap();
    previous.await.unwrap();
    service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .attempt_finished = false;
    assert_eq!(
        service.resume(&id).await.unwrap_err().public_code(),
        PublicErrorCode::Conflict
    );
    assert_eq!(backend.attempts.load(Ordering::SeqCst), 1);
    service
        .inner
        .records
        .lock()
        .await
        .get_mut(&id)
        .unwrap()
        .attempt_finished = true;
    service.shutdown().await.unwrap();
}

struct LeaseOwner;

impl ExternalTargetLeaseOwner for LeaseOwner {
    /// 所有测试 scope 均可重新 retain，隔离 HostId 校验本身。
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        Ok(())
    }
    /// 测试 scope 无外部资源，仅遵守释放接口。
    fn release_scope(&self, _host_id: &HostId) {}
}

/// 外部目标必须重新提供精确原 HostId，错误目标和缺失 lease 均不能启动续传。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_requires_matching_external_target_lease() {
    let (service, backend, _, mut request) = fixture(Duration::from_secs(2));
    let host = HostId::parse("external:resume-test").unwrap();
    let lease = ExternalTargetLease::new_registered(host.clone(), Arc::new(LeaseOwner)).unwrap();
    let other = ExternalTargetLease::new_registered(
        HostId::parse("external:other").unwrap(),
        Arc::new(LeaseOwner),
    )
    .unwrap();
    request.source = FileEndpoint::remote(host, "/source.bin").unwrap();
    let id = pause(&service, &backend, request, Some(&lease)).await;
    assert!(service.resume(&id).await.is_err());
    assert!(
        service
            .resume_with_external_target(&id, &other)
            .await
            .is_err()
    );
    assert_eq!(
        service
            .resume_with_external_target(&id, &lease)
            .await
            .unwrap()
            .id,
        id
    );
    timeout(Duration::from_secs(2), backend.continued.notified())
        .await
        .unwrap();
    backend.release.notify_one();
    wait_status(&service, &id, TransferStatus::Succeeded).await;
    service.shutdown().await.unwrap();
}
