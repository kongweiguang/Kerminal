//! Desktop MCP runtime 的唯一 listener owner 与 Agent endpoint 同步适配。
//!
//! @author kongweiguang

use std::sync::{
    Arc, Mutex as StdMutex, RwLock as StdRwLock,
    atomic::{AtomicBool, Ordering},
};

mod lifecycle;
mod synchronization;

use kerminal_application::{
    AppError, AppResult,
    agents::AgentService,
    mcp_runtime::{McpRuntimeControl, McpRuntimeSnapshot, McpRuntimeState},
    platform::{
        DesktopNotificationEvent, DesktopNotificationService, NotificationImportance,
        WindowVisibility,
    },
};
use tokio::{
    runtime::Handle,
    sync::{Mutex, MutexGuard},
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use crate::{McpServerConfig, McpServerHandle, McpServices};
use synchronization::PendingEndpointSync;

/// listener 与尚未完成的 Agent context 同步都由此 owner 持有，不能因界面取消而脱离跟踪。
#[derive(Default)]
struct RuntimeOwner {
    server: Option<McpServerHandle>,
    /// 标识一次 listener 所有权，避免异步同步完成后把旧启动结果发布到新 owner。
    server_identity: Option<Arc<()>>,
    pending_sync: Option<PendingEndpointSync>,
}

/// 将 shutdown token 与创建它的 listener identity 绑定，防止旧代 token 冒充当前 owner。
pub(super) struct ActiveShutdown {
    pub(super) identity: Arc<()>,
    pub(super) cancellation: CancellationToken,
}

/// Desktop MCP 的 application port adapter；不构造第二组服务或 session registry。
pub struct McpRuntimeController {
    runtime: Handle,
    services: McpServices,
    config: McpServerConfig,
    agent_service: Option<Arc<dyn AgentService>>,
    notifications: DesktopNotificationService,
    unavailable_reason: Option<String>,
    owner: Mutex<RuntimeOwner>,
    operation_gate: Mutex<()>,
    snapshot: StdRwLock<McpRuntimeSnapshot>,
    active_shutdown: StdMutex<Option<ActiveShutdown>>,
    shutting_down: AtomicBool,
}

impl McpRuntimeController {
    /// 复用桌面已装配的服务和 Tokio runtime；服务缺失只让 MCP 显示不可用，不阻断主窗口。
    pub fn new(
        runtime: Handle,
        services: McpServices,
        config: McpServerConfig,
        notifications: DesktopNotificationService,
    ) -> Self {
        let unavailable_reason = services
            .validate_complete()
            .err()
            .map(|error| error.to_string());
        let initial = unavailable_reason
            .as_deref()
            .map(McpRuntimeSnapshot::unavailable)
            .unwrap_or_else(McpRuntimeSnapshot::stopped);
        Self {
            runtime,
            agent_service: services.agents.clone(),
            services,
            config,
            notifications,
            unavailable_reason,
            owner: Mutex::new(RuntimeOwner::default()),
            operation_gate: Mutex::new(()),
            snapshot: StdRwLock::new(initial),
            active_shutdown: StdMutex::new(None),
            shutting_down: AtomicBool::new(false),
        }
    }

    /// 以非阻塞锁发布状态快照，UI 可在 listener join 或 workspace 写入期间持续刷新。
    fn publish(&self, snapshot: McpRuntimeSnapshot) {
        match self.snapshot.write() {
            Ok(mut current) => *current = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    /// 只把 application 的稳定公开错误文案放进状态，不泄露 source 或路径。
    fn snapshot_with_failure(
        state: McpRuntimeState,
        endpoint: Option<String>,
        pending: bool,
        error: &AppError,
    ) -> McpRuntimeSnapshot {
        McpRuntimeSnapshot {
            state,
            endpoint,
            client_json: None,
            endpoint_sync_pending: pending,
            unavailable_reason: None,
            failure_reason: Some(error.public_message().to_owned()),
        }
    }

    /// 生成旧桌面设置页使用的 MCP client 配置，地址必须来自实际绑定的 handle。
    fn client_json(endpoint: &str) -> String {
        serde_json::json!({
            "mcpServers": {
                "kerminal": {
                    "type": "http",
                    "url": endpoint
                }
            }
        })
        .to_string()
    }

    /// 尝试向桌面通知策略发送真实 bind/start 失败事件；通知失败不覆盖 MCP 原始错误。
    async fn notify_start_failure(&self, error: &AppError) {
        let visibility = self
            .notifications
            .window_visibility()
            .unwrap_or(WindowVisibility::Foreground);
        if let Err(notification_error) = self
            .notifications
            .notify(
                DesktopNotificationEvent::McpServerFailed {
                    port: Some(self.config.bind_address().port()),
                    reason: Some(error.public_message().to_owned()),
                    importance: Some(NotificationImportance::High),
                },
                visibility,
            )
            .await
        {
            tracing::debug!(
                code = notification_error.public_code().as_str(),
                "MCP start failure notification could not be submitted"
            );
        }
    }

    /// 拒绝缺少生产工具族的启动请求，避免 advertize 全目录但部分服务未装配。
    fn ensure_available(&self) -> AppResult<()> {
        if self.unavailable_reason.is_some() {
            Err(AppError::unsupported(
                "MCP runtime services are unavailable",
            ))
        } else if self.shutting_down.load(Ordering::Acquire) {
            Err(AppError::cancelled("MCP runtime is shutting down"))
        } else {
            Ok(())
        }
    }

    /// 获取串行控制门；仅在等待门期间响应取消，取得 owner 后操作会自行完成或保留重试句柄。
    async fn acquire_start_gate(
        &self,
        cancellation: &CancellationToken,
    ) -> AppResult<MutexGuard<'_, ()>> {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(AppError::cancelled("MCP start was cancelled")),
            guard = self.operation_gate.lock() => Ok(guard),
        }
    }

    /// 等待控制门直到桌面共享 deadline，超时只中止等待，不丢现存 listener owner。
    async fn acquire_stop_gate(&self, deadline: Instant) -> AppResult<MutexGuard<'_, ()>> {
        timeout_at(deadline, self.operation_gate.lock())
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source(
                    "MCP runtime operation deadline elapsed",
                    error,
                )
            })
    }

    /// 计算真实状态快照；只有 listener 存在且所有 Agent context 已同步时才开放 client JSON。
    fn running_snapshot(&self, endpoint: String) -> McpRuntimeSnapshot {
        McpRuntimeSnapshot {
            client_json: Some(Self::client_json(&endpoint)),
            state: McpRuntimeState::Running,
            endpoint: Some(endpoint),
            endpoint_sync_pending: false,
            unavailable_reason: None,
            failure_reason: None,
        }
    }

    /// 生成状态中的 endpoint 与绑定 handle 一致，不从设置或请求方推算。
    fn current_endpoint(owner: &RuntimeOwner) -> Option<String> {
        owner
            .server
            .as_ref()
            .map(|server| server.endpoint().to_owned())
    }

    /// 用同一 owner 完成 stop 和 endpoint 清理；超时或 CAS 冲突保留原始同步任务供重试。
    async fn stop_inner(&self, deadline: Instant) -> AppResult<McpRuntimeSnapshot> {
        let _gate = self.acquire_stop_gate(deadline).await?;
        let mut owner = timeout_at(deadline, self.owner.lock())
            .await
            .map_err(|error| {
                AppError::infrastructure_with_source("MCP runtime owner deadline elapsed", error)
            })?;
        let had_server = owner.server.is_some();

        if !had_server && owner.pending_sync.is_none() {
            let current = self.snapshot();
            if current.state == McpRuntimeState::Starting {
                self.publish(McpRuntimeSnapshot::stopped());
                return Ok(self.snapshot());
            }
            return Ok(current);
        }

        let endpoint_sync_pending = owner.pending_sync.is_some();
        if let Some(server) = owner.server.as_mut() {
            let endpoint = server.endpoint().to_owned();
            self.publish(McpRuntimeSnapshot {
                state: McpRuntimeState::Stopping,
                endpoint: Some(endpoint.clone()),
                client_json: None,
                endpoint_sync_pending,
                unavailable_reason: None,
                failure_reason: None,
            });
            server.begin_shutdown();
            if let Err(error) = server.shutdown_until(deadline).await {
                let snapshot = Self::snapshot_with_failure(
                    McpRuntimeState::Stopping,
                    Some(endpoint),
                    endpoint_sync_pending,
                    &error,
                );
                self.publish(snapshot);
                return Err(error);
            }
            owner.server = None;
            owner.server_identity = None;
            let mut shutdown = self
                .active_shutdown
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *shutdown = None;
        }

        let should_sync_clear = owner.pending_sync.is_some() || had_server;
        self.publish(McpRuntimeSnapshot {
            state: McpRuntimeState::Stopped,
            endpoint: None,
            client_json: None,
            endpoint_sync_pending: should_sync_clear,
            unavailable_reason: None,
            failure_reason: None,
        });
        if !should_sync_clear {
            return Ok(self.snapshot());
        }

        // 停止后的 None 写只由本次 stop owner 驱动；不受桌面 shutdown token 再次取消。
        drop(owner);
        let sync_result = self
            .progress_endpoint_sync(None, None, Some(deadline))
            .await;
        match sync_result {
            Ok(()) => {
                self.publish(McpRuntimeSnapshot::stopped());
                Ok(self.snapshot())
            }
            Err(error) => {
                let snapshot =
                    Self::snapshot_with_failure(McpRuntimeState::Stopped, None, true, &error);
                self.publish(snapshot);
                Err(error)
            }
        }
    }
}

/// 在同一把短时同步锁下序列化关闭请求与 Running 快照发布。
///
/// @author kongweiguang
pub(super) fn with_running_publication_gate(
    active_shutdown: &StdMutex<Option<ActiveShutdown>>,
    shutting_down: &AtomicBool,
    expected_identity: &Arc<()>,
    publish_running: impl FnOnce(),
    publish_stopping: impl FnOnce(),
) -> bool {
    let active_shutdown = active_shutdown
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let may_publish = !shutting_down.load(Ordering::Acquire)
        && active_shutdown.as_ref().is_some_and(|active| {
            Arc::ptr_eq(&active.identity, expected_identity) && !active.cancellation.is_cancelled()
        });
    if may_publish {
        publish_running();
    } else {
        publish_stopping();
    }
    may_publish
}

/// 让 listener 取消和 shutdown 标记在 Running 发布门内线性化。
///
/// @author kongweiguang
pub(super) fn request_runtime_shutdown(
    active_shutdown: &StdMutex<Option<ActiveShutdown>>,
    shutting_down: &AtomicBool,
) {
    let active_shutdown = active_shutdown
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    shutting_down.store(true, Ordering::Release);
    if let Some(active) = active_shutdown.as_ref() {
        active.cancellation.cancel();
    }
}
