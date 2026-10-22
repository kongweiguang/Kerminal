//! RDP 启动状态订阅契约测试。
//!
//! @author kongweiguang

use kerminal_application::{
    AppError,
    connections::rdp_launch_status::{
        RdpLaunchId, RdpLaunchStatus, RdpLaunchStatusMessage, RdpLaunchStatusSubscription,
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// 默认启动身份不能是 nil UUID，且每次构造都必须分配独立 v4 身份。
/// @author kongweiguang
#[test]
fn default_launch_ids_are_fresh_v4_values() {
    let first = RdpLaunchId::default();
    let second = RdpLaunchId::default();

    assert_ne!(first, second);
    assert_eq!(first.as_uuid().get_version_num(), 4);
    assert_eq!(second.as_uuid().get_version_num(), 4);
}

/// 首次读取必须立即给当前快照，避免 UI 订阅晚于启动状态变更时漏掉结果。
#[tokio::test]
async fn first_receive_returns_current_status_without_waiting_for_change() {
    let launch_id = RdpLaunchId::new();
    let initial = RdpLaunchStatusMessage {
        launch_id,
        revision: 0,
        status: RdpLaunchStatus::Requested,
    };
    let (_sender, receiver) = watch::channel(initial);
    let subscription = RdpLaunchStatusSubscription::from_receiver(receiver);

    let received = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        subscription.recv(CancellationToken::new()),
    )
    .await
    .expect("首条状态应为当前值")
    .expect("状态通道应保持可读");

    assert_eq!(received, initial);
}

/// watch 只保留最新状态，晚到订阅者不应消费过期中间值或丢失终态。
#[tokio::test]
async fn receive_returns_latest_terminal_status() {
    let launch_id = RdpLaunchId::new();
    let initial = RdpLaunchStatusMessage {
        launch_id,
        revision: 0,
        status: RdpLaunchStatus::Requested,
    };
    let (sender, receiver) = watch::channel(initial);
    let subscription = RdpLaunchStatusSubscription::from_receiver(receiver);
    let _ = subscription.recv(CancellationToken::new()).await.unwrap();

    let terminal = RdpLaunchStatusMessage {
        launch_id,
        revision: 1,
        status: RdpLaunchStatus::DelegatedToSystem,
    };
    sender.send_replace(terminal);

    assert_eq!(
        subscription.recv(CancellationToken::new()).await.unwrap(),
        terminal
    );
    assert_eq!(
        subscription.recv(CancellationToken::new()).await.unwrap(),
        terminal,
        "终态应保持可查询且不会变成客户端已关闭的推断"
    );
}

/// 取消一个 UI watcher 只能结束观察，不能悄悄转成成功或修改启动状态。
#[tokio::test]
async fn cancellation_ends_waiting_subscription_with_cancelled_code() {
    let initial = RdpLaunchStatusMessage {
        launch_id: RdpLaunchId::new(),
        revision: 0,
        status: RdpLaunchStatus::Requested,
    };
    let (_sender, receiver) = watch::channel(initial);
    let subscription = RdpLaunchStatusSubscription::from_receiver(receiver);
    let _ = subscription.recv(CancellationToken::new()).await.unwrap();
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let error = subscription.recv(cancellation).await.unwrap_err();
    assert!(matches!(error, AppError::Cancelled { .. }));
}
