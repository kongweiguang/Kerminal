//! 跳板实体重排后的配置、CAS 与真实加密 Vault 凭据归属回归。
//!
//! @author kongweiguang

use std::sync::Arc;

use kerminal_application::connections::{
    CredentialResolver, ResolvedCredential,
    credentials::{CredentialService, CredentialVaultRepository, SecretValue},
    management::{
        HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft,
        HostManagementRepository, HostManagementService, HostRouteSettings, ManagedHost,
        ManagedJumpHost,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostEndpoint, RemoteProtocol, SshAuthentication,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
    vault::VaultStore,
};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// 密码、私钥和口令均实际改变索引后再保存重开，避免只验证仍在原位的私钥而漏掉归属错误。
/// @author kongweiguang
#[tokio::test]
async fn jump_credentials_follow_their_hosts_after_reorder_and_deletion() {
    let root = tempdir().expect("隔离配置根");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let repository = HostConfigurationRepository::new(configuration.clone());
    let service = HostManagementService::with_credentials(
        Arc::new(repository.clone()),
        CredentialService::new(vault.clone()),
    );
    let host_id = HostId::parse("jump-reorder-host").expect("主机 ID 合法");
    let draft = ssh_draft(host_id.clone());

    let created = service
        .save_host_with_credentials(
            draft,
            None,
            vec![
                replace(HostCredentialSlot::TargetPassword, "target secret"),
                replace(HostCredentialSlot::JumpPassword(0), "jump A password"),
                replace(HostCredentialSlot::JumpPrivateKey(1), "jump B private key"),
                replace(
                    HostCredentialSlot::JumpKeyPassphrase(1),
                    "jump B passphrase",
                ),
                replace(HostCredentialSlot::JumpPassword(2), "jump C password"),
            ],
            CancellationToken::new(),
        )
        .await
        .expect("主机和加密凭据一并保存")
        .host;
    let original_a = password_reference(&created.route.jump_hosts[0]);
    let original_b_key = private_key_reference(&created.route.jump_hosts[1]);
    let original_b_passphrase = private_key_passphrase_reference(&created.route.jump_hosts[1]);
    let original_c = password_reference(&created.route.jump_hosts[2]);
    let target = password_authentication_reference(&created.authentication);

    let mut reordered = draft_from_host(&created);
    reordered.route.jump_hosts = vec![
        created.route.jump_hosts[1].clone(),
        created.route.jump_hosts[2].clone(),
    ];
    let saved = service
        .save_host_with_credentials(
            reordered,
            Some(created.revision.clone()),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("重排并删除前项后仍可保存")
        .host;

    assert_eq!(password_reference(&saved.route.jump_hosts[1]), original_c);
    assert_eq!(
        private_key_reference(&saved.route.jump_hosts[0]),
        original_b_key
    );
    assert_eq!(
        private_key_passphrase_reference(&saved.route.jump_hosts[0]),
        original_b_passphrase
    );
    assert!(
        !vault
            .contains(&original_a)
            .await
            .expect("检查已删除跳板的密文")
    );

    drop(service);
    drop(repository);
    drop(configuration);
    drop(vault);

    let reopened = ConfigurationStore::initialize(root.path()).expect("重开配置");
    let reopened_vault = Arc::new(VaultStore::open(reopened.storage_coordinator()));
    let reopened_repository = HostConfigurationRepository::new(reopened.clone());
    let readback = reopened_repository
        .load_host(&host_id)
        .await
        .expect("读取已保存主机")
        .expect("主机存在");
    assert_eq!(
        password_reference(&readback.route.jump_hosts[1]),
        original_c
    );
    assert_eq!(
        private_key_reference(&readback.route.jump_hosts[0]),
        original_b_key
    );
    assert_eq!(
        private_key_passphrase_reference(&readback.route.jump_hosts[0]),
        original_b_passphrase
    );
    assert_password(&reopened_vault, &original_c, "jump C password").await;
    assert_private_key(&reopened_vault, &original_b_key, b"jump B private key").await;
    assert_password(&reopened_vault, &original_b_passphrase, "jump B passphrase").await;
    assert_password(&reopened_vault, &target, "target secret").await;

    let readonly_service = HostManagementService::new(Arc::new(reopened_repository.clone()));
    let target_mixed = draft_with_jump_password(&readback, target.clone());
    assert!(
        readonly_service
            .save_host(target_mixed, Some(readback.revision.clone()))
            .await
            .is_err(),
        "target-scoped references must not become jump credentials"
    );

    let foreign_host = HostId::new();
    let foreign_jump_reference = CredentialReference::new(
        CredentialKind::parse("jump-host").expect("credential kind is valid"),
        foreign_host,
        CredentialScope::jump(0),
        CredentialMaterial::Password,
    )
    .expect("foreign typed reference is structurally valid")
    .reference()
    .clone();
    let cross_host_mixed = draft_with_jump_password(&readback, foreign_jump_reference);
    assert!(
        readonly_service
            .save_host(cross_host_mixed, Some(readback.revision.clone()))
            .await
            .is_err(),
        "another host's jump reference must not enter this route"
    );

    let unchanged = reopened_repository
        .load_host(&host_id)
        .await
        .expect("确认失败写入没有损坏配置")
        .expect("主机仍存在");
    assert_eq!(unchanged.revision, readback.revision);
}

/// 建立三类跳板认证的最小 SSH 草稿，秘密通过后续独立凭据变更保存。
/// @author kongweiguang
fn ssh_draft(host_id: HostId) -> HostDraft {
    let jump = |name: &str, mode| ManagedJumpHost {
        name: name.to_owned(),
        endpoint: HostEndpoint {
            host: format!("{name}.example.invalid"),
            port: 22,
            username: format!("{name}-user"),
        },
        authentication: None,
        authentication_mode: mode,
    };
    HostDraft {
        id: Some(host_id),
        group_id: None,
        name: "jump credential fixture".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "target.example.invalid".to_owned(),
            port: 22,
            username: "target-user".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Password,
        rdp_password: None,
        route: HostRouteSettings {
            proxy: None,
            jump_hosts: vec![
                jump("jump-a", HostAuthenticationMode::Password),
                jump("jump-b", HostAuthenticationMode::PrivateKey),
                jump("jump-c", HostAuthenticationMode::Password),
            ],
        },
        serial: None,
        tags: Vec::new(),
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    }
}

/// 保存回执投影回无秘密草稿并保留 revision，以覆盖真实 CAS 更新路径。
/// @author kongweiguang
fn draft_from_host(host: &ManagedHost) -> HostDraft {
    HostDraft {
        id: Some(host.id.clone()),
        group_id: host.group_id.clone(),
        name: host.name.clone(),
        note: host.note.clone(),
        protocol: host.protocol,
        endpoint: host.endpoint.clone(),
        authentication: host.authentication.clone(),
        authentication_mode: host.authentication_mode,
        rdp_password: host.rdp_password.clone(),
        route: host.route.clone(),
        serial: host.serial.clone(),
        tags: host.tags.clone(),
        sort_order: host.sort_order,
        connect_timeout_ms: host.connect_timeout_ms,
        keepalive_interval_ms: host.keepalive_interval_ms,
        terminal_options: host.terminal_options.clone(),
        sftp_options: host.sftp_options.clone(),
        rdp_display: host.rdp_display.clone(),
    }
}

/// 用现有 target 口令引用制造一次错误路由草稿，验证 infra 会拒绝跨作用域复用。
/// @author kongweiguang
fn draft_with_jump_password(host: &ManagedHost, reference: CredentialRef) -> HostDraft {
    let mut draft = draft_from_host(host);
    let jump = draft
        .route
        .jump_hosts
        .first_mut()
        .expect("fixture retains a jump host");
    jump.authentication_mode = HostAuthenticationMode::Password;
    jump.authentication = Some(SshAuthentication::Password { secret: reference });
    draft
}

/// 构造按当前 host、kind、scope 和材料类型唯一的一条测试更新。
/// @author kongweiguang
fn replace(slot: HostCredentialSlot, value: &str) -> HostCredentialChange {
    HostCredentialChange::Replace {
        slot,
        secret: SecretValue::new(value.to_owned()),
    }
}

/// 读取已保存密码的 opaque ref，不触碰也不复制 vault 明文。
/// @author kongweiguang
fn password_reference(jump: &ManagedJumpHost) -> CredentialRef {
    match jump.authentication.as_ref() {
        Some(SshAuthentication::Password { secret }) => secret.clone(),
        _ => panic!("jump should retain password authentication"),
    }
}

/// 读取私钥 vault ref 以确认它始终随私钥认证实体移动。
/// @author kongweiguang
fn private_key_reference(jump: &ManagedJumpHost) -> CredentialRef {
    match jump.authentication.as_ref() {
        Some(SshAuthentication::PrivateKey {
            secret: Some(secret),
            ..
        }) => secret.clone(),
        _ => panic!("jump should retain its private key reference"),
    }
}

/// 读取私钥口令 ref，确保它没有因索引变化与另一跳板的私钥交叉绑定。
/// @author kongweiguang
fn private_key_passphrase_reference(jump: &ManagedJumpHost) -> CredentialRef {
    match jump.authentication.as_ref() {
        Some(SshAuthentication::PrivateKey {
            passphrase: Some(passphrase),
            ..
        }) => passphrase.clone(),
        _ => panic!("jump should retain its passphrase reference"),
    }
}

/// 从目标密码认证读取 target ref，用于验证它不会通过 jump 的兼容 scope 校验。
/// @author kongweiguang
fn password_authentication_reference(authentication: &Option<SshAuthentication>) -> CredentialRef {
    match authentication {
        Some(SshAuthentication::Password { secret }) => secret.clone(),
        _ => panic!("target should retain password authentication"),
    }
}

/// 在真实加密 Vault 中核对重排后仍解析到正确密码或私钥口令。
/// @author kongweiguang
async fn assert_password(vault: &VaultStore, reference: &CredentialRef, expected: &str) {
    let resolved = vault
        .resolve(reference, &CancellationToken::new())
        .await
        .unwrap_or_else(|_| panic!("vault password reference resolves"));
    match resolved {
        ResolvedCredential::Password(value) | ResolvedCredential::KeyPassphrase(value) => {
            assert_eq!(value.as_str(), expected);
        }
        ResolvedCredential::PrivateKey(_) => panic!("expected password material"),
    }
}

/// 在真实加密 Vault 中核对私钥材料仍属于原跳板。
/// @author kongweiguang
async fn assert_private_key(vault: &VaultStore, reference: &CredentialRef, expected: &[u8]) {
    let resolved = vault
        .resolve(reference, &CancellationToken::new())
        .await
        .unwrap_or_else(|_| panic!("vault private key reference resolves"));
    match resolved {
        ResolvedCredential::PrivateKey(value) => assert_eq!(value.as_slice(), expected),
        ResolvedCredential::Password(_) | ResolvedCredential::KeyPassphrase(_) => {
            panic!("expected private key material")
        }
    }
}
