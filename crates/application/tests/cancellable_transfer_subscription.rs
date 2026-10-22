//! 队列唤醒订阅的取消、lag 与 channel 关闭语义回归测试。
//!
//! @author kongweiguang

use kerminal_application::transfers::TransferQueueEvent;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

mod transfers {
    pub use kerminal_application::transfers::TransferQueueEvent;
}

#[path = "../src/transfers/subscription.rs"]
mod subscription;

/// 单个变更事件仅作为一次唤醒交付。
#[tokio::test]
async fn event_wakes_subscription() {
    let (sender, mut receiver) = broadcast::channel(2);
    for event in [TransferQueueEvent::Changed, TransferQueueEvent::Cleared] {
        sender.send(event).expect("receiver exists");

        assert_eq!(
            subscription::receive_transfer_wakeup(&mut receiver, &CancellationToken::new()).await,
            Some(()),
        );
    }
}

/// 广播 lag 仍然唤醒读者，由其重新查询权威任务快照。
#[tokio::test]
async fn lagged_receiver_is_prompted_to_reload_snapshot() {
    let (sender, mut receiver) = broadcast::channel(1);
    sender
        .send(TransferQueueEvent::Changed)
        .expect("receiver exists");
    sender
        .send(TransferQueueEvent::Changed)
        .expect("receiver exists");

    assert_eq!(
        subscription::receive_transfer_wakeup(&mut receiver, &CancellationToken::new()).await,
        Some(()),
    );
}

/// receiver 关闭时返回 None，与有事件或 lag 的唤醒区分。
#[tokio::test]
async fn closed_sender_ends_subscription() {
    let (sender, mut receiver) = broadcast::channel(1);
    drop(sender);

    assert_eq!(
        subscription::receive_transfer_wakeup(&mut receiver, &CancellationToken::new()).await,
        None,
    );
}

/// 取消优先于已排队事件，保证 panel 退出时不继续消费旧唤醒。
#[tokio::test]
async fn cancellation_wins_over_queued_event() {
    let (sender, mut receiver) = broadcast::channel(1);
    sender
        .send(TransferQueueEvent::Cleared)
        .expect("receiver exists");
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    assert_eq!(
        subscription::receive_transfer_wakeup(&mut receiver, &cancellation).await,
        None,
    );
}

/// waiter 只持克隆令牌，保留测试侧的原令牌在等待启动后发出取消信号。
#[tokio::test]
async fn cancellation_interrupts_idle_subscription() {
    let (_sender, mut receiver) = broadcast::channel::<TransferQueueEvent>(1);
    let cancellation = CancellationToken::new();
    let waiter_cancellation = cancellation.clone();
    let waiter = tokio::spawn(async move {
        subscription::receive_transfer_wakeup(&mut receiver, &waiter_cancellation).await
    });
    tokio::task::yield_now().await;
    cancellation.cancel();

    assert_eq!(waiter.await.expect("subscription does not panic"), None);
}
