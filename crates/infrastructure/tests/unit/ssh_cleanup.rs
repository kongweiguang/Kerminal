//! SSH cleanup owner 准入预算的私有单元测试。
//!
//! @author kongweiguang

use super::{
    ConnectTask, MAX_ROUTE_OWNERS, PendingHandshakeCleanup, PendingRusshCleanup, RusshHandler,
};
use kerminal_application::{AppError, PublicErrorCode};
use kerminal_domain::connections::HostEndpoint;
use russh::client::Handle;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// 固定路由预算耗尽后拒绝新连接，并在一个 route 完成后重新开放单个名额。
#[test]
fn route_admission_is_bounded_and_released_after_route_completion() {
    let cleanup = PendingRusshCleanup::new();
    let mut reservations = (0..MAX_ROUTE_OWNERS)
        .map(|_| cleanup.reserve().expect("budget has one free slot"))
        .collect::<Vec<_>>();

    assert_eq!(cleanup.status(true).reserved_route_owners, MAX_ROUTE_OWNERS);
    assert!(cleanup.reserve().is_err());

    let completed_route = reservations.pop().expect("one reservation exists");
    completed_route.finish_route();
    assert_eq!(
        cleanup.status(true).reserved_route_owners,
        MAX_ROUTE_OWNERS - 1
    );
    assert!(cleanup.reserve().is_ok());

    drop(reservations);
    assert_eq!(cleanup.status(true).reserved_route_owners, 0);
}

/// route 已释放但 Russh handle 尚未确认 join 时继续占用配额，避免 pending owner 无界积累。
#[test]
fn pending_cleanup_keeps_admission_until_join_acknowledgement() {
    let cleanup = PendingRusshCleanup::new();
    let reservations = (0..(MAX_ROUTE_OWNERS - 1))
        .map(|_| cleanup.reserve().expect("route budget remains available"))
        .collect::<Vec<_>>();
    let reservation = cleanup.reserve().expect("one route slot is available");
    reservation.add_pending_cleanup();
    reservation.finish_route();

    assert_eq!(cleanup.status(true).reserved_route_owners, MAX_ROUTE_OWNERS);
    assert!(cleanup.reserve().is_err());

    reservation.complete_pending_cleanup();
    assert_eq!(
        cleanup.status(true).reserved_route_owners,
        MAX_ROUTE_OWNERS - 1
    );
    assert!(cleanup.reserve().is_ok());
    drop(reservations);
    assert_eq!(cleanup.status(true).reserved_route_owners, 0);
}

/// handler 记录的内部决策错误（如公钥无法序列化）仍优先返回，保留真实的拒绝原因。
#[tokio::test]
async fn handler_decision_error_takes_precedence_over_kex_failure() {
    let cleanup = PendingRusshCleanup::new();
    let reservation = cleanup.reserve().expect("route budget has a free slot");
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let task: ConnectTask =
        tokio::spawn(async { Err::<Handle<RusshHandler>, russh::Error>(russh::Error::Kex) });
    let handshake = PendingHandshakeCleanup::new(
        reservation,
        endpoint,
        Arc::new(StdMutex::new(Some(AppError::invalid(
            "test host key serialization failure",
        )))),
        CancellationToken::new(),
        task,
    );

    let error = handshake.map_connect_error(russh::Error::Kex);

    assert_eq!(error.public_code(), PublicErrorCode::Invalid);
}

/// 退出收尾重放已失败握手时，join 只证明 task 已结束且没有 session；该错误不能
/// 再把资源误报为未清理，否则桌面退出和插件重启会被假失败挡住。
///
/// @author kongweiguang
#[tokio::test]
async fn completed_failed_handshake_does_not_remain_pending_cleanup() {
    let cleanup = PendingRusshCleanup::new();
    let reservation = cleanup.reserve().expect("route budget has a free slot");
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let task: ConnectTask =
        tokio::spawn(async { Err::<Handle<RusshHandler>, russh::Error>(russh::Error::Kex) });
    let handshake = PendingHandshakeCleanup::new(
        Arc::clone(&reservation),
        endpoint,
        Arc::new(StdMutex::new(None)),
        CancellationToken::new(),
        task,
    );
    cleanup.retain_handshake(handshake);
    reservation.finish_route();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    cleanup
        .drain_until(deadline)
        .await
        .expect("a completed handshake task leaves no session to clean");

    let status = cleanup.status(false);
    assert_eq!(status.pending_handshakes, 0);
    assert_eq!(status.pending_sessions, 0);
    assert_eq!(status.reserved_route_owners, 0);
}

/// 已由其他 owner 确认结束的握手不消耗 I/O 预算，剩余时间为零仍可清掉状态记录。
#[tokio::test]
async fn completed_handshake_is_pruned_even_after_the_deadline() {
    let cleanup = PendingRusshCleanup::new();
    let reservation = cleanup.reserve().expect("route budget has a free slot");
    let handshake = PendingHandshakeCleanup::new(
        Arc::clone(&reservation),
        HostEndpoint {
            host: "example.test".to_owned(),
            port: 22,
            username: "test".to_owned(),
        },
        Arc::new(StdMutex::new(None)),
        CancellationToken::new(),
        tokio::spawn(async { Err(russh::Error::Kex) }),
    );
    cleanup.retain_handshake(Arc::clone(&handshake));
    reservation.finish_route();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    assert!(handshake.session_until(deadline).await.is_err());
    assert!(handshake.is_complete());

    cleanup
        .drain_until(tokio::time::Instant::now())
        .await
        .expect("pruning completed state does not need an I/O budget");
    assert_eq!(cleanup.status(false).pending_handshakes, 0);
}

/// 仍在运行且无法在预算内 join 的握手继续持有资源，必须保留超时错误供退出重试。
///
/// @author kongweiguang
#[tokio::test]
async fn running_handshake_remains_pending_after_drain_deadline() {
    let cleanup = PendingRusshCleanup::new();
    let reservation = cleanup.reserve().expect("route budget has a free slot");
    let endpoint = HostEndpoint {
        host: "example.test".to_owned(),
        port: 22,
        username: "test".to_owned(),
    };
    let (release, released) = tokio::sync::oneshot::channel::<()>();
    let task: ConnectTask = tokio::spawn(async move {
        let _ = released.await;
        Err::<Handle<RusshHandler>, russh::Error>(russh::Error::Kex)
    });
    let handshake = PendingHandshakeCleanup::new(
        Arc::clone(&reservation),
        endpoint,
        Arc::new(StdMutex::new(None)),
        CancellationToken::new(),
        task,
    );
    cleanup.retain_handshake(handshake);
    reservation.finish_route();

    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
    let error = cleanup
        .drain_until(deadline)
        .await
        .expect_err("a handshake task that cannot join is still a pending owner");
    assert_eq!(error.public_code(), PublicErrorCode::Timeout);
    assert_eq!(cleanup.status(false).pending_handshakes, 1);

    let _ = release.send(());
}
