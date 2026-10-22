//! 未保存连接探测用例的外置单元测试。
//!
//! @author kongweiguang

use super::*;
use crate::connections::credentials::SecretValue;
use crate::connections::management::HostRouteSettings;
use std::path::PathBuf;

/// 构造不会访问任何资源的合法 SSH 草稿，测试探测计划本身的边界。
///
/// @author kongweiguang
fn ssh_draft(mode: HostAuthenticationMode) -> HostDraft {
    HostDraft {
        id: None,
        group_id: None,
        name: "probe test".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 22,
            username: "user".to_owned(),
        },
        authentication: match mode {
            HostAuthenticationMode::Agent => Some(SshAuthentication::Agent),
            HostAuthenticationMode::Password => None,
            HostAuthenticationMode::PrivateKey => Some(SshAuthentication::PrivateKey {
                path: Some(PathBuf::from("C:/keys/id_ed25519")),
                secret: None,
                passphrase: None,
            }),
        },
        authentication_mode: mode,
        rdp_password: None,
        route: HostRouteSettings::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    }
}

/// 以 request 构造器减少测试关注点，默认不提供网络确认或凭据。
fn request(draft: HostDraft, changes: Vec<HostCredentialChange>) -> ConnectionProbeRequest {
    ConnectionProbeRequest {
        draft,
        credential_changes: changes,
        timeout: Duration::from_secs(10),
        host_key_confirmation: None,
    }
}

/// 用自有字符串创建一次性秘密，保持探测 fixture 与零化凭据接口一致。
#[test]
fn probe_plan_uses_ephemeral_reference_for_new_password() {
    let built = build_probe_target(request(
        ssh_draft(HostAuthenticationMode::Password),
        vec![HostCredentialChange::Replace {
            slot: HostCredentialSlot::TargetPassword,
            secret: SecretValue::new("test secret".to_owned()),
        }],
    ))
    .expect("valid SSH probe draft");

    let SshAuthentication::Password { secret } = built
        .profile
        .authentication
        .as_ref()
        .expect("password auth")
    else {
        panic!("password mode must remain password auth");
    };
    assert_eq!(built.transient_credentials.len(), 1);
    assert_eq!(secret, &built.transient_credentials[0].reference);
    let ProbeCredentialMaterial::Password(value) = &built.transient_credentials[0].material else {
        panic!("target password must keep password material");
    };
    assert_eq!(value.as_str(), "test secret");
    assert!(!format!("{:?}", built.transient_credentials[0]).contains("test secret"));
}

/// 确认 RDP 可用性探测不会把输入的秘密放进客户端或网络目标。
#[test]
fn probe_plan_drops_rdp_password_without_client_or_network_payload() {
    let mut draft = ssh_draft(HostAuthenticationMode::Agent);
    draft.protocol = RemoteProtocol::Rdp;
    draft.endpoint = HostEndpoint {
        host: "rdp.example.invalid".to_owned(),
        port: 3389,
        username: String::new(),
    };
    draft.authentication = None;

    let built = build_probe_target(request(
        draft,
        vec![HostCredentialChange::Replace {
            slot: HostCredentialSlot::RdpPassword,
            secret: SecretValue::new("must never reach client".to_owned()),
        }],
    ))
    .expect("valid availability-only RDP probe");

    assert!(built.transient_credentials.is_empty());
    assert!(built.profile.rdp_password.is_none());
}

/// 确保 SFTP 探测只传递已校验的起始目录，未配置时与 UI 一致指向根目录。
///
/// @author kongweiguang
#[test]
fn sftp_probe_plan_carries_normalized_start_directory_and_root_default() {
    let mut draft = ssh_draft(HostAuthenticationMode::Agent);
    draft.protocol = RemoteProtocol::Sftp;
    draft.sftp_options.remote_start_directory = Some("/srv//workspace/./project".to_owned());
    let configured = build_probe_target(request(draft, Vec::new())).expect("valid SFTP path");
    assert_eq!(
        configured
            .sftp_remote_start_directory
            .as_ref()
            .map(|path| path.as_str()),
        Some("/srv/workspace/project")
    );

    let mut draft = ssh_draft(HostAuthenticationMode::Agent);
    draft.protocol = RemoteProtocol::Sftp;
    let defaulted = build_probe_target(request(draft, Vec::new())).expect("root directory");
    assert_eq!(
        defaulted
            .sftp_remote_start_directory
            .as_ref()
            .map(|path| path.as_str()),
        Some("/")
    );
}

/// 拒绝 probe 路径绕过保存规则进入 SFTP adapter，避免探测语义与重开配置不一致。
///
/// @author kongweiguang
#[test]
fn sftp_probe_plan_rejects_relative_and_parent_start_directories() {
    for path in ["relative/path", "/srv/../etc"] {
        let mut draft = ssh_draft(HostAuthenticationMode::Agent);
        draft.protocol = RemoteProtocol::Sftp;
        draft.sftp_options.remote_start_directory = Some(path.to_owned());
        assert!(
            build_probe_target(request(draft, Vec::new())).is_err(),
            "{path}"
        );
    }
}

#[test]
fn probe_plan_rejects_wrong_protocol_credential_slot_and_missing_password() {
    let telnet_draft = HostDraft {
        protocol: RemoteProtocol::Telnet,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 23,
            username: String::new(),
        },
        authentication: None,
        serial: None,
        ..ssh_draft(HostAuthenticationMode::Agent)
    };
    assert!(
        build_probe_target(request(
            telnet_draft,
            vec![HostCredentialChange::Clear {
                slot: HostCredentialSlot::TargetPassword,
            }],
        ))
        .is_err()
    );

    assert!(
        build_probe_target(request(
            ssh_draft(HostAuthenticationMode::Password),
            vec![HostCredentialChange::Clear {
                slot: HostCredentialSlot::TargetPassword,
            }],
        ))
        .is_err()
    );
}

#[test]
fn duplicate_credential_slot_is_rejected_before_probe() {
    let changes = vec![
        HostCredentialChange::Clear {
            slot: HostCredentialSlot::TargetPassword,
        },
        HostCredentialChange::Clear {
            slot: HostCredentialSlot::TargetPassword,
        },
    ];
    assert!(
        build_probe_target(request(
            ssh_draft(HostAuthenticationMode::Password),
            changes,
        ))
        .is_err()
    );
}

#[test]
fn confirmation_is_only_allowed_for_network_ssh_protocols() {
    let mut draft = ssh_draft(HostAuthenticationMode::Agent);
    draft.protocol = RemoteProtocol::Rdp;
    draft.endpoint = HostEndpoint {
        host: "rdp.example.invalid".to_owned(),
        port: 3389,
        username: String::new(),
    };
    draft.authentication = None;
    let confirmation = ProbeHostKeyConfirmation {
        peer: HostKeyPeer::Target,
        endpoint: draft.endpoint.clone(),
        fingerprint_sha256: "SHA256:fixture".to_owned(),
    };
    let mut request = request(draft, Vec::new());
    request.host_key_confirmation = Some(confirmation);
    assert!(build_probe_target(request).is_err());
}
