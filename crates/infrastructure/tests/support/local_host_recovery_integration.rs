//! 本地主机恢复门禁与跨进程锁的故障注入回归。
//!
//! @author kongweiguang

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use kerminal_application::connections::credentials::CredentialPresence;
use kerminal_application::{AppError, AppResult, PublicErrorCode};

/// 对真实 Vault 注入一次未知回执或暂时清理失败，不改变文件存储语义。
struct FaultVault {
    inner: Arc<VaultStore>,
    unknown_after_save: AtomicBool,
    generic_after_save: AtomicBool,
    fail_remove: AtomicBool,
}

#[async_trait]
impl CredentialVaultRepository for FaultVault {
    /// 在真实提交后丢失一次回执，模拟调用方不能判断密文是否已写入。
    ///
    /// @author kongweiguang
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        let result = self.inner.save(reference, secret).await?;
        if self.unknown_after_save.swap(false, Ordering::SeqCst) {
            if self.generic_after_save.load(Ordering::SeqCst) {
                Err(AppError::infrastructure("测试注入阻塞任务回执丢失"))
            } else {
                Err(AppError::outcome_unknown("测试注入密文回执丢失"))
            }
        } else {
            Ok(result)
        }
    }

    /// 查询直接走真实 Vault，确保故障后可判断密文是否保留。
    ///
    /// @author kongweiguang
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool> {
        self.inner.contains(reference).await
    }

    /// 用开关模拟可恢复的清理故障，成功时仍执行真实删除。
    ///
    /// @author kongweiguang
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        if self.fail_remove.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("测试注入凭据清理失败"));
        }
        self.inner.remove(reference).await
    }
}

/// 每个故障测试使用隔离根与同一存储协调器，故障注入仅包装 Vault 端口。
///
/// @author kongweiguang
fn fault_fixture() -> (
    TempDir,
    HostManagementService,
    Arc<FaultVault>,
    CredentialService,
) {
    let root = tempfile::tempdir().expect("创建隔离数据根");
    let configuration = ConfigurationStore::initialize(root.path()).expect("初始化配置仓储");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let fault = Arc::new(FaultVault {
        inner: vault.clone(),
        unknown_after_save: AtomicBool::new(false),
        generic_after_save: AtomicBool::new(false),
        fail_remove: AtomicBool::new(false),
    });
    let credentials = CredentialService::new(fault.clone() as Arc<dyn CredentialVaultRepository>);
    let repository: Arc<dyn HostManagementRepository> =
        Arc::new(HostConfigurationRepository::new(configuration));
    let service = HostManagementService::with_credentials(repository, credentials.clone())
        .with_local_environment_resolver(vault as Arc<dyn CredentialResolver>);
    (root, service, fault, credentials)
}

/// 密文提交结果未知时拒绝再次写入，只有读回清理后才能安全重试。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_vault_save_blocks_retry_until_recovered() {
    for generic_error in [false, true] {
        let (root, service, fault, credentials) = fault_fixture();
        let mut input = draft("local-unknown", None);
        input.env.insert("PASSWORD".into(), "unknown-secret".into());
        fault.unknown_after_save.store(true, Ordering::SeqCst);
        fault
            .generic_after_save
            .store(generic_error, Ordering::SeqCst);
        let recovery = match service
            .save_local_host_recoverable(input.clone(), None, CancellationToken::new())
            .await
            .expect("返回未知结果")
        {
            RecoverableLocalHostSaveOutcome::OutcomeUnknown { error, recovery } => {
                assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
                recovery
            }
            _ => panic!("未知回执不能当成成功"),
        };
        let reference = recovery.new_reference().expect("候选引用").clone();
        assert_eq!(
            credentials.inspect(&reference).await.expect("检查密文"),
            CredentialPresence::Saved
        );
        assert!(
            root.path()
                .join(".storage-transactions/local-host-environment.json")
                .is_file()
        );
        match service
            .save_local_host_recoverable(input.clone(), None, CancellationToken::new())
            .await
        {
            Err(error) => assert_eq!(error.public_code(), PublicErrorCode::Conflict),
            _ => panic!("待恢复事务必须阻止重试"),
        }
        assert_eq!(
            service
                .recover_unknown_local_host_save(&recovery)
                .await
                .expect("读回清理"),
            CredentialCleanupStatus::Complete
        );
        assert_eq!(
            credentials.inspect(&reference).await.expect("检查旧候选"),
            CredentialPresence::Missing
        );
        assert!(
            !root
                .path()
                .join(".storage-transactions/local-host-environment.json")
                .exists()
        );
        saved(
            service
                .save_local_host_recoverable(input, None, CancellationToken::new())
                .await
                .expect("恢复后重试"),
        );
    }
}

/// 删除已提交但 Vault 暂时不可清理时保留 journal，恢复后才放行后续操作。
///
/// @author kongweiguang
#[tokio::test]
async fn failed_delete_cleanup_keeps_journal_until_retry_succeeds() {
    let (root, service, fault, credentials) = fault_fixture();
    let mut input = draft("local-delete", None);
    input.env.insert("PASSWORD".into(), "delete-secret".into());
    let host = saved(
        service
            .save_local_host_recoverable(input, None, CancellationToken::new())
            .await
            .expect("保存主机"),
    );
    let reference = host.environment_ref.clone().expect("主机环境引用");
    fault.fail_remove.store(true, Ordering::SeqCst);
    let receipt = service
        .delete_local_host(&host.id, &host.revision)
        .await
        .expect("删除已提交");
    assert_eq!(
        receipt.credential_cleanup,
        CredentialCleanupStatus::Deferred { remaining: 1 }
    );
    assert!(
        service
            .load_local_host(&host.id)
            .await
            .expect("读取主机")
            .is_none()
    );
    assert_eq!(
        credentials.inspect(&reference).await.expect("检查密文"),
        CredentialPresence::Saved
    );
    assert!(
        root.path()
            .join(".storage-transactions/local-host-environment.json")
            .is_file()
    );
    match service
        .save_local_host_recoverable(draft("next-local", None), None, CancellationToken::new())
        .await
    {
        Err(error) => assert_eq!(error.public_code(), PublicErrorCode::Conflict),
        _ => panic!("清理未完成时不能开始另一操作"),
    }
    assert_eq!(
        service
            .recover_pending_local_host_save()
            .await
            .expect("清理仍失败"),
        CredentialCleanupStatus::Deferred { remaining: 1 }
    );
    fault.fail_remove.store(false, Ordering::SeqCst);
    assert_eq!(
        service
            .recover_pending_local_host_save()
            .await
            .expect("重试清理"),
        CredentialCleanupStatus::Complete
    );
    assert_eq!(
        credentials.inspect(&reference).await.expect("检查清理结果"),
        CredentialPresence::Missing
    );
    assert!(
        !root
            .path()
            .join(".storage-transactions/local-host-environment.json")
            .exists()
    );
    saved(
        service
            .save_local_host_recoverable(draft("next-local", None), None, CancellationToken::new())
            .await
            .expect("清理后恢复写入"),
    );
}

/// 分组在 Vault 保存途中删除时，最终写锁必须拒绝孤立主机并回收候选密文。
///
/// @author kongweiguang
#[tokio::test]
async fn group_deleted_while_local_host_save_is_inflight_rejects_host() {
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
    let service = HostManagementService::with_credentials(repository.clone(), credentials.clone());
    let group = service
        .save_group(
            HostGroupDraft {
                id: Some("removed-group".into()),
                name: "Removed".into(),
                sort_order: 0,
            },
            None,
        )
        .await
        .expect("创建分组");
    let mut input = draft("local-group-race", Some("removed-group"));
    input
        .env
        .insert("PASSWORD".into(), "group-race-secret".into());
    let saving_service = service.clone();
    let saving = tokio::spawn(async move {
        saving_service
            .save_local_host_recoverable(input, None, CancellationToken::new())
            .await
    });
    entered.notified().await;
    let candidate = repository
        .pending_local_host_recovery()
        .await
        .expect("读取恢复日志")
        .expect("保存已登记")
        .new_reference()
        .expect("候选引用")
        .clone();
    service
        .delete_group(
            "removed-group".into(),
            None,
            group.revision.expect("分组 revision"),
        )
        .await
        .expect("删除暂时没有主机的分组");
    release.notify_one();
    match saving.await.expect("保存任务") {
        Err(error) => assert_eq!(error.public_code(), PublicErrorCode::Conflict),
        _ => panic!("不存在的分组不能写入主机"),
    }
    assert!(
        service
            .load_local_host(&HostId::parse("local-group-race").expect("主机 ID"))
            .await
            .expect("读取主机")
            .is_none()
    );
    assert_eq!(
        credentials.inspect(&candidate).await.expect("检查候选"),
        CredentialPresence::Missing
    );
    assert!(
        !root
            .path()
            .join(".storage-transactions/local-host-environment.json")
            .exists()
    );
}

/// 由父测试启动的独立进程检验 OS 锁；普通测试执行时不接触外部配置。
///
/// @author kongweiguang
#[test]
fn cross_process_recovery_probe() {
    let Some(root) = std::env::var_os("KERMINAL_TEST_LOCAL_HOST_RECOVERY_ROOT") else {
        return;
    };
    let configuration = ConfigurationStore::initialize(&root).expect("读取测试根");
    let vault = Arc::new(VaultStore::open(configuration.storage_coordinator()));
    let credentials = CredentialService::new(vault as Arc<dyn CredentialVaultRepository>);
    let repository: Arc<dyn HostManagementRepository> =
        Arc::new(HostConfigurationRepository::new(configuration));
    let service = HostManagementService::with_credentials(repository, credentials);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("建立测试运行时");
    let error = runtime
        .block_on(service.recover_pending_local_host_save())
        .expect_err("活动操作必须占有跨进程锁");
    assert_eq!(error.public_code(), PublicErrorCode::Conflict);
    fs::write(
        std::path::Path::new(&root).join("recovery-probe-complete"),
        b"ok",
    )
    .expect("记录子进程已执行");
}
