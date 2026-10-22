//! SSH adapter 在路由边界验证代理凭据绑定关系的私有单元测试。
//!
//! @author kongweiguang

use super::validate_profile;
use kerminal_application::AppError;
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, ProxyProfile, ProxyProtocol,
        RemoteProtocol, RemoteShellDialect, SshAuthentication, SshRoute,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
};

/// 按测试参数创建一条代理 password ref，不将该 ref 解析成或暴露为秘密。
fn credential_ref(
    kind: &str,
    host_id: &HostId,
    scope: &str,
    material: CredentialMaterial,
) -> CredentialRef {
    CredentialReference::new(
        CredentialKind::parse(kind).expect("valid kind"),
        host_id.clone(),
        CredentialScope::parse(scope).expect("valid scope"),
        material,
    )
    .expect("valid typed credential reference")
    .reference()
    .clone()
}

/// 创建显式身份和代理引用的 profile，验证逻辑不需要打开网络或解析用户配置。
fn profile_with_proxy(
    host_id: HostId,
    username: &str,
    password: Option<CredentialRef>,
) -> HostConnectionProfile {
    HostConnectionProfile {
        host_id,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "target.example".to_owned(),
            port: 22,
            username: "target-user".to_owned(),
        },
        authentication: Some(SshAuthentication::Agent),
        rdp_password: None,
        route: SshRoute {
            proxy: Some(ProxyProfile {
                protocol: ProxyProtocol::Socks5,
                endpoint: HostEndpoint {
                    host: "proxy.example".to_owned(),
                    port: 1080,
                    username: username.to_owned(),
                },
                password,
            }),
            jump_hosts: Vec::new(),
        },
        shell_dialect: RemoteShellDialect::Unknown,
        connect_timeout_ms: 10_000,
        keepalive_interval_ms: None,
    }
}

/// 新业务引用仅在 kind、实际 HostId、proxy scope、Password 和用户名全部匹配时通过。
#[test]
fn validates_proxy_reference_at_the_host_route_boundary() {
    let host_id = HostId::new();
    let reference = credential_ref(
        "proxy-host",
        &host_id,
        "proxy-00112233445566778899aabbccddeeff",
        CredentialMaterial::Password,
    );
    let profile = profile_with_proxy(host_id, "proxy-user", Some(reference));

    assert!(validate_profile(&profile).is_ok());
}

/// 不同槽、不同主机、target scope、非密码材料和缺少代理用户名都 fail closed。
#[test]
fn rejects_cross_slot_or_unusable_proxy_references() {
    let host_id = HostId::new();
    let other_host_id = HostId::new();
    let invalid_references = [
        credential_ref(
            "ssh-host",
            &host_id,
            "proxy-00112233445566778899aabbccddeeff",
            CredentialMaterial::Password,
        ),
        credential_ref(
            "proxy-host",
            &other_host_id,
            "proxy-00112233445566778899aabbccddeeff",
            CredentialMaterial::Password,
        ),
        credential_ref(
            "proxy-host",
            &host_id,
            "target-unique",
            CredentialMaterial::Password,
        ),
        credential_ref(
            "proxy-host",
            &host_id,
            "proxy-00112233445566778899aabbccddeeff",
            CredentialMaterial::PrivateKey,
        ),
        credential_ref(
            "proxy-host",
            &host_id,
            "proxy-nottransaction",
            CredentialMaterial::Password,
        ),
    ];
    for reference in invalid_references {
        let profile = profile_with_proxy(host_id.clone(), "proxy-user", Some(reference));
        assert!(matches!(
            validate_profile(&profile),
            Err(AppError::Invalid { .. })
        ));
    }

    let password = credential_ref(
        "proxy-host",
        &host_id,
        "proxy-00112233445566778899aabbccddeeff",
        CredentialMaterial::Password,
    );
    let profile = profile_with_proxy(host_id, "", Some(password));
    assert!(matches!(
        validate_profile(&profile),
        Err(AppError::Invalid { .. })
    ));
}
