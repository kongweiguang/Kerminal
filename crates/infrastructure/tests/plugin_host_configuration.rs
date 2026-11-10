//! 插件主机配置、真实 watcher 和运行提供方的隔离闭环。
//!
//! @author kongweiguang

use std::{
    fs,
    sync::{
        Arc, Condvar, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
    configuration_changes::{ConfigurationChangeMessage, ConfigurationChangeService},
    connections::{
        CredentialResolver, ResolvedCredential,
        credentials::{
            CredentialService, CredentialVaultRepository, CredentialVaultSaveMetadata, SecretValue,
        },
        host_types::{
            BuiltinHost, BuiltinHostOpenIntent, BuiltinHostProvider, HostConfigurationCatalog,
            HostConfigurationCodec, HostConfigurationType, HostTestOutcome, HostTypeProvider,
            PluginHost, PluginHostDraft, PluginHostOpenIntent, PluginHostService,
        },
        management::{
            CredentialCleanupStatus, HostAuthenticationMode, HostCredentialChange,
            HostCredentialCleanup, HostCredentialReferenceCoverage, HostCredentialSlot, HostDraft,
            HostGroupDraft, HostManagementRepository, HostManagementService, LocalHostDraft,
            RecoverableHostSaveOutcome, RecoverableLocalHostSaveOutcome,
            ReferencedCredentialBehavior,
        },
        probe::{
            ConnectionProbeBackend, ConnectionProbeOutcome, ConnectionProbeRequest,
            ConnectionProbeService, ConnectionProbeTarget, ProbeVerificationLevel,
        },
    },
    data_migration::{DataMigrationReadiness, DataMigrationRepository},
    preferences::ConfigurationValidationScope,
};
use kerminal_domain::{
    connections::{
        CredentialRef, HostEndpoint, RemoteProtocol, SshAuthentication,
        credentials::CredentialReference,
    },
    identity::HostId,
    preferences::ConfigurationMode,
};
use kerminal_infrastructure::{
    configuration::{ConfigurationStore, host_configuration_catalog},
    configuration_changes::ConfigurationChangeWatcher,
    connections::management::{AtomicHostCredentialCleanup, HostConfigurationRepository},
    data_migration::FileDataMigrationRepository,
    vault::VaultStore,
};
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

struct FixtureCodec;

#[derive(Default)]
struct FixtureProbeBackend {
    calls: AtomicUsize,
    saw_transient: AtomicBool,
}

#[async_trait]
impl ConnectionProbeBackend for FixtureProbeBackend {
    /// 用真实探测目标验证临时凭据经过贡献分派后仍然存在。
    async fn probe(
        &self,
        target: ConnectionProbeTarget,
        cancel: CancellationToken,
    ) -> AppResult<ConnectionProbeOutcome> {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试已取消"));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.saw_transient
            .store(!target.transient_credentials.is_empty(), Ordering::SeqCst);
        Ok(ConnectionProbeOutcome::Verified(
            ProbeVerificationLevel::SshAuthenticated,
        ))
    }
}

impl HostConfigurationCodec for FixtureCodec {
    /// 测试 codec 必须真实拒绝错误版本与字段，才能验证各入口复用同一规则。
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        if version != 1
            || configuration
                .get("address")
                .and_then(toml::Value::as_str)
                .is_none_or(str::is_empty)
        {
            return Err(AppError::invalid("测试主机配置无效"));
        }
        Ok(configuration.clone())
    }

    /// 测试写入与读取使用同一规则，确保错误不是由测试替身自行放宽。
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }
}

#[derive(Clone, Copy, Debug)]
enum ReferenceClaim {
    BuiltinOnly,
    Referenced,
    MissingProvider,
    DefaultUnproven,
    ExtractionError,
    ExplicitEmpty,
}

struct ReferenceCodec(ReferenceClaim);

impl HostConfigurationCodec for ReferenceCodec {
    /// 复用正常配置校验，使引用回归经过与真实插件相同的解码入口。
    ///
    /// @author kongweiguang
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        FixtureCodec.decode(version, configuration)
    }

    /// 保持编码和解码一致，确保测试中的引用真实写入插件配置。
    ///
    /// @author kongweiguang
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }

    /// 分别模拟完整声明、无引用声明和提取失败，验证宿主不得自行猜测字段。
    ///
    /// @author kongweiguang
    fn credential_references(
        &self,
        _version: u32,
        configuration: &toml::Value,
    ) -> AppResult<Option<Vec<CredentialRef>>> {
        match self.0 {
            ReferenceClaim::Referenced | ReferenceClaim::MissingProvider => {
                let value = configuration
                    .get("secret_ref")
                    .and_then(toml::Value::as_str)
                    .ok_or_else(|| AppError::invalid("测试引用缺失"))?;
                let reference = CredentialRef::new(value.to_owned())
                    .map_err(|_| AppError::invalid("测试引用无效"))?;
                Ok(Some(vec![reference]))
            }
            ReferenceClaim::ExplicitEmpty => Ok(Some(Vec::new())),
            ReferenceClaim::ExtractionError => Err(AppError::invalid("测试引用提取失败")),
            ReferenceClaim::BuiltinOnly | ReferenceClaim::DefaultUnproven => unreachable!(),
        }
    }
}

/// 用有界等待固定清理扫描时序，验证配置写入必须等扫描与密文处理完成。
///
/// @author kongweiguang
struct BlockingReferenceCodec {
    entered: mpsc::SyncSender<()>,
    release: Arc<(StdMutex<bool>, Condvar)>,
    blocked: AtomicBool,
}

impl HostConfigurationCodec for BlockingReferenceCodec {
    /// 使用现有样本的真实配置校验，避免并发测试额外放宽插件格式。
    ///
    /// @author kongweiguang
    fn decode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        FixtureCodec.decode(version, configuration)
    }

    /// 编码沿用相同约束，让写入线程经过正常配置事务。
    ///
    /// @author kongweiguang
    fn encode(&self, version: u32, configuration: &toml::Value) -> AppResult<toml::Value> {
        self.decode(version, configuration)
    }

    /// 首次提取时通知测试并等待释放，其后按实际配置返回完整引用集合。
    ///
    /// @author kongweiguang
    fn credential_references(
        &self,
        _version: u32,
        configuration: &toml::Value,
    ) -> AppResult<Option<Vec<CredentialRef>>> {
        if !self.blocked.swap(true, Ordering::SeqCst) {
            self.entered
                .send(())
                .map_err(|_| AppError::infrastructure("测试扫描通知失败"))?;
            let (mutex, signal) = &*self.release;
            let ready = mutex
                .lock()
                .map_err(|_| AppError::infrastructure("测试扫描门禁失败"))?;
            let (_guard, timeout) = signal
                .wait_timeout_while(ready, Duration::from_secs(5), |ready| !*ready)
                .map_err(|_| AppError::infrastructure("测试扫描等待失败"))?;
            if timeout.timed_out() {
                return Err(AppError::infrastructure("测试扫描等待超时"));
            }
        }
        let reference = configuration
            .get("secret_ref")
            .and_then(toml::Value::as_str)
            .map(|value| CredentialRef::new(value.to_owned()))
            .transpose()
            .map_err(|_| AppError::invalid("测试引用无效"))?;
        Ok(Some(reference.into_iter().collect()))
    }
}

struct UnknownSaveVault {
    inner: Arc<VaultStore>,
    report_unknown: AtomicBool,
    attempted: Mutex<Option<CredentialRef>>,
}

#[async_trait]
impl CredentialVaultRepository for UnknownSaveVault {
    /// 实际密封完成后模拟丢失回执，令恢复用例处理真实 vault 中的候选引用。
    ///
    /// @author kongweiguang
    async fn save(
        &self,
        reference: CredentialReference,
        secret: Zeroizing<String>,
    ) -> AppResult<CredentialVaultSaveMetadata> {
        let attempted = reference.reference().clone();
        let receipt = self.inner.save(reference, secret).await?;
        if self.report_unknown.swap(false, Ordering::SeqCst) {
            *self.attempted.lock().await = Some(attempted);
            Err(AppError::outcome_unknown("测试密封回执丢失"))
        } else {
            Ok(receipt)
        }
    }

    /// 查询仍由真实 vault 执行，避免内存状态掩盖凭据丢失。
    ///
    /// @author kongweiguang
    async fn contains(&self, reference: &CredentialRef) -> AppResult<bool> {
        self.inner.contains(reference).await
    }

    /// 删除转交真实 vault，以验证恢复流程的实际磁盘副作用。
    ///
    /// @author kongweiguang
    async fn remove(&self, reference: &CredentialRef) -> AppResult<bool> {
        self.inner.remove(reference).await
    }
}

#[derive(Default)]
struct FixtureProvider {
    calls: AtomicUsize,
    wait_for_release: bool,
    wait_for_test: bool,
    fail_test: bool,
    content_provider: Option<&'static str>,
    content_version: Option<u32>,
    rdp_host_id: Option<&'static str>,
    entered: Notify,
    release: Notify,
}

#[async_trait]
impl HostTypeProvider for FixtureProvider {
    /// 可由测试暂停实际提供方，验证配置变更和取消发生在运行期间。
    async fn test(&self, _host: &PluginHost, cancel: CancellationToken) -> AppResult<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancel.is_cancelled() {
            return Err(AppError::cancelled("测试已取消"));
        }
        if self.wait_for_test {
            self.entered.notify_one();
            tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("测试已取消")),
                _ = self.release.notified() => {}
            }
        }
        if self.fail_test {
            return Err(AppError::infrastructure("测试提供方失败"));
        }
        Ok(())
    }

    /// 等待门由测试持有，以稳定复现配置更新和取消发生在提供方执行期间。
    async fn open(
        &self,
        _host: &PluginHost,
        cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        if self.wait_for_release {
            tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled("测试已取消")),
                _ = self.release.notified() => {}
            }
        }
        if let Some(host_id) = self.rdp_host_id {
            return Ok(PluginHostOpenIntent::Rdp {
                host_id: HostId::parse(host_id).expect("测试 RDP 主机身份"),
            });
        }
        Ok(PluginHostOpenIntent::Content {
            provider_id: self.content_provider.unwrap_or("example.plugin").to_owned(),
            content_type_id: "example.content".to_owned(),
            title: "Fixture".to_owned(),
            state_version: self.content_version.unwrap_or(1),
            state: serde_json::json!({ "connected": false }),
        })
    }
}

struct ForgedBuiltinProvider;

#[async_trait]
impl HostTypeProvider for ForgedBuiltinProvider {
    /// 测试替身不接受扩展主机，避免混淆被测身份分支。
    async fn test(&self, _host: &PluginHost, _cancel: CancellationToken) -> AppResult<()> {
        Err(AppError::unsupported("测试仅覆盖内置主机"))
    }

    /// 测试替身不为扩展主机构造打开意图。
    async fn open(
        &self,
        _host: &PluginHost,
        _cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        Err(AppError::unsupported("测试仅覆盖内置主机"))
    }

    /// 模拟提供方试图把当前内置配置重定向到其他主机。
    async fn open_builtin(
        &self,
        host: &BuiltinHost,
        _cancel: CancellationToken,
    ) -> AppResult<PluginHostOpenIntent> {
        Ok(PluginHostOpenIntent::Builtin(BuiltinHostOpenIntent {
            host_id: HostId::parse("other-host").expect("测试身份"),
            configuration_type: host.configuration_type().to_owned(),
        }))
    }
}

/// 构造无凭据的 SSH 样本，避免运行接口测试依赖外部网络或 Vault。
fn builtin_remote_draft(id: HostId) -> HostDraft {
    HostDraft {
        id: Some(id),
        group_id: None,
        name: "Runtime SSH".to_owned(),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 22,
            username: "test".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: Default::default(),
        serial: None,
        tags: Vec::new(),
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: None,
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
    }
}

/// 配置目录冻结后才装配运行服务时，列表和打开操作都必须看到同一提供方。
#[tokio::test]
async fn runtime_provider_can_bind_after_configuration_initialization() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let types = catalog(None);
    let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
        .expect("初始化配置");
    let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
    let id = HostId::parse("late-runtime").expect("合法主机 ID");
    hosts
        .save_plugin_host(draft(id.clone(), None, "one"), None)
        .await
        .expect("保存扩展配置");
    assert!(
        !hosts
            .load_plugin_host(&id)
            .await
            .expect("读取")
            .expect("存在")
            .is_runnable()
    );
    let provider = Arc::new(FixtureProvider::default());
    assert!(
        types
            .bind_runtime_provider("wrong.plugin", "example-host", provider.clone())
            .is_err()
    );
    types
        .bind_runtime_provider("example.plugin", "example-host", provider.clone())
        .expect("运行服务就绪后绑定");
    assert!(
        types
            .bind_runtime_provider("example.plugin", "example-host", provider)
            .is_err()
    );
    assert!(
        hosts
            .load_plugin_host(&id)
            .await
            .expect("重新读取")
            .expect("存在")
            .is_runnable()
    );
    assert!(matches!(
        PluginHostService::new(hosts, types)
            .open(&id, CancellationToken::new())
            .await,
        Ok(PluginHostOpenIntent::Content { .. })
    ));
}

/// 内置本地与 SSH 使用真实仓储读回并返回既有连接入口所需的原始身份。
#[tokio::test]
async fn builtin_runtime_returns_typed_remote_and_local_intents() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let types = host_configuration_catalog(Vec::new()).expect("内置目录");
    let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
        .expect("初始化配置");
    let repository = Arc::new(HostConfigurationRepository::new(store));
    let hosts = HostManagementService::new(repository.clone());
    let remote_id = HostId::parse("runtime-ssh").expect("合法主机 ID");
    let local_id = HostId::parse("runtime-local").expect("合法主机 ID");
    hosts
        .save_host(builtin_remote_draft(remote_id.clone()), None)
        .await
        .expect("保存远程主机");
    repository
        .save_local_host(
            local_id.clone(),
            LocalHostDraft {
                id: Some(local_id.clone()),
                group_id: None,
                name: "Runtime local".to_owned(),
                shell: "test-shell".to_owned(),
                args: Vec::new(),
                cwd: None,
                env: Default::default(),
                sort_order: 0,
            },
            None,
            None,
        )
        .await
        .expect("保存本地主机");
    let probe = Arc::new(ConnectionProbeService::new(Arc::new(
        FixtureProbeBackend::default(),
    )));
    let builtin = Arc::new(BuiltinHostProvider::new(probe));
    for type_id in ["ssh", "local"] {
        types
            .bind_runtime_provider("kerminal", type_id, builtin.clone())
            .expect("绑定内置实现");
    }
    let runtime = PluginHostService::new(hosts, types);
    for (id, expected_type) in [(remote_id, "ssh"), (local_id, "local")] {
        assert!(matches!(
            runtime.open(&id, CancellationToken::new()).await,
            Ok(PluginHostOpenIntent::Builtin(intent))
                if intent.host_id == id && intent.configuration_type == expected_type
        ));
    }
}

/// 草稿和已保存主机经同一提供方探测，临时输入与真实结果不可降级为普通成功。
#[tokio::test]
async fn builtin_probe_routes_draft_and_preserves_typed_results() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let types = host_configuration_catalog(Vec::new()).expect("内置目录");
    let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
        .expect("初始化配置");
    let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
    let backend = Arc::new(FixtureProbeBackend::default());
    let raw = Arc::new(ConnectionProbeService::new(backend.clone()));
    types
        .bind_runtime_provider("kerminal", "ssh", Arc::new(BuiltinHostProvider::new(raw)))
        .expect("绑定内置提供方");
    let routed = ConnectionProbeService::new(backend.clone()).with_host_types(types.clone());
    let id = HostId::parse("probe-host").expect("合法主机 ID");
    let mut draft = builtin_remote_draft(id.clone());
    draft.authentication_mode = HostAuthenticationMode::Password;
    let outcome = routed
        .probe(
            ConnectionProbeRequest {
                draft,
                credential_changes: vec![HostCredentialChange::Replace {
                    slot: HostCredentialSlot::TargetPassword,
                    secret: SecretValue::new("fixture secret".to_owned()),
                }],
                timeout: Duration::from_secs(10),
            },
            CancellationToken::new(),
        )
        .await
        .expect("草稿探测");
    assert!(matches!(
        outcome,
        ConnectionProbeOutcome::Verified(ProbeVerificationLevel::SshAuthenticated)
    ));
    assert!(backend.saw_transient.load(Ordering::SeqCst));

    hosts
        .save_host(builtin_remote_draft(id.clone()), None)
        .await
        .expect("保存主机");
    let runtime = PluginHostService::new(hosts, types);
    assert!(matches!(
        runtime
            .test(&id, CancellationToken::new())
            .await
            .expect("保存主机探测"),
        HostTestOutcome::Builtin(ConnectionProbeOutcome::Verified(
            ProbeVerificationLevel::SshAuthenticated
        ))
    ));
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        runtime
            .test(&id, cancelled)
            .await
            .expect_err("取消不得探测")
            .public_code(),
        PublicErrorCode::Cancelled
    );
    assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
}

/// 运行提供方不能借内置或 RDP 意图切换到其他持久化主机。
#[tokio::test]
async fn provider_rejects_cross_host_builtin_and_rdp_intents() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let types = catalog(Some(Arc::new(FixtureProvider {
        rdp_host_id: Some("other-host"),
        ..Default::default()
    })));
    let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
        .expect("初始化配置");
    let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
    let plugin_id = HostId::parse("plugin-host").expect("合法主机 ID");
    let builtin_id = HostId::parse("builtin-host").expect("合法主机 ID");
    hosts
        .save_plugin_host(draft(plugin_id.clone(), None, "one"), None)
        .await
        .expect("保存扩展主机");
    hosts
        .save_host(builtin_remote_draft(builtin_id.clone()), None)
        .await
        .expect("保存内置主机");
    types
        .bind_runtime_provider("kerminal", "ssh", Arc::new(ForgedBuiltinProvider))
        .expect("绑定测试提供方");
    let runtime = PluginHostService::new(hosts, types);
    for id in [plugin_id, builtin_id] {
        assert_eq!(
            runtime
                .open(&id, CancellationToken::new())
                .await
                .expect_err("拒绝跨主机意图")
                .public_code(),
            PublicErrorCode::Invalid
        );
    }
}

/// 每个测试共享同一冻结目录，生产扫描和主机仓储因此观察到同一 codec。
fn catalog(provider: Option<Arc<FixtureProvider>>) -> Arc<HostConfigurationCatalog> {
    let mut host_type =
        HostConfigurationType::new("example.plugin", "example-host", Arc::new(FixtureCodec))
            .expect("构造测试类型");
    if let Some(provider) = provider {
        host_type = host_type.with_provider(provider);
    }
    host_configuration_catalog(vec![host_type]).expect("冻结测试目录")
}

/// 附加运行实现不能改变六类配置的 codec 约束；伪造实现和错绑协议仍需在扫描前拒绝。
#[test]
fn builtin_codecs_allow_runtime_providers_but_reject_substitution() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let original = host_configuration_catalog(Vec::new()).expect("内置目录");
    let provider = Arc::new(FixtureProvider::default());
    let entries = original
        .iter()
        .cloned()
        .map(|entry| entry.with_provider(provider.clone()))
        .collect::<Vec<_>>();
    let with_runtime = Arc::new(HostConfigurationCatalog::new(entries.clone()).expect("冻结目录"));
    assert_eq!(with_runtime.iter().count(), 6);
    assert!(with_runtime.iter().all(|entry| entry.provider().is_some()));
    assert!(ConfigurationStore::initialize_with_host_types(root.path(), with_runtime).is_ok());

    let mut forged = entries.clone();
    let ssh = forged
        .iter_mut()
        .find(|entry| entry.type_id == "ssh")
        .expect("SSH");
    ssh.codec = Arc::new(FixtureCodec);
    let forged = HostConfigurationCatalog::new(forged).expect("冻结伪造目录");
    assert!(
        ConfigurationStore::validate_root_with_host_types(
            root.path(),
            ConfigurationValidationScope::Hosts,
            &forged,
        )
        .is_err()
    );

    let mut mismatched = entries;
    let sftp_codec = original
        .get("kerminal", "sftp")
        .expect("SFTP")
        .codec
        .clone();
    let ssh = mismatched
        .iter_mut()
        .find(|entry| entry.type_id == "ssh")
        .expect("SSH");
    ssh.codec = sftp_codec;
    let mismatched = Arc::new(HostConfigurationCatalog::new(mismatched).expect("冻结错绑目录"));
    assert!(ConfigurationStore::initialize_with_host_types(root.path(), mismatched).is_err());
}

/// 创建主机只使用非秘密字段，避免把测试数据路径或凭据带入共享环境。
fn draft(id: HostId, group_id: Option<&str>, address: &str) -> PluginHostDraft {
    PluginHostDraft {
        id: Some(id),
        provider_id: "example.plugin".to_owned(),
        type_id: "example-host".to_owned(),
        name: "Fixture host".to_owned(),
        group_id: group_id.map(str::to_owned),
        sort_order: 3,
        configuration_version: 1,
        configuration: toml::Value::try_from(
            [("address", address)]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>(),
        )
        .expect("构造配置表"),
    }
}

/// 真实仓储与 Vault 覆盖删除、替换和插件引用声明，防止仅用内存替身漏掉目录投影。
///
/// @author kongweiguang
#[tokio::test]
async fn credential_cleanup_requires_complete_plugin_reference_coverage() {
    for claim in [
        ReferenceClaim::BuiltinOnly,
        ReferenceClaim::Referenced,
        ReferenceClaim::MissingProvider,
        ReferenceClaim::DefaultUnproven,
        ReferenceClaim::ExtractionError,
        ReferenceClaim::ExplicitEmpty,
    ] {
        for replace in [false, true] {
            let root = tempfile::tempdir().expect("隔离配置根");
            let host_types = if matches!(claim, ReferenceClaim::BuiltinOnly) {
                host_configuration_catalog(Vec::new()).expect("内置目录")
            } else {
                let codec: Arc<dyn HostConfigurationCodec> =
                    if matches!(claim, ReferenceClaim::DefaultUnproven) {
                        Arc::new(FixtureCodec)
                    } else {
                        Arc::new(ReferenceCodec(claim))
                    };
                let host_type = HostConfigurationType::new("example.plugin", "example-host", codec)
                    .expect("注册测试类型");
                host_configuration_catalog(vec![host_type]).expect("冻结目录")
            };
            let store = ConfigurationStore::initialize_with_host_types(root.path(), host_types)
                .expect("初始化配置");
            let mut vault = Arc::new(VaultStore::open(store.storage_coordinator()));
            let mut hosts = HostManagementService::with_credentials(
                Arc::new(HostConfigurationRepository::new(store.clone())),
                CredentialService::new(vault.clone()),
            )
            .with_credential_cleanup(Arc::new(AtomicHostCredentialCleanup::new(store)));
            let host_id = HostId::parse("cleanup-ssh").expect("主机 ID");
            let mut ssh = builtin_remote_draft(host_id.clone());
            ssh.authentication_mode = HostAuthenticationMode::Password;
            let initial = hosts
                .save_host_with_credentials(
                    ssh,
                    None,
                    vec![HostCredentialChange::Replace {
                        slot: HostCredentialSlot::TargetPassword,
                        secret: SecretValue::new("initial test secret".to_owned()),
                    }],
                    CancellationToken::new(),
                )
                .await
                .expect("保存 SSH 主机及初始凭据");
            let old = initial.credentials[0].reference.clone();

            if !matches!(claim, ReferenceClaim::BuiltinOnly) {
                let mut plugin = draft(
                    HostId::parse("cleanup-plugin").expect("插件主机 ID"),
                    None,
                    "example.invalid",
                );
                if !matches!(claim, ReferenceClaim::ExplicitEmpty) {
                    plugin
                        .configuration
                        .as_table_mut()
                        .expect("配置是表")
                        .insert(
                            "secret_ref".to_owned(),
                            toml::Value::String(old.as_str().to_owned()),
                        );
                }
                hosts
                    .save_plugin_host(plugin, None)
                    .await
                    .expect("保存插件主机");
            }

            if matches!(claim, ReferenceClaim::MissingProvider) {
                let store = ConfigurationStore::initialize_with_host_types(
                    root.path(),
                    host_configuration_catalog(Vec::new()).expect("缺失插件目录"),
                )
                .expect("重新打开未知提供方配置");
                vault = Arc::new(VaultStore::open(store.storage_coordinator()));
                hosts = HostManagementService::with_credentials(
                    Arc::new(HostConfigurationRepository::new(store.clone())),
                    CredentialService::new(vault.clone()),
                )
                .with_credential_cleanup(Arc::new(AtomicHostCredentialCleanup::new(store)));
            }

            let status = if replace {
                let mut replacement = builtin_remote_draft(host_id.clone());
                replacement.authentication_mode = HostAuthenticationMode::Password;
                replacement.authentication = Some(SshAuthentication::Password {
                    secret: old.clone(),
                });
                let receipt = hosts
                    .save_host_with_credentials(
                        replacement,
                        Some(initial.host.revision.clone()),
                        vec![HostCredentialChange::Replace {
                            slot: HostCredentialSlot::TargetPassword,
                            secret: SecretValue::new("replacement test secret".to_owned()),
                        }],
                        CancellationToken::new(),
                    )
                    .await
                    .expect("替换 SSH 凭据");
                assert!(
                    vault
                        .contains(&receipt.credentials[0].reference)
                        .await
                        .expect("检查新凭据")
                );
                receipt.cleanup
            } else {
                hosts
                    .delete_host(&host_id, &initial.host.revision)
                    .await
                    .expect("删除 SSH 主机")
                    .credential_cleanup
            };
            let retained = !matches!(
                claim,
                ReferenceClaim::BuiltinOnly | ReferenceClaim::ExplicitEmpty
            );
            assert_eq!(
                status,
                if retained {
                    CredentialCleanupStatus::Deferred { remaining: 1 }
                } else {
                    CredentialCleanupStatus::Complete
                },
                "引用声明 {claim:?}，替换 {replace}"
            );
            assert_eq!(vault.contains(&old).await.expect("检查旧凭据"), retained);
            if retained {
                let resolved = vault
                    .resolve(&old, &CancellationToken::new())
                    .await
                    .expect("保留的旧凭据可解析");
                match resolved {
                    ResolvedCredential::Password(secret) => {
                        assert_eq!(secret.as_str(), "initial test secret")
                    }
                    _ => panic!("旧凭据应保留为密码"),
                }
            }
        }
    }
}

/// 未知密封回执后原主机可被替换为插件；恢复必须保留插件已经接管的候选引用。
///
/// @author kongweiguang
#[tokio::test]
async fn unknown_credential_save_recovery_preserves_reference_in_replacement_plugin() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let host_type = HostConfigurationType::new(
        "example.plugin",
        "example-host",
        Arc::new(ReferenceCodec(ReferenceClaim::Referenced)),
    )
    .expect("注册插件类型");
    let types = host_configuration_catalog(vec![host_type]).expect("冻结目录");
    let store =
        ConfigurationStore::initialize_with_host_types(root.path(), types).expect("初始化配置");
    let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
    let fault = Arc::new(UnknownSaveVault {
        inner: vault.clone(),
        report_unknown: AtomicBool::new(false),
        attempted: Mutex::new(None),
    });
    let repository = Arc::new(HostConfigurationRepository::new(store));
    let hosts = HostManagementService::with_credentials(
        repository.clone(),
        CredentialService::new(fault.clone()),
    );
    let host_id = HostId::parse("recovery-plugin").expect("主机 ID");
    let mut ssh = builtin_remote_draft(host_id.clone());
    ssh.authentication_mode = HostAuthenticationMode::Password;
    let initial = hosts
        .save_host_with_credentials(
            ssh,
            None,
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("old recovery secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("保存原主机");
    let old = initial.credentials[0].reference.clone();
    fault.report_unknown.store(true, Ordering::SeqCst);
    let mut replacement = builtin_remote_draft(host_id.clone());
    replacement.authentication_mode = HostAuthenticationMode::Password;
    replacement.authentication = Some(SshAuthentication::Password {
        secret: old.clone(),
    });
    let recovery = match hosts
        .save_host_with_credentials_recoverable(
            replacement,
            Some(initial.host.revision.clone()),
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("new recovery secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("返回未知结果及恢复令牌")
    {
        RecoverableHostSaveOutcome::OutcomeUnknown { recovery, .. } => recovery,
        RecoverableHostSaveOutcome::Saved(_) => panic!("测试必须模拟未知回执"),
    };
    let new = fault
        .attempted
        .lock()
        .await
        .clone()
        .expect("捕获已密封引用");
    repository
        .delete_host(&host_id, &initial.host.revision)
        .await
        .expect("外部删除原主机");
    let mut plugin = draft(host_id.clone(), None, "example.invalid");
    plugin
        .configuration
        .as_table_mut()
        .expect("配置是表")
        .insert(
            "secret_ref".to_owned(),
            toml::Value::String(new.as_str().to_owned()),
        );
    hosts
        .save_plugin_host(plugin, None)
        .await
        .expect("以同一 ID 保存插件主机");

    assert_eq!(
        hosts
            .recover_unknown_host_save(&recovery)
            .await
            .expect("显式读回后恢复"),
        CredentialCleanupStatus::Complete
    );
    assert!(!vault.contains(&old).await.expect("检查旧候选"));
    assert!(vault.contains(&new).await.expect("检查插件引用"));
    match vault
        .resolve(&new, &CancellationToken::new())
        .await
        .expect("插件引用仍可解析")
    {
        ResolvedCredential::Password(secret) => {
            assert_eq!(secret.as_str(), "new recovery secret");
        }
        _ => panic!("插件引用应保留为密码"),
    }
}

/// 本地环境引用属于同一凭据目录，覆盖结果必须包含它以供各类候选清理复用。
///
/// @author kongweiguang
#[tokio::test]
async fn credential_reference_coverage_includes_local_environment() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let store = ConfigurationStore::initialize(root.path()).expect("初始化配置");
    let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
    let repository = Arc::new(HostConfigurationRepository::new(store));
    let hosts =
        HostManagementService::with_credentials(repository.clone(), CredentialService::new(vault));
    let local = LocalHostDraft {
        id: Some(HostId::parse("coverage-local").expect("主机 ID")),
        group_id: None,
        name: "Local".to_owned(),
        shell: "test-shell".to_owned(),
        args: Vec::new(),
        cwd: None,
        env: [("TEST_KEY".to_owned(), "test value".to_owned())]
            .into_iter()
            .collect(),
        sort_order: 0,
    };
    let host = match hosts
        .save_local_host_recoverable(local, None, CancellationToken::new())
        .await
        .expect("保存本地主机")
    {
        RecoverableLocalHostSaveOutcome::Saved { host, .. } => host,
        RecoverableLocalHostSaveOutcome::OutcomeUnknown { .. } => panic!("本地主机应确认保存"),
    };
    let reference = host.environment_ref.expect("环境引用");
    match repository
        .credential_reference_coverage()
        .await
        .expect("扫描主机引用")
    {
        HostCredentialReferenceCoverage::Complete {
            additional_references,
            ..
        } => assert!(additional_references.contains(&reference)),
        HostCredentialReferenceCoverage::Unproven => panic!("内置目录应能证明引用覆盖"),
    }
}

/// 为环境保存与恢复测试复用稳定主机 ID，避免环境数据以明文写入文件配置。
///
/// @author kongweiguang
fn local_environment_draft(id: &HostId, value: &str) -> LocalHostDraft {
    LocalHostDraft {
        id: Some(id.clone()),
        group_id: None,
        name: "Local".to_owned(),
        shell: "test-shell".to_owned(),
        args: Vec::new(),
        cwd: None,
        env: [("TEST_KEY".to_owned(), value.to_owned())]
            .into_iter()
            .collect(),
        sort_order: 0,
    }
}

/// 已确认的插件引用保留密文，但不能使已提交的本地主机事务永久占用恢复门禁。
///
/// @author kongweiguang
#[tokio::test]
async fn local_environment_cleanup_preserves_known_plugin_references() {
    for delete in [false, true] {
        let root = tempfile::tempdir().expect("隔离配置根");
        let host_type = HostConfigurationType::new(
            "example.plugin",
            "example-host",
            Arc::new(ReferenceCodec(ReferenceClaim::Referenced)),
        )
        .expect("注册插件类型");
        let store = ConfigurationStore::initialize_with_host_types(
            root.path(),
            host_configuration_catalog(vec![host_type]).expect("冻结目录"),
        )
        .expect("初始化配置");
        let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
        let repository = Arc::new(HostConfigurationRepository::new(store.clone()));
        let hosts = HostManagementService::with_credentials(
            repository.clone(),
            CredentialService::new(vault.clone()),
        )
        .with_credential_cleanup(Arc::new(AtomicHostCredentialCleanup::new(store)));
        let host_id = HostId::parse("local-known-reference").expect("主机 ID");
        let initial = match hosts
            .save_local_host_recoverable(
                local_environment_draft(&host_id, "first"),
                None,
                CancellationToken::new(),
            )
            .await
            .expect("保存本地主机")
        {
            RecoverableLocalHostSaveOutcome::Saved { host, .. } => host,
            RecoverableLocalHostSaveOutcome::OutcomeUnknown { .. } => panic!("保存结果应明确"),
        };
        let old = initial.environment_ref.clone().expect("环境引用");
        let mut plugin = draft(
            HostId::parse("local-reference-plugin").expect("插件主机 ID"),
            None,
            "example.invalid",
        );
        plugin
            .configuration
            .as_table_mut()
            .expect("配置是表")
            .insert(
                "secret_ref".to_owned(),
                toml::Value::String(old.as_str().to_owned()),
            );
        hosts
            .save_plugin_host(plugin, None)
            .await
            .expect("保存插件主机");

        let status = if delete {
            hosts
                .delete_local_host(&host_id, &initial.revision)
                .await
                .expect("删除本地主机")
                .credential_cleanup
        } else {
            match hosts
                .save_local_host_recoverable(
                    local_environment_draft(&host_id, "second"),
                    Some(initial.revision),
                    CancellationToken::new(),
                )
                .await
                .expect("替换本地环境")
            {
                RecoverableLocalHostSaveOutcome::Saved {
                    host,
                    credential_cleanup,
                } => {
                    let next = hosts
                        .save_local_host_recoverable(
                            local_environment_draft(&host_id, "third"),
                            Some(host.revision),
                            CancellationToken::new(),
                        )
                        .await
                        .expect("确认恢复门禁已解除");
                    assert!(matches!(
                        next,
                        RecoverableLocalHostSaveOutcome::Saved { .. }
                    ));
                    credential_cleanup
                }
                RecoverableLocalHostSaveOutcome::OutcomeUnknown { .. } => panic!("保存结果应明确"),
            }
        };
        assert_eq!(status, CredentialCleanupStatus::Complete);
        assert!(vault.contains(&old).await.expect("检查插件引用"));
        assert!(
            repository
                .pending_local_host_recovery()
                .await
                .expect("检查恢复记录")
                .is_none()
        );
    }
}

/// 未知引用覆盖须保留本地恢复记录；覆盖重新可证明后只清理旧环境并保留当前环境。
///
/// @author kongweiguang
#[tokio::test]
async fn local_environment_recovery_waits_for_complete_plugin_coverage() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let store = ConfigurationStore::initialize_with_host_types(root.path(), catalog(None))
        .expect("初始化配置");
    let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
    let repository = Arc::new(HostConfigurationRepository::new(store.clone()));
    let hosts = HostManagementService::with_credentials(
        repository.clone(),
        CredentialService::new(vault.clone()),
    )
    .with_credential_cleanup(Arc::new(AtomicHostCredentialCleanup::new(store)));
    let host_id = HostId::parse("local-unknown-reference").expect("主机 ID");
    let initial = match hosts
        .save_local_host_recoverable(
            local_environment_draft(&host_id, "first"),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("保存本地主机")
    {
        RecoverableLocalHostSaveOutcome::Saved { host, .. } => host,
        RecoverableLocalHostSaveOutcome::OutcomeUnknown { .. } => panic!("保存结果应明确"),
    };
    let old = initial.environment_ref.clone().expect("原环境引用");
    hosts
        .save_plugin_host(
            draft(
                HostId::parse("unproven-plugin").expect("插件主机 ID"),
                None,
                "example.invalid",
            ),
            None,
        )
        .await
        .expect("保存未声明引用的插件");
    let current = match hosts
        .save_local_host_recoverable(
            local_environment_draft(&host_id, "second"),
            Some(initial.revision),
            CancellationToken::new(),
        )
        .await
        .expect("替换本地环境")
    {
        RecoverableLocalHostSaveOutcome::Saved {
            host,
            credential_cleanup,
        } => {
            assert_eq!(
                credential_cleanup,
                CredentialCleanupStatus::Deferred { remaining: 1 }
            );
            host.environment_ref.expect("当前环境引用")
        }
        RecoverableLocalHostSaveOutcome::OutcomeUnknown { .. } => panic!("保存结果应明确"),
    };
    assert!(vault.contains(&old).await.expect("检查旧环境"));
    assert!(
        repository
            .pending_local_host_recovery()
            .await
            .expect("读取恢复记录")
            .is_some()
    );
    assert_eq!(
        hosts
            .recover_pending_local_host_save()
            .await
            .expect("尝试恢复"),
        CredentialCleanupStatus::Deferred { remaining: 1 }
    );

    let host_type = HostConfigurationType::new(
        "example.plugin",
        "example-host",
        Arc::new(ReferenceCodec(ReferenceClaim::ExplicitEmpty)),
    )
    .expect("注册可证明无引用的类型");
    let store = ConfigurationStore::initialize_with_host_types(
        root.path(),
        host_configuration_catalog(vec![host_type]).expect("冻结目录"),
    )
    .expect("重新初始化配置");
    let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
    let repository = Arc::new(HostConfigurationRepository::new(store.clone()));
    let hosts = HostManagementService::with_credentials(
        repository.clone(),
        CredentialService::new(vault.clone()),
    )
    .with_credential_cleanup(Arc::new(AtomicHostCredentialCleanup::new(store)));
    assert_eq!(
        hosts
            .recover_pending_local_host_save()
            .await
            .expect("重试恢复"),
        CredentialCleanupStatus::Complete
    );
    assert!(!vault.contains(&old).await.expect("检查旧环境已清理"));
    assert!(vault.contains(&current).await.expect("检查当前环境已保留"));
    assert!(
        repository
            .pending_local_host_recovery()
            .await
            .expect("检查恢复记录")
            .is_none()
    );
}

/// 扫描和删除之间不得让共享协调器的配置写入穿插，释放后写入与重试按顺序完成。
///
/// @author kongweiguang
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn atomic_cleanup_serializes_configuration_writes() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let release = Arc::new((StdMutex::new(false), Condvar::new()));
    let codec = Arc::new(BlockingReferenceCodec {
        entered: entered_tx,
        release: release.clone(),
        blocked: AtomicBool::new(false),
    });
    let host_type =
        HostConfigurationType::new("example.plugin", "example-host", codec).expect("注册插件类型");
    let store = ConfigurationStore::initialize_with_host_types(
        root.path(),
        host_configuration_catalog(vec![host_type]).expect("冻结目录"),
    )
    .expect("初始化配置");
    let vault = Arc::new(VaultStore::open(store.storage_coordinator()));
    let repository = Arc::new(HostConfigurationRepository::new(store.clone()));
    let hosts = HostManagementService::with_credentials(
        repository.clone(),
        CredentialService::new(vault.clone()),
    );
    let host_id = HostId::parse("atomic-cleanup-ssh").expect("主机 ID");
    let mut ssh = builtin_remote_draft(host_id.clone());
    ssh.authentication_mode = HostAuthenticationMode::Password;
    let initial = hosts
        .save_host_with_credentials(
            ssh,
            None,
            vec![HostCredentialChange::Replace {
                slot: HostCredentialSlot::TargetPassword,
                secret: SecretValue::new("atomic test secret".to_owned()),
            }],
            CancellationToken::new(),
        )
        .await
        .expect("保存 SSH 凭据");
    let reference = initial.credentials[0].reference.clone();
    let plugin_id = HostId::parse("atomic-cleanup-plugin").expect("插件主机 ID");
    let mut plugin = draft(plugin_id.clone(), None, "example.invalid");
    plugin
        .configuration
        .as_table_mut()
        .expect("配置是表")
        .insert(
            "secret_ref".to_owned(),
            toml::Value::String(reference.as_str().to_owned()),
        );
    let plugin = hosts
        .save_plugin_host(plugin, None)
        .await
        .expect("保存插件引用");
    repository
        .delete_host(&host_id, &initial.host.revision)
        .await
        .expect("删除原主机以准备清理候选");

    let cleanup = AtomicHostCredentialCleanup::new(store);
    let cleanup_task = {
        let cleanup = cleanup.clone();
        let reference = reference.clone();
        tokio::spawn(async move {
            cleanup
                .cleanup(&[reference], ReferencedCredentialBehavior::Defer)
                .await
        })
    };
    let entered =
        tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(4)))
            .await
            .expect("扫描通知任务结束")
            .is_ok();
    if !entered {
        let (mutex, signal) = &*release;
        *mutex.lock().expect("获取扫描门禁") = true;
        signal.notify_all();
        panic!("凭据扫描未进入插件 codec");
    }
    let started = Arc::new(Notify::new());
    let mut writer = {
        let hosts = hosts.clone();
        let started = started.clone();
        tokio::spawn(async move {
            started.notify_one();
            hosts
                .save_plugin_host(
                    draft(plugin_id, None, "example.invalid"),
                    Some(plugin.revision),
                )
                .await
        })
    };
    started.notified().await;
    let waited = tokio::time::timeout(Duration::from_millis(150), &mut writer)
        .await
        .is_err();
    let (mutex, signal) = &*release;
    *mutex.lock().expect("获取扫描门禁") = true;
    signal.notify_all();
    assert!(waited, "配置写入应等待凭据扫描释放存储锁");
    assert_eq!(
        cleanup_task
            .await
            .expect("清理任务结束")
            .expect("读取清理状态"),
        CredentialCleanupStatus::Deferred { remaining: 1 }
    );
    writer.await.expect("写入任务结束").expect("更新插件配置");
    assert!(vault.contains(&reference).await.expect("原引用仍存在"));
    assert_eq!(
        cleanup
            .cleanup(
                std::slice::from_ref(&reference),
                ReferencedCredentialBehavior::Defer
            )
            .await
            .expect("移除插件引用后重试"),
        CredentialCleanupStatus::Complete
    );
    assert!(!vault.contains(&reference).await.expect("无引用后已清理"));
}

/// 等待指定恢复态的真实 watcher 通知，并过滤同一次文件修改产生的中间事件。
async fn wait_for_mode(
    changes: &mut kerminal_application::configuration_changes::ConfigurationChangeSubscription,
    expected: ConfigurationMode,
) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match changes.recv(CancellationToken::new()).await {
                ConfigurationChangeMessage::Changed(event)
                    if event.recovery_state.mode == expected =>
                {
                    return;
                }
                ConfigurationChangeMessage::Changed(_)
                | ConfigurationChangeMessage::RescanRequired => {}
                ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed => {
                    panic!("配置监听意外结束")
                }
            }
        }
    })
    .await
    .expect("等待 watcher 更新恢复态");
}

/// 同一 codec 控制保存、读回、外部修改、只读 validator 和恢复门禁。
#[tokio::test]
async fn codec_round_trip_and_watcher_recovery_use_the_shared_catalog() {
    let root = tempfile::tempdir().expect("隔离配置根");
    fs::write(root.path().join(".gitignore"), "secrets/vault-key.toml\n")
        .expect("配置校验所需的密钥忽略规则");
    let types = catalog(Some(Arc::new(FixtureProvider::default())));
    let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
        .expect("初始化配置");
    let hosts =
        HostManagementService::new(Arc::new(HostConfigurationRepository::new(store.clone())));
    let id = HostId::parse("fixture-host").expect("合法主机 ID");
    let saved = hosts
        .save_plugin_host(draft(id.clone(), None, "one"), None)
        .await
        .expect("保存");
    assert!(saved.is_available());
    assert!(saved.is_runnable());
    assert_eq!(
        saved
            .configuration
            .as_ref()
            .and_then(|value| value.get("address"))
            .and_then(toml::Value::as_str),
        Some("one")
    );
    assert!(hosts.list_hosts().await.expect("读取内置主机").is_empty());
    assert_eq!(
        hosts.list_plugin_hosts().await.expect("读取插件主机").len(),
        1
    );

    let changes = ConfigurationChangeService::new(Arc::new(store.clone()));
    let mut subscription = changes.subscribe();
    let mut watcher = ConfigurationChangeWatcher::start(store.clone()).expect("启动隔离 watcher");
    let path = root.path().join("hosts/fixture-host.toml");
    let original = fs::read_to_string(&path).expect("读取配置");
    let invalid = original.replace("address = \"one\"", "address = \"\"");
    assert_ne!(invalid, original);
    fs::write(&path, &invalid).expect("写入损坏配置");
    wait_for_mode(&mut subscription, ConfigurationMode::ReadOnlyRecovery).await;
    let invalid_report = ConfigurationStore::validate_root_with_host_types(
        root.path(),
        ConfigurationValidationScope::Hosts,
        &types,
    )
    .expect("执行 validator");
    assert!(!invalid_report.valid);
    assert!(
        invalid_report
            .diagnostics
            .iter()
            .any(|diagnostic| { diagnostic.path.replace('\\', "/") == "hosts/fixture-host.toml" })
    );
    assert!(hosts.load_plugin_host(&id).await.is_err());
    assert_eq!(fs::read_to_string(&path).expect("读取损坏原文"), invalid);

    let fixed = original.replace("address = \"one\"", "address = \"two\"");
    fs::write(&path, &fixed).expect("修复配置");
    wait_for_mode(&mut subscription, ConfigurationMode::ReadWrite).await;
    let current = hosts
        .load_plugin_host(&id)
        .await
        .expect("读取修复配置")
        .expect("主机存在");
    assert_eq!(
        current
            .configuration
            .as_ref()
            .and_then(|value| value.get("address"))
            .and_then(toml::Value::as_str),
        Some("two")
    );
    let fixed_report = ConfigurationStore::validate_root_with_host_types(
        root.path(),
        ConfigurationValidationScope::Hosts,
        &types,
    )
    .expect("再次校验");
    assert!(fixed_report.valid, "{fixed_report:?}");
    watcher
        .shutdown_until(Instant::now() + Duration::from_secs(2))
        .await
        .expect("关闭 watcher");
}

/// 未安装提供方的原文仍参与分组事务，且不能被错误解码或打开。
#[tokio::test]
async fn missing_provider_preserves_configuration_through_group_deletion() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let known =
        ConfigurationStore::initialize_with_host_types(root.path(), catalog(None)).expect("初始化");
    let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(known)));
    let source = hosts
        .save_group(
            HostGroupDraft {
                id: Some("source".to_owned()),
                name: "Source".to_owned(),
                sort_order: 0,
            },
            None,
        )
        .await
        .expect("源分组");
    let groups = hosts
        .save_group(
            HostGroupDraft {
                id: Some("target".to_owned()),
                name: "Target".to_owned(),
                sort_order: 1,
            },
            source.revision,
        )
        .await
        .expect("目标分组");
    let id = HostId::parse("unknown-host").expect("合法 ID");
    hosts
        .save_plugin_host(draft(id.clone(), Some("source"), "keep"), None)
        .await
        .expect("保存主机");
    let path = root.path().join("hosts/unknown-host.toml");
    let original = fs::read_to_string(&path).expect("读取原文").replace(
        "address = \"keep\"",
        "address = \"keep\" # keep plugin comment",
    );
    fs::write(&path, &original).expect("添加插件注释");
    let unknown_types = host_configuration_catalog(Vec::new()).expect("无插件目录");
    let unknown_store =
        ConfigurationStore::initialize_with_host_types(root.path(), unknown_types.clone())
            .expect("保留未知插件");
    let unknown_hosts =
        HostManagementService::new(Arc::new(HostConfigurationRepository::new(unknown_store)));
    let unknown = unknown_hosts
        .load_plugin_host(&id)
        .await
        .expect("读取未知项")
        .expect("主机仍存在");
    assert!(!unknown.is_available());
    assert!(!unknown.is_runnable());
    let runtime = PluginHostService::new(unknown_hosts.clone(), unknown_types);
    assert_eq!(
        runtime
            .open(&id, CancellationToken::new())
            .await
            .expect_err("拒绝打开")
            .public_code(),
        PublicErrorCode::Unsupported
    );
    unknown_hosts
        .delete_group(
            "source".to_owned(),
            Some("target".to_owned()),
            groups.revision.expect("分组 revision"),
        )
        .await
        .expect("事务迁移分组");
    let moved = fs::read_to_string(&path).expect("读取移动后的原文");
    assert!(moved.contains("address = \"keep\" # keep plugin comment"));
    assert!(moved.contains("group_id = \"target\""));
    assert_eq!(
        unknown_hosts
            .load_plugin_host(&id)
            .await
            .expect("再次读取")
            .expect("主机仍存在")
            .group_id
            .as_deref(),
        Some("target")
    );
}

/// 成功结果必须属于当前提供方并有正版本，身份或版本不匹配不能进入工作区。
#[tokio::test]
async fn provider_open_checks_content_identity_and_version() {
    for (content_provider, content_version, accepted) in [
        (None, None, true),
        (Some("other.plugin"), None, false),
        (None, Some(0), false),
    ] {
        let root = tempfile::tempdir().expect("隔离配置根");
        let provider = Arc::new(FixtureProvider {
            content_provider,
            content_version,
            ..Default::default()
        });
        let types = catalog(Some(provider.clone()));
        let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
            .expect("初始化配置");
        let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
        let id = HostId::parse("runtime-host").expect("合法主机 ID");
        hosts
            .save_plugin_host(draft(id.clone(), None, "one"), None)
            .await
            .expect("保存主机");
        let result = PluginHostService::new(hosts, types)
            .open(&id, CancellationToken::new())
            .await;
        if accepted {
            assert!(matches!(
                result,
                Ok(PluginHostOpenIntent::Content { provider_id, state_version: 1, .. })
                    if provider_id == "example.plugin"
            ));
        } else {
            assert_eq!(
                result.expect_err("拒绝非法内容").public_code(),
                PublicErrorCode::Invalid
            );
        }
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }
}

/// 探测期间修改主机后，提供方的成功或失败都不能作为当前配置的回执。
#[tokio::test]
async fn provider_test_rejects_result_after_configuration_changes() {
    for fail_test in [false, true] {
        let root = tempfile::tempdir().expect("隔离配置根");
        let provider = Arc::new(FixtureProvider {
            wait_for_test: true,
            fail_test,
            ..Default::default()
        });
        let types = catalog(Some(provider.clone()));
        let store = ConfigurationStore::initialize_with_host_types(root.path(), types.clone())
            .expect("初始化配置");
        let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
        let id = HostId::parse("runtime-host").expect("合法主机 ID");
        let saved = hosts
            .save_plugin_host(draft(id.clone(), None, "one"), None)
            .await
            .expect("保存主机");
        let runtime = PluginHostService::new(hosts.clone(), types);
        let test_id = id.clone();
        let task =
            tokio::spawn(async move { runtime.test(&test_id, CancellationToken::new()).await });
        tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
            .await
            .expect("提供方开始探测");
        hosts
            .save_plugin_host(draft(id, None, "two"), Some(saved.revision))
            .await
            .expect("修改主机");
        provider.release.notify_one();
        assert_eq!(
            task.await
                .expect("探测任务完成")
                .expect_err("拒绝旧结果")
                .public_code(),
            PublicErrorCode::Conflict
        );
    }
}

/// 运行前取消不调用提供方，运行中取消传入提供方，配置变更使迟到意图失效。
#[tokio::test]
async fn provider_receives_cancellation_and_stale_open_is_rejected() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let provider = Arc::new(FixtureProvider {
        wait_for_release: true,
        ..Default::default()
    });
    let types = catalog(Some(provider.clone()));
    let store =
        ConfigurationStore::initialize_with_host_types(root.path(), types.clone()).expect("初始化");
    let hosts = HostManagementService::new(Arc::new(HostConfigurationRepository::new(store)));
    let id = HostId::parse("runtime-host").expect("合法 ID");
    let saved = hosts
        .save_plugin_host(draft(id.clone(), None, "one"), None)
        .await
        .expect("保存");
    let runtime = PluginHostService::new(hosts.clone(), types);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert_eq!(
        runtime
            .test(&id, cancelled)
            .await
            .expect_err("已取消")
            .public_code(),
        PublicErrorCode::Cancelled
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    runtime
        .test(&id, CancellationToken::new())
        .await
        .expect("测试提供方");

    let stale_runtime = runtime.clone();
    let stale_id = id.clone();
    let stale = tokio::spawn(async move {
        stale_runtime
            .open(&stale_id, CancellationToken::new())
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
        .await
        .expect("提供方开始");
    hosts
        .save_plugin_host(draft(id.clone(), None, "two"), Some(saved.revision))
        .await
        .expect("修改原主机");
    provider.release.notify_one();
    assert_eq!(
        stale
            .await
            .expect("任务完成")
            .expect_err("拒绝过期目标")
            .public_code(),
        PublicErrorCode::Conflict
    );

    let cancel = CancellationToken::new();
    let open_cancel = cancel.clone();
    let cancelled_runtime = runtime.clone();
    let cancelled_id = id.clone();
    let task =
        tokio::spawn(async move { cancelled_runtime.open(&cancelled_id, open_cancel).await });
    tokio::time::timeout(Duration::from_secs(5), provider.entered.notified())
        .await
        .expect("再次进入提供方");
    cancel.cancel();
    assert_eq!(
        task.await
            .expect("任务完成")
            .expect_err("取消生效")
            .public_code(),
        PublicErrorCode::Cancelled
    );
}

/// 旧格式规划必须使用当前提供方 codec；缺失提供方保留原文，已知损坏项阻断迁移。
#[test]
fn legacy_migration_uses_the_supplied_codec_catalog() {
    let root = tempfile::tempdir().expect("隔离配置根");
    let hosts = root.path().join("hosts");
    fs::create_dir_all(&hosts).expect("创建主机目录");
    let path = hosts.join("migration-host.toml");
    let raw = "schema_version = 3\nid = \"migration-host\"\nprotocol = \"example-host\"\nprovider_id = \"example.plugin\"\nname = \"Migration\"\nsort_order = 0\nconfiguration_version = 1\ncreated_at = \"2026-01-01T00:00:00Z\"\nupdated_at = \"2026-01-01T00:00:00Z\"\n\n[configuration]\naddress = \"\" # preserve unknown\n";
    fs::write(&path, raw).expect("写入插件配置");
    let known = FileDataMigrationRepository::new(root.path()).with_host_types(catalog(None));
    assert_eq!(
        known.inspect().expect("预检已知插件").readiness,
        DataMigrationReadiness::Blocked
    );
    let unknown = FileDataMigrationRepository::new(root.path())
        .with_host_types(host_configuration_catalog(Vec::new()).expect("无插件目录"));
    assert_eq!(
        unknown.inspect().expect("预检未知插件").readiness,
        DataMigrationReadiness::Legacy
    );
    assert_eq!(fs::read_to_string(&path).expect("原文未被改写"), raw);
}
