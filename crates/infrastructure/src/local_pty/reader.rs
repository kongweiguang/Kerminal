//! 本地 PTY 输出 reader；有界队列在运行态施加背压，结束态持续 drain 原生输出。
//!
//! @author kongweiguang

use std::io::{self, Read};
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::mpsc::{self, Sender, error::TrySendError};
use tokio::task::JoinHandle;

use super::expected_pty_end;

const OUTPUT_CHUNK_BYTES: usize = 4 * 1024;
const OUTPUT_QUEUE_CHUNKS: usize = 16;
const FULL_QUEUE_RETRY: Duration = Duration::from_millis(2);

/// 原生 PTY reader 的有界异步接收端；顺序按单个 writer 的投递顺序保留。
pub(super) struct PtyOutputReader {
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    current: Vec<u8>,
    offset: usize,
    finished: bool,
}

/// PTY 输出的传递与关闭期舍弃统计，供生命周期诊断使用。
#[derive(Debug, Default)]
pub(super) struct PtyReaderStats {
    pub(super) bytes_read: u64,
    pub(super) bytes_queued: u64,
    pub(super) end_tail_discarded_bytes: u64,
}

/// 建立固定容量的输出通道；关闭时可以停止等待队列空间而继续 drain PTY。
///
/// @author kongweiguang
pub(super) fn output_reader_channel() -> (PtyOutputReader, Sender<io::Result<Vec<u8>>>) {
    let (sender, receiver) = mpsc::channel(OUTPUT_QUEUE_CHUNKS);
    (
        PtyOutputReader {
            receiver,
            current: Vec::new(),
            offset: 0,
            finished: false,
        },
        sender,
    )
}

impl AsyncRead for PtyOutputReader {
    /// 在异步调用方读取有界队列，队列满时由生产线程背压而不是扩大内存。
    ///
    /// @author kongweiguang
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if output.remaining() == 0 || self.finished {
            return Poll::Ready(Ok(()));
        }

        loop {
            if self.offset < self.current.len() {
                let available = &self.current[self.offset..];
                let count = available.len().min(output.remaining());
                output.put_slice(&available[..count]);
                self.offset += count;
                return Poll::Ready(Ok(()));
            }

            self.current.clear();
            self.offset = 0;
            match self.receiver.poll_recv(context) {
                Poll::Ready(Some(Ok(chunk))) => self.current = chunk,
                Poll::Ready(Some(Err(error))) => {
                    self.finished = true;
                    return Poll::Ready(Err(error));
                }
                Poll::Ready(None) => {
                    self.finished = true;
                    return Poll::Ready(Ok(()));
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// 在阻塞池读取 PTY；运行态有序等待有界容量，结束态丢弃满队列后的尾部并继续读至 EOF。
///
/// @author kongweiguang
pub(super) fn spawn_pty_reader(
    mut pty_reader: Box<dyn Read + Send>,
    sender: Sender<io::Result<Vec<u8>>>,
    closing: Arc<AtomicBool>,
    child_exited: Arc<AtomicBool>,
) -> JoinHandle<io::Result<PtyReaderStats>> {
    tokio::task::spawn_blocking(move || {
        let mut stats = PtyReaderStats::default();
        let mut buffer = [0_u8; OUTPUT_CHUNK_BYTES];
        let mut consumer_closed_while_running = false;

        loop {
            let count = match pty_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) if expected_pty_end(&error, &closing, &child_exited) => break,
                Err(error) => {
                    let marker = io::Error::new(error.kind(), "本地 PTY 输出读取失败");
                    let _ = send_output(&sender, Err(marker), &closing);
                    return Err(error);
                }
            };

            stats.bytes_read = stats.bytes_read.saturating_add(count as u64);
            if consumer_closed_while_running {
                if is_closing(&closing) {
                    stats.end_tail_discarded_bytes =
                        stats.end_tail_discarded_bytes.saturating_add(count as u64);
                }
                continue;
            }

            let chunk = buffer[..count].to_vec();
            match send_output(&sender, Ok(chunk), &closing) {
                OutputSend::Queued => {
                    stats.bytes_queued = stats.bytes_queued.saturating_add(count as u64);
                }
                OutputSend::DiscardedAfterEnd => {
                    stats.end_tail_discarded_bytes =
                        stats.end_tail_discarded_bytes.saturating_add(count as u64);
                }
                OutputSend::ReceiverClosed => {
                    consumer_closed_while_running = !is_closing(&closing);
                    if !consumer_closed_while_running {
                        stats.end_tail_discarded_bytes =
                            stats.end_tail_discarded_bytes.saturating_add(count as u64);
                    }
                }
            }
        }

        if consumer_closed_while_running {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "本地 PTY 输出接收端在会话运行期间关闭；原生输出已持续 drain",
            ));
        }

        Ok(stats)
    })
}

/// 有界队列在活动态等待空间，关闭或自然退出时则不再阻塞原生 PTY reader。
///
/// @author kongweiguang
fn send_output(
    sender: &Sender<io::Result<Vec<u8>>>,
    mut message: io::Result<Vec<u8>>,
    closing: &AtomicBool,
) -> OutputSend {
    loop {
        match sender.try_send(message) {
            Ok(()) => return OutputSend::Queued,
            Err(TrySendError::Closed(_)) => return OutputSend::ReceiverClosed,
            Err(TrySendError::Full(returned)) => {
                message = returned;
                if is_closing(closing) {
                    return OutputSend::DiscardedAfterEnd;
                }
                thread::sleep(FULL_QUEUE_RETRY);
            }
        }
    }
}

/// 仅把显式关闭视为允许舍弃有界队列尾部的生命周期边界，自然退出仍保持 lossless backpressure。
///
/// @author kongweiguang
fn is_closing(closing: &AtomicBool) -> bool {
    closing.load(Ordering::Acquire)
}

/// 标记每次有界通道投递的结果，区分可回压、关闭尾部舍弃和接收端消失。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OutputSend {
    Queued,
    DiscardedAfterEnd,
    ReceiverClosed,
}

#[cfg(test)]
#[path = "../../tests/support/local_pty_output_reader.rs"]
mod tests;
