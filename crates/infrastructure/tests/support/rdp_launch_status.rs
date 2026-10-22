//! RDP 状态目录的独立集成测试辅助用例。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::{
    AppError, PublicErrorCode,
    connections::{
        RdpClientLifecycle,
        rdp_launch_status::{RdpLaunchStatus, RdpLaunchStatusService},
    },
};
use kerminal_infrastructure::connections::rdp::RdpLaunchStatusHub;

/// 委派端只能发布委派完成或失败，不能伪造外部客户端的关闭状态。
#[tokio::test]
async fn delegated_client_cannot_publish_managed_closed_state() {
    let hub = Arc::new(RdpLaunchStatusHub::new(4, 4).unwrap());
    let (launch_id, reporter) = hub.register(RdpClientLifecycle::DelegatedToSystem).unwrap();

    let error = reporter.publish(RdpLaunchStatus::Closed).unwrap_err();
    assert!(matches!(error, AppError::Conflict { .. }));
    assert_eq!(
        hub.query(launch_id).unwrap().status,
        RdpLaunchStatus::Requested
    );
}

/// 已登记的 UI watcher 必须收到 broker 晚到失败，且消息只暴露稳定错误码。
#[tokio::test]
async fn subscriber_observes_late_broker_failure_without_sensitive_details() {
    let hub = Arc::new(RdpLaunchStatusHub::new(4, 4).unwrap());
    let (launch_id, reporter) = hub.register(RdpClientLifecycle::DelegatedToSystem).unwrap();
    let subscription = hub.subscribe(launch_id).unwrap();

    assert_eq!(
        subscription
            .recv(tokio_util::sync::CancellationToken::new())
            .await
            .unwrap()
            .status,
        RdpLaunchStatus::Requested
    );
    reporter
        .publish(RdpLaunchStatus::Failed {
            code: PublicErrorCode::Infrastructure,
        })
        .unwrap();

    let message = subscription
        .recv(tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(message.launch_id, launch_id);
    assert_eq!(
        message.status,
        RdpLaunchStatus::Failed {
            code: PublicErrorCode::Infrastructure,
        }
    );
}

/// 终态保留数量受限；容量压力淘汰最早的终态，不淘汰仍运行的请求。
#[tokio::test]
async fn registry_evicts_oldest_terminal_and_preserves_active_launches() {
    let hub = Arc::new(RdpLaunchStatusHub::new(2, 1).unwrap());
    let (old_id, old_reporter) = hub.register(RdpClientLifecycle::ManagedProcess).unwrap();
    old_reporter.publish(RdpLaunchStatus::Closed).unwrap();
    let (active_id, _active_reporter) = hub.register(RdpClientLifecycle::ManagedProcess).unwrap();
    let (new_id, _new_reporter) = hub.register(RdpClientLifecycle::ManagedProcess).unwrap();

    assert!(matches!(hub.query(old_id), Err(AppError::NotFound { .. })));
    assert_eq!(
        hub.query(active_id).unwrap().status,
        RdpLaunchStatus::Requested
    );
    assert_eq!(
        hub.query(new_id).unwrap().status,
        RdpLaunchStatus::Requested
    );
}

/// 目录达到全活动上限时拒绝新客户端，避免状态无主且进程脱离可观察记录。
#[test]
fn registry_rejects_new_launch_when_only_active_entries_remain() {
    let hub = Arc::new(RdpLaunchStatusHub::new(1, 1).unwrap());
    let _ = hub.register(RdpClientLifecycle::ManagedProcess).unwrap();

    assert!(matches!(
        hub.register(RdpClientLifecycle::ManagedProcess),
        Err(AppError::Conflict { .. })
    ));
}
