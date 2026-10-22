//! MCP runtime start/stop state transitions over the single retained listener owner.
//!
//! @author kongweiguang

use std::sync::atomic::Ordering;

use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    mcp_runtime::{McpRuntimeControl, McpRuntimeSnapshot},
};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::McpServer;

use super::{
    ActiveShutdown, McpRuntimeController, McpRuntimeSnapshot as Snapshot, McpRuntimeState as State,
    RuntimeOwner, request_runtime_shutdown, with_running_publication_gate,
};

#[async_trait::async_trait]
impl McpRuntimeControl for McpRuntimeController {
    /// 立即读取最近发布状态，不等待控制门或持久 workspace 同步。
    fn snapshot(&self) -> McpRuntimeSnapshot {
        match self.snapshot.read() {
            Ok(snapshot) => snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    /// 启动唯一 listener 并同步 Agent workspace endpoint；失败保留 listener 及重试 owner。
    async fn start(&self, cancellation: CancellationToken) -> AppResult<McpRuntimeSnapshot> {
        self.ensure_available()?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("MCP start was cancelled"));
        }
        let _gate = self.acquire_start_gate(&cancellation).await?;
        self.ensure_available()?;
        if cancellation.is_cancelled() {
            return Err(AppError::cancelled("MCP start was cancelled"));
        }
        let mut owner = self.owner.lock().await;

        if owner.server.is_none()
            && owner
                .pending_sync
                .as_ref()
                .is_some_and(|pending| pending.desired_endpoint.is_none())
        {
            drop(owner);
            if let Err(error) = self
                .progress_endpoint_sync(None, Some(cancellation.clone()), None)
                .await
            {
                self.publish(Self::snapshot_with_failure(
                    State::Stopped,
                    None,
                    true,
                    &error,
                ));
                drop(_gate);
                if error.public_code() != PublicErrorCode::Cancelled {
                    self.notify_start_failure(&error).await;
                }
                return Err(error);
            }
            owner = self.owner.lock().await;
        }

        if let Some(server) = owner.server.as_ref()
            && server.shutdown_signal().is_cancelled()
        {
            let endpoint = server.endpoint().to_owned();
            self.publish(Snapshot {
                state: State::Stopping,
                endpoint: Some(endpoint),
                client_json: None,
                endpoint_sync_pending: owner.pending_sync.is_some(),
                unavailable_reason: None,
                failure_reason: Some("MCP listener shutdown is pending; retry stop".to_owned()),
            });
            return Err(AppError::conflict(
                "MCP listener shutdown is pending; retry stop",
            ));
        }

        if let Some(endpoint) = Self::current_endpoint(&owner)
            && owner.pending_sync.is_none()
        {
            let identity = owner.server_identity.as_ref().ok_or_else(|| {
                AppError::infrastructure("MCP listener owner identity is missing")
            })?;
            return self.publish_running_if_current(&owner, identity, endpoint);
        }

        if owner.server.is_none() {
            self.publish(Snapshot {
                state: State::Starting,
                endpoint: None,
                client_json: None,
                endpoint_sync_pending: false,
                unavailable_reason: None,
                failure_reason: None,
            });
            let server =
                match McpServer::start(&self.runtime, self.services.clone(), self.config).await {
                    Ok(server) => server,
                    Err(error) => {
                        self.publish(Self::snapshot_with_failure(
                            State::Failed,
                            None,
                            false,
                            &error,
                        ));
                        drop(owner);
                        drop(_gate);
                        self.notify_start_failure(&error).await;
                        return Err(error);
                    }
                };
            let endpoint = server.endpoint().to_owned();
            let shutdown_signal = server.shutdown_signal();
            let server_identity = std::sync::Arc::new(());
            owner.server = Some(server);
            owner.server_identity = Some(server_identity.clone());
            owner.pending_sync = Some(super::synchronization::PendingEndpointSync {
                desired_endpoint: Some(endpoint.clone()),
                task_endpoint: None,
                cancellation: None,
                task: None,
            });
            let mut active_shutdown = self
                .active_shutdown
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *active_shutdown = Some(ActiveShutdown {
                identity: server_identity,
                cancellation: shutdown_signal.clone(),
            });
            if self.shutting_down.load(Ordering::Acquire) {
                shutdown_signal.cancel();
            }
            drop(active_shutdown);
            if self.shutting_down.load(Ordering::Acquire) {
                if let Some(shutdown) = owner.server.as_ref() {
                    shutdown.begin_shutdown();
                }
                self.publish(Snapshot {
                    state: State::Stopping,
                    endpoint: Some(endpoint),
                    client_json: None,
                    endpoint_sync_pending: true,
                    unavailable_reason: None,
                    failure_reason: Some("MCP runtime is shutting down".to_owned()),
                });
                return Err(AppError::cancelled("MCP runtime is shutting down"));
            }
        }

        let endpoint = Self::current_endpoint(&owner)
            .ok_or_else(|| AppError::infrastructure("MCP listener owner disappeared"))?;
        let identity = owner
            .server_identity
            .clone()
            .ok_or_else(|| AppError::infrastructure("MCP listener owner identity is missing"))?;
        self.publish(Snapshot {
            state: State::Synchronizing,
            endpoint: Some(endpoint.clone()),
            client_json: None,
            endpoint_sync_pending: true,
            unavailable_reason: None,
            failure_reason: None,
        });
        drop(owner);
        if let Err(error) = self
            .progress_endpoint_sync(Some(endpoint.clone()), Some(cancellation), None)
            .await
        {
            self.publish(Self::snapshot_with_failure(
                State::Synchronizing,
                Some(endpoint),
                true,
                &error,
            ));
            drop(_gate);
            if error.public_code() != PublicErrorCode::Cancelled {
                self.notify_start_failure(&error).await;
            }
            return Err(error);
        }

        let owner = self.owner.lock().await;
        self.publish_running_if_current(&owner, &identity, endpoint)
    }

    /// 停止 listener 后清理活动 Agent endpoint；超时仍由 controller 持有句柄供再次尝试。
    async fn stop_until(&self, deadline: Instant) -> AppResult<McpRuntimeSnapshot> {
        self.stop_inner(deadline).await
    }

    /// 同步关闭 intake 并向当前 listener/启动中的 Agent context 同步发取消信号。
    fn begin_shutdown(&self) {
        request_runtime_shutdown(&self.active_shutdown, &self.shutting_down);
        let current = self.snapshot();
        if matches!(
            current.state,
            State::Starting | State::Synchronizing | State::Running
        ) {
            self.publish(Snapshot {
                state: State::Stopping,
                endpoint: current.endpoint,
                client_json: None,
                endpoint_sync_pending: current.endpoint_sync_pending,
                unavailable_reason: current.unavailable_reason,
                failure_reason: Some("MCP runtime is shutting down".to_owned()),
            });
        }
    }

    /// 用桌面统一 deadline 停止 runtime；失败时保留 handle 与 endpoint sync task。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        self.begin_shutdown();
        self.stop_inner(deadline).await.map(|_| ())
    }
}

impl McpRuntimeController {
    /// 仅在当前 listener、endpoint 同步与 shutdown token 仍属于同一代 owner 时发布 Running。
    fn publish_running_if_current(
        &self,
        owner: &RuntimeOwner,
        expected_identity: &std::sync::Arc<()>,
        endpoint: String,
    ) -> AppResult<Snapshot> {
        let owner_matches = owner.server.as_ref().is_some_and(|server| {
            server.endpoint() == endpoint
                && owner
                    .server_identity
                    .as_ref()
                    .is_some_and(|identity| std::sync::Arc::ptr_eq(identity, expected_identity))
        });
        if !owner_matches || owner.pending_sync.is_some() {
            let shutdown_requested = self.shutting_down.load(Ordering::Acquire)
                || owner
                    .server
                    .as_ref()
                    .is_some_and(|server| server.shutdown_signal().is_cancelled());
            let state = if shutdown_requested {
                State::Stopping
            } else if owner.server.is_some() {
                State::Synchronizing
            } else {
                State::Stopped
            };
            self.publish(Snapshot {
                state,
                endpoint: Self::current_endpoint(owner),
                client_json: None,
                endpoint_sync_pending: owner.pending_sync.is_some(),
                unavailable_reason: None,
                failure_reason: Some(if shutdown_requested {
                    "MCP runtime is shutting down".to_owned()
                } else {
                    "MCP listener owner changed before startup completed".to_owned()
                }),
            });
            return Err(if shutdown_requested {
                AppError::cancelled("MCP runtime is shutting down")
            } else {
                AppError::conflict("MCP listener owner changed before startup completed")
            });
        }

        let running = self.running_snapshot(endpoint.clone());
        let stopping = Snapshot {
            state: State::Stopping,
            endpoint: Some(endpoint),
            client_json: None,
            endpoint_sync_pending: false,
            unavailable_reason: None,
            failure_reason: Some("MCP runtime is shutting down".to_owned()),
        };
        let mut published = false;
        let mut rejected = false;
        with_running_publication_gate(
            &self.active_shutdown,
            &self.shutting_down,
            expected_identity,
            || {
                self.publish(running.clone());
                published = true;
            },
            || {
                self.publish(stopping);
                rejected = true;
            },
        );
        if rejected {
            return Err(AppError::cancelled("MCP runtime is shutting down"));
        }
        if !published {
            return Err(AppError::infrastructure(
                "MCP running state publication did not complete",
            ));
        }
        Ok(running)
    }
}
