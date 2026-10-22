//! Git 同步 worker 的可重试 join 生命周期测试。
//!
//! @author kongweiguang

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        ContainerStageCleanupCapability, ContainerStageCleanupRequest, OperationsCommandExecutor,
        OperationsCommandStream,
    },
};
use tokio_util::sync::CancellationToken;

use crate::configuration_sync::runner::{GitCommandRuntime, GitWorkerState};

struct NoopExecutor;

#[async_trait]
impl OperationsCommandExecutor for NoopExecutor {
    /// 该 worker 生命周期测试不执行命令，避免依赖本机 shell 或远端连接。
    async fn execute(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        Err(AppError::unsupported("测试不执行命令"))
    }

    /// 该测试不需要流式执行，拒绝调用可确保 fixture 不创建隐藏 worker。
    async fn execute_stream(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        Err(AppError::unsupported("测试不创建命令流"))
    }

    /// fixture 不管理容器 staging，不签发真实清理能力。
    fn reserve_container_stage_cleanup(
        &self,
        _request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        Err(AppError::unsupported("测试不创建容器 staging"))
    }

    /// fixture 没有在途命令，停止入口不需要额外动作。
    fn begin_shutdown(&self, _deadline: tokio::time::Instant) {}

    /// fixture 没有后台资源，退出完成为空操作。
    async fn shutdown_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        Ok(())
    }
}

/// 超时只结束本次等待，仓库许可与原 JoinHandle 留给后续绝对 deadline 收尾。
#[tokio::test]
async fn shutdown_timeout_retains_worker_owner_and_gate_until_join() {
    let runtime = Arc::new(GitCommandRuntime::new(
        Arc::new(NoopExecutor),
        tokio::runtime::Handle::current(),
    ));
    let permit = runtime
        .begin_operation(CancellationToken::new())
        .await
        .expect("取得隔离 Git worker 许可");
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let worker = tokio::task::spawn_blocking(move || {
        let _ = started_sender.send(());
        release_receiver.recv().expect("测试释放原 worker");
    });
    runtime.track_worker(permit, worker);
    started_receiver.await.expect("原 worker 已开始");
    runtime.state.send_replace(GitWorkerState::PendingCleanup);

    let first_deadline = tokio::time::Instant::now() + Duration::from_millis(25);
    assert!(runtime.shutdown_until(first_deadline).await.is_err());
    assert_eq!(runtime.state(), GitWorkerState::PendingCleanup);
    assert!(runtime.worker.lock().await.is_some());
    assert!(runtime.gate.clone().try_lock_owned().is_err());

    release_sender.send(()).expect("释放原 blocking worker");
    runtime
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("用新 deadline join 同一个 worker");

    assert_eq!(runtime.state(), GitWorkerState::Idle);
    assert!(runtime.worker.lock().await.is_none());
    assert!(runtime.gate.clone().try_lock_owned().is_ok());
}
