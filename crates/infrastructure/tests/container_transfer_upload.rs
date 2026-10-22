// @author kongweiguang

use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        CommandStreamCompletion, CommandStreamEvent, CommandStreamExit, CommandStreamInputMessage,
        CommandStreamState, ContainerStageCleanupCapability, ContainerStageCleanupRequest,
        OperationsCommandExecutor, OperationsCommandStream, StageCreationResult,
    },
    transfers::{TransferQueueOptions, TransferService},
};
use kerminal_domain::files::{
    ConflictPolicy, ContainerRuntime, FileEndpoint, TransferFailureKind, TransferKind,
    TransferRequest, TransferStatus,
};
use kerminal_infrastructure::operations::ContainerTransferBackend;
use tokio::{
    runtime::Handle,
    sync::{mpsc, watch},
    time::{Instant, sleep, timeout},
};
use tokio_util::sync::CancellationToken;

#[derive(Default)]
struct ExecutorReceipts {
    streams: AtomicUsize,
    target_commits: AtomicUsize,
    stage_cleanup_attempts: AtomicUsize,
}

struct IncompleteThenCompleteExecutor {
    receipts: Arc<ExecutorReceipts>,
}

#[async_trait]
impl OperationsCommandExecutor for IncompleteThenCompleteExecutor {
    /// 回放目标类型探针和最终提交脚本，只统计真实到达目标提交屏障的命令。
    async fn execute(
        &self,
        _target: CommandExecutionTarget,
        invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        assert_eq!(
            invocation.arguments.get(3).map(String::as_str),
            Some("sh"),
            "container invocation keeps the fixed shell executable slot"
        );
        assert_eq!(
            invocation.arguments.get(4).map(String::as_str),
            Some("-c"),
            "container invocation keeps the fixed shell script flag slot"
        );
        let script = invocation
            .arguments
            .get(5)
            .map(String::as_str)
            .ok_or_else(|| AppError::invalid("测试命令缺少脚本参数槽"))?;
        let (stdout, exit_code) = if script.contains("mv -fT") {
            self.receipts.target_commits.fetch_add(1, Ordering::AcqRel);
            (Vec::new(), 0)
        } else if script.contains("if [ -L \"$p\" ]") {
            let path = invocation
                .arguments
                .last()
                .map(String::as_str)
                .unwrap_or_default();
            if path == "/workspace" {
                (b"directory\n".to_vec(), 0)
            } else {
                (b"missing\n".to_vec(), 0)
            }
        } else {
            return Err(AppError::unsupported("测试 fake 收到未识别的命令阶段"));
        };
        Ok(CommandExecutionResult {
            stdout,
            stderr: Vec::new(),
            exit_code: Some(exit_code),
            truncated: false,
            input_complete: true,
        })
    }

    /// 将入参视作调用方父令牌，并为流与 worker 派生子令牌以匹配真实 executor 的所有权边界。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        cancellation: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        let attempt = self.receipts.streams.fetch_add(1, Ordering::AcqRel);
        let input_incomplete = attempt == 0;
        let (events_tx, events_rx) = mpsc::channel::<CommandStreamEvent>(8);
        let (input_tx, mut input_rx) = mpsc::channel::<CommandStreamInputMessage>(8);
        let (completion_tx, completion_rx) =
            watch::channel(CommandStreamState::Running { attempt: 0 });
        let (cleanup_deadline, _) = watch::channel(None);
        let (shutdown_ceiling, shutdown_ceiling_rx) = watch::channel(None);
        let stream_cancellation = cancellation.child_token();
        let worker_cancellation = stream_cancellation.child_token();
        tokio::spawn(async move {
            let mut input_complete = false;
            loop {
                let message = tokio::select! {
                    _ = worker_cancellation.cancelled() => break,
                    message = input_rx.recv() => message,
                };
                let Some(message) = message else { break };
                match message {
                    CommandStreamInputMessage::Data {
                        acknowledgement, ..
                    } => {
                        let _ = acknowledgement.send(Ok(()));
                    }
                    CommandStreamInputMessage::Close { acknowledgement } => {
                        let receipt = if input_incomplete {
                            Err(AppError::conflict("测试未确认 stdin EOF"))
                        } else {
                            Ok(())
                        };
                        input_complete = receipt.is_ok();
                        let _ = acknowledgement.send(receipt);
                        break;
                    }
                }
            }
            drop(events_tx);
            completion_tx.send_replace(CommandStreamState::Complete(CommandStreamCompletion {
                execution: Ok(CommandStreamExit {
                    exit_code: Some(0),
                    stderr: Vec::new(),
                    stderr_truncated: false,
                    input_complete,
                }),
                cleanup: Ok(()),
            }));
        });
        shutdown_ceiling.send_replace(None);
        Ok(OperationsCommandStream::new(
            events_rx,
            input_tx,
            completion_rx,
            stream_cancellation,
            cleanup_deadline,
            shutdown_ceiling_rx,
        ))
    }

    /// 为每次上传绑定自己的 staging cleanup owner，不开放任意路径清理。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Ok(Arc::new(FakeStageCleanup {
            receipts: self.receipts.clone(),
        }))
    }

    /// 本 fake 不创建常规 worker；队列关闭入口保持确定的空操作。
    fn begin_shutdown(&self, _deadline: Instant) {}

    /// 本 fake 的流 worker 在 input EOF 后即完成，不保留其它后台资源。
    async fn shutdown_until(&self, _deadline: Instant) -> AppResult<()> {
        Ok(())
    }
}

struct FakeStageCleanup {
    receipts: Arc<ExecutorReceipts>,
}

#[async_trait]
impl ContainerStageCleanupCapability for FakeStageCleanup {
    /// 首次失败模拟远端 staging 清理未收到确认，确保队列保留重试 owner。
    async fn create_stage_until(&self, _deadline: Instant) -> AppResult<StageCreationResult> {
        Ok(StageCreationResult::Created)
    }

    /// 清理 owner 的第二次调用成功，证明 pending cleanup 可通过显式路径释放。
    async fn cleanup_until(&self, _deadline: Instant) -> AppResult<()> {
        let attempt = self
            .receipts
            .stage_cleanup_attempts
            .fetch_add(1, Ordering::AcqRel);
        if attempt == 0 {
            Err(AppError::infrastructure("测试 staging cleanup 首次未确认"))
        } else {
            Ok(())
        }
    }
}

/// 轮询权威队列快照并在任一终态立即返回，避免失败重试被掩盖成等待超时。
async fn wait_for_status(
    transfers: &TransferService,
    id: &kerminal_domain::identity::TransferId,
    expected: TransferStatus,
) -> kerminal_application::AppResult<kerminal_domain::files::TransferSummary> {
    let mut latest = None;
    let result = timeout(Duration::from_secs(8), async {
        loop {
            let summary = transfers.get(id).await?;
            let terminal = matches!(
                summary.status,
                TransferStatus::Succeeded
                    | TransferStatus::Skipped
                    | TransferStatus::Unchanged
                    | TransferStatus::Failed
                    | TransferStatus::Cancelled
            );
            if summary.status == expected || terminal {
                return Ok(summary);
            }
            latest = Some(summary);
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => {
            let Some(summary) = latest else {
                panic!("等待传输终态超时，未能取得队列快照");
            };
            panic!(
                "等待传输终态超时: expected={expected:?}, last_status={:?}, failure_kind={:?}, safe_error={:?}, cleanup_pending={}, retryable={}, partial_commit={}, bytes={}/{}",
                summary.status,
                summary.failure_kind,
                summary.safe_error,
                summary.cleanup_pending,
                summary.retryable,
                summary.partial_commit,
                summary.bytes_transferred,
                summary.total_bytes.unwrap_or_default(),
            );
        }
    }
}

/// 输入 ACK 缺失时不跨目标 commit；资源清理确认后，显式 retry 才能完成提交。
#[tokio::test]
async fn incomplete_archive_input_requires_cleanup_before_explicit_retry() {
    let source_root = tempfile::tempdir().expect("create isolated source directory");
    let source = source_root.path().join("payload.bin");
    fs::write(&source, b"archive payload whose EOF receipt is tested")
        .expect("write isolated source file");
    let destination = FileEndpoint::Container {
        host_id: None,
        runtime: ContainerRuntime::Podman,
        container_id: "kerminal-test-fixture".to_owned(),
        path: kerminal_domain::files::RemotePath::parse_container("/workspace/payload.bin")
            .expect("valid normalized endpoint"),
    };
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: PathBuf::from(&source),
        },
        destination,
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    let receipts = Arc::new(ExecutorReceipts::default());
    let executor = Arc::new(IncompleteThenCompleteExecutor {
        receipts: receipts.clone(),
    });
    let backend = Arc::new(ContainerTransferBackend::new(executor));
    let backend: Arc<dyn kerminal_application::transfers::TransferBackend> = backend;
    let transfers = TransferService::new(
        vec![backend],
        Handle::current(),
        TransferQueueOptions::default(),
    )
    .expect("create transfer queue");

    let first = transfers.enqueue(request).await.expect("enqueue upload");
    let failed = wait_for_status(&transfers, &first.id, TransferStatus::Failed)
        .await
        .expect("first upload reaches failed state");
    assert_eq!(
        failed.status,
        TransferStatus::Failed,
        "first attempt reached a different terminal state: failure_kind={:?}, safe_error={:?}, cleanup_pending={}, retryable={}",
        failed.failure_kind,
        failed.safe_error,
        failed.cleanup_pending,
        failed.retryable,
    );
    assert_eq!(failed.failure_kind, Some(TransferFailureKind::Connection));
    assert!(failed.retryable, "retry is allowed after resource cleanup");
    assert!(
        failed.cleanup_pending,
        "unconfirmed stage cleanup stays owned"
    );
    assert!(
        !failed.partial_commit,
        "input receipt is before target commit"
    );
    assert_eq!(receipts.target_commits.load(Ordering::Acquire), 0);
    assert_eq!(receipts.streams.load(Ordering::Acquire), 1);

    assert!(
        transfers.retry(&first.id).await.is_err(),
        "retry cannot start while the original staging owner is pending"
    );
    transfers
        .retry_cleanup_until(&first.id, Instant::now() + Duration::from_secs(3))
        .await
        .expect("explicit cleanup retry confirms stream and staging release");
    assert!(
        !transfers
            .get(&first.id)
            .await
            .expect("read first task")
            .cleanup_pending
    );

    let successor = transfers
        .retry(&first.id)
        .await
        .expect("user can explicitly retry after cleanup is confirmed");
    let succeeded = wait_for_status(&transfers, &successor.id, TransferStatus::Succeeded)
        .await
        .expect("retry reaches success");
    assert_eq!(
        succeeded.status,
        TransferStatus::Succeeded,
        "retry reached a different terminal state: failure_kind={:?}, safe_error={:?}, cleanup_pending={}, retryable={}, partial_commit={}, bytes={}/{}",
        succeeded.failure_kind,
        succeeded.safe_error,
        succeeded.cleanup_pending,
        succeeded.retryable,
        succeeded.partial_commit,
        succeeded.bytes_transferred,
        succeeded.total_bytes.unwrap_or_default(),
    );
    assert_eq!(succeeded.failure_kind, None);
    assert_eq!(receipts.streams.load(Ordering::Acquire), 2);
    assert_eq!(receipts.target_commits.load(Ordering::Acquire), 1);
    assert!(receipts.stage_cleanup_attempts.load(Ordering::Acquire) >= 2);

    transfers
        .shutdown_until(Instant::now() + Duration::from_secs(5))
        .await
        .expect("close transfer workers");
}
