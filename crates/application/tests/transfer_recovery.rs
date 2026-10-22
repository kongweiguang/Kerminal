//! 恢复只读入口及写前持久化门禁的受控回归，不启动文件系统或网络传输。
//!
//! @author kongweiguang

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    preferences::SftpPerformanceSettings,
    transfers::{
        InMemoryTransferRecoveryRepository, SftpTransferPolicyStore, TransferBackend,
        TransferExecutionContext, TransferExecutionError, TransferExecutionResult,
        TransferQueueOptions, TransferRecoveryInspection, TransferRecoveryPage,
        TransferRecoveryRecord, TransferRecoveryRepository, TransferService,
        TransferStagingOwnership,
    },
};
use kerminal_domain::{
    files::{ConflictPolicy, FileEndpoint, TransferKind, TransferRequest, TransferStatus},
    identity::TransferId,
};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio_util::sync::CancellationToken;

struct ReadOnlyBackend {
    executions: Arc<AtomicUsize>,
}

#[async_trait]
impl TransferBackend for ReadOnlyBackend {
    /// 受控后端仅支持测试本机端点，避免测试误路由到真实 SSH。
    fn supports(&self, _: &TransferRequest) -> bool {
        true
    }

    /// 恢复入口不应触发 execute，调用计数固定此边界而不写入任何目标。
    async fn execute(
        &self,
        _: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        self.executions.fetch_add(1, Ordering::SeqCst);
        Err(TransferExecutionError::Cancelled {
            checkpoint: None,
            actual_destination: None,
        })
    }

    /// 只返回目标缺失证据，不把核对视作新传输或旧任务成功。
    async fn inspect_recovery(
        &self,
        _: &TransferRecoveryRecord,
        _: CancellationToken,
    ) -> AppResult<TransferRecoveryInspection> {
        Ok(TransferRecoveryInspection::DestinationMissing)
    }
}

struct FailingRepository;

struct FailSecondWriteRepository {
    writes: AtomicUsize,
    inner: InMemoryTransferRecoveryRepository,
}

#[async_trait]
impl TransferRecoveryRepository for FailSecondWriteRepository {
    /// 所有读取保持正常，聚焦暂存创建前第二次可靠写入的失败边界。
    async fn list(&self, token: CancellationToken) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.inner.list(token).await
    }
    /// 入队材料成功但 stage 意图落盘失败，后端不得在未获许可时创建对象。
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        token: CancellationToken,
    ) -> AppResult<()> {
        if self.writes.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(AppError::infrastructure("受控暂存登记失败"));
        }
        self.inner.upsert(record, token).await
    }
    /// 删除仍遵循原内存仓储契约，不吞掉其他所有者的材料。
    async fn remove(&self, id: &TransferId, token: CancellationToken) -> AppResult<()> {
        self.inner.remove(id, token).await
    }

    /// 替身显式代理归档，不能用默认实现掩盖真实仓储的接口遗漏。
    /// @author kongweiguang
    async fn archive(&self, id: &TransferId, token: CancellationToken) -> AppResult<()> {
        self.inner.archive(id, token).await
    }

    /// 历史与 active 使用同一内存替身，分页预算继续由真正实现执行。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        token: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        self.inner.list_archived(cursor, limit, token).await
    }
}

struct StagingBackend {
    side_effects: Arc<AtomicUsize>,
}

#[async_trait]
impl TransferBackend for StagingBackend {
    /// 受控后端不调用文件系统，许可是唯一允许增加模拟副作用计数的边界。
    fn supports(&self, _: &TransferRequest) -> bool {
        true
    }

    /// 模拟 marker 创建前的首个可靠登记；回执失败直接结束，不创建 marker 或数据对象。
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let ownership = TransferStagingOwnership {
            staging_destination: FileEndpoint::Local {
                path: ".kr-owned.partial".into(),
            },
            actual_destination: context.request.destination.clone(),
            staging_owner: context.transfer_id.clone(),
            staging_token: "a".repeat(32),
        };
        if context.progress.register_staging(ownership).await.is_ok() {
            self.side_effects.fetch_add(1, Ordering::SeqCst);
        }
        Err(TransferExecutionError::Cancelled {
            checkpoint: None,
            actual_destination: Some(context.request.destination),
        })
    }
}

#[async_trait]
impl TransferRecoveryRepository for FailingRepository {
    /// 首次读取合法为空，隔离测试聚焦写前门禁。
    async fn list(&self, _: CancellationToken) -> AppResult<Vec<TransferRecoveryRecord>> {
        Ok(Vec::new())
    }
    /// 模拟落盘失败时应用必须拒绝启动 worker。
    async fn upsert(&self, _: TransferRecoveryRecord, _: CancellationToken) -> AppResult<()> {
        Err(AppError::infrastructure("受控保存失败"))
    }
    /// 测试无终态材料可删，保持删除为幂等无副作用。
    async fn remove(&self, _: &TransferId, _: CancellationToken) -> AppResult<()> {
        Ok(())
    }

    /// 受控写失败也覆盖归档写，不能让测试替身假报材料已经耐久保存。
    /// @author kongweiguang
    async fn archive(&self, _: &TransferId, _: CancellationToken) -> AppResult<()> {
        Err(AppError::infrastructure("受控归档失败"))
    }

    /// 空历史是此替身明确声明的能力，不依赖 trait 默认兜底。
    /// @author kongweiguang
    async fn list_archived(
        &self,
        _: Option<TransferId>,
        _: usize,
        _: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        Ok(TransferRecoveryPage {
            records: Vec::new(),
            next_cursor: None,
        })
    }
}

/// 生成合法业务事实，不包含凭据、运行时 owner 或真实用户目录。
fn recovery_record() -> TransferRecoveryRecord {
    TransferRecoveryRecord {
        id: TransferId::new(),
        request: TransferRequest {
            source: FileEndpoint::Local {
                path: "fixture-source".into(),
            },
            destination: FileEndpoint::Local {
                path: "fixture-destination".into(),
            },
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        },
        actual_destination: Some(FileEndpoint::Local {
            path: "fixture-destination (1)".into(),
        }),
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: false,
        commit_unknown: true,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    }
}

/// 使用显式注入的仓储建立服务，不运行真实 IO 或创建第二套 runtime。
fn service(
    repository: Arc<dyn TransferRecoveryRepository>,
    executions: Arc<AtomicUsize>,
) -> TransferService {
    TransferService::new_with_recovery_repository(
        vec![Arc::new(ReadOnlyBackend { executions })],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(SftpPerformanceSettings::default()).expect("fixture policy"),
        repository,
        None,
    )
    .expect("fixture queue")
}

/// 重启和冷归档历史均不创建 worker；归档后核对入口必须拒绝，且保留此前的证据结论。
/// @author kongweiguang
#[tokio::test]
async fn restart_inspection_never_replays_transfer() {
    let repository = Arc::new(InMemoryTransferRecoveryRepository::default());
    let record = recovery_record();
    repository
        .upsert(record.clone(), CancellationToken::new())
        .await
        .expect("save fixture");
    let executions = Arc::new(AtomicUsize::new(0));
    let queue = service(repository.clone(), executions.clone());
    assert!(queue.list().await.is_empty());
    assert_eq!(
        queue
            .list_recovery(CancellationToken::new())
            .await
            .expect("recovery")
            .len(),
        1
    );
    let inspected = queue
        .inspect_recovery(&record.id, CancellationToken::new())
        .await
        .expect("read-only inspect");
    assert_eq!(
        inspected.inspection,
        TransferRecoveryInspection::DestinationMissing
    );
    assert!(inspected.commit_unknown);
    queue
        .archive_recovery(&record.id, CancellationToken::new())
        .await
        .expect("archive evidence");
    assert!(
        queue
            .list_archived_recovery(None, 50, CancellationToken::new())
            .await
            .expect("archive history")
            .records[0]
            .archived
    );
    assert!(
        repository
            .list_archived(None, 50, CancellationToken::new())
            .await
            .expect("archive retained")
            .records[0]
            .archived
    );
    assert!(
        queue
            .list_recovery(CancellationToken::new())
            .await
            .expect("active list")
            .is_empty()
    );
    assert!(
        queue
            .inspect_recovery(&record.id, CancellationToken::new())
            .await
            .is_err()
    );
    let page = queue
        .list_archived_recovery(None, 50, CancellationToken::new())
        .await
        .expect("unchanged archive");
    assert_eq!(
        page.records[0].inspection,
        TransferRecoveryInspection::DestinationMissing
    );
    assert!(
        repository
            .upsert(record, CancellationToken::new())
            .await
            .is_err()
    );
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

/// 恢复材料未持久化时拒绝入队，不能先写目标再报告记录失败。
#[tokio::test]
async fn persistence_failure_prevents_backend_execution() {
    let executions = Arc::new(AtomicUsize::new(0));
    let queue = service(Arc::new(FailingRepository), executions.clone());
    assert!(queue.enqueue(recovery_record().request).await.is_err());
    assert!(queue.list().await.is_empty());
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

/// 冷归档不会抛弃旧资源 owner，材料改由有界历史入口读取而不再占 active 容量。
/// @author kongweiguang
#[tokio::test]
async fn archive_keeps_unresolved_cleanup_visible() {
    let repository = Arc::new(InMemoryTransferRecoveryRepository::default());
    let mut record = recovery_record();
    record.cleanup_pending = true;
    repository
        .upsert(record.clone(), CancellationToken::new())
        .await
        .expect("save fixture");
    let queue = service(repository, Arc::new(AtomicUsize::new(0)));
    queue
        .inspect_recovery(&record.id, CancellationToken::new())
        .await
        .expect("inspect");
    queue
        .archive_recovery(&record.id, CancellationToken::new())
        .await
        .expect("archive retained owner");
    let records = queue
        .list_archived_recovery(None, 50, CancellationToken::new())
        .await
        .expect("pending owner");
    assert!(records.records[0].archived && records.records[0].cleanup_pending);
}

/// 目录或临时目标可能永远缺少自动核对证据，显式人工归档仍保留未知事实而不是制造成功。
/// @author kongweiguang
#[tokio::test]
async fn manual_archive_allows_insufficient_evidence_without_changing_facts() {
    let repository = Arc::new(InMemoryTransferRecoveryRepository::default());
    let mut record = recovery_record();
    record.request.kind = TransferKind::Directory;
    record.cleanup_pending = true;
    repository
        .upsert(record.clone(), CancellationToken::new())
        .await
        .expect("save unknown fixture");
    let executions = Arc::new(AtomicUsize::new(0));
    let queue = service(repository, executions.clone());
    queue
        .archive_recovery(&record.id, CancellationToken::new())
        .await
        .expect("manual archive");
    let archived = queue
        .list_archived_recovery(None, 50, CancellationToken::new())
        .await
        .expect("archived history")
        .records
        .remove(0);
    assert!(archived.archived && archived.commit_unknown && archived.cleanup_pending);
    assert_eq!(
        archived.inspection,
        TransferRecoveryInspection::Insufficient
    );
    assert_eq!(archived.checkpoint, record.checkpoint);
    assert_eq!(executions.load(Ordering::SeqCst), 0);
}

/// 首个可信字节断点之前也必须保留创建意图，失败回执不能被误当允许创建暂存。
#[tokio::test]
async fn staging_registration_failure_prevents_first_staging_creation() {
    let repository = Arc::new(FailSecondWriteRepository {
        writes: AtomicUsize::new(0),
        inner: InMemoryTransferRecoveryRepository::default(),
    });
    let side_effects = Arc::new(AtomicUsize::new(0));
    let queue = TransferService::new_with_recovery_repository(
        vec![Arc::new(StagingBackend {
            side_effects: side_effects.clone(),
        })],
        tokio::runtime::Handle::current(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(SftpPerformanceSettings::default()).expect("fixture policy"),
        repository.clone(),
        None,
    )
    .expect("fixture queue");
    let queued = queue
        .enqueue(recovery_record().request)
        .await
        .expect("initial material persists");
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            if matches!(
                queue.get(&queued.id).await.expect("owned task").status,
                TransferStatus::Failed | TransferStatus::Cancelled
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controlled worker stops");
    queue.shutdown().await.expect("worker ownership released");
    assert_eq!(side_effects.load(Ordering::SeqCst), 0);
    let records = repository
        .list(CancellationToken::new())
        .await
        .expect("retained create intent");
    assert_eq!(records[0].staging_ownership.len(), 1);
    assert!(records[0].checkpoint.is_none());
}
