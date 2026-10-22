//! MCP runtime UI 等待桥的协作取消回归。
//!
//! @author kongweiguang

use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use tokio_util::sync::CancellationToken;

use crate::features::mcp_runtime::await_operation_with_cooperative_cancellation;

struct CleanupOperation {
    cancellation: CancellationToken,
    cleanup_acknowledged: Arc<AtomicBool>,
    cleanup_started: Arc<AtomicBool>,
    cleanup_completed: Arc<AtomicBool>,
    dropped: Arc<AtomicBool>,
}

impl Future for CleanupOperation {
    type Output = &'static str;

    /// 操作收到取消后仍需等待资源 owner 确认收尾，再向 UI 返回结果。
    fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if !this.cancellation.is_cancelled() {
            return Poll::Pending;
        }

        this.cleanup_started.store(true, Ordering::SeqCst);
        if !this.cleanup_acknowledged.load(Ordering::SeqCst) {
            return Poll::Pending;
        }

        this.cleanup_completed.store(true, Ordering::SeqCst);
        Poll::Ready("cleanup acknowledged")
    }
}

impl Drop for CleanupOperation {
    /// 标记原操作 future 被丢弃的时点，验证取消只发信号而不提前抛弃 owner。
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::SeqCst);
    }
}

/// 用无运行时的手动 poll 驱动取消状态，避免测试引入第二个 Tokio runtime。
fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

/// 共享验证用户取消与 task 取消都等待同一操作的 cleanup acknowledgement。
fn assert_cancel_waits_for_cleanup(cancel_user_request: bool) {
    let operation_cancellation = CancellationToken::new();
    let user_cancellation = CancellationToken::new();
    let task_cancellation = CancellationToken::new();
    let cleanup_acknowledged = Arc::new(AtomicBool::new(false));
    let cleanup_started = Arc::new(AtomicBool::new(false));
    let cleanup_completed = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicBool::new(false));

    let operation = CleanupOperation {
        cancellation: operation_cancellation.clone(),
        cleanup_acknowledged: Arc::clone(&cleanup_acknowledged),
        cleanup_started: Arc::clone(&cleanup_started),
        cleanup_completed: Arc::clone(&cleanup_completed),
        dropped: Arc::clone(&dropped),
    };
    let mut waiting = Box::pin(await_operation_with_cooperative_cancellation(
        operation,
        operation_cancellation.clone(),
        user_cancellation.clone(),
        task_cancellation.clone(),
    ));

    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    if cancel_user_request {
        user_cancellation.cancel();
    } else {
        task_cancellation.cancel();
    }
    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    assert!(operation_cancellation.is_cancelled());
    assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
    assert!(cleanup_started.load(Ordering::SeqCst));
    assert!(!cleanup_completed.load(Ordering::SeqCst));
    assert!(!dropped.load(Ordering::SeqCst));

    cleanup_acknowledged.store(true, Ordering::SeqCst);
    assert_eq!(
        poll_once(waiting.as_mut()),
        Poll::Ready("cleanup acknowledged")
    );
    assert!(cleanup_completed.load(Ordering::SeqCst));
    assert!(dropped.load(Ordering::SeqCst));
}

/// 用户点取消时等待真实 cleanup 完成，不把 start future 直接 drop。
#[test]
fn user_cancellation_waits_for_cleanup_acknowledgement() {
    assert_cancel_waits_for_cleanup(true);
}

/// 应用 executor 取消等待时同样只向 operation 发协作取消信号。
#[test]
fn task_cancellation_waits_for_cleanup_acknowledgement() {
    assert_cancel_waits_for_cleanup(false);
}

/// 若 operation 已先完成，随后可观察到的取消不应覆盖它的真实完成结果。
#[test]
fn operation_completion_wins_before_cancellation() {
    let operation_cancellation = CancellationToken::new();
    let user_cancellation = CancellationToken::new();
    let task_cancellation = CancellationToken::new();
    user_cancellation.cancel();

    let mut waiting = Box::pin(await_operation_with_cooperative_cancellation(
        async { "operation completed" },
        operation_cancellation.clone(),
        user_cancellation,
        task_cancellation,
    ));

    assert_eq!(
        poll_once(waiting.as_mut()),
        Poll::Ready("operation completed")
    );
    assert!(!operation_cancellation.is_cancelled());
}
