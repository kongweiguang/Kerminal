//! 主机与 vault 组合保存的 CAS、未知结果和并发回归测试。
//!
//! @author kongweiguang

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::{
        credentials::{
            CredentialService, CredentialVaultRepository, CredentialVaultSaveMetadata, SecretValue,
        },
        management::{
            CredentialCleanupStatus, HostAuthenticationMode, HostCredentialChange,
            HostCredentialSlot, HostDocumentRevision, HostDraft, HostGroup,
            HostGroupDeletionReceipt, HostGroupDraft, HostGroupSnapshot, HostManagementRepository,
            HostManagementService, HostRouteSettings, ManagedHost, RecoverableHostSaveOutcome,
        },
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostEndpoint, ProxyProfile, ProxyProtocol, RemoteProtocol,
        SshAuthentication,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
    preferences::ConfigRevision,
};
use tokio::sync::{Barrier, Mutex};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

#[derive(Clone, Copy)]
enum SaveDisposition {
    Commit,
    KnownFailure,
    UnknownFailure,
    UnknownWithoutCommit,
    InitiallyReadOnly,
    ReadOnlyAfterPreflight,
}

struct HostState {
    hosts: HashMap<String, ManagedHost>,
    groups: Vec<HostGroup>,
    group_revision: Option<ConfigRevision>,
    disposition: SaveDisposition,
    next_revision: u64,
    read_only: bool,
    bump_host_before_group_delete: bool,
}

#[derive(Clone)]
struct MemoryHosts {
    state: Arc<Mutex<HostState>>,
    save_barrier: Option<Arc<Barrier>>,
}

impl MemoryHosts {
    /// 使用可控 CAS 结果的内存仓储，避免这些用例依赖文件系统或真实配置目录。
    fn new(host: ManagedHost, disposition: SaveDisposition) -> Self {
        Self {
            state: Arc::new(Mutex::new(HostState {
                hosts: HashMap::from([(host.id.as_str().to_owned(), host)]),
                groups: Vec::new(),
                group_revision: None,
                disposition,
                next_revision: 2,
                read_only: matches!(disposition, SaveDisposition::InitiallyReadOnly),
                bump_host_before_group_delete: false,
            })),
            save_barrier: None,
        }
    }

    /// 以完整目录初始化隔离测试仓储；revision 构造器消费拥有所有权的摘要文本。
    fn with_directory(hosts: Vec<ManagedHost>, groups: Vec<HostGroup>) -> Self {
        let hosts = hosts
            .into_iter()
            .map(|host| (host.id.as_str().to_owned(), host))
            .collect();
        Self {
            state: Arc::new(Mutex::new(HostState {
                hosts,
                group_revision: (!groups.is_empty())
                    .then(|| ConfigRevision::from_sha256("groups-revision-1".to_owned())),
                groups,
                disposition: SaveDisposition::Commit,
                next_revision: 2,
                read_only: false,
                bump_host_before_group_delete: false,
            })),
            save_barrier: None,
        }
    }
}

#[async_trait]
impl HostManagementRepository for MemoryHosts {
    /// 模拟配置仓储共享的当前读写门禁，最终写入仍由 save_host 独立复核。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        if self.state.lock().await.read_only {
            return Err(AppError::configuration_corrupt("主机配置需要先恢复"));
        }
        Ok(())
    }

    /// 返回完整测试目录，供应用层清理前验证所有 host 引用。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.state.lock().await.hosts.values().cloned().collect())
    }

    /// 按稳定 ID 返回一份可编辑快照，不为不存在的条目合成默认值。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.state.lock().await.hosts.get(host_id.as_str()).cloned())
    }

    /// 在内存仓储中模拟 CAS、二次恢复态门禁及提交前/后的未知结果。
    ///
    /// @author kongweiguang
    async fn save_host(
        &self,
        host_id: HostId,
        draft: HostDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        if let Some(barrier) = &self.save_barrier {
            barrier.wait().await;
        }
        let mut state = self.state.lock().await;
        if matches!(state.disposition, SaveDisposition::ReadOnlyAfterPreflight) {
            state.read_only = true;
        }
        if state.read_only {
            return Err(AppError::configuration_corrupt("主机配置需要先恢复"));
        }
        if matches!(state.disposition, SaveDisposition::KnownFailure) {
            return Err(AppError::conflict("测试中模拟的确定 CAS 冲突"));
        }
        let existing = state.hosts.get(host_id.as_str());
        match (expected_revision.as_ref(), existing) {
            (Some(expected), Some(current)) if expected == &current.revision => {}
            (None, None) => {}
            _ => return Err(AppError::conflict("测试中模拟的过期 CAS")),
        }
        if matches!(state.disposition, SaveDisposition::UnknownWithoutCommit) {
            return Err(AppError::outcome_unknown("测试中模拟的未提交未知结果"));
        }
        let revision = ConfigRevision::from_sha256(format!("revision-{}", state.next_revision));
        state.next_revision += 1;
        let host = host_from_draft(draft, host_id, revision);
        state
            .hosts
            .insert(host.id.as_str().to_owned(), host.clone());
        if matches!(state.disposition, SaveDisposition::UnknownFailure) {
            return Err(AppError::outcome_unknown("测试中模拟的未知提交结果"));
        }
        Ok(host)
    }

    /// 以 revision 比对后删除内存主机；未知结果模拟提交已发生但回执丢失。
    async fn delete_host(
        &self,
        host_id: &HostId,
        expected_revision: &ConfigRevision,
    ) -> AppResult<ManagedHost> {
        let mut state = self.state.lock().await;
        if state.read_only {
            return Err(AppError::configuration_corrupt("主机配置需要先恢复"));
        }
        let existing = state
            .hosts
            .get(host_id.as_str())
            .ok_or_else(|| AppError::not_found("主机不存在"))?;
        if &existing.revision != expected_revision {
            return Err(AppError::conflict("主机已变化，请刷新后再删除"));
        }
        if matches!(state.disposition, SaveDisposition::KnownFailure) {
            return Err(AppError::conflict("模拟的确定删除失败"));
        }
        let deleted = state
            .hosts
            .remove(host_id.as_str())
            .ok_or_else(|| AppError::not_found("主机不存在"))?;
        if matches!(state.disposition, SaveDisposition::UnknownFailure) {
            return Err(AppError::outcome_unknown("模拟的未知删除结果"));
        }
        Ok(deleted)
    }

    /// 返回夹具中的整份分组快照与 revision。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        let state = self.state.lock().await;
        Ok(HostGroupSnapshot {
            groups: state.groups.clone(),
            revision: state.group_revision.clone(),
        })
    }

    /// 当前用例不创建分组，误调用会显式失败以防测试夹具掩盖错误行为。
    async fn save_group(
        &self,
        _draft: HostGroupDraft,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        Err(AppError::unsupported("当前测试用例不覆盖分组编辑"))
    }

    /// 在同一内存锁下核对全部 host revision 后迁移组成员并删除组。
    /// @author kongweiguang
    async fn delete_group_and_reassign(
        &self,
        group_id: String,
        destination_group_id: Option<String>,
        expected_group_revision: ConfigRevision,
        mut expected_hosts: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        let mut state = self.state.lock().await;
        if state.read_only {
            return Err(AppError::configuration_corrupt("主机配置需要先恢复"));
        }
        if state.group_revision.as_ref() != Some(&expected_group_revision) {
            return Err(AppError::conflict("主机分组已变化，请刷新后再删除"));
        }
        if !state.groups.iter().any(|group| group.id == group_id) {
            return Err(AppError::not_found("主机分组不存在"));
        }
        if destination_group_id
            .as_ref()
            .is_some_and(|destination| !state.groups.iter().any(|group| &group.id == destination))
        {
            return Err(AppError::not_found("目标分组不存在"));
        }
        expected_hosts.sort_by(|left, right| left.host_id.as_str().cmp(right.host_id.as_str()));
        if state.bump_host_before_group_delete
            && let Some(host) = state.hosts.values_mut().next()
        {
            host.revision = ConfigRevision::from_sha256("raced-host-revision".to_owned());
        }
        let mut actual_hosts = state
            .hosts
            .values()
            .map(|host| HostDocumentRevision {
                host_id: host.id.clone(),
                revision: host.revision.clone(),
            })
            .collect::<Vec<_>>();
        actual_hosts.sort_by(|left, right| left.host_id.as_str().cmp(right.host_id.as_str()));
        if expected_hosts != actual_hosts {
            return Err(AppError::conflict("主机目录已变化，请刷新后再删除分组"));
        }
        let mut reassigned_hosts = 0;
        for host in state.hosts.values_mut() {
            if host.group_id.as_deref() == Some(group_id.as_str()) {
                host.group_id = destination_group_id.clone();
                reassigned_hosts += 1;
            }
        }
        state.groups.retain(|group| group.id != group_id);
        let revision_number = state.next_revision;
        state.next_revision += 1;
        let revision = ConfigRevision::from_sha256(format!("groups-revision-{revision_number}"));
        state.group_revision = Some(revision.clone());
        Ok(HostGroupDeletionReceipt {
            groups: HostGroupSnapshot {
                groups: state.groups.clone(),
                revision: Some(revision),
            },
            reassigned_hosts,
        })
    }
}

#[derive(Default)]
struct MemoryVault(
    Mutex<HashMap<String, Zeroizing<String>>>,
    AtomicUsize,
    AtomicUsize,
    AtomicUsize,
);

#[async_trait]
impl CredentialVaultRepository for MemoryVault {
    /// 模拟密封 entry 持久化，并可在写入后注入一次 OutcomeUnknown；仅保存零化秘密。
    ///
    /// @author kongweiguang
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        self.1.fetch_add(1, Ordering::SeqCst);
        let reference_text = reference.reference().as_str().to_owned();
        let kind = reference.kind().as_str().to_owned();
        self.0.lock().await.insert(reference_text.clone(), secret);
        let unknown_countdown = self.3.load(Ordering::SeqCst);
        if unknown_countdown > 0 && self.3.fetch_sub(1, Ordering::SeqCst) == 1 {
            return Err(AppError::outcome_unknown("测试中模拟的 Vault 保存结果未知"));
        }
        Ok(CredentialVaultSaveMetadata {
            entry_id: reference_text,
            kind,
            key_id: "test-key".to_owned(),
            updated_at: "2026-10-16T00:00:00Z".to_owned(),
        })
    }

    /// 检查 vault 中 opaque 引用是否存在，不返回或复制明文秘密。
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool> {
        Ok(self.0.lock().await.contains_key(reference.as_str()))
    }

    /// 删除指定 vault entry；不存在时返回 false 并保持幂等，可注入一次删除失败验证恢复门禁。
    ///
    /// @author kongweiguang
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        if self.2.load(Ordering::SeqCst) > 0 {
            self.2.fetch_sub(1, Ordering::SeqCst);
            return Err(AppError::infrastructure("测试中模拟的凭据删除失败"));
        }
        Ok(self.0.lock().await.remove(reference.as_str()).is_some())
    }
}

/// 以旧版 host schema 的稳定 target ref 建立密码主机快照。
///
/// @author kongweiguang
fn password_host() -> ManagedHost {
    let id = HostId::new();
    ManagedHost {
        id: id.clone(),
        group_id: None,
        name: "test host".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "127.0.0.1".to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: Some(SshAuthentication::Password {
            secret: old_password_reference(&id),
        }),
        authentication_mode: HostAuthenticationMode::Password,
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
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256("revision-1".to_owned()),
    }
}

/// 建立显式 revision 的分组夹具，供删除迁移用例检查成员归属。
fn host_group(id: &str, name: &str, sort_order: i64) -> HostGroup {
    HostGroup {
        id: id.to_owned(),
        name: name.to_owned(),
        sort_order,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

/// 生成既存 schema-v2 target password ref，确认更换时不会覆盖固定旧 entry。
fn old_password_reference(host_id: &HostId) -> CredentialRef {
    let reference = CredentialReference::new(
        CredentialKind::parse("ssh-host").expect("valid kind"),
        host_id.clone(),
        CredentialScope::target(),
        CredentialMaterial::Password,
    )
    .expect("valid reference");
    reference.reference().clone()
}

/// 生成代理密码的既存 typed ref，验证代理密文与目标 SSH 密码不会共用 kind/scope。
fn old_proxy_password_reference(host_id: &HostId) -> CredentialRef {
    CredentialReference::new(
        CredentialKind::parse("proxy-host").expect("valid proxy kind"),
        host_id.clone(),
        CredentialScope::parse("proxy-legacy").expect("valid proxy scope"),
        CredentialMaterial::Password,
    )
    .expect("valid proxy reference")
    .reference()
    .clone()
}

/// 构造带已保存代理密码的 SSH 主机，目标认证引用仍独立保留。
fn proxy_password_host() -> (ManagedHost, CredentialRef) {
    let mut host = password_host();
    let proxy_reference = old_proxy_password_reference(&host.id);
    host.route.proxy = Some(ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: "127.0.0.2".to_owned(),
            port: 1080,
            username: "proxy-user".to_owned(),
        },
        password: Some(proxy_reference.clone()),
    });
    (host, proxy_reference)
}

/// 把应用草稿变成仓储返回快照，时间字段保持稳定以便测试只观察业务字段。
///
/// @author kongweiguang
fn host_from_draft(draft: HostDraft, id: HostId, revision: ConfigRevision) -> ManagedHost {
    ManagedHost {
        id,
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
        created_at: "1".to_owned(),
        updated_at: "2".to_owned(),
        revision,
    }
}

/// 从测试基线构造可保存草稿，继续携带旧凭据直到 CAS 成功提交替换。
///
/// @author kongweiguang
fn draft_for(host: &ManagedHost, name: &str) -> HostDraft {
    HostDraft {
        id: Some(host.id.clone()),
        group_id: host.group_id.clone(),
        name: name.to_owned(),
        note: host.note.clone(),
        protocol: host.protocol,
        endpoint: host.endpoint.clone(),
        authentication: host.authentication.clone(),
        authentication_mode: host.authentication_mode,
        rdp_password: host.rdp_password.clone(),
        route: host.route.clone(),
        serial: host.serial.clone(),
        tags: host.tags.clone(),
        is_favorite: host.is_favorite,
        sort_order: host.sort_order,
        connect_timeout_ms: host.connect_timeout_ms,
        keepalive_interval_ms: host.keepalive_interval_ms,
        terminal_options: host.terminal_options.clone(),
        sftp_options: host.sftp_options.clone(),
        rdp_display: host.rdp_display.clone(),
    }
}

/// 建立保存服务与 vault fake；预置旧 entry 后清零计数，只统计本次用例副作用。
async fn fixture(
    disposition: SaveDisposition,
    barrier: Option<Arc<Barrier>>,
) -> (HostManagementService, ManagedHost, Arc<MemoryVault>) {
    let host = password_host();
    let mut repository = MemoryHosts::new(host.clone(), disposition);
    repository.save_barrier = barrier;
    let vault = Arc::new(MemoryVault::default());
    let old_reference =
        CredentialReference::parse(old_password_reference(&host.id)).expect("valid ref");
    vault
        .save(old_reference, Zeroizing::new("old secret".to_owned()))
        .await
        .expect("seed legacy credential");
    vault.1.store(0, Ordering::SeqCst);
    let credentials = CredentialService::new(vault.clone());
    (
        HostManagementService::with_credentials(Arc::new(repository), credentials),
        host,
        vault,
    )
}

/// 建立同时包含目标密码和代理密码的隔离 vault，确保清理只影响被替换的引用。
async fn proxy_fixture(
    disposition: SaveDisposition,
) -> (
    HostManagementService,
    ManagedHost,
    CredentialRef,
    Arc<MemoryVault>,
) {
    let (host, proxy_reference) = proxy_password_host();
    let repository = MemoryHosts::new(host.clone(), disposition);
    let vault = Arc::new(MemoryVault::default());
    for reference in [old_password_reference(&host.id), proxy_reference.clone()] {
        vault
            .save(
                CredentialReference::parse(reference).expect("valid seeded reference"),
                Zeroizing::new("old secret".to_owned()),
            )
            .await
            .expect("seed existing credential");
    }
    vault.1.store(0, Ordering::SeqCst);
    let credentials = CredentialService::new(vault.clone());
    (
        HostManagementService::with_credentials(Arc::new(repository), credentials),
        host,
        proxy_reference,
        vault,
    )
}

/// 确认成功替换使用新 scope，并在 host CAS 后才删除旧 ref。
#[tokio::test]
async fn successful_update_publishes_unique_reference_then_cleans_old_entry() {
    let (service, host, vault) = fixture(SaveDisposition::Commit, None).await;
    let old = old_password_reference(&host.id);
    let receipt = service
        .save_host_with_credentials(
            draft_for(&host, "updated host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("new secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("host CAS succeeds");
    let new = receipt.credentials[0].reference.clone();
    let parsed = CredentialReference::parse(new.clone()).expect("new ref parses");

    assert_ne!(new, old);
    assert!(parsed.scope().as_str().starts_with("target-"));
    assert_eq!(receipt.cleanup, CredentialCleanupStatus::Complete);
    assert!(!vault.contains(&old).await.expect("check old ref"));
    assert!(vault.contains(&new).await.expect("check new ref"));
}

/// 只读恢复态在密封任何新凭据前拒绝保存，并保留原始主机与旧 vault entry。
#[tokio::test]
async fn read_only_recovery_blocks_before_vault_write() {
    let (service, host, vault) = fixture(SaveDisposition::InitiallyReadOnly, None).await;
    let old = old_password_reference(&host.id);
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "blocked host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("must not be saved".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::ConfigurationCorrupt { .. })));
    assert_eq!(vault.1.load(Ordering::SeqCst), 0);
    assert_eq!(vault.0.lock().await.len(), 1);
    assert!(vault.contains(&old).await.expect("old entry remains"));
}

/// 即使恢复态在 preflight 后改变，CAS 写门仍阻断主机文件并清理本次唯一新引用。
#[tokio::test]
async fn recovery_mode_change_after_preflight_cleans_only_new_credential() {
    let (service, host, vault) = fixture(SaveDisposition::ReadOnlyAfterPreflight, None).await;
    let old = old_password_reference(&host.id);
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "raced read-only host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("race secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::ConfigurationCorrupt { .. })));
    assert_eq!(vault.1.load(Ordering::SeqCst), 1);
    assert_eq!(vault.0.lock().await.len(), 1);
    assert!(vault.contains(&old).await.expect("old entry remains"));
}

/// 确认确定 CAS 失败只回收本次独有且未发布的新 entry，不动原 host 凭据。
#[tokio::test]
async fn known_cas_failure_cleans_only_the_unpublished_replacement() {
    let (service, host, vault) = fixture(SaveDisposition::KnownFailure, None).await;
    let old = old_password_reference(&host.id);
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "uncommitted host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("uncommitted secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert!(vault.contains(&old).await.expect("old entry remains"));
    assert_eq!(vault.0.lock().await.len(), 1);
}

/// 未知提交结果必须保留新旧 entry，禁止应用层凭一次失败猜测是否回滚。
#[tokio::test]
async fn unknown_commit_retains_both_old_and_new_entries() {
    let (service, host, vault) = fixture(SaveDisposition::UnknownFailure, None).await;
    let old = old_password_reference(&host.id);
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "uncertain host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("uncertain secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::OutcomeUnknown { .. })));
    assert!(vault.contains(&old).await.expect("old entry remains"));
    assert_eq!(vault.0.lock().await.len(), 2);
    let reloaded = service
        .load_host(&host.id)
        .await
        .expect("host can be reread after unknown commit")
        .expect("host still exists");
    let new = match reloaded.authentication {
        Some(SshAuthentication::Password { secret }) => secret,
        _ => panic!("unknown update rereads the committed password reference"),
    };
    assert_ne!(new, old);
    assert!(
        vault
            .contains(&new)
            .await
            .expect("reloaded new entry remains")
    );
}

/// 未知但实际已提交时，显式读回恢复会清理失去引用的旧 entry 并保留新 host 正在引用的 entry。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_after_unknown_commit_cleans_old_and_preserves_published_new_reference() {
    let (service, host, vault) = fixture(SaveDisposition::UnknownFailure, None).await;
    let old = old_password_reference(&host.id);
    let recovery = match service
        .save_host_with_credentials_recoverable(
            draft_for(&host, "unknown committed host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("replacement secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("recoverable save preserves the unknown result")
    {
        RecoverableHostSaveOutcome::OutcomeUnknown { error, recovery } => {
            assert_eq!(
                error.public_code(),
                kerminal_application::PublicErrorCode::OutcomeUnknown
            );
            recovery
        }
        RecoverableHostSaveOutcome::Saved(_) => panic!("fixture must return an unknown commit"),
    };

    assert_eq!(recovery.host_id(), &host.id);
    assert_eq!(recovery.baseline_revision(), Some(&host.revision));
    let reloaded = service
        .load_host(recovery.host_id())
        .await
        .expect("host readback succeeds")
        .expect("committed host exists");
    let new = match reloaded.authentication {
        Some(SshAuthentication::Password { secret }) => secret,
        _ => panic!("unknown commit publishes the replacement reference"),
    };
    assert!(!format!("{recovery:?}").contains(new.as_str()));

    let cleanup = service
        .recover_unknown_host_save(&recovery)
        .await
        .expect("explicit readback permits candidate cleanup");
    assert_eq!(cleanup, CredentialCleanupStatus::Complete);
    assert!(!vault.contains(&old).await.expect("check old reference"));
    assert!(
        vault
            .contains(&new)
            .await
            .expect("published ref is retained")
    );
    assert_eq!(vault.0.lock().await.len(), 1);
}

/// Vault 第二次保存提交后回执未知时，恢复令牌包含已返回与推定候选两项新 ref，并保留旧 host 引用。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_after_unknown_vault_save_removes_only_new_unreferenced_candidates() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::Commit).await;
    let old_target = old_password_reference(&host.id);
    vault.3.store(2, Ordering::SeqCst);

    let recovery = match service
        .save_host_with_credentials_recoverable(
            draft_for(&host, "unknown vault save"),
            Some(host.revision.clone()),
            vec![
                HostCredentialChange::Replace {
                    slot: HostCredentialSlot::TargetPassword,
                    secret: SecretValue::new("target replacement".to_owned()),
                },
                HostCredentialChange::Replace {
                    slot: HostCredentialSlot::ProxyPassword,
                    secret: SecretValue::new("proxy replacement".to_owned()),
                },
            ],
            CancellationToken::new(),
        )
        .await
        .expect("Vault unknown result is represented by a recovery token")
    {
        RecoverableHostSaveOutcome::OutcomeUnknown { error, recovery } => {
            assert_eq!(
                error.public_code(),
                kerminal_application::PublicErrorCode::OutcomeUnknown
            );
            recovery
        }
        RecoverableHostSaveOutcome::Saved(_) => panic!("fixture injects an unknown Vault result"),
    };

    let reloaded = service
        .load_host(recovery.host_id())
        .await
        .expect("host readback succeeds")
        .expect("baseline host remains because host CAS was not reached");
    assert_eq!(reloaded.revision, host.revision);
    let new_references = vault
        .0
        .lock()
        .await
        .keys()
        .filter(|reference| {
            reference.as_str() != old_target.as_str() && reference.as_str() != old_proxy.as_str()
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(new_references.len(), 2);
    let debug = format!("{recovery:?}");
    assert!(
        new_references
            .iter()
            .all(|reference| !debug.contains(reference))
    );

    let cleanup = service
        .recover_unknown_host_save(&recovery)
        .await
        .expect("explicit readback permits idempotent Vault cleanup");
    assert_eq!(cleanup, CredentialCleanupStatus::Complete);
    assert!(
        vault
            .contains(&old_target)
            .await
            .expect("old target remains referenced")
    );
    assert!(
        vault
            .contains(&old_proxy)
            .await
            .expect("old proxy remains referenced")
    );
    for reference in new_references {
        let reference = CredentialRef::new(reference).expect("candidate ref is valid");
        assert!(
            !vault
                .contains(&reference)
                .await
                .expect("check removed candidate")
        );
    }
    assert_eq!(vault.0.lock().await.len(), 2);
}

/// 未知但未提交时仅回收新引用；一次删除失败返回 Deferred，保留令牌后重试可完成且不动旧引用。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_after_unknown_uncommitted_save_is_deferred_and_retryable() {
    let (service, host, vault) = fixture(SaveDisposition::UnknownWithoutCommit, None).await;
    let old = old_password_reference(&host.id);
    let recovery = match service
        .save_host_with_credentials_recoverable(
            draft_for(&host, "unknown uncommitted host"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("unpublished secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("recoverable save preserves the unknown result")
    {
        RecoverableHostSaveOutcome::OutcomeUnknown { error, recovery } => {
            assert_eq!(
                error.public_code(),
                kerminal_application::PublicErrorCode::OutcomeUnknown
            );
            recovery
        }
        RecoverableHostSaveOutcome::Saved(_) => panic!("fixture must return an unknown result"),
    };

    let reloaded = service
        .load_host(recovery.host_id())
        .await
        .expect("host readback succeeds")
        .expect("baseline host remains");
    assert_eq!(reloaded.revision, host.revision);
    let new = vault
        .0
        .lock()
        .await
        .keys()
        .find(|reference| reference.as_str() != old.as_str())
        .cloned()
        .expect("new unique reference was retained until readback");
    let new = CredentialRef::new(new).expect("new reference is valid");
    vault.2.store(1, Ordering::SeqCst);

    let deferred = service
        .recover_unknown_host_save(&recovery)
        .await
        .expect("cleanup failure is reported as retryable state");
    assert_eq!(deferred, CredentialCleanupStatus::Deferred { remaining: 1 });
    assert!(
        vault
            .contains(&old)
            .await
            .expect("baseline reference is retained")
    );
    assert!(vault.contains(&new).await.expect("deferred entry remains"));
    assert_eq!(vault.0.lock().await.len(), 2);

    let retried = service
        .recover_unknown_host_save(&recovery)
        .await
        .expect("same token can safely retry cleanup after explicit readback");
    assert_eq!(retried, CredentialCleanupStatus::Complete);
    assert!(
        vault
            .contains(&old)
            .await
            .expect("baseline reference is still used")
    );
    assert!(
        !vault
            .contains(&new)
            .await
            .expect("check removed unpublished entry")
    );
    assert_eq!(vault.0.lock().await.len(), 1);
}

/// 两个旧 revision 并发写入时只允许一个 CAS 获胜，失败者只删除自己唯一的新 entry。
#[tokio::test]
async fn concurrent_updates_never_remove_the_winning_credential() {
    let barrier = Arc::new(Barrier::new(2));
    let (service, host, vault) = fixture(SaveDisposition::Commit, Some(barrier)).await;
    let request = |name: &'static str, secret: &'static str| {
        service.save_host_with_credentials(
            draft_for(&host, name),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new(secret.to_owned()),
            }],
            CancellationToken::new(),
        )
    };
    let (left, right) = tokio::join!(
        request("left update", "left secret"),
        request("right update", "right secret"),
    );
    let winner = match (left, right) {
        (Ok(receipt), Err(AppError::Conflict { .. }))
        | (Err(AppError::Conflict { .. }), Ok(receipt)) => receipt,
        _ => panic!("预期并发保存产生一个 CAS 成功和一个冲突"),
    };
    let winner_ref = winner.credentials[0].reference.clone();

    assert!(
        vault
            .contains(&winner_ref)
            .await
            .expect("winning entry remains")
    );
    assert_eq!(vault.0.lock().await.len(), 1);
}

/// 空白代理密码不生成凭据变更，保存其它字段时仍保留原代理 ref 与密文。
#[tokio::test]
async fn empty_proxy_password_keeps_existing_reference() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::Commit).await;
    let receipt = service
        .save_host_with_credentials(
            draft_for(&host, "proxy unchanged"),
            Some(host.revision.clone()),
            Vec::new(),
            CancellationToken::new(),
        )
        .await
        .expect("empty password leaves existing ref untouched");

    assert_eq!(
        receipt.host.route.proxy.unwrap().password,
        Some(old_proxy.clone())
    );
    assert!(vault.contains(&old_proxy).await.expect("old proxy remains"));
    assert_eq!(vault.0.lock().await.len(), 2);
}

/// 成功替换代理密码使用 host-bound 的唯一 proxy scope，并在回读提交后删除旧 entry。
#[tokio::test]
async fn successful_proxy_password_replacement_publishes_before_cleanup() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::Commit).await;
    let receipt = service
        .save_host_with_credentials(
            draft_for(&host, "proxy replacement"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::ProxyPassword,
                secret: SecretValue::new("new proxy secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("proxy ref is saved by host CAS");
    let new_proxy = receipt
        .host
        .route
        .proxy
        .expect("proxy route persists")
        .password
        .expect("new ref is published");
    let parsed = CredentialReference::parse(new_proxy.clone()).expect("new ref parses");

    assert_eq!(parsed.kind().as_str(), "proxy-host");
    assert_eq!(parsed.host_id(), &host.id);
    let unique_suffix = parsed
        .scope()
        .as_str()
        .strip_prefix("proxy-")
        .expect("proxy transaction scope has its typed prefix");
    assert_eq!(unique_suffix.len(), 32);
    assert!(
        unique_suffix
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    assert_eq!(parsed.material(), CredentialMaterial::Password);
    assert!(!vault.contains(&old_proxy).await.expect("old ref removed"));
    assert!(vault.contains(&new_proxy).await.expect("new ref exists"));
    assert!(
        vault
            .contains(&old_password_reference(&host.id))
            .await
            .expect("target secret remains")
    );
    assert_eq!(receipt.cleanup, CredentialCleanupStatus::Complete);
    assert_eq!(vault.0.lock().await.len(), 2);
}

/// 明确清除代理密码只删除 CAS 后已不再被配置引用的旧 vault entry。
#[tokio::test]
async fn clearing_proxy_password_removes_only_the_old_proxy_entry() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::Commit).await;
    let mut draft = draft_for(&host, "proxy cleared");
    draft
        .route
        .proxy
        .as_mut()
        .expect("proxy exists")
        .endpoint
        .username
        .clear();
    let receipt = service
        .save_host_with_credentials(
            draft,
            Some(host.revision.clone()),
            vec![HostCredentialChange::Clear {
                slot: HostCredentialSlot::ProxyPassword,
            }],
            CancellationToken::new(),
        )
        .await
        .expect("clear commits before obsolete proxy secret is removed");

    assert_eq!(receipt.host.route.proxy.unwrap().password, None);
    assert_eq!(receipt.cleanup, CredentialCleanupStatus::Complete);
    assert!(!vault.contains(&old_proxy).await.expect("old proxy removed"));
    assert!(
        vault
            .contains(&old_password_reference(&host.id))
            .await
            .expect("target credential remains")
    );
    assert_eq!(vault.0.lock().await.len(), 1);
}

/// 新代理密码在用户名缺失时于 vault 副作用前失败，原有代理 entry 保持完整。
#[tokio::test]
async fn proxy_password_requires_username_before_vault_write() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::Commit).await;
    let mut draft = draft_for(&host, "proxy username missing");
    draft
        .route
        .proxy
        .as_mut()
        .expect("proxy exists")
        .endpoint
        .username
        .clear();
    let result = service
        .save_host_with_credentials(
            draft,
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::ProxyPassword,
                secret: SecretValue::new("must not be saved".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::Invalid { .. })));
    assert_eq!(vault.1.load(Ordering::SeqCst), 0);
    assert!(vault.contains(&old_proxy).await.expect("old proxy remains"));
    assert_eq!(vault.0.lock().await.len(), 2);
}

/// 确定 CAS 失败时只回收此次新建代理密文，旧代理和目标凭据都保持可用。
#[tokio::test]
async fn known_proxy_cas_failure_keeps_old_secret_and_removes_new_secret() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::KnownFailure).await;
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "proxy CAS failure"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::ProxyPassword,
                secret: SecretValue::new("replacement proxy secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert!(vault.contains(&old_proxy).await.expect("old proxy remains"));
    assert!(
        vault
            .contains(&old_password_reference(&host.id))
            .await
            .expect("target credential remains")
    );
    assert_eq!(vault.0.lock().await.len(), 2);
}

/// 未知 CAS 结果必须保留新旧代理 ref，回读配置可确认此次提交实际发布了哪一个。
#[tokio::test]
async fn unknown_proxy_commit_retains_both_secrets_and_published_reference() {
    let (service, host, old_proxy, vault) = proxy_fixture(SaveDisposition::UnknownFailure).await;
    let result = service
        .save_host_with_credentials(
            draft_for(&host, "proxy uncertain commit"),
            Some(host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::ProxyPassword,
                secret: SecretValue::new("uncertain proxy secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await;

    assert!(matches!(result, Err(AppError::OutcomeUnknown { .. })));
    assert!(
        vault
            .contains(&old_proxy)
            .await
            .expect("old proxy retained")
    );
    let reloaded = service
        .load_host(&host.id)
        .await
        .expect("host can be read after uncertain commit")
        .expect("host exists");
    let new_proxy = reloaded
        .route
        .proxy
        .expect("proxy route remains")
        .password
        .expect("new proxy ref is published");
    let parsed = CredentialReference::parse(new_proxy.clone()).expect("new ref parses");
    assert_eq!(parsed.kind().as_str(), "proxy-host");
    assert!(parsed.scope().as_str().starts_with("proxy-"));
    assert!(
        vault
            .contains(&new_proxy)
            .await
            .expect("new proxy retained")
    );
    assert_eq!(vault.0.lock().await.len(), 3);
}

/// 副本只继承非秘密连接设置，密钥路径可复用但 target、代理和跳板的 vault refs 必须清空。
///
/// @author kongweiguang
#[tokio::test]
async fn copy_draft_preserves_connection_settings_without_credential_references() {
    use std::path::PathBuf;

    let mut host = password_host();
    host.note = Some("生产构建节点\n维护前联系发布团队。".to_owned());
    let target_key = CredentialReference::new(
        CredentialKind::parse("ssh-host").expect("valid kind"),
        host.id.clone(),
        CredentialScope::target(),
        CredentialMaterial::PrivateKey,
    )
    .expect("valid key reference")
    .reference()
    .clone();
    let target_passphrase = CredentialReference::new(
        CredentialKind::parse("ssh-host").expect("valid kind"),
        host.id.clone(),
        CredentialScope::target(),
        CredentialMaterial::KeyPassphrase,
    )
    .expect("valid passphrase reference")
    .reference()
    .clone();
    let jump_password = CredentialReference::new(
        CredentialKind::parse("jump-host").expect("valid kind"),
        host.id.clone(),
        CredentialScope::jump(0),
        CredentialMaterial::Password,
    )
    .expect("valid jump reference")
    .reference()
    .clone();
    host.group_id = Some("development".to_owned());
    host.authentication_mode = HostAuthenticationMode::PrivateKey;
    host.authentication = Some(SshAuthentication::PrivateKey {
        path: Some(PathBuf::from("C:/keys/id_ed25519")),
        secret: Some(target_key),
        passphrase: Some(target_passphrase),
    });
    host.route.proxy = Some(ProxyProfile {
        protocol: ProxyProtocol::Socks5,
        endpoint: HostEndpoint {
            host: "127.0.0.2".to_owned(),
            port: 1080,
            username: "proxy".to_owned(),
        },
        password: Some(old_proxy_password_reference(&host.id)),
    });
    host.route.jump_hosts.push(
        kerminal_application::connections::management::ManagedJumpHost {
            name: "bastion".to_owned(),
            endpoint: HostEndpoint {
                host: "bastion.example.test".to_owned(),
                port: 22,
                username: "tester".to_owned(),
            },
            authentication: Some(SshAuthentication::Password {
                secret: jump_password,
            }),
            authentication_mode: HostAuthenticationMode::Password,
        },
    );
    let repository = MemoryHosts::new(host.clone(), SaveDisposition::Commit);
    let service = HostManagementService::new(Arc::new(repository.clone()));

    let draft = service
        .draft_for_copy(&host.id, "copy".to_owned())
        .await
        .expect("source host is available");

    assert!(draft.id.is_none());
    assert_eq!(draft.group_id.as_deref(), Some("development"));
    assert_eq!(draft.name, "copy");
    assert_eq!(
        draft.note.as_deref(),
        Some("生产构建节点\n维护前联系发布团队。")
    );
    assert_eq!(
        draft.authentication_mode,
        HostAuthenticationMode::PrivateKey
    );
    assert_eq!(
        draft.authentication,
        Some(SshAuthentication::PrivateKey {
            path: Some(PathBuf::from("C:/keys/id_ed25519")),
            secret: None,
            passphrase: None,
        })
    );
    assert_eq!(
        draft
            .route
            .proxy
            .as_ref()
            .and_then(|proxy| proxy.password.as_ref()),
        None
    );
    assert!(draft.route.jump_hosts[0].authentication.is_none());
    assert!(
        repository
            .load_host(&host.id)
            .await
            .expect("source remains readable")
            .is_some_and(|source| source == host)
    );
}

/// 保存主机备注时保留有效换行、折叠纯空白，并在提交前拒绝超长或异常控制字符。
///
/// @author kongweiguang
#[tokio::test]
async fn host_notes_preserve_multiline_text_and_normalize_empty_values() {
    let original = password_host();
    let repository = MemoryHosts::new(original.clone(), SaveDisposition::Commit);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let mut draft = draft_for(&original, "updated host");
    let expected_note = "第一行\n第二行\t负责人：运维";
    draft.note = Some(expected_note.to_owned());

    let saved = service
        .save_host(draft, Some(original.revision.clone()))
        .await
        .expect("保存多行备注");
    assert_eq!(saved.note.as_deref(), Some(expected_note));

    let mut blank = draft_for(&saved, "updated host");
    blank.note = Some(" \n\t ".to_owned());
    let saved_without_note = service
        .save_host(blank, Some(saved.revision.clone()))
        .await
        .expect("空白备注归一化后可保存");
    assert_eq!(saved_without_note.note, None);

    for invalid_note in [
        Some("中".repeat(kerminal_application::connections::management::HOST_NOTE_MAX_CHARS + 1)),
        Some("不允许的\0控制字符".to_owned()),
    ] {
        let mut invalid = draft_for(&saved_without_note, "updated host");
        invalid.note = invalid_note;
        assert!(
            service
                .save_host(invalid, Some(saved_without_note.revision.clone()))
                .await
                .is_err()
        );
    }
    assert_eq!(
        repository
            .load_host(&original.id)
            .await
            .expect("确认拒绝无效备注后仍可读")
            .expect("原主机仍存在")
            .note,
        None
    );
}

/// 未知删除回执不能猜测文件状态并清理旧凭据；确认删除后只清理已无主机引用的 entry。
#[tokio::test]
async fn delete_host_cleans_credentials_only_after_confirmed_commit() {
    let (service, host, vault) = fixture(SaveDisposition::Commit, None).await;
    let old = old_password_reference(&host.id);
    let receipt = service
        .delete_host(&host.id, &host.revision)
        .await
        .expect("host deletion is confirmed");
    assert_eq!(
        receipt.credential_cleanup,
        CredentialCleanupStatus::Complete
    );
    assert!(!vault.contains(&old).await.expect("old reference removed"));
    assert!(
        service
            .load_host(&host.id)
            .await
            .expect("deleted host lookup succeeds")
            .is_none()
    );

    let (uncertain, host, vault) = fixture(SaveDisposition::UnknownFailure, None).await;
    let old = old_password_reference(&host.id);
    let result = uncertain.delete_host(&host.id, &host.revision).await;
    assert!(matches!(result, Err(AppError::OutcomeUnknown { .. })));
    assert!(
        vault
            .contains(&old)
            .await
            .expect("unknown result keeps vault ref")
    );
}

/// 删除分组可明确迁移到有效目标或未分组，返回事务快照并保留其它组与主机。
#[tokio::test]
async fn delete_group_reassigns_hosts_or_ungroups_atomically() {
    let first = host_group("dev", "Development", 0);
    let second = host_group("ops", "Operations", 1);
    let mut dev_host = password_host();
    dev_host.group_id = Some(first.id.clone());
    let mut second_host = password_host();
    second_host.group_id = Some(first.id.clone());
    let repository = MemoryHosts::with_directory(
        vec![dev_host.clone(), second_host.clone()],
        vec![first.clone(), second.clone()],
    );
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let groups = service.load_groups().await.expect("groups load");

    let receipt = service
        .delete_group(
            first.id.clone(),
            Some(second.id.clone()),
            groups.revision.clone().expect("group revision"),
        )
        .await
        .expect("group and memberships commit together");

    assert_eq!(receipt.reassigned_hosts, 2);
    assert_eq!(receipt.groups.groups, vec![second.clone()]);
    assert!(
        repository
            .list_hosts()
            .await
            .expect("host list after transaction")
            .iter()
            .all(|host| host.group_id.as_deref() == Some("ops"))
    );

    let mut ungrouped_host = password_host();
    ungrouped_host.group_id = Some(second.id.clone());
    let repository = MemoryHosts::with_directory(vec![ungrouped_host], vec![second.clone()]);
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let groups = service.load_groups().await.expect("groups load");
    let receipt = service
        .delete_group(
            second.id.clone(),
            None,
            groups.revision.clone().expect("group revision"),
        )
        .await
        .expect("ungrouped is explicit valid destination");
    assert_eq!(receipt.reassigned_hosts, 1);
    assert!(receipt.groups.groups.is_empty());
    assert!(
        repository
            .list_hosts()
            .await
            .expect("host list after ungroup")
            .iter()
            .all(|host| host.group_id.is_none())
    );
}

/// 分组目标无效、删除自身或 host 快照并发变化时整项拒绝且不迁移任何主机。
/// 调用会消费分组 ID，因此保留夹具 ID 的副本供后续状态断言使用。
#[tokio::test]
async fn delete_group_rejects_invalid_target_and_host_revision_race() {
    let source = host_group("dev", "Development", 0);
    let destination = host_group("ops", "Operations", 1);
    let mut host = password_host();
    host.group_id = Some(source.id.clone());
    let repository = MemoryHosts::with_directory(
        vec![host.clone()],
        vec![source.clone(), destination.clone()],
    );
    let service = HostManagementService::new(Arc::new(repository.clone()));
    let revision = service
        .load_groups()
        .await
        .expect("group read")
        .revision
        .expect("group revision");
    assert!(matches!(
        service
            .delete_group(source.id.clone(), Some(source.id.clone()), revision.clone())
            .await,
        Err(AppError::Invalid { .. })
    ));
    assert!(matches!(
        service
            .delete_group(
                source.id.clone(),
                Some("missing".to_owned()),
                revision.clone()
            )
            .await,
        Err(AppError::NotFound { .. })
    ));

    repository.state.lock().await.bump_host_before_group_delete = true;
    let result = service
        .delete_group(source.id.clone(), Some(destination.id.clone()), revision)
        .await;
    assert!(matches!(result, Err(AppError::Conflict { .. })));
    assert_eq!(
        repository
            .load_host(&host.id)
            .await
            .expect("host remains")
            .expect("host is not deleted")
            .group_id,
        Some(source.id.clone())
    );
    assert_eq!(
        service
            .load_groups()
            .await
            .expect("groups remain readable")
            .groups,
        vec![source, destination]
    );
}
