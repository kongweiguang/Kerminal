//! SSH exec channel 的双向命令流实现；不处理连接认证、路由或 channel 准入。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        CommandEvent, CommandInputHandle, CommandStreamHandle, RemoteCommandInput,
        RemoteCommandStream,
    },
};
use russh::{ChannelMsg, client::Msg};
use tokio::{
    sync::{Mutex, mpsc},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use super::{CLOSE_OPERATION_TIMEOUT, RusshPeer, russh_error};

const STREAM_BUFFER: usize = 32;

/// 将已通过 session 请求确认的 channel 半部封装为命令流，不重新执行任何准入决策。
pub(super) fn from_channel(
    reader: russh::ChannelReadHalf,
    writer: russh::ChannelWriteHalf<Msg>,
    peer: Arc<RusshPeer>,
    pump_cancel: CancellationToken,
    initial_events: Vec<CommandEvent>,
) -> CommandStreamHandle {
    let (tx, receiver) = mpsc::channel(STREAM_BUFFER);
    let input = Arc::new(RusshCommandInput {
        writer: Arc::new(writer),
        write_lock: Mutex::new(()),
        stdin_closed: AtomicBool::new(false),
        channel_closed: AtomicBool::new(false),
        cancel: pump_cancel.clone(),
    });
    let pump = spawn_command_reader(
        reader,
        tx,
        Arc::clone(&input),
        pump_cancel.clone(),
        initial_events,
    );
    CommandStreamHandle::new(Box::new(RusshCommandStream {
        receiver,
        input,
        pump_cancel,
        pump: Some(pump),
        _peer: peer,
    }))
}

/// 有界命令读泵保留 stdout/stderr 顺序，并把退出状态与 EOF 分开处理。
fn spawn_command_reader(
    mut channel: russh::ChannelReadHalf,
    tx: mpsc::Sender<CommandEvent>,
    input: Arc<RusshCommandInput>,
    cancel: CancellationToken,
    initial_events: Vec<CommandEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut exited = initial_events
            .iter()
            .any(|event| matches!(event, CommandEvent::Exited { .. }));
        let mut close_channel = false;
        for event in initial_events {
            if !send_command_event(&tx, &cancel, event).await {
                close_channel = true;
                break;
            }
        }
        if close_channel {
            let _ = tokio::time::timeout(CLOSE_OPERATION_TIMEOUT, input.close_channel()).await;
            return;
        }
        loop {
            let message = tokio::select! {
                _ = cancel.cancelled() => {
                    close_channel = true;
                    break;
                }
                message = channel.wait() => message,
            };
            match message {
                Some(ChannelMsg::Data { data }) => {
                    if !send_command_event(&tx, &cancel, CommandEvent::Stdout(data.to_vec())).await
                    {
                        close_channel = true;
                        break;
                    }
                }
                Some(ChannelMsg::ExtendedData { data, .. }) => {
                    if !send_command_event(&tx, &cancel, CommandEvent::Stderr(data.to_vec())).await
                    {
                        close_channel = true;
                        break;
                    }
                }
                Some(ChannelMsg::ExitStatus { exit_status }) => {
                    if !exited {
                        exited = true;
                        if !send_command_event(
                            &tx,
                            &cancel,
                            CommandEvent::Exited {
                                code: Some(exit_status),
                                signal: None,
                            },
                        )
                        .await
                        {
                            close_channel = true;
                            break;
                        }
                    }
                }
                Some(ChannelMsg::ExitSignal { signal_name, .. }) => {
                    if !exited {
                        exited = true;
                        if !send_command_event(
                            &tx,
                            &cancel,
                            CommandEvent::Exited {
                                code: None,
                                signal: Some(format!("{signal_name:?}")),
                            },
                        )
                        .await
                        {
                            close_channel = true;
                            break;
                        }
                    }
                }
                Some(ChannelMsg::Eof) => continue,
                Some(ChannelMsg::Close) | None => break,
                _ => {}
            }
        }
        if close_channel {
            let _ = tokio::time::timeout(CLOSE_OPERATION_TIMEOUT, input.close_channel()).await;
            return;
        }
        if !exited
            && !send_command_event(
                &tx,
                &cancel,
                CommandEvent::Exited {
                    code: None,
                    signal: None,
                },
            )
            .await
        {
            let _ = tokio::time::timeout(CLOSE_OPERATION_TIMEOUT, input.close_channel()).await;
        }
    })
}

/// 让有界事件队列的背压能被 channel 取消打断，避免 close 时读泵永久卡在 send。
async fn send_command_event(
    sender: &mpsc::Sender<CommandEvent>,
    cancel: &CancellationToken,
    event: CommandEvent,
) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        result = sender.send(event) => result.is_ok(),
    }
}

/// 只在 channel owner 持有期间可写 stdin；close_stdin 发送 SSH EOF 但保留输出读取。
struct RusshCommandStream {
    receiver: mpsc::Receiver<CommandEvent>,
    input: Arc<RusshCommandInput>,
    pump_cancel: CancellationToken,
    pump: Option<JoinHandle<()>>,
    _peer: Arc<RusshPeer>,
}

#[async_trait]
impl RemoteCommandStream for RusshCommandStream {
    /// 等待读取下一条远端输出或退出事件，只有 channel 结束且事件耗尽才返回 None。
    async fn next_event(&mut self) -> AppResult<Option<CommandEvent>> {
        Ok(self.receiver.recv().await)
    }

    /// 返回共享 writer 句柄，使上层可同时读取输出和写入远端 stdin。
    fn input_handle(&self) -> CommandInputHandle {
        CommandInputHandle::new(self.input.clone())
    }

    /// 使用独立有限预算作为便利入口，涉及多个资源时由调用者提供共享 deadline。
    async fn close(&mut self) -> AppResult<()> {
        self.close_until(tokio::time::Instant::now() + CLOSE_OPERATION_TIMEOUT)
            .await
    }

    /// 取消命令读泵并在同一 deadline 内关闭 channel、join task；超时保留 JoinHandle 供重试。
    async fn close_until(&mut self, deadline: tokio::time::Instant) -> AppResult<()> {
        self.pump_cancel.cancel();
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::timeout("SSH exec channel close deadline"));
        }

        let input = Arc::clone(&self.input);
        let close_channel = input.close_channel_until(deadline);
        let (channel_result, pump_result) = match self.pump.as_mut() {
            Some(pump) => {
                let (channel_result, pump_result) = tokio::join!(
                    tokio::time::timeout_at(deadline, close_channel),
                    tokio::time::timeout_at(deadline, pump),
                );
                (channel_result, Some(pump_result))
            }
            None => (tokio::time::timeout_at(deadline, close_channel).await, None),
        };

        let mut first_error = match channel_result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(_) => Some(AppError::timeout("SSH exec channel close")),
        };
        match pump_result {
            Some(Ok(Ok(()))) => {
                self.pump.take();
            }
            Some(Ok(Err(error))) => {
                self.pump.take();
                first_error.get_or_insert_with(|| {
                    AppError::infrastructure_with_source("SSH exec reader task failed", error)
                });
            }
            Some(Err(_)) => {
                first_error.get_or_insert_with(|| AppError::timeout("SSH exec reader shutdown"));
            }
            None => {}
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// 可并发克隆的 russh stdin writer；同一 channel 的发送顺序由单一锁确定。
struct RusshCommandInput {
    writer: Arc<russh::ChannelWriteHalf<Msg>>,
    write_lock: Mutex<()>,
    stdin_closed: AtomicBool,
    channel_closed: AtomicBool,
    cancel: CancellationToken,
}

impl RusshCommandInput {
    /// 用有限默认预算幂等关闭 channel，供后台读泵处理取消和队列故障。
    async fn close_channel(&self) -> AppResult<()> {
        self.close_channel_until(tokio::time::Instant::now() + CLOSE_OPERATION_TIMEOUT)
            .await
    }

    /// 在统一 deadline 内获取 stdin 写锁并关闭 channel，超时时保留原子状态以便重试。
    async fn close_channel_until(&self, deadline: tokio::time::Instant) -> AppResult<()> {
        let _guard = tokio::time::timeout_at(deadline, self.write_lock.lock())
            .await
            .map_err(|_| AppError::timeout("SSH exec channel write lock"))?;
        if self.channel_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::time::timeout_at(deadline, self.writer.close())
            .await
            .map_err(|_| AppError::timeout("SSH exec channel close"))?
            .map_err(|error| russh_error("SSH exec channel close failed", error))?;
        self.channel_closed.store(true, Ordering::Release);
        Ok(())
    }
}

#[async_trait]
impl RemoteCommandInput for RusshCommandInput {
    /// 写入期间可被 command close 取消；序列锁避免并发调用重排远端输入字节。
    async fn write_stdin(&self, bytes: Vec<u8>) -> AppResult<()> {
        let _guard = tokio::select! {
            _ = self.cancel.cancelled() => return Err(AppError::cancelled("SSH exec input was closed")),
            guard = self.write_lock.lock() => guard,
        };
        if self.stdin_closed.load(Ordering::Acquire) {
            return Err(AppError::conflict("SSH exec stdin is already closed"));
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(AppError::cancelled("SSH exec input was closed")),
            result = self.writer.data_bytes(bytes) => result.map_err(|error| russh_error("SSH exec stdin write failed", error)),
        }
    }

    /// 在同一写序锁下仅发送一次 SSH EOF，保留输出读泵和父连接。
    async fn close_stdin(&self) -> AppResult<()> {
        let _guard = tokio::select! {
            _ = self.cancel.cancelled() => return Err(AppError::cancelled("SSH exec input was closed")),
            guard = self.write_lock.lock() => guard,
        };
        if self.stdin_closed.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(AppError::cancelled("SSH exec input was closed")),
            result = self.writer.eof() => {
                result.map_err(|error| russh_error("SSH exec stdin EOF failed", error))?;
                self.stdin_closed.store(true, Ordering::Release);
                Ok(())
            }
        }
    }
}

impl Drop for RusshCommandStream {
    /// 丢弃 owner 时取消当前 exec 读泵，由读泵在运行时尝试关闭本 channel。
    fn drop(&mut self) {
        self.pump_cancel.cancel();
    }
}
