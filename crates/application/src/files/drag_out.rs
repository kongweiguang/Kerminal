//! 将拖出来源交给统一传输队列，系统拖放只接收已确认完成的暂存路径。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest,
        TransferStatus, TransferSummary,
    },
    identity::TransferId,
};
use tokio_util::sync::CancellationToken;

use crate::{
    AppError, AppResult, files::FileService, platform::external_launch::ExternalTargetLease,
    transfers::TransferService,
};

/// 暂存 owner 负责限制路径和清理；不能确认传输停止时必须保留整个目录。
#[async_trait::async_trait]
pub trait DragStaging: Send + Sync + 'static {
    /// 只接受可安全映射到本机文件系统的单层文件名。
    fn path_for(&self, name: &str) -> AppResult<PathBuf>;

    /// 返回实际暂存根，供未知结果向用户提供可核对的位置。
    fn directory(&self) -> PathBuf;

    /// 结果未知时放弃自动清理，避免删除仍可能被传输持有的文件。
    fn preserve(&self);

    /// 确认材料确实被保留，避免向用户展示已经删除的目录。
    fn is_preserved(&self) -> bool;

    /// 只有确定无活动传输及目标读取后，才显式删除私有目录。
    async fn cleanup(&self) -> AppResult<()>;
}

/// 一次拖动冻结条目类型与临时连接 lease，切换面板后也不得改用新目标。
#[derive(Clone)]
pub struct DragOutSource {
    pub endpoint: FileEndpoint,
    pub name: String,
    pub kind: TransferKind,
    pub external_target_lease: Option<ExternalTargetLease>,
}

/// 已完成的暂存路径与所有权一起交给系统层，路径生命周期不得早于目标读取。
pub struct PreparedDragOut {
    pub paths: Vec<(PathBuf, bool)>,
    pub session: Arc<DragOutSession>,
}

/// 每次拖动共同拥有暂存根和全部恢复记录，直到目标和文件流都释放它们。
pub struct DragOutSession {
    staging: Arc<dyn DragStaging>,
    transfers: TransferService,
    uncertain: AtomicBool,
    cleaned: AtomicBool,
    cleanup_lock: tokio::sync::Mutex<()>,
}

impl DragOutSession {
    /// 提供本机恢复位置，不暴露远端身份或认证材料。
    ///
    /// @author kongweiguang
    pub fn directory(&self) -> PathBuf {
        self.staging.directory()
    }

    /// 同步冻结未知状态，供 COM 收尾时立刻保护 owner 而无需等待应用执行器。
    ///
    /// @author kongweiguang
    pub fn preserve(&self) {
        self.uncertain.store(true, Ordering::Release);
        self.staging.preserve();
    }

    /// 结果未知时恢复列表必须可见，包括已经失去原 UI 目标的旧拖动。
    ///
    /// @author kongweiguang
    pub async fn retain_unknown(&self) {
        self.preserve();
        self.transfers
            .expose_drag_staging_evidence(&self.directory())
            .await;
    }

    /// 删除前复验所有队列任务，再清理磁盘，最后移除持久恢复记录。
    ///
    /// @author kongweiguang
    pub async fn cleanup(&self) -> AppResult<()> {
        let _cleanup = self.cleanup_lock.lock().await;
        if self.cleaned.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.uncertain.load(Ordering::Acquire) {
            return Err(AppError::outcome_unknown("拖出结果未知，已保留暂存目录"));
        }
        let directory = self.directory();
        if let Err(error) = self
            .transfers
            .validate_drag_staging_release(&directory)
            .await
        {
            self.retain_unknown().await;
            return Err(error);
        }
        if let Err(error) = self.staging.cleanup().await {
            self.retain_unknown().await;
            return Err(error);
        }
        if let Err(error) = self
            .transfers
            .release_drag_staging_evidence(&directory)
            .await
        {
            self.retain_unknown().await;
            return Err(error);
        }
        self.cleaned.store(true, Ordering::Release);
        Ok(())
    }

    /// 只在根目录确实保留时对外显示其位置。
    ///
    /// @author kongweiguang
    pub fn is_preserved(&self) -> bool {
        self.staging.is_preserved()
    }

    /// 异步结果未知后禁止常规清理，即使传输后续返回终态。
    ///
    /// @author kongweiguang
    pub fn is_uncertain(&self) -> bool {
        self.uncertain.load(Ordering::Acquire)
    }
}

/// 逐项通过统一队列暂存；任何一项失败都阻止发布整组文件路径。
///
/// @author kongweiguang
pub async fn prepare_drag_out(
    files: FileService,
    transfers: TransferService,
    cancellation: CancellationToken,
    sources: Vec<DragOutSource>,
) -> AppResult<PreparedDragOut> {
    let captured = Arc::new(Mutex::new(None::<Arc<DragOutSession>>));
    let observed = captured.clone();
    let result =
        prepare_drag_out_observed(files, transfers, cancellation, sources, move |session| {
            *observed.lock().unwrap_or_else(|error| error.into_inner()) = Some(session);
        })
        .await;
    if result.is_err() {
        let session = captured
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(session) = session {
            let _ = session.cleanup().await;
        }
    }
    result
}

/// 在创建任务前发布暂存 owner，使未知提交也能回报原目录。
///
/// @author kongweiguang
pub async fn prepare_drag_out_observed(
    files: FileService,
    transfers: TransferService,
    cancellation: CancellationToken,
    sources: Vec<DragOutSource>,
    on_staging: impl FnOnce(Arc<DragOutSession>) + Send,
) -> AppResult<PreparedDragOut> {
    if sources.is_empty() {
        return Err(AppError::invalid("没有可拖出的文件或目录"));
    }
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("文件拖出已取消"));
    }

    let staging = files.create_drag_staging().await?;
    staging.preserve();
    let session = Arc::new(DragOutSession {
        staging: staging.clone(),
        transfers: transfers.clone(),
        uncertain: AtomicBool::new(false),
        cleaned: AtomicBool::new(false),
        cleanup_lock: tokio::sync::Mutex::new(()),
    });
    on_staging(session.clone());
    let mut destinations = HashSet::with_capacity(sources.len());
    let mut paths = Vec::with_capacity(sources.len());
    for source in sources {
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("文件拖出已取消"));
        }
        let path = staging.path_for(&source.name)?;
        if !destinations.insert(source.name.to_lowercase()) {
            return Err(AppError::conflict("选择的项目在本机存在同名冲突"));
        }
        let destination = FileEndpoint::Local { path: path.clone() };
        let request = TransferRequest {
            source: source.endpoint,
            destination: destination.clone(),
            kind: source.kind,
            conflict_policy: ConflictPolicy::Skip,
            idle_timeout_seconds: None,
        };
        let mut subscription = transfers.subscribe();
        let summary = transfers
            .enqueue_drag_staging(request, source.external_target_lease.as_ref())
            .await?;
        let result = loop {
            if cancellation.is_cancelled() {
                break None;
            }
            match transfers.get_settled(&summary.id).await {
                Ok(Some(current)) => break Some(current),
                Ok(None) => {}
                Err(_) => break None,
            }
            if subscription.recv(cancellation.clone()).await.is_none() {
                break None;
            }
        };
        let Some(result) = result else {
            let result = cancel_and_drain(&transfers, &summary.id).await;
            if result.as_ref().is_none_or(transfer_cleanup_uncertain) {
                session.retain_unknown().await;
            }
            return Err(AppError::cancelled("文件拖出已取消或传输状态不可用"));
        };
        if transfer_cleanup_uncertain(&result) {
            session.retain_unknown().await;
        }
        if result.status != TransferStatus::Succeeded
            || result.actual_destination.as_ref() != Some(&destination)
            || result.cleanup_pending
        {
            return Err(AppError::infrastructure(
                "拖出文件暂存未完成，请检查传输列表",
            ));
        }
        paths.push((path, source.kind == TransferKind::Directory));
    }
    if cancellation.is_cancelled() {
        return Err(AppError::cancelled("文件拖出已取消"));
    }
    Ok(PreparedDragOut { paths, session })
}

/// 终态也可能保留后端清理 owner 或未知提交，不能仅根据状态删除暂存目录。
///
/// @author kongweiguang
fn transfer_cleanup_uncertain(summary: &TransferSummary) -> bool {
    summary.cleanup_pending || summary.failure_kind == Some(TransferFailureKind::CommitUnknown)
}

/// 取消后等待明确终态；超时交还未知状态，由暂存 owner 保留材料。
///
/// @author kongweiguang
async fn cancel_and_drain(transfers: &TransferService, id: &TransferId) -> Option<TransferSummary> {
    let mut subscription = transfers.subscribe();
    let _ = transfers.cancel(id).await;
    tokio::time::timeout(Duration::from_secs(30), async {
        let cancellation = CancellationToken::new();
        loop {
            if let Some(summary) = transfers.get_settled(id).await.ok()? {
                return Some(summary);
            }
            subscription.recv(cancellation.clone()).await?;
        }
    })
    .await
    .ok()
    .flatten()
}
