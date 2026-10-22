//! 平台原生 RDP 客户端启动与子进程/临时文件生命周期适配。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        RdpClientLifecycle, RdpLaunchReceipt, RdpLaunchStatus, RdpLaunchStatusService,
        RdpLaunchTarget, RdpPlatformLauncher,
    },
};
use tokio::{
    sync::{Mutex, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod artifact;
mod launch_status;
mod platform;

pub use launch_status::{RdpLaunchStatusHub, RdpLaunchStatusReporter};

#[cfg(target_os = "windows")]
#[path = "rdp/windows.rs"]
mod windows_security;

use crate::process_tree::ProcessTreeGuard;
use crate::process_tree::spawn::{ManagedTokioChild, ProcessSpawnOptions};
use artifact::TemporaryRdpArtifact;
use platform::build_launch_plan;

/// 本机 RDP 测试能证实的启动层级；系统打开器不等同于已核验的 RDP 客户端。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RdpProbeAvailability {
    /// Windows 专属值由 Windows 探测器构造；共享结果映射在其它平台仍需保留该变体。
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    WindowsMstsc,
    /// Linux 专属值由 Linux 探测器构造；共享结果映射在其它平台仍需保留该变体。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    LinuxFreeRdp,
    /// macOS 专属值由 macOS 探测器构造；共享结果映射在其它平台仍需保留该变体。
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    MacSystemLauncher,
}

/// 只探测平台客户端或系统启动器是否存在，不创建临时文件、不启动客户端、不连接远端。
pub(super) fn probe_client_availability() -> AppResult<RdpProbeAvailability> {
    platform::probe_client_availability()
}

/// 独占管理由本次启动的 RDP 客户端；不接管用户手动打开的客户端进程。
pub struct SystemRdpPlatformLauncher {
    runtime_directory: PathBuf,
    lifetime: CancellationToken,
    shutting_down: AtomicBool,
    clients: Mutex<HashMap<Uuid, ManagedClient>>,
    status_hub: Arc<RdpLaunchStatusHub>,
}

impl SystemRdpPlatformLauncher {
    /// 注入应用生命周期下的私有运行目录，避免 adapter 自行猜测持久化位置。
    pub fn new(runtime_directory: PathBuf) -> AppResult<Self> {
        let status_hub = Arc::new(RdpLaunchStatusHub::new(128, 64)?);
        Self::with_status_hub(runtime_directory, status_hub)
    }

    /// 与桌面注入的状态查询端口共享有界 launch 目录，避免 composition 拆成两份状态源。
    pub fn with_status_hub(
        runtime_directory: PathBuf,
        status_hub: Arc<RdpLaunchStatusHub>,
    ) -> AppResult<Self> {
        if runtime_directory.as_os_str().is_empty() {
            return Err(AppError::invalid("RDP 临时文件目录不能为空"));
        }
        Ok(Self {
            runtime_directory,
            lifetime: CancellationToken::new(),
            shutting_down: AtomicBool::new(false),
            clients: Mutex::new(HashMap::new()),
            status_hub,
        })
    }

    /// 提供给应用装配层注入 UI 的同一个 launch 状态服务实例。
    pub fn status_service(&self) -> Arc<dyn RdpLaunchStatusService> {
        self.status_hub.clone()
    }

    /// 以固定宽限期转入绝对 deadline 关闭，便于普通退出路径复用可重试实现。
    ///
    /// @author kongweiguang
    pub async fn shutdown(&self) -> AppResult<usize> {
        self.shutdown_until(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
    }

    /// 取消全部受管 child 并在同一截止时间内 join；超时保留未完成 handle 供后续重试。
    ///
    /// @author kongweiguang
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<usize> {
        self.shutting_down.store(true, Ordering::Release);
        let mut registry = tokio::time::timeout_at(deadline, self.clients.lock())
            .await
            .map_err(|_| AppError::timeout("RDP 关闭等待客户端 registry"))?;
        let count = registry.len();
        let client_ids = registry.keys().copied().collect::<Vec<_>>();
        for client in registry.values() {
            client.cleanup_deadline.send_replace(Some(deadline));
        }
        for client in registry.values() {
            client.cancellation.cancel();
        }
        self.lifetime.cancel();

        let mut failure = None;
        for id in client_ids {
            let Some(mut managed) = registry.remove(&id) else {
                continue;
            };
            match tokio::time::timeout_at(deadline, &mut managed.task).await {
                Err(_) => {
                    registry.insert(id, managed);
                    return Err(AppError::timeout("RDP 客户端退出与临时文件清理"));
                }
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => {
                    let _ = managed.status.publish(RdpLaunchStatus::Failed {
                        code: error.public_code(),
                    });
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
                Ok(Err(error)) => {
                    let failure_error = AppError::infrastructure_with_source(
                        "RDP 客户端 owner task 异常退出",
                        error,
                    );
                    let _ = managed.status.publish(RdpLaunchStatus::Failed {
                        code: failure_error.public_code(),
                    });
                    if failure.is_none() {
                        failure = Some(failure_error);
                    }
                }
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(count),
        }
    }
}

impl Drop for SystemRdpPlatformLauncher {
    /// 即使 composition root 忘记显式 shutdown，也先发取消信号供 owner 尽力回收。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        for client in self.clients.get_mut().values() {
            client.cleanup_deadline.send_replace(Some(deadline));
            client.cancellation.cancel();
        }
        self.lifetime.cancel();
    }
}

#[async_trait]
impl RdpPlatformLauncher for SystemRdpPlatformLauncher {
    /// 请求 token 只约束启动和 registry 登记；登记后客户端归应用生命周期所有。
    ///
    /// @author kongweiguang
    async fn launch(
        &self,
        target: RdpLaunchTarget,
        cancel: CancellationToken,
    ) -> AppResult<RdpLaunchReceipt> {
        artifact::validate_endpoint(&target.endpoint)?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("RDP 客户端启动已取消"));
        }
        if self.shutting_down.load(Ordering::Acquire) || self.lifetime.is_cancelled() {
            return Err(AppError::conflict("RDP 客户端管理器正在关闭"));
        }

        let lifecycle = current_platform_lifecycle()?;
        let (launch_id, reporter) = self.status_hub.register(lifecycle)?;
        let mut plan = match build_launch_plan(&self.runtime_directory, target).await {
            Ok(plan) => plan,
            Err(error) => return Err(publish_prelaunch_failure(&reporter, error)),
        };
        if cancel.is_cancelled()
            || self.lifetime.is_cancelled()
            || self.shutting_down.load(Ordering::Acquire)
        {
            let error = match plan.cleanup().await {
                Ok(()) => AppError::cancelled("RDP 客户端启动已取消"),
                Err(error) => error,
            };
            return Err(publish_prelaunch_failure(&reporter, error));
        }

        // registry 锁跨越 spawn 与登记，避免 shutdown 漏进程；等待锁期间仍可响应请求取消。
        let registry = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            _ = self.lifetime.cancelled() => None,
            registry = self.clients.lock() => Some(registry),
        };
        let Some(mut registry) = registry else {
            let error = match plan.cleanup().await {
                Ok(()) => AppError::cancelled("RDP 客户端启动已取消"),
                Err(error) => error,
            };
            return Err(publish_prelaunch_failure(&reporter, error));
        };
        if let Err(error) = reap_failed_clients(&mut registry).await {
            drop(registry);
            let error = match plan.cleanup().await {
                Ok(()) => error,
                Err(cleanup_error) => AppError::infrastructure_with_source(
                    "RDP 客户端回收失败且本次临时文件清理失败",
                    RdpShutdownFailure {
                        process: error,
                        cleanup: cleanup_error,
                    },
                ),
            };
            return Err(publish_prelaunch_failure(&reporter, error));
        }
        if cancel.is_cancelled() || self.shutting_down.load(Ordering::Acquire) {
            drop(registry);
            let error = match plan.cleanup().await {
                Ok(()) => AppError::cancelled("RDP 客户端启动已取消"),
                Err(error) => error,
            };
            return Err(publish_prelaunch_failure(&reporter, error));
        }

        let command = plan.command();
        let spawn_options: ProcessSpawnOptions = plan.spawn_options();
        let child = match ProcessTreeGuard::spawn_tokio(command, spawn_options) {
            Ok(child) => child,
            Err(error) => {
                drop(registry);
                let launch_error = if let Err(cleanup_error) = plan.cleanup().await {
                    AppError::infrastructure_with_source(
                        "RDP 客户端启动失败且临时文件清理失败",
                        cleanup_error,
                    )
                } else {
                    AppError::infrastructure_with_source("无法启动 RDP 客户端", error)
                };
                return Err(publish_prelaunch_failure(&reporter, launch_error));
            }
        };
        let endpoint = plan.endpoint.clone();
        let (process_id, lifecycle) = match plan.lifecycle {
            RdpClientLifecycle::ManagedProcess => (child.id(), RdpClientLifecycle::ManagedProcess),
            RdpClientLifecycle::DelegatedToSystem => (None, RdpClientLifecycle::DelegatedToSystem),
        };
        let id = launch_id.as_uuid();
        let owner_cancellation = self.lifetime.child_token();
        let (cleanup_deadline, cleanup_deadline_rx) = watch::channel(None);
        let task = tokio::spawn(supervise_client(
            child,
            plan.artifact.take(),
            owner_cancellation.clone(),
            cleanup_deadline_rx,
            lifecycle,
            reporter.clone(),
        ));
        registry.insert(
            id,
            ManagedClient {
                cancellation: owner_cancellation,
                cleanup_deadline,
                task,
                status: reporter,
            },
        );
        if cancel.is_cancelled()
            || self.lifetime.is_cancelled()
            || self.shutting_down.load(Ordering::Acquire)
        {
            if let Some(managed) = registry.get(&id) {
                managed
                    .cleanup_deadline
                    .send_replace(Some(tokio::time::Instant::now() + Duration::from_secs(5)));
                managed.cancellation.cancel();
            }
            drop(registry);
            return Err(AppError::cancelled("RDP 客户端启动已取消"));
        }
        drop(registry);

        // 回执区分本机受管进程与系统打开委派，不把本地请求结果当作远端登录证明。
        Ok(RdpLaunchReceipt {
            launch_id,
            endpoint,
            process_id,
            lifecycle,
        })
    }
}

/// 保留子进程和临时文件直到系统客户端退出或应用关闭。
struct ManagedClient {
    cancellation: CancellationToken,
    cleanup_deadline: watch::Sender<Option<tokio::time::Instant>>,
    task: JoinHandle<AppResult<()>>,
    status: RdpLaunchStatusReporter,
}

/// 监管平台 child，在退出或取消后回收子进程和临时文件；系统委派 app 不进入此 child 所有权。
///
/// @author kongweiguang
async fn supervise_client(
    mut child: ManagedTokioChild,
    artifact: Option<TemporaryRdpArtifact>,
    cancellation: CancellationToken,
    cleanup_deadline: watch::Receiver<Option<tokio::time::Instant>>,
    lifecycle: RdpClientLifecycle,
    reporter: RdpLaunchStatusReporter,
) -> AppResult<()> {
    let process_result = match wait_or_terminate_child(&mut child, &cancellation, cleanup_deadline)
        .await
    {
        Ok(ChildCompletion::Exited) => Ok(()),
        Ok(ChildCompletion::Cancelled) if lifecycle == RdpClientLifecycle::DelegatedToSystem => {
            Err(AppError::cancelled("RDP 系统打开请求已取消"))
        }
        Ok(ChildCompletion::Cancelled) => Ok(()),
        Err(error) => Err(error),
    };
    let cleanup_result = match artifact {
        Some(artifact) => artifact.cleanup().await,
        None => Ok(()),
    };
    let result = match (process_result, cleanup_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
        (Err(process), Err(cleanup)) => Err(AppError::infrastructure_with_source(
            "RDP 客户端进程回收与临时文件清理均失败",
            RdpShutdownFailure { process, cleanup },
        )),
    };
    let status = match &result {
        Ok(()) => match lifecycle {
            RdpClientLifecycle::ManagedProcess => RdpLaunchStatus::Closed,
            RdpClientLifecycle::DelegatedToSystem => RdpLaunchStatus::DelegatedToSystem,
        },
        Err(error) => RdpLaunchStatus::Failed {
            code: error.public_code(),
        },
    };
    match (result, reporter.publish(status)) {
        (Ok(()), Ok(_)) => Ok(()),
        (Err(error), Ok(_)) => Err(error),
        (Ok(()), Err(status_error)) => Err(status_error),
        (Err(error), Err(status_error)) => Err(error.with_source(status_error)),
    }
}

/// 在应用退出时终止本 adapter 启动的 child，并等待内核确认进程回收。
///
/// @author kongweiguang
async fn wait_or_terminate_child(
    child: &mut ManagedTokioChild,
    cancellation: &CancellationToken,
    mut cleanup_deadline: watch::Receiver<Option<tokio::time::Instant>>,
) -> AppResult<ChildCompletion> {
    loop {
        let completed = tokio::select! {
            biased;
            result = child.wait() => Some(result),
            _ = cancellation.cancelled() => None,
        };
        match completed {
            Some(Ok(status)) => return child_exit_result(status.success()),
            Some(Err(_wait_error)) => {
                if let Ok(Some(status)) = child.try_wait() {
                    return child_exit_result(status.success());
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => {
                        return terminate_and_reap_child(child, &mut cleanup_deadline).await;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                }
            }
            None => return terminate_and_reap_child(child, &mut cleanup_deadline).await,
        }
    }
}

/// 将已经由 wait/try_wait 确认的直接客户端退出映射为状态，失败码不包含路径或参数。
///
/// @author kongweiguang
fn child_exit_result(success: bool) -> AppResult<ChildCompletion> {
    if success {
        Ok(ChildCompletion::Exited)
    } else {
        Err(AppError::infrastructure("RDP 客户端已退出并报告失败"))
    }
}

/// 在每次 shutdown 提供的绝对 deadline 内终止并回收进程树；超时保留 child 供新 deadline 重试。
///
/// @author kongweiguang
async fn terminate_and_reap_child(
    child: &mut ManagedTokioChild,
    cleanup_deadline: &mut watch::Receiver<Option<tokio::time::Instant>>,
) -> AppResult<ChildCompletion> {
    let mut deadline_sender_open = cleanup_deadline.has_changed().is_ok();
    let mut fallback_deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    loop {
        let published_deadline = *cleanup_deadline.borrow_and_update();
        let deadline = if deadline_sender_open {
            published_deadline.unwrap_or(fallback_deadline)
        } else {
            fallback_deadline
        };

        let terminate_result = if deadline_sender_open {
            tokio::select! {
                biased;
                result = child.terminate_tree_until(deadline) => Some(result),
                changed = cleanup_deadline.changed() => {
                    if changed.is_err() {
                        deadline_sender_open = false;
                    }
                    None
                }
            }
        } else {
            Some(child.terminate_tree_until(deadline).await)
        };
        let Some(_terminate_result) = terminate_result else {
            continue;
        };

        let wait_result = if deadline_sender_open {
            tokio::select! {
                biased;
                result = tokio::time::timeout_at(deadline, child.wait()) => Some(result),
                changed = cleanup_deadline.changed() => {
                    if changed.is_err() {
                        deadline_sender_open = false;
                    }
                    None
                }
            }
        } else {
            Some(tokio::time::timeout_at(deadline, child.wait()).await)
        };
        match wait_result {
            None => continue,
            Some(Ok(Ok(_status))) => return Ok(ChildCompletion::Cancelled),
            Some(Ok(Err(_wait_error))) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return Ok(ChildCompletion::Cancelled);
                }
                retry_backoff_or_deadline_update(cleanup_deadline, &mut deadline_sender_open).await;
            }
            Some(Err(_elapsed)) => {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    return Ok(ChildCompletion::Cancelled);
                }
            }
        }

        // 截止时间已过时等待自然退出或下一次 shutdown 的新 deadline，不持有旧 wait future。
        let wait_or_update = if deadline_sender_open {
            tokio::select! {
                biased;
                result = child.wait() => Some(result),
                changed = cleanup_deadline.changed() => {
                    if changed.is_err() {
                        deadline_sender_open = false;
                    }
                    None
                }
            }
        } else {
            tokio::select! {
                biased;
                result = child.wait() => Some(result),
                _ = tokio::time::sleep(Duration::from_millis(250)) => None,
            }
        };
        match wait_or_update {
            Some(Ok(_status)) => return Ok(ChildCompletion::Cancelled),
            Some(Err(_wait_error)) => {
                fallback_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                retry_backoff_or_deadline_update(cleanup_deadline, &mut deadline_sender_open).await;
            }
            None => {
                fallback_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            }
        }
    }
}

/// 等待下一次轮询或立即接收更宽的新 deadline，避免短时间系统 wait 错误形成忙循环。
///
/// @author kongweiguang
async fn retry_backoff_or_deadline_update(
    cleanup_deadline: &mut watch::Receiver<Option<tokio::time::Instant>>,
    deadline_sender_open: &mut bool,
) {
    if *deadline_sender_open {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(25)) => {}
            changed = cleanup_deadline.changed() => {
                if changed.is_err() {
                    *deadline_sender_open = false;
                }
            }
        }
    } else {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// 区分客户端自行退出与 adapter 取消 broker，避免系统委派被错误记成已打开。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ChildCompletion {
    Exited,
    Cancelled,
}

/// 把双重回收错误保留在内部 source 链，同时向 UI 返回不含路径或凭据的固定文案。
#[derive(Debug)]
struct RdpShutdownFailure {
    process: AppError,
    cleanup: AppError,
}

impl std::fmt::Display for RdpShutdownFailure {
    /// 错误源文案保持固定，不展开平台命令行或临时文件路径。
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "process cleanup error: {}; artifact cleanup error: {}",
            self.process.public_message(),
            self.cleanup.public_message()
        )
    }
}

impl std::error::Error for RdpShutdownFailure {}

/// 删除已结束但回收失败的 owner 条目，确保故障不会被后续启动静默忽略。
async fn reap_failed_clients(registry: &mut HashMap<Uuid, ManagedClient>) -> AppResult<()> {
    let finished_ids = registry
        .iter()
        .filter_map(|(id, client)| client.task.is_finished().then_some(*id))
        .collect::<Vec<_>>();
    for id in finished_ids {
        let Some(client) = registry.remove(&id) else {
            continue;
        };
        let ManagedClient { task, status, .. } = client;
        match task.await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                let _ = status.publish(RdpLaunchStatus::Failed {
                    code: error.public_code(),
                });
                return Err(error);
            }
            Err(error) => {
                let failure =
                    AppError::infrastructure_with_source("RDP 客户端 owner task 异常退出", error);
                let _ = status.publish(RdpLaunchStatus::Failed {
                    code: failure.public_code(),
                });
                return Err(failure);
            }
        }
    }
    Ok(())
}

/// 按当前平台的真实所有权模型选择终态语义，不把 LaunchServices broker 当作 RDP 客户端。
fn current_platform_lifecycle() -> AppResult<RdpClientLifecycle> {
    if cfg!(any(target_os = "windows", target_os = "linux")) {
        Ok(RdpClientLifecycle::ManagedProcess)
    } else if cfg!(target_os = "macos") {
        Ok(RdpClientLifecycle::DelegatedToSystem)
    } else {
        Err(AppError::unsupported("当前平台没有可用的 RDP 客户端适配器"))
    }
}

/// 让所有发生在 spawn 前的错误也进入 launch 的终态，UI watcher 无需等待下一次启动才发现。
fn publish_prelaunch_failure(reporter: &RdpLaunchStatusReporter, error: AppError) -> AppError {
    match reporter.publish(RdpLaunchStatus::Failed {
        code: error.public_code(),
    }) {
        Ok(_) => error,
        Err(status_error) => error.with_source(status_error),
    }
}

#[cfg(test)]
#[path = "../../tests/support/rdp_tests.rs"]
mod tests;
