//! 桌面更新用例与基础设施端口；应用层只处理版本候选和可观察进度。
//!
//! @author kongweiguang

use std::{future::Future, path::PathBuf, sync::Arc};

use async_trait::async_trait;
use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::{AppError, AppResult};

/// 用不同类型隔离远端候选，防止下载阶段误把已验证安装包 ID 当作输入。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UpdateCandidateId(Uuid);

impl UpdateCandidateId {
    /// 由适配器在一次检查结果进入运行态目录时分配，避免复用发布版本作为身份。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for UpdateCandidateId {
    /// 默认只创建新的不透明身份，不隐含网络检查或任何更新副作用。
    fn default() -> Self {
        Self::new()
    }
}

/// 下载成功并经 updater 签名验证后生成的独立身份。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct DownloadedUpdateId(Uuid);

impl DownloadedUpdateId {
    /// 只有 infra 完成传输和签名校验后才创建可安装身份。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for DownloadedUpdateId {
    /// 默认只创建不透明身份，不代表更新包已经下载或通过验签。
    fn default() -> Self {
        Self::new()
    }
}

/// 跨进程更新恢复记录的独立身份，不能误传为可安装的下载句柄。
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct UpdateRecoveryId(Uuid);

impl UpdateRecoveryId {
    /// 由基础设施在写入恢复记录时创建，UI 只能用它确认或清理恢复材料。
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// 从恢复目录中的 UUID 重建类型化身份，不表示包已验签或允许安装。
    pub fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// 将不透明恢复身份序列化进受限 receipt 文件名与正文。
    pub fn as_uuid(self) -> Uuid {
        self.0
    }
}

impl Default for UpdateRecoveryId {
    /// 默认只分配不透明恢复身份，不触发更新副作用。
    fn default() -> Self {
        Self::new()
    }
}

/// 启动时展示的恢复状态；任何状态都不能被 UI 当作自动安装授权。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateRecoveryState {
    PendingConfirmation,
    OutcomeUnknown,
    VersionMatchedAwaitingConfirmation,
}

/// 供设置界面呈现的最小恢复信息，不含更新 URL、签名、公钥或安装参数。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateRecoveryRecord {
    pub id: UpdateRecoveryId,
    pub version: String,
    pub target: String,
    pub artifact_sha256: String,
    pub state: UpdateRecoveryState,
    pub recovery_path: PathBuf,
}

/// 更新源配置与运行平台能力的合并状态；无源时仍保留恢复记录读取能力。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateAvailability {
    Configured,
    Unavailable { reason: &'static str },
}

/// 更新候选仅包含可展示的发布信息，不包含下载 URL、公钥或签名原文。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateCandidate {
    pub id: UpdateCandidateId,
    pub version: String,
    pub notes: Option<String>,
    pub published_at: Option<String>,
}

/// 进度采用 watch 最新值语义，终端高频下载回调不会堆积 UI 事件队列。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UpdateProgress {
    pub received_bytes: u64,
    pub total_bytes: Option<u64>,
}

/// 应用层持有的下载进度发送端；UI 不接触 Tokio watch 类型或通道细节。
pub struct UpdateProgressReporter {
    sender: watch::Sender<UpdateProgress>,
}

/// 共享的最新值订阅；锁只用于序列化多个异步读取者，不在等待期间阻塞线程。
#[derive(Clone)]
pub struct UpdateProgressSubscription {
    receiver: Arc<Mutex<watch::Receiver<UpdateProgress>>>,
}

impl UpdateProgressSubscription {
    /// 等待下一份最新快照或协作取消，把 watch 通道和 Tokio 细节留在 application。
    pub async fn next(&self, cancellation: CancellationToken) -> Option<UpdateProgress> {
        let mut receiver = self.receiver.lock().await;
        tokio::select! {
            biased;
            changed = receiver.changed() => changed
                .ok()
                .map(|()| *receiver.borrow_and_update()),
            _ = cancellation.cancelled() => None,
        }
    }
}

/// 下载成功的更新包句柄；其 ID 仅能由 infra 对应运行态目录解释。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadedUpdate {
    pub id: DownloadedUpdateId,
    pub version: String,
}

/// macOS/Linux 安装器已替换磁盘文件后，调用方负责结束旧进程并重新启动。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpdateInstallDisposition {
    RestartRequired,
}

/// 安装端口用确定性结果区分可安全重试与必须人工确认的提交状态。
#[derive(Clone, Debug)]
pub enum UpdateInstallResult {
    Installed(UpdateInstallDisposition),
    NotStarted(AppError),
    FailedNoEffect(AppError),
    OutcomeUnknown(AppError),
}

impl UpdateInstallResult {
    /// 只有能证明安装器尚未启动或目标未改变的失败才允许复用同一已验签句柄。
    pub fn can_retry(&self) -> bool {
        matches!(self, Self::NotStarted(_) | Self::FailedNoEffect(_))
    }
}

/// 更新端口隔离 blocking updater API、下载缓存与平台安装动作。
#[async_trait]
pub trait UpdatePort: Send + Sync + 'static {
    /// 返回静态能力状态，供 UI 在发起网络请求前明确呈现不可用原因。
    fn availability(&self) -> UpdateAvailability {
        UpdateAvailability::Configured
    }

    /// 检查当前构建目标；取消和网络错误都必须与“无更新”区分。
    async fn check(&self, cancel: CancellationToken) -> AppResult<Option<UpdateCandidate>>;

    /// 下载并验签一个确切候选；实现方必须拒绝重复下载和已失效 ID。
    async fn download(
        &self,
        candidate_id: UpdateCandidateId,
        cancel: CancellationToken,
        progress: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate>;

    /// 安装此前已通过签名校验的包；仅确定无副作用的结果可复用原句柄重试。
    async fn install(&self, update_id: DownloadedUpdateId) -> UpdateInstallResult;

    /// 用户在安装前取消时释放暂存包，避免占用内存和磁盘。
    async fn discard(&self, update_id: DownloadedUpdateId) -> AppResult<()>;

    /// 返回启动时需人工核实的恢复条目；默认无记录，避免非更新适配器伪造状态。
    async fn recoveries(&self) -> AppResult<Vec<UpdateRecoveryRecord>> {
        Ok(Vec::new())
    }

    /// 仅在用户已人工核实后清理指定恢复材料，绝不重放 SDK 安装。
    async fn acknowledge_recovery(&self, _recovery_id: UpdateRecoveryId) -> AppResult<()> {
        Err(AppError::unsupported("当前更新器不支持恢复材料确认"))
    }
}

/// 应用用例门面预检取消并将版本检查、下载和安装交给唯一端口实现。
#[derive(Clone)]
pub struct UpdateService {
    port: Arc<dyn UpdatePort>,
}

impl UpdateService {
    /// 由 composition root 注入单一 updater adapter，避免 UI 自行装配更新器。
    pub fn new(port: Arc<dyn UpdatePort>) -> Self {
        Self { port }
    }

    /// 查询同一更新适配器的当前能力；它不访问网络且不影响恢复材料。
    pub fn availability(&self) -> UpdateAvailability {
        self.port.availability()
    }

    /// 创建一个类型化进度通道；只有 application 能把 reporter 交给更新端口。
    pub fn progress_channel() -> (UpdateProgressReporter, UpdateProgressSubscription) {
        let (sender, receiver) = watch::channel(UpdateProgress::default());
        (
            UpdateProgressReporter { sender },
            UpdateProgressSubscription {
                receiver: Arc::new(Mutex::new(receiver)),
            },
        )
    }

    /// 用户已取消时不触发请求；错误由 adapter 保留脱敏 source 后回传。
    pub async fn check(&self, cancel: CancellationToken) -> AppResult<Option<UpdateCandidate>> {
        self.check_with_cancellation(cancel, CancellationToken::new())
            .await
    }

    /// 将用户取消与应用关闭合并，并等待端口任务完成收尾后再返回。
    pub async fn check_with_cancellation(
        &self,
        user_cancel: CancellationToken,
        application_cancel: CancellationToken,
    ) -> AppResult<Option<UpdateCandidate>> {
        let port = Arc::clone(&self.port);
        run_cancellable(
            "更新检查后台任务失败",
            "检查更新已取消",
            "应用退出时取消更新检查",
            user_cancel,
            application_cancel,
            move |cancel| async move { port.check(cancel).await },
        )
        .await
    }

    /// 在下载前检查取消状态，阻止已取消的更新继续进入阻塞网络 API。
    pub async fn download(
        &self,
        candidate_id: UpdateCandidateId,
        cancel: CancellationToken,
        progress: watch::Sender<UpdateProgress>,
    ) -> AppResult<DownloadedUpdate> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("下载更新已取消"));
        }
        self.port.download(candidate_id, cancel, progress).await
    }

    /// 通过 application-owned reporter 下载，并在取消后等待适配器释放网络和暂存资源。
    pub async fn download_with_cancellation(
        &self,
        candidate_id: UpdateCandidateId,
        user_cancel: CancellationToken,
        application_cancel: CancellationToken,
        progress: UpdateProgressReporter,
    ) -> AppResult<DownloadedUpdate> {
        let port = Arc::clone(&self.port);
        run_cancellable(
            "更新下载后台任务失败",
            "下载更新已取消",
            "应用退出时取消更新下载",
            user_cancel,
            application_cancel,
            move |cancel| async move { port.download(candidate_id, cancel, progress.sender).await },
        )
        .await
    }

    /// 只接受类型化已验签句柄，并把适配器对安装提交结果的确定性分类原样交给宿主。
    pub async fn install(&self, update_id: DownloadedUpdateId) -> UpdateInstallResult {
        self.port.install(update_id).await
    }

    /// 用户在安装提交前明确放弃时释放暂存包；适配器拒绝删除结果未知的恢复材料。
    pub async fn discard(&self, update_id: DownloadedUpdateId) -> AppResult<()> {
        self.port.discard(update_id).await
    }

    /// 查询持久化恢复条目供 UI 明确展示，调用本身不重试、安装或删除任何材料。
    pub async fn recoveries(&self) -> AppResult<Vec<UpdateRecoveryRecord>> {
        self.port.recoveries().await
    }

    /// 将用户对恢复结果的人工核实转给基础设施，只清理指定记录而不调用安装器。
    pub async fn acknowledge_recovery(&self, recovery_id: UpdateRecoveryId) -> AppResult<()> {
        self.port.acknowledge_recovery(recovery_id).await
    }
}

/// 将可取消端口调用置于受管 Tokio task 中；取消只先通知端口，再 join 其清理结果。
async fn run_cancellable<T, Build, Fut>(
    task_error_context: &'static str,
    user_cancel_context: &'static str,
    application_cancel_context: &'static str,
    user_cancel: CancellationToken,
    application_cancel: CancellationToken,
    build: Build,
) -> AppResult<T>
where
    T: Send + 'static,
    Build: FnOnce(CancellationToken) -> Fut + Send + 'static,
    Fut: Future<Output = AppResult<T>> + Send + 'static,
{
    if user_cancel.is_cancelled() {
        return Err(AppError::cancelled(user_cancel_context));
    }
    if application_cancel.is_cancelled() {
        return Err(AppError::cancelled(application_cancel_context));
    }

    let operation_cancel = CancellationToken::new();
    let task_cancel = operation_cancel.clone();
    let mut task = tokio::spawn(async move { build(task_cancel).await });
    tokio::select! {
        biased;
        result = &mut task => result
            .map_err(|error| AppError::infrastructure_with_source(task_error_context, error))?,
        _ = user_cancel.cancelled() => {
            operation_cancel.cancel();
            task.await
                .map_err(|error| AppError::infrastructure_with_source(task_error_context, error))?
        }
        _ = application_cancel.cancelled() => {
            operation_cancel.cancel();
            task.await
                .map_err(|error| AppError::infrastructure_with_source(task_error_context, error))?
        }
    }
}
