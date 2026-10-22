//! 保留 Agent endpoint 更新任务句柄，确保取消与重试不脱离资源 owner。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::{AppError, AppResult, agents::AgentService};
use tokio::{
    sync::Mutex,
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

use super::McpRuntimeController;

/// 共享可重入等待的后台任务槽，超时后仍由同一 owner 持有以供再次等待。
///
/// @author kongweiguang
pub(super) type PendingEndpointSyncTask = Arc<Mutex<Option<JoinHandle<AppResult<()>>>>>;

/// Agent context 更新任务的期望目标和 JoinHandle；deadline 到达后保留以便再次等待。
///
/// @author kongweiguang
pub(super) struct PendingEndpointSync {
    pub(super) desired_endpoint: Option<String>,
    pub(super) task_endpoint: Option<Option<String>>,
    pub(super) cancellation: Option<CancellationToken>,
    pub(super) task: Option<PendingEndpointSyncTask>,
}

/// 等待 Agent endpoint 更新的结果，超时只终止等待，不丢失底层任务。
pub(super) enum SyncTaskOutcome {
    Completed(AppResult<()>),
    DeadlineElapsed,
}

impl McpRuntimeController {
    /// 同步 Agent endpoint 时不持有 listener owner 锁跨越应用用例调用。
    ///
    /// @author kongweiguang
    pub(super) async fn progress_endpoint_sync(
        &self,
        desired_endpoint: Option<String>,
        request_cancellation: Option<CancellationToken>,
        deadline: Option<Instant>,
    ) -> AppResult<()> {
        let Some(agent_service) = self.agent_service.as_ref().cloned() else {
            return Err(AppError::unsupported(
                "Agent endpoint synchronization is unavailable",
            ));
        };

        let (task_owner, task_endpoint, cancellation) = {
            let mut owner = self.owner.lock().await;
            let pending = owner
                .pending_sync
                .get_or_insert_with(|| PendingEndpointSync {
                    desired_endpoint: desired_endpoint.clone(),
                    task_endpoint: None,
                    cancellation: None,
                    task: None,
                });
            pending.desired_endpoint = desired_endpoint.clone();
            (
                pending.task.clone(),
                pending.task_endpoint.clone(),
                pending.cancellation.clone(),
            )
        };

        if let Some(task_owner) = task_owner.as_ref()
            && task_endpoint.as_ref() != Some(&desired_endpoint)
        {
            if let Some(cancellation) = cancellation.as_ref() {
                cancellation.cancel();
            }
            match await_sync_task(task_owner, deadline).await {
                SyncTaskOutcome::Completed(_) => {
                    let mut owner = self.owner.lock().await;
                    if let Some(pending) = owner.pending_sync.as_mut()
                        && pending
                            .task
                            .as_ref()
                            .is_some_and(|current| Arc::ptr_eq(current, task_owner))
                    {
                        pending.task = None;
                        pending.task_endpoint = None;
                        pending.cancellation = None;
                    }
                }
                SyncTaskOutcome::DeadlineElapsed => {
                    return Err(AppError::timeout("MCP endpoint synchronization"));
                }
            }
        }

        let (task_owner, task_endpoint) = {
            let mut owner = self.owner.lock().await;
            let pending = owner
                .pending_sync
                .get_or_insert_with(|| PendingEndpointSync {
                    desired_endpoint: desired_endpoint.clone(),
                    task_endpoint: None,
                    cancellation: None,
                    task: None,
                });
            pending.desired_endpoint = desired_endpoint.clone();
            if pending.task.is_none() {
                let task_cancellation = CancellationToken::new();
                let task_token = task_cancellation.clone();
                let request_token = request_cancellation.clone();
                let shutdown_token = self.active_controller_cancellation().unwrap_or_default();
                let endpoint = desired_endpoint.clone();
                let cancel_on_shutdown = endpoint.is_some();
                let task_owner = Arc::new(Mutex::new(None));
                let task_slot = Arc::clone(&task_owner);
                let agent_service = Arc::clone(&agent_service);
                let task = self.runtime.spawn(async move {
                    run_endpoint_sync(
                        agent_service,
                        endpoint,
                        task_token,
                        request_token,
                        shutdown_token,
                        cancel_on_shutdown,
                    )
                    .await
                });
                *task_owner.lock().await = Some(task);
                pending.task_endpoint = Some(desired_endpoint.clone());
                pending.cancellation = Some(task_cancellation);
                pending.task = Some(task_slot);
            }
            (pending.task.clone(), pending.task_endpoint.clone())
        };

        let Some(task_owner) = task_owner else {
            return Err(AppError::infrastructure(
                "MCP endpoint sync task is missing",
            ));
        };
        if task_endpoint.as_ref() != Some(&desired_endpoint) {
            return Err(AppError::conflict(
                "MCP endpoint synchronization target changed",
            ));
        }

        match await_sync_task(&task_owner, deadline).await {
            SyncTaskOutcome::Completed(Ok(())) => {
                let mut owner = self.owner.lock().await;
                if owner.pending_sync.as_ref().is_some_and(|pending| {
                    pending
                        .task
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &task_owner))
                }) {
                    owner.pending_sync = None;
                }
                Ok(())
            }
            SyncTaskOutcome::Completed(Err(error)) => {
                self.clear_completed_sync_task(&task_owner).await;
                Err(error)
            }
            SyncTaskOutcome::DeadlineElapsed => {
                Err(AppError::timeout("MCP endpoint synchronization"))
            }
        }
    }

    /// 任务已结束时移除对应句柄，但保留期望 endpoint 以便后续 CAS 重试。
    async fn clear_completed_sync_task(
        &self,
        task_owner: &Arc<Mutex<Option<JoinHandle<AppResult<()>>>>>,
    ) {
        let mut owner = self.owner.lock().await;
        if let Some(pending) = owner.pending_sync.as_mut()
            && pending
                .task
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, task_owner))
        {
            pending.task = None;
            pending.task_endpoint = None;
            pending.cancellation = None;
        }
    }

    /// 复制 controller shutdown token，避免在异步工作期间持有标准锁。
    fn active_controller_cancellation(&self) -> Option<CancellationToken> {
        self.active_shutdown
            .lock()
            .ok()
            .and_then(|active| active.as_ref().map(|owner| owner.cancellation.clone()))
    }
}

/// 以可变 JoinHandle 等待；超时后把句柄留在共享 owner 中供调用方重试。
async fn await_sync_task(
    task_owner: &Arc<Mutex<Option<JoinHandle<AppResult<()>>>>>,
    deadline: Option<Instant>,
) -> SyncTaskOutcome {
    let mut task_owner = task_owner.lock().await;
    let Some(task) = task_owner.as_mut() else {
        return SyncTaskOutcome::Completed(Err(AppError::infrastructure(
            "MCP endpoint sync task is missing",
        )));
    };
    let outcome = match deadline {
        Some(deadline) => match timeout_at(deadline, &mut *task).await {
            Ok(result) => SyncTaskOutcome::Completed(join_result(result)),
            Err(_) => SyncTaskOutcome::DeadlineElapsed,
        },
        None => SyncTaskOutcome::Completed(join_result((&mut *task).await)),
    };
    if matches!(&outcome, SyncTaskOutcome::Completed(_)) {
        task_owner.take();
    }
    outcome
}

/// 把 endpoint worker panic 转为安全 application error 并保留诊断 source。
fn join_result(result: Result<AppResult<()>, tokio::task::JoinError>) -> AppResult<()> {
    result.map_err(|error| {
        AppError::infrastructure_with_source("Agent endpoint synchronization task failed", error)
    })?
}

/// 收到取消后通知 Agent 用例协作退出，并等待它保留或提交实际 workspace 状态。
async fn run_endpoint_sync(
    agent_service: Arc<dyn AgentService>,
    endpoint: Option<String>,
    task_token: CancellationToken,
    request_token: Option<CancellationToken>,
    shutdown_token: CancellationToken,
    cancel_on_shutdown: bool,
) -> AppResult<()> {
    let mut operation =
        Box::pin(agent_service.configure_mcp_endpoint(endpoint, task_token.clone()));
    match (request_token, cancel_on_shutdown) {
        (Some(request), true) => tokio::select! {
            biased;
            result = operation.as_mut() => result,
            _ = request.cancelled() => {
                task_token.cancel();
                operation.await
            }
            _ = shutdown_token.cancelled() => {
                task_token.cancel();
                operation.await
            }
        },
        (Some(request), false) => tokio::select! {
            biased;
            result = operation.as_mut() => result,
            _ = request.cancelled() => {
                task_token.cancel();
                operation.await
            }
        },
        (None, true) => tokio::select! {
            biased;
            result = operation.as_mut() => result,
            _ = shutdown_token.cancelled() => {
                task_token.cancel();
                operation.await
            }
        },
        (None, false) => operation.await,
    }
}

#[cfg(test)]
#[path = "../../tests/unit/runtime_control.rs"]
mod tests;
