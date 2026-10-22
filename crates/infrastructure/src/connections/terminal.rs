//! SSH PTY channel 到唯一 SessionService transport 的适配。
//!
//! @author kongweiguang

use std::{
    collections::VecDeque,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, ExecRequest, LeasedSessionTransportControl, RemoteConnectionLease,
        RemoteConnectionService, RemoteShellDialect, RemoteShellKind, TerminalOpenRequest,
    },
    sessions::{
        SessionOpenCleanupOwner, SessionTransport, SessionTransportControl,
        SessionTransportFactory, SessionTransportOpenError,
    },
};
use kerminal_domain::sessions::{
    SessionExitStatus, SessionTarget, SshInitialCommand, SshSessionTarget, TerminalSize,
};
use russh::{ChannelMsg, ChannelReadHalf, ChannelWriteHalf, client::Msg};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex, mpsc, watch},
};
use tokio_util::sync::CancellationToken;

use super::{
    exec::{encode_exec_command, encode_terminal_shell_command},
    ssh::{RusshPeer, russh_error},
};

const MAX_SETUP_OUTPUT_BYTES: usize = 1024 * 1024;

/// 为共享 SSH connection service 提供 SessionService 使用的 PTY transport。
pub struct SshSessionTransportFactory {
    connections: Arc<dyn RemoteConnectionService>,
    hosts: Option<kerminal_application::connections::management::HostManagementService>,
}

impl SshSessionTransportFactory {
    /// 注入统一连接端口，避免 terminal factory 另建 SSH 栈或 session registry。
    ///
    /// @author kongweiguang
    pub fn new(connections: Arc<dyn RemoteConnectionService>) -> Self {
        Self {
            connections,
            hosts: None,
        }
    }

    /// 让首次启动读取主机默认项，显式恢复目录仍优先；不把 UI 配置复制进终端领域目标。
    ///
    /// @author kongweiguang
    pub fn with_host_management(
        mut self,
        hosts: kerminal_application::connections::management::HostManagementService,
    ) -> Self {
        self.hosts = Some(hosts);
        self
    }
}

#[async_trait]
impl SessionTransportFactory for SshSessionTransportFactory {
    /// 只处理 SSH session target；连接服务在读凭据和联网前拒绝 SFTP-only 主机。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Ssh(target) = target else {
            return Err(AppError::unsupported("SSH transport 不支持此会话目标").into());
        };
        let options = if let Some(hosts) = &self.hosts {
            // 外部临时目标没有持久主机默认值；实际目标仍由 connect 原样验证，绝不换投其它主机。
            hosts
                .load_host(&target.host_id)
                .await?
                .map(|host| host.terminal_options)
                .unwrap_or_default()
        } else {
            Default::default()
        };
        open_ssh_target(&self.connections, target, size, cancel, options).await
    }
}

/// 建立本次专属 SSH lease，并把它与 PTY channel 的清理入口绑定。
///
/// @author kongweiguang
async fn open_ssh_target(
    connections: &Arc<dyn RemoteConnectionService>,
    target: &SshSessionTarget,
    size: TerminalSize,
    cancel: CancellationToken,
    options: kerminal_application::connections::management::HostTerminalOptions,
) -> Result<SessionTransport, SessionTransportOpenError> {
    let lease = connections
        .connect(
            target.host_id.clone(),
            ConnectionIntent::Terminal,
            cancel.clone(),
        )
        .await?;
    let transport_result = lease
        .open_terminal(ssh_terminal_open_request(target, size, options), cancel)
        .await;
    let mut transport = match transport_result {
        Ok(transport) => transport,
        Err(error) => return Err(close_failed_ssh_lease(lease, error).await),
    };
    let channel_control = Arc::clone(&transport.control);
    transport.control = Arc::new(LeasedSessionTransportControl::new(
        channel_control,
        lease,
        "SSH terminal session closed",
    ));
    Ok(transport)
}

/// 显式启动/恢复目录优先于主机默认；集中纯投影以防重连和首开采用两套解释规则。
///
/// @author kongweiguang
fn ssh_terminal_open_request(
    target: &SshSessionTarget,
    size: TerminalSize,
    options: kerminal_application::connections::management::HostTerminalOptions,
) -> TerminalOpenRequest {
    let (command, program, args) = match target.command.as_ref() {
        None => (None, None, Vec::new()),
        Some(SshInitialCommand::Shell(command)) => (Some(command.clone()), None, Vec::new()),
        Some(SshInitialCommand::Process { program, args }) => {
            (None, Some(program.clone()), args.clone())
        }
    };
    TerminalOpenRequest {
        term: options.term.unwrap_or_else(|| "xterm-256color".to_owned()),
        size,
        environment: Default::default(),
        command,
        program,
        args,
        working_directory: target
            .working_directory
            .as_ref()
            .map(|path| path.to_string_lossy().into_owned())
            .or(options.initial_working_directory),
    }
}

/// PTY channel 创建失败或取消时，在返回前释放这次独占 lease 并保留原错误分类。
///
/// @author kongweiguang
async fn close_failed_ssh_lease(
    lease: RemoteConnectionLease,
    error: AppError,
) -> SessionTransportOpenError {
    match lease
        .close_until(
            tokio::time::Instant::now() + Duration::from_secs(5),
            "SSH terminal setup failed",
        )
        .await
    {
        Ok(()) => SessionTransportOpenError::new(error),
        Err(cleanup_error) => SessionTransportOpenError::with_cleanup(
            error.with_source(cleanup_error),
            Arc::new(SshLeaseCleanupOwner { lease }),
        ),
    }
}

/// 保留尚未成功关闭的独占 SSH lease，供 SessionManager 在原 open 错误后续重试清理。
///
/// @author kongweiguang
struct SshLeaseCleanupOwner {
    lease: RemoteConnectionLease,
}

#[async_trait]
impl SessionOpenCleanupOwner for SshLeaseCleanupOwner {
    /// 只重试本次 SSH lease 的释放，不合成 PTY 退出状态，也不影响其它 pane。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.lease
            .close_until(deadline, "SSH terminal setup cleanup")
            .await
    }
}

/// 把一次 SSH PTY channel 的 bytes 和退出消息通过有界队列送给终端会话。
///
/// 默认交互 shell 通过 exec channel 启动，确保启动脚本和 cwd 不进入可能尚处于 canonical
/// 模式的 PTY stdin；只有服务端确认 exec request 后才公开 integration 启动状态。
/// channel reader、writer adapter 与 control 共享同一个终止 proof，避免在 session manager 引入 SSH 特例。
///
/// @author kongweiguang
pub(super) async fn make_ssh_terminal_transport(
    mut channel: russh::Channel<Msg>,
    request: TerminalOpenRequest,
    dialect: RemoteShellDialect,
    shell_kind: RemoteShellKind,
    request_timeout: Duration,
    peer: Arc<RusshPeer>,
    cancel: CancellationToken,
) -> AppResult<SessionTransport> {
    validate_terminal_command(&request)?;
    let mut initial_messages = VecDeque::new();
    let mut shell_integration_enabled = false;
    let setup = async {
        send_channel_request(
            channel.request_pty(
                true,
                &request.term,
                u32::from(request.size.columns()),
                u32::from(request.size.rows()),
                u32::from(request.size.pixel_width()),
                u32::from(request.size.pixel_height()),
                &[],
            ),
            request_timeout,
            "SSH PTY 请求发送失败",
        )
        .await?;
        await_channel_request_reply(
            &mut channel,
            &mut initial_messages,
            "远端拒绝 SSH PTY 请求",
            &cancel,
            request_timeout,
        )
        .await?;

        if let Some(program) = &request.program {
            let encoded = encode_exec_command(
                &ExecRequest {
                    program: program.clone(),
                    args: request.args.clone(),
                    environment: request.environment.clone(),
                    working_directory: request.working_directory.clone(),
                },
                dialect,
            )
            .map_err(exec_encoding_error)?;
            send_channel_request(
                channel.exec(true, encoded.into_bytes()),
                request_timeout,
                "SSH 终端程序请求发送失败",
            )
            .await?;
            await_channel_request_reply(
                &mut channel,
                &mut initial_messages,
                "远端拒绝 SSH exec 请求",
                &cancel,
                request_timeout,
            )
            .await?;
        } else if request.command.is_some() {
            let command = encode_terminal_shell_command(
                request.command.as_deref(),
                request.working_directory.as_deref(),
                dialect,
            )
            .map_err(exec_encoding_error)?
            .ok_or_else(|| AppError::invalid("SSH terminal command was not encoded"))?;
            for (name, value) in &request.environment {
                send_channel_request(
                    channel.set_env(false, name, value),
                    request_timeout,
                    "SSH 终端环境变量请求发送失败",
                )
                .await?;
            }
            send_channel_request(
                channel.exec(true, command.into_bytes()),
                request_timeout,
                "SSH 终端命令请求发送失败",
            )
            .await?;
            await_channel_request_reply(
                &mut channel,
                &mut initial_messages,
                "远端拒绝 SSH exec 请求",
                &cancel,
                request_timeout,
            )
            .await?;
        } else {
            let startup = crate::shell_integration::remote_interactive_startup(
                shell_kind,
                dialect,
                request.working_directory.as_deref(),
            )?;
            for (name, value) in &request.environment {
                send_channel_request(
                    channel.set_env(false, name, value),
                    request_timeout,
                    "SSH 终端环境变量请求发送失败",
                )
                .await?;
            }
            if let Some(startup) = startup {
                let startup_integration_enabled = startup.shell_integration_enabled;
                send_channel_request(
                    channel.exec(true, startup.exec_command.into_bytes()),
                    request_timeout,
                    "SSH interactive shell exec request send failed",
                )
                .await?;
                await_channel_request_reply(
                    &mut channel,
                    &mut initial_messages,
                    "远端拒绝 SSH interactive shell exec request",
                    &cancel,
                    request_timeout,
                )
                .await?;
                shell_integration_enabled = startup_integration_enabled;
            } else {
                send_channel_request(
                    channel.request_shell(true),
                    request_timeout,
                    "SSH shell 请求发送失败",
                )
                .await?;
                await_channel_request_reply(
                    &mut channel,
                    &mut initial_messages,
                    "远端拒绝 SSH shell 请求",
                    &cancel,
                    request_timeout,
                )
                .await?;
            }
        }
        Ok::<_, AppError>(())
    };
    let setup_result = tokio::select! {
        _ = cancel.cancelled() => Err(AppError::cancelled("SSH 终端通道已取消")),
        result = setup => result,
    };
    if let Err(error) = setup_result {
        let _ = tokio::time::timeout(Duration::from_secs(5), channel.close()).await;
        return Err(error);
    }
    let (reader, writer) = channel.split();
    let write_half = Arc::new(writer);
    let remote_channel_end_confirmed = Arc::new(AtomicBool::new(false));
    let channel_writer = SshChannelWriter {
        inner: write_half.make_writer(),
        remote_channel_end_confirmed: Arc::clone(&remote_channel_end_confirmed),
    };
    let (output_tx, output_rx) = mpsc::channel(32);
    let (exit_tx, exit_rx) = watch::channel(None);
    let pump_cancel = CancellationToken::new();
    let local_close_requested = Arc::new(AtomicBool::new(false));
    let close_sent = Arc::new(Mutex::new(false));
    let reader_task = spawn_ssh_terminal_reader(
        reader,
        SshTerminalReaderContext {
            output: output_tx,
            exit: exit_tx,
            writer: Arc::clone(&write_half),
            close_sent: Arc::clone(&close_sent),
            cancel: pump_cancel.clone(),
            local_close_requested: Arc::clone(&local_close_requested),
            remote_channel_end_confirmed: Arc::clone(&remote_channel_end_confirmed),
            initial_messages,
        },
    );

    let reader = SshChannelReader {
        receiver: output_rx,
        current: Vec::new(),
        offset: 0,
    };
    let control = SshSessionTransportControl {
        write_half,
        exit_receiver: Mutex::new(exit_rx),
        reader_task: Mutex::new(Some(reader_task)),
        close_lock: Mutex::new(()),
        close_sent,
        local_close_requested,
        remote_channel_end_confirmed,
        cancel: pump_cancel,
        _peer: peer,
    };
    Ok(SessionTransport::new(reader, channel_writer, control)
        .with_shell_dialect(dialect)
        .with_shell_integration_enabled(shell_integration_enabled))
}

/// 把已结束的 exact SSH channel 的 shutdown 收口为幂等成功，同时保留普通写入的真实错误。
///
/// @author kongweiguang
struct SshChannelWriter<W> {
    inner: W,
    remote_channel_end_confirmed: Arc<AtomicBool>,
}

impl<W> AsyncWrite for SshChannelWriter<W>
where
    W: AsyncWrite + Unpin,
{
    /// 写入仍完整交给 russh；channel end proof 只影响 shutdown，不伪装可写或重放输入。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buffer)
    }

    /// flush 保持底层 channel 语义；只有写半部收口可由 exact channel end proof 短路。
    ///
    /// @author kongweiguang
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    /// 已确认对端结束后不再向死 channel 发送 EOF；无证明时照常向 russh 传播失败。
    ///
    /// @author kongweiguang
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.remote_channel_end_confirmed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut this.inner).poll_shutdown(cx)
    }
}

/// 为 Russh channel request 的写入也设上界，防止共享 SSH 发送队列阻塞创建流程。
async fn send_channel_request(
    request: impl std::future::Future<Output = Result<(), russh::Error>>,
    timeout: Duration,
    context: &'static str,
) -> AppResult<()> {
    match tokio::time::timeout(timeout, request).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(russh_error(context, error)),
        Err(_) => Err(AppError::timeout("SSH channel request send")),
    }
}

/// 等待 SSH channel request 的真实确认，并暂存早于确认到达的输出以保持字节顺序。
async fn await_channel_request_reply(
    channel: &mut russh::Channel<Msg>,
    buffered: &mut VecDeque<ChannelMsg>,
    rejection_context: &'static str,
    cancel: &CancellationToken,
    timeout: Duration,
) -> AppResult<()> {
    let mut buffered_bytes = 0usize;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(AppError::timeout("SSH PTY request confirmation"));
        }
        let message = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled("SSH 终端请求已取消")),
            result = tokio::time::timeout(remaining, channel.wait()) => match result {
                Ok(message) => message,
                Err(_) => return Err(AppError::timeout("SSH PTY request confirmation")),
            },
        };
        match message {
            Some(ChannelMsg::Success) => return Ok(()),
            Some(ChannelMsg::Failure) => return Err(AppError::unsupported(rejection_context)),
            Some(
                message @ (ChannelMsg::Data { .. }
                | ChannelMsg::ExtendedData { .. }
                | ChannelMsg::ExitStatus { .. }
                | ChannelMsg::ExitSignal { .. }
                | ChannelMsg::Eof
                | ChannelMsg::Close),
            ) => {
                let message_size = match &message {
                    ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => data.len(),
                    _ => 0,
                };
                buffered_bytes = buffered_bytes.saturating_add(message_size);
                if buffered_bytes > MAX_SETUP_OUTPUT_BYTES {
                    return Err(AppError::infrastructure(
                        "SSH PTY output exceeded the setup buffer limit",
                    ));
                }
                buffered.push_back(message);
            }
            Some(_) => {}
            None => return Err(AppError::infrastructure("SSH channel 在请求确认前关闭")),
        }
    }
}

/// 检查 raw command 与结构化程序启动字段是否互斥，避免解释器语义含糊。
fn validate_terminal_command(request: &TerminalOpenRequest) -> AppResult<()> {
    if request.command.is_some() && (request.program.is_some() || !request.args.is_empty()) {
        return Err(AppError::invalid(
            "终端 shell 命令不能与结构化程序参数同时提供",
        ));
    }
    if request.program.is_none() && !request.args.is_empty() {
        return Err(AppError::invalid("结构化终端参数缺少程序"));
    }
    if request
        .command
        .as_ref()
        .is_some_and(|command| command.contains('\0'))
    {
        return Err(AppError::invalid("终端命令包含无效字符"));
    }
    Ok(())
}

/// 把编码错误映射成不含用户命令和路径的稳定应用错误。
fn exec_encoding_error(error: super::exec::ExecEncodingError) -> AppError {
    match error {
        super::exec::ExecEncodingError::UnknownShellDialect => {
            AppError::unsupported("目标 shell 方言未知，无法安全执行")
        }
        super::exec::ExecEncodingError::InvalidEnvironmentName
        | super::exec::ExecEncodingError::NulByte
        | super::exec::ExecEncodingError::EmptyProgram
        | super::exec::ExecEncodingError::OptionLikeProgram => {
            AppError::invalid("结构化终端命令参数无效")
        }
    }
}

/// 描述读泵结束的来源，使本地收尾与远端异常断线保持不同错误分类。
#[derive(Clone, Copy, Debug)]
enum SshPtyReaderEnd {
    Cancelled,
    OutputConsumerClosed,
    RemoteChannelClosed,
    RemoteTransportEnded,
}

/// 将远端 PTY 关闭原因保留为内部 typed source，供内部诊断而不污染稳定错误文案。
#[derive(Debug)]
struct SshPtyDisconnectSource {
    kind: SshPtyReaderEnd,
}

impl std::fmt::Display for SshPtyDisconnectSource {
    /// 错误链只描述 channel 结束类别，不包含主机、命令或认证数据。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self.kind {
            SshPtyReaderEnd::RemoteChannelClosed => "SSH peer closed PTY without exit status",
            SshPtyReaderEnd::RemoteTransportEnded => "SSH transport ended before PTY exit status",
            SshPtyReaderEnd::Cancelled | SshPtyReaderEnd::OutputConsumerClosed => {
                "SSH PTY did not end through a remote disconnect"
            }
        })
    }
}

impl std::error::Error for SshPtyDisconnectSource {}

/// 只把无退出消息的远端关闭标记为可重连；本地 close/cancel 和消费端收尾保持非重试。
///
/// @author kongweiguang
fn ssh_terminal_reader_end_error(
    end: SshPtyReaderEnd,
    local_close_requested: bool,
    cancelled: bool,
) -> AppError {
    if local_close_requested || cancelled || matches!(end, SshPtyReaderEnd::Cancelled) {
        return AppError::cancelled("SSH PTY 会话已关闭或取消");
    }
    match end {
        SshPtyReaderEnd::RemoteChannelClosed | SshPtyReaderEnd::RemoteTransportEnded => {
            AppError::retryable_remote_disconnect(SshPtyDisconnectSource { kind: end })
        }
        SshPtyReaderEnd::OutputConsumerClosed => AppError::cancelled("SSH PTY 输出消费者已关闭"),
        SshPtyReaderEnd::Cancelled => AppError::cancelled("SSH PTY 会话已关闭或取消"),
    }
}

/// 单个 PTY 读泵的有界通道与退出、取消、关闭状态，作为一个任务上下文交接。
///
/// @author kongweiguang
struct SshTerminalReaderContext {
    output: mpsc::Sender<Vec<u8>>,
    exit: watch::Sender<Option<AppResult<SessionExitStatus>>>,
    writer: Arc<ChannelWriteHalf<Msg>>,
    close_sent: Arc<Mutex<bool>>,
    cancel: CancellationToken,
    local_close_requested: Arc<AtomicBool>,
    remote_channel_end_confirmed: Arc<AtomicBool>,
    initial_messages: VecDeque<ChannelMsg>,
}

/// 用 channel 消息构造 PTY 读泵；队列满时暂停消费 SSH 窗口以实现端到端背压。
/// 将任务上下文整体移入后台读取任务；只有真实的 Channel Close/transport end 才发布精确收尾证明，
/// ExitStatus/ExitSignal 仍单独保留为退出结果。
///
/// @author kongweiguang
fn spawn_ssh_terminal_reader(
    mut channel: ChannelReadHalf,
    context: SshTerminalReaderContext,
) -> tokio::task::JoinHandle<()> {
    let SshTerminalReaderContext {
        output,
        exit,
        writer,
        close_sent,
        cancel,
        local_close_requested,
        remote_channel_end_confirmed,
        initial_messages,
    } = context;
    tokio::spawn(async move {
        let mut initial_messages = initial_messages;
        let mut observed_exit = None;
        let end = loop {
            let message = if let Some(message) = initial_messages.pop_front() {
                Some(message)
            } else {
                tokio::select! {
                    _ = cancel.cancelled() => break SshPtyReaderEnd::Cancelled,
                    message = channel.wait() => message,
                }
            };
            match message {
                Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                    match tokio::select! {
                        _ = cancel.cancelled() => break SshPtyReaderEnd::Cancelled,
                        result = output.send(data.to_vec()) => result,
                    } {
                        Ok(()) => {}
                        Err(_) => {
                            let _ = close_ssh_channel_writer_until(
                                &writer,
                                &close_sent,
                                tokio::time::Instant::now() + Duration::from_secs(5),
                            )
                            .await;
                            break SshPtyReaderEnd::OutputConsumerClosed;
                        }
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    let code = i32::try_from(exit_status).ok();
                    let status = SessionExitStatus {
                        code,
                        signal: None,
                        success: code == Some(0),
                    };
                    observed_exit = Some(status.clone());
                    let _ = exit.send(Some(Ok(status)));
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    let status = SessionExitStatus {
                        code: None,
                        signal: Some(format!("{signal_name:?}")),
                        success: false,
                    };
                    observed_exit = Some(status.clone());
                    let _ = exit.send(Some(Ok(status)));
                }
                Some(ChannelMsg::Eof) => continue,
                Some(ChannelMsg::Close) => break SshPtyReaderEnd::RemoteChannelClosed,
                None => break SshPtyReaderEnd::RemoteTransportEnded,
                _ => {}
            }
        };
        if matches!(
            end,
            SshPtyReaderEnd::RemoteChannelClosed | SshPtyReaderEnd::RemoteTransportEnded
        ) {
            // This is the actual end of this reader's channel, independent of whether it sent an exit status.
            remote_channel_end_confirmed.store(true, Ordering::Release);
        }
        if observed_exit.is_none() {
            let error = ssh_terminal_reader_end_error(
                end,
                local_close_requested.load(Ordering::Acquire),
                cancel.is_cancelled(),
            );
            let _ = exit.send(Some(Err(error)));
        }
        drop(output);
    })
}

/// 以异步读取接口消费有界 PTY 消息队列，不把终端输出缓存到无界集合。
struct SshChannelReader {
    receiver: mpsc::Receiver<Vec<u8>>,
    current: Vec<u8>,
    offset: usize,
}

impl AsyncRead for SshChannelReader {
    /// 将最多填满当前 ReadBuf 的字节复制到终端 engine，未消费部分保留供下次 poll。
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }
        loop {
            if self.offset < self.current.len() {
                let count = output.remaining().min(self.current.len() - self.offset);
                output.put_slice(&self.current[self.offset..self.offset + count]);
                self.offset += count;
                return Poll::Ready(Ok(()));
            }
            match self.receiver.poll_recv(cx) {
                Poll::Ready(Some(bytes)) => {
                    self.current = bytes;
                    self.offset = 0;
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// PTY 控制面只操作当前 channel；关闭它不会断开复用的 SSH parent connection。
struct SshSessionTransportControl {
    write_half: Arc<ChannelWriteHalf<Msg>>,
    exit_receiver: Mutex<watch::Receiver<Option<AppResult<SessionExitStatus>>>>,
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    close_lock: Mutex<()>,
    close_sent: Arc<Mutex<bool>>,
    local_close_requested: Arc<AtomicBool>,
    remote_channel_end_confirmed: Arc<AtomicBool>,
    cancel: CancellationToken,
    _peer: Arc<RusshPeer>,
}

#[async_trait]
impl SessionTransportControl for SshSessionTransportControl {
    /// 把终端网格变化发送给远端 PTY，像素尺寸允许为零。
    async fn resize(&self, size: TerminalSize) -> AppResult<()> {
        self.write_half
            .window_change(
                u32::from(size.columns()),
                u32::from(size.rows()),
                u32::from(size.pixel_width()),
                u32::from(size.pixel_height()),
            )
            .await
            .map_err(|error| russh_error("SSH PTY resize 失败", error))
    }

    /// 共用 SessionService 的绝对 deadline；只有同一 reader 确认 channel end 后才跳过 Close，
    /// 仍须 join 读泵，外层 leased control 仍须取得 parent lease 的清理 ACK。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.local_close_requested.store(true, Ordering::Release);
        if tokio::time::Instant::now() >= deadline {
            self.cancel.cancel();
            return Err(AppError::timeout("SSH PTY channel close deadline"));
        }
        let _close_guard = match tokio::time::timeout_at(deadline, self.close_lock.lock()).await {
            Ok(guard) => guard,
            Err(_) => {
                self.cancel.cancel();
                return Err(AppError::timeout("SSH PTY channel close lock"));
            }
        };
        let mut reader_task = match tokio::time::timeout_at(deadline, self.reader_task.lock()).await
        {
            Ok(task) => task,
            Err(_) => {
                self.cancel.cancel();
                return Err(AppError::timeout("SSH PTY reader task lock"));
            }
        };
        if reader_task.is_none() {
            return Ok(());
        }
        if !self.remote_channel_end_confirmed.load(Ordering::Acquire)
            && let Err(error) =
                close_ssh_channel_writer_until(&self.write_half, &self.close_sent, deadline).await
        {
            self.cancel.cancel();
            return Err(error);
        }
        let join_result = match reader_task.as_mut() {
            Some(task) => tokio::time::timeout_at(deadline, task).await,
            None => return Ok(()),
        };
        match join_result {
            Ok(Ok(())) => {
                reader_task.take();
                Ok(())
            }
            Ok(Err(error)) => {
                reader_task.take();
                Err(AppError::infrastructure_with_source(
                    "SSH PTY reader task failed",
                    error,
                ))
            }
            Err(_) => {
                self.cancel.cancel();
                Err(AppError::timeout("SSH PTY reader shutdown"))
            }
        }
    }

    /// 返回真实退出状态或 reader 已判定的断线/取消错误，不伪造退出码。
    ///
    /// @author kongweiguang
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        let mut receiver = self.exit_receiver.lock().await;
        loop {
            if let Some(status) = receiver.borrow().clone() {
                return status;
            }
            if receiver.changed().await.is_err() {
                return Err(AppError::infrastructure(
                    "SSH PTY 读泵结束但没有发布退出结果",
                ));
            }
        }
    }
}

/// 只在 SSH channel writer 确认接受 Close 后置位，deadline 超时允许安全重试同一操作。
async fn close_ssh_channel_writer_until(
    writer: &ChannelWriteHalf<Msg>,
    close_sent: &Mutex<bool>,
    deadline: tokio::time::Instant,
) -> AppResult<()> {
    let mut close_sent = match tokio::time::timeout_at(deadline, close_sent.lock()).await {
        Ok(state) => state,
        Err(_) => return Err(AppError::timeout("SSH PTY close state lock")),
    };
    if *close_sent {
        return Ok(());
    }
    match tokio::time::timeout_at(deadline, writer.close()).await {
        Ok(Ok(())) => {
            *close_sent = true;
            Ok(())
        }
        Ok(Err(error)) => Err(russh_error("SSH PTY channel 关闭失败", error)),
        Err(_) => Err(AppError::timeout("SSH PTY channel close")),
    }
}

#[cfg(test)]
#[path = "../../tests/unit/ssh_terminal_options.rs"]
mod host_options_tests;

#[cfg(test)]
#[path = "../../tests/unit/ssh_channel_writer.rs"]
mod ssh_channel_writer_tests;
