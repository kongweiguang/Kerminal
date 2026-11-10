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
