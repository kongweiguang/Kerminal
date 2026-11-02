//! 本地主机在共享配置、分组和 vault 中的完整契约回归。
//!
//! @author kongweiguang

use std::{collections::BTreeMap, fs, process::Command, sync::Arc, time::Duration};

use async_trait::async_trait;
use kerminal_application::connections::{
    CredentialResolver,
    credentials::{
        CredentialService, CredentialVaultRepository, CredentialVaultSaveMetadata,
        SaveCredentialRequest, SecretValue,
    },
    management::{
        CredentialCleanupStatus, HostGroupDraft, HostManagementRepository, HostManagementService,
        LocalHostDraft, LocalHostSaveRecoveryToken, ManagedLocalHost,
        RecoverableLocalHostSaveOutcome,
    },
};
use kerminal_domain::{
    connections::{
        CredentialRef,
        credentials::{CredentialKind, CredentialMaterial, CredentialReference, CredentialScope},
    },
    identity::HostId,
};
use kerminal_infrastructure::{
    configuration::ConfigurationStore, connections::management::HostConfigurationRepository,
    vault::VaultStore,
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

/// 每个测试使用单独数据根并复用同一 coordinator，避免触碰用户配置或绕开写锁。
///
/// @author kongweiguang
fn fixture() -> (TempDir, HostManagementService, HostManagementService) {
    let root = tempfile::tempdir().expect("创建隔离数据根");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置仓储");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let credentials = CredentialService::new(vault.clone() as Arc<dyn CredentialVaultRepository>);
    let repository: Arc<dyn HostManagementRepository> =
        Arc::new(HostConfigurationRepository::new(configuration));
    let without_resolver = HostManagementService::new(repository.clone());
    let service = HostManagementService::with_credentials(repository, credentials)
        .with_local_environment_resolver(vault as Arc<dyn CredentialResolver>);
    (root, service, without_resolver)
}

/// 构造稳定 ID 和可见启动字段，防止测试依赖系统 Shell 或运行时目录。
///
/// @author kongweiguang
fn draft(id: &str, group_id: Option<&str>) -> LocalHostDraft {
    LocalHostDraft {
        id: Some(HostId::parse(id).expect("合法主机 ID")),
        group_id: group_id.map(str::to_owned),
        name: "Local test".to_owned(),
        shell: "shell-test".to_owned(),
        args: vec!["--login".to_owned()],
        cwd: Some("test-directory".to_owned()),
        env: BTreeMap::new(),
        sort_order: 10,
    }
}

/// 提取已确认保存结果；未知结果必须显式失败，不能被测试伪装成成功。
///
/// @author kongweiguang
fn saved(outcome: RecoverableLocalHostSaveOutcome) -> ManagedLocalHost {
    match outcome {
        RecoverableLocalHostSaveOutcome::Saved {
            host,
            credential_cleanup: CredentialCleanupStatus::Complete,
        } => host,
        _ => panic!("本地主机未确认保存及完成清理"),
    }
}

/// 环境仅以独立 vault 引用落盘；重启式重新读取、启动解析和并发冲突均保留原值。
///
/// @author kongweiguang
#[tokio::test]
async fn local_host_environment_is_encrypted_and_saved_with_revision() {
    let (root, service, without_resolver) = fixture();
    let mut input = draft("local-encrypted", None);
    input
        .env
        .insert("PASSWORD".into(), "secret-local-value".into());
    input.env.insert("PROJECT".into(), "demo-project".into());
    let original_env = input.env.clone();
    let host = saved(
        service
            .save_local_host_recoverable(input.clone(), None, CancellationToken::new())
            .await
            .expect("保存本地主机"),
    );
    assert!(host.environment_ref.is_some());
    let raw =
        fs::read_to_string(root.path().join("hosts/local-encrypted.toml")).expect("读取主机文件");
    assert!(!raw.contains("secret-local-value"));
    assert!(!raw.contains("PASSWORD"));
    assert!(!raw.contains("demo-project"));
    assert!(raw.contains("environment_ref"));
    let vault = fs::read_to_string(root.path().join("secrets/vault.toml")).expect("读取 vault");
    assert!(!vault.contains("secret-local-value"));
    assert_eq!(
        service
            .read_local_environment(&host, &CancellationToken::new())
            .await
            .expect("解密环境"),
        original_env
    );
    let target = service
        .local_session_target(&host.id, &CancellationToken::new())
        .await
        .expect("解析本地目标")
        .expect("主机存在");
    assert_eq!(
        target
            .environment
            .get(std::ffi::OsStr::new("PASSWORD"))
            .map(|value| value.as_os_str()),
        Some(std::ffi::OsStr::new("secret-local-value"))
    );
    assert_eq!(
        without_resolver
            .list_local_hosts()
            .await
            .expect("列表无需密钥")
            .len(),
        1
    );
    assert!(
        without_resolver
            .read_local_environment(&host, &CancellationToken::new())
            .await
            .is_err()
    );
    assert!(
        service
            .load_host(&host.id)
            .await
            .expect("远程视图可读取")
            .is_none()
    );
    let unchanged_vault = fs::read(root.path().join("secrets/vault.toml")).expect("冻结密文");
    let conflict = service
        .save_local_host_recoverable(input, None, CancellationToken::new())
        .await;
    assert!(conflict.is_err(), "已有主机不得按新建覆盖");
    assert_eq!(
        fs::read(root.path().join("secrets/vault.toml")).expect("重读密文"),
        unchanged_vault
    );
}

/// 本地主机作为 host 参与分组事务；模板不随分组删除迁移或出现在侧栏。
///
/// @author kongweiguang
#[tokio::test]
async fn group_delete_reassigns_local_hosts_under_the_same_revision_gate() {
    let (_root, service, _) = fixture();
    let group = service
        .save_group(
            HostGroupDraft {
                id: Some("group-a".into()),
                name: "A".into(),
                sort_order: 0,
            },
            None,
        )
        .await
        .expect("创建主机分组");
    let host = saved(
        service
            .save_local_host_recoverable(
                draft("local-group", Some("group-a")),
                None,
                CancellationToken::new(),
            )
            .await
            .expect("保存分组内本地主机"),
    );
    let receipt = service
        .delete_group(
            "group-a".into(),
            None,
            group.revision.expect("分组 revision"),
        )
        .await
        .expect("迁移本地主机并删除组");
    assert_eq!(receipt.reassigned_hosts, 1);
    let updated = service
        .load_local_host(&host.id)
        .await
        .expect("读回")
        .expect("主机保留");
    assert_eq!(updated.group_id, None);
    assert_ne!(updated.revision, host.revision);
}

/// 模拟进程在 Vault 写入后退出，重启只删除未被当前主机引用的候选密文。
///
/// @author kongweiguang
#[tokio::test]
async fn interrupted_environment_save_recovers_from_persisted_journal() {
    for committed in [false, true] {
        let root = tempfile::tempdir().expect("创建隔离数据根");
        let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置");
        let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
        let credentials =
            CredentialService::new(vault.clone() as Arc<dyn CredentialVaultRepository>);
        let repository: Arc<dyn HostManagementRepository> =
            Arc::new(HostConfigurationRepository::new(configuration));
        let host_id = HostId::parse("local-recovery").expect("主机 ID");
        let kind = CredentialKind::parse("local-host").expect("凭据类别");
        let scope = CredentialScope::parse("environment-00000000000000000000000000000001")
            .expect("环境作用域");
        let reference = CredentialReference::new(
            kind.clone(),
            host_id.clone(),
            scope.clone(),
            CredentialMaterial::Password,
        )
        .expect("创建引用")
        .reference()
        .clone();
        let token = LocalHostSaveRecoveryToken::from_parts(
            "00000000-0000-4000-8000-000000000001".into(),
            host_id.clone(),
            None,
            None,
            Some(reference.clone()),
        )
        .expect("创建恢复令牌");
        repository
            .begin_local_host_recovery(&token)
            .await
            .expect("登记恢复日志");
        credentials
            .save(SaveCredentialRequest {
                kind,
                host_id: host_id.clone(),
                scope,
                material: CredentialMaterial::Password,
                secret: SecretValue::new("{\"PASSWORD\":\"recovery-secret\"}".into()),
            })
            .await
            .expect("密封环境");
        let journal = fs::read_to_string(
            root.path()
                .join(".storage-transactions/local-host-environment.json"),
        )
        .expect("恢复日志已持久化");
        assert!(!journal.contains("recovery-secret"));
        assert!(!journal.contains("PASSWORD"));
        if committed {
            repository
                .save_local_host(
                    host_id.clone(),
                    draft("local-recovery", None),
                    Some(reference.clone()),
                    None,
                )
                .await
                .expect("模拟主机已提交");
        }
        drop(repository);
        drop(credentials);
        drop(vault);

        let reopened = ConfigurationStore::initialize(root.path()).expect("重启配置仓储");
        let vault = Arc::new(VaultStore::open(reopened.storage_coordinator()));
        let credentials =
            CredentialService::new(vault.clone() as Arc<dyn CredentialVaultRepository>);
        let repository: Arc<dyn HostManagementRepository> =
            Arc::new(HostConfigurationRepository::new(reopened));
        let service = HostManagementService::with_credentials(repository, credentials.clone())
            .with_local_environment_resolver(vault as Arc<dyn CredentialResolver>);
        assert_eq!(
            service
                .recover_pending_local_host_save()
                .await
                .expect("恢复未完成操作"),
            CredentialCleanupStatus::Complete
        );
        assert_eq!(
            service
                .recover_pending_local_host_save()
                .await
                .expect("幂等重复恢复"),
            CredentialCleanupStatus::Complete
        );
        assert_eq!(
            credentials.inspect(&reference).await.expect("检查密文"),
            if committed {
                kerminal_application::connections::credentials::CredentialPresence::Saved
            } else {
                kerminal_application::connections::credentials::CredentialPresence::Missing
            }
        );
        if committed {
            let host = service
                .load_local_host(&host_id)
                .await
                .expect("读取主机")
                .expect("主机存在");
            assert_eq!(
                service
                    .read_local_environment(&host, &CancellationToken::new())
                    .await
                    .expect("解密环境")
                    .get("PASSWORD")
                    .map(String::as_str),
                Some("recovery-secret")
            );
        }
        assert!(
            !root
                .path()
                .join(".storage-transactions/local-host-environment.json")
                .exists()
        );
    }
}

/// 让 Vault 在提交前保持可控等待，检验恢复不会抢先清理在途候选。
///
/// @author kongweiguang
struct PausedVault {
    inner: Arc<VaultStore>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl CredentialVaultRepository for PausedVault {
    /// 暂停真实密封操作，直到测试释放该次调用。
    ///
    /// @author kongweiguang
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> kerminal_application::AppResult<CredentialVaultSaveMetadata> {
        self.entered.notify_one();
        self.release.notified().await;
        self.inner.save(reference, secret).await
    }

    /// 状态查询沿用实际 Vault，避免测试替身改变存储语义。
    ///
    /// @author kongweiguang
    async fn contains(&self, reference: &CredentialRef) -> kerminal_application::AppResult<bool> {
        self.inner.contains(reference).await
    }

    /// 清理也走真实 Vault，以便发现被提前删除的候选。
    ///
    /// @author kongweiguang
    async fn remove(&self, reference: &CredentialRef) -> kerminal_application::AppResult<bool> {
        self.inner.remove(reference).await
    }
}

/// 同一个 service 的首次恢复与在途保存互斥，恢复不能删除尚未完成的环境密文。
///
/// @author kongweiguang
#[tokio::test]
async fn recovery_waits_for_inflight_local_host_save() {
    let root = tempfile::tempdir().expect("创建隔离数据根");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let paused = Arc::new(PausedVault {
        inner: vault.clone(),
        entered: entered.clone(),
        release: release.clone(),
    });
    let credentials = CredentialService::new(paused as Arc<dyn CredentialVaultRepository>);
    let repository: Arc<dyn HostManagementRepository> =
        Arc::new(HostConfigurationRepository::new(configuration));
    let other_service =
        HostManagementService::with_credentials(repository.clone(), credentials.clone());
    let service = HostManagementService::with_credentials(repository, credentials)
        .with_local_environment_resolver(vault as Arc<dyn CredentialResolver>);
    let mut input = draft("local-concurrent", None);
    input
        .env
        .insert("PASSWORD".into(), "concurrent-secret".into());
    let saving_service = service.clone();
    let saving = tokio::spawn(async move {
        saving_service
            .save_local_host_recoverable(input, None, CancellationToken::new())
            .await
    });
    entered.notified().await;
    let error = other_service
        .recover_pending_local_host_save()
        .await
        .expect_err("独立服务不能抢先恢复在途保存");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::Conflict
    );
    let probe = Command::new(std::env::current_exe().expect("当前测试程序"))
        .args([
            "--exact",
            "local_host_recovery::cross_process_recovery_probe",
            "--nocapture",
        ])
        .env("KERMINAL_TEST_LOCAL_HOST_RECOVERY_ROOT", root.path())
        .status()
        .expect("启动隔离子进程");
    assert!(probe.success(), "其他进程不得抢先恢复在途保存");
    assert!(root.path().join("recovery-probe-complete").is_file());
    let recovering_service = service.clone();
    let mut recovering =
        tokio::spawn(async move { recovering_service.recover_pending_local_host_save().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut recovering)
            .await
            .is_err(),
        "恢复必须等待当前保存结束"
    );
    release.notify_one();
    let host = saved(saving.await.expect("保存任务").expect("保存本地主机"));
    assert_eq!(
        recovering.await.expect("恢复任务").expect("恢复结果"),
        CredentialCleanupStatus::Complete
    );
    assert_eq!(
        service
            .read_local_environment(&host, &CancellationToken::new())
            .await
            .expect("环境仍可解密")
            .get("PASSWORD")
            .map(String::as_str),
        Some("concurrent-secret")
    );
}

#[path = "support/local_host_recovery_integration.rs"]
mod local_host_recovery;
