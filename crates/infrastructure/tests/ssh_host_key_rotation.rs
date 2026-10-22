//! SSH 主机密钥轮换必须重新握手，并将确认绑定到当前主机与逐跳 peer。
//!
//! @author kongweiguang

mod support;

use std::sync::Arc;

use kerminal_application::{
    PublicErrorCode,
    connections::{
        HostKeyIdentity, HostKeyPeer, HostKeyRotationRequest, HostKeyStatus, KnownHostsRepository,
        RemoteConnectionService,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol, RemoteShellDialect,
        SshAuthentication, SshRoute,
    },
    identity::HostId,
};
use russh::keys::{HashAlg, ssh_key::PublicKey};
use support::ssh_harness::{
    SshServerFixture, SshServerOptions, TestCredentialResolver, TestKnownHostsRepository,
    connection_service,
};
use tokio_util::sync::CancellationToken;

const PREVIOUS_HOST_PUBLIC_KEY: &str =
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJdD7y3aLq454yWBdwLWbieU1ebz9/cu7/QEXn9OIeZJ";

struct RotationHarness {
    server: SshServerFixture,
    profile: HostConnectionProfile,
    known_hosts: Arc<TestKnownHostsRepository>,
    service: Arc<kerminal_infrastructure::connections::RusshRemoteConnectionService>,
    previous_fingerprint_sha256: String,
}

/// 构造隔离的旧信任记录和 loopback SSH 服务，避免访问系统 known_hosts 或真实主机。
///
/// @author kongweiguang
async fn rotation_harness() -> RotationHarness {
    let server = SshServerFixture::spawn(SshServerOptions::default())
        .await
        .expect("loopback SSH server starts");
    let profile = HostConnectionProfile {
        host_id: HostId::new(),
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: server.address.ip().to_string(),
            port: server.address.port(),
            username: "kerminal".to_owned(),
        },
        authentication: Some(SshAuthentication::Password {
            secret: CredentialRef::new("test:password").expect("test credential ref is valid"),
        }),
        rdp_password: None,
        route: SshRoute::default(),
        shell_dialect: RemoteShellDialect::Posix,
        connect_timeout_ms: 2_000,
        keepalive_interval_ms: None,
    };
    let previous = previous_host_identity();
    assert_ne!(
        previous.fingerprint_sha256, server.fingerprint_sha256,
        "the saved key must differ from the loopback server key"
    );
    let previous_fingerprint_sha256 = previous.fingerprint_sha256.clone();
    let known_hosts = Arc::new(TestKnownHostsRepository::default());
    known_hosts.trust_fingerprint(
        &profile.endpoint,
        &previous.algorithm,
        &previous.fingerprint_sha256,
    );
    let service = connection_service(
        profile.clone(),
        Arc::new(TestCredentialResolver::default()),
        known_hosts.clone(),
    );
    RotationHarness {
        server,
        profile,
        known_hosts,
        service,
        previous_fingerprint_sha256,
    }
}

/// 从仓库已有的有效测试公钥计算旧指纹，避免用格式伪造值代替轮换前状态。
///
/// @author kongweiguang
fn previous_host_identity() -> HostKeyIdentity {
    let key = PublicKey::from_openssh(PREVIOUS_HOST_PUBLIC_KEY)
        .expect("previous host key fixture is valid OpenSSH data");
    HostKeyIdentity {
        algorithm: key.algorithm().to_string(),
        fingerprint_sha256: key.fingerprint(HashAlg::Sha256).to_string(),
        public_key_openssh: PREVIOUS_HOST_PUBLIC_KEY.to_owned(),
    }
}

/// 将同一 HostId、目标 peer、endpoint 与前后指纹绑定到单次轮换确认。
///
/// @author kongweiguang
fn rotation_request(harness: &RotationHarness) -> HostKeyRotationRequest {
    HostKeyRotationRequest {
        host_id: harness.profile.host_id.clone(),
        peer: HostKeyPeer::Target,
        endpoint: harness.profile.endpoint.clone(),
        expected_previous_fingerprint_sha256: harness.previous_fingerprint_sha256.clone(),
        expected_fingerprint_sha256: harness.server.fingerprint_sha256.clone(),
    }
}

/// 实际 loopback handshake 观察到新 key 后，known_hosts 只将指定旧指纹 CAS 替换为新 key。
///
/// @author kongweiguang
#[tokio::test]
async fn host_key_rotation_rehandshakes_and_replaces_saved_key() {
    let harness = rotation_harness().await;

    let receipt = harness
        .service
        .replace_host_key(rotation_request(&harness), CancellationToken::new())
        .await
        .expect("the current loopback key matches the explicitly confirmed rotation");

    assert_eq!(receipt.host_id, harness.profile.host_id);
    assert_eq!(receipt.peer, HostKeyPeer::Target);
    assert_eq!(receipt.endpoint, harness.profile.endpoint);
    assert_eq!(
        receipt.fingerprint_sha256,
        harness.server.fingerprint_sha256
    );
    assert_eq!(
        harness
            .server
            .accepted_connections
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "rotation performs a real SSH handshake against the loopback service"
    );

    let observed = harness
        .service
        .inspect_host_key(
            harness.profile.host_id.clone(),
            HostKeyPeer::Target,
            CancellationToken::new(),
        )
        .await
        .expect("the post-rotation verification handshakes with the loopback service");
    assert_eq!(observed.status, HostKeyStatus::Known);
    assert_eq!(
        observed.fingerprint_sha256,
        harness.server.fingerprint_sha256
    );
    harness.server.shutdown().await;
}

/// 错误 endpoint、peer 或新指纹不得改写旧信任；指纹错误分支仍须经过真实握手。
///
/// @author kongweiguang
#[tokio::test]
async fn host_key_rotation_rejects_mismatched_bindings_without_overwriting_trust() {
    let harness = rotation_harness().await;

    let mut wrong_endpoint = rotation_request(&harness);
    wrong_endpoint.endpoint.host = "127.0.0.2".to_owned();
    let endpoint_error = harness
        .service
        .replace_host_key(wrong_endpoint, CancellationToken::new())
        .await
        .expect_err("a stale endpoint is rejected before connecting");
    assert_eq!(endpoint_error.public_code(), PublicErrorCode::Conflict);

    let mut wrong_peer = rotation_request(&harness);
    wrong_peer.peer = HostKeyPeer::Jump { index: 0 };
    let peer_error = harness
        .service
        .replace_host_key(wrong_peer, CancellationToken::new())
        .await
        .expect_err("a peer absent from the selected route is rejected");
    assert_eq!(peer_error.public_code(), PublicErrorCode::NotFound);
    assert_eq!(
        harness
            .server
            .accepted_connections
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "endpoint and peer binding failures do not connect to any target"
    );

    let mut wrong_fingerprint = rotation_request(&harness);
    wrong_fingerprint.expected_fingerprint_sha256 = "SHA256:incorrect".to_owned();
    let fingerprint_error = harness
        .service
        .replace_host_key(wrong_fingerprint, CancellationToken::new())
        .await
        .expect_err("a live key different from the confirmed fingerprint is rejected");
    assert_eq!(
        fingerprint_error.public_code(),
        PublicErrorCode::HostKeyTrustMismatch
    );
    assert_eq!(
        harness
            .server
            .accepted_connections
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "new-key confirmation is checked against a real SSH handshake"
    );

    let unchanged = harness
        .service
        .inspect_host_key(
            harness.profile.host_id.clone(),
            HostKeyPeer::Target,
            CancellationToken::new(),
        )
        .await
        .expect("the trust state can be inspected after a rejected attempt");
    assert_eq!(
        unchanged.status,
        HostKeyStatus::Changed {
            previous_fingerprint_sha256: harness.previous_fingerprint_sha256.clone(),
        }
    );
    harness.server.shutdown().await;
}

/// 请求在入口前已取消时不得握手或修改 known_hosts 中的轮换前指纹。
///
/// @author kongweiguang
#[tokio::test]
async fn pre_cancelled_host_key_rotation_preserves_saved_key_without_connecting() {
    let harness = rotation_harness().await;
    let cancel = CancellationToken::new();
    cancel.cancel();

    let error = harness
        .service
        .replace_host_key(rotation_request(&harness), cancel)
        .await
        .expect_err("a pre-cancelled rotation stops before starting work");
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(
        harness
            .server
            .accepted_connections
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "pre-cancellation occurs before network connection"
    );

    let current = HostKeyIdentity {
        algorithm: "ecdsa-sha2-nistp256".to_owned(),
        fingerprint_sha256: harness.server.fingerprint_sha256.clone(),
        public_key_openssh: String::new(),
    };
    assert_eq!(
        harness
            .known_hosts
            .inspect(&harness.profile.endpoint, &current)
            .await
            .expect("in-memory known_hosts inspection succeeds"),
        HostKeyStatus::Changed {
            previous_fingerprint_sha256: harness.previous_fingerprint_sha256.clone(),
        }
    );
    harness.server.shutdown().await;
}
