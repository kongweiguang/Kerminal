//! 配置 Git 命令的共享执行入口与仓库级清理协调。
//!
//! @author kongweiguang

use std::time::Duration;
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use kerminal_application::{
    AppError,
    operations::{
        CommandExecutionLimits, CommandExecutionTarget, CommandInvocation, CommandStreamEvent,
        OperationsCommandExecutor, OperationsCommandStream,
    },
};
use tokio::{
    sync::{Mutex, OwnedMutexGuard, watch},
    task::JoinHandle,
    time::timeout_at,
};
use tokio_util::sync::CancellationToken;

use super::git::{GitCommandError, GitOutput, classify_failure};

const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub(super) const INITIAL_CLEANUP_BUDGET: Duration = Duration::from_secs(1);
const MAX_GIT_STDOUT_BYTES: usize = 64 * 1024 * 1024;
const STDERR_CAPTURE_LIMIT: usize = 64 * 1024;

/// 仓库内唯一 Git worker 的生命周期状态；PendingCleanup 保留原操作栈和流句柄。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GitWorkerState {
    Idle,
    Running,
    PendingCleanup,
}

#[derive(Clone, Copy, Debug)]
struct RetryRequest {
    generation: u64,
    deadline: tokio::time::Instant,
}

/// 复用桌面现有运维命令执行器，并序列化同一配置根上的 Git 命令与恢复。
pub(super) struct GitCommandRuntime {
    executor: Arc<dyn OperationsCommandExecutor>,
    runtime: tokio::runtime::Handle,
    program: String,
    environment: BTreeMap<String, String>,
    command_timeout: Duration,
    max_output_bytes: usize,
    gate: Arc<Mutex<()>>,
    worker: Arc<Mutex<Option<TrackedGitWorker>>>,
    state: watch::Sender<GitWorkerState>,
    retry: watch::Sender<Option<RetryRequest>>,
    retry_generation: AtomicU64,
    accepting: AtomicBool,
    cancellation: StdMutex<Option<CancellationToken>>,
}

/// 一个 Git 操作持有仓库 gate，避免在旧子进程或管道仍待清理时并发改动 `.git`。
pub(super) struct GitOperationPermit {
    runtime: Arc<GitCommandRuntime>,
    guard: Option<OwnedMutexGuard<()>>,
    worker_slot: Option<OwnedMutexGuard<Option<TrackedGitWorker>>>,
}

/// 保留真实 blocking JoinHandle 与仓库许可，直到有调用方观察到任务退出。
struct TrackedGitWorker {
    join: JoinHandle<()>,
    _guard: Option<OwnedMutexGuard<()>>,
}

impl GitCommandRuntime {
    /// 绑定唯一执行器并保留生产 Git 程序名与默认预算，不在同步仓储内创建 runtime。
    pub(super) fn new(
        executor: Arc<dyn OperationsCommandExecutor>,
        runtime: tokio::runtime::Handle,
    ) -> Self {
        Self::with_configuration(
            executor,
            runtime,
            "git".to_owned(),
            BTreeMap::new(),
            GIT_COMMAND_TIMEOUT,
            MAX_GIT_STDOUT_BYTES,
        )
    }

    /// 配置同一生产 runner 的程序和预算；组合根传固定 Git，隔离测试注入假程序及小预算。
    pub(super) fn with_configuration(
        executor: Arc<dyn OperationsCommandExecutor>,
        runtime: tokio::runtime::Handle,
        program: String,
        environment: BTreeMap<String, String>,
        command_timeout: Duration,
        max_output_bytes: usize,
    ) -> Self {
        let (state, _) = watch::channel(GitWorkerState::Idle);
        let (retry, _) = watch::channel(None);
        Self {
            executor,
            runtime,
            program,
            environment,
            command_timeout,
            max_output_bytes,
            gate: Arc::new(Mutex::new(())),
            worker: Arc::new(Mutex::new(None)),
            state,
            retry,
            retry_generation: AtomicU64::new(0),
            accepting: AtomicBool::new(true),
            cancellation: StdMutex::new(None),
        }
    }

    /// 查询 worker 状态供只读仓库状态与新的同步请求决定是否可继续。
    pub(super) fn state(&self) -> GitWorkerState {
        *self.state.borrow()
    }

    /// 为新操作获取仓库级许可；旧清理未确认时仅唤醒并等待原 worker。
    pub(super) async fn begin_operation(
        self: &Arc<Self>,
        cancellation: CancellationToken,
    ) -> Result<GitOperationPermit, GitOperationStartError> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(GitOperationStartError::ShuttingDown);
        }

        let deadline = tokio::time::Instant::now() + INITIAL_CLEANUP_BUDGET;
        loop {
            if !self.accepting.load(Ordering::Acquire) {
                return Err(GitOperationStartError::ShuttingDown);
            }
            match self.state() {
                GitWorkerState::Idle => {
                    if let Ok(guard) = self.gate.clone().try_lock_owned() {
                        if self.state() != GitWorkerState::Idle {
                            drop(guard);
                            continue;
                        }
                        let worker_slot = self.worker.clone().lock_owned().await;
                        if worker_slot.is_some() {
                            drop(worker_slot);
                            drop(guard);
                            self.join_worker_until(deadline).await.map_err(
                                |error| match error {
                                    GitWorkerJoinError::Pending => {
                                        GitOperationStartError::PendingCleanup
                                    }
                                    GitWorkerJoinError::Failed(source) => {
                                        GitOperationStartError::PreviousWorkerFailed(source)
                                    }
                                },
                            )?;
                            continue;
                        }
                        if !self.accepting.load(Ordering::Acquire)
                            || self.state() != GitWorkerState::Idle
                        {
                            drop(worker_slot);
                            drop(guard);
                            continue;
                        }
                        self.state.send_replace(GitWorkerState::Running);
                        *self
                            .cancellation
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
                            Some(cancellation.clone());
                        return Ok(GitOperationPermit {
                            runtime: self.clone(),
                            guard: Some(guard),
                            worker_slot: Some(worker_slot),
                        });
                    }
                    return Err(GitOperationStartError::Busy);
                }
                GitWorkerState::Running => {
                    if self.worker_is_finished().await {
                        self.join_worker_until(deadline)
                            .await
                            .map_err(|error| match error {
                                GitWorkerJoinError::Pending => GitOperationStartError::Busy,
                                GitWorkerJoinError::Failed(source) => {
                                    GitOperationStartError::PreviousWorkerFailed(source)
                                }
                            })?;
                        continue;
                    }
                    return Err(GitOperationStartError::Busy);
                }
                GitWorkerState::PendingCleanup => {
                    self.request_retry(deadline);
                    match self.join_worker_until(deadline).await {
                        Ok(()) => continue,
                        Err(GitWorkerJoinError::Pending) => {
                            return Err(GitOperationStartError::PendingCleanup);
                        }
                        Err(GitWorkerJoinError::Failed(source)) => {
                            return Err(GitOperationStartError::PreviousWorkerFailed(source));
                        }
                    }
                }
            }
        }
    }

    /// 把原 blocking task 的 JoinHandle 和 gate 许可登记到唯一仓库 owner 中。
    pub(super) fn track_worker(&self, mut permit: GitOperationPermit, join: JoinHandle<()>) {
        let mut slot = permit
            .worker_slot
            .take()
            .expect("Git operation permit always reserves its worker slot");
        let guard = permit
            .guard
            .take()
            .expect("Git operation permit always owns its repository gate");
        debug_assert!(slot.is_none());
        *slot = Some(TrackedGitWorker {
            join,
            _guard: Some(guard),
        });
    }

    /// 检查原 blocking task 是否已退出，供新 Git 操作只回收完成 owner。
    async fn worker_is_finished(&self) -> bool {
        self.worker
            .lock()
            .await
            .as_ref()
            .is_some_and(|worker| worker.join.is_finished())
    }

    /// 有界等待并消费同一个 worker owner；超时时保留 JoinHandle 和 gate 供新 deadline 重试。
    pub(super) async fn join_worker_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<(), GitWorkerJoinError> {
        let mut slot = self.worker.lock().await;
        let Some(worker) = slot.as_mut() else {
            return if self.state() == GitWorkerState::Idle {
                Ok(())
            } else {
                Err(GitWorkerJoinError::Failed(AppError::infrastructure(
                    "Git worker 状态仍忙但原任务 owner 不存在",
                )))
            };
        };

        let joined = timeout_at(deadline, &mut worker.join)
            .await
            .map_err(|_| GitWorkerJoinError::Pending)?;
        let completed = slot
            .take()
            .expect("the worker slot remains locked until its JoinHandle is observed");
        drop(completed);
        *self
            .cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        self.state.send_replace(GitWorkerState::Idle);
        joined.map_err(|source| {
            GitWorkerJoinError::Failed(AppError::infrastructure_with_source(
                "Git blocking worker 异常终止",
                source,
            ))
        })
    }

    /// 运行结构化本地 Git argv，并在管道或进程清理未确认时保留当前流等待后续 deadline。
    pub(super) fn run_git(
        &self,
        root: &std::path::Path,
        arguments: &[std::ffi::OsString],
        stdin_data: Option<Vec<u8>>,
        cancellation: &CancellationToken,
        use_editor_sentinel: bool,
        allow_nonzero: bool,
    ) -> Result<GitOutput, GitCommandError> {
        if cancellation.is_cancelled() {
            return Err(GitCommandError::Cancelled);
        }

        let mut invocation = CommandInvocation::new(
            self.program.clone(),
            arguments
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect(),
        );
        invocation.environment.extend(self.environment.clone());
        invocation.working_directory = Some(root.to_string_lossy().into_owned());
        invocation
            .environment
            .insert("GIT_TERMINAL_PROMPT".into(), "0".into());
        invocation
            .environment
            .insert("GCM_INTERACTIVE".into(), "Never".into());
        invocation
            .environment
            .insert("GIT_OPTIONAL_LOCKS".into(), "0".into());
        if use_editor_sentinel {
            invocation
                .environment
                .insert("GIT_EDITOR".into(), ":".into());
            invocation
                .environment
                .insert("GIT_SEQUENCE_EDITOR".into(), ":".into());
        }

        let retry_generation = self.retry_generation.load(Ordering::Acquire);
        let mut retry = self.retry.subscribe();
        self.runtime.block_on(self.run_stream(
            invocation,
            stdin_data,
            cancellation.clone(),
            allow_nonzero,
            retry_generation,
            &mut retry,
        ))
    }

    /// 消费 Git 输出并确认 worker 清理；失败时先释放不再轮询的 writer，再重试同一 stream。
    ///
    /// writer 等待输入 ACK 时可能持有共享 EOF 锁，必须在再次 close 前结束其真实 future，
    /// 避免清理路径等待一个已不再轮询的锁持有者；外层存储锁仍由原 worker 保留。
    /// @author kongweiguang
    async fn run_stream(
        &self,
        invocation: CommandInvocation,
        stdin_data: Option<Vec<u8>>,
        cancellation: CancellationToken,
        allow_nonzero: bool,
        retry_generation: u64,
        retry: &mut watch::Receiver<Option<RetryRequest>>,
    ) -> Result<GitOutput, GitCommandError> {
        let limits = CommandExecutionLimits {
            timeout: self.command_timeout,
            max_output_bytes: self.max_output_bytes,
        };
        let mut stream = self
            .executor
            .execute_stream(
                CommandExecutionTarget::Local,
                invocation,
                limits,
                cancellation.clone(),
            )
            .await
            .map_err(GitCommandError::Executor)?;

        let input = stream.input();
        let input_requested = stdin_data.as_ref().is_some_and(|bytes| !bytes.is_empty());
        let mut stdout = Vec::new();
        let mut stderr_prefix = Vec::new();
        let mut writer_done = false;
        let mut output_done = false;
        let mut input_delivery_error = None;
        let mut primary = None;
        {
            let writer_input = input.clone();
            let writer = async move {
                if let Some(bytes) = stdin_data {
                    for chunk in bytes.chunks(32 * 1024) {
                        writer_input.write(chunk.to_vec()).await?;
                    }
                }
                writer_input.close().await
            };
            tokio::pin!(writer);

            while !writer_done || !output_done {
                tokio::select! {
                    _ = cancellation.cancelled(), if primary.is_none() => {
                        primary = Some(GitCommandError::Cancelled);
                        break;
                    }
                    result = &mut writer, if !writer_done => {
                        writer_done = true;
                        if let Err(error) = result {
                            if cancellation.is_cancelled()
                                || error.public_code() == kerminal_application::PublicErrorCode::Cancelled
                            {
                                primary = Some(GitCommandError::Cancelled);
                                break;
                            }
                            if input_requested {
                                input_delivery_error = Some(error);
                            }
                        }
                    }
                    event = stream.next_event(), if !output_done => match event {
                        Ok(Some(CommandStreamEvent::Stdout(bytes))) => {
                            let remaining = self.max_output_bytes.saturating_sub(stdout.len());
                            let copied = remaining.min(bytes.len());
                            stdout.extend_from_slice(&bytes[..copied]);
                            if copied != bytes.len() {
                                primary = Some(GitCommandError::OutputLimit);
                                break;
                            }
                        }
                        Ok(Some(CommandStreamEvent::Stderr(bytes))) => {
                            let remaining = STDERR_CAPTURE_LIMIT.saturating_sub(stderr_prefix.len());
                            stderr_prefix.extend_from_slice(&bytes[..remaining.min(bytes.len())]);
                        }
                        Ok(None) => output_done = true,
                        Err(error) => {
                            primary = Some(GitCommandError::Executor(error));
                            break;
                        }
                    }
                }
            }
        }

        if let Some(primary) = primary {
            input.close().await.ok();
            self.close_with_retry(&mut stream, retry_generation, retry)
                .await?;
            if cancellation.is_cancelled() {
                return Err(GitCommandError::Cancelled);
            }
            if matches!(&primary, GitCommandError::OutputLimit) {
                return Err(primary);
            }
            let receipt = GitOutput {
                code: None,
                stdout,
                stderr_prefix,
            };
            return Err(GitCommandError::OutcomeUnknown {
                receipt: Some(Box::new(receipt)),
                cause: match primary {
                    GitCommandError::Executor(error) => Some(error),
                    _ => None,
                },
            });
        }

        let exit = match stream.finish().await {
            Ok(exit) => exit,
            Err(error) => {
                // finish 的执行或清理错误不能丢弃 stream；先拿到 ACK 再结束原操作 owner。
                let error = super::git::map_executor_error(error);
                self.close_with_retry(&mut stream, retry_generation, retry)
                    .await?;
                return Err(error);
            }
        };

        let output = GitOutput {
            code: exit.exit_code,
            stdout,
            stderr_prefix: if stderr_prefix.is_empty() {
                exit.stderr
            } else {
                stderr_prefix
            },
        };
        let input_delivery_unconfirmed =
            input_requested && (!exit.input_complete || input_delivery_error.is_some());
        if input_delivery_unconfirmed {
            return Err(GitCommandError::OutcomeUnknown {
                receipt: Some(Box::new(output)),
                cause: input_delivery_error,
            });
        }
        if !allow_nonzero && output.code != Some(0) {
            return Err(GitCommandError::Failed {
                kind: classify_failure(&output.stderr_prefix),
            });
        }
        Ok(output)
    }

    /// 先尝试短清理预算；超时后让调用方看见 PendingBusy，同时保留 stream 等待未来 deadline。
    async fn close_with_retry(
        &self,
        stream: &mut OperationsCommandStream,
        retry_generation: u64,
        retry: &mut watch::Receiver<Option<RetryRequest>>,
    ) -> Result<(), GitCommandError> {
        let first_deadline = tokio::time::Instant::now() + INITIAL_CLEANUP_BUDGET;
        if stream.close_until(first_deadline).await.is_ok() {
            return Ok(());
        }
        self.state.send_replace(GitWorkerState::PendingCleanup);
        let mut last_generation = retry_generation;
        loop {
            if let Some(request) = *retry.borrow_and_update()
                && request.generation > last_generation
            {
                last_generation = request.generation;
                if stream.close_until(request.deadline).await.is_ok() {
                    self.state.send_replace(GitWorkerState::Running);
                    return Ok(());
                }
            }
            retry.changed().await.map_err(|_| {
                GitCommandError::Executor(kerminal_application::AppError::infrastructure(
                    "Git 清理 deadline 订阅已关闭",
                ))
            })?;
        }
    }

    /// 等待退出中的命令释放 gate；超时返回错误且保留仍在工作的 permit owner。
    pub(super) async fn shutdown_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> kerminal_application::AppResult<()> {
        self.accepting.store(false, Ordering::Release);
        if let Some(cancellation) = self
            .cancellation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            cancellation.cancel();
        }
        self.request_retry(deadline);
        match self.join_worker_until(deadline).await {
            Ok(()) => Ok(()),
            Err(GitWorkerJoinError::Pending) => Err(AppError::timeout("Git 同步退出清理")),
            Err(GitWorkerJoinError::Failed(source)) => Err(source),
        }
    }

    /// 用新的绝对 deadline 唤醒同一个 pending worker，不创建替代 Git 子进程。
    pub(super) fn request_retry(&self, deadline: tokio::time::Instant) {
        let generation = self.retry_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.retry.send_replace(Some(RetryRequest {
            generation,
            deadline,
        }));
    }

    /// 返回当前状态订阅，使 async 组合层可在 worker 未退出时及时报告未知结果。
    pub(super) fn subscribe_state(&self) -> watch::Receiver<GitWorkerState> {
        self.state.subscribe()
    }
}

impl Drop for GitOperationPermit {
    /// 只有尚未登记 worker 的启动失败才直接释放许可；登记后由 join 路径决定 Idle。
    fn drop(&mut self) {
        if let Some(guard) = self.guard.take() {
            drop(guard);
            *self
                .runtime
                .cancellation
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            self.runtime.state.send_replace(GitWorkerState::Idle);
        }
    }
}

/// 表示 join worker 时遇到的可重试超时或保留 source 的致命失败。
pub(super) enum GitWorkerJoinError {
    Pending,
    Failed(AppError),
}

#[derive(Debug)]
pub(super) enum GitOperationStartError {
    Busy,
    PendingCleanup,
    ShuttingDown,
    PreviousWorkerFailed(AppError),
}

#[cfg(test)]
#[path = "../../tests/unit/configuration_sync_runner.rs"]
mod tests;
