//! 验证 SSH lease 最后释放、channel clone 与可重试服务 shutdown 的资源归属。
//!
//! @author kongweiguang

mod support;

use std::{sync::Arc, time::Duration};

use kerminal_application::connections::{
    ConnectionIntent, OpenSftpRequest, RemoteConnectionService,
};
use kerminal_domain::{
    connections::{
        ChannelLane, CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol,
        RemoteShellDialect, SshAuthentication, SshRoute,
    },
    identity::HostId,
};
use tokio_util::sync::CancellationToken;

use support::ssh_harness::{
    SshServerFixture, SshServerOptions, TestCredentialResolver, TestKnownHostsRepository,
    connection_service,
};

/// 构造只指向 loopback fixture 的密码认证 profile，并在独立内存仓储中预先信任其 host key。
fn test_service(
    server: &SshServerFixture,
) -> (
    Arc<kerminal_infrastructure::connections::RusshRemoteConnectionService>,
    HostId,
) {
    let host_id = HostId::new();
    let endpoint = HostEndpoint {
        host: server.address.ip().to_string(),
        port: server.address.port(),
        username: "kerminal".to_owned(),
    };
    let profile = HostConnectionProfile {
        host_id: host_id.clone(),
        protocol: RemoteProtocol::Ssh,
        endpoint: endpoint.clone(),
        authentication: Some(SshAuthentication::Password {
            secret: CredentialRef::new("cleanup-test:password").expect("fixed ref is valid"),
        }),
        rdp_password: None,
        route: SshRoute::default(),
        shell_dialect: RemoteShellDialect::Posix,
        connect_timeout_ms: 2_000,
        keepalive_interval_ms: None,
    };
    let credentials = Arc::new(TestCredentialResolver::default());
    credentials.insert_password("cleanup-test:password", "test-password");
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    known_hosts.trust_fingerprint(&endpoint, "ecdsa-sha2-nistp256", &server.fingerprint_sha256);
    (
        connection_service(profile, credentials, known_hosts),
        host_id,
    )
}

/// 删除最终 lease 后，Russh handle 必须留在 service owner 内直到真实 join 完成。
#[tokio::test]
async fn final_lease_drop_is_drained_by_service_shutdown() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback SSH fixture starts");
    let (service, host_id) = test_service(&server);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await;
    let lease = lease.expect("fixture host connects");
    drop(lease);

    let status = service.cleanup_status();
    assert_eq!(status.active_peer_owners, 0);
    assert_eq!(status.reserved_route_owners, 1);
    assert_eq!(status.pending_sessions, 1);
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .expect("service joins the last dropped lease");
    assert_eq!(service.cleanup_status().pending_sessions, 0);
    assert_eq!(service.cleanup_status().reserved_route_owners, 0);
    server.shutdown().await;
}

/// 父 lease 值被丢弃后，channel clone 仍持有共享 peer 生命周期。
#[tokio::test]
async fn sftp_channel_clone_delays_last_drop_cleanup_until_channel_release() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback SSH fixture starts");
    let (service, host_id) = test_service(&server);
    let lease = service
        .connect(host_id, ConnectionIntent::Sftp, CancellationToken::new())
        .await
        .expect("fixture host connects");
    let channel = lease
        .open_sftp(
            OpenSftpRequest {
                lane: ChannelLane::Bulk,
            },
            CancellationToken::new(),
        )
        .await
        .expect("fixture accepts an SFTP subsystem channel");
    drop(lease);
    assert_eq!(service.cleanup_status().active_peer_owners, 1);
    assert_eq!(service.cleanup_status().pending_sessions, 0);

    drop(channel);
    assert_eq!(service.cleanup_status().active_peer_owners, 0);
    assert_eq!(service.cleanup_status().reserved_route_owners, 1);
    assert_eq!(service.cleanup_status().pending_sessions, 1);
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .expect("dropping the final channel transfers the route to cleanup");
    assert_eq!(service.cleanup_status().pending_sessions, 0);
    assert_eq!(service.cleanup_status().reserved_route_owners, 0);
    server.shutdown().await;
}

/// 已显式 join 的连接在 lease 最后 Drop 时不再进入 pending cleanup。
#[tokio::test]
async fn explicit_close_does_not_register_completed_session_on_drop() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback SSH fixture starts");
    let (service, host_id) = test_service(&server);
    let lease = service
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .expect("fixture host connects");
    lease
        .close_until(
            tokio::time::Instant::now() + Duration::from_secs(3),
            "cleanup lifecycle test completed",
        )
        .await
        .expect("explicit close joins its russh handle");
    assert_eq!(service.cleanup_status().active_peer_owners, 0);
    assert_eq!(service.cleanup_status().reserved_route_owners, 0);
    drop(lease);
    assert_eq!(service.cleanup_status().pending_sessions, 0);
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(2))
        .await
        .expect("closed service drains without pending resources");
    server.shutdown().await;
}

/// shutdown 到期后仍保留 pending owner，下一次更晚的同一服务 shutdown 可以继续 join。
#[tokio::test]
async fn expired_shutdown_keeps_cleanup_owner_for_retry() {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback SSH fixture starts");
    let (service, host_id) = test_service(&server);
    let lease = service
        .connect(
            host_id.clone(),
            ConnectionIntent::Exec,
            CancellationToken::new(),
        )
        .await
        .expect("fixture host connects");
    drop(lease);

    assert!(
        service
            .shutdown_until(tokio::time::Instant::now())
            .await
            .is_err()
    );
    assert!(!service.cleanup_status().accepting_connections);
    assert_eq!(service.cleanup_status().pending_sessions, 1);
    assert_eq!(service.cleanup_status().reserved_route_owners, 1);
    assert!(
        service
            .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
            .await
            .is_err()
    );
    service
        .shutdown_until(tokio::time::Instant::now() + Duration::from_secs(3))
        .await
        .expect("later shutdown retries the retained Russh handle");
    assert_eq!(service.cleanup_status().pending_sessions, 0);
    assert_eq!(service.cleanup_status().reserved_route_owners, 0);
    server.shutdown().await;
}
