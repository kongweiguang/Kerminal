//! 验证 ConPTY reader 合批：普通输出立即交付、隐藏/重绘原子交付，以及超时/上限/异常收尾。
//!
//! @author kongweiguang

use super::conpty_output_coalescer::{CURSOR_HOLD_DEADLINE, ReaderEvent, SessionOutputReader};
use std::{
    collections::VecDeque,
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWriteExt, ReadBuf, duplex},
    sync::oneshot,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

const HIDE: &[u8] = b"\x1b[m\x1b[?25l";
const CAPTURED_REDRAW: &[u8] = b"\x1b[93m\x1b[1;20Hab \x08\x1b[?25h";

/// 有界等待 reader 事件；超时直接失败，避免合批状态错误导致测试挂起。
///
/// @author kongweiguang
async fn next_event(reader: &mut SessionOutputReader, buffer: &mut [u8]) -> ReaderEvent {
    timeout(Duration::from_millis(500), reader.read(buffer))
        .await
        .expect("reader event timed out")
}

/// 显式 poll 一次但不丢弃 future，用于断言合批仍在等待且不依赖 sleep 时序。
///
/// @author kongweiguang
async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
}

/// 在独立任务里执行一次读取，便于在不取消 future 的前提下检查它是否仍在等待。
///
/// @author kongweiguang
async fn read_owned(
    mut reader: SessionOutputReader,
    mut buffer: Vec<u8>,
) -> (ReaderEvent, Vec<u8>, SessionOutputReader) {
    let event = reader.read(&mut buffer).await;
    (event, buffer, reader)
}

/// 普通输出没有隐藏切换，必须原样立即交付。
///
/// @author kongweiguang
#[tokio::test]
async fn ordinary_output_is_delivered_immediately() {
    let (mut writer, reader) = duplex(64);
    writer.write_all(b"plain prompt").await.expect("write");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 128];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), b"plain prompt"),
        _ => panic!("ordinary output must be data"),
    }
}

/// 实际抓取的两段 ConPTY 输出必须在第二段到达前保持未发布，show 后一次性按原始顺序交付。
///
/// @author kongweiguang
#[tokio::test]
async fn captured_hide_and_redraw_are_published_atomically() {
    let (mut writer, reader) = duplex(4096);
    writer.write_all(HIDE).await.expect("write hide");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 8192];
    let event = {
        let read = reader.read(&mut buffer);
        tokio::pin!(read);
        assert!(
            poll_once(read.as_mut()).await.is_pending(),
            "hide must not be published before the redraw arrives"
        );

        writer
            .write_all(CAPTURED_REDRAW)
            .await
            .expect("write redraw");
        read.await
    };
    match event {
        ReaderEvent::Data(output) => {
            let combined = [HIDE, CAPTURED_REDRAW].concat();
            assert_eq!(output.visible.as_slice(), combined.as_slice());
        }
        _ => panic!("captured pair must be data"),
    }
}

/// 非法私有正文中的 show 不得提前结束真实重绘，否则过滤后的画面仍会闪动。
///
/// @author kongweiguang
#[tokio::test]
async fn private_frame_bytes_cannot_close_a_redraw() {
    let (mut writer, reader) = duplex(128);
    writer.write_all(HIDE).await.expect("write hide");
    writer
        .write_all(b"\x1b]6977;Kerminal;\x1b[?25h\x07")
        .await
        .expect("write private frame");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 128];
    let read = reader.read(&mut buffer);
    tokio::pin!(read);
    assert!(poll_once(read.as_mut()).await.is_pending());
    writer
        .write_all(CAPTURED_REDRAW)
        .await
        .expect("write redraw");
    match read.await {
        ReaderEvent::Data(output) => {
            assert_eq!(output.visible, [HIDE, CAPTURED_REDRAW].concat());
            assert_eq!(output.frames.len(), 1);
            assert_eq!(output.frames[0].visible_offset, HIDE.len());
        }
        _ => panic!("the real redraw must close the batch"),
    }
}

/// 同一片段已经包含 show 时立即交付，不额外等待。
///
/// @author kongweiguang
#[tokio::test]
async fn chunk_containing_show_does_not_wait() {
    let (mut writer, reader) = duplex(128);
    let mut wire = HIDE.to_vec();
    wire.extend_from_slice(CAPTURED_REDRAW);
    writer.write_all(&wire).await.expect("write");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 256];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), wire.as_slice()),
        _ => panic!("combined chunk must be data"),
    }
}

/// 应用有意保持隐藏时绝对截止时间放行一次；超时后普通隐藏重绘立即通过，
/// 而同一片段里的 show→hide 必须重新开始等待。
///
/// @author kongweiguang
#[tokio::test]
async fn intentional_hiding_times_out_then_resumes_immediately() {
    let (mut writer, reader) = duplex(256);
    writer.write_all(HIDE).await.expect("write hide");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 256];
    let started = tokio::time::Instant::now();
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("deadline must release held hide"),
    }
    assert!(
        started.elapsed() >= CURSOR_HOLD_DEADLINE - Duration::from_millis(8),
        "deadline released too early: {:?}",
        started.elapsed()
    );

    // 光标仍未恢复可见，但这里没有新的隐藏周期：重绘必须立即通过。
    writer.write_all(b"still-hidden").await.expect("write");
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), b"still-hidden"),
        _ => panic!("hidden output after timeout must pass through"),
    }

    // 超时释放后光标仍隐藏；同一片段里的 show→hide 是新的隐藏周期，必须重新等待。
    let show_hide = b"\x1b[?25h\x1b[?25l";
    writer
        .write_all(show_hide)
        .await
        .expect("write show then hide");
    let read = {
        let read = reader.read(&mut buffer);
        tokio::pin!(read);
        assert!(
            poll_once(read.as_mut()).await.is_pending(),
            "show then hide must start a new hold period"
        );
        writer
            .write_all(CAPTURED_REDRAW)
            .await
            .expect("write redraw");
        read.await
    };
    match read {
        ReaderEvent::Data(output) => {
            let mut combined = show_hide.to_vec();
            combined.extend_from_slice(CAPTURED_REDRAW);
            assert_eq!(output.visible.as_slice(), combined.as_slice());
        }
        _ => panic!("second cycle must publish atomically"),
    }
}

/// 调度恢复时若截止和 PTY 输出都已就绪，先放行本批，再读取后续输出。
///
/// @author kongweiguang
#[tokio::test]
async fn ready_output_does_not_overtake_the_deadline() {
    let (mut writer, reader) = duplex(128);
    writer.write_all(HIDE).await.expect("write hide");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 128];
    let event = {
        let read = reader.read(&mut buffer);
        tokio::pin!(read);
        assert!(poll_once(read.as_mut()).await.is_pending());
        tokio::time::sleep(CURSOR_HOLD_DEADLINE + Duration::from_millis(5)).await;
        writer
            .write_all(CAPTURED_REDRAW)
            .await
            .expect("write redraw");
        read.await
    };
    match event {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("deadline must release the held bytes first"),
    }
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), CAPTURED_REDRAW),
        _ => panic!("queued output must remain available after the deadline"),
    }
}

/// EOF 发生在持有期间时先交付已读字节，再报告结束。
///
/// @author kongweiguang
#[tokio::test]
async fn eof_flushes_held_bytes_before_end() {
    let (mut writer, reader) = duplex(128);
    writer.write_all(HIDE).await.expect("write hide");
    drop(writer);
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 128];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("held bytes must be delivered before EOF"),
    }
    assert!(matches!(
        next_event(&mut reader, &mut buffer).await,
        ReaderEvent::End
    ));
}

/// 取消发生在持有期间时先交付已读字节，下一次调用才报告取消。
///
/// @author kongweiguang
#[tokio::test]
async fn cancellation_flushes_held_bytes_before_exit() {
    let (signal, ready) = oneshot::channel();
    let reader = ScriptedReader::with_pending_after(vec![Ok(HIDE.to_vec())], signal);
    let cancellation = CancellationToken::new();
    let reader = SessionOutputReader::new(Box::pin(reader), cancellation.clone(), true);
    let task = tokio::spawn(read_owned(reader, vec![0_u8; 128]));
    timeout(Duration::from_millis(500), ready)
        .await
        .expect("reader must consume hide before cancellation")
        .expect("pending signal dropped");
    cancellation.cancel();
    let (event, mut buffer, mut reader) = timeout(Duration::from_millis(500), task)
        .await
        .expect("cancelled read timed out")
        .expect("read task failed");
    match event {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("cancellation must flush consumed bytes first"),
    }
    assert!(matches!(
        next_event(&mut reader, &mut buffer).await,
        ReaderEvent::Cancelled
    ));
}

/// 底层读取错误发生在持有期间时同样先交付已读字节，再报告原错误。
///
/// @author kongweiguang
#[tokio::test]
async fn read_error_flushes_held_bytes_before_failure() {
    let reader = ScriptedReader::new(vec![Ok(HIDE.to_vec()), Err(io::Error::other("pty failed"))]);
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 128];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("error path must flush consumed bytes first"),
    }
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Failed(error) => assert_eq!(error.kind(), io::ErrorKind::Other),
        _ => panic!("original error must be preserved"),
    }
}

/// 缓冲写满时立即交付已读部分，异常隐藏不会累积超过单次读取上限。
///
/// @author kongweiguang
#[tokio::test]
async fn bounded_buffer_releases_at_read_cap() {
    let reader = ScriptedReader::new(vec![Ok(HIDE.to_vec()), Ok(b"redraw".to_vec())]);
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), true);
    let mut buffer = [0_u8; 12];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible, [HIDE, &b"red"[..]].concat()),
        _ => panic!("full buffer must release held bytes"),
    }
    let mut rest = [0_u8; 8];
    match next_event(&mut reader, &mut rest).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible, b"raw"),
        _ => panic!("remaining held bytes must follow"),
    }
}

/// 非 ConPTY transport 不启用合批，隐藏序列也按原始读取立即交付。
///
/// @author kongweiguang
#[tokio::test]
async fn generic_transport_never_holds() {
    let (mut writer, reader) = duplex(128);
    writer.write_all(HIDE).await.expect("write hide");
    let mut reader = SessionOutputReader::new(Box::pin(reader), CancellationToken::new(), false);
    let mut buffer = [0_u8; 128];
    match next_event(&mut reader, &mut buffer).await {
        ReaderEvent::Data(output) => assert_eq!(output.visible.as_slice(), HIDE),
        _ => panic!("generic transport must pass through"),
    }
}

/// 按脚本返回固定 chunk 或错误的测试 reader，用于确定性地覆盖错误、容量和取消路径。
struct ScriptedReader {
    chunks: VecDeque<io::Result<Vec<u8>>>,
    pending: bool,
    pending_signal: Option<oneshot::Sender<()>>,
}

impl ScriptedReader {
    /// 构造一个按顺序返回结果的 reader；队列耗尽即 EOF。
    ///
    /// @author kongweiguang
    fn new(chunks: Vec<io::Result<Vec<u8>>>) -> Self {
        Self {
            chunks: chunks.into(),
            pending: false,
            pending_signal: None,
        }
    }

    /// 队列耗尽后保持 Pending 并通知测试，模拟隐藏周期中真实 reader 的无数据等待。
    ///
    /// @author kongweiguang
    fn with_pending_after(chunks: Vec<io::Result<Vec<u8>>>, signal: oneshot::Sender<()>) -> Self {
        Self {
            chunks: chunks.into(),
            pending: true,
            pending_signal: Some(signal),
        }
    }
}

impl AsyncRead for ScriptedReader {
    /// 每次轮询消费一个脚本结果；缓冲区不足时把剩余字节留在队首。
    ///
    /// @author kongweiguang
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Some(chunk) = this.chunks.pop_front() {
            return match chunk {
                Ok(bytes) => {
                    let length = bytes.len().min(buffer.remaining());
                    buffer.put_slice(&bytes[..length]);
                    if length < bytes.len() {
                        this.chunks.push_front(Ok(bytes[length..].to_vec()));
                    }
                    Poll::Ready(Ok(()))
                }
                Err(error) => Poll::Ready(Err(error)),
            };
        }
        if this.pending {
            if let Some(signal) = this.pending_signal.take() {
                let _ = signal.send(());
            }
            Poll::Pending
        } else {
            Poll::Ready(Ok(()))
        }
    }
}
