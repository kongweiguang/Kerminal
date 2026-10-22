//! 将结构化运维调用适配到本机子进程或共享 SSH exec channel。
//!
//! @author kongweiguang

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as StdMutex, MutexGuard as StdMutexGuard},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::RemoteConnectionService,
    operations::{
        CommandExecutionLimits, CommandExecutionResult, CommandExecutionTarget, CommandInvocation,
        CommandStreamEvent, CommandStreamState, ContainerStageCleanupCapability,
        ContainerStageCleanupRequest, OperationsCommandExecutor, OperationsCommandStream,
        StageCreationResult,
    },
};
use kerminal_domain::files::FileEndpoint;
use tokio::{
    sync::watch,
    time::{Instant, sleep_until},
};
use tokio_util::sync::CancellationToken;

const MAX_COMMAND_INPUT_BYTES: usize = 64 * 1024 * 1024;
const LOCAL_CLEANUP_FALLBACK: std::time::Duration = std::time::Duration::from_secs(5);

/// 本机命令与已保存 SSH 主机共享同一结构化端口，不接受 shell 命令字符串。
pub struct StructuredOperationsCommandExecutor {
    remote_connections: Arc<dyn RemoteConnectionService>,
    lifecycle: Arc<StdMutex<ExecutorLifecycle>>,
    active_count: watch::Sender<usize>,
    shutdown_ceiling: watch::Sender<Option<Instant>>,
}

struct ExecutorLifecycle {
    accepting: bool,
    next_id: u64,
    active: HashMap<u64, ActiveCommand>,
    stage_cleanup_reservations: HashSet<u64>,
}

struct ActiveCommand {
    cancellation: CancellationToken,
    cleanup_deadline: watch::Sender<Option<Instant>>,
    cancel_on_shutdown: bool,
}

struct CommandPermit {
    id: u64,
    lifecycle: Arc<StdMutex<ExecutorLifecycle>>,
    active_count: watch::Sender<usize>,
    cancel: CancellationToken,
    cleanup_deadline: watch::Receiver<Option<Instant>>,
    cleanup_deadline_sender: watch::Sender<Option<Instant>>,
    shutdown_ceiling: watch::Receiver<Option<Instant>>,
}

/// 单个上传预签的清理许可；状态决定 capability drop 能否证明 staging 从未建立。
struct ContainerStageCleanupCapabilityImpl {
    id: u64,
    staging_endpoint: FileEndpoint,
    owner: String,
    remote_connections: Arc<dyn RemoteConnectionService>,
    lifecycle: Arc<StdMutex<ExecutorLifecycle>>,
    active_count: watch::Sender<usize>,
    shutdown_ceiling: watch::Sender<Option<Instant>>,
    phase: StdMutex<StageCapabilityPhase>,
    operation: tokio::sync::Mutex<()>,
    pending_command: tokio::sync::Mutex<Option<OperationsCommandStream>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum StageCapabilityPhase {
    Reserved,
    CreationMayHaveStarted,
    Created,
    Cleaned,
    NotCreated,
}

impl StructuredOperationsCommandExecutor {
    /// 注入连接 service 以复用认证 lease；本机命令仍直接由 Tokio 启动，不经过 shell。
    pub fn new(remote_connections: Arc<dyn RemoteConnectionService>) -> Self {
        let (active_count, _) = watch::channel(0);
        let (shutdown_ceiling, _) = watch::channel(None);
        Self {
            remote_connections,
            lifecycle: Arc::new(StdMutex::new(ExecutorLifecycle {
                accepting: true,
                next_id: 1,
                active: HashMap::new(),
                stage_cleanup_reservations: HashSet::new(),
            })),
            active_count,
            shutdown_ceiling,
        }
    }

    /// 将调用取消 token 纳入统一退出目录，确保 shutdown 能打断连接、管道和子进程。
    fn begin_command(&self, caller_cancel: &CancellationToken) -> AppResult<CommandPermit> {
        let mut lifecycle = lock_recover(&self.lifecycle);
        if !lifecycle.accepting {
            return Err(AppError::cancelled("应用正在退出，已停止接收运维命令"));
        }
        let id = lifecycle.next_id;
        lifecycle.next_id = lifecycle.next_id.wrapping_add(1).max(1);
        let cancel = caller_cancel.child_token();
        let (cleanup_deadline_sender, cleanup_deadline) = watch::channel(None);
        lifecycle.active.insert(
            id,
            ActiveCommand {
                cancellation: cancel.clone(),
                cleanup_deadline: cleanup_deadline_sender.clone(),
                cancel_on_shutdown: true,
            },
        );
        self.active_count
            .send_replace(tracked_resources(&lifecycle));
        Ok(CommandPermit {
            id,
            lifecycle: self.lifecycle.clone(),
            active_count: self.active_count.clone(),
            cancel,
            cleanup_deadline,
            cleanup_deadline_sender,
            shutdown_ceiling: self.shutdown_ceiling.subscribe(),
        })
    }
}

impl Drop for CommandPermit {
    /// 命令 future 正常结束、失败或被取消时都从在途目录移除。
    fn drop(&mut self) {
        let mut lifecycle = lock_recover(&self.lifecycle);
        lifecycle.active.remove(&self.id);
        self.active_count
            .send_replace(tracked_resources(&lifecycle));
    }
}

/// 锁中毒时沿用已有命令目录，避免一个已退出任务阻断其余命令的清理。
fn lock_recover<T>(mutex: &StdMutex<T>) -> StdMutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Shutdown 等待 worker 与预签发 cleanup capability 两类资源，不能只统计在途命令。
fn tracked_resources(lifecycle: &ExecutorLifecycle) -> usize {
    lifecycle
        .active
        .len()
        .saturating_add(lifecycle.stage_cleanup_reservations.len())
}

#[async_trait]
impl OperationsCommandExecutor for StructuredOperationsCommandExecutor {
    /// 执行单个 argv 请求；取消只结束本次子进程或远端 exec channel。
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        let mut stream = <Self as OperationsCommandExecutor>::execute_stream(
            self, target, invocation, limits, cancel,
        )
        .await?;
        collect_buffered_stream(
            &mut stream,
            None,
            limits.max_output_bytes,
            CleanupDeadline::CommandBudget,
        )
        .await
        .result
    }

    /// 通过 stdin 发送有界二进制数据，避免把文件内容拼入命令行或 shell 脚本。
    async fn execute_with_input(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        input: Vec<u8>,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        if input.len() > MAX_COMMAND_INPUT_BYTES {
            return Err(AppError::invalid("运维命令输入超过安全上限"));
        }
        let mut stream = <Self as OperationsCommandExecutor>::execute_stream(
            self, target, invocation, limits, cancel,
        )
        .await?;
        collect_buffered_stream(
            &mut stream,
            Some(input),
            limits.max_output_bytes,
            CleanupDeadline::CommandBudget,
        )
        .await
        .result
    }

    /// 启动有界双向流并把 permit 移入 worker，调用方提前 drop 时后台仍负责回收进程与 SSH lease。
    async fn execute_stream(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream> {
        validate_invocation(&invocation, limits)?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("运维命令已取消"));
        }
        let permit = self.begin_command(&cancel)?;
        Ok(spawn_command_stream(
            self.remote_connections.clone(),
            target,
            invocation,
            limits,
            permit,
        ))
    }

    /// 为单个 staging endpoint 预留清理 worker 计数；关闭普通 intake 后不再签发新能力。
    fn reserve_container_stage_cleanup(
        &self,
        request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>> {
        let owner = super::container_transfer::stage_owner_marker(&request.transfer_id);
        super::container_transfer::stage_cleanup_invocation(
            &request.staging_endpoint,
            &owner,
            super::container_transfer::CREATE_STAGE_SCRIPT,
        )?;

        let mut lifecycle = lock_recover(&self.lifecycle);
        if !lifecycle.accepting {
            return Err(AppError::cancelled(
                "应用正在退出，已停止签发容器 staging 清理能力",
            ));
        }
        let id = lifecycle.next_id;
        lifecycle.next_id = lifecycle.next_id.wrapping_add(1).max(1);
        lifecycle.stage_cleanup_reservations.insert(id);
        self.active_count
            .send_replace(tracked_resources(&lifecycle));
        Ok(Arc::new(ContainerStageCleanupCapabilityImpl {
            id,
            staging_endpoint: request.staging_endpoint,
            owner,
            remote_connections: self.remote_connections.clone(),
            lifecycle: self.lifecycle.clone(),
            active_count: self.active_count.clone(),
            shutdown_ceiling: self.shutdown_ceiling.clone(),
            phase: StdMutex::new(StageCapabilityPhase::Reserved),
            operation: tokio::sync::Mutex::new(()),
            pending_command: tokio::sync::Mutex::new(None),
        }))
    }

    /// 关闭普通执行入口并取消常规命令；预签的 cleanup worker 只接收新 deadline，不被取消。
    fn begin_shutdown(&self, deadline: Instant) {
        let mut lifecycle = lock_recover(&self.lifecycle);
        lifecycle.accepting = false;
        self.shutdown_ceiling.send_replace(Some(deadline));
        for active in lifecycle.active.values() {
            active.cleanup_deadline.send_replace(Some(deadline));
            if active.cancel_on_shutdown {
                active.cancellation.cancel();
            }
        }
        self.active_count
            .send_replace(tracked_resources(&lifecycle));
    }

    /// 重复使用 composition root 的 deadline 等待命令与未释放 cleanup capability 收尾。
    async fn shutdown_until(&self, deadline: Instant) -> AppResult<()> {
        let mut active_count = self.active_count.subscribe();
        self.begin_shutdown(deadline);
        loop {
            if *active_count.borrow_and_update() == 0 {
                return Ok(());
            }
            tokio::select! {
                _ = sleep_until(deadline) => {
                    return Err(AppError::timeout("运维命令退出清理"));
                }
                changed = active_count.changed() => {
                    if changed.is_err() {
                        return Err(AppError::infrastructure("运维命令在途状态订阅已关闭"));
                    }
                }
            }
        }
    }
}

struct BufferedStreamOutcome {
    result: AppResult<CommandExecutionResult>,
    resources_released: bool,
}

#[derive(Clone, Copy)]
enum CleanupDeadline {
    /// 普通命令只有进入失败清理时才开始计算本次预算，避免长命令拿到已经过期的时限。
    CommandBudget,
    /// 预签发 staging 操作使用调用方传入的绝对时限，不能自行延长提交/清理窗口。
    Fixed(Instant),
}

impl CleanupDeadline {
    /// 将普通命令预算锚定到清理开始时，保留 staging 调用的绝对 deadline 语义。
    fn resolve(self) -> Instant {
        match self {
            Self::CommandBudget => Instant::now() + LOCAL_CLEANUP_FALLBACK,
            Self::Fixed(deadline) => deadline,
        }
    }
}

/// 把结构化短命令收敛到与长流相同的 worker，使输出回收超时仍保留进程和清理 owner。
///
/// @author kongweiguang
fn spawn_command_stream(
    remote_connections: Arc<dyn RemoteConnectionService>,
    target: CommandExecutionTarget,
    invocation: CommandInvocation,
    limits: CommandExecutionLimits,
    permit: CommandPermit,
) -> OperationsCommandStream {
    let cancellation = permit.cancel.clone();
    // worker 结束时会取消自己的 token；父 token 只代表调用方取消，避免自然 EOF 冒充用户取消。
    let worker_cancellation = cancellation.child_token();
    let cleanup_deadline = permit.cleanup_deadline.clone();
    let (event_sender, event_receiver) = tokio::sync::mpsc::channel(8);
    let (input_sender, input_receiver) = tokio::sync::mpsc::channel(8);
    let (completion_sender, completion_receiver) =
        watch::channel(CommandStreamState::Running { attempt: 0 });
    let stream = OperationsCommandStream::new(
        event_receiver,
        input_sender,
        completion_receiver,
        cancellation.clone(),
        permit.cleanup_deadline_sender.clone(),
        permit.shutdown_ceiling.clone(),
    );
    tokio::spawn(async move {
        let completion =
            super::command_stream::run_stream_worker(super::command_stream::StreamWorkerRequest {
                remote_connections,
                target,
                invocation,
                limits,
                input: input_receiver,
                output: event_sender,
                cancellation: worker_cancellation,
                cleanup_deadline,
                completion_state: completion_sender.clone(),
            })
            .await;
        completion_sender.send_replace(CommandStreamState::Complete(completion));
        drop(permit);
    });
    stream
}

/// 并行发送 stdin 与收集有界输出；清理未确认时返回错误但保留 stream worker 供 shutdown 重试。
async fn collect_buffered_stream(
    stream: &mut OperationsCommandStream,
    input: Option<Vec<u8>>,
    max_output_bytes: usize,
    cleanup_deadline: CleanupDeadline,
) -> BufferedStreamOutcome {
    let input_handle = stream.input();
    let input_expected = input.is_some();
    let mut writer = Box::pin(async move {
        if let Some(input) = input {
            if input.is_empty() {
                // 空 payload 仍须得到一次真实 writer ACK，区分“无输入命令”和“请求发送空文件”。
                input_handle.write(Vec::new()).await?;
            } else {
                for chunk in input.chunks(32 * 1024) {
                    input_handle.write(chunk.to_vec()).await?;
                }
            }
        }
        input_handle.close().await
    });
    let mut input_result = None;
    let mut input_error = None;
    let mut output_closed = false;
    let mut output_error = None;
    let mut input_incomplete = false;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut used = 0usize;
    let mut truncated = false;

    while input_result.is_none() || !output_closed {
        tokio::select! {
            result = &mut writer, if input_result.is_none() => {
                if let Err(error) = &result {
                    if input_expected && error.public_code() != kerminal_application::PublicErrorCode::Cancelled {
                        input_incomplete = true;
                    } else if !input_expected && error.public_code() != kerminal_application::PublicErrorCode::Cancelled {
                        // 未申请输入时，子进程退出可使冗余 EOF 失败；只有真实调用取消需要传播。
                    } else {
                        input_error = Some(error.clone());
                    }
                }
                input_result = Some(result);
            }
            event = stream.next_event(), if !output_closed => match event {
                Ok(Some(CommandStreamEvent::Stdout(bytes))) => {
                    append_bounded(&mut stdout, bytes, &mut used, max_output_bytes, &mut truncated);
                }
                Ok(Some(CommandStreamEvent::Stderr(bytes))) => {
                    append_bounded(&mut stderr, bytes, &mut used, max_output_bytes, &mut truncated);
                }
                Ok(None) => output_closed = true,
                Err(error) => {
                    output_error = Some(error);
                    break;
                }
            }
        }
    }

    let primary_error = output_error.or(input_error);
    if let Some(primary_error) = primary_error {
        let cleanup = stream.close_until(cleanup_deadline.resolve()).await;
        return BufferedStreamOutcome {
            resources_released: cleanup.is_ok(),
            result: combine_cleanup(Err(primary_error), cleanup),
        };
    }

    match stream.finish().await {
        Ok(exit) => BufferedStreamOutcome {
            result: Ok(CommandExecutionResult {
                stdout,
                stderr,
                exit_code: exit.exit_code,
                truncated,
                input_complete: !input_incomplete && exit.input_complete,
            }),
            resources_released: true,
        },
        Err(primary_error) => {
            let cleanup = stream.close_until(cleanup_deadline.resolve()).await;
            BufferedStreamOutcome {
                resources_released: cleanup.is_ok(),
                result: combine_cleanup(Err(primary_error), cleanup),
            }
        }
    }
}

#[async_trait]
impl ContainerStageCleanupCapability for ContainerStageCleanupCapabilityImpl {
    /// 唯一创建命令固定到预签端点；任何不确定失败都保留 capability 供安全清理重试。
    async fn create_stage_until(&self, deadline: Instant) -> AppResult<StageCreationResult> {
        let _operation = self.operation.lock().await;
        self.close_pending_command(deadline).await?;
        match *lock_recover(&self.phase) {
            StageCapabilityPhase::Created => return Ok(StageCreationResult::Created),
            StageCapabilityPhase::Reserved => {
                *lock_recover(&self.phase) = StageCapabilityPhase::CreationMayHaveStarted;
            }
            StageCapabilityPhase::CreationMayHaveStarted => {
                return Err(AppError::conflict(
                    "容器 staging 创建结果未知，必须先清理后才能重试",
                ));
            }
            StageCapabilityPhase::Cleaned | StageCapabilityPhase::NotCreated => {
                return Err(AppError::conflict("容器 staging 清理能力已结束"));
            }
        }

        let result = self
            .run_bound_command(super::container_transfer::CREATE_STAGE_SCRIPT, deadline)
            .await?;
        match result.exit_code {
            Some(0) if !result.truncated => {
                *lock_recover(&self.phase) = StageCapabilityPhase::Created;
                Ok(StageCreationResult::Created)
            }
            // 脚本只在观察到已有路径时返回 41，此分支确认本次没有建立自己的 staging。
            Some(41) if !result.truncated => {
                *lock_recover(&self.phase) = StageCapabilityPhase::NotCreated;
                release_stage_reservation(self.id, &self.lifecycle, &self.active_count);
                Ok(StageCreationResult::AlreadyExists)
            }
            _ => Err(stage_command_error(&result, "容器 staging 创建失败")),
        }
    }

    /// 仅在 staging 不存在或 marker 匹配后删除并确认消失时释放 shutdown 预留计数。
    async fn cleanup_until(&self, deadline: Instant) -> AppResult<()> {
        let _operation = self.operation.lock().await;
        self.close_pending_command(deadline).await?;
        match *lock_recover(&self.phase) {
            StageCapabilityPhase::Cleaned => return Ok(()),
            StageCapabilityPhase::NotCreated => {
                return Err(AppError::conflict("staging 路径属于其他资源，拒绝清理"));
            }
            StageCapabilityPhase::Reserved
            | StageCapabilityPhase::CreationMayHaveStarted
            | StageCapabilityPhase::Created => {}
        }
        let result = self
            .run_bound_command(super::container_transfer::CLEANUP_STAGE_SCRIPT, deadline)
            .await?;
        if result.truncated || result.exit_code != Some(0) {
            return Err(stage_command_error(
                &result,
                "容器 staging 所有权校验或清理失败",
            ));
        }
        *lock_recover(&self.phase) = StageCapabilityPhase::Cleaned;
        release_stage_reservation(self.id, &self.lifecycle, &self.active_count);
        Ok(())
    }
}

impl ContainerStageCleanupCapabilityImpl {
    /// 重试上次 reserved command 的同一 stream owner，关闭确认后才允许发起下一条命令。
    async fn close_pending_command(&self, deadline: Instant) -> AppResult<()> {
        let mut pending = self.pending_command.lock().await;
        if let Some(stream) = pending.as_mut() {
            stream.close_until(deadline).await?;
            pending.take();
        }
        Ok(())
    }

    /// 只接受调用方预签的 endpoint/marker；命令未确认清理时保留同一 stream 供后续 deadline 重试。
    async fn run_bound_command(
        &self,
        script: &'static str,
        deadline: Instant,
    ) -> AppResult<CommandExecutionResult> {
        if deadline <= Instant::now() {
            return Err(AppError::timeout("容器 staging 清理"));
        }
        let (target, invocation) = super::container_transfer::stage_cleanup_invocation(
            &self.staging_endpoint,
            &self.owner,
            script,
        )?;
        let permit = begin_reserved_command(
            self.id,
            deadline,
            &self.lifecycle,
            &self.active_count,
            &self.shutdown_ceiling,
        )?;
        let limits = CommandExecutionLimits {
            timeout: deadline.saturating_duration_since(Instant::now()),
            max_output_bytes: 64 * 1024,
        };
        let mut stream = spawn_command_stream(
            self.remote_connections.clone(),
            target,
            invocation,
            limits,
            permit,
        );
        let outcome = collect_buffered_stream(
            &mut stream,
            None,
            limits.max_output_bytes,
            CleanupDeadline::Fixed(deadline),
        )
        .await;
        if !outcome.resources_released {
            let mut pending = self.pending_command.lock().await;
            if pending.is_some() {
                return Err(AppError::conflict(
                    "staging 清理命令已有待重试 stream，拒绝覆盖其资源 owner",
                ));
            }
            *pending = Some(stream);
        }
        outcome.result
    }
}

impl Drop for ContainerStageCleanupCapabilityImpl {
    /// 未尝试创建 staging 时释放空预留；结果可能有副作用后必须让退出等待可见的 cleanup owner。
    fn drop(&mut self) {
        if *lock_recover(&self.phase) == StageCapabilityPhase::Reserved {
            release_stage_reservation(self.id, &self.lifecycle, &self.active_count);
        }
    }
}

/// 通过 reservation ID 登记 cleanup 命令；它不受普通 intake 开关与常规取消影响。
fn begin_reserved_command(
    reservation_id: u64,
    deadline: Instant,
    lifecycle: &Arc<StdMutex<ExecutorLifecycle>>,
    active_count: &watch::Sender<usize>,
    shutdown_ceiling: &watch::Sender<Option<Instant>>,
) -> AppResult<CommandPermit> {
    let mut lifecycle_guard = lock_recover(lifecycle);
    if !lifecycle_guard
        .stage_cleanup_reservations
        .contains(&reservation_id)
    {
        return Err(AppError::cancelled("容器 staging 清理许可已释放"));
    }
    let id = lifecycle_guard.next_id;
    lifecycle_guard.next_id = lifecycle_guard.next_id.wrapping_add(1).max(1);
    let cancellation = CancellationToken::new();
    let (cleanup_deadline_sender, cleanup_deadline) = watch::channel(Some(deadline));
    lifecycle_guard.active.insert(
        id,
        ActiveCommand {
            cancellation: cancellation.clone(),
            cleanup_deadline: cleanup_deadline_sender.clone(),
            cancel_on_shutdown: false,
        },
    );
    active_count.send_replace(tracked_resources(&lifecycle_guard));
    Ok(CommandPermit {
        id,
        lifecycle: lifecycle.clone(),
        active_count: active_count.clone(),
        cancel: cancellation,
        cleanup_deadline,
        cleanup_deadline_sender,
        shutdown_ceiling: shutdown_ceiling.subscribe(),
    })
}

/// Cleanup 成功回执或明确未创建结果才释放 executor 的 staging reservation。
fn release_stage_reservation(
    id: u64,
    lifecycle: &Arc<StdMutex<ExecutorLifecycle>>,
    active_count: &watch::Sender<usize>,
) {
    let mut lifecycle = lock_recover(lifecycle);
    lifecycle.stage_cleanup_reservations.remove(&id);
    active_count.send_replace(tracked_resources(&lifecycle));
}

/// 将 staging 控制命令的确定非零状态转为安全错误，不把缺失退出码解释成成功。
fn stage_command_error(result: &CommandExecutionResult, context: &'static str) -> AppError {
    if result.truncated {
        AppError::infrastructure("容器 staging 控制命令输出不完整")
    } else {
        AppError::infrastructure_with_source(context, AppError::remote_exit(result.exit_code))
    }
}

/// 拒绝不安全的 argv 与读取预算，并只以固定标签诊断原因，不记录命令值或路径。
fn validate_invocation(
    invocation: &CommandInvocation,
    limits: CommandExecutionLimits,
) -> AppResult<()> {
    if invocation.program.trim().is_empty() {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [program_empty]",
        ));
    }
    if invocation.program.chars().any(char::is_control) {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [program_control_character]",
        ));
    }
    if invocation.arguments.len() > 256 {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [argument_count_over_limit]",
        ));
    }
    if invocation
        .arguments
        .iter()
        .any(|value| value.contains('\0'))
    {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [argument_nul_byte]",
        ));
    }
    if invocation.environment.iter().any(|(key, value)| {
        key.is_empty()
            || key.contains('=')
            || key.chars().any(char::is_control)
            || value.chars().any(char::is_control)
    }) {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [environment_invalid]",
        ));
    }
    if invocation
        .working_directory
        .as_ref()
        .is_some_and(|path| path.is_empty() || path.chars().any(char::is_control))
    {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [working_directory_invalid]",
        ));
    }
    if limits.timeout.is_zero() {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [timeout_zero]",
        ));
    }
    if limits.max_output_bytes == 0 {
        return Err(AppError::invalid(
            "运维命令参数或读取预算无效 [output_budget_zero]",
        ));
    }
    let total_chars = invocation
        .program
        .chars()
        .count()
        .saturating_add(
            invocation
                .arguments
                .iter()
                .map(|value| value.chars().count())
                .fold(0usize, usize::saturating_add),
        )
        .saturating_add(
            invocation
                .environment
                .iter()
                .map(|(key, value)| key.chars().count().saturating_add(value.chars().count()))
                .fold(0usize, usize::saturating_add),
        )
        .saturating_add(
            invocation
                .working_directory
                .as_ref()
                .map_or(0, |path| path.chars().count()),
        );
    if total_chars > 64_000 {
        return Err(AppError::invalid(
            "运维命令参数总长度超出限制 [argument_total_length_over_limit]",
        ));
    }
    Ok(())
}

/// 在保留命令主错误的同时把 stream 或 lease 关闭失败挂入诊断链。
fn combine_cleanup<T>(result: AppResult<T>, cleanup: AppResult<()>) -> AppResult<T> {
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error),
        (Err(error), Err(cleanup_error)) => Err(error.with_source(cleanup_error)),
    }
}

/// 不同 stream 的事件由一个循环消费，可直接维护唯一总量预算。
fn append_bounded(
    target: &mut Vec<u8>,
    input: Vec<u8>,
    used: &mut usize,
    max_output_bytes: usize,
    truncated: &mut bool,
) {
    let accepted = max_output_bytes.saturating_sub(*used).min(input.len());
    target.extend_from_slice(&input[..accepted]);
    *used = (*used).saturating_add(accepted);
    *truncated |= accepted != input.len();
}
