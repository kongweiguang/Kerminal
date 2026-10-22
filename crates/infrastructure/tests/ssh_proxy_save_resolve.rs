//! 代理凭据从 host 管理保存、回读到 SSH resolver 的跨层回归。
//!
//! @author kongweiguang

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        ConnectionIntent, CredentialResolver, HostConnectionRepository, HostKeyIdentity,
        HostKeyStatus, KnownHostsRepository, RemoteConnectionService, ResolvedCredential,
        credentials::{
            CredentialService, CredentialVaultRepository, CredentialVaultSaveMetadata, SecretValue,
        },
        management::{
            HostAuthenticationMode, HostCredentialChange, HostCredentialSlot, HostDraft,
            HostGroupDraft, HostGroupSnapshot, HostManagementRepository, HostManagementService,
            HostRouteSettings, ManagedHost,
        },
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostConnectionProfile, HostEndpoint, ProxyProfile, ProxyProtocol,
        RemoteProtocol, RemoteShellDialect, SshAuthentication, SshJumpHost, SshRoute,
        credentials::CredentialReference,
    },
    identity::HostId,
    preferences::ConfigRevision,
};
use kerminal_infrastructure::connections::{
    RusshRemoteConnectionDependencies, RusshRemoteConnectionService,
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Default)]
struct MemoryHostRepository {
    hosts: Mutex<HashMap<String, ManagedHost>>,
}

#[async_trait]
impl HostManagementRepository for MemoryHostRepository {
    /// 本测试只保存新建主机，并以单一 mutex 模拟最小原子 CAS 边界。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }

    /// 返回完整内存快照，使 host 管理用例能够检查待清理旧引用。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.hosts.lock().await.values().cloned().collect())
    }

    /// 按稳定 HostId 回读刚保存的 typed route，模拟 runtime 与管理共用仓储。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.hosts.lock().await.get(host_id.as_str()).cloned())
    }

    /// 只接受首次创建，避免该跨层用例把并发更新语义混入代理引用验证。
    ///
    /// @author kongweiguang
    async fn save_host(
        &self,
        host_id: HostId,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        let mut hosts = self.hosts.lock().await;
        if expected_revision.is_some() || hosts.contains_key(host_id.as_str()) {
            return Err(AppError::conflict("测试主机已存在"));
        }
        let host = ManagedHost {
            id: host_id.clone(),
            group_id: draft.group_id,
            name: draft.name,
            note: draft.note,
            protocol: draft.protocol,
            endpoint: draft.endpoint,
            authentication: draft.authentication,
            authentication_mode: draft.authentication_mode,
            rdp_password: draft.rdp_password,
            route: draft.route,
            serial: draft.serial,
            tags: draft.tags,
            is_favorite: draft.is_favorite,
            sort_order: draft.sort_order,
            connect_timeout_ms: draft.connect_timeout_ms,
            keepalive_interval_ms: draft.keepalive_interval_ms,
            terminal_options: draft.terminal_options,
            sftp_options: draft.sftp_options,
            rdp_display: draft.rdp_display,
            created_at: "test-created".to_owned(),
            updated_at: "test-updated".to_owned(),
            revision: ConfigRevision::from_sha256("test-revision".to_owned()),
        };
        hosts.insert(host_id.as_str().to_owned(), host.clone());
        Ok(host)
    }

    /// 此跨层夹具只覆盖凭据解析，不实现与代理验证无关的主机删除。
    async fn delete_host(
        &self,
        _host_id: &HostId,
        _expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        Err(AppError::unsupported("测试仓储不支持主机删除"))
    }

    /// 测试不修改分组，因此主机草稿应不需要隐式创建 group 文件。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot::default())
    }

    /// 阻止测试仓储意外承担与代理凭据无关的分组写入。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("测试仓储不支持分组编辑"))
    }

    /// 此跨层夹具不执行分组迁移事务，避免假装验证文件事务语义。
    async fn delete_group_and_reassign(
        &self,
        _group_id: String,
        _destination_group_id: Option<String>,
        _expected_group_revision: ConfigRevision,
        _expected_hosts: Vec<kerminal_application::connections::management::HostDocumentRevision>,
    ) -> AppResult<kerminal_application::connections::management::HostGroupDeletionReceipt> {
        Err(AppError::unsupported("测试仓储不支持分组迁移"))
    }
}

#[async_trait]
impl HostConnectionRepository for MemoryHostRepository {
    /// 把同一份管理快照投影为 SSH runtime profile，不另造代理配置来源。
    async fn load(&self, host_id: &HostId) -> AppResult<HostConnectionProfile> {
        let host = self
            .load_host(host_id)
            .await?
            .ok_or_else(|| AppError::not_found("测试主机不存在"))?;
        let jump_hosts = host
            .route
            .jump_hosts
            .into_iter()
            .map(|jump| {
                Ok(SshJumpHost {
                    endpoint: jump.endpoint,
                    authentication: jump
                        .authentication
                        .ok_or_else(|| AppError::invalid("测试跳板认证缺失"))?,
                })
            })
            .collect::<AppResult<Vec<_>>>()?;
        Ok(HostConnectionProfile {
            host_id: host.id,
            protocol: host.protocol,
            endpoint: host.endpoint,
            authentication: host.authentication,
            rdp_password: host.rdp_password,
            route: SshRoute {
                proxy: host.route.proxy,
                jump_hosts,
            },
            shell_dialect: RemoteShellDialect::Unknown,
            connect_timeout_ms: host.connect_timeout_ms,
            keepalive_interval_ms: host.keepalive_interval_ms,
        })
    }
}

#[derive(Default)]
struct MemoryVault {
    entries: Mutex<HashMap<String, Zeroizing<String>>>,
    resolved_references: Mutex<Vec<String>>,
}

#[async_trait]
impl CredentialVaultRepository for MemoryVault {
    /// 测试 vault 仅以 opaque 引用索引零化秘密，metadata 不复制秘密内容。
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        let reference_text = reference.reference().as_str().to_owned();
        let kind = reference.kind().as_str().to_owned();
        self.entries
            .lock()
            .await
            .insert(reference_text.clone(), secret);
        Ok(CredentialVaultSaveMetadata {
            entry_id: reference_text,
            kind,
            key_id: "test-key".to_owned(),
            updated_at: "test-time".to_owned(),
        })
    }

    /// 只检查 opaque entry 是否存在，绝不把测试秘密回传给管理 use case。
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool> {
        Ok(self.entries.lock().await.contains_key(reference.as_str()))
    }

    /// 删除未被 host metadata 引用的条目，保持应用保存器的 CAS 清理契约可用。
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        Ok(self
            .entries
            .lock()
            .await
            .remove(reference.as_str())
            .is_some())
    }
}

#[async_trait]
impl CredentialResolver for MemoryVault {
    /// 只为 runtime 调用短暂解密并记录被使用的 opaque ref，以证明保存结果到达解析边界。
    async fn resolve(
        &self,
        reference: &CredentialRef,
        cancel: &CancellationToken,
    ) -> AppResult<ResolvedCredential> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试凭据解析已取消"));
        }
        let secret = self
            .entries
            .lock()
            .await
            .get(reference.as_str())
            .map(|value| Zeroizing::new(value.to_string()))
            .ok_or_else(|| AppError::not_found("测试 vault entry 不存在"))?;
        self.resolved_references
            .lock()
            .await
            .push(reference.as_str().to_owned());
        Ok(ResolvedCredential::Password(secret))
    }
}

struct UnknownKnownHosts;

#[async_trait]
impl KnownHostsRepository for UnknownKnownHosts {
    /// 测试在 proxy handshake 后即终止，不应读取或写入任何 known_hosts 状态。
    async fn inspect(
        &self,
        _endpoint: &HostEndpoint,
        _identity: &HostKeyIdentity,
    ) -> AppResult<HostKeyStatus> {
        Ok(HostKeyStatus::Unknown)
    }

    /// 测试不进入 SSH host-key trust 阶段，因此任何信任写入都应保留无副作用语义。
    async fn trust(&self, _endpoint: &HostEndpoint, _identity: &HostKeyIdentity) -> AppResult<()> {
        Ok(())
    }

    /// 测试不执行密钥轮换；明确 Unsupported 以免把静态 fixture 状态误作持久信任。
    ///
    /// @author kongweiguang
    async fn replace(
        &self,
        _endpoint: &HostEndpoint,
        _expected_previous_fingerprint_sha256: &str,
        _identity: &HostKeyIdentity,
    ) -> AppResult<()> {
        Err(kerminal_application::AppError::unsupported(
            "test known_hosts fake does not support key rotation",
        ))
    }
}

/// 从业务用例新建 proxy credential，再经 host 回读与 production SSH route 解析同一个 ref。
///
/// @author kongweiguang
#[tokio::test]
async fn saved_proxy_reference_survives_readback_and_reaches_runtime_resolver() {
    let hosts = Arc::new(MemoryHostRepository::default());
    let vault = Arc::new(MemoryVault::default());
    let host_id = HostId::new();
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("loopback proxy port is available");
    let proxy_port = proxy_listener
        .local_addr()
        .expect("loopback proxy endpoint is readable")
        .port();
    let proxy_accept = tokio::spawn(async move {
        let (stream, _) = tokio::time::timeout(Duration::from_secs(2), proxy_listener.accept())
            .await
            .expect("runtime route reaches the loopback proxy in bounded time")
            .expect("runtime route reaches the loopback proxy");
        drop(stream);
    });

    let management = HostManagementService::with_credentials(
        hosts.clone(),
        CredentialService::new(vault.clone()),
    );
    let receipt = management
        .save_host_with_credentials(
            HostDraft {
                id: Some(host_id.clone()),
                group_id: None,
                name: "isolated proxy fixture".to_owned(),
                note: None,
                protocol: RemoteProtocol::Ssh,
                endpoint: HostEndpoint {
                    host: "target.example".to_owned(),
                    port: 22,
                    username: "target-user".to_owned(),
                },
                authentication: Some(SshAuthentication::Agent),
                authentication_mode: HostAuthenticationMode::Agent,
                rdp_password: None,
                route: HostRouteSettings {
                    proxy: Some(ProxyProfile {
                        protocol: ProxyProtocol::Socks5,
                        endpoint: HostEndpoint {
                            host: "127.0.0.1".to_owned(),
                            port: proxy_port,
                            username: "proxy-user".to_owned(),
                        },
                        password: None,
                    }),
                    jump_hosts: Vec::new(),
                },
                serial: None,
                tags: Vec::new(),
                is_favorite: false,
                sort_order: 0,
                connect_timeout_ms: 30_000,
                keepalive_interval_ms: Some(60_000),
                terminal_options: Default::default(),
                sftp_options: Default::default(),
                rdp_display: Default::default(),
            },
            None,
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::ProxyPassword,
                secret: SecretValue::new("loopback-only proxy secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("typed proxy secret and host route commit together");
    let saved_reference = receipt
        .host
        .route
        .proxy
        .as_ref()
        .and_then(|proxy| proxy.password.as_ref())
        .expect("host receipt contains the saved proxy reference")
        .clone();
    let read_back = hosts
        .load_host(&host_id)
        .await
        .expect("saved host can be read back")
        .expect("host exists after save");
    assert_eq!(
        read_back
            .route
            .proxy
            .as_ref()
            .and_then(|proxy| proxy.password.as_ref()),
        Some(&saved_reference),
        "the host store preserves the exact reference produced by the credential transaction"
    );

    let remote = RusshRemoteConnectionService::new(RusshRemoteConnectionDependencies {
        hosts: hosts.clone(),
        credentials: vault.clone(),
        known_hosts: Arc::new(UnknownKnownHosts),
    });
    let error = remote
        .connect(host_id, ConnectionIntent::Exec, CancellationToken::new())
        .await
        .err()
        .expect("the test proxy closes the route after credential resolution");
    tokio::time::timeout(Duration::from_secs(2), proxy_accept)
        .await
        .expect("test proxy accept completes")
        .expect("test proxy task joins");

    let parsed = CredentialReference::parse(saved_reference.clone())
        .expect("saved proxy reference remains typed after readback");
    assert_eq!(parsed.kind().as_str(), "proxy-host");
    assert_eq!(
        parsed.scope().as_str().strip_prefix("proxy-").map(str::len),
        Some(32)
    );
    assert!(matches!(error, AppError::Infrastructure { .. }));
    assert_eq!(
        vault.resolved_references.lock().await.as_slice(),
        &[saved_reference.as_str().to_owned()],
        "runtime resolves the precise reference persisted by the host management use case"
    );
}
