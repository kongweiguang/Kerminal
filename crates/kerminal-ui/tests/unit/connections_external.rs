//! 外部连接动作与 caller-provided host-key pin 的纯逻辑测试。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use kerminal_application::{
    AppResult,
    platform::external_launch::{
        ExternalLaunchIntent, ExternalLaunchOutcome, ExternalRoutedTarget,
        ExternalTargetClassification, ExternalTargetIdentity, ExternalTargetLease,
        ExternalTargetLeaseOwner,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::HostId,
};

use super::{
    expected_fingerprint_matches, external_intent_supported, take_current_external_username_result,
    validate_external_username,
};

struct CountingLeaseOwner(Arc<AtomicUsize>);

impl ExternalTargetLeaseOwner for CountingLeaseOwner {
    /// 测试 owner 不需持久化；retain 只模拟有效 scope 的成功创建。
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        Ok(())
    }

    /// 记录过期 UI 回调丢弃最后一个 target 时的 RAII 释放。
    fn release_scope(&self, _host_id: &HostId) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// 只允许 SSH terminal 与 SFTP transfer 一一对应，避免外部参数被静默改成别的协议。
#[test]
fn external_intent_requires_its_matching_protocol() {
    let ssh = ExternalLaunchIntent::SshTerminal {
        remote_command: Vec::new(),
    };
    let sftp = ExternalLaunchIntent::SftpTransfer {
        remote_path: "/".to_owned(),
        expected_host_key: None,
    };

    assert!(external_intent_supported(RemoteProtocol::Ssh, &ssh));
    assert!(external_intent_supported(RemoteProtocol::Sftp, &sftp));
    assert!(!external_intent_supported(RemoteProtocol::Sftp, &ssh));
    assert!(!external_intent_supported(RemoteProtocol::Ssh, &sftp));
    assert!(!external_intent_supported(RemoteProtocol::Telnet, &ssh));
}

/// SHA-256 指纹只去首尾空格、不折叠大小写，以免把不同 Base64 key pin 当成相同值。
#[test]
fn expected_host_key_pin_is_trimmed_but_case_sensitive() {
    assert!(expected_fingerprint_matches(
        " SHA256:abC+123=\n",
        "SHA256:abC+123="
    ));
    assert!(!expected_fingerprint_matches(
        "SHA256:abC+123=",
        "SHA256:abc+123="
    ));
    assert!(!expected_fingerprint_matches("", "SHA256:abC+123="));
}

/// 显式用户名只做首尾清理，保留大小写并拒绝空值和控制字符。
#[test]
fn external_username_requires_explicit_safe_input() {
    assert_eq!(
        validate_external_username("  Alice  ").expect("trimmed username is valid"),
        "Alice"
    );
    assert!(validate_external_username("  ").is_err());
    assert!(validate_external_username("alice\nexample").is_err());
}

/// 取消后迟到的续接结果不得进入工作区，且结果中的临时目标 lease 必须释放。
#[test]
fn cancelled_username_prompt_drops_late_target_lease() {
    let released = Arc::new(AtomicUsize::new(0));
    let lease = ExternalTargetLease::new_registered(
        HostId::parse("external:late-username-test").expect("external host id is valid"),
        Arc::new(CountingLeaseOwner(Arc::clone(&released))),
    )
    .expect("registered lease is valid");
    let result = Ok(ExternalLaunchOutcome::Ready {
        target: ExternalRoutedTarget {
            lease,
            identity: ExternalTargetIdentity {
                protocol: RemoteProtocol::Ssh,
                endpoint: HostEndpoint {
                    host: "host.example".to_owned(),
                    port: 22,
                    username: "alice".to_owned(),
                },
                jump_hosts: Vec::new(),
            },
            classification: ExternalTargetClassification::RestrictedUnknown,
            intent: ExternalLaunchIntent::SshTerminal {
                remote_command: Vec::new(),
            },
            display_name: None,
            authentication_required: false,
            open_sftp_after_connect: false,
        },
    });

    assert!(take_current_external_username_result(3, 4, result).is_none());
    assert_eq!(released.load(Ordering::SeqCst), 1);
}
