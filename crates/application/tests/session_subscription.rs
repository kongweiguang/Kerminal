//! 验证终端变化订阅可取消、可观察 sender 关闭且不会重复交付 watch revision。
//!
//! @author kongweiguang

use std::time::Duration;

use kerminal_application::AppError;
use kerminal_application::sessions::{SessionChange, SessionChangeKind, SessionSubscription};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// 把新变化作为单次事件交付，并确认消费后不会立即重复唤醒。
///
/// @author kongweiguang
#[tokio::test]
async fn subscription_consumes_each_revision_once() {
    let (sender, receiver) = watch::channel(SessionChange {
        revision: 0,
        kind: SessionChangeKind::Lifecycle,
    });
    let mut subscription = SessionSubscription::from_receiver(receiver);
    let cancellation = CancellationToken::new();
    sender.send_replace(SessionChange {
        revision: 1,
        kind: SessionChangeKind::Output,
    });

    assert_eq!(
        subscription
            .recv(&cancellation)
            .await
            .expect("revision delivery"),
        Some(SessionChange {
            revision: 1,
            kind: SessionChangeKind::Output,
        }),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(20), subscription.recv(&cancellation),)
            .await
            .is_err(),
        "an already consumed revision must not be delivered twice",
    );
}

/// 取消正在等待的订阅必须立即结束，不要求发送新的 watch 变化。
///
/// @author kongweiguang
#[tokio::test]
async fn cancellation_interrupts_idle_subscription() {
    let (_sender, receiver) = watch::channel(SessionChange {
        revision: 0,
        kind: SessionChangeKind::Lifecycle,
    });
    let mut subscription = SessionSubscription::from_receiver(receiver);
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert!(matches!(
        subscription.recv(&cancellation).await,
        Err(AppError::Cancelled { .. }),
    ));
}

/// sender 退出后以 `None` 表示订阅结束，区别于调用方主动取消。
///
/// @author kongweiguang
#[tokio::test]
async fn closed_sender_ends_subscription() {
    let (sender, receiver) = watch::channel(SessionChange {
        revision: 0,
        kind: SessionChangeKind::Lifecycle,
    });
    let mut subscription = SessionSubscription::from_receiver(receiver);
    drop(sender);

    assert_eq!(
        subscription
            .recv(&CancellationToken::new())
            .await
            .expect("closed channel is not an infrastructure error"),
        None,
    );
}
