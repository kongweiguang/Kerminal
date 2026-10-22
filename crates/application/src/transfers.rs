//! 文件传输队列的用例契约与有界进度通道。
//!
//! @author kongweiguang

use kerminal_domain::files::{
    FileEndpoint, FileRevision, TransferCommitSummary, TransferFailureKind, TransferRequest,
};
use kerminal_domain::identity::TransferId;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::{AppError, AppResult};

mod policy;
mod queue;
mod recovery;
mod subscription;

pub use kerminal_domain::files::TransferPhase;
pub use policy::{SftpTransferPolicySnapshot, SftpTransferPolicyStore};
pub use queue::{TransferQueueOptions, TransferService};
pub use recovery::{
    InMemoryTransferRecoveryRepository, MAX_TRANSFER_ARCHIVE_PAGE_SIZE, TransferRecoveryInspection,
    TransferRecoveryPage, TransferRecoveryRecord, TransferRecoveryRepository,
    TransferStagingOwnership,
};

/// 进度只报告已经确认的字节；更新走 watch 通道，繁忙时合并旧帧而不积压内存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferProgressUpdate {
    pub confirmed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub phase: TransferPhase,
    pub actual_destination: Option<FileEndpoint>,
    /// 仅当后端已验证源版本和前缀哈希后才允许附带断点。
    pub checkpoint: Option<TransferCheckpoint>,
    /// 内部活动序号用于区分有实际 IO 的重复进度与真正停滞，不属于外部 MCP 契约。
    pub activity_sequence: u64,
}

impl Default for TransferProgressUpdate {
    /// 初始进度不假设大小或实际目标，避免在远端 stat 前显示伪百分比。
    fn default() -> Self {
        Self {
            confirmed_bytes: 0,
            total_bytes: None,
            phase: TransferPhase::Preparing,
            actual_destination: None,
            checkpoint: None,
            activity_sequence: 0,
        }
    }
}

/// 可信检查点由传输后端生成；partial 文件长度本身不能构成可恢复偏移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferCheckpoint {
    /// 断点生成时源文件的稳定 revision，重试前必须重新核对。
    pub source_revision: FileRevision,
    pub confirmed_bytes: u64,
    pub confirmed_prefix_sha256: String,
    /// partial 的唯一所有者；续传必须验证其路径与这个 ID 对应。
    pub staging_owner: TransferId,
    /// 部分文件路径固定在最终目标的同目录，不从最终文件名重新推断。
    pub staging_destination: FileEndpoint,
    /// 首次创建时随机生成并由同目录 marker 持久绑定，重试不得重新生成。
    pub staging_token: String,
    pub actual_destination: FileEndpoint,
}

/// 可靠生命周期消息与可合并进度分离，确保必经状态迁移不能被 watch 覆盖。
pub(crate) enum TransferControlRequest {
    UpdateStagingOwnership {
        ownership: TransferStagingOwnership,
        retained: bool,
        acknowledgment: oneshot::Sender<AppResult<()>>,
    },
    BeginTransfer {
        total_bytes: Option<u64>,
        acknowledgment: oneshot::Sender<AppResult<()>>,
    },
    BeginCommit {
        destination: FileEndpoint,
        confirmed_bytes: u64,
        total_bytes: Option<u64>,
        checkpoint: Option<Box<TransferCheckpoint>>,
        acknowledgment: oneshot::Sender<AppResult<()>>,
    },
    RecordCommittedEntry {
        committed_bytes: u64,
        acknowledgment: oneshot::Sender<AppResult<TransferCommitSummary>>,
    },
    ConfirmNoChange {
        acknowledgment: oneshot::Sender<AppResult<TransferCommitSummary>>,
    },
    FinishCommit {
        acknowledgment: oneshot::Sender<AppResult<TransferCommitSummary>>,
    },
    FinishNoChanges {
        acknowledgment: oneshot::Sender<AppResult<TransferCommitSummary>>,
    },
    RetainCleanupOwner {
        owner: Arc<dyn TransferResourceCleanup>,
        acknowledgment: oneshot::Sender<AppResult<()>>,
    },
    ReleaseCleanupOwner {
        owner: Arc<dyn TransferResourceCleanup>,
        acknowledgment: oneshot::Sender<AppResult<()>>,
    },
}

/// 本次尝试上下文只含稳定目标、可验证检查点和取消信号，不复制认证材料。
#[derive(Clone)]
pub struct TransferExecutionContext {
    pub transfer_id: TransferId,
    pub request: TransferRequest,
    pub resume_from: Option<TransferCheckpoint>,
    pub actual_destination: Option<FileEndpoint>,
    pub cancellation: CancellationToken,
    pub progress: TransferProgressReporter,
    /// SFTP 入队时捕获的不可变协议与 idle 参数；在途请求不受设置热更新影响。
    pub sftp_policy: Option<SftpTransferPolicySnapshot>,
    /// 两端主机选项在入队时冻结；重试和当前配置变化不能替换在途任务语义。
    pub source_sftp: Option<crate::connections::management::HostSftpOptions>,
    pub destination_sftp: Option<crate::connections::management::HostSftpOptions>,
    shutdown_deadline: watch::Receiver<Option<tokio::time::Instant>>,
}

impl TransferExecutionContext {
    /// 读取退出协调器提供的绝对 deadline，让后端的 channel、SSH lease 与队列 join 共用预算。
    pub fn shutdown_deadline(&self) -> Option<tokio::time::Instant> {
        *self.shutdown_deadline.borrow()
    }
}

/// 原子提交成功后才返回结果；普通字节写完成不能被当作整个任务成功。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferExecutionResult {
    pub actual_destination: FileEndpoint,
    pub total_bytes: Option<u64>,
    /// Skip 是正常业务结果，但没有文件写入或提交，队列据此落为独立 Skipped 终态。
    pub skipped: bool,
    /// 已遍历并校验目录但目标没有变化；不代表任何提交 receipt。
    pub unchanged: bool,
}

/// 传输资源无法确认关闭时保留同一 owner，供应用退出或用户显式重试清理。
#[async_trait::async_trait]
pub trait TransferResourceCleanup: Send + Sync + 'static {
    /// 使用调用方的绝对 deadline 重试关闭本轮资源，不在 adapter 内重置预算。
    async fn cleanup_until(&self, deadline: Instant) -> AppResult<()>;
}

/// 每个失败构造点都必须说明资源已经释放，或交付可重试的清理 owner。
#[derive(Clone)]
pub enum TransferCleanupState {
    Released,
    Pending(Arc<dyn TransferResourceCleanup>),
}

impl std::fmt::Debug for TransferCleanupState {
    /// 清理 owner 可能间接引用连接上下文，Debug 只暴露是否仍有待清理资源。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Released => formatter.write_str("Released"),
            Self::Pending(_) => formatter.write_str("Pending([owner omitted])"),
        }
    }
}

impl TransferCleanupState {
    /// 创建待清理状态时必须交付真正拥有底层资源的对象，避免只保留一个空状态标记。
    pub fn pending(owner: Arc<dyn TransferResourceCleanup>) -> Self {
        Self::Pending(owner)
    }

    /// 判断调用方是否已提供资源释放回执。
    pub const fn is_released(&self) -> bool {
        matches!(self, Self::Released)
    }

    /// 复制 Arc owner 供队列跨 await 重试；不暴露 adapter 的具体连接实现。
    pub fn cleanup_owner(&self) -> Option<Arc<dyn TransferResourceCleanup>> {
        match self {
            Self::Released => None,
            Self::Pending(owner) => Some(Arc::clone(owner)),
        }
    }
}

/// 后端失败的安全视图不接受原始网络文本，防止用户名、路径和凭据进入队列摘要。
#[derive(Debug, Clone)]
pub struct TransferExecutionFailure {
    kind: TransferFailureKind,
    retryable: bool,
    resumable: bool,
    checkpoint: Option<TransferCheckpoint>,
    actual_destination: Option<FileEndpoint>,
    cleanup: TransferCleanupState,
    safe_message: &'static str,
}

impl TransferExecutionFailure {
    /// commitUnknown 由领域强制不可重试，其他错误是否重试取决于后端是否能安全重放。
    pub fn new(
        kind: TransferFailureKind,
        retryable: bool,
        resumable: bool,
        checkpoint: Option<TransferCheckpoint>,
        actual_destination: Option<FileEndpoint>,
        cleanup: TransferCleanupState,
    ) -> Self {
        let commit_unknown = kind == TransferFailureKind::CommitUnknown;
        Self {
            kind,
            retryable: retryable && !commit_unknown,
            resumable: resumable && checkpoint.is_some() && !commit_unknown,
            checkpoint: if commit_unknown { None } else { checkpoint },
            actual_destination,
            cleanup,
            safe_message: failure_message(kind),
        }
    }

    /// 保留后端已经脱敏的静态业务原因，让 UI 与验收能区分权限拒绝和目标冲突。
    ///
    /// @author kongweiguang
    pub fn with_safe_message(mut self, message: &'static str) -> Self {
        self.safe_message = message;
        self
    }

    /// 返回稳定失败类别，具体 transport 错误只进入基础设施日志。
    pub const fn kind(&self) -> TransferFailureKind {
        self.kind
    }

    /// 允许界面显示恢复动作时读取后端策略结果。
    pub const fn retryable(&self) -> bool {
        self.retryable
    }

    /// 只有存在经过源身份和前缀校验的 checkpoint 才返回 true。
    pub const fn resumable(&self) -> bool {
        self.resumable
    }

    /// 获取可恢复断点供同一应用进程中的 retry 或一次自动恢复使用。
    pub fn checkpoint(&self) -> Option<&TransferCheckpoint> {
        self.checkpoint.as_ref()
    }

    /// 获取首次选定的实际目标，以保证 rename 策略重试时不会改名。
    pub fn actual_destination(&self) -> Option<&FileEndpoint> {
        self.actual_destination.as_ref()
    }

    /// 返回后端对连接、channel 和其它本轮资源的显式清理状态。
    pub fn cleanup_state(&self) -> &TransferCleanupState {
        &self.cleanup
    }

    /// 仅返回源码定义的脱敏文案，不展示后端原始错误或动态输入。
    pub fn safe_message(&self) -> &'static str {
        self.safe_message
    }
}

/// 后端可以区分用户取消与失败；取消只有在关闭资源后才由队列确认为终态。
#[derive(Debug, Clone)]
pub enum TransferExecutionError {
    Cancelled {
        checkpoint: Option<TransferCheckpoint>,
        actual_destination: Option<FileEndpoint>,
    },
    Failed(TransferExecutionFailure),
}

/// 文件传输实现负责实际协议 IO，任务队列、幂等重试和并发预算集中在应用层。
#[async_trait::async_trait]
pub trait TransferBackend: Send + Sync + 'static {
    /// 判断是否完整支持源到目标的组合，避免对跨主机请求分两半伪成功。
    fn supports(&self, request: &TransferRequest) -> bool;

    /// 只有真实 SFTP 后端使用动态 SFTP 限流，避免容器任务重复占用两套并发预算。
    fn uses_sftp_policy(&self, _request: &TransferRequest) -> bool {
        false
    }

    /// 恢复核对仅允许读取原目标；不支持的后端保留未知，不启动新传输或猜测成功。
    async fn inspect_recovery(
        &self,
        _record: &TransferRecoveryRecord,
        _cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryInspection> {
        Ok(TransferRecoveryInspection::Insufficient)
    }

    /// 执行一轮传输并在返回前关闭本轮资源；远端 rename 前必须 await progress.begin_commit 取得提交许可。
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError>;
}

/// 进度发送端可由多个 protocol reader 克隆共享，watch 会合并高频刷新而不丢字节。
#[derive(Clone)]
pub struct TransferProgressReporter {
    sender: watch::Sender<TransferProgressUpdate>,
    control_sender: mpsc::Sender<TransferControlRequest>,
    activity_sequence: Arc<AtomicU64>,
}

impl TransferProgressReporter {
    /// marker 与数据暂存首次创建前取得可靠持久化回执；watch 进度不能替代该所有权门禁。
    pub async fn register_staging(&self, ownership: TransferStagingOwnership) -> AppResult<()> {
        self.update_staging_ownership(ownership, true).await
    }

    /// 后端已验证原暂存不再存在才释放登记；失败或回执未知继续保留原 owner，不触发删除。
    pub async fn release_staging(&self, ownership: TransferStagingOwnership) -> AppResult<()> {
        self.update_staging_ownership(ownership, false).await
    }

    /// 登记和释放复用有界可靠消息，队列持久化后才确认，避免进程崩溃丢失随机暂存路径。
    async fn update_staging_ownership(
        &self,
        ownership: TransferStagingOwnership,
        retained: bool,
    ) -> AppResult<()> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::UpdateStagingOwnership {
                ownership,
                retained,
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止，暂存所有权未确认"))?;
        result.await.map_err(|_| {
            AppError::outcome_unknown("暂存所有权持久化回执未知，禁止创建或盲目清理")
        })?
    }

    /// 新建 watch sender；队列只读 receiver 消费最新快照，因此进度更新不会产生无界事件堆积。
    pub(crate) fn new(
        sender: watch::Sender<TransferProgressUpdate>,
        control_sender: mpsc::Sender<TransferControlRequest>,
    ) -> Self {
        Self {
            sender,
            control_sender,
            activity_sequence: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 非阻塞发布最新确认偏移，丢弃的是过期 UI 帧而不是传输数据。
    pub fn report(&self, mut update: TransferProgressUpdate) {
        update.activity_sequence = self
            .activity_sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        self.sender.send_replace(update);
    }

    /// 在报告进度之前取得可靠的 Transferring 状态，watch 合并不会跳过聚合状态转换。
    pub async fn begin_transfer(&self, total_bytes: Option<u64>) -> AppResult<()> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::BeginTransfer {
                total_bytes,
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("传输阶段确认已撤销"))?
    }

    /// 在原子 rename 前同步最后确认字节并申请提交许可，watch 仅负责界面刷新。
    pub async fn begin_commit(
        &self,
        destination: FileEndpoint,
        confirmed_bytes: u64,
        total_bytes: Option<u64>,
        checkpoint: Option<TransferCheckpoint>,
    ) -> AppResult<()> {
        self.request_commit(destination, confirmed_bytes, total_bytes, checkpoint, None)
            .await
    }

    /// 在目标项收到明确服务端提交回执后同步累计摘要；此控制消息不能与进度帧合并。
    pub async fn record_committed_entry(
        &self,
        committed_bytes: u64,
    ) -> AppResult<TransferCommitSummary> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::RecordCommittedEntry {
                committed_bytes,
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("目标项回执确认已撤销"))?
    }

    /// 服务端明确确认当前目标项未被修改时释放已获提交许可，不产生虚假 receipt。
    pub async fn confirm_no_change(&self) -> AppResult<TransferCommitSummary> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::ConfirmNoChange { acknowledgment })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("无副作用提交确认已撤销"))?
    }

    /// 声明所有目标项均已获得回执并进入最终结果保护区，不代表再执行一次目标提交。
    pub async fn finish_commit(&self) -> AppResult<TransferCommitSummary> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::FinishCommit { acknowledgment })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("最终提交回执确认已撤销"))?
    }

    /// 全量遍历证明目标没有变化后结束；控制通道原子验证无活动提交和既有回执。
    pub async fn finish_no_changes(&self) -> AppResult<TransferCommitSummary> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::FinishNoChanges { acknowledgment })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("无变更结果确认已撤销"))?
    }

    /// 在数据结果已线性化后登记待清理资源，使终态摘要和 clear/shutdown 保留真实 owner。
    ///
    /// @author kongweiguang
    pub async fn retain_cleanup_owner(
        &self,
        owner: Arc<dyn TransferResourceCleanup>,
    ) -> AppResult<()> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::RetainCleanupOwner {
                owner,
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止，未能登记清理 owner"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("清理 owner 登记确认已撤销"))?
    }

    /// 仅在同一资源 owner 确认释放后登记完成，防止旧清理回执移除新的待清理资源。
    ///
    /// @author kongweiguang
    pub async fn release_cleanup_owner(
        &self,
        owner: Arc<dyn TransferResourceCleanup>,
    ) -> AppResult<()> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::ReleaseCleanupOwner {
                owner,
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止，未能确认清理 owner"))?;
        result
            .await
            .map_err(|_| AppError::cancelled("清理 owner 释放确认已撤销"))?
    }

    /// 通过单一有界控制通道等待状态机确认，并将可选 checkpoint 间接存储，避免大字段撑大控制消息；
    /// 测试仍可在请求排队后构造竞态窗口。
    ///
    /// @author kongweiguang
    async fn request_commit(
        &self,
        destination: FileEndpoint,
        confirmed_bytes: u64,
        total_bytes: Option<u64>,
        checkpoint: Option<TransferCheckpoint>,
        enqueued: Option<oneshot::Sender<()>>,
    ) -> AppResult<()> {
        let (acknowledgment, result) = oneshot::channel();
        self.control_sender
            .send(TransferControlRequest::BeginCommit {
                destination,
                confirmed_bytes,
                total_bytes,
                checkpoint: checkpoint.map(Box::new),
                acknowledgment,
            })
            .await
            .map_err(|_| AppError::cancelled("传输队列已停止"))?;
        if let Some(enqueued) = enqueued {
            let _ = enqueued.send(());
        }
        result
            .await
            .map_err(|_| AppError::cancelled("传输提交许可已撤销"))?
    }
}

/// 队列事件仅用于唤醒界面刷新；接收方遇到 lag 应调用 list 重建完整快照。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferQueueEvent {
    Changed,
    /// 清理终态后通知观察者重新读取完整快照，包括队列已变为空的情况。
    Cleared,
}

/// 有界广播订阅只向调用方暴露唤醒信号，避免 UI 依赖 Tokio 广播错误类型。
pub struct TransferQueueSubscription {
    receiver: broadcast::Receiver<TransferQueueEvent>,
}

impl TransferQueueSubscription {
    /// 丢帧也返回唤醒，调用方随后读取完整快照，不把合并事件当作状态存储。
    ///
    /// @author kongweiguang
    pub async fn recv(&mut self, cancellation: CancellationToken) -> Option<()> {
        subscription::receive_transfer_wakeup(&mut self.receiver, &cancellation).await
    }
}

/// 传输错误的公开文案按稳定类别映射，原始 adapter detail 不进入 MCP/UI 投影。
///
/// @author kongweiguang
fn failure_message(kind: TransferFailureKind) -> &'static str {
    match kind {
        TransferFailureKind::IdleTimeout => "传输长时间没有确认进度",
        TransferFailureKind::Connection => "传输连接失败",
        TransferFailureKind::Permission => "目标权限不足",
        TransferFailureKind::Conflict => "传输目标发生冲突",
        TransferFailureKind::InvalidSource => "传输源已变化或不可读取",
        TransferFailureKind::Unsupported => "当前端点组合不支持传输",
        TransferFailureKind::CommitUnknown => "无法确认目标文件是否已提交，请先检查目标",
        TransferFailureKind::Internal => "传输未能完成",
    }
}
