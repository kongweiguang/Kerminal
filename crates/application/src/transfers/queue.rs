//! 有界传输队列：并发、幂等重试、取消与 idle 恢复在应用层形成闭环。
//!
//! @author kongweiguang

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use kerminal_domain::{
    files::{
        TransferCommitSummary, TransferFailureKind, TransferRequest, TransferStatus,
        TransferSummary, TransferTask,
    },
    identity::{HostId, TransferId},
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, broadcast, watch},
    task::JoinHandle,
    time::{Instant, sleep, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::{
    InMemoryTransferRecoveryRepository, SftpTransferPolicySnapshot, SftpTransferPolicyStore,
    TransferBackend, TransferCheckpoint, TransferCleanupState, TransferControlRequest,
    TransferExecutionContext, TransferExecutionError, TransferExecutionFailure, TransferPhase,
    TransferProgressReporter, TransferProgressUpdate, TransferQueueEvent,
    TransferQueueSubscription, TransferRecoveryInspection, TransferRecoveryRecord,
    TransferRecoveryRepository, TransferResourceCleanup, TransferStagingOwnership,
};
use crate::{
    AppError, AppResult,
    connections::management::{HostManagementService, HostSftpOptions},
    platform::external_launch::ExternalTargetLease,
    preferences::SftpPerformanceSettings,
};

/// 队列的资源预算；桌面装配层显式传入，测试可用短 idle 阈值覆盖慢后端。
#[derive(Clone, Debug)]
pub struct TransferQueueOptions {
    pub max_concurrent: usize,
    pub max_concurrent_per_host: usize,
    pub max_queued: usize,
    pub max_history: usize,
    pub idle_timeout: Duration,
    pub recovery_delay: Duration,
    pub shutdown_timeout: Duration,
}

impl Default for TransferQueueOptions {
    /// 默认并发保持保守，长时间无确认进度时只尝试一次可信断点恢复。
    fn default() -> Self {
        Self {
            max_concurrent: 3,
            max_concurrent_per_host: 2,
            max_queued: 100,
            max_history: 500,
            idle_timeout: Duration::from_secs(180),
            recovery_delay: Duration::from_secs(2),
            shutdown_timeout: Duration::from_secs(10),
        }
    }
}

/// 可克隆队列服务句柄；每个传输后端负责协议 IO，队列集中维护业务状态。
#[derive(Clone)]
pub struct TransferService {
    inner: Arc<TransferServiceInner>,
}

struct TransferServiceInner {
    backends: Arc<[Arc<dyn TransferBackend>]>,
    runtime: Handle,
    options: TransferQueueOptions,
    sftp_policy: SftpTransferPolicyStore,
    recovery_repository: Arc<dyn TransferRecoveryRepository>,
    host_management: Option<Arc<HostManagementService>>,
    records: Mutex<BTreeMap<TransferId, TransferRecord>>,
    host_slots: Mutex<HashMap<HostId, Arc<Semaphore>>>,
    global_slots: Arc<Semaphore>,
    events: broadcast::Sender<TransferQueueEvent>,
    next_sequence: AtomicU64,
    shutting_down: AtomicBool,
    intake_lock: Mutex<()>,
    shutdown_lock: Mutex<()>,
    shutdown_workers: Mutex<Vec<ShutdownWorkerOwner>>,
    shutdown_deadline: watch::Sender<Option<Instant>>,
}

struct TransferRecord {
    task: TransferTask,
    checkpoint: Option<TransferCheckpoint>,
    staging_ownership: Vec<TransferStagingOwnership>,
    sftp_policy: Option<SftpTransferPolicySnapshot>,
    source_sftp: Option<HostSftpOptions>,
    destination_sftp: Option<HostSftpOptions>,
    /// 阶段仅是内存投影，不影响业务字节、超时活动序号或提交状态机。
    phase: Option<TransferPhase>,
    /// 入队时捕获的阈值属于本任务；设置热更新只影响之后新建的传输。
    effective_idle_timeout_seconds: u64,
    /// 仅用本任务实际新确认字节计算吞吐，不把恢复前缀计入速度。
    speed_bytes_per_second: u64,
    last_progress_at: Option<u64>,
    progress_sample: Option<(u64, Instant)>,
    /// 活动任务保留临时目标；只有未知提交或未确认清理时才跨终态保留。
    external_target_lease: Option<ExternalTargetLease>,
    /// 未确认资源由原后端 owner 保留；不能仅靠释放临时目标凭据假设连接已关闭。
    cleanup_owner: Option<Arc<dyn TransferResourceCleanup>>,
    cancellation: CancellationToken,
    commit_finalized: bool,
    no_changes_finalized: bool,
    /// 只有后端 future 已返回才能删除恢复材料；数据屏障成功不等于资源收尾完成。
    attempt_finished: bool,
    worker: Option<JoinHandle<()>>,
    sequence: u64,
}

impl TransferRecord {
    /// 将领域状态与应用层资源所有权合并投影，避免领域模型依赖运行时清理机制。
    ///
    /// @author kongweiguang
    fn summary(&self) -> TransferSummary {
        let mut summary = self.task.summary();
        summary.cleanup_pending = self.cleanup_owner.is_some();
        summary.phase = match summary.status {
            TransferStatus::Connecting
            | TransferStatus::Transferring
            | TransferStatus::Committing
            | TransferStatus::Cancelling => self.phase,
            _ => None,
        };
        summary.speed_bytes_per_second = self.speed_bytes_per_second;
        summary.last_progress_at = self.last_progress_at;
        summary.effective_idle_timeout_seconds = Some(self.effective_idle_timeout_seconds);
        summary
    }
}

/// 关闭协调器保留 worker 的异常 join 事实，避免后续调用误报清洁关闭。
enum ShutdownWorkerOwner {
    Running {
        id: TransferId,
        handle: JoinHandle<()>,
    },
    JoinFailed {
        id: TransferId,
        cancelled: bool,
        panicked: bool,
    },
}

/// 单轮执行的输入快照，隔离队列协调参数与后端可见的执行上下文。
///
/// @author kongweiguang
struct TransferAttemptInput<'a> {
    id: &'a TransferId,
    backend: Arc<dyn TransferBackend>,
    request: TransferRequest,
    resume_from: Option<TransferCheckpoint>,
    actual_destination: Option<kerminal_domain::files::FileEndpoint>,
    cancellation: CancellationToken,
    sftp_policy: Option<SftpTransferPolicySnapshot>,
    source_sftp: Option<HostSftpOptions>,
    destination_sftp: Option<HostSftpOptions>,
    effective_idle_timeout_seconds: u64,
}

enum AttemptOutcome {
    Completed(super::TransferExecutionResult),
    Failed(TransferExecutionFailure),
    Cancelled {
        checkpoint: Option<TransferCheckpoint>,
        actual_destination: Option<kerminal_domain::files::FileEndpoint>,
    },
    Idle(TransferProgressUpdate),
}

enum IdleAction {
    Recover,
    Finished,
}

struct AttemptPermit {
    _sftp: Option<super::policy::SftpTransferPermit>,
    _other: Vec<OwnedSemaphorePermit>,
}

impl TransferService {
    /// 校验所有预算并冻结后端集合，避免服务启动后出现无界队列或零并发死锁。
    pub fn new(
        backends: Vec<Arc<dyn TransferBackend>>,
        runtime: Handle,
        options: TransferQueueOptions,
    ) -> AppResult<Self> {
        let global_transfers = options.max_concurrent.clamp(1, 16);
        let host_transfers = options
            .max_concurrent_per_host
            .clamp(1, 8)
            .min(global_transfers);
        let policy = SftpTransferPolicyStore::new(SftpPerformanceSettings {
            global_transfers,
            host_transfers,
            ..SftpPerformanceSettings::default()
        })?;
        Self::new_with_sftp_policy(backends, runtime, options, policy)
    }

    /// 复用 composition root 的策略 store，使设置热更新能影响后续准入与新任务快照。
    pub fn new_with_sftp_policy(
        backends: Vec<Arc<dyn TransferBackend>>,
        runtime: Handle,
        options: TransferQueueOptions,
        sftp_policy: SftpTransferPolicyStore,
    ) -> AppResult<Self> {
        Self::new_with_recovery_repository(
            backends,
            runtime,
            options,
            sftp_policy,
            Arc::new(InMemoryTransferRecoveryRepository::default()),
            None,
        )
    }

    /// 正式装配显式传入恢复仓储与主机目录；构造期不做 IO，恢复列表读取失败可在界面原位重试。
    pub fn new_with_recovery_repository(
        backends: Vec<Arc<dyn TransferBackend>>,
        runtime: Handle,
        options: TransferQueueOptions,
        sftp_policy: SftpTransferPolicyStore,
        recovery_repository: Arc<dyn TransferRecoveryRepository>,
        host_management: Option<Arc<HostManagementService>>,
    ) -> AppResult<Self> {
        if backends.is_empty()
            || options.max_concurrent == 0
            || options.max_concurrent > 32
            || options.max_concurrent_per_host == 0
            || options.max_concurrent_per_host > options.max_concurrent
            || options.max_queued == 0
            || options.max_queued > 1_000
            || options.max_history == 0
            || options.max_history > 5_000
            || options.idle_timeout.is_zero()
            || options.idle_timeout > Duration::from_secs(3_600)
            || options.recovery_delay > Duration::from_secs(30)
            || options.shutdown_timeout.is_zero()
            || options.shutdown_timeout > Duration::from_secs(60)
        {
            return Err(AppError::invalid("传输队列资源预算无效"));
        }
        let (events, _) = broadcast::channel(256);
        let (shutdown_deadline, _) = watch::channel(None);
        Ok(Self {
            inner: Arc::new(TransferServiceInner {
                backends: backends.into(),
                runtime,
                options: options.clone(),
                sftp_policy,
                recovery_repository,
                host_management,
                records: Mutex::new(BTreeMap::new()),
                host_slots: Mutex::new(HashMap::new()),
                global_slots: Arc::new(Semaphore::new(options.max_concurrent)),
                events,
                next_sequence: AtomicU64::new(1),
                shutting_down: AtomicBool::new(false),
                intake_lock: Mutex::new(()),
                shutdown_lock: Mutex::new(()),
                shutdown_workers: Mutex::new(Vec::new()),
                shutdown_deadline,
            }),
        })
    }

    /// 与退出 intake 锁串行化入队和 worker 登记，避免 shutdown 漏掉刚创建的后台任务。
    ///
    /// @author kongweiguang
    pub async fn enqueue(&self, request: TransferRequest) -> AppResult<TransferSummary> {
        self.enqueue_inner(request, None).await
    }

    /// 为一次性外部连接显式保留活动传输 scope，普通 MCP/保存主机请求不能冒用该入口。
    ///
    /// @author kongweiguang
    pub async fn enqueue_with_external_target(
        &self,
        request: TransferRequest,
        lease: &ExternalTargetLease,
    ) -> AppResult<TransferSummary> {
        let external_host = external_host_for_request(&request)?
            .ok_or_else(|| AppError::invalid("传输请求没有使用临时外部目标"))?;
        if &external_host != lease.host_id() {
            return Err(AppError::invalid("临时外部目标与传输端点不匹配"));
        }
        let retained_lease = lease.retain()?;
        self.enqueue_inner(request, Some(retained_lease)).await
    }

    /// 校验请求并固化本任务保护阈值，把临时 scope 放入 worker 所属记录以隔离 pane 生命周期。
    /// Local→Local 也走统一后端，但只有含 Remote 的任务才申请 SFTP 策略与主机配额。
    ///
    /// @author kongweiguang
    async fn enqueue_inner(
        &self,
        request: TransferRequest,
        external_target_lease: Option<ExternalTargetLease>,
    ) -> AppResult<TransferSummary> {
        let _intake = self.inner.intake_lock.lock().await;
        self.ensure_open()?;
        let request_external_host = external_host_for_request(&request)?;
        match (&request_external_host, &external_target_lease) {
            (Some(_), None) => {
                return Err(AppError::conflict(
                    "临时外部目标需要由当前文件工作区持有 lease 后入队",
                ));
            }
            (Some(host_id), Some(lease)) if host_id != lease.host_id() => {
                return Err(AppError::invalid("临时外部目标与传输端点不匹配"));
            }
            (None, Some(_)) => {
                return Err(AppError::invalid("传输请求没有使用临时外部目标"));
            }
            _ => {}
        }
        if request
            .idle_timeout_seconds
            .is_some_and(|seconds| !(30..=3_600).contains(&seconds))
        {
            return Err(AppError::invalid("传输无进度阈值必须在 30 至 3600 秒之间"));
        }
        let backend = self.backend_for(&request)?;
        let sftp_policy = backend
            .uses_sftp_policy(&request)
            .then(|| self.inner.sftp_policy.snapshot());
        let (source_sftp, destination_sftp) = if sftp_policy.is_some() {
            (
                self.sftp_options_for_endpoint(&request.source).await?,
                self.sftp_options_for_endpoint(&request.destination).await?,
            )
        } else {
            (None, None)
        };
        let effective_idle_timeout_seconds = request
            .idle_timeout_seconds
            .map(u64::from)
            .or_else(|| {
                sftp_policy
                    .as_ref()
                    .map(|policy| policy.idle_timeout().as_secs())
            })
            .unwrap_or(self.inner.options.idle_timeout.as_secs());

        let id = TransferId::new();
        let task = TransferTask::queued(id.clone(), request)
            .map_err(|_| AppError::invalid("传输源和目标不能相同"))?;
        let summary = {
            let mut records = self.inner.records.lock().await;
            let active_count = records
                .values()
                .filter(|record| !is_terminal(record.task.status()))
                .count();
            if active_count >= self.inner.options.max_queued + self.inner.options.max_concurrent {
                return Err(AppError::conflict("传输队列已达到容量上限"));
            }
            let record = TransferRecord {
                task,
                checkpoint: None,
                staging_ownership: Vec::new(),
                sftp_policy,
                source_sftp,
                destination_sftp,
                phase: None,
                effective_idle_timeout_seconds,
                speed_bytes_per_second: 0,
                last_progress_at: None,
                progress_sample: None,
                external_target_lease,
                cleanup_owner: None,
                cancellation: CancellationToken::new(),
                commit_finalized: false,
                no_changes_finalized: false,
                attempt_finished: false,
                worker: None,
                sequence: self.inner.next_sequence.fetch_add(1, Ordering::Relaxed),
            };
            self.persist_recovery_record(&record).await?;
            let summary = record.summary();
            records.insert(id.clone(), record);
            publish(&self.inner.events);
            // 首次返回值与列表查询共用同一快照，避免默认保护值在两处出现差异。
            summary
        };
        self.spawn_worker(id).await;
        Ok(summary)
    }

    /// 返回当前保留的任务快照；事件频道只作唤醒通知，调用方以此查询重建完整状态。
    pub async fn list(&self) -> Vec<TransferSummary> {
        self.inner
            .records
            .lock()
            .await
            .values()
            .map(|record| record.summary())
            .collect()
    }

    /// 恢复项只读加载，不恢复 worker 或 SSH lease；活跃任务仍由普通队列独占展示。
    pub async fn list_recovery(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        let mut recovery = self.inner.recovery_repository.list(cancellation).await?;
        let records = self.inner.records.lock().await;
        recovery.retain(|record| {
            records.get(&record.id).is_none_or(|live| {
                is_terminal(live.task.status())
                    && (live.task.failure_kind() == Some(TransferFailureKind::CommitUnknown)
                        || live.cleanup_owner.is_some())
            })
        });
        Ok(recovery)
    }

    /// 对原事务做只读证据核对；运行中 owner 不可核对，也不会由此启动第二次传输。
    /// 已归档事实不再交给 backend，避免证据不足短路把旧结论覆盖掉。
    /// @author kongweiguang
    pub async fn inspect_recovery(
        &self,
        id: &TransferId,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryRecord> {
        let _shutdown = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("传输恢复核对已取消")),
            guard = self.inner.shutdown_lock.lock() => guard,
        };
        let _intake = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("传输恢复核对已取消")),
            guard = self.inner.intake_lock.lock() => guard,
        };
        self.ensure_open()?;
        self.ensure_recovery_idle(id).await?;
        let mut record = self
            .inner
            .recovery_repository
            .list(cancellation.clone())
            .await?
            .into_iter()
            .find(|record| &record.id == id)
            .ok_or_else(|| {
                AppError::not_found("传输恢复记录不存在或已归档；归档证据不能重新核对")
            })?;
        if record.archived {
            return Err(AppError::conflict("归档证据不能重新核对或覆盖原结论"));
        }
        if external_host_for_request(&record.request)?.is_some() {
            return Err(AppError::conflict(
                "临时外部目标已失效；保留记录并人工核对原目标",
            ));
        }
        record.inspection = self
            .backend_for(&record.request)?
            .inspect_recovery(&record, cancellation.clone())
            .await?;
        self.inner
            .recovery_repository
            .upsert(record.clone(), cancellation)
            .await?;
        publish(&self.inner.events);
        Ok(record)
    }

    /// 归档由仓储先耐久保存再释放待恢复容量；活 cleanup owner 必须先清理，不能冒充传输成功。
    /// @author kongweiguang
    pub async fn archive_recovery(
        &self,
        id: &TransferId,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        let _shutdown = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("传输恢复归档已取消")),
            guard = self.inner.shutdown_lock.lock() => guard,
        };
        let _intake = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(AppError::cancelled("传输恢复归档已取消")),
            guard = self.inner.intake_lock.lock() => guard,
        };
        self.ensure_open()?;
        self.ensure_recovery_idle(id).await?;
        if self
            .inner
            .records
            .lock()
            .await
            .get(id)
            .is_some_and(|live| live.cleanup_owner.is_some())
        {
            return Err(AppError::conflict("原传输仍有资源 owner，请先重试清理"));
        }
        self.inner
            .recovery_repository
            .archive(id, cancellation)
            .await?;
        let mut records = self.inner.records.lock().await;
        if records.get(id).is_some_and(|live| {
            is_terminal(live.task.status())
                && live.cleanup_owner.is_none()
                && live.external_target_lease.is_none()
                && live
                    .worker
                    .as_ref()
                    .is_none_or(|worker| worker.is_finished())
        }) {
            records.remove(id);
        }
        publish(&self.inner.events);
        Ok(())
    }

    /// 冷历史按有界页读取，与活队列和核对入口隔离，不会恢复 worker 或改变归档结论。
    /// @author kongweiguang
    pub async fn list_archived_recovery(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<super::TransferRecoveryPage> {
        super::recovery::validate_archive_page_limit(limit)?;
        self.inner
            .recovery_repository
            .list_archived(cursor, limit, cancellation)
            .await
    }

    /// 原 worker 尚未退出时即使投影为终态，也不允许恢复写入覆盖其迟到回执。
    async fn ensure_recovery_idle(&self, id: &TransferId) -> AppResult<()> {
        let records = self.inner.records.lock().await;
        if records.get(id).is_some_and(|record| {
            !is_terminal(record.task.status())
                || record
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished())
        }) {
            Err(AppError::conflict("原传输仍在收尾，请完成后再核对"))
        } else {
            Ok(())
        }
    }

    /// 主机默认值只在准入时捕获，禁用 SFTP 不影响 SSH 终端，也不强杀在途任务。
    async fn sftp_options_for_endpoint(
        &self,
        endpoint: &kerminal_domain::files::FileEndpoint,
    ) -> AppResult<Option<HostSftpOptions>> {
        let kerminal_domain::files::FileEndpoint::Remote { host_id, .. } = endpoint else {
            return Ok(None);
        };
        if host_id.as_str().starts_with("external:") {
            return Ok(None);
        }
        let Some(management) = &self.inner.host_management else {
            return Ok(None);
        };
        let options = management
            .load_host(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("传输主机不存在"))?
            .sftp_options;
        if !options.enabled {
            return Err(AppError::unsupported(
                "此主机已禁用 SFTP，请先在连接设置中启用",
            ));
        }
        Ok(Some(options))
    }

    /// 在持锁状态下提交完整恢复快照，保证实际目标及 owner 先于提交许可落盘，旧进度不会覆盖新状态。
    async fn persist_recovery_record(&self, record: &TransferRecord) -> AppResult<()> {
        let summary = record.summary();
        let cancellation = CancellationToken::new();
        if matches!(
            summary.status,
            TransferStatus::Succeeded | TransferStatus::Skipped | TransferStatus::Unchanged
        ) && record.cleanup_owner.is_none()
            && record.attempt_finished
        {
            return self
                .inner
                .recovery_repository
                .remove(&summary.id, cancellation)
                .await;
        }
        let commit_unknown = summary.failure_kind == Some(TransferFailureKind::CommitUnknown)
            || record.task.commit_item_in_progress();
        let mut checkpoint = record.checkpoint.clone();
        if commit_unknown && checkpoint.is_none() {
            checkpoint = self
                .inner
                .recovery_repository
                .list(cancellation.clone())
                .await?
                .into_iter()
                .find(|saved| saved.id == summary.id)
                .and_then(|saved| saved.checkpoint);
        }
        self.inner
            .recovery_repository
            .upsert(
                TransferRecoveryRecord {
                    id: summary.id,
                    request: summary.request,
                    actual_destination: summary.actual_destination,
                    checkpoint,
                    staging_ownership: record.staging_ownership.clone(),
                    cleanup_pending: record.cleanup_owner.is_some(),
                    commit_unknown,
                    inspection: TransferRecoveryInspection::Insufficient,
                    archived: false,
                },
                cancellation,
            )
            .await
    }

    /// 提供当前 SFTP 策略默认值供新任务表单使用；已入队摘要使用各自冻结的阈值。
    ///
    /// @author kongweiguang
    pub fn default_idle_timeout_seconds(&self) -> u64 {
        self.inner.sftp_policy.snapshot().idle_timeout().as_secs()
    }

    /// 按 ID 读取单个状态快照，未知 ID 采用脱敏 not-found 错误。
    pub async fn get(&self, id: &TransferId) -> AppResult<TransferSummary> {
        self.inner
            .records
            .lock()
            .await
            .get(id)
            .map(|record| record.summary())
            .ok_or_else(|| AppError::not_found("传输任务不存在"))
    }

    /// 先 join 可清理终态任务的 worker，再移除历史；删除线性化前可取消，并保留待核对提交与待清理 owner。
    ///
    /// @author kongweiguang
    pub async fn clear_completed(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<(usize, usize)> {
        let deadline = Instant::now() + self.inner.options.shutdown_timeout;
        let _shutdown = cancellable_clear(&cancellation, async {
            timeout_at(deadline, self.inner.shutdown_lock.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输清理锁"))
        })
        .await?;
        let _intake = cancellable_clear(&cancellation, async {
            timeout_at(deadline, self.inner.intake_lock.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输入队锁"))
        })
        .await?;
        self.ensure_open()?;

        // 共享 worker 容器在等待期间继续持有句柄，超时返回也不会 detach 后丢失所有权。
        let mut owned_workers = cancellable_clear(&cancellation, async {
            timeout_at(deadline, self.inner.shutdown_workers.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输 worker 所有权锁"))
        })
        .await?;
        let terminal_ids = {
            let mut records = cancellable_clear(&cancellation, async {
                timeout_at(deadline, self.inner.records.lock())
                    .await
                    .map_err(|_| AppError::timeout("等待传输记录锁"))
            })
            .await?;
            let mut terminal_ids = Vec::new();
            for (id, record) in records.iter_mut() {
                if is_removable_history_record(record) {
                    terminal_ids.push(id.clone());
                    if let Some(handle) = record.worker.take() {
                        owned_workers.push(ShutdownWorkerOwner::Running {
                            id: id.clone(),
                            handle,
                        });
                    }
                }
            }
            terminal_ids
        };

        let mut join_failed = false;
        let mut index = 0;
        while index < owned_workers.len() {
            let id = match &owned_workers[index] {
                ShutdownWorkerOwner::Running { id, .. }
                | ShutdownWorkerOwner::JoinFailed { id, .. } => id.clone(),
            };
            let is_terminal_id = terminal_ids.contains(&id);
            if !is_terminal_id
                && matches!(&owned_workers[index], ShutdownWorkerOwner::Running { .. })
            {
                index += 1;
                continue;
            }
            let joined = match &mut owned_workers[index] {
                ShutdownWorkerOwner::Running { handle, .. } => {
                    cancellable_clear(&cancellation, async {
                        Ok(if handle.is_finished() {
                            Some((&mut *handle).await)
                        } else {
                            match timeout_at(deadline, &mut *handle).await {
                                Ok(result) => Some(result),
                                Err(_) if handle.is_finished() => Some((&mut *handle).await),
                                Err(_) => None,
                            }
                        })
                    })
                    .await?
                }
                ShutdownWorkerOwner::JoinFailed {
                    cancelled,
                    panicked,
                    ..
                } => {
                    cancellable_clear(
                        &cancellation,
                        self.record_worker_join_failure_until(&id, *cancelled, *panicked, deadline),
                    )
                    .await?;
                    join_failed = true;
                    index += 1;
                    continue;
                }
            };
            match joined {
                None => {
                    return Err(AppError::timeout(
                        "终态传输 worker 清理超过期限；JoinHandle 已保留供后续清理",
                    ));
                }
                Some(Ok(())) => {
                    owned_workers.remove(index);
                }
                Some(Err(error)) => {
                    let cancelled = error.is_cancelled();
                    let panicked = error.is_panic();
                    tracing::error!(
                        transfer_id = %id,
                        cancelled,
                        panicked,
                        "清理传输历史时 worker join 失败"
                    );
                    owned_workers[index] = ShutdownWorkerOwner::JoinFailed {
                        id: id.clone(),
                        cancelled,
                        panicked,
                    };
                    cancellable_clear(
                        &cancellation,
                        self.record_worker_join_failure_until(&id, cancelled, panicked, deadline),
                    )
                    .await?;
                    join_failed = true;
                    index += 1;
                }
            }
        }
        if join_failed {
            return Err(AppError::infrastructure("传输历史 worker 未能正常结束"));
        }

        let mut records = cancellable_clear(&cancellation, async {
            timeout_at(deadline, self.inner.records.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输记录清理"))
        })
        .await?;
        // 记录锁内的最后一次检查作为删除线性化点；之后的取消不能回滚已移除的历史。
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("传输历史清理已取消"));
        }
        let removed = terminal_ids
            .into_iter()
            .filter(|id| {
                records.get(id).is_some_and(|record| {
                    is_removable_history_record(record) && record.worker.is_none()
                })
            })
            .collect::<Vec<_>>();
        for id in &removed {
            records.remove(id);
        }
        let remaining = records.len();
        if !removed.is_empty() {
            let _ = self.inner.events.send(TransferQueueEvent::Cleared);
        }
        Ok((removed.len(), remaining))
    }

    /// 取消等待任务或登记提交中的延迟取消，避免打断已经获准的目标原子操作。
    pub async fn cancel(&self, id: &TransferId) -> AppResult<TransferSummary> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        match record.task.status() {
            TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled => {
                return Ok(record.summary());
            }
            _ => {}
        }
        if record.task.request_cancel() {
            if !record.task.commit_item_in_progress() {
                record.cancellation.cancel();
            }
            let summary = record.summary();
            publish(&self.inner.events);
            Ok(summary)
        } else {
            Ok(record.summary())
        }
    }

    /// 在 intake 锁内创建唯一后继任务；重复调用返回既有后继并保留可信检查点与 rename 目标。
    pub async fn retry(&self, id: &TransferId) -> AppResult<TransferSummary> {
        self.retry_inner(id, None).await
    }

    /// 临时目标任务只在调用者仍持有匹配 scope 时创建后继，不把一次性凭据留在历史记录。
    ///
    /// @author kongweiguang
    pub async fn retry_with_external_target(
        &self,
        id: &TransferId,
        lease: &ExternalTargetLease,
    ) -> AppResult<TransferSummary> {
        self.retry_inner(id, Some(lease)).await
    }

    /// 使用同一资源 owner 重试关闭，并在确认释放后才释放临时目标 scope。
    ///
    /// @author kongweiguang
    pub async fn retry_cleanup_until(&self, id: &TransferId, deadline: Instant) -> AppResult<()> {
        let _shutdown = timeout_at(deadline, self.inner.shutdown_lock.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输清理锁"))?;
        self.retry_cleanup_locked(id, deadline).await
    }

    /// shutdown 与用户清理重试共用此路径；owner 仅在获得明确释放回执后从记录移除。
    ///
    /// @author kongweiguang
    async fn retry_cleanup_locked(&self, id: &TransferId, deadline: Instant) -> AppResult<()> {
        let owner = {
            let records = timeout_at(deadline, self.inner.records.lock())
                .await
                .map_err(|_| AppError::timeout("等待传输清理记录锁"))?;
            let record = records
                .get(id)
                .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
            record.cleanup_owner.as_ref().cloned()
        };
        let Some(owner) = owner else {
            return Ok(());
        };
        timeout_at(deadline, owner.cleanup_until(deadline))
            .await
            .map_err(|_| AppError::timeout("传输资源清理超过期限"))??;

        let mut records = timeout_at(deadline, self.inner.records.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输清理回执锁"))?;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        let still_same_owner = record
            .cleanup_owner
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &owner));
        if !still_same_owner {
            return Err(AppError::conflict("传输资源 owner 已变化，未确认清理结果"));
        }
        record.cleanup_owner = None;
        record.external_target_lease.take();
        self.persist_recovery_record(record).await?;
        publish(&self.inner.events);
        Ok(())
    }

    /// 复用原请求和可信检查点；外部目标凭据必须由当前工作区重新提供并独立 retain。
    ///
    /// @author kongweiguang
    async fn retry_inner(
        &self,
        id: &TransferId,
        external_lease: Option<&ExternalTargetLease>,
    ) -> AppResult<TransferSummary> {
        let _intake = self.inner.intake_lock.lock().await;
        self.ensure_open()?;
        let successor = TransferId::new();
        {
            let mut records = self.inner.records.lock().await;
            let old = records
                .get(id)
                .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
            if old.task.summary().failure_kind == Some(TransferFailureKind::CommitUnknown) {
                return Err(AppError::outcome_unknown("提交结果未确认，不能安全重试"));
            }
            if old.cleanup_owner.is_some() {
                return Err(AppError::conflict("传输资源尚未确认释放，不能安全重试"));
            }
            if let Some(existing) = old.task.retry_successor() {
                return records
                    .get(existing)
                    .map(|record| record.summary())
                    .ok_or_else(|| AppError::infrastructure("传输后继记录缺失"));
            }
            if !old.task.retryable() {
                return Err(AppError::conflict("该传输结果不能安全重试"));
            }
            let external_target_lease = match external_host_for_request(old.task.request())? {
                Some(host_id) => {
                    let lease = external_lease.ok_or_else(|| {
                        AppError::conflict("临时外部目标已释放；请重新接入后再重试")
                    })?;
                    if lease.host_id() != &host_id {
                        return Err(AppError::invalid("重试目标与当前临时外部连接不匹配"));
                    }
                    Some(lease.retain()?)
                }
                None => {
                    if external_lease.is_some() {
                        return Err(AppError::invalid("重试请求没有使用临时外部目标"));
                    }
                    None
                }
            };
            let active_count = records
                .values()
                .filter(|record| !is_terminal(record.task.status()))
                .count();
            if active_count >= self.inner.options.max_queued + self.inner.options.max_concurrent {
                return Err(AppError::conflict("传输队列已达到容量上限"));
            }
            let request = old.task.request().clone();
            let effective_idle_timeout_seconds = old.effective_idle_timeout_seconds;
            let source_sftp = old.source_sftp.clone();
            let destination_sftp = old.destination_sftp.clone();
            let backend = self.backend_for(&request)?;
            let sftp_policy = backend
                .uses_sftp_policy(&request)
                .then(|| self.inner.sftp_policy.snapshot());
            let checkpoint = if old.task.resumable() {
                old.checkpoint.clone()
            } else {
                None
            };
            let stable_destination = old.task.actual_destination().cloned();
            let staging_ownership = old.staging_ownership.clone();
            let mut task = TransferTask::queued(successor.clone(), request)
                .map_err(|_| AppError::invalid("传输请求已失效"))?;
            if let Some(checkpoint) = &checkpoint {
                task.restore_confirmed_progress(
                    checkpoint.confirmed_bytes,
                    Some(checkpoint.source_revision.size),
                )
                .map_err(|_| AppError::conflict("可信传输断点不符合任务大小"))?;
            }
            if let Some(destination) = stable_destination {
                task.select_actual_destination(destination)
                    .map_err(|_| AppError::conflict("无法保留原传输目标"))?;
            }
            let record = TransferRecord {
                task,
                checkpoint,
                staging_ownership,
                sftp_policy,
                source_sftp,
                destination_sftp,
                phase: None,
                effective_idle_timeout_seconds,
                speed_bytes_per_second: 0,
                last_progress_at: None,
                progress_sample: None,
                external_target_lease,
                cleanup_owner: None,
                cancellation: CancellationToken::new(),
                commit_finalized: false,
                no_changes_finalized: false,
                attempt_finished: false,
                worker: None,
                sequence: self.inner.next_sequence.fetch_add(1, Ordering::Relaxed),
            };
            self.persist_recovery_record(&record).await?;
            records
                .get_mut(id)
                .ok_or_else(|| AppError::not_found("传输任务不存在"))?
                .task
                .set_retry_successor(successor.clone())
                .map_err(|_| AppError::conflict("该传输任务已被重试"))?;
            records.insert(successor.clone(), record);
            publish(&self.inner.events);
        }
        self.spawn_worker(successor.clone()).await;
        self.get(&successor).await
    }

    /// 订阅有界队列唤醒信号；丢帧时接收方仍通过 list 获取权威快照。
    ///
    /// @author kongweiguang
    pub fn subscribe(&self) -> TransferQueueSubscription {
        TransferQueueSubscription {
            receiver: self.inner.events.subscribe(),
        }
    }

    /// 使用队列默认预算关闭；desktop 可用 shutdown_until 把此队列纳入单一应用退出 deadline。
    pub async fn shutdown(&self) -> AppResult<()> {
        self.shutdown_until(Instant::now() + self.inner.options.shutdown_timeout)
            .await
    }

    /// 串行化并发关闭与入队，遵守调用方绝对 deadline；未完成 worker 留在服务句柄并返回清理超时。
    ///
    /// @author kongweiguang
    pub async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        let _shutdown = timeout_at(deadline, self.inner.shutdown_lock.lock())
            .await
            .map_err(|_| AppError::timeout("传输队列关闭等待"))?;
        self.inner.shutdown_deadline.send_replace(Some(deadline));
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.sftp_policy.close();

        // intake 临界区只登记请求与 worker；所有等待都使用同一绝对 deadline。
        let _intake = timeout_at(deadline, self.inner.intake_lock.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输入队锁关闭"))?;
        let mut records = timeout_at(deadline, self.inner.records.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输记录锁关闭"))?;
        let mut workers = timeout_at(deadline, self.inner.shutdown_workers.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输 worker 所有权锁关闭"))?;
        let mut pending = std::mem::take(&mut *workers);
        for (id, record) in records.iter_mut() {
            if !is_terminal(record.task.status()) && record.task.request_cancel() {
                if !record.task.commit_item_in_progress() {
                    record.cancellation.cancel();
                }
                publish(&self.inner.events);
            }
            if let Some(handle) = record.worker.take() {
                pending.push(ShutdownWorkerOwner::Running {
                    id: id.clone(),
                    handle,
                });
            }
        }
        drop(records);

        // 暂时保留 shutdown_workers 的唯一所有权，deadline 到时把未完成 JoinHandle 放回。
        let mut join_failed = false;
        let mut remaining = Vec::new();
        let mut pending = pending.into_iter();
        while let Some(owner) = pending.next() {
            let (id, join_result) = match owner {
                ShutdownWorkerOwner::Running { id, mut handle } => {
                    let join_result = if handle.is_finished() {
                        Some((&mut handle).await)
                    } else {
                        match timeout_at(deadline, &mut handle).await {
                            Ok(result) => Some(result),
                            Err(_) if handle.is_finished() => Some((&mut handle).await),
                            Err(_) => None,
                        }
                    };
                    match &join_result {
                        Some(Err(error)) => remaining.push(ShutdownWorkerOwner::JoinFailed {
                            id: id.clone(),
                            cancelled: error.is_cancelled(),
                            panicked: error.is_panic(),
                        }),
                        None => remaining.push(ShutdownWorkerOwner::Running {
                            id: id.clone(),
                            handle,
                        }),
                        _ => {}
                    }
                    (id, join_result)
                }
                ShutdownWorkerOwner::JoinFailed {
                    id,
                    cancelled,
                    panicked,
                } => {
                    remaining.push(ShutdownWorkerOwner::JoinFailed {
                        id: id.clone(),
                        cancelled,
                        panicked,
                    });
                    join_failed = true;
                    if let Err(error) = self
                        .record_worker_join_failure_until(&id, cancelled, panicked, deadline)
                        .await
                    {
                        remaining.extend(pending);
                        *workers = remaining;
                        return Err(error);
                    }
                    continue;
                }
            };
            match join_result {
                Some(Ok(())) => {}
                Some(Err(error)) => {
                    let cancelled = error.is_cancelled();
                    let panicked = error.is_panic();
                    tracing::error!(
                        transfer_id = %id,
                        cancelled,
                        panicked,
                        "传输 worker 异常终止"
                    );
                    if let Err(record_error) = self
                        .record_worker_join_failure_until(&id, cancelled, panicked, deadline)
                        .await
                    {
                        remaining.extend(pending);
                        *workers = remaining;
                        return Err(record_error);
                    }
                    join_failed = true;
                }
                None => {}
            }
        }

        let workers_still_running = remaining
            .iter()
            .any(|owner| matches!(owner, ShutdownWorkerOwner::Running { .. }));
        *workers = remaining;
        if workers_still_running {
            return Err(AppError::timeout(
                "传输 worker 清理超过关闭期限；应用尚未完成资源关闭",
            ));
        }
        drop(workers);
        if join_failed {
            return Err(AppError::infrastructure(
                "传输 worker 异常终止，队列状态已标记为失败或提交结果未知",
            ));
        }
        drop(_intake);
        let cleanup_ids = {
            let records = timeout_at(deadline, self.inner.records.lock())
                .await
                .map_err(|_| AppError::timeout("等待待清理传输记录"))?;
            records
                .iter()
                .filter_map(|(id, record)| record.cleanup_owner.as_ref().map(|_| id.clone()))
                .collect::<Vec<_>>()
        };
        for id in cleanup_ids {
            self.retry_cleanup_locked(&id, deadline).await?;
        }
        Ok(())
    }

    /// 只允许在真实后端能力唯一匹配时路由请求，避免双适配器对同一目标产生竞态写入。
    fn backend_for(&self, request: &TransferRequest) -> AppResult<Arc<dyn TransferBackend>> {
        let mut supported = self
            .inner
            .backends
            .iter()
            .filter(|backend| backend.supports(request));
        let backend = supported
            .next()
            .cloned()
            .ok_or_else(|| AppError::unsupported("当前端点组合没有可用传输实现"))?;
        if supported.next().is_some() {
            return Err(AppError::infrastructure("传输请求匹配到多个后端"));
        }
        Ok(backend)
    }

    /// 队列服务停止后拒绝新的 enqueue/retry，避免退出期间再次产生后台任务。
    fn ensure_open(&self) -> AppResult<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            Err(AppError::conflict("传输队列正在关闭"))
        } else {
            Ok(())
        }
    }

    /// 持记录锁注册句柄后才允许 worker 读取任务，避免极快终态被裁剪并丢失 join 和恢复材料所有权。
    async fn spawn_worker(&self, id: TransferId) {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(&id) else {
            return;
        };
        let service = self.clone();
        let worker_id = id.clone();
        let handle = self.inner.runtime.spawn(async move {
            service.run_worker(worker_id).await;
        });
        record.worker = Some(handle);
    }

    /// 每轮只持有有界并发许可，idle 恢复等待期间释放连接配额让其它任务先运行。
    async fn run_worker(&self, id: TransferId) {
        self.run_worker_inner(id.clone()).await;
        let mut records = self.inner.records.lock().await;
        if let Some(record) = records.get_mut(&id) {
            record.attempt_finished = true;
            if self.persist_recovery_record(record).await.is_err() {
                tracing::error!(transfer_id = %id, "传输终态恢复材料未确认保存；保留提交前证据等待核对");
            }
        }
    }

    /// 执行结束后由外层统一持久化终态；任意提前返回均不能跳过恢复材料收尾。
    async fn run_worker_inner(&self, id: TransferId) {
        let (request, cancellation, sftp_policy, source_sftp, destination_sftp) = {
            let records = self.inner.records.lock().await;
            let Some(record) = records.get(&id) else {
                return;
            };
            (
                record.task.request().clone(),
                record.cancellation.clone(),
                record.sftp_policy,
                record.source_sftp.clone(),
                record.destination_sftp.clone(),
            )
        };
        let Ok(backend) = self.backend_for(&request) else {
            self.finish_failure(
                &id,
                TransferExecutionFailure::new(
                    TransferFailureKind::Unsupported,
                    false,
                    false,
                    None,
                    None,
                    TransferCleanupState::Released,
                ),
            )
            .await;
            return;
        };

        loop {
            if cancellation.is_cancelled() {
                self.finish_cancelled(&id, None, None).await;
                break;
            }
            if self.transition_connecting(&id).await.is_err() {
                if cancellation.is_cancelled() {
                    self.finish_cancelled(&id, None, None).await;
                } else {
                    self.finish_failure(
                        &id,
                        TransferExecutionFailure::new(
                            TransferFailureKind::Internal,
                            false,
                            false,
                            None,
                            None,
                            TransferCleanupState::Released,
                        ),
                    )
                    .await;
                }
                break;
            }
            let permits = match self
                .acquire_permits(
                    &request,
                    sftp_policy,
                    source_sftp.as_ref(),
                    destination_sftp.as_ref(),
                    &cancellation,
                )
                .await
            {
                Ok(permits) => permits,
                Err(()) => {
                    self.finish_cancelled(&id, None, None).await;
                    break;
                }
            };
            let (checkpoint, actual_destination, effective_idle_timeout_seconds) = {
                let records = self.inner.records.lock().await;
                let Some(record) = records.get(&id) else {
                    return;
                };
                (
                    record.checkpoint.clone(),
                    record.task.actual_destination().cloned(),
                    record.effective_idle_timeout_seconds,
                )
            };
            let attempt_cancel = cancellation.child_token();
            let outcome = self
                .run_attempt(TransferAttemptInput {
                    id: &id,
                    backend: backend.clone(),
                    request: request.clone(),
                    resume_from: checkpoint.clone(),
                    actual_destination,
                    cancellation: attempt_cancel,
                    sftp_policy,
                    source_sftp: source_sftp.clone(),
                    destination_sftp: destination_sftp.clone(),
                    effective_idle_timeout_seconds,
                })
                .await;
            drop(permits);
            match outcome {
                AttemptOutcome::Completed(result) => {
                    self.finish_success(&id, result).await;
                    break;
                }
                AttemptOutcome::Failed(failure) => {
                    self.finish_failure(&id, failure).await;
                    break;
                }
                AttemptOutcome::Cancelled {
                    checkpoint,
                    actual_destination,
                } => {
                    self.finish_cancelled(&id, checkpoint, actual_destination)
                        .await;
                    break;
                }
                AttemptOutcome::Idle(last_update) => {
                    match self.handle_idle(&id, last_update).await {
                        IdleAction::Recover => {
                            tokio::select! {
                                _ = cancellation.cancelled() => {
                                    self.finish_cancelled(&id, None, None).await;
                                    break;
                                }
                                _ = sleep(self.inner.options.recovery_delay) => {}
                            }
                        }
                        IdleAction::Finished => break,
                    }
                }
            }
        }
    }

    /// 申请共享计数下的入队快照许可，防止跨主机复制死锁及配置版本间重复计算活动任务。
    /// @author kongweiguang
    async fn acquire_permits(
        &self,
        request: &TransferRequest,
        sftp_policy: Option<SftpTransferPolicySnapshot>,
        source_sftp: Option<&HostSftpOptions>,
        destination_sftp: Option<&HostSftpOptions>,
        cancellation: &CancellationToken,
    ) -> Result<AttemptPermit, ()> {
        if let Some(snapshot) = sftp_policy {
            let hosts = [
                (&request.source, source_sftp),
                (&request.destination, destination_sftp),
            ]
            .into_iter()
            .filter_map(|(endpoint, options)| {
                endpoint.host_id().map(|host| {
                    (
                        host.clone(),
                        options.and_then(|value| value.concurrency).map(usize::from),
                    )
                })
            });
            let permit = self
                .inner
                .sftp_policy
                .acquire_with_host_limits_at_snapshot(hosts, snapshot, cancellation)
                .await
                .map_err(|_| ())?;
            return Ok(AttemptPermit {
                _sftp: Some(permit),
                _other: Vec::new(),
            });
        }

        let mut permits = Vec::new();
        let global = tokio::select! {
            _ = cancellation.cancelled() => return Err(()),
            result = self.inner.global_slots.clone().acquire_owned() => result.map_err(|_| ())?,
        };
        permits.push(global);

        let hosts = request
            .source
            .host_id()
            .into_iter()
            .chain(request.destination.host_id())
            .cloned()
            .collect::<BTreeSet<_>>();
        for host_id in hosts {
            let semaphore = {
                let mut slots = self.inner.host_slots.lock().await;
                slots
                    .entry(host_id)
                    .or_insert_with(|| {
                        Arc::new(Semaphore::new(self.inner.options.max_concurrent_per_host))
                    })
                    .clone()
            };
            let permit = tokio::select! {
                _ = cancellation.cancelled() => return Err(()),
                result = semaphore.acquire_owned() => result.map_err(|_| ())?,
            };
            permits.push(permit);
        }
        Ok(AttemptPermit {
            _sftp: None,
            _other: permits,
        })
    }

    /// 单次执行固定本轮的空闲超时、取消令牌与策略快照，避免后续队列变化改写运行中尝试的约束；
    /// 进度快照仅刷新展示，必经的传输与提交状态仍由有界控制消息可靠确认。
    ///
    /// @author kongweiguang
    async fn run_attempt(&self, attempt: TransferAttemptInput<'_>) -> AttemptOutcome {
        let TransferAttemptInput {
            id,
            backend,
            request,
            resume_from,
            actual_destination,
            cancellation,
            sftp_policy,
            source_sftp,
            destination_sftp,
            effective_idle_timeout_seconds,
        } = attempt;
        let idle_timeout = Duration::from_secs(effective_idle_timeout_seconds);
        let (sender, mut progress) = tokio::sync::watch::channel(TransferProgressUpdate::default());
        let (control_sender, mut control_requests) = tokio::sync::mpsc::channel(4);
        let context = TransferExecutionContext {
            transfer_id: id.clone(),
            request,
            resume_from,
            actual_destination,
            cancellation: cancellation.clone(),
            progress: TransferProgressReporter::new(sender, control_sender),
            sftp_policy,
            source_sftp,
            destination_sftp,
            shutdown_deadline: self.inner.shutdown_deadline.subscribe(),
        };
        let mut execution = Box::pin(backend.execute(context));
        let mut last_update = TransferProgressUpdate::default();
        let mut last_activity = Instant::now();
        let mut progress_open = true;
        let mut control_requests_open = true;
        let mut commit_item_in_progress = false;
        let mut commit_started = false;
        let mut commit_finalized = false;
        let mut idle_expired = false;
        let mut cancellation_observed = false;
        let mut control_rejected = false;
        let mut no_changes_finalized = false;

        loop {
            let deadline = last_activity + idle_timeout;
            tokio::select! {
                biased;
                _ = cancellation.cancelled(), if !commit_item_in_progress && !commit_finalized && !no_changes_finalized && !cancellation_observed => {
                    // 取消只通知后端协作停止；在它报告 channel 与 staging 已收尾前不丢弃执行 future。
                    cancellation_observed = true;
                }
                changed = progress.changed(), if progress_open => {
                    if changed.is_err() {
                        progress_open = false;
                        continue;
                    }
                    let update = progress.borrow_and_update().clone();
                    if has_activity(&last_update, &update) {
                        last_activity = Instant::now();
                    }
                    last_update = update.clone();
                    if self.apply_progress(id, update).await.is_err() {
                        control_rejected = true;
                        cancellation.cancel();
                        cancellation_observed = true;
                    }
                }
                control_request = control_requests.recv(), if control_requests_open => {
                    let Some(request) = control_request else {
                        control_requests_open = false;
                        continue;
                    };
                    let (request_kind, succeeded, cancelled, acknowledgment_failed) = match request {
                        TransferControlRequest::BeginTransfer { total_bytes, acknowledgment } => {
                            let result = self.apply_begin_transfer(id, total_bytes).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (0, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::BeginCommit {
                            destination,
                            confirmed_bytes,
                            total_bytes,
                            checkpoint,
                            acknowledgment,
                        } => {
                            let result = self.apply_commit_barrier(
                                id,
                                destination,
                                confirmed_bytes,
                                total_bytes,
                                checkpoint.map(|checkpoint| *checkpoint),
                            )
                            .await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (1, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::RecordCommittedEntry {
                            committed_bytes,
                            acknowledgment,
                        } => {
                            let result = self.apply_record_committed_entry(id, committed_bytes).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (2, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::ConfirmNoChange { acknowledgment } => {
                            let result = self.apply_confirm_no_change(id).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (5, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::FinishCommit { acknowledgment } => {
                            let result = self.apply_finish_commit(id).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (3, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::FinishNoChanges { acknowledgment } => {
                            let result = self.apply_finish_no_changes(id).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (4, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::RetainCleanupOwner { owner, acknowledgment } => {
                            let result = self.apply_retain_cleanup_owner(id, owner).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (6, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::ReleaseCleanupOwner { owner, acknowledgment } => {
                            let result = self.apply_release_cleanup_owner(id, owner).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (7, succeeded, cancelled, acknowledgment_failed)
                        }
                        TransferControlRequest::UpdateStagingOwnership { ownership, retained, acknowledgment } => {
                            let result = self.apply_staging_ownership(id, ownership, retained).await;
                            let cancelled = is_cancelled_result(&result);
                            let succeeded = result.is_ok();
                            let acknowledgment_failed = acknowledgment.send(result).is_err();
                            (if retained { 8 } else { 9 }, succeeded, cancelled, acknowledgment_failed)
                        }
                    };
                    if request_kind == 1 && succeeded && acknowledgment_failed {
                        // 未送达后端的许可必须撤销，避免凭队列内部状态误认目标操作已开始。
                        self.abort_unacknowledged_commit_item(id).await;
                    } else if request_kind == 1 && succeeded {
                        commit_started = true;
                        commit_item_in_progress = true;
                    }
                    if request_kind == 2 && succeeded {
                        commit_item_in_progress = false;
                    }
                    if request_kind == 5 && succeeded {
                        commit_item_in_progress = false;
                    }
                    if request_kind == 3 && succeeded {
                        commit_finalized = true;
                    }
                    if request_kind == 4 && succeeded {
                        no_changes_finalized = true;
                    }
                    if (request_kind < 6 || request_kind == 8)
                        && (cancelled || !succeeded || (acknowledgment_failed && request_kind != 4))
                    {
                        cancellation.cancel();
                        cancellation_observed = true;
                        control_rejected |= (!succeeded || acknowledgment_failed) && !cancelled;
                    }
                }
                result = &mut execution => {
                    let final_update = progress.borrow().clone();
                    if final_update != last_update {
                        if self.apply_progress(id, final_update.clone()).await.is_err() {
                            control_rejected = true;
                            cancellation.cancel();
                        }
                        last_update = final_update;
                    }
                    if control_rejected {
                        return match result {
                            Err(TransferExecutionError::Failed(failure)) => {
                                AttemptOutcome::Failed(failure)
                            }
                            Err(TransferExecutionError::Cancelled {
                                actual_destination,
                                ..
                            }) if commit_item_in_progress => {
                                AttemptOutcome::Failed(TransferExecutionFailure::new(
                                    TransferFailureKind::CommitUnknown,
                                    false,
                                    false,
                                    None,
                                    actual_destination.or(last_update.actual_destination),
                                    TransferCleanupState::Released,
                                ))
                            }
                            _ => AttemptOutcome::Failed(TransferExecutionFailure::new(
                                TransferFailureKind::Internal,
                                false,
                                false,
                                None,
                                last_update.actual_destination,
                                TransferCleanupState::Released,
                            )),
                        };
                    }
                    if idle_expired {
                        if let Err(TransferExecutionError::Failed(failure)) = &result
                            && (!failure.cleanup_state().is_released()
                                || failure.kind() == TransferFailureKind::CommitUnknown)
                        {
                            return AttemptOutcome::Failed(failure.clone());
                        }
                        return AttemptOutcome::Idle(last_update);
                    }
                    return match result {
                        Ok(result) if result.skipped && !result.unchanged && !commit_started && !no_changes_finalized => AttemptOutcome::Completed(result),
                        // 域的无变更屏障已验证零回执与无活动提交；尝试过但明确确认无副作用的项不应阻止合法终态。
                        Ok(result) if result.unchanged && no_changes_finalized && !result.skipped => AttemptOutcome::Completed(result),
                        Ok(result) if commit_finalized && !result.skipped && !result.unchanged => AttemptOutcome::Completed(result),
                        Ok(_) => AttemptOutcome::Failed(TransferExecutionFailure::new(
                            TransferFailureKind::Internal, false, false, None, None,
                            TransferCleanupState::Released,
                        )),
                        Err(TransferExecutionError::Cancelled { checkpoint, actual_destination }) => {
                            if commit_item_in_progress {
                                AttemptOutcome::Failed(TransferExecutionFailure::new(
                                    TransferFailureKind::CommitUnknown, false, false, None,
                                    actual_destination.or(last_update.actual_destination),
                                    TransferCleanupState::Released,
                                ))
                            } else {
                                AttemptOutcome::Cancelled {
                                    checkpoint: checkpoint.or(last_update.checkpoint),
                                    actual_destination: actual_destination.or(last_update.actual_destination),
                                }
                            }
                        }
                        Err(TransferExecutionError::Failed(failure)) if commit_item_in_progress => {
                            if failure.kind() == TransferFailureKind::CommitUnknown {
                                AttemptOutcome::Failed(failure)
                            } else {
                                AttemptOutcome::Failed(TransferExecutionFailure::new(
                                    TransferFailureKind::CommitUnknown, false, false, None,
                                    failure.actual_destination().cloned().or(last_update.actual_destination),
                                    failure.cleanup_state().clone(),
                                ))
                            }
                        }
                        Err(TransferExecutionError::Failed(failure)) if failure.kind() == TransferFailureKind::CommitUnknown => {
                            AttemptOutcome::Failed(TransferExecutionFailure::new(
                                TransferFailureKind::Internal, false, false, None,
                                failure.actual_destination().cloned().or(last_update.actual_destination),
                                failure.cleanup_state().clone(),
                            ))
                        }
                        Err(TransferExecutionError::Failed(failure)) => AttemptOutcome::Failed(failure),
                    };
                }
                _ = tokio::time::sleep_until(deadline), if !idle_expired && !commit_item_in_progress && !commit_finalized && !no_changes_finalized => {
                    cancellation.cancel();
                    idle_expired = true;
                }
            }
        }
    }

    /// 暂存创建前持久登记完整 owner，失败保留“是否创建待核对”事实；释放必须匹配原 token 而不能只凭路径。
    async fn apply_staging_ownership(
        &self,
        id: &TransferId,
        ownership: TransferStagingOwnership,
        retained: bool,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        if !valid_staging_token(&ownership.staging_token)
            || ownership.staging_destination == ownership.actual_destination
            || !ownership
                .staging_destination
                .same_filesystem(&ownership.actual_destination)
            || !ownership
                .actual_destination
                .same_filesystem(&record.task.request().destination)
            || !same_parent_directory(
                &ownership.staging_destination,
                &ownership.actual_destination,
            )
        {
            return Err(AppError::invalid("暂存所有权路径或 token 无效"));
        }
        if retained
            && (record.cancellation.is_cancelled()
                || matches!(
                    record.task.status(),
                    TransferStatus::Cancelling | TransferStatus::Cancelled
                ))
        {
            return Err(AppError::cancelled("传输已取消，禁止创建新的暂存对象"));
        }
        if retained && is_terminal(record.task.status()) {
            return Err(AppError::conflict("终态传输不能创建新的暂存对象"));
        }
        if retained && record.task.actual_destination().is_none() {
            record
                .task
                .select_actual_destination(ownership.actual_destination.clone())
                .map_err(|_| AppError::conflict("无法登记实际暂存目标"))?;
        }
        if let Some(current) = record
            .staging_ownership
            .iter()
            .find(|current| current.staging_destination == ownership.staging_destination)
        {
            if current != &ownership {
                return Err(AppError::conflict(
                    "同一路径已有不同暂存 owner，拒绝覆盖或释放",
                ));
            }
            if !retained {
                record
                    .staging_ownership
                    .retain(|current| current != &ownership);
            }
        } else if retained {
            if record.staging_ownership.len() >= 512 {
                return Err(AppError::conflict("传输暂存所有权记录已达上限"));
            }
            record.staging_ownership.push(ownership);
        } else {
            return Ok(());
        }
        self.persist_recovery_record(record).await?;
        publish(&self.inner.events);
        Ok(())
    }

    /// 进度快照只更新正在传输任务的展示投影；状态迁移由控制消息负责。
    /// 在同一记录锁内保存更新前的目标与检查点，仅在恢复信息变化时写入持久记录。
    ///
    /// @author kongweiguang
    async fn apply_progress(
        &self,
        id: &TransferId,
        update: TransferProgressUpdate,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        let previous_destination = record.task.actual_destination().cloned();
        let previous_checkpoint = record.checkpoint.clone();
        match record.task.status() {
            TransferStatus::Transferring => {
                if let Some(destination) = update.actual_destination.clone().or_else(|| {
                    update
                        .checkpoint
                        .as_ref()
                        .map(|checkpoint| checkpoint.actual_destination.clone())
                }) {
                    record
                        .task
                        .select_actual_destination(destination)
                        .map_err(|_| AppError::conflict("实际传输目标不能在执行中改变"))?;
                }
                let previous_bytes = record.task.bytes_transferred();
                record
                    .task
                    .record_progress(update.confirmed_bytes, update.total_bytes)
                    .map_err(|_| AppError::conflict("传输确认字节不连续"))?;
                update_progress_telemetry(record, previous_bytes, update.confirmed_bytes);
                // 真实提交边界只由可靠控制消息确认；watch 中的 Committing 不能提前发布。
                if update.phase != TransferPhase::Committing {
                    record.phase = Some(update.phase);
                }
            }
            TransferStatus::Committing | TransferStatus::Cancelling => return Ok(()),
            TransferStatus::Connecting if update.confirmed_bytes == 0 => return Ok(()),
            _ => return Err(AppError::conflict("传输进度超出当前状态")),
        }
        if let Some(checkpoint) = update.checkpoint
            && valid_checkpoint(
                &checkpoint,
                update.confirmed_bytes,
                record.task.actual_destination(),
            )
        {
            record.checkpoint = Some(checkpoint);
        }
        if previous_destination != record.task.actual_destination().cloned()
            || previous_checkpoint != record.checkpoint
        {
            self.persist_recovery_record(record).await?;
        }
        publish(&self.inner.events);
        Ok(())
    }

    /// 用可靠最终字节计数和单一提交许可确定线性化点，避免 watch 进度被合并后误判。
    ///
    /// @author kongweiguang
    async fn apply_commit_barrier(
        &self,
        id: &TransferId,
        destination: kerminal_domain::files::FileEndpoint,
        confirmed_bytes: u64,
        total_bytes: Option<u64>,
        checkpoint: Option<TransferCheckpoint>,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        match record.task.status() {
            TransferStatus::Transferring => {
                let previous_bytes = record.task.bytes_transferred();
                record
                    .task
                    .record_progress(confirmed_bytes, total_bytes)
                    .map_err(|_| AppError::conflict("最终确认字节不符合传输进度"))?;
                update_progress_telemetry(record, previous_bytes, confirmed_bytes);
                if let Some(checkpoint) = checkpoint {
                    if !valid_checkpoint(
                        &checkpoint,
                        confirmed_bytes,
                        record.task.actual_destination(),
                    ) {
                        return Err(AppError::conflict("提交时传入的断点未通过校验"));
                    }
                    record.checkpoint = Some(checkpoint);
                }
                record
                    .task
                    .select_actual_destination(destination.clone())
                    .map_err(|_| AppError::conflict("实际提交目标不能改变"))?;
            }
            TransferStatus::Committing
                if record.task.commit_between_items()
                    && record.task.actual_destination() == Some(&destination)
                    && record.task.bytes_transferred() == confirmed_bytes
                    && total_bytes
                        .is_none_or(|total| record.summary().total_bytes == Some(total))
                    && checkpoint.is_none() => {}
            TransferStatus::Cancelling | TransferStatus::Cancelled => {
                return Err(AppError::cancelled("传输已取消"));
            }
            _ => return Err(AppError::conflict("传输当前不能进入提交阶段")),
        }
        record
            .task
            .begin_commit(destination)
            .map_err(|_| AppError::conflict("提交阶段转换无效"))?;
        if let Err(error) = self.persist_recovery_record(record).await {
            // 没有授予后端许可，因此可撤销当前项；存储结果未知仍保留磁盘材料而不执行目标写入。
            let _ = record.task.abort_unacknowledged_commit_item();
            return Err(error);
        }
        record.phase = Some(TransferPhase::Committing);
        publish(&self.inner.events);
        Ok(())
    }

    /// 只在 backend 报告远端或本机原子操作成功后计数，并在延迟取消时关闭后续目标写入。
    ///
    /// @author kongweiguang
    async fn apply_record_committed_entry(
        &self,
        id: &TransferId,
        committed_bytes: u64,
    ) -> AppResult<TransferCommitSummary> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        let summary = record
            .task
            .record_committed_entry(committed_bytes)
            .map_err(|_| AppError::conflict("当前没有可确认的目标项回执"))?;
        record.checkpoint = None;
        record.phase = None;
        if record.task.status() == TransferStatus::Cancelling {
            record.cancellation.cancel();
        }
        self.persist_recovery_record(record).await?;
        publish(&self.inner.events);
        Ok(summary)
    }

    /// 仅接受 backend 明确无副作用回执，释放当前提交许可并保留之前的目标变化统计。
    ///
    /// @author kongweiguang
    async fn apply_confirm_no_change(&self, id: &TransferId) -> AppResult<TransferCommitSummary> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        let summary = record
            .task
            .confirm_commit_item_unchanged()
            .map_err(|_| AppError::conflict("当前没有可确认的无副作用提交项"))?;
        record.phase = None;
        if record.task.status() == TransferStatus::Cancelling {
            record.cancellation.cancel();
        }
        publish(&self.inner.events);
        Ok(summary)
    }

    /// 在所有回执确认且字节总量完整后发布成功；最终屏障和迟到取消由记录锁线性化。
    ///
    /// @author kongweiguang
    async fn apply_finish_commit(&self, id: &TransferId) -> AppResult<TransferCommitSummary> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        if record
            .summary()
            .total_bytes
            .is_some_and(|total| record.task.bytes_transferred() != total)
        {
            return Err(AppError::conflict("提交回执前的传输字节尚未完整确认"));
        }
        let summary = record
            .task
            .finish_commit()
            .map_err(|_| AppError::conflict("目标项回执未全部完成或最终屏障重复"))?;
        record
            .task
            .complete()
            .map_err(|_| AppError::conflict("传输最终结果屏障无效"))?;
        record.commit_finalized = true;
        record.checkpoint = None;
        record.phase = None;
        publish(&self.inner.events);
        prune_history(&mut records, self.inner.options.max_history);
        Ok(summary)
    }

    /// 以记录锁作为线性化点发布无变更终态，确保迟到取消不会覆盖完整目录结果。
    ///
    /// @author kongweiguang
    async fn apply_finish_no_changes(&self, id: &TransferId) -> AppResult<TransferCommitSummary> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        let summary = record
            .task
            .finish_no_changes()
            .map_err(|_| AppError::conflict("无变更结果不符合目录传输状态"))?;
        record.checkpoint = None;
        record.no_changes_finalized = true;
        record.phase = None;
        publish(&self.inner.events);
        prune_history(&mut records, self.inner.options.max_history);
        Ok(summary)
    }

    /// 仅接受同一任务的待清理资源，终态投影和历史修剪都必须看见 owner。
    async fn apply_retain_cleanup_owner(
        &self,
        id: &TransferId,
        owner: Arc<dyn TransferResourceCleanup>,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        if !is_terminal(record.task.status()) {
            return Err(AppError::conflict("仅终态传输可以登记资源清理 owner"));
        }
        if let Some(current) = &record.cleanup_owner {
            if !Arc::ptr_eq(current, &owner) {
                return Err(AppError::conflict("该任务已有不同的资源清理 owner"));
            }
        } else {
            record.cleanup_owner = Some(owner);
        }
        self.persist_recovery_record(record).await?;
        publish(&self.inner.events);
        Ok(())
    }

    /// 只在队列仍登记相同 owner 时释放记录；陈旧回执不能移除后来登记的资源。
    async fn apply_release_cleanup_owner(
        &self,
        id: &TransferId,
        owner: Arc<dyn TransferResourceCleanup>,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        match record.cleanup_owner.as_ref() {
            Some(current) if Arc::ptr_eq(current, &owner) => {
                record.cleanup_owner = None;
                record.external_target_lease.take();
                self.persist_recovery_record(record).await?;
                publish(&self.inner.events);
            }
            Some(_) => return Err(AppError::conflict("清理 owner 已变化，拒绝陈旧释放回执")),
            None => return Ok(()),
        }
        prune_history(&mut records, self.inner.options.max_history);
        Ok(())
    }

    /// 控制通道证明 backend 未收到许可时撤销这一项，保留之前已确认的目录项统计。
    ///
    /// @author kongweiguang
    async fn abort_unacknowledged_commit_item(&self, id: &TransferId) {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        if record.task.abort_unacknowledged_commit_item().is_ok() {
            record.phase = None;
            if record.task.status() == TransferStatus::Cancelling {
                record.cancellation.cancel();
            }
            publish(&self.inner.events);
        }
    }

    /// 从 Connecting 进入 Transferring 由 backend 明确请求并等待确认，包括空文件传输。
    ///
    /// @author kongweiguang
    async fn apply_begin_transfer(
        &self,
        id: &TransferId,
        total_bytes: Option<u64>,
    ) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        match record.task.status() {
            TransferStatus::Cancelling | TransferStatus::Cancelled => {
                return Err(AppError::cancelled("传输已取消"));
            }
            TransferStatus::Connecting => {}
            _ => return Err(AppError::conflict("当前任务不能开始传输")),
        }
        record
            .task
            .begin_transferring(total_bytes)
            .map_err(|_| AppError::conflict("传输阶段转换无效"))?;
        record.phase = Some(TransferPhase::Preparing);
        record.progress_sample = Some((record.task.bytes_transferred(), Instant::now()));
        record.speed_bytes_per_second = 0;
        publish(&self.inner.events);
        Ok(())
    }

    /// worker 获得资源后进入 Connecting；被取消的排队任务不再被错误地重启。
    ///
    /// @author kongweiguang
    async fn transition_connecting(&self, id: &TransferId) -> AppResult<()> {
        let mut records = self.inner.records.lock().await;
        let record = records
            .get_mut(id)
            .ok_or_else(|| AppError::not_found("传输任务不存在"))?;
        if record.task.status() == TransferStatus::Cancelled {
            return Err(AppError::cancelled("传输已取消"));
        }
        record
            .task
            .begin_connecting()
            .map_err(|_| AppError::conflict("传输任务当前不能启动"))?;
        record.phase = None;
        publish(&self.inner.events);
        Ok(())
    }

    /// idle timeout 时只用源版本和前缀摘要都可信的检查点恢复，提交阶段统一转为 commitUnknown。
    ///
    /// @author kongweiguang
    async fn handle_idle(&self, id: &TransferId, update: TransferProgressUpdate) -> IdleAction {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return IdleAction::Finished;
        };
        if record.task.commit_item_in_progress() {
            let _ = record.task.fail(
                TransferFailureKind::CommitUnknown,
                "无法确认目标文件是否已提交".to_owned(),
                false,
                false,
            );
            record.checkpoint = None;
            record.phase = None;
            publish(&self.inner.events);
            return IdleAction::Finished;
        }
        let checkpoint = update
            .checkpoint
            .filter(|checkpoint| checkpoint.confirmed_bytes <= update.confirmed_bytes)
            .or_else(|| record.checkpoint.clone());
        if let Some(checkpoint) =
            checkpoint.filter(|checkpoint| valid_sha256(&checkpoint.confirmed_prefix_sha256))
            && record.task.begin_automatic_recovery(true).is_ok()
        {
            record.checkpoint = Some(checkpoint.clone());
            let _ = record
                .task
                .select_actual_destination(checkpoint.actual_destination.clone());
            record.phase = None;
            publish(&self.inner.events);
            return IdleAction::Recover;
        }
        let checkpoint = record.checkpoint.clone();
        let resumable = checkpoint.is_some();
        let _ = record.task.fail(
            TransferFailureKind::IdleTimeout,
            "传输长时间没有确认进度".to_owned(),
            true,
            resumable,
        );
        record.external_target_lease.take();
        record.phase = None;
        publish(&self.inner.events);
        IdleAction::Finished
    }

    /// 成功结果必须越过字节传输和正式提交状态，且已知总字节必须全部确认。
    ///
    /// @author kongweiguang
    async fn finish_success(&self, id: &TransferId, result: super::TransferExecutionResult) {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        record.phase = None;
        if record.commit_finalized {
            // finish_commit 已在线性化点发布成功；backend 返回只负责结束执行与资源 owner。
            if record.task.status() != TransferStatus::Succeeded
                || record.task.actual_destination() != Some(&result.actual_destination)
            {
                tracing::error!(transfer_id = %id, "最终回执已发布但 backend 结果不匹配");
            }
            if record.cleanup_owner.is_none() {
                record.external_target_lease.take();
            }
            return;
        }
        if record.no_changes_finalized {
            if record.task.status() != TransferStatus::Unchanged
                || !result.unchanged
                || result.skipped
                || record.task.actual_destination() != Some(&result.actual_destination)
            {
                tracing::error!(transfer_id = %id, "无变更屏障已发布但 backend 结果不匹配");
            }
            if record.cleanup_owner.is_none() {
                record.external_target_lease.take();
            }
            return;
        }
        if result.skipped {
            if record.task.status() == TransferStatus::Cancelling {
                let _ = record.task.confirm_cancelled(false);
            } else if record.task.status() != TransferStatus::Transferring
                || record.task.bytes_transferred() != 0
                || record.task.actual_destination() != Some(&result.actual_destination)
                || record.task.skip().is_err()
            {
                let _ = record.task.fail(
                    TransferFailureKind::Internal,
                    "跳过结果与传输状态不一致".to_owned(),
                    false,
                    false,
                );
            }
            record.checkpoint = None;
            record.external_target_lease.take();
            publish(&self.inner.events);
            prune_history(&mut records, self.inner.options.max_history);
            return;
        }
        let commit_unknown = record.task.commit_item_in_progress();
        let valid_result = record.task.status() == TransferStatus::Succeeded
            && record.task.actual_destination() == Some(&result.actual_destination)
            && result
                .total_bytes
                .is_none_or(|total| record.task.bytes_transferred() == total);
        if !valid_result {
            let kind = if commit_unknown {
                TransferFailureKind::CommitUnknown
            } else {
                TransferFailureKind::Internal
            };
            let _ = record
                .task
                .fail(kind, super::failure_message(kind).to_owned(), false, false);
            record.checkpoint = None;
            if kind != TransferFailureKind::CommitUnknown {
                record.external_target_lease.take();
            }
            publish(&self.inner.events);
            return;
        }
        // 成功状态已由有完整逐项回执的 finish_commit 控制消息发布。
        record.external_target_lease.take();
    }

    /// 后端失败按稳定类别进入领域状态；CommitUnknown 必须保留临时目标 scope 供人工核对。
    ///
    /// @author kongweiguang
    async fn finish_failure(&self, id: &TransferId, failure: TransferExecutionFailure) {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        record.phase = None;
        if is_terminal(record.task.status()) {
            if let Some(owner) = failure.cleanup_state().cleanup_owner() {
                record.cleanup_owner = Some(owner);
            }
            let commit_unknown =
                record.task.failure_kind() == Some(TransferFailureKind::CommitUnknown);
            if record.cleanup_owner.is_none() && !commit_unknown {
                record.external_target_lease.take();
            }
            publish(&self.inner.events);
            return;
        }
        record.cleanup_owner = failure.cleanup_state().cleanup_owner();
        let failure_kind = if failure.kind() == TransferFailureKind::CommitUnknown
            && !record.task.commit_item_in_progress()
        {
            TransferFailureKind::Internal
        } else {
            failure.kind()
        };
        let safe_message = if failure_kind == failure.kind() {
            failure.safe_message()
        } else {
            super::failure_message(TransferFailureKind::Internal)
        };
        if failure_kind == TransferFailureKind::CommitUnknown {
            let _ = record.task.fail(
                TransferFailureKind::CommitUnknown,
                safe_message.to_owned(),
                false,
                false,
            );
            record.checkpoint = None;
        } else if record.task.status() == TransferStatus::Cancelling
            && failure.kind() != TransferFailureKind::CommitUnknown
            && failure.cleanup_state().is_released()
        {
            if let Some(checkpoint) = failure.checkpoint().cloned() {
                record.checkpoint = Some(checkpoint);
            }
            let _ = record.task.confirm_cancelled(record.checkpoint.is_some());
        } else {
            if let Some(checkpoint) = failure.checkpoint().cloned() {
                record.checkpoint = Some(checkpoint);
            }
            let _ = record.task.fail(
                failure_kind,
                safe_message.to_owned(),
                failure.retryable(),
                failure.resumable(),
            );
        }
        if record.cleanup_owner.is_none() && failure_kind != TransferFailureKind::CommitUnknown {
            record.external_target_lease.take();
        }
        publish(&self.inner.events);
        prune_history(&mut records, self.inner.options.max_history);
    }

    /// 后端确认资源释放后才确认取消；提交期间或已有未知结果时保留目标 scope 供回读。
    ///
    /// @author kongweiguang
    async fn finish_cancelled(
        &self,
        id: &TransferId,
        checkpoint: Option<TransferCheckpoint>,
        actual_destination: Option<kerminal_domain::files::FileEndpoint>,
    ) {
        let mut records = self.inner.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return;
        };
        record.phase = None;
        let commit_unknown = record.task.commit_item_in_progress()
            || record.task.failure_kind() == Some(TransferFailureKind::CommitUnknown);
        if record.task.commit_item_in_progress() {
            let _ = record.task.fail(
                TransferFailureKind::CommitUnknown,
                super::failure_message(TransferFailureKind::CommitUnknown).to_owned(),
                false,
                false,
            );
            record.checkpoint = None;
        } else if record.task.status() != TransferStatus::Cancelled {
            if let Some(destination) = actual_destination
                && matches!(
                    record.task.status(),
                    TransferStatus::Queued
                        | TransferStatus::Connecting
                        | TransferStatus::Transferring
                )
            {
                let _ = record.task.select_actual_destination(destination);
            }
            if record.task.status() != TransferStatus::Cancelling {
                let _ = record.task.request_cancel();
            }
            if let Some(checkpoint) = checkpoint {
                record.checkpoint = Some(checkpoint);
            }
            if record.task.status() == TransferStatus::Cancelling {
                let _ = record.task.confirm_cancelled(record.checkpoint.is_some());
            }
        }
        if !commit_unknown && record.cleanup_owner.is_none() {
            record.external_target_lease.take();
        }
        publish(&self.inner.events);
        prune_history(&mut records, self.inner.options.max_history);
    }

    /// worker 异常按提交边界落入安全终态；记录锁等待服从同一个退出 deadline。
    ///
    /// @author kongweiguang
    async fn record_worker_join_failure_until(
        &self,
        id: &TransferId,
        cancelled: bool,
        panicked: bool,
        deadline: Instant,
    ) -> AppResult<()> {
        let mut records = timeout_at(deadline, self.inner.records.lock())
            .await
            .map_err(|_| AppError::timeout("等待传输异常退出记录"))?;
        let Some(record) = records.get_mut(id) else {
            return Ok(());
        };
        if is_terminal(record.task.status()) {
            return Ok(());
        }
        record.phase = None;
        let committing = record.task.commit_item_in_progress();
        let kind = if committing {
            TransferFailureKind::CommitUnknown
        } else {
            TransferFailureKind::Internal
        };
        let message = if committing {
            super::failure_message(TransferFailureKind::CommitUnknown)
        } else {
            "传输后台任务异常终止"
        };
        let _ = record.task.fail(kind, message.to_owned(), false, false);
        record.checkpoint = None;
        tracing::error!(transfer_id = %id, cancelled, panicked, "传输 worker 异常退出已记录");
        publish(&self.inner.events);
        Ok(())
    }
}

/// 变化广播只负责唤醒订阅者；权威摘要始终由 list 读取，避免高频事件复制完整投影。
///
/// @author kongweiguang
fn publish(sender: &broadcast::Sender<TransferQueueEvent>) {
    let _ = sender.send(TransferQueueEvent::Changed);
}

/// 可靠控制请求仅把明确取消分类成协作停止，其它拒绝属于协议错误。
fn is_cancelled_result<T>(result: &AppResult<T>) -> bool {
    matches!(
        result,
        Err(error) if error.public_code() == crate::PublicErrorCode::Cancelled
    )
}

/// 判断一个任务状态是否已经终结，以便容量计算和有界历史清理一致使用。
fn is_terminal(status: TransferStatus) -> bool {
    matches!(
        status,
        TransferStatus::Succeeded
            | TransferStatus::Skipped
            | TransferStatus::Unchanged
            | TransferStatus::Failed
            | TransferStatus::Cancelled
    )
}

/// 统一判定手动清理与自动裁剪可移除的历史，保留待人工核对或仍持有资源引用的记录。
///
/// @author kongweiguang
fn is_removable_history_record(record: &TransferRecord) -> bool {
    is_terminal(record.task.status())
        && record.cleanup_owner.is_none()
        && record.external_target_lease.is_none()
        && record.task.failure_kind() != Some(TransferFailureKind::CommitUnknown)
}

/// 在等待队列锁或 worker 时响应取消，保留共享容器中的句柄以便下一次清理接续。
///
/// @author kongweiguang
async fn cancellable_clear<T>(
    cancellation: &CancellationToken,
    operation: impl Future<Output = AppResult<T>>,
) -> AppResult<T> {
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Err(AppError::cancelled("传输历史清理已取消")),
        result = operation => result,
    }
}

/// 提取请求引用的唯一临时目标，拒绝把两个一次性 profile 藏进单个 lease。
///
/// @author kongweiguang
fn external_host_for_request(request: &TransferRequest) -> AppResult<Option<HostId>> {
    let mut external_host = None;
    for endpoint in [&request.source, &request.destination] {
        let Some(host_id) = endpoint
            .host_id()
            .filter(|host_id| host_id.as_str().starts_with("external:"))
        else {
            continue;
        };
        if external_host
            .as_ref()
            .is_some_and(|existing: &HostId| existing != host_id)
        {
            return Err(AppError::unsupported("单次传输不能混用两个临时外部连接"));
        }
        external_host = Some(host_id.clone());
    }
    Ok(external_host)
}

/// 仅接受传输阶段推进或确认字节增长作为 idle 计时活动，避免纯 UI 心跳掩盖卡住的数据流。
fn has_activity(previous: &TransferProgressUpdate, next: &TransferProgressUpdate) -> bool {
    next.activity_sequence != previous.activity_sequence
}

/// 只在任务确认字节严格增长时更新速度与时间，排除恢复偏移、重复帧和控制阶段心跳。
///
/// @author kongweiguang
fn update_progress_telemetry(
    record: &mut TransferRecord,
    previous_bytes: u64,
    confirmed_bytes: u64,
) {
    if confirmed_bytes <= previous_bytes {
        return;
    }

    let sampled_at = Instant::now();
    let (sample_bytes, sample_time) = record
        .progress_sample
        .unwrap_or((previous_bytes, sampled_at));
    let delta_bytes = confirmed_bytes.saturating_sub(sample_bytes);
    let elapsed_seconds = sampled_at
        .saturating_duration_since(sample_time)
        .as_secs_f64();
    if delta_bytes > 0 && elapsed_seconds > 0.0 {
        record.speed_bytes_per_second = ((delta_bytes as f64 / elapsed_seconds)
            .round()
            .min(u64::MAX as f64)) as u64;
    }
    record.progress_sample = Some((confirmed_bytes, sampled_at));
    record.last_progress_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis().min(u128::from(u64::MAX)) as u64);
}

/// checkpoint 摘要必须是固定长度十六进制，畸形摘要不能被写入可信恢复状态。
fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 断点必须绑定强源 revision、确认前缀、唯一暂存 owner 和最终目标的同目录路径。
///
/// @author kongweiguang
fn valid_checkpoint(
    checkpoint: &TransferCheckpoint,
    confirmed_bytes: u64,
    actual_destination: Option<&kerminal_domain::files::FileEndpoint>,
) -> bool {
    let Some(source_digest) = checkpoint.source_revision.content_digest.as_deref() else {
        return false;
    };
    let owner = checkpoint.staging_owner.as_str();
    checkpoint.confirmed_bytes <= confirmed_bytes
        && checkpoint.confirmed_bytes <= checkpoint.source_revision.size
        && valid_sha256(&checkpoint.confirmed_prefix_sha256)
        && valid_sha256(source_digest)
        && !owner.is_empty()
        && valid_staging_token(&checkpoint.staging_token)
        && !checkpoint
            .staging_destination
            .same_object(&checkpoint.actual_destination)
        && checkpoint
            .staging_destination
            .same_filesystem(&checkpoint.actual_destination)
        && same_parent_directory(
            &checkpoint.staging_destination,
            &checkpoint.actual_destination,
        )
        && actual_destination
            .is_none_or(|destination| destination == &checkpoint.actual_destination)
}

/// 暂存 marker token 必须是固定长度的小写十六进制 nonce，避免检查点携带路径或任意文本。
fn valid_staging_token(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 比较 endpoint 父路径时按本机语义用 Path、远端语义用纯 POSIX RemotePath。
///
/// @author kongweiguang
fn same_parent_directory(
    staging: &kerminal_domain::files::FileEndpoint,
    destination: &kerminal_domain::files::FileEndpoint,
) -> bool {
    use kerminal_domain::files::FileEndpoint;
    match (staging, destination) {
        (FileEndpoint::Local { path: left }, FileEndpoint::Local { path: right }) => {
            left.parent() == right.parent()
        }
        (
            FileEndpoint::Remote {
                host_id: left_host,
                path: left,
            },
            FileEndpoint::Remote {
                host_id: right_host,
                path: right,
            },
        ) => left_host == right_host && left.parent() == right.parent(),
        (
            FileEndpoint::Container {
                host_id: left_host,
                runtime: left_runtime,
                container_id: left_id,
                path: left,
            },
            FileEndpoint::Container {
                host_id: right_host,
                runtime: right_runtime,
                container_id: right_id,
                path: right,
            },
        ) => {
            left_host == right_host
                && left_runtime == right_runtime
                && left_id == right_id
                && left.parent() == right.parent()
        }
        _ => false,
    }
}

/// 仅裁剪普通终态历史；未知提交、待清理 owner 和临时目标租约必须留在队列供恢复或核对。
///
/// @author kongweiguang
fn prune_history(records: &mut BTreeMap<TransferId, TransferRecord>, max_history: usize) {
    let mut terminal = records
        .iter()
        .filter(|(_, record)| {
            is_removable_history_record(record)
                && record
                    .worker
                    .as_ref()
                    .is_none_or(|worker| worker.is_finished())
        })
        .map(|(id, record)| (record.sequence, id.clone()))
        .collect::<Vec<_>>();
    if terminal.len() <= max_history {
        return;
    }
    terminal.sort_unstable_by_key(|(sequence, _)| *sequence);
    let excess = terminal.len().saturating_sub(max_history);
    for (_, id) in terminal.into_iter().take(excess) {
        records.remove(&id);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/private_queue.rs"]
mod tests;
