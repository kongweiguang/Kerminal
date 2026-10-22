//! Telnet TCP transport 与 RFC 854 协议适配。
//!
//! @author kongweiguang

use std::time::Duration;

use async_trait::async_trait;
use justelnet_core::{Core, Event, OptionPolicy};
use kerminal_application::{
    AppError, AppResult,
    sessions::{
        SessionTransport, SessionTransportControl, SessionTransportFactory,
        SessionTransportOpenError,
    },
};
use kerminal_domain::sessions::{
    SessionExitStatus, SessionTarget, TelnetSessionTarget, TerminalSize,
};
use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, tcp::OwnedWriteHalf},
    sync::{Mutex, watch},
    task::JoinHandle,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const TELNET_READ_BUFFER_BYTES: usize = 16 * 1024;

/// Telnet 连接超时和会话缓冲预算由 composition root 显式配置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TelnetRuntimeConfig {
    pub connect_timeout: Duration,
    pub channel_buffer_bytes: usize,
}

impl Default for TelnetRuntimeConfig {
    /// 给连接建立设置有界等待，并以固定大小 duplex 通道控制终端背压。
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(10),
            channel_buffer_bytes: 256 * 1024,
        }
    }
}

/// 使用 Tokio TCP 和 Sans-IO Telnet core 建立独立远端会话。
pub struct TelnetSessionTransportFactory {
    config: TelnetRuntimeConfig,
}

impl TelnetSessionTransportFactory {
    /// 在构造期拒绝无效资源预算，避免每个连接重复处理同一装配错误。
    pub fn new(config: TelnetRuntimeConfig) -> AppResult<Self> {
        if config.connect_timeout.is_zero() {
            return Err(AppError::invalid("Telnet 连接超时必须大于零"));
        }
        if config.channel_buffer_bytes == 0 {
            return Err(AppError::invalid("Telnet 通道缓冲容量必须大于零"));
        }
        Ok(Self { config })
    }
}

#[async_trait]
impl SessionTransportFactory for TelnetSessionTransportFactory {
    /// 取消只作用于本次 connect；成功返回后，连接由 SessionTransportControl 独立拥有。
    ///
    /// @author kongweiguang
    async fn open(
        &self,
        target: &SessionTarget,
        size: TerminalSize,
        cancel: CancellationToken,
    ) -> Result<SessionTransport, SessionTransportOpenError> {
        let SessionTarget::Telnet(target) = target else {
            return Err(AppError::invalid("Telnet transport 收到了非 Telnet 目标").into());
        };
        validate_telnet_target(target)?;
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("Telnet 连接已取消").into());
        }

        let address = (target.host.as_str(), target.port);
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::cancelled("Telnet 连接已取消").into()),
            result = timeout(self.config.connect_timeout, TcpStream::connect(address)) => {
                match result {
                    Ok(Ok(stream)) => stream,
                    Ok(Err(error)) => {
                        return Err(AppError::infrastructure_with_source("Telnet TCP 连接失败", error).into());
                    }
                    Err(_) => return Err(AppError::timeout("Telnet connect").into()),
                }
            }
        };
        if cancel.is_cancelled() {
            drop(stream);
            return Err(AppError::cancelled("Telnet 连接已取消").into());
        }
        Ok(make_telnet_transport(
            stream,
            size,
            self.config.channel_buffer_bytes,
        ))
    }
}

/// 拒绝无法表达为安全 socket 目标的空白和控制字符，不把原始输入放入错误文案。
///
/// @author kongweiguang
fn validate_telnet_target(target: &TelnetSessionTarget) -> AppResult<()> {
    if target.host.trim().is_empty()
        || target
            .host
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(AppError::invalid("Telnet 主机地址无效"));
    }
    if target.port == 0 {
        return Err(AppError::invalid("Telnet 端口必须大于零"));
    }
    Ok(())
}

/// 将 GPUI/终端的 owned stream 与 TCP actor 连接，并为 resize/关闭提供幂等控制。
///
/// @author kongweiguang
fn make_telnet_transport(
    stream: TcpStream,
    initial_size: TerminalSize,
    channel_buffer_bytes: usize,
) -> SessionTransport {
    let (terminal, bridge) = io::duplex(channel_buffer_bytes);
    let (reader, writer) = io::split(terminal);
    let (mut bridge_reader, mut bridge_writer) = io::split(bridge);
    let (size_sender, size_receiver) = watch::channel(initial_size);
    let cancellation = CancellationToken::new();
    let task_cancellation = cancellation.clone();
    let task = tokio::spawn(async move {
        run_telnet_actor(
            stream,
            &mut bridge_reader,
            &mut bridge_writer,
            size_receiver,
            task_cancellation,
            initial_size,
        )
        .await
    });
    let control = TelnetSessionTransportControl {
        cancellation,
        size_sender,
        state: Mutex::new(TelnetControlState {
            task: Some(task),
            finished: None,
        }),
    };
    SessionTransport::new(reader, writer, control)
}

/// 在单一任务中串行驱动终端输入、TCP 输出、Telnet 协商和窗口尺寸更新。
async fn run_telnet_actor(
    stream: TcpStream,
    bridge_reader: &mut (impl AsyncRead + Unpin),
    bridge_writer: &mut (impl AsyncWrite + Unpin),
    mut size_receiver: watch::Receiver<TerminalSize>,
    cancellation: CancellationToken,
    initial_size: TerminalSize,
) -> AppResult<SessionExitStatus> {
    let (mut socket_reader, mut socket_writer) = stream.into_split();
    let mut protocol = TelnetProtocol::new(initial_size);
    if !write_protocol_output(&mut socket_writer, protocol.take_transmit(), &cancellation).await? {
        return Ok(closed_status());
    }

    let mut socket_buffer = [0_u8; TELNET_READ_BUFFER_BYTES];
    let mut input_buffer = [0_u8; TELNET_READ_BUFFER_BYTES];
    let mut input_closed = false;
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => {
                let _ = socket_writer.shutdown().await;
                let _ = bridge_writer.shutdown().await;
                return Ok(closed_status());
            }
            changed = size_receiver.changed() => {
                if changed.is_err() {
                    return Err(AppError::infrastructure("Telnet resize channel 已提前关闭"));
                }
                let size = *size_receiver.borrow_and_update();
                protocol.set_window_size(size);
                if !write_protocol_output(&mut socket_writer, protocol.take_transmit(), &cancellation).await? {
                    return Ok(closed_status());
                }
            }
            result = socket_reader.read(&mut socket_buffer) => {
                let count = result.map_err(|error| {
                    AppError::infrastructure_with_source("Telnet TCP 读取失败", error)
                })?;
                if count == 0 {
                    let _ = bridge_writer.shutdown().await;
                    return Ok(peer_closed_status());
                }
                let terminal_data = protocol.receive(&socket_buffer[..count]);
                if !write_terminal_output(bridge_writer, &terminal_data, &cancellation).await? {
                    return Ok(closed_status());
                }
                if !write_protocol_output(&mut socket_writer, protocol.take_transmit(), &cancellation).await? {
                    return Ok(closed_status());
                }
            }
            result = bridge_reader.read(&mut input_buffer), if !input_closed => {
                let count = result.map_err(|error| {
                    AppError::infrastructure_with_source("Telnet 终端输入读取失败", error)
                })?;
                if count == 0 {
                    // 关闭输入半边只发送 TCP FIN；远端尾部输出仍继续送到 terminal reader。
                    socket_writer.shutdown().await.map_err(|error| {
                        AppError::infrastructure_with_source("Telnet TCP 输入半关闭失败", error)
                    })?;
                    input_closed = true;
                } else {
                    protocol.send_data(&input_buffer[..count]);
                    if !write_protocol_output(&mut socket_writer, protocol.take_transmit(), &cancellation).await? {
                        return Ok(closed_status());
                    }
                }
            }
        }
    }
}

/// 把 parser 产出的 application data 有界写入 SessionService reader 一侧。
async fn write_terminal_output(
    writer: &mut (impl AsyncWrite + Unpin),
    bytes: &[u8],
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    if bytes.is_empty() {
        return Ok(true);
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(false),
        result = writer.write_all(bytes) => result
            .map(|()| true)
            .map_err(|error| AppError::infrastructure_with_source("Telnet 终端输出写入失败", error)),
    }
}

/// 仅发送 Core 已经排队的有限 Telnet 字节，取消可中断背压中的 socket 写。
async fn write_protocol_output(
    writer: &mut OwnedWriteHalf,
    bytes: Vec<u8>,
    cancellation: &CancellationToken,
) -> AppResult<bool> {
    if bytes.is_empty() {
        return Ok(true);
    }
    tokio::select! {
        biased;
        _ = cancellation.cancelled() => Ok(false),
        result = writer.write_all(&bytes) => result
            .map(|()| true)
            .map_err(|error| AppError::infrastructure_with_source("Telnet TCP 写入失败", error)),
    }
}

/// 终端用户不可见的协议包装层：所有原始字节都交由成熟 Sans-IO Core 解析和编码。
struct TelnetProtocol {
    core: Core,
}

impl TelnetProtocol {
    /// 固定声明 xterm 终端类型，并让初始 NAWS 与当前绘制网格一致。
    fn new(size: TerminalSize) -> Self {
        let policy = OptionPolicy::builder()
            .terminal_types(["xterm-256color", "xterm"])
            .window_size(size.columns(), size.rows())
            .build();
        Self {
            core: Core::with_policy(policy),
        }
    }

    /// 将任意 TCP 分块送入持久化 parser 状态，只把 Data 事件按顺序交给终端。
    fn receive(&mut self, bytes: &[u8]) -> Vec<u8> {
        self.core.receive(bytes);
        let mut terminal_data = Vec::new();
        while let Some(event) = self.core.poll_event() {
            if let Event::Data(data) = event {
                terminal_data.extend_from_slice(&data);
            }
        }
        terminal_data
    }

    /// 交由 Sans-IO Core 处理 CR/NUL、BINARY 和 IAC 双转义规则。
    fn send_data(&mut self, bytes: &[u8]) {
        self.core.send_data(bytes);
    }

    /// 记录分屏尺寸变化；Core 仅在 NAWS 已协商时向 peer 发送更新。
    fn set_window_size(&mut self, size: TerminalSize) {
        self.core.set_window_size(size.columns(), size.rows());
    }

    /// 取走本轮协商、NAWS 或用户输入产生的 wire bytes，避免共享无界发送队列。
    fn take_transmit(&mut self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.core.poll_transmit(&mut bytes);
        bytes
    }
}

/// Telnet session 的唯一控制入口，关闭和状态查询通过同一把锁消费 task。
struct TelnetSessionTransportControl {
    cancellation: CancellationToken,
    size_sender: watch::Sender<TerminalSize>,
    state: Mutex<TelnetControlState>,
}

/// 保存未消费 task 或第一次退出结果，确保 close/wait_exit 可重复调用。
struct TelnetControlState {
    task: Option<JoinHandle<AppResult<SessionExitStatus>>>,
    finished: Option<AppResult<SessionExitStatus>>,
}

#[async_trait]
impl SessionTransportControl for TelnetSessionTransportControl {
    /// 将最新网格写入 watch，不在 UI 调用线程执行网络操作。
    async fn resize(&self, size: TerminalSize) -> AppResult<()> {
        if self.cancellation.is_cancelled() || self.size_sender.is_closed() {
            return Err(AppError::conflict("Telnet 会话已关闭"));
        }
        self.size_sender
            .send(size)
            .map_err(|_| AppError::conflict("Telnet 会话已关闭"))
    }

    /// 用调用方统一的截止时间取消 actor；超时后保留 JoinHandle，避免丢失 TCP 清理所有权。
    ///
    /// @author kongweiguang
    async fn close_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.cancellation.cancel();
        self.finish_until(Some(deadline)).await.map(|_| ())
    }

    /// 等待远端 FIN 或显式关闭的稳定退出结果。
    async fn wait_exit(&self) -> AppResult<SessionExitStatus> {
        self.finish().await
    }
}

impl TelnetSessionTransportControl {
    /// 缓存 actor 终态，重复 wait/close 保持同一结果并保留首次 I/O 错误。
    async fn finish(&self) -> AppResult<SessionExitStatus> {
        self.finish_until(None).await
    }

    /// 使用绝对 deadline 获取 actor 状态并等待退出；超时后保存 task 供后续关闭继续 join。
    ///
    /// @author kongweiguang
    async fn finish_until(
        &self,
        deadline: Option<tokio::time::Instant>,
    ) -> AppResult<SessionExitStatus> {
        let mut state = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, self.state.lock())
                .await
                .map_err(|_| AppError::timeout("Telnet 关闭等待状态锁"))?,
            None => self.state.lock().await,
        };
        if let Some(result) = &state.finished {
            return result.clone();
        }
        let Some(mut task) = state.task.take() else {
            let result = Err(AppError::infrastructure("Telnet actor 缺少退出结果"));
            state.finished = Some(result.clone());
            return result;
        };
        let joined = match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, &mut task).await {
                Ok(joined) => joined,
                Err(_) => {
                    state.task = Some(task);
                    return Err(AppError::timeout("Telnet TCP transport 释放"));
                }
            },
            None => task.await,
        };
        let result = match joined {
            Ok(result) => result,
            Err(error) => Err(AppError::infrastructure_with_source(
                "Telnet actor 异常退出",
                error,
            )),
        };
        state.finished = Some(result.clone());
        result
    }
}

/// 用固定状态表示本地主动关闭，避免把请求取消伪装为远端故障。
fn closed_status() -> SessionExitStatus {
    SessionExitStatus {
        code: None,
        signal: Some("telnet session closed".to_owned()),
        success: true,
    }
}

/// 用固定状态表示 peer FIN，供 SessionInfo 展示关闭来源且不暴露地址。
fn peer_closed_status() -> SessionExitStatus {
    SessionExitStatus {
        code: None,
        signal: Some("telnet peer closed".to_owned()),
        success: true,
    }
}

#[cfg(test)]
#[path = "../../tests/support/telnet_protocol.rs"]
mod tests;
