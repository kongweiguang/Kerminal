//! 主机密钥回读后的连接意图生命周期。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::{
    AppError, AppResult,
    connections::{HostKeyChallenge, HostKeyIdentity, HostKeyPeer, HostKeyStatus},
    platform::external_launch::{
        ExternalLaunchIntent, ExternalRoutedTarget, ExternalTargetClassification,
        ExternalTargetIdentity, ExternalTargetLease, ExternalTargetLeaseOwner,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    files::FileEndpoint,
    identity::HostId,
};

use super::{HostKeyMutationReadback, PendingAction, external_action_host, readback_ends_action};

struct TestLeaseOwner;

impl ExternalTargetLeaseOwner for TestLeaseOwner {
    /// 此回归只验证事件归属；保留动作无需读取真实连接注册表。
    ///
    /// @author kongweiguang
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        Ok(())
    }

    /// 测试租约没有外部资源，释放不触发持久化或网络操作。
    ///
    /// @author kongweiguang
    fn release_scope(&self, _host_id: &HostId) {}
}

/// 构造绑定稳定 endpoint 的读回结果，测试只改变实际相关的信任状态。
///
/// @author kongweiguang
fn challenge(status: HostKeyStatus) -> HostKeyChallenge {
    HostKeyChallenge {
        host_id: HostId::parse("external:readback-test").expect("valid external ID"),
        peer: HostKeyPeer::Target,
        endpoint: HostEndpoint {
            host: "host.example".to_owned(),
            port: 22,
            username: "alice".to_owned(),
        },
        algorithm: "ssh-ed25519".to_owned(),
        fingerprint_sha256: "SHA256:test".to_owned(),
        identity: HostKeyIdentity {
            algorithm: "ssh-ed25519".to_owned(),
            fingerprint_sha256: "SHA256:test".to_owned(),
            public_key_openssh: "ssh-ed25519 AAAA".to_owned(),
        },
        status,
    }
}

/// 确认中的未知或变化密钥保留意图；用户取消和无法安全续接的结果终止意图。
///
/// @author kongweiguang
#[test]
fn readback_keeps_reconfirmable_challenges_and_terminates_cancelled_or_failed_requests() {
    let trusted = HostKeyMutationReadback::Trusted;
    let unknown = HostKeyMutationReadback::Current(challenge(HostKeyStatus::Unknown));
    let changed = HostKeyMutationReadback::Current(challenge(HostKeyStatus::Changed {
        previous_fingerprint_sha256: "SHA256:old".to_owned(),
    }));
    let different_trusted = HostKeyMutationReadback::Current(challenge(HostKeyStatus::Known));
    let endpoint_changed = HostKeyMutationReadback::EndpointChanged;
    let failed = HostKeyMutationReadback::Failed(AppError::infrastructure("readback failed"));

    for result in [&trusted, &unknown, &changed] {
        assert!(!readback_ends_action(result, false));
        assert!(readback_ends_action(result, true));
    }
    for result in [&different_trusted, &endpoint_changed, &failed] {
        assert!(readback_ends_action(result, false));
        assert!(readback_ends_action(result, true));
    }
}

/// 只有被丢弃的外部目标发出对应 HostId，普通文件动作不能误清理外部重试。
///
/// @author kongweiguang
#[test]
fn cancellation_identity_belongs_to_the_external_target_only() {
    let challenge = challenge(HostKeyStatus::Unknown);
    let host_id = challenge.host_id.clone();
    let external = PendingAction::External(ExternalRoutedTarget {
        lease: ExternalTargetLease::new_registered(host_id.clone(), Arc::new(TestLeaseOwner))
            .expect("registered external test lease"),
        identity: ExternalTargetIdentity {
            protocol: RemoteProtocol::Ssh,
            endpoint: challenge.endpoint,
            jump_hosts: Vec::new(),
        },
        classification: ExternalTargetClassification::RestrictedUnknown,
        intent: ExternalLaunchIntent::SshTerminal {
            remote_command: Vec::new(),
        },
        display_name: None,
        authentication_required: true,
        password_retry_available: false,
        open_sftp_after_connect: false,
    });
    let ordinary = PendingAction::Sftp {
        request_id: "request".to_owned(),
        endpoint: FileEndpoint::Local { path: "/".into() },
    };

    assert_eq!(external_action_host(&external), Some(host_id));
    assert_eq!(external_action_host(&ordinary), None);
}
