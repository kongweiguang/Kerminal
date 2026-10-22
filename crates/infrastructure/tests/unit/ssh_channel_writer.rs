//! Exact SSH channel-end shutdown proof regression.
//!
//! @author kongweiguang

use std::{
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll},
};

use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::SshChannelWriter;

struct BrokenPipeWriter {
    shutdown_calls: Arc<AtomicUsize>,
}

impl AsyncWrite for BrokenPipeWriter {
    /// 证明存在 channel end 时只豁免 shutdown；普通 input 仍由底层返回 BrokenPipe。
    ///
    /// @author kongweiguang
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        _buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test closed SSH channel",
        )))
    }

    /// flush 不参与该回归，保持可观察行为与 shutdown 分离。
    ///
    /// @author kongweiguang
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    /// 记录底层 shutdown 是否真的被调用，以证明无 proof 时没有短路。
    ///
    /// @author kongweiguang
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutdown_calls.fetch_add(1, Ordering::AcqRel);
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "test closed SSH channel",
        )))
    }
}

/// 只允许 exact channel-end proof 把 dead-channel shutdown 视为已完成，仍拒绝 input 重放。
///
/// @author kongweiguang
#[tokio::test]
async fn shutdown_requires_channel_end_proof_and_never_masks_write_errors() {
    let shutdown_calls = Arc::new(AtomicUsize::new(0));
    let remote_channel_end_confirmed = Arc::new(AtomicBool::new(false));
    let mut writer = SshChannelWriter {
        inner: BrokenPipeWriter {
            shutdown_calls: Arc::clone(&shutdown_calls),
        },
        remote_channel_end_confirmed: Arc::clone(&remote_channel_end_confirmed),
    };

    let first_shutdown = writer
        .shutdown()
        .await
        .expect_err("without proof, shutdown must reach russh and preserve its error");
    assert_eq!(first_shutdown.kind(), io::ErrorKind::BrokenPipe);
    assert_eq!(shutdown_calls.load(Ordering::Acquire), 1);
    let input_error_without_proof = writer
        .write_all(b"ordinary-input")
        .await
        .expect_err("without proof, ordinary writes retain the bottom-layer error");
    assert_eq!(input_error_without_proof.kind(), io::ErrorKind::BrokenPipe);

    remote_channel_end_confirmed.store(true, Ordering::Release);
    writer
        .shutdown()
        .await
        .expect("confirmed channel end makes only shutdown idempotent");
    assert_eq!(shutdown_calls.load(Ordering::Acquire), 1);

    let input_error = writer
        .write_all(b"must-not-be-replayed")
        .await
        .expect_err("channel-end proof cannot mask a normal input write error");
    assert_eq!(input_error.kind(), io::ErrorKind::BrokenPipe);
}
