//! 运维用例共享的结构化命令端口与服务装配。
//!
//! 本模块不拼 shell 文本；本机与远端命令适配器分别负责进程启动和安全方言编码。
//!
//! @author kongweiguang

mod containers;
mod port_forward;
mod server_info;
mod terminal_proxy;
mod tmux;

pub use containers::{ContainerLogs, ContainerRuntimeService};
pub use port_forward::{
    ManagedPortForwardSnapshot, PortForwardDefinition, PortForwardDefinitionRepository,
    PortForwardRuntime, PortForwardRuntimeHandle, PortForwardService, PortForwardSnapshot,
};
pub use server_info::{ServerInfoCollector, ServerInfoQuery, ServerInfoService, ServerInfoTier};
pub use terminal_proxy::{
    CurrentTerminalProxyRequest, FutureProxyApplyRequest, FutureProxyApplyState,
    FutureProxyAttemptResult, FutureProxyPolicyStore, FutureProxyRetryIntent,
    ProxyScriptExecutionContext, ReviewableProxyScript, TerminalProxyApplication,
    TerminalProxyApplyOutcome, TerminalProxyApplyResult, TerminalProxyReconcileOutcome,
    TerminalProxyReconcileResult, TerminalProxyUnavailableReason,
};
pub use tmux::{TmuxCapability, TmuxCreateRequest, TmuxService};

use std::{
    collections::BTreeMap,
    error::Error,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_domain::{
    files::FileEndpoint,
    identity::{HostId, TransferId},
    operations::ContainerRuntime,
};
use tokio::sync::{Mutex, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

const COMMAND_STREAM_INPUT_CHUNK_MAX_BYTES: usize = 64 * 1024;

use crate::{
    AppError, AppResult, commands::CommandsService, connections::RemoteConnectionService,
    preferences::ProfileRepository, sessions::SessionService,
};

/// 命令执行目标在本地 CLI 和已保存 SSH 主机之间显式区分，不包含凭据或临时连接状态。
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CommandExecutionTarget {
    Local,
    Host(HostId),
}

/// 结构化程序调用；参数保持独立元素，不允许用 shell 命令字符串承载用户输入。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandInvocation {
    pub program: String,
    pub arguments: Vec<String>,
    pub environment: BTreeMap<String, String>,
    pub working_directory: Option<String>,
}

impl CommandInvocation {
    /// 构造不带环境覆盖和工作目录的命令，减少常见只读操作的误配机会。
    pub fn new(program: impl Into<String>, arguments: Vec<String>) -> Self {
        Self {
            program: program.into(),
            arguments,
            environment: BTreeMap::new(),
            working_directory: None,
        }
    }
}

/// 一次命令读取预算；统一限制时长和总输出以防日志、容器清单撑大内存。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandExecutionLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// 命令结果保留退出码和截断状态，调用方不能把未完成或截断的 JSON 当成完整成功。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandExecutionResult {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<i32>,
    pub truncated: bool,
    /// 仅在所有输入字节与 EOF 均收到实际 stdin ACK 后为 true；false 必须按结果未知处理。
    pub input_complete: bool,
}

/// 只转发原始 stdout/stderr 块，不要求调用方先把文件或目录归档完整读入内存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandStreamEvent {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// 命令完成回执保留退出状态与有界 stderr 尾部，长时间运行的 stdout 由事件流消费。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandStreamExit {
    pub exit_code: Option<i32>,
    pub stderr: Vec<u8>,
    pub stderr_truncated: bool,
    /// 输入在调用方要求送达时完整写入并确认 EOF；自然退出与实际输入交付分别报告。
    pub input_complete: bool,
}

/// 有界输入消息携带 ACK，只有 writer 完成真实 I/O 后 producer 才能报告成功。
pub enum CommandStreamInputMessage {
    Data {
        bytes: Vec<u8>,
        acknowledgement: oneshot::Sender<AppResult<()>>,
    },
    Close {
        acknowledgement: oneshot::Sender<AppResult<()>>,
    },
}

/// Worker 在释放在途命令 permit 前分别报告执行结果与资源清理结果。
#[derive(Debug, Clone)]
pub struct CommandStreamCompletion {
    pub execution: AppResult<CommandStreamExit>,
    pub cleanup: AppResult<()>,
}

/// Worker 在完成清理前公开每次失败，允许调用方保留同一资源句柄并提交新 deadline 重试。
#[derive(Debug, Clone)]
pub enum CommandStreamState {
    Running { attempt: u64 },
    CleanupPending { attempt: u64, error: AppError },
    Complete(CommandStreamCompletion),
}

/// 可克隆 stdin 句柄串行化写入与 EOF，且在发送等待期间响应命令取消。
#[derive(Clone)]
pub struct CommandStreamInput {
    sender: mpsc::Sender<CommandStreamInputMessage>,
    closed: Arc<Mutex<bool>>,
    cancellation: CancellationToken,
    delivery: Arc<CommandStreamInputDelivery>,
}

/// 记录本次输入是否曾被要求送达，以及其数据和 EOF 是否得到 writer 回执。
struct CommandStreamInputDelivery {
    required: AtomicBool,
    failed: AtomicBool,
    eof_acknowledged: AtomicBool,
}

/// producer 在 ACK 前被 drop 时记录未确认状态，防止之后的 EOF 把请求误标为完整。
struct InputDeliveryAttempt {
    delivery: Arc<CommandStreamInputDelivery>,
    acknowledged: bool,
}

impl InputDeliveryAttempt {
    /// 每次 data 写入有自己的完成护栏，取消任务不会留下可误报成功的共享状态。
    fn new(delivery: Arc<CommandStreamInputDelivery>) -> Self {
        Self {
            delivery,
            acknowledged: false,
        }
    }

    /// 仅在调用方确实收到 writer ACK 后解除 Drop 时的失败标记。
    fn acknowledge(&mut self) {
        self.acknowledged = true;
    }
}

impl Drop for InputDeliveryAttempt {
    /// producer future 中断意味着没有可证明的交付回执，后续命令结果必须保守处理。
    fn drop(&mut self) {
        if !self.acknowledged {
            self.delivery.failed.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum InputDeliveryStage {
    Data,
    Eof,
}

#[derive(Debug)]
struct InputDeliveryUnconfirmed {
    stage: InputDeliveryStage,
}

impl fmt::Display for InputDeliveryUnconfirmed {
    /// source 仅用于内部诊断，不包含命令参数、输入正文或主机信息。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.stage {
            InputDeliveryStage::Data => formatter.write_str("stdin 数据未收到 writer 确认"),
            InputDeliveryStage::Eof => formatter.write_str("stdin EOF 未收到 writer 确认"),
        }
    }
}

impl Error for InputDeliveryUnconfirmed {}

/// 预先登记的容器 staging 清理范围；运行时与容器身份来自不可变文件端点。
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContainerStageCleanupRequest {
    pub transfer_id: TransferId,
    pub staging_endpoint: FileEndpoint,
}

/// 容器 staging 创建返回的确定结果；冲突表示本次没有创建目标目录。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageCreationResult {
    Created,
    AlreadyExists,
}

/// 只能操作预先绑定到单个传输 ID 和 staging endpoint 的创建/清理许可。
#[async_trait]
pub trait ContainerStageCleanupCapability: Send + Sync + 'static {
    /// 在资源创建前签发的能力内创建唯一 staging 目录和 owner marker。
    async fn create_stage_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> AppResult<StageCreationResult>;

    /// 仅在路径不存在或 marker 确认属于本 transfer 后删除 staging，并复核路径已消失。
    async fn cleanup_until(&self, deadline: tokio::time::Instant) -> AppResult<()>;
}

impl CommandStreamInput {
    /// 构造共用一个 EOF 状态的 writer 副本，避免多个 producer 违反输入顺序。
    pub fn new(
        sender: mpsc::Sender<CommandStreamInputMessage>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            sender,
            closed: Arc::new(Mutex::new(false)),
            cancellation,
            delivery: Arc::new(CommandStreamInputDelivery {
                required: AtomicBool::new(false),
                failed: AtomicBool::new(false),
                eof_acknowledged: AtomicBool::new(false),
            }),
        }
    }

    /// 等待 writer 确认实际写完原始块；通道仅入队不算送达，避免传输层提前报成功。
    pub async fn write(&self, bytes: Vec<u8>) -> AppResult<()> {
        if bytes.len() > COMMAND_STREAM_INPUT_CHUNK_MAX_BYTES {
            return Err(AppError::invalid("运维命令输入块超过安全上限"));
        }
        let closed = self.closed.lock().await;
        if *closed {
            return Err(AppError::conflict("运维命令 stdin 已关闭"));
        }
        if self.delivery.failed.load(Ordering::Acquire) {
            return Err(input_delivery_unconfirmed(InputDeliveryStage::Data));
        }
        self.delivery.required.store(true, Ordering::Release);
        self.delivery
            .eof_acknowledged
            .store(false, Ordering::Release);
        let mut delivery_attempt = InputDeliveryAttempt::new(self.delivery.clone());
        let (acknowledgement, acknowledged) = oneshot::channel();
        tokio::select! {
            _ = self.cancellation.cancelled() => {
                self.delivery.failed.store(true, Ordering::Release);
                Err(AppError::cancelled("运维命令输入已取消"))
            }
            result = self.sender.send(CommandStreamInputMessage::Data { bytes, acknowledgement }) => {
                if result.is_err() {
                    self.delivery.failed.store(true, Ordering::Release);
                    return if self.cancellation.is_cancelled() {
                        Err(AppError::cancelled("运维命令输入已取消"))
                    } else {
                        Err(input_delivery_unconfirmed(InputDeliveryStage::Data))
                    };
                }
                match tokio::select! {
                    _ = self.cancellation.cancelled() => Err(AppError::cancelled("运维命令输入已取消")),
                    acknowledgement = acknowledged => acknowledgement
                        .unwrap_or_else(|_| Err(input_delivery_unconfirmed(InputDeliveryStage::Data))),
                } {
                    Ok(()) => {
                        delivery_attempt.acknowledge();
                        Ok(())
                    }
                    Err(error) => {
                        self.delivery.failed.store(true, Ordering::Release);
                        if self.cancellation.is_cancelled() {
                            Err(AppError::cancelled("运维命令输入已取消"))
                        } else if error.public_code() == crate::PublicErrorCode::Cancelled {
                            Err(input_delivery_unconfirmed(InputDeliveryStage::Data))
                        } else {
                            Err(error)
                        }
                    }
                }
            }
        }
    }

    /// 发送有序 EOF 并等到真实 shutdown 回执；数据存在但 EOF 未确认时结果保持不完整。
    pub async fn close(&self) -> AppResult<()> {
        let mut closed = self.closed.lock().await;
        if *closed {
            return if self.delivery.required.load(Ordering::Acquire)
                && !self.delivery.eof_acknowledged.load(Ordering::Acquire)
            {
                Err(input_delivery_unconfirmed(InputDeliveryStage::Eof))
            } else {
                Ok(())
            };
        }
        if self.cancellation.is_cancelled() {
            *closed = true;
            self.delivery.failed.store(true, Ordering::Release);
            return Err(AppError::cancelled("运维命令 stdin 关闭已取消"));
        }
        *closed = true;
        let (acknowledgement, acknowledged) = oneshot::channel();
        tokio::select! {
            _ = self.cancellation.cancelled() => {
                self.delivery.failed.store(true, Ordering::Release);
                Err(AppError::cancelled("运维命令 stdin 关闭已取消"))
            }
            result = self.sender.send(CommandStreamInputMessage::Close { acknowledgement }) => {
                if result.is_err() {
                    if self.delivery.required.load(Ordering::Acquire) {
                        self.delivery.failed.store(true, Ordering::Release);
                    }
                    return if self.cancellation.is_cancelled() {
                        Err(AppError::cancelled("运维命令 stdin 关闭已取消"))
                    } else {
                        Err(input_delivery_unconfirmed(InputDeliveryStage::Eof))
                    };
                }
                match tokio::select! {
                    _ = self.cancellation.cancelled() => Err(AppError::cancelled("运维命令 stdin 关闭已取消")),
                    acknowledgement = acknowledged => acknowledgement
                        .unwrap_or_else(|_| Err(input_delivery_unconfirmed(InputDeliveryStage::Eof))),
                } {
                    Ok(()) => {
                        self.delivery.eof_acknowledged.store(true, Ordering::Release);
                        Ok(())
                    }
                    Err(error) => {
                        if self.delivery.required.load(Ordering::Acquire) {
                            self.delivery.failed.store(true, Ordering::Release);
                        }
                        if self.cancellation.is_cancelled() {
                            Err(AppError::cancelled("运维命令 stdin 关闭已取消"))
                        } else if error.public_code() == crate::PublicErrorCode::Cancelled {
                            Err(input_delivery_unconfirmed(InputDeliveryStage::Eof))
                        } else {
                            Err(error)
                        }
                    }
                }
            }
        }
    }

    /// 读取可靠输入状态；未申请输入时不把命令自然退出误判成内容缺失。
    pub fn input_complete(&self) -> bool {
        !self.delivery.required.load(Ordering::Acquire)
            || (!self.delivery.failed.load(Ordering::Acquire)
                && self.delivery.eof_acknowledged.load(Ordering::Acquire))
    }
}

/// 为内部状态检查提供稳定标识，调用方据此区分 writer 未确认与用户主动取消。
/// 生成不携带输入正文或命令参数的 typed 未确认错误。
fn input_delivery_unconfirmed(stage: InputDeliveryStage) -> AppError {
    AppError::conflict("运维命令 stdin 未确认完整送达")
        .with_source(InputDeliveryUnconfirmed { stage })
}

/// 拥有输入、输出与 worker completion 的长流句柄；Drop 会请求取消，worker 自己完成回收。
pub struct OperationsCommandStream {
    events: mpsc::Receiver<CommandStreamEvent>,
    input: CommandStreamInput,
    completion: watch::Receiver<CommandStreamState>,
    cancellation: CancellationToken,
    cleanup_deadline: watch::Sender<Option<tokio::time::Instant>>,
    shutdown_ceiling: watch::Receiver<Option<tokio::time::Instant>>,
}

impl OperationsCommandStream {
    /// 仅由基础设施组装边界通道，并保留统一 shutdown ceiling 以限制后续清理请求。
    ///
    /// `cancellation` 必须是当前流的自有子令牌；命令执行器负责从调用方令牌派生它，
    /// 这样 Drop 取消当前流时不会把局部清理动作反向传播成整项传输取消。
    pub fn new(
        events: mpsc::Receiver<CommandStreamEvent>,
        input_sender: mpsc::Sender<CommandStreamInputMessage>,
        completion: watch::Receiver<CommandStreamState>,
        cancellation: CancellationToken,
        cleanup_deadline: watch::Sender<Option<tokio::time::Instant>>,
        shutdown_ceiling: watch::Receiver<Option<tokio::time::Instant>>,
    ) -> Self {
        Self {
            events,
            input: CommandStreamInput::new(input_sender, cancellation.clone()),
            completion,
            cancellation,
            cleanup_deadline,
            shutdown_ceiling,
        }
    }

    /// 克隆输入端以便独立生产者在持续读取 stdout/stderr 时并发回写 stdin。
    pub fn input(&self) -> CommandStreamInput {
        self.input.clone()
    }

    /// 获取下一块原始输出并同时观察清理失败，避免输出队列仍打开时调用方等不到重试信号。
    pub async fn next_event(&mut self) -> AppResult<Option<CommandStreamEvent>> {
        loop {
            let state = { self.completion.borrow_and_update().clone() };
            match state {
                CommandStreamState::CleanupPending { error, .. } => return Err(error),
                // worker 先关闭输出 sender 再发布 Complete，此分支仍 drain 已排队数据。
                CommandStreamState::Complete(_) => return Ok(self.events.recv().await),
                CommandStreamState::Running { .. } => {}
            }
            tokio::select! {
                event = self.events.recv() => return Ok(event),
                changed = self.completion.changed() => {
                    if changed.is_err() {
                        match self.completion.borrow_and_update().clone() {
                            CommandStreamState::Complete(_) => continue,
                            CommandStreamState::CleanupPending { error, .. } => return Err(error),
                            CommandStreamState::Running { .. } => {
                                return Err(AppError::infrastructure("运维命令 worker 状态通道提前关闭"));
                            }
                        }
                    }
                }
            }
        }
    }

    /// 关闭 stdin 并并行 drain 事件与清理状态；清理失败先返回，保留句柄供同 worker 重试。
    pub async fn finish(&mut self) -> AppResult<CommandStreamExit> {
        let input = self.input.clone();
        let mut input_close = Box::pin(input.close());
        let mut input_close_result = None;
        let mut events_drained = false;
        let mut completed_report = None;
        while input_close_result.is_none() || !events_drained {
            let state = { self.completion.borrow_and_update().clone() };
            match state {
                CommandStreamState::CleanupPending { error, .. } => return Err(error),
                CommandStreamState::Complete(report) => completed_report = Some(report),
                CommandStreamState::Running { .. } => {}
            }
            tokio::select! {
                result = &mut input_close, if input_close_result.is_none() => {
                    input_close_result = Some(result);
                }
                event = self.events.recv(), if !events_drained => {
                    if event.is_none() {
                        events_drained = true;
                    }
                }
                changed = self.completion.changed(), if completed_report.is_none() => {
                    if changed.is_err() {
                        match self.completion.borrow_and_update().clone() {
                            CommandStreamState::Complete(report) => completed_report = Some(report),
                            CommandStreamState::CleanupPending { error, .. } => return Err(error),
                            CommandStreamState::Running { .. } => {
                                return Err(AppError::infrastructure("运维命令 worker 状态通道提前关闭"));
                            }
                        }
                    } else {
                        match self.completion.borrow_and_update().clone() {
                            CommandStreamState::Complete(report) => completed_report = Some(report),
                            CommandStreamState::CleanupPending { error, .. } => return Err(error),
                            CommandStreamState::Running { .. } => {}
                        }
                    }
                }
            }
        }
        let completion = match completed_report {
            Some(report) => report,
            None => self.wait_for_report(None, None).await?,
        };
        combine_stream_completion(
            completion,
            input_close_result.unwrap_or(Ok(())),
            self.input.input_complete(),
            self.input.delivery.required.load(Ordering::Acquire),
        )
    }

    /// 取消后停止接收输出并等待资源清理；请求 deadline 受共享 shutdown ceiling 限制。
    pub async fn close_until(&mut self, requested_deadline: tokio::time::Instant) -> AppResult<()> {
        let shutdown_ceiling = { *self.shutdown_ceiling.borrow() };
        let deadline = shutdown_ceiling.map_or(requested_deadline, |ceiling| {
            requested_deadline.min(ceiling)
        });
        let previous_attempt = match &*self.completion.borrow_and_update() {
            CommandStreamState::Running { attempt }
            | CommandStreamState::CleanupPending { attempt, .. } => *attempt,
            CommandStreamState::Complete(report) => return report.cleanup.clone(),
        };
        self.cleanup_deadline.send_replace(Some(deadline));
        self.cancellation.cancel();
        self.events.close();
        let report = tokio::time::timeout_at(
            deadline,
            self.wait_for_report(Some(deadline), Some(previous_attempt)),
        )
        .await
        .map_err(|_| AppError::timeout("运维命令流清理"))??;
        report.cleanup
    }

    /// 只返回本次调用之后的新清理结果；deadline 超时或上次失败都保留 watch receiver。
    async fn wait_for_report(
        &mut self,
        deadline: Option<tokio::time::Instant>,
        after_attempt: Option<u64>,
    ) -> AppResult<CommandStreamCompletion> {
        loop {
            let state = self.completion.borrow_and_update().clone();
            match state {
                CommandStreamState::Complete(report) => return Ok(report),
                CommandStreamState::CleanupPending { attempt, error }
                    if after_attempt.is_none_or(|previous| attempt > previous) =>
                {
                    return Err(error);
                }
                CommandStreamState::Running { .. } | CommandStreamState::CleanupPending { .. } => {}
            }
            match deadline {
                Some(deadline) => {
                    tokio::time::timeout_at(deadline, self.completion.changed())
                        .await
                        .map_err(|_| AppError::timeout("运维命令流清理"))?
                        .map_err(|_| AppError::infrastructure("运维命令 worker 状态通道已关闭"))?;
                }
                None => self
                    .completion
                    .changed()
                    .await
                    .map_err(|_| AppError::infrastructure("运维命令 worker 状态通道已关闭"))?,
            }
        }
    }
}

impl Drop for OperationsCommandStream {
    /// 取消当前流持有的自有子令牌；调用方令牌不得直接传入，避免释放流时反向取消父任务。
    fn drop(&mut self) {
        self.cancellation.cancel();
    }
}

/// 按执行结果优先级保留业务错误，同时附加 worker 清理与 stdin 关闭证据。
fn combine_stream_completion(
    completion: CommandStreamCompletion,
    input_close: AppResult<()>,
    input_complete: bool,
    input_required: bool,
) -> AppResult<CommandStreamExit> {
    let execution = match (completion.execution, input_close) {
        (Ok(mut exit), Ok(())) => {
            exit.input_complete = input_complete;
            Ok(exit)
        }
        (Ok(mut exit), Err(error))
            if !input_complete
                && input_required
                && error.public_code() != crate::PublicErrorCode::Cancelled =>
        {
            exit.input_complete = false;
            Ok(exit)
        }
        (Ok(mut exit), Err(error))
            if !input_required && error.public_code() != crate::PublicErrorCode::Cancelled =>
        {
            // 没有请求 payload 时，子进程先退出可使冗余 EOF 失败；不能盖过真实退出回执。
            exit.input_complete = true;
            Ok(exit)
        }
        (Ok(_), Err(error)) => Err(error),
        (Err(error), Ok(())) => Err(error),
        (Err(operation), Err(close)) => Err(operation.with_source(close)),
    };
    match (execution, completion.cleanup) {
        (Ok(exit), Ok(())) => Ok(exit),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(operation), Err(cleanup)) => Err(operation.with_source(cleanup)),
    }
}

/// 本地 PTY-free command 与远端 exec 共用的端口；实现方必须遵守输出预算并响应取消。
#[async_trait]
pub trait OperationsCommandExecutor: Send + Sync + 'static {
    /// 运行一次非交互命令；远端实现须通过 RemoteConnectionService 执行结构化 argv。
    async fn execute(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult>;

    /// 将有界二进制输入送给命令 stdin；没有流式输入能力的 mock/adapter 明确拒绝写入。
    async fn execute_with_input(
        &self,
        _target: CommandExecutionTarget,
        _invocation: CommandInvocation,
        _input: Vec<u8>,
        _limits: CommandExecutionLimits,
        _cancel: CancellationToken,
    ) -> AppResult<CommandExecutionResult> {
        Err(AppError::unsupported("当前命令执行器不支持 stdin 数据流"))
    }

    /// 以有界双向块流执行命令；实现必须为流与 worker 派生自有取消令牌，不能让流 Drop 取消调用方令牌。
    /// 不具备真实流能力的 adapter 必须明确拒绝而非缓冲伪装。
    async fn execute_stream(
        &self,
        target: CommandExecutionTarget,
        invocation: CommandInvocation,
        limits: CommandExecutionLimits,
        cancel: CancellationToken,
    ) -> AppResult<OperationsCommandStream>;

    /// 在普通命令 intake 关闭前预留单个容器 staging 的安全创建与重试清理能力。
    fn reserve_container_stage_cleanup(
        &self,
        request: ContainerStageCleanupRequest,
    ) -> AppResult<Arc<dyn ContainerStageCleanupCapability>>;

    /// 同步停止普通命令 intake 并取消常规在途执行，已签发的 staging capability 保持可用。
    fn begin_shutdown(&self, deadline: tokio::time::Instant);

    /// 在统一截止时间内等待常规命令与已签发 staging capability 完成资源清理。
    async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()>;
}

/// 聚合容器、tmux、监控等运维用例所需的共享应用端口。
#[derive(Clone)]
pub struct OperationsService {
    pub containers: ContainerRuntimeService,
    pub tmux: TmuxService,
    pub server_info: ServerInfoService,
    pub port_forwards: PortForwardService,
    sessions: Arc<dyn SessionService>,
    future_proxy_policy: FutureProxyPolicyStore,
    terminal_proxy: Option<TerminalProxyApplication>,
    command_executor: Arc<dyn OperationsCommandExecutor>,
}

impl OperationsService {
    /// 注入纯用例端口并建立唯一代理策略存储，避免 profile attach 偷读全局配置或另建会话 registry。
    pub fn new(
        commands: Arc<dyn OperationsCommandExecutor>,
        sessions: Arc<dyn SessionService>,
        connections: Arc<dyn RemoteConnectionService>,
        profiles: Arc<dyn ProfileRepository>,
        server_info_collector: Arc<dyn ServerInfoCollector>,
        forward_runtime: Arc<dyn PortForwardRuntime>,
    ) -> Self {
        let future_proxy_policy = FutureProxyPolicyStore::new();
        let port_forwards = PortForwardService::new(forward_runtime)
            .with_future_proxy_policy_store(future_proxy_policy.clone());
        Self {
            containers: ContainerRuntimeService::new(commands.clone(), sessions.clone()),
            tmux: TmuxService::new(commands.clone(), sessions.clone(), connections, profiles),
            server_info: ServerInfoService::new(server_info_collector),
            port_forwards,
            sessions,
            future_proxy_policy,
            terminal_proxy: None,
            command_executor: commands,
        }
    }

    /// 定义仓储与运行时分离，关闭隧道只回收资源，不丢失用户可再次启动的配置。
    ///
    /// @author kongweiguang
    pub fn with_port_forward_definition_repository(
        mut self,
        repository: Arc<dyn PortForwardDefinitionRepository>,
    ) -> Self {
        self.port_forwards = self.port_forwards.with_definition_repository(repository);
        self
    }

    /// 用既有 commands 与 session service 装配代理用例，确保转发关闭会取消同一策略状态。
    pub fn with_terminal_proxy(mut self, commands: Arc<CommandsService>) -> Self {
        self.terminal_proxy = Some(TerminalProxyApplication::new(
            commands,
            self.sessions.clone(),
            self.port_forwards.clone(),
            self.future_proxy_policy.clone(),
        ));
        self
    }

    /// 返回供 UI 与组合根共享的同一份进程内后续代理策略状态。
    pub fn future_proxy_policy_store(&self) -> FutureProxyPolicyStore {
        self.future_proxy_policy.clone()
    }

    /// 返回已装配的代理用例句柄；未提供 CommandsService 时保持显式不可用。
    pub fn terminal_proxy(&self) -> Option<TerminalProxyApplication> {
        self.terminal_proxy.clone()
    }

    /// 用同一个总截止时间关闭转发与命令，任一子系统失败都不跳过另一侧清理。
    pub async fn shutdown(&self, timeout: Duration) -> AppResult<()> {
        if timeout.is_zero() {
            return Err(AppError::invalid("运维退出清理时限必须大于零"));
        }
        self.shutdown_until(tokio::time::Instant::now() + timeout)
            .await
    }

    /// 在组合根并行 join 各服务前关闭普通命令/转发入口，保留已签发的 staging cleanup 能力。
    pub fn begin_shutdown(&self, deadline: tokio::time::Instant) {
        self.port_forwards.begin_shutdown(deadline);
        self.command_executor.begin_shutdown(deadline);
    }

    /// 接受 composition root 提供的共享绝对截止时间，避免各子系统各自延长退出预算。
    pub async fn shutdown_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.begin_shutdown(deadline);
        let (forward_result, command_result) = tokio::join!(
            self.port_forwards.shutdown_until(deadline),
            self.command_executor.shutdown_until(deadline),
        );
        match (forward_result, command_result) {
            (Ok(()), Ok(())) => Ok(()),
            (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
            (Err(forward), Err(command)) => Err(AppError::infrastructure_with_source(
                "多个运维模块退出清理失败",
                OperationShutdownFailure { forward, command },
            )),
        }
    }
}

/// 把取消、超时和命令执行统一收口，确保超时会取消底层独立 channel。
pub(crate) async fn execute_with_deadline(
    executor: &dyn OperationsCommandExecutor,
    target: CommandExecutionTarget,
    invocation: CommandInvocation,
    limits: CommandExecutionLimits,
    cancel: CancellationToken,
) -> AppResult<CommandExecutionResult> {
    if limits.timeout.is_zero() || limits.max_output_bytes == 0 {
        return Err(AppError::invalid("运维命令预算必须大于零"));
    }
    if cancel.is_cancelled() {
        return Err(AppError::cancelled("运维命令已取消"));
    }
    let operation_cancel = cancel.child_token();
    let mut execution =
        Box::pin(executor.execute(target, invocation, limits, operation_cancel.clone()));
    let deadline = tokio::time::Instant::now() + limits.timeout;
    tokio::select! {
        _ = cancel.cancelled() => {
            operation_cancel.cancel();
            match wait_for_command_cleanup(&mut execution).await {
                Ok(()) => Err(AppError::cancelled("运维命令已取消")),
                Err(cleanup_error) => Err(
                    AppError::cancelled("运维命令已取消").with_source(cleanup_error),
                ),
            }
        }
        _ = tokio::time::sleep_until(deadline) => {
            operation_cancel.cancel();
            match wait_for_command_cleanup(&mut execution).await {
                Ok(()) => Err(AppError::timeout("运维命令")),
                Err(cleanup_error) => {
                    Err(AppError::timeout("运维命令").with_source(cleanup_error))
                }
            }
        }
        result = &mut execution => result,
    }
}

/// 发出取消后持续驱动 executor 收尾，不能因清理超时而丢弃子进程 reap 或 SSH channel close。
async fn wait_for_command_cleanup<F, T>(execution: &mut F) -> AppResult<()>
where
    F: std::future::Future<Output = AppResult<T>> + Unpin,
{
    execution.await.map(|_| ())
}

/// 同时保留转发与命令两侧清理错误，便于内部诊断继续追踪两个失败源。
#[derive(Debug)]
struct OperationShutdownFailure {
    forward: AppError,
    command: AppError,
}

impl fmt::Display for OperationShutdownFailure {
    /// 对外只显示固定摘要；具体子系统错误通过 source 供内部诊断读取。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "端口转发退出清理失败：{}；运维命令退出清理失败：{}",
            self.forward.public_message(),
            self.command.public_message(),
        )
    }
}

impl Error for OperationShutdownFailure {}

/// 把运行时枚举映射为固定可执行名，不把配置文件字符串当成 program。
pub(crate) fn runtime_program(runtime: ContainerRuntime) -> &'static str {
    match runtime {
        ContainerRuntime::Docker => "docker",
        ContainerRuntime::Podman => "podman",
    }
}
