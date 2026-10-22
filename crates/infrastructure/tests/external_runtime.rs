//! 外部临时连接 registry 的凭据隔离、生命周期和容量边界测试。
//!
//! @author kongweiguang

use std::sync::Arc;

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{CredentialResolver, HostConnectionRepository, ResolvedCredential},
    platform::external_launch::{
        ExternalAuthenticationHint, ExternalTargetIdentity, ExternalTemporaryTargetRegistry,
        ExternalTemporaryTargetSpec,
    },
};
use kerminal_domain::{
    connections::{CredentialRef, HostConnectionProfile, HostEndpoint, RemoteProtocol},
    identity::HostId,
};
use kerminal_infrastructure::connections::external_runtime::ExternalRuntimeRegistry;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

struct MissingSavedHosts;

#[async_trait]
impl HostConnectionRepository for MissingSavedHosts {
    /// 测试只接受临时 ID 命中 registry，普通 ID 明确委托为未找到。
    async fn load(&self, _host_id: &HostId) -> AppResult<HostConnectionProfile> {
        Err(AppError::not_found("测试中无保存主机"))
    }
}

struct MissingSavedCredentials;

#[async_trait]
impl CredentialResolver for MissingSavedCredentials {
    /// 测试只解析临时外部凭据，持久化引用由假仓储明确拒绝。
    async fn resolve(
        &self,
        _reference: &CredentialRef,
        _cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        Err(AppError::not_found("测试中无保存凭据"))
    }
}

/// 建立仅含 fake fallback 的 registry，避免接触真实 host 文件或 vault。
fn isolated_registry() -> ExternalRuntimeRegistry {
    ExternalRuntimeRegistry::new(
        Arc::new(MissingSavedHosts),
        Arc::new(MissingSavedCredentials),
    )
}

/// 生成有效的外部 SSH 密码提示目标，不携带任何实际凭据材料。
fn password_target() -> ExternalTemporaryTargetSpec {
    ExternalTemporaryTargetSpec {
        identity: ExternalTargetIdentity {
            protocol: RemoteProtocol::Ssh,
            endpoint: HostEndpoint {
                host: "example.test".to_owned(),
                port: 22,
                username: "tester".to_owned(),
            },
            jump_hosts: Vec::new(),
        },
        display_name: Some("temporary test".to_owned()),
        authentication: ExternalAuthenticationHint::PasswordPrompt,
    }
}

/// 确认多个消费者各自持有引用，只有最后一个 lease drop 后 profile 与密码才不可访问。
#[tokio::test]
async fn temporary_password_exists_until_last_consumer_releases_lease() {
    let registry = isolated_registry();
    let lease = registry.register(password_target()).await.unwrap();
    let retained = lease.retain().unwrap();
    let host_id = lease.host_id().clone();
    assert!(host_id.as_str().starts_with("external:"));
    assert_eq!(registry.active_target_count().await, 1);

    let profile = HostConnectionRepository::load(&registry, &host_id)
        .await
        .unwrap();
    assert_eq!(profile.protocol, RemoteProtocol::Ssh);
    assert_eq!(profile.endpoint.host, "example.test");
    let credential_ref = match profile.authentication.unwrap() {
        kerminal_domain::connections::SshAuthentication::Password { secret } => secret,
        _ => panic!("密码提示必须生成密码认证 profile"),
    };
    let cancel = CancellationToken::new();
    assert!(
        CredentialResolver::resolve(&registry, &credential_ref, &cancel)
            .await
            .is_err()
    );

    registry
        .set_password(&host_id, Zeroizing::new("temporary-secret".to_owned()))
        .await
        .unwrap();
    match CredentialResolver::resolve(&registry, &credential_ref, &cancel)
        .await
        .unwrap()
    {
        ResolvedCredential::Password(secret) => assert_eq!(secret.as_str(), "temporary-secret"),
        _ => panic!("临时外部凭据必须以密码材料返回"),
    }

    drop(lease);
    assert_eq!(registry.active_target_count().await, 1);
    assert!(
        HostConnectionRepository::load(&registry, &host_id)
            .await
            .is_ok()
    );
    drop(retained);
    assert_eq!(registry.active_target_count().await, 0);
    assert!(
        HostConnectionRepository::load(&registry, &host_id)
            .await
            .is_err()
    );
    assert!(
        CredentialResolver::resolve(&registry, &credential_ref, &cancel)
            .await
            .is_err()
    );
}

/// 目标数达到固定上限时拒绝新注册，不自动淘汰会影响用户重连的临时身份。
#[tokio::test]
async fn registry_rejects_capacity_overflow_without_eviction() {
    let registry = isolated_registry();
    let mut registered = Vec::new();
    let mut leases = Vec::new();
    for _ in 0..64 {
        let lease = registry.register(password_target()).await.unwrap();
        registered.push(lease.host_id().clone());
        leases.push(lease);
    }
    assert!(registry.register(password_target()).await.is_err());
    assert_eq!(registry.active_target_count().await, 64);
    assert!(
        HostConnectionRepository::load(&registry, &registered[0])
            .await
            .is_ok()
    );
    drop(leases);
    assert_eq!(registry.active_target_count().await, 0);
}

/// 取消信号优先于秘密解析，避免已取消的外部连接继续交付认证材料。
#[tokio::test]
async fn cancelled_credential_resolution_does_not_return_password() {
    let registry = isolated_registry();
    let lease = registry.register(password_target()).await.unwrap();
    let host_id = lease.host_id().clone();
    registry
        .set_password(&host_id, Zeroizing::new("temporary-secret".to_owned()))
        .await
        .unwrap();
    let profile = HostConnectionRepository::load(&registry, &host_id)
        .await
        .unwrap();
    let credential_ref = match profile.authentication.unwrap() {
        kerminal_domain::connections::SshAuthentication::Password { secret } => secret,
        _ => panic!("密码提示必须生成密码认证 profile"),
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(
        CredentialResolver::resolve(&registry, &credential_ref, &cancellation)
            .await
            .is_err()
    );
}

/// 进程级清空会立刻零化所有 entry，旧 lease 之后的 retain 失败且 Drop 不再次改动状态。
#[tokio::test]
async fn clear_all_invalidates_existing_leases_and_removes_profiles() {
    let registry = isolated_registry();
    let lease = registry.register(password_target()).await.unwrap();
    let consumer = lease.retain().unwrap();
    let host_id = lease.host_id().clone();

    registry.clear_all().await.unwrap();

    assert_eq!(registry.active_target_count().await, 0);
    assert!(lease.retain().is_err());
    assert!(
        HostConnectionRepository::load(&registry, &host_id)
            .await
            .is_err()
    );
    drop(lease);
    drop(consumer);
    assert_eq!(registry.active_target_count().await, 0);
}
