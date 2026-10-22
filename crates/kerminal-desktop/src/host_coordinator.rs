//! 有界串行宿主协调器，统一退出、更新安装和单实例资源收尾。
//!
//! @author kongweiguang

#[cfg(any(windows, test))]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(not(target_os = "windows"))]
use std::process::Command;

use kerminal_application::{
    AppError, AppResult,
    updates::{DownloadedUpdateId, UpdateInstallDisposition, UpdateInstallResult, UpdateService},
};
use kerminal_ui::QuitIntent;
#[cfg(any(windows, test))]
use single_application::SingleApplication;
use single_application::{PrimaryInstance, PrimaryInstanceShutdownHandle};
use tokio::{
    sync::{Mutex as AsyncMutex, mpsc, oneshot},
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::composition::DesktopComposition;
use crate::external_activation::{ActivationQueueSender, decode_message};

const HOST_QUEUE_CAPACITY: usize = 8;

#[cfg(feature = "acceptance-metrics")]
pub(crate) mod quit_diagnostics;

/// 宿主边界收到的状态转换；安装重试显式绕过已完成的 composition shutdown。
#[derive(Clone, Debug)]
pub(crate) enum HostCommand {
    Quit {
        intent: QuitIntent,
        deadline: Instant,
    },
    Install {
        update_id: DownloadedUpdateId,
        version: String,
        deadline: Instant,
        retry: bool,
    },
    Finalize {
        deadline: Instant,
    },
}

/// 命令执行后回给 GPUI 的最小结果；界面只负责状态展示和退出，不接触宿主资源。
pub(crate) enum HostResponse {
    Quit {
        intent: QuitIntent,
        result: Result<(), String>,
    },
    UpdateQuiesceFailed {
        update_id: DownloadedUpdateId,
        version: String,
        message: String,
    },
    UpdateCompleted {
        update_id: DownloadedUpdateId,
        version: String,
        result: UpdateInstallResult,
        action: UpdateHostAction,
    },
    Finalized(Result<(), String>),
}

/// 更新结果决定 UI 保持可重试、退出或显示已安装但无法自动重启的确定状态。
pub(crate) enum UpdateHostAction {
    KeepOpen,
    Quit,
    #[cfg(not(target_os = "windows"))]
    RestartFailed(&'static str),
    #[cfg(windows)]
    ListenerRecovery(PrimaryListenerRecovery),
}

/// Windows 更新无副作用失败后，单实例入口必须恢复或明确报告已被其他主实例占用。
#[cfg(any(windows, test))]
pub(crate) enum PrimaryListenerRecovery {
    Restored,
    Occupied,
    Failed(AppError),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateCompletionPath {
    KeepOpen,
    Restart,
}

struct HostRequest {
    command: HostCommand,
    completion: oneshot::Sender<HostResponse>,
}

/// Root 持有的有界命令发送端；容量耗尽时等待队列消费，绝不静默丢退出意图。
#[derive(Clone)]
pub(crate) struct HostCoordinatorSender {
    sender: mpsc::Sender<HostRequest>,
}

/// 仍由 desktop 持有实际 composition 和 coordinator JoinHandle，失败后可以继续收尾。
pub(crate) struct HostCoordinator {
    sender: HostCoordinatorSender,
    task: JoinHandle<()>,
    composition: std::sync::Arc<AsyncMutex<DesktopComposition>>,
    listener: std::sync::Arc<AsyncMutex<PrimaryListener>>,
    finalize_receiver: Option<oneshot::Receiver<HostResponse>>,
    task_done: bool,
}

/// 主实例监听 task 与取消令牌保持同一 owner，截止时间耗尽时不丢弃 JoinHandle。
/// Windows 更新恢复元数据仅在 Windows 产品构建保留，跨平台单测仍覆盖恢复状态路径。
pub(crate) struct PrimaryListener {
    #[cfg(any(windows, test))]
    instance_id: String,
    #[cfg(any(windows, test))]
    activation_sender: ActivationQueueSender,
    #[cfg(any(windows, test))]
    runtime: tokio::runtime::Handle,
    cancellation: CancellationToken,
    task: Option<JoinHandle<()>>,
    package_listener: Option<PrimaryInstanceShutdownHandle>,
}

impl HostCoordinator {
    /// 在唯一 Tokio runtime 上启动串行 worker，保持 shutdown/update owner 跨 UI 回调存活。
    pub(crate) fn start(
        handle: tokio::runtime::Handle,
        composition: std::sync::Arc<AsyncMutex<DesktopComposition>>,
        update_service: Option<UpdateService>,
        listener: PrimaryListener,
    ) -> Self {
        let (sender, receiver) = mpsc::channel(HOST_QUEUE_CAPACITY);
        let sender = HostCoordinatorSender { sender };
        let listener = std::sync::Arc::new(AsyncMutex::new(listener));
        let task = handle.spawn(run_host_coordinator(
            receiver,
            composition.clone(),
            update_service,
            listener.clone(),
        ));
        Self {
            sender,
            task,
            composition,
            listener,
            finalize_receiver: None,
            task_done: false,
        }
    }

    /// 克隆轻量发送端给 Root 订阅；队列所有权仍由宿主 wrapper 持有。
    pub(crate) fn sender(&self) -> HostCoordinatorSender {
        self.sender.clone()
    }

    /// GPUI loop 结束后复用同一 owner 完成关闭；固定提交事件可区分最终补偿与窗口内主动退出。
    ///
    /// @author kongweiguang
    pub(crate) async fn finalize(&mut self, deadline: Instant) -> AppResult<()> {
        if !self.task_done && self.finalize_receiver.is_none() {
            tracing::info!(event = "HOST_FINALIZE_SUBMITTED");
            let (completion, receiver) = oneshot::channel();
            let request = HostRequest {
                command: HostCommand::Finalize { deadline },
                completion,
            };
            match timeout_at(deadline, self.sender.sender.send(request)).await {
                Ok(Ok(())) => self.finalize_receiver = Some(receiver),
                Ok(Err(_)) if self.task.is_finished() => {}
                Ok(Err(_)) => return Err(AppError::infrastructure("桌面宿主协调器已停止")),
                Err(_) => return Err(AppError::timeout("等待宿主收尾进入有界队列")),
            }
        }

        let mut result = if let Some(receiver) = self.finalize_receiver.as_mut() {
            match timeout_at(deadline, receiver).await {
                Ok(Ok(HostResponse::Finalized(result))) => {
                    self.finalize_receiver = None;
                    result.map_err(|message| {
                        AppError::infrastructure_with_source(
                            "桌面宿主最终收尾失败",
                            std::io::Error::other(message),
                        )
                    })
                }
                Ok(Ok(_)) => {
                    self.finalize_receiver = None;
                    Err(AppError::infrastructure("宿主收尾返回了不匹配的结果"))
                }
                Ok(Err(_)) => {
                    self.finalize_receiver = None;
                    Err(AppError::infrastructure("桌面宿主未返回收尾结果"))
                }
                Err(_) => return Err(AppError::timeout("等待桌面宿主收尾回执")),
            }
        } else {
            Err(AppError::infrastructure("桌面宿主协调器已完成"))
        };

        if !self.task_done {
            match timeout_at(deadline, &mut self.task).await {
                Ok(Ok(())) => self.task_done = true,
                Ok(Err(join_error)) => {
                    self.task_done = true;
                    if result.is_ok() {
                        result = Err(AppError::infrastructure_with_source(
                            "宿主协调器异常终止",
                            join_error,
                        ));
                    }
                }
                Err(_) => return Err(AppError::timeout("等待宿主协调器完成退出")),
            }
        }

        if result.is_err() {
            let mut composition = timeout_at(deadline, self.composition.lock())
                .await
                .map_err(|_| AppError::timeout("等待桌面资源收尾锁"))?;
            let cleanup = timeout_at(deadline, composition.shutdown_until(deadline))
                .await
                .map_err(|_| AppError::timeout("重试桌面资源收尾"))?;
            let mut listener = timeout_at(deadline, self.listener.lock())
                .await
                .map_err(|_| AppError::timeout("等待单实例监听器收尾锁"))?;
            let listener_result = listener.stop_until(deadline).await;
            if cleanup.is_ok() && listener_result.is_ok() {
                return Ok(());
            }
        }
        result
    }
}

impl HostCoordinatorSender {
    /// 排队与回执仅记录固定事件，保留原错误语义并区分请求未入队和入队后丢失回执。
    ///
    /// @author kongweiguang
    pub(crate) async fn submit(&self, command: HostCommand) -> AppResult<HostResponse> {
        match &command {
            HostCommand::Quit { .. } => tracing::info!(event = "HOST_QUIT_SUBMITTED"),
            HostCommand::Install { .. } => tracing::info!(event = "HOST_INSTALL_SUBMITTED"),
            HostCommand::Finalize { .. } => tracing::info!(event = "HOST_FINALIZE_SUBMITTED"),
        }
        let (completion, receiver) = oneshot::channel();
        self.sender
            .send(HostRequest {
                command,
                completion,
            })
            .await
            .map_err(|_| {
                tracing::warn!(event = "HOST_COMMAND_SEND_FAILED", code = "infrastructure");
                AppError::infrastructure("桌面宿主协调器已停止")
            })?;
        receiver.await.map_err(|_| {
            tracing::warn!(event = "HOST_COMMAND_RECEIPT_LOST", code = "infrastructure");
            AppError::infrastructure("桌面宿主协调器未返回结果")
        })
    }
}

impl PrimaryListener {
    /// 使用同一 instance ID 与激活队列；仅 Windows 更新失败恢复需要在关闭后保留这些状态。
    pub(crate) fn new(
        _instance_id: String,
        activation_sender: ActivationQueueSender,
        runtime: tokio::runtime::Handle,
        primary: PrimaryInstance<String>,
    ) -> Self {
        let package_listener = primary.shutdown_handle();
        let cancellation = CancellationToken::new();
        let task = spawn_message_forwarder(
            &runtime,
            cancellation.clone(),
            primary,
            activation_sender.clone(),
        );
        Self {
            #[cfg(any(windows, test))]
            instance_id: _instance_id,
            #[cfg(any(windows, test))]
            activation_sender,
            #[cfg(any(windows, test))]
            runtime,
            cancellation,
            task: Some(task),
            package_listener: Some(package_listener),
        }
    }

    /// 同一绝对 deadline 内停止 consumer 与 package accept loop；任一超时都保留原 owner 供重试。
    async fn stop_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.cancellation.cancel();
        let consumer_result = if let Some(task) = self.task.as_mut() {
            match timeout_at(deadline, task).await {
                Ok(Ok(())) => {
                    self.task = None;
                    Ok(())
                }
                Ok(Err(error)) => {
                    self.task = None;
                    Err(AppError::infrastructure_with_source(
                        "主实例消息转发任务异常终止",
                        error,
                    ))
                }
                Err(_) => Err(AppError::timeout("等待主实例消息转发任务退出")),
            }
        } else {
            Ok(())
        };
        let package_result = if let Some(package_listener) = self.package_listener.as_ref() {
            match package_listener.shutdown_until(deadline).await {
                Ok(()) => {
                    self.package_listener = None;
                    Ok(())
                }
                Err(error) => Err(AppError::infrastructure_with_source(
                    "关闭单实例 socket listener",
                    error,
                )),
            }
        } else {
            Ok(())
        };

        match (consumer_result, package_result) {
            (Err(error), _) => Err(error),
            (Ok(()), Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    /// 旧 listener 完整 join 后才重新争用原身份，并把新消息转发到原 activation queue。
    /// Windows 产品路径需要它；Linux/macOS 单测保留相同状态机覆盖，产品构建不带此方法。
    #[cfg(any(windows, test))]
    async fn restore_until(&mut self, deadline: Instant) -> PrimaryListenerRecovery {
        if Instant::now() >= deadline {
            return PrimaryListenerRecovery::Failed(AppError::timeout(
                "等待更新后恢复单实例监听器",
            ));
        }
        if self.task.is_some() || self.package_listener.is_some() {
            return PrimaryListenerRecovery::Failed(AppError::conflict(
                "旧单实例监听器尚未完成关闭，不能并行恢复",
            ));
        }

        let acquired = catch_unwind(AssertUnwindSafe(|| {
            SingleApplication::<String>::new(&self.instance_id)
        }));
        let primary = match acquired {
            Ok(SingleApplication::PrimaryInstance(primary)) => primary,
            Ok(SingleApplication::SecondaryInstance(_)) => {
                return PrimaryListenerRecovery::Occupied;
            }
            Err(_) => {
                return PrimaryListenerRecovery::Failed(AppError::infrastructure(
                    "无法恢复 Kerminal 单实例监听器",
                ));
            }
        };

        self.package_listener = Some(primary.shutdown_handle());
        self.cancellation = CancellationToken::new();
        self.task = Some(spawn_message_forwarder(
            &self.runtime,
            self.cancellation.clone(),
            primary,
            self.activation_sender.clone(),
        ));

        // Listener 已成功重建并由同一 owner 接管，回执必须反映实际状态；超时检查只在获取前执行。
        PrimaryListenerRecovery::Restored
    }
}

/// 每次恢复都在唯一 runtime 上创建一条转发任务，停止时可由 PrimaryListener join。
fn spawn_message_forwarder(
    runtime: &tokio::runtime::Handle,
    cancellation: CancellationToken,
    primary: PrimaryInstance<String>,
    activation_sender: ActivationQueueSender,
) -> JoinHandle<()> {
    runtime.spawn(async move {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {},
            _ = forward_primary_messages(primary, activation_sender) => {},
        }
    })
}

/// 接收本进程的单实例消息并转入有界 Root 队列，错误日志不包含 wire 内容。
async fn forward_primary_messages(
    mut primary: PrimaryInstance<String>,
    queue: ActivationQueueSender,
) {
    while let Some(wire) = primary.recv().await {
        let request = match decode_message(&wire) {
            Ok(request) => request,
            Err(_) => {
                tracing::warn!(event = "SINGLE_INSTANCE_MESSAGE_INVALID");
                continue;
            }
        };
        if queue.submit(request).await.is_err() {
            tracing::warn!(event = "ACTIVATION_QUEUE_STOPPED");
            break;
        }
    }
}

/// 串行处理同一 owner 的关闭重试；日志只记录固定命令阶段，避免失败活窗再次无法定位。
/// Quit 完成与回执 send 返回分开记录，后者仅确认交付及同步唤醒已返回，不代表 GPUI 已消费。
/// 验收构建在成功回执后后台采样两次，不等待诊断，也不把诊断加入业务关闭 owner。
/// @author kongweiguang
async fn run_host_coordinator(
    mut receiver: mpsc::Receiver<HostRequest>,
    composition: std::sync::Arc<AsyncMutex<DesktopComposition>>,
    update_service: Option<UpdateService>,
    listener: std::sync::Arc<AsyncMutex<PrimaryListener>>,
) {
    let mut quiesced = false;
    while let Some(request) = receiver.recv().await {
        match request.command {
            HostCommand::Quit { intent, deadline } => {
                tracing::info!(event = "HOST_QUIT_BEGIN");
                let result = match ensure_quiesced(&composition, &mut quiesced, deadline).await {
                    Ok(()) => stop_listener_until(&listener, deadline)
                        .await
                        .map_err(|error| error.public_message().to_owned()),
                    Err(error) => Err(error.public_message().to_owned()),
                };
                let shutdown_complete = result.is_ok();
                if shutdown_complete {
                    tracing::info!(event = "HOST_QUIT_COMPLETE");
                } else {
                    tracing::warn!(event = "HOST_QUIT_FAILED");
                }
                if request
                    .completion
                    .send(HostResponse::Quit { intent, result })
                    .is_ok()
                {
                    tracing::info!(event = "HOST_QUIT_RECEIPT_SENT");
                    #[cfg(feature = "acceptance-metrics")]
                    if shutdown_complete {
                        quit_diagnostics::sample_after_successful_quit();
                    }
                }
            }
            HostCommand::Install {
                update_id,
                version,
                deadline,
                retry,
            } => {
                tracing::info!(event = "HOST_INSTALL_BEGIN");
                let response = install_update(
                    update_id,
                    version,
                    deadline,
                    retry,
                    InstallUpdateContext {
                        quiesced: &mut quiesced,
                        composition: &composition,
                        update_service: update_service.as_ref(),
                        listener: &listener,
                    },
                )
                .await;
                match &response {
                    HostResponse::UpdateQuiesceFailed { .. } => {
                        tracing::warn!(event = "HOST_INSTALL_QUIESCE_FAILED");
                    }
                    HostResponse::UpdateCompleted { result, .. } => match result {
                        UpdateInstallResult::Installed(_) => {
                            tracing::info!(event = "HOST_INSTALL_COMPLETE");
                        }
                        UpdateInstallResult::NotStarted(error)
                        | UpdateInstallResult::FailedNoEffect(error) => {
                            tracing::warn!(
                                event = "HOST_INSTALL_FAILED",
                                code = error.public_code().as_str()
                            );
                        }
                        UpdateInstallResult::OutcomeUnknown(error) => {
                            tracing::warn!(
                                event = "HOST_INSTALL_OUTCOME_UNKNOWN",
                                code = error.public_code().as_str()
                            );
                        }
                    },
                    _ => {}
                }
                let _ = request.completion.send(response);
            }
            HostCommand::Finalize { deadline } => {
                tracing::info!(event = "HOST_FINALIZE_BEGIN");
                let shutdown = ensure_quiesced(&composition, &mut quiesced, deadline)
                    .await
                    .map_err(|error| error.public_message().to_owned());
                let listener_result = stop_listener_until(&listener, deadline)
                    .await
                    .map_err(|error| error.public_message().to_owned());
                let result = shutdown.and(listener_result);
                if result.is_ok() {
                    tracing::info!(event = "HOST_FINALIZE_COMPLETE");
                } else {
                    tracing::warn!(event = "HOST_FINALIZE_FAILED");
                }
                let _ = request.completion.send(HostResponse::Finalized(result));
                return;
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/support/host_coordinator_unit.rs"]
mod tests;

/// 保留原 owner/截止时间和错误类别；固定事件只报告 composition 阶段，不泄露聚合错误的动态正文。
///
/// @author kongweiguang
async fn ensure_quiesced(
    composition: &std::sync::Arc<AsyncMutex<DesktopComposition>>,
    quiesced: &mut bool,
    deadline: Instant,
) -> AppResult<()> {
    if *quiesced {
        return Ok(());
    }
    tracing::info!(event = "HOST_COMPOSITION_STOP_BEGIN");
    let result: AppResult<()> = async {
        let mut composition = timeout_at(deadline, composition.lock())
            .await
            .map_err(|_| AppError::timeout("等待桌面资源收尾锁"))?;
        composition.shutdown_until(deadline).await?;
        *quiesced = true;
        Ok(())
    }
    .await;
    match &result {
        Ok(()) => tracing::info!(event = "HOST_COMPOSITION_STOP_COMPLETE"),
        Err(error) => tracing::warn!(
            event = "HOST_COMPOSITION_STOP_FAILED",
            code = error.public_code().as_str()
        ),
    }
    result
}

/// 在同一截止时间停止 listener；只记录稳定错误码，超时仍由同一 owner 保留实际重试能力。
///
/// @author kongweiguang
async fn stop_listener_until(
    listener: &std::sync::Arc<AsyncMutex<PrimaryListener>>,
    deadline: Instant,
) -> AppResult<()> {
    tracing::info!(event = "HOST_LISTENER_STOP_BEGIN");
    let result: AppResult<()> = async {
        let mut listener = timeout_at(deadline, listener.lock())
            .await
            .map_err(|_| AppError::timeout("等待单实例监听器收尾锁"))?;
        listener.stop_until(deadline).await
    }
    .await;
    match &result {
        Ok(()) => tracing::info!(event = "HOST_LISTENER_STOP_COMPLETE"),
        Err(error) => tracing::warn!(
            event = "HOST_LISTENER_STOP_FAILED",
            code = error.public_code().as_str()
        ),
    }
    result
}

/// 安装用例共享宿主资源上下文，保留退出重试和 listener 恢复所需的同一 owner。
/// @author kongweiguang
struct InstallUpdateContext<'a> {
    quiesced: &'a mut bool,
    composition: &'a std::sync::Arc<AsyncMutex<DesktopComposition>>,
    update_service: Option<&'a UpdateService>,
    listener: &'a std::sync::Arc<AsyncMutex<PrimaryListener>>,
}

/// 先完成应用资源收尾，再直接调用保留的 UpdateService；重试只允许复用已 quiesce 的宿主。
/// @author kongweiguang
async fn install_update(
    update_id: DownloadedUpdateId,
    version: String,
    deadline: Instant,
    retry: bool,
    context: InstallUpdateContext<'_>,
) -> HostResponse {
    let InstallUpdateContext {
        quiesced,
        composition,
        update_service,
        listener,
    } = context;
    if retry {
        if !*quiesced {
            return HostResponse::UpdateCompleted {
                update_id,
                version,
                result: UpdateInstallResult::NotStarted(AppError::conflict(
                    "更新重试前宿主尚未完成资源收尾",
                )),
                action: UpdateHostAction::KeepOpen,
            };
        }
    } else if let Err(error) = ensure_quiesced(composition, quiesced, deadline).await {
        return HostResponse::UpdateQuiesceFailed {
            update_id,
            version,
            message: error.public_message().to_owned(),
        };
    }

    let Some(update_service) = update_service.cloned() else {
        return HostResponse::UpdateCompleted {
            update_id,
            version,
            result: UpdateInstallResult::NotStarted(AppError::unsupported(
                "当前构建没有可用的更新安装服务",
            )),
            action: UpdateHostAction::KeepOpen,
        };
    };

    #[cfg(target_os = "windows")]
    if let Err(error) = stop_listener_until(listener, deadline).await {
        return HostResponse::UpdateQuiesceFailed {
            update_id,
            version,
            message: error.public_message().to_owned(),
        };
    }

    let result = match tokio::spawn(async move { update_service.install(update_id).await }).await {
        Ok(result) => result,
        Err(error) => UpdateInstallResult::OutcomeUnknown(AppError::outcome_unknown_with_source(
            "更新安装调用的最终结果未知",
            error,
        )),
    };

    let action = match classify_install_result(&result) {
        UpdateCompletionPath::KeepOpen => {
            #[cfg(target_os = "windows")]
            {
                let recovery = match restore_listener_until(listener, deadline).await {
                    Ok(recovery) => recovery,
                    Err(error) => PrimaryListenerRecovery::Failed(error),
                };
                UpdateHostAction::ListenerRecovery(recovery)
            }
            #[cfg(not(target_os = "windows"))]
            {
                UpdateHostAction::KeepOpen
            }
        }
        UpdateCompletionPath::Restart => {
            #[cfg(target_os = "windows")]
            {
                let _ = listener;
                UpdateHostAction::Quit
            }
            #[cfg(not(target_os = "windows"))]
            {
                match stop_listener_until(listener, deadline).await {
                    Ok(()) if restart_current_process().is_ok() => UpdateHostAction::Quit,
                    Ok(()) => UpdateHostAction::RestartFailed(
                        "安装已完成，但新版进程未能启动；请手动重新打开 Kerminal。",
                    ),
                    Err(_) => UpdateHostAction::RestartFailed(
                        "安装已完成，但单实例监听器未能关闭；请确认进程退出后重新打开 Kerminal。",
                    ),
                }
            }
        }
    };

    let _ = deadline;
    HostResponse::UpdateCompleted {
        update_id,
        version,
        result,
        action,
    }
}

/// 仅 Windows 更新失败后需要恢复已关闭的 listener；其他平台在无副作用失败时监听仍保持运行。
///
/// 在同一宿主 owner 上串行恢复 listener；超时保留其内部任务句柄，不创建第二条 accept loop。
#[cfg(windows)]
async fn restore_listener_until(
    listener: &std::sync::Arc<AsyncMutex<PrimaryListener>>,
    deadline: Instant,
) -> AppResult<PrimaryListenerRecovery> {
    let mut listener = timeout_at(deadline, listener.lock())
        .await
        .map_err(|_| AppError::timeout("等待单实例恢复 owner"))?;
    Ok(listener.restore_until(deadline).await)
}

/// 结果未知时保留 Root 的人工确认状态；只对确定安装结果启动新版进程。
fn classify_install_result(result: &UpdateInstallResult) -> UpdateCompletionPath {
    match result {
        UpdateInstallResult::NotStarted(_)
        | UpdateInstallResult::FailedNoEffect(_)
        | UpdateInstallResult::OutcomeUnknown(_) => UpdateCompletionPath::KeepOpen,
        UpdateInstallResult::Installed(UpdateInstallDisposition::RestartRequired) => {
            UpdateCompletionPath::Restart
        }
    }
}

/// 只在宿主已同步关闭旧 listener 后重启当前可执行文件，避免建立第二条等待启动路径。
#[cfg(not(target_os = "windows"))]
fn restart_current_process() -> std::io::Result<()> {
    let executable = std::env::current_exe()?;
    let working_directory = std::env::current_dir()?;
    Command::new(executable)
        .current_dir(working_directory)
        .spawn()
        .map(|_| ())
}
