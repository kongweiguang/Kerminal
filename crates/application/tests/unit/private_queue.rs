//! 传输提交许可与取消的异步竞态回归测试。
//!
//! @author kongweiguang

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use super::{TransferRecord, prune_history};
use crate::transfers::{
    TransferBackend, TransferCleanupState, TransferExecutionContext, TransferExecutionError,
    TransferExecutionFailure, TransferExecutionResult, TransferPhase, TransferProgressUpdate,
    TransferQueueOptions, TransferResourceCleanup, TransferService,
};
use crate::{
    AppError, AppResult, PublicErrorCode,
    platform::external_launch::{ExternalTargetLease, ExternalTargetLeaseOwner},
    preferences::SftpPerformanceSettings,
};
use async_trait::async_trait;
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest,
        TransferStatus, TransferTask,
    },
    identity::{HostId, TransferId},
};
use tokio::{
    runtime::Handle,
    sync::{Notify, oneshot},
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum BackendMode {
    NoChanges,
    NoChangesAfterNoEffectItem,
    CancelAfterNoChangesBarrier,
    CancelDuringTraversal,
    PanicBeforeCommit,
    FailBeforeCommit,
    FailWithPendingCleanup,
    CancelBeforeAcknowledgment,
    CancelAfterAcknowledgment,
    LoseCommitReceipt,
    CancelBetweenItems,
    FailAfterFirstReceipt,
    CommitThenRetainCleanupOwner,
    ProgressTelemetry,
    PhaseTransitions,
}

struct FakeTransferBackend {
    mode: BackendMode,
    cleanup_owner: Arc<RetryableCleanup>,
    started: Notify,
    release_commit_request: Notify,
    commit_request_enqueued: Mutex<Option<oneshot::Sender<()>>>,
    acknowledgment_received: Notify,
    release_after_acknowledgment: Notify,
    first_receipt_recorded: Notify,
    release_after_first_receipt: Notify,
    no_changes_finalized: Notify,
    release_after_nochanges: Notify,
    verification_reported: Notify,
    release_after_verification: Notify,
    transfer_reported: Notify,
    release_after_transfer: Notify,
    rename_applied: AtomicBool,
}

struct RetryableCleanup {
    attempts: AtomicUsize,
    cleaned: AtomicBool,
}

#[async_trait]
impl TransferResourceCleanup for RetryableCleanup {
    /// 第一次模拟超时，第二次确认同一 owner 的资源已经释放。
    async fn cleanup_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        if self.attempts.fetch_add(1, Ordering::AcqRel) == 0 {
            return Err(AppError::timeout("受控清理超时"));
        }
        self.cleaned.store(true, Ordering::Release);
        Ok(())
    }
}

impl FakeTransferBackend {
    /// 为每项竞态测试建立独立闸门与传输结果记录，避免测试并行时共享状态互相影响。
    ///
    /// @author kongweiguang
    fn new(mode: BackendMode) -> (Self, oneshot::Receiver<()>) {
        let (commit_request_enqueued, request_receiver) = oneshot::channel();
        (
            Self {
                mode,
                cleanup_owner: Arc::new(RetryableCleanup {
                    attempts: AtomicUsize::new(0),
                    cleaned: AtomicBool::new(false),
                }),
                started: Notify::new(),
                release_commit_request: Notify::new(),
                commit_request_enqueued: Mutex::new(Some(commit_request_enqueued)),
                acknowledgment_received: Notify::new(),
                release_after_acknowledgment: Notify::new(),
                first_receipt_recorded: Notify::new(),
                release_after_first_receipt: Notify::new(),
                no_changes_finalized: Notify::new(),
                release_after_nochanges: Notify::new(),
                verification_reported: Notify::new(),
                release_after_verification: Notify::new(),
                transfer_reported: Notify::new(),
                release_after_transfer: Notify::new(),
                rename_applied: AtomicBool::new(false),
            },
            request_receiver,
        )
    }
}

#[async_trait]
impl TransferBackend for FakeTransferBackend {
    /// 测试适配器仅模拟 Remote 到 Local 的传输请求。
    fn supports(&self, request: &TransferRequest) -> bool {
        matches!(request.source, FileEndpoint::Remote { .. })
            && matches!(request.destination, FileEndpoint::Local { .. })
    }

    /// 测试后端模拟 SFTP 策略使用者，以验证新任务捕获最新阈值而旧摘要保持不变。
    fn uses_sftp_policy(&self, _request: &TransferRequest) -> bool {
        true
    }

    /// 用受控提交许可模拟原子 rename 前后边界，并可模拟成功后丢失服务端回执。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let destination = context.request.destination.clone();
        if matches!(self.mode, BackendMode::FailWithPendingCleanup) {
            return Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    true,
                    false,
                    None,
                    Some(destination),
                    TransferCleanupState::pending(self.cleanup_owner.clone()),
                ),
            ));
        }
        if matches!(
            self.mode,
            BackendMode::NoChanges
                | BackendMode::NoChangesAfterNoEffectItem
                | BackendMode::CancelAfterNoChangesBarrier
                | BackendMode::CancelDuringTraversal
        ) {
            context
                .progress
                .begin_transfer(Some(0))
                .await
                .expect("directory traversal starts before final classification");
            context.progress.report(TransferProgressUpdate {
                confirmed_bytes: 0,
                total_bytes: Some(0),
                phase: TransferPhase::Transferring,
                actual_destination: Some(destination.clone()),
                checkpoint: None,
                activity_sequence: 0,
            });
            if matches!(
                self.mode,
                BackendMode::NoChangesAfterNoEffectItem | BackendMode::CancelAfterNoChangesBarrier
            ) {
                context
                    .progress
                    .begin_commit(destination.clone(), 0, Some(0), None)
                    .await
                    .expect("an item may enter commit before the server confirms no change");
                let summary = context
                    .progress
                    .confirm_no_change()
                    .await
                    .expect("an explicit no-side-effect response releases the item permit");
                assert_eq!(summary.committed_entries(), 0);
            }
            if matches!(
                self.mode,
                BackendMode::NoChanges
                    | BackendMode::NoChangesAfterNoEffectItem
                    | BackendMode::CancelAfterNoChangesBarrier
            ) {
                let summary = context
                    .progress
                    .finish_no_changes()
                    .await
                    .expect("complete traversal can finalize without receipts");
                assert_eq!(summary.committed_entries(), 0);
                if matches!(self.mode, BackendMode::CancelAfterNoChangesBarrier) {
                    self.no_changes_finalized.notify_one();
                    self.release_after_nochanges.notified().await;
                }
                return Ok(TransferExecutionResult {
                    actual_destination: destination,
                    total_bytes: Some(0),
                    skipped: false,
                    unchanged: true,
                });
            }
            self.started.notify_one();
            context.cancellation.cancelled().await;
            return Err(TransferExecutionError::Cancelled {
                checkpoint: None,
                actual_destination: Some(destination),
            });
        }
        let total_bytes = match self.mode {
            BackendMode::CancelBetweenItems | BackendMode::FailAfterFirstReceipt => 2,
            BackendMode::NoChanges
            | BackendMode::NoChangesAfterNoEffectItem
            | BackendMode::CancelAfterNoChangesBarrier
            | BackendMode::CancelDuringTraversal => 0,
            _ => 1,
        };
        context
            .progress
            .begin_transfer(Some(total_bytes))
            .await
            .expect("transfer state is acknowledged before byte progress");
        if matches!(self.mode, BackendMode::PhaseTransitions) {
            context.progress.report(TransferProgressUpdate {
                confirmed_bytes: 0,
                total_bytes: Some(total_bytes),
                phase: TransferPhase::Verifying,
                actual_destination: Some(destination.clone()),
                checkpoint: None,
                activity_sequence: 0,
            });
            self.verification_reported.notify_one();
            self.release_after_verification.notified().await;
        }
        if matches!(self.mode, BackendMode::ProgressTelemetry) {
            sleep(Duration::from_millis(25)).await;
        }
        if matches!(self.mode, BackendMode::FailBeforeCommit) {
            return Err(TransferExecutionError::Failed(
                TransferExecutionFailure::new(
                    TransferFailureKind::Connection,
                    true,
                    false,
                    None,
                    Some(destination),
                    TransferCleanupState::Released,
                ),
            ));
        }
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: 1,
            total_bytes: Some(total_bytes),
            phase: TransferPhase::Transferring,
            actual_destination: Some(destination.clone()),
            checkpoint: None,
            activity_sequence: 0,
        });
        if matches!(self.mode, BackendMode::PhaseTransitions) {
            self.transfer_reported.notify_one();
            self.release_after_transfer.notified().await;
        }

        if matches!(self.mode, BackendMode::PanicBeforeCommit) {
            self.started.notify_one();
            panic!("受控测试后端在提交许可前 panic");
        }

        if matches!(self.mode, BackendMode::CancelBeforeAcknowledgment) {
            self.started.notify_one();
            self.release_commit_request.notified().await;
        }

        let enqueued = self
            .commit_request_enqueued
            .lock()
            .expect("test mutex is not poisoned")
            .take()
            .expect("commit request is issued once");
        let result = context.progress.request_commit(
            destination.clone(),
            total_bytes,
            Some(total_bytes),
            None,
            Some(enqueued),
        );

        if result.await.is_err() {
            return Err(TransferExecutionError::Cancelled {
                checkpoint: None,
                actual_destination: Some(destination),
            });
        }

        match self.mode {
            BackendMode::NoChanges
            | BackendMode::NoChangesAfterNoEffectItem
            | BackendMode::CancelAfterNoChangesBarrier
            | BackendMode::CancelDuringTraversal => {
                unreachable!("目录 no-change 场景已在前置路径结束")
            }
            BackendMode::PanicBeforeCommit => unreachable!("panic 场景已提前结束"),
            BackendMode::FailBeforeCommit => unreachable!("传输前失败场景已提前结束"),
            BackendMode::FailWithPendingCleanup => unreachable!("待清理场景已提前结束"),
            BackendMode::CancelBeforeAcknowledgment => {
                self.rename_applied.store(true, Ordering::Release);
            }
            BackendMode::CancelAfterAcknowledgment => {
                self.acknowledgment_received.notify_one();
                self.release_after_acknowledgment.notified().await;
                self.rename_applied.store(true, Ordering::Release);
                context
                    .progress
                    .record_committed_entry(1)
                    .await
                    .expect("queue records the confirmed target receipt");
                context
                    .progress
                    .finish_commit()
                    .await
                    .expect("all confirmed receipts pass the final barrier");
            }
            BackendMode::LoseCommitReceipt => {
                self.rename_applied.store(true, Ordering::Release);
                return Err(TransferExecutionError::Failed(
                    TransferExecutionFailure::new(
                        TransferFailureKind::CommitUnknown,
                        true,
                        true,
                        None,
                        Some(destination),
                        TransferCleanupState::Released,
                    ),
                ));
            }
            BackendMode::CancelBetweenItems => {
                self.rename_applied.store(true, Ordering::Release);
                context
                    .progress
                    .record_committed_entry(1)
                    .await
                    .expect("first directory entry receipt is retained");
                self.first_receipt_recorded.notify_one();
                self.release_after_first_receipt.notified().await;
                return Err(TransferExecutionError::Cancelled {
                    checkpoint: None,
                    actual_destination: Some(destination),
                });
            }
            BackendMode::FailAfterFirstReceipt => {
                self.rename_applied.store(true, Ordering::Release);
                context
                    .progress
                    .record_committed_entry(1)
                    .await
                    .expect("first directory entry receipt is retained");
                return Err(TransferExecutionError::Failed(
                    TransferExecutionFailure::new(
                        TransferFailureKind::Connection,
                        true,
                        false,
                        None,
                        Some(destination),
                        TransferCleanupState::Released,
                    ),
                ));
            }
            BackendMode::CommitThenRetainCleanupOwner => {
                self.rename_applied.store(true, Ordering::Release);
                context
                    .progress
                    .record_committed_entry(1)
                    .await
                    .expect("confirmed target receipt is retained before cleanup");
                context
                    .progress
                    .finish_commit()
                    .await
                    .expect("data result is linearized before best-effort cleanup");
                context
                    .progress
                    .retain_cleanup_owner(self.cleanup_owner.clone())
                    .await
                    .expect("pending cleanup owner is acknowledged by the queue");
            }
            BackendMode::ProgressTelemetry | BackendMode::PhaseTransitions => {
                self.rename_applied.store(true, Ordering::Release);
                context
                    .progress
                    .record_committed_entry(1)
                    .await
                    .expect("telemetry sample follows a confirmed target receipt");
                context
                    .progress
                    .finish_commit()
                    .await
                    .expect("telemetry sample remains visible at final barrier");
            }
        }

        Ok(TransferExecutionResult {
            actual_destination: destination,
            total_bytes: Some(total_bytes),
            skipped: false,
            unchanged: false,
        })
    }
}

struct CountingExternalTargetOwner {
    scopes: AtomicUsize,
}

impl ExternalTargetLeaseOwner for CountingExternalTargetOwner {
    /// 测试仓储只接受仍活跃的 scope，并以原子计数观察入队与清理边界。
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        if self.scopes.load(Ordering::Acquire) == 0 {
            return Err(AppError::not_found("测试临时目标已释放"));
        }
        self.scopes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// 记录 scope 释放次数，确保队列不会把一次性目标留到普通历史清理之后。
    fn release_scope(&self, _host_id: &HostId) {
        self.scopes.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 创建一个测试持有者和初始注册 scope；后续 retain/release 可被生命周期测试精确观察。
fn external_target_lease() -> (ExternalTargetLease, Arc<CountingExternalTargetOwner>) {
    let owner = Arc::new(CountingExternalTargetOwner {
        scopes: AtomicUsize::new(1),
    });
    let lease = ExternalTargetLease::new_registered(
        HostId::parse("external:files-test").expect("temporary host ID is valid"),
        owner.clone(),
    )
    .expect("registered external host creates an initial lease");
    (lease, owner)
}

/// 等待 scope 计数跨越 worker 收尾边界，避免把任务状态发布早于 cleanup 误当成释放完成。
async fn wait_for_external_scopes(owner: &CountingExternalTargetOwner, expected: usize) {
    timeout(Duration::from_secs(2), async {
        loop {
            if owner.scopes.load(Ordering::Acquire) == expected {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("external target scopes settle before the bounded test deadline");
}

/// 创建共用一个后端的服务和合法跨端点请求，便于精确控制后台 worker 时序。
fn service_for(
    mode: BackendMode,
) -> (
    TransferService,
    Arc<FakeTransferBackend>,
    TransferRequest,
    oneshot::Receiver<()>,
) {
    service_for_kind(mode, TransferKind::File)
}

/// 为目录逐项提交场景使用独立源类型，验证部分确认与 staging 字节分离。
fn service_for_kind(
    mode: BackendMode,
    kind: TransferKind,
) -> (
    TransferService,
    Arc<FakeTransferBackend>,
    TransferRequest,
    oneshot::Receiver<()>,
) {
    service_for_kind_with_timeout(mode, kind, TransferQueueOptions::default().shutdown_timeout)
}

/// 为生命周期测试注入短关闭预算，避免真实等待长达默认队列期限。
/// @author kongweiguang
fn service_for_kind_with_timeout(
    mode: BackendMode,
    kind: TransferKind,
    shutdown_timeout: Duration,
) -> (
    TransferService,
    Arc<FakeTransferBackend>,
    TransferRequest,
    oneshot::Receiver<()>,
) {
    let options = TransferQueueOptions {
        shutdown_timeout,
        ..TransferQueueOptions::default()
    };
    service_for_kind_with_options(mode, kind, options)
}

/// 让清理边界测试单独限制历史容量，而不改变传输服务的其它预算。
///
/// @author kongweiguang
fn service_for_kind_with_options(
    mode: BackendMode,
    kind: TransferKind,
    options: TransferQueueOptions,
) -> (
    TransferService,
    Arc<FakeTransferBackend>,
    TransferRequest,
    oneshot::Receiver<()>,
) {
    let (backend, request_receiver) = FakeTransferBackend::new(mode);
    let backend = Arc::new(backend);
    let service = TransferService::new(vec![backend.clone()], Handle::current(), options)
        .expect("valid queue options");
    let request = TransferRequest {
        source: FileEndpoint::remote(HostId::new(), "/source.bin").expect("valid remote path"),
        destination: FileEndpoint::Local {
            path: PathBuf::from("target.bin"),
        },
        kind,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    (service, backend, request, request_receiver)
}

/// 构造精简终态记录，让 prune_history 回归直接覆盖记录保留策略而无需运行异步后端。
///
/// @author kongweiguang
fn terminal_record(kind: TransferFailureKind, sequence: u64) -> (TransferId, TransferRecord) {
    let destination = FileEndpoint::Local {
        path: PathBuf::from("destination.bin"),
    };
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: PathBuf::from("source.bin"),
        },
        destination: destination.clone(),
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    let mut task = TransferTask::queued(TransferId::new(), request)
        .expect("test fixture uses distinct endpoints");
    task.begin_connecting().expect("queued task can connect");
    task.begin_transferring(Some(1))
        .expect("connected task can transfer");
    if kind == TransferFailureKind::CommitUnknown {
        task.record_progress(1, Some(1))
            .expect("test fixture records the outgoing byte");
        task.begin_commit(destination)
            .expect("test fixture enters the commit boundary");
    }
    task.fail(kind, "受控终态".to_owned(), true, false)
        .expect("test fixture reaches its requested failure state");
    let id = task.id().clone();
    (
        id,
        TransferRecord {
            task,
            checkpoint: None,
            staging_ownership: Vec::new(),
            sftp_policy: None,
            source_sftp: None,
            destination_sftp: None,
            phase: None,
            effective_idle_timeout_seconds: 60,
            speed_bytes_per_second: 0,
            last_progress_at: None,
            progress_sample: None,
            external_target_lease: None,
            cleanup_owner: None,
            cancellation: CancellationToken::new(),
            commit_finalized: false,
            no_changes_finalized: false,
            attempt_finished: true,
            worker: None,
            sequence,
        },
    )
}

/// 完整遍历零变更进入独立终态，提交回执摘要必须保持为空。
#[tokio::test]
async fn complete_directory_traversal_can_finish_unchanged_without_receipts() {
    let (service, _, mut request, _) =
        service_for_kind(BackendMode::NoChanges, TransferKind::Directory);
    request.conflict_policy = ConflictPolicy::Overwrite;
    let queued = service
        .enqueue(request)
        .await
        .expect("queue accepts directory transfer");
    wait_for_status(&service, &queued.id, TransferStatus::Unchanged).await;
    let summary = service
        .get(&queued.id)
        .await
        .expect("task remains queryable");
    assert_eq!(summary.commit_summary.committed_entries(), 0);
    assert_eq!(summary.commit_summary.committed_bytes(), 0);
    assert!(!summary.partial_commit);
    assert!(!summary.retryable);
}

/// MCP 查询默认空闲阈值读取当前 SFTP policy，而不是队列创建时的旧副本。
/// @author kongweiguang
#[tokio::test]
async fn default_idle_timeout_tracks_live_sftp_policy_updates() {
    let (service, _, _, _) = service_for(BackendMode::NoChanges);
    let settings = SftpPerformanceSettings {
        idle_timeout_seconds: 240,
        ..SftpPerformanceSettings::default()
    };
    service
        .inner
        .sftp_policy
        .update(settings)
        .expect("valid SFTP policy can be applied live");

    assert_eq!(service.default_idle_timeout_seconds(), 240);
}

/// 队列 worker 以记录中的入队快照准入；新任务读取热更新后的并发上限。
///
/// @author kongweiguang
#[tokio::test]
async fn queued_sftp_task_keeps_enqueue_concurrency_limit_after_update() {
    let (service, _, request, _) =
        service_for_kind(BackendMode::NoChanges, TransferKind::Directory);
    let host = request
        .source
        .host_id()
        .cloned()
        .expect("fixture source is remote");
    let old_policy = service
        .inner
        .sftp_policy
        .update(SftpPerformanceSettings {
            global_transfers: 1,
            host_transfers: 1,
            ..SftpPerformanceSettings::default()
        })
        .expect("valid initial policy is applied");
    let blocker = service
        .inner
        .sftp_policy
        .acquire_with_host_limits_at_snapshot([(host, None)], old_policy, &CancellationToken::new())
        .await
        .expect("test blocker owns the initial SFTP slot");

    let queued = service
        .enqueue(request.clone())
        .await
        .expect("first transfer is accepted");
    wait_for_status(&service, &queued.id, TransferStatus::Connecting).await;

    service
        .inner
        .sftp_policy
        .update(SftpPerformanceSettings {
            global_transfers: 2,
            host_transfers: 2,
            ..SftpPerformanceSettings::default()
        })
        .expect("new task limits are applied");
    let new_task = service
        .enqueue(request)
        .await
        .expect("second transfer is accepted with the new policy");
    wait_for_status(&service, &new_task.id, TransferStatus::Unchanged).await;
    sleep(Duration::from_millis(25)).await;

    assert_eq!(
        service
            .get(&queued.id)
            .await
            .expect("queued transfer remains available")
            .status,
        TransferStatus::Connecting,
        "an enqueue snapshot must keep the old host admission limit"
    );

    drop(blocker);
    wait_for_status(&service, &queued.id, TransferStatus::Unchanged).await;
    service
        .shutdown()
        .await
        .expect("all queue workers are joined after their permits are released");
}

/// 新任务快照读取更新后的策略，而既有任务的 idle 预算始终保持入队时的值。
/// @author kongweiguang
#[tokio::test]
async fn task_summary_freezes_effective_idle_timeout_at_enqueue() {
    let (service, _, request, _) =
        service_for_kind(BackendMode::NoChanges, TransferKind::Directory);
    let first = service
        .enqueue(request.clone())
        .await
        .expect("first directory task is accepted");
    assert_eq!(first.effective_idle_timeout_seconds, Some(180));

    let settings = SftpPerformanceSettings {
        idle_timeout_seconds: 240,
        ..SftpPerformanceSettings::default()
    };
    service
        .inner
        .sftp_policy
        .update(settings)
        .expect("new SFTP idle policy is accepted");

    let first_after_update = service
        .get(&first.id)
        .await
        .expect("first task remains queryable");
    assert_eq!(first_after_update.effective_idle_timeout_seconds, Some(180));
    let second = service
        .enqueue(request)
        .await
        .expect("second directory task is accepted");
    assert_eq!(second.effective_idle_timeout_seconds, Some(240));
}

/// 仅新增确认字节更新时间与速度，最终提交屏障可补齐已合并的最新进度快照。
#[tokio::test]
async fn task_summary_reports_real_progress_telemetry() {
    let (service, _, request, _) = service_for(BackendMode::ProgressTelemetry);
    let task = service.enqueue(request).await.expect("request is accepted");
    wait_for_status(&service, &task.id, TransferStatus::Succeeded).await;

    let summary = service.get(&task.id).await.expect("task remains queryable");
    assert_eq!(summary.bytes_transferred, 1);
    assert!(summary.speed_bytes_per_second > 0);
    assert!(summary.last_progress_at.is_some());
}

/// 预读校验期间保持零传输字节并展示 Verifying，复制开始后切到 Transferring，终态清空阶段。
///
/// @author kongweiguang
#[tokio::test]
async fn task_summary_projects_verification_then_transfer_phase_only_while_running() {
    let (service, backend, request, _) = service_for(BackendMode::PhaseTransitions);
    let task = service.enqueue(request).await.expect("request is accepted");

    timeout(
        Duration::from_secs(2),
        backend.verification_reported.notified(),
    )
    .await
    .expect("backend reports its source verification stage");
    let verifying = wait_for_phase(&service, &task.id, TransferPhase::Verifying).await;
    assert_eq!(verifying.bytes_transferred, 0);
    backend.release_after_verification.notify_one();

    timeout(Duration::from_secs(2), backend.transfer_reported.notified())
        .await
        .expect("backend reports the start of actual copying");
    let transferring = wait_for_phase(&service, &task.id, TransferPhase::Transferring).await;
    assert_eq!(transferring.bytes_transferred, 1);
    backend.release_after_transfer.notify_one();

    wait_for_status(&service, &task.id, TransferStatus::Succeeded).await;
    let terminal = service.get(&task.id).await.expect("task remains queryable");
    assert_eq!(terminal.phase, None);
}

/// 仅在目录完整遍历并确认无变化后释放外部 scope，不让测试文件传输冒充无变更目录结果。
///
/// @author kongweiguang
#[tokio::test]
async fn external_transfer_lease_is_released_after_confirmed_worker_cleanup() {
    let (service, _, mut request, _) =
        service_for_kind(BackendMode::NoChanges, TransferKind::Directory);
    let (lease, owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");

    assert!(service.enqueue(request.clone()).await.is_err());
    let queued = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease explicitly authorizes the temporary endpoint");
    assert_eq!(owner.scopes.load(Ordering::Acquire), 2);

    wait_for_status(&service, &queued.id, TransferStatus::Unchanged).await;
    wait_for_external_scopes(&owner, 1).await;
    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("clean worker history is removable");
    assert_eq!((removed, remaining), (1, 0));
    drop(lease);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 0);
}

/// 清理失败保留真实 owner；普通传输 retry 必须等待 cleanup 专用路径确认资源释放。
///
/// @author kongweiguang
#[tokio::test]
async fn pending_cleanup_owner_is_retained_until_explicit_retry_succeeds() {
    let (service, backend, mut request, _) = service_for(BackendMode::FailWithPendingCleanup);
    let (lease, external_owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");
    let task = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease authorizes the transfer");

    wait_for_status(&service, &task.id, TransferStatus::Failed).await;
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 2);
    let retry_error = service
        .retry(&task.id)
        .await
        .expect_err("a pending cleanup owner blocks a new transfer attempt");
    assert_eq!(retry_error.public_code(), PublicErrorCode::Conflict);
    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("history cleanup leaves records with cleanup owners actionable");
    assert_eq!((removed, remaining), (0, 1));
    assert!(
        service
            .get(&task.id)
            .await
            .expect("the pending owner remains queryable")
            .cleanup_pending
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    assert!(
        service
            .retry_cleanup_until(&task.id, deadline)
            .await
            .is_err()
    );
    assert!(!backend.cleanup_owner.cleaned.load(Ordering::Acquire));
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 2);

    service
        .retry_cleanup_until(
            &task.id,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect("the retained owner confirms cleanup on retry");
    assert!(backend.cleanup_owner.cleaned.load(Ordering::Acquire));
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 1);
    assert_eq!(
        service
            .clear_completed(CancellationToken::new())
            .await
            .expect("history is removable only after cleanup confirmation")
            .0,
        1
    );
    drop(lease);
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 0);
}

/// 退出复用同一绝对 deadline 清理 owner；首次失败不丢句柄，后续 shutdown 可继续完成。
#[tokio::test]
async fn shutdown_retries_pending_cleanup_without_losing_owner() {
    let (service, backend, mut request, _) = service_for(BackendMode::FailWithPendingCleanup);
    let (lease, external_owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");
    let task = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease authorizes the transfer");
    wait_for_status(&service, &task.id, TransferStatus::Failed).await;

    assert!(
        service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    assert!(!backend.cleanup_owner.cleaned.load(Ordering::Acquire));
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 2);

    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("second shutdown retries the retained owner");
    assert!(backend.cleanup_owner.cleaned.load(Ordering::Acquire));
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 1);
    drop(lease);
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 0);
}

/// 外部任务失败后的重试必须由当前工作区重新提供同一目标 lease，不从历史暗中保留凭据。
#[tokio::test]
async fn external_retry_requires_and_retains_current_workspace_lease() {
    let (service, _, mut request, _) = service_for(BackendMode::FailBeforeCommit);
    let (lease, owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");
    let original = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease explicitly authorizes the temporary endpoint");
    wait_for_status(&service, &original.id, TransferStatus::Failed).await;
    wait_for_external_scopes(&owner, 1).await;

    assert!(service.retry(&original.id).await.is_err());
    let successor = service
        .retry_with_external_target(&original.id, &lease)
        .await
        .expect("retry retains a fresh scope from the active workspace");
    assert_ne!(successor.id, original.id);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 2);
    wait_for_status(&service, &successor.id, TransferStatus::Failed).await;
    wait_for_external_scopes(&owner, 1).await;
    drop(lease);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 0);
}

/// 普通历史清理保留 CommitUnknown 与外部目标 scope，供用户核对真实提交结果。
///
/// @author kongweiguang
#[tokio::test]
async fn external_unknown_commit_scope_survives_history_cleanup_for_manual_readback() {
    let (service, _, mut request, _) = service_for(BackendMode::LoseCommitReceipt);
    let (lease, owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");
    let task = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease explicitly authorizes the temporary endpoint");
    wait_for_status(&service, &task.id, TransferStatus::Failed).await;
    wait_for_external_scopes(&owner, 2).await;
    assert_eq!(
        service
            .get(&task.id)
            .await
            .expect("unknown task remains queryable")
            .failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );

    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("history cleanup must skip a result that still needs inspection");
    assert_eq!((removed, remaining), (0, 1));
    assert_eq!(owner.scopes.load(Ordering::Acquire), 2);
    assert_eq!(
        service
            .get(&task.id)
            .await
            .expect("unknown task stays available for readback")
            .failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
    drop(lease);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 1);
}

/// 明确 no-change 回执可释放单项许可后继续遍历，且不伪造目录提交 receipt。
#[tokio::test]
async fn confirmed_no_effect_item_can_finish_directory_unchanged() {
    let (service, _, request, _) = service_for_kind(
        BackendMode::NoChangesAfterNoEffectItem,
        TransferKind::Directory,
    );
    let queued = service
        .enqueue(request)
        .await
        .expect("queue accepts directory transfer");
    wait_for_status(&service, &queued.id, TransferStatus::Unchanged).await;
    let summary = service
        .get(&queued.id)
        .await
        .expect("task remains queryable");
    assert_eq!(summary.commit_summary.committed_entries(), 0);
    assert_eq!(summary.commit_summary.committed_bytes(), 0);
    assert!(!summary.partial_commit);
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("finished worker is joined");
}

/// 无变更屏障线性化后迟到取消只能读取 Unchanged，不得反向覆盖最终结果。
#[tokio::test]
async fn no_changes_barrier_wins_over_late_cancel() {
    let (service, backend, request, _) = service_for_kind(
        BackendMode::CancelAfterNoChangesBarrier,
        TransferKind::Directory,
    );
    let queued = service
        .enqueue(request)
        .await
        .expect("queue accepts directory transfer");
    backend.no_changes_finalized.notified().await;
    let after_cancel = service
        .cancel(&queued.id)
        .await
        .expect("late cancel is idempotent");
    assert_eq!(after_cancel.status, TransferStatus::Unchanged);
    backend.release_after_nochanges.notify_one();
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("worker cleanup remains joined after the barrier");
    assert_eq!(
        service
            .get(&queued.id)
            .await
            .expect("task remains queryable")
            .status,
        TransferStatus::Unchanged
    );
}

/// 遍历尚未完成时收到取消只能结束为 Cancelled，不能把未检查完整的目录标为无变更。
#[tokio::test]
async fn cancellation_during_directory_traversal_cannot_be_unchanged() {
    let (service, backend, request, _) =
        service_for_kind(BackendMode::CancelDuringTraversal, TransferKind::Directory);
    let queued = service
        .enqueue(request)
        .await
        .expect("queue accepts directory transfer");
    backend.started.notified().await;
    service
        .cancel(&queued.id)
        .await
        .expect("cancel is accepted");
    wait_for_status(&service, &queued.id, TransferStatus::Cancelled).await;
    let summary = service
        .get(&queued.id)
        .await
        .expect("cancelled task remains queryable");
    assert_eq!(summary.commit_summary.committed_entries(), 0);
    assert_eq!(summary.commit_summary.committed_bytes(), 0);
    assert_ne!(summary.status, TransferStatus::Unchanged);
}

/// 在有界时间内观察权威队列快照，避免依赖可能丢失的广播事件推进断言。
async fn wait_for_status(
    service: &TransferService,
    id: &kerminal_domain::identity::TransferId,
    expected: TransferStatus,
) {
    timeout(Duration::from_secs(2), async {
        loop {
            if service
                .get(id)
                .await
                .expect("task remains available")
                .status
                == expected
            {
                return;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("queue reaches expected status");
}

/// 在受控后端暂停时读取指定运行阶段，避免只观察到已合并的最终快照。
///
/// @author kongweiguang
async fn wait_for_phase(
    service: &TransferService,
    id: &kerminal_domain::identity::TransferId,
    expected: TransferPhase,
) -> kerminal_domain::files::TransferSummary {
    timeout(Duration::from_secs(2), async {
        loop {
            let summary = service.get(id).await.expect("task remains available");
            if summary.phase == Some(expected) {
                return summary;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("queue publishes expected transfer phase")
}

#[tokio::test]
/// 提交请求排队等待许可时取消必须赢得线性化顺序，后端不得执行 rename。
async fn cancellation_before_commit_acknowledgment_keeps_destination_unmodified() {
    let (service, backend, request, enqueued) =
        service_for(BackendMode::CancelBeforeAcknowledgment);
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.started.notified().await;
    wait_for_status(&service, &task.id, TransferStatus::Transferring).await;

    let records = service.inner.records.lock().await;
    backend.release_commit_request.notify_one();
    enqueued.await.expect("backend enqueues commit request");
    let mut records = records;
    let record = records.get_mut(&task.id).expect("task record exists");
    assert!(record.task.request_cancel());
    record.cancellation.cancel();
    drop(records);

    wait_for_status(&service, &task.id, TransferStatus::Cancelled).await;
    assert!(!backend.rename_applied.load(Ordering::Acquire));
}

#[tokio::test]
/// 当前目标项取消被延迟到回执，最终屏障应让成功结果赢得竞态。
async fn cancellation_during_final_item_is_deferred_and_final_receipt_wins() {
    let (service, backend, request, enqueued) = service_for(BackendMode::CancelAfterAcknowledgment);
    let task = service.enqueue(request).await.expect("request is accepted");
    enqueued.await.expect("backend enqueues commit request");
    backend.acknowledgment_received.notified().await;

    let cancellation = service
        .cancel(&task.id)
        .await
        .expect("cancel intent is accepted while an item is committing");
    assert_eq!(cancellation.status, TransferStatus::Committing);
    backend.release_after_acknowledgment.notify_one();
    wait_for_status(&service, &task.id, TransferStatus::Succeeded).await;
    assert!(backend.rename_applied.load(Ordering::Acquire));
}

#[tokio::test]
/// 目录合并已确认一项后取消，队列保留该项摘要并禁止将部分传输安全重试。
async fn cancellation_between_directory_items_keeps_confirmed_partial_summary() {
    let (service, backend, request, enqueued) =
        service_for_kind(BackendMode::CancelBetweenItems, TransferKind::Directory);
    let task = service.enqueue(request).await.expect("request is accepted");
    enqueued
        .await
        .expect("backend reaches the first commit boundary");
    backend.first_receipt_recorded.notified().await;

    let cancellation = service
        .cancel(&task.id)
        .await
        .expect("cancel intent is accepted between directory items");
    assert_eq!(cancellation.status, TransferStatus::Cancelling);
    backend.release_after_first_receipt.notify_one();

    wait_for_status(&service, &task.id, TransferStatus::Cancelled).await;
    let result = service.get(&task.id).await.expect("task remains available");
    assert_eq!(result.commit_summary.committed_entries(), 1);
    assert_eq!(result.commit_summary.committed_bytes(), 1);
    assert!(result.partial_commit);
    assert!(!result.retryable);
}

#[tokio::test]
/// 已收到目录项回执后发生明确失败，投影真实部分结果而不标成提交未知。
async fn known_failure_after_directory_receipt_keeps_partial_summary() {
    let (service, backend, request, enqueued) =
        service_for_kind(BackendMode::FailAfterFirstReceipt, TransferKind::Directory);
    let task = service.enqueue(request).await.expect("request is accepted");
    enqueued
        .await
        .expect("backend reaches the first commit boundary");

    wait_for_status(&service, &task.id, TransferStatus::Failed).await;
    let result = service.get(&task.id).await.expect("task remains available");
    assert_eq!(result.failure_kind, Some(TransferFailureKind::Connection));
    assert_eq!(result.commit_summary.committed_entries(), 1);
    assert_eq!(result.commit_summary.committed_bytes(), 1);
    assert!(result.partial_commit);
    assert!(!result.retryable);
    assert!(backend.rename_applied.load(Ordering::Acquire));
}

#[tokio::test]
/// rename 已执行但回执丢失必须收敛到不可重试的 CommitUnknown，而不能伪报成功。
async fn successful_rename_with_lost_receipt_is_commit_unknown_and_not_retryable() {
    let (service, backend, request, enqueued) = service_for(BackendMode::LoseCommitReceipt);
    let task = service.enqueue(request).await.expect("request is accepted");
    enqueued.await.expect("backend enqueues commit request");

    wait_for_status(&service, &task.id, TransferStatus::Failed).await;
    let result = service.get(&task.id).await.expect("task remains available");
    assert_eq!(
        result.failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
    assert!(!result.retryable);
    assert!(backend.rename_applied.load(Ordering::Acquire));
    let retry_error = service
        .retry(&task.id)
        .await
        .expect_err("an unknown commit must not be replayed");
    assert_eq!(retry_error.public_code(), PublicErrorCode::OutcomeUnknown);
}

/// 终态清理先 join worker，再移除无需人工核对的普通历史记录。
///
/// @author kongweiguang
#[tokio::test]
async fn clear_completed_joins_worker_before_removing_ordinary_terminal_history() {
    let (service, backend, request, _) = service_for_kind_with_timeout(
        BackendMode::CancelAfterNoChangesBarrier,
        TransferKind::Directory,
        Duration::from_secs(2),
    );
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.no_changes_finalized.notified().await;

    let clear_service = service.clone();
    let clear = tokio::spawn(async move {
        clear_service
            .clear_completed(CancellationToken::new())
            .await
    });
    timeout(Duration::from_secs(2), async {
        loop {
            if service
                .inner
                .records
                .lock()
                .await
                .get(&task.id)
                .is_some_and(|record| record.worker.is_none())
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("history cleanup takes the handle before waiting for the paused terminal worker");
    backend.release_after_nochanges.notify_one();

    let (removed, remaining) = timeout(Duration::from_secs(2), clear)
        .await
        .expect("worker join completes after it is released")
        .expect("clear task does not panic")
        .expect("terminal history is joined before removal");
    assert_eq!(removed, 1);
    assert_eq!(remaining, 0);
    assert!(service.get(&task.id).await.is_err());
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("cleared worker was already joined");
}

/// 历史清理等待 worker 所有权锁受绝对 deadline 约束，超时后可重试并完成 join。
///
/// @author kongweiguang
#[tokio::test]
async fn clear_completed_lock_contention_keeps_worker_owner_for_retry() {
    let (service, backend, request, _) = service_for_kind_with_timeout(
        BackendMode::CancelAfterNoChangesBarrier,
        TransferKind::Directory,
        Duration::from_millis(20),
    );
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.no_changes_finalized.notified().await;
    let owner_lock = service.inner.shutdown_workers.lock().await;

    assert!(
        service
            .clear_completed(CancellationToken::new())
            .await
            .is_err()
    );
    drop(owner_lock);
    backend.release_after_nochanges.notify_one();

    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("a later pass joins the worker retained after lock contention");
    assert_eq!(removed, 1);
    assert_eq!(remaining, 0);
    assert!(service.get(&task.id).await.is_err());
}

/// 用户取消锁等待阶段的清理后，历史记录仍可查询并能在释放锁后再次清理。
///
/// @author kongweiguang
#[tokio::test]
async fn clear_completed_cancellation_before_deletion_preserves_history_for_retry() {
    let (service, _, request, _) =
        service_for_kind(BackendMode::NoChanges, TransferKind::Directory);
    let task = service.enqueue(request).await.expect("request is accepted");
    wait_for_status(&service, &task.id, TransferStatus::Unchanged).await;

    let shutdown_lock = service.inner.shutdown_lock.lock().await;
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let clear_service = service.clone();
    let clear = tokio::spawn(async move { clear_service.clear_completed(task_cancellation).await });
    tokio::task::yield_now().await;
    cancellation.cancel();

    let error = timeout(Duration::from_secs(2), clear)
        .await
        .expect("cancelled lock wait completes")
        .expect("clear task does not panic")
        .expect_err("cancelled history cleanup must not remove records");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(
        service
            .get(&task.id)
            .await
            .expect("history remains queryable after cancellation")
            .status,
        TransferStatus::Unchanged
    );
    drop(shutdown_lock);

    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("a later attempt removes the safe terminal record");
    assert_eq!((removed, remaining), (1, 0));
    assert!(service.get(&task.id).await.is_err());
}

/// 清理只选择可移除的终态，不能让活动传输消失或影响后续取消。
///
/// @author kongweiguang
#[tokio::test]
async fn clear_completed_leaves_active_transfer_available() {
    let (service, backend, request, _) =
        service_for_kind(BackendMode::CancelDuringTraversal, TransferKind::Directory);
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.started.notified().await;

    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("active work does not block safe history cleanup");
    assert_eq!((removed, remaining), (0, 1));
    assert_eq!(
        service
            .get(&task.id)
            .await
            .expect("active transfer remains visible")
            .status,
        TransferStatus::Transferring
    );

    service
        .cancel(&task.id)
        .await
        .expect("active transfer can cancel");
    wait_for_status(&service, &task.id, TransferStatus::Cancelled).await;
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("cancelled active worker is joined during cleanup");
}

/// 历史容量仅裁剪最旧的普通终态，CommitUnknown 必须保留到人工核对。
///
/// @author kongweiguang
#[test]
fn history_pruning_retains_unknown_commit_beyond_ordinary_limit() {
    let (unknown_id, unknown) = terminal_record(TransferFailureKind::CommitUnknown, 1);
    let (old_id, old) = terminal_record(TransferFailureKind::Connection, 2);
    let (recent_id, recent) = terminal_record(TransferFailureKind::Connection, 3);
    let mut records = BTreeMap::from([
        (unknown_id.clone(), unknown),
        (old_id.clone(), old),
        (recent_id.clone(), recent),
    ]);

    prune_history(&mut records, 1);

    assert!(records.contains_key(&unknown_id));
    assert!(!records.contains_key(&old_id));
    assert!(records.contains_key(&recent_id));
    assert_eq!(records.len(), 2);
}

/// 在提交控制请求排队前完成 shutdown 取消，验证 worker 收到撤销后不会触发目标写入。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_cancels_and_joins_precommit_worker() {
    let (service, backend, request, enqueued) =
        service_for(BackendMode::CancelBeforeAcknowledgment);
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.started.notified().await;
    let retry_service = service.clone();
    let shutdown = tokio::spawn(async move {
        retry_service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    });
    wait_for_status(&service, &task.id, TransferStatus::Cancelling).await;
    backend.release_commit_request.notify_one();
    enqueued.await.expect("backend reaches the commit boundary");

    shutdown
        .await
        .expect("shutdown task does not panic")
        .expect("precommit worker cooperatively stops before the deadline");

    let summary = service.get(&task.id).await.expect("task remains available");
    assert_eq!(summary.status, TransferStatus::Cancelled);
    assert!(!backend.rename_applied.load(Ordering::Acquire));
}

/// 关闭 deadline 不会丢弃已提交的执行 future；未收到真实后端结果时保留 Committing 并报告清理超时。
#[tokio::test]
async fn shutdown_deadline_keeps_unacknowledged_commit_active_and_can_resume_join() {
    let (service, backend, request, enqueued) = service_for(BackendMode::CancelAfterAcknowledgment);
    let task = service.enqueue(request).await.expect("request is accepted");
    enqueued.await.expect("backend reaches the commit boundary");
    backend.acknowledgment_received.notified().await;

    let first_shutdown = service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_millis(10))
        .await;
    assert!(
        first_shutdown.is_err(),
        "unfinished worker must not report clean shutdown"
    );

    let summary = service.get(&task.id).await.expect("task remains available");
    assert_eq!(summary.status, TransferStatus::Committing);
    assert!(!summary.retryable);

    backend.release_after_acknowledgment.notify_one();
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("a later cleanup pass joins the worker after its real result arrives");
    let summary = service.get(&task.id).await.expect("task remains available");
    assert_eq!(summary.status, TransferStatus::Succeeded);
}

#[tokio::test]
/// 锁竞争受同一绝对关闭期限约束；再次关闭继续持有的 worker 清理并完成取消。
async fn shutdown_lock_contention_times_out_and_a_later_pass_joins_worker() {
    let (service, backend, request, _enqueued) =
        service_for(BackendMode::CancelBeforeAcknowledgment);
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.started.notified().await;
    let intake = service.inner.intake_lock.lock().await;

    assert!(
        service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_millis(10))
            .await
            .is_err()
    );
    drop(intake);

    let retry_service = service.clone();
    let retry_shutdown = tokio::spawn(async move {
        retry_service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
    });
    wait_for_status(&service, &task.id, TransferStatus::Cancelling).await;
    backend.release_commit_request.notify_one();
    retry_shutdown
        .await
        .expect("second shutdown task does not panic")
        .expect("second shutdown joins the retained worker");

    let result = service.get(&task.id).await.expect("task remains available");
    assert_eq!(result.status, TransferStatus::Cancelled);
    assert!(!backend.rename_applied.load(Ordering::Acquire));
}

/// worker 确认开始后再触发 panic，验证 JoinError 被保留且重复 shutdown 不伪报成功。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_retains_worker_join_failure_across_retries() {
    let (service, backend, request, _) = service_for(BackendMode::PanicBeforeCommit);
    let task = service.enqueue(request).await.expect("request is accepted");
    backend.started.notified().await;

    assert!(
        service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
    let summary = service
        .get(&task.id)
        .await
        .expect("failed worker remains visible");
    assert_eq!(summary.status, TransferStatus::Failed);
    assert_eq!(summary.failure_kind, Some(TransferFailureKind::Internal));

    assert!(
        service
            .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
            .await
            .is_err()
    );
}

/// 数据提交成功后仍保留清理 owner；错配释放不得丢 owner，重试成功后才清理历史与凭据。
///
/// @author kongweiguang
#[tokio::test]
async fn successful_transfer_projects_pending_cleanup_until_matching_owner_releases() {
    let (service, backend, mut request, _) = service_for(BackendMode::CommitThenRetainCleanupOwner);
    let (lease, external_owner) = external_target_lease();
    request.source = FileEndpoint::remote(lease.host_id().clone(), "/source.bin")
        .expect("temporary source endpoint is valid");
    let task = service
        .enqueue_with_external_target(request, &lease)
        .await
        .expect("matching lease authorizes the transfer");

    timeout(Duration::from_secs(2), async {
        loop {
            let summary = service.get(&task.id).await.expect("task remains queryable");
            if summary.cleanup_pending {
                assert_eq!(summary.status, TransferStatus::Succeeded);
                assert_eq!(summary.commit_summary.committed_entries(), 1);
                break;
            }
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("queue projects the acknowledged cleanup owner");

    let (removed, remaining) = service
        .clear_completed(CancellationToken::new())
        .await
        .expect("history cleanup skips the record while its cleanup owner is pending");
    assert_eq!((removed, remaining), (0, 1));
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 2);
    let stale_owner: Arc<dyn TransferResourceCleanup> = Arc::new(RetryableCleanup {
        attempts: AtomicUsize::new(0),
        cleaned: AtomicBool::new(false),
    });
    assert!(
        service
            .apply_release_cleanup_owner(&task.id, stale_owner)
            .await
            .is_err()
    );
    assert!(
        service
            .get(&task.id)
            .await
            .expect("task remains queryable")
            .cleanup_pending
    );

    assert!(
        service
            .retry_cleanup_until(
                &task.id,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .is_err()
    );
    assert!(
        service
            .get(&task.id)
            .await
            .expect("failed cleanup keeps its owner")
            .cleanup_pending
    );
    service
        .retry_cleanup_until(
            &task.id,
            tokio::time::Instant::now() + Duration::from_secs(2),
        )
        .await
        .expect("same owner confirms cleanup on retry");
    assert!(backend.cleanup_owner.cleaned.load(Ordering::Acquire));
    assert!(
        !service
            .get(&task.id)
            .await
            .expect("task remains queryable")
            .cleanup_pending
    );
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 1);
    assert_eq!(
        service
            .clear_completed(CancellationToken::new())
            .await
            .expect("owner released")
            .0,
        1
    );
    drop(lease);
    assert_eq!(external_owner.scopes.load(Ordering::Acquire), 0);
}

/// 队列变更事件只唤醒订阅者，状态必须从服务端权威列表重新读取。
///
/// @author kongweiguang
#[tokio::test]
async fn transfer_change_wakeup_is_followed_by_authoritative_list_state() {
    let (service, _, request, _) = service_for(BackendMode::NoChanges);
    let mut subscription = service.subscribe();
    let accepted = service.enqueue(request).await.expect("request is accepted");

    let wakeup = timeout(
        Duration::from_secs(2),
        subscription.recv(CancellationToken::new()),
    )
    .await
    .expect("queue change wakes the subscriber");
    assert_eq!(wakeup, Some(()));

    let transfers = service.list().await;
    assert!(
        transfers
            .iter()
            .any(|summary| { summary.id.as_str() == accepted.id.as_str() })
    );

    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("test queue worker is joined before the test ends");
}
