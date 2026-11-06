//! Cancellation behavior for source digest reads that stall after request dispatch.
//!
//! @author kongweiguang

use std::{
    future::pending,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};

use kerminal_application::{AppError, AppResult};
use tokio::{
    io::{AsyncRead, AsyncReadExt, ReadBuf},
    sync::Notify,
    time::timeout,
};
use tokio_util::sync::CancellationToken;

use super::*;

struct DropCounter(Arc<AtomicUsize>);

impl Drop for DropCounter {
    /// 取消必须丢弃挂起的 endpoint/read future，使外层 session owner 能继续回收连接资源。
    ///
    /// @author kongweiguang
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct PendingReader {
    started: Arc<Notify>,
    _drop_counter: DropCounter,
}

impl AsyncRead for PendingReader {
    /// 模拟远端 READ 已发出但服务端保持连接且不返回响应的状态。
    ///
    /// @author kongweiguang
    fn poll_read(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        _buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.started.notify_one();
        Poll::Pending
    }
}

/// endpoint 打开或读取无响应时，取消令牌应丢弃对应 future 而非等待网络请求超时。
///
/// @author kongweiguang
#[tokio::test]
async fn stalled_open_and_read_are_cancelled_and_dropped() -> AppResult<()> {
    let cancellation = CancellationToken::new();
    let open_started = Arc::new(Notify::new());
    let open_dropped = Arc::new(AtomicUsize::new(0));
    let open_task = tokio::spawn({
        let cancellation = cancellation.clone();
        let open_started = open_started.clone();
        let open_dropped = open_dropped.clone();
        async move {
            let operation = async move {
                let _drop_counter = DropCounter(open_dropped);
                open_started.notify_one();
                pending::<AppResult<()>>().await
            };
            await_unless_cancelled(&cancellation, operation).await
        }
    });
    timeout(Duration::from_secs(1), open_started.notified())
        .await
        .map_err(|_| AppError::infrastructure("endpoint 打开 future 未开始运行"))?;
    cancellation.cancel();
    let opened = timeout(Duration::from_secs(1), open_task)
        .await
        .map_err(|_| AppError::infrastructure("挂起的源 endpoint 打开未及时响应取消"))?
        .map_err(|_| AppError::infrastructure("endpoint 取消任务异常结束"))?;
    assert!(opened.is_none());
    assert_eq!(open_dropped.load(Ordering::SeqCst), 1);

    let cancellation = CancellationToken::new();
    let read_started = Arc::new(Notify::new());
    let read_dropped = Arc::new(AtomicUsize::new(0));
    let read_task = tokio::spawn({
        let cancellation = cancellation.clone();
        let read_started = read_started.clone();
        let read_dropped = read_dropped.clone();
        async move {
            let mut reader = PendingReader {
                started: read_started,
                _drop_counter: DropCounter(read_dropped.clone()),
            };
            let mut buffer = [0_u8; 16];
            let result = await_unless_cancelled(&cancellation, reader.read(&mut buffer)).await;
            drop(reader);
            (result, read_dropped.load(Ordering::SeqCst))
        }
    });
    timeout(Duration::from_secs(1), read_started.notified())
        .await
        .map_err(|_| AppError::infrastructure("源 READ future 未开始运行"))?;
    cancellation.cancel();
    let read_result = timeout(Duration::from_secs(1), read_task)
        .await
        .map_err(|_| AppError::infrastructure("挂起的源 READ 未及时响应取消"))?
        .map_err(|_| AppError::infrastructure("源 READ 取消任务异常结束"))?;
    assert!(read_result.0.is_none());
    assert_eq!(read_result.1, 1);
    Ok(())
}
