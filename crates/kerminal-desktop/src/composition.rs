//! 桌面应用的唯一 composition root；在 GPUI 窗口启动前完成共享服务装配。
//!
//! @author kongweiguang

use std::{error::Error, fmt, future::Future, path::Path, sync::Arc, time::Duration};

use kerminal_application::{
    AppError, AppResult, SessionService,
    agents::{AgentService, AgentSessionService, AgentWorkspaceRepository},
    commands::{
        CommandHistoryRepository, CommandSuggestionFeedbackRepository, CommandSuggestionProvider,
        CommandsService, CompositeCommandSuggestionProvider, HistoryCommandSuggestionProvider,
        SnippetPreferenceRepository, SnippetRepository, SnippetTransferFileService,
        WorkflowRepository,
    },
    configuration_changes::{ConfigurationChangeMessage, ConfigurationChangeService},
    configuration_directory::ConfigurationDirectoryService,
    configuration_sync::{ConfigurationSyncRepository, ConfigurationSyncService},
    connections::{
        CredentialResolver, HostConnectionRepository, KnownHostsRepository, RdpLaunchService,
        RdpPlatformLauncher, RemoteConnectionService, SerialPortRepository, SerialPortService,
        credentials::{
            CredentialService, CredentialVaultRepository, VaultKeyManagementService,
            VaultKeyRepository,
        },
        management::{HostManagementRepository, HostManagementService},
        probe::{ConnectionProbeBackend, ConnectionProbeService},
    },
    executor::ApplicationTaskExecutor,
    files::{
        FileAccessProvider, FileBookmarkRepository, FileService,
        clipboard_download::{FileClipboardDownloadService, FileClipboardPort},
    },
    mcp_runtime::{McpRuntimeControl, McpRuntimeSnapshot},
    operations::{
        OperationsCommandExecutor, OperationsService, PortForwardRuntime, ServerInfoCollector,
    },
    platform::external_launch::{
        ExternalLaunchService, ExternalTemporaryTargetRegistry, SavedExternalTargetLookup,
    },
    platform::{DesktopNotificationService, ExternalProtocolService, NotificationPort},
    preferences::{
        ConfigurationRepository, ConfigurationValidationService, PreferencesService,
        ProfileRepository, ProfileService,
        settings::{DesktopNotificationSettings, ExternalLaunchSettings, SftpPerformanceSettings},
    },
    sessions::{
        SessionTransportFactory,
        logging::{SessionLogRepository, SessionLogService, SessionOutputRecorder},
    },
    transfers::{SftpTransferPolicyStore, TransferBackend, TransferQueueOptions, TransferService},
    updates::UpdateService,
    workspace::{WorkspaceService, WorkspaceSessionRepository, WorkspaceSessionService},
};
use kerminal_domain::{files::FileEndpoint, preferences::ConfigurationMode};
use kerminal_infrastructure::{
    agents::FileAgentWorkspaceRepository,
    command_history::SqliteCommandHistoryRepository,
    commands::{
        FileCommandCatalogRepository, RemoteCommandSuggestionProvider, RemoteSuggestionPolicy,
        StaticSpecSuggestionProvider, SystemSnippetTransferFileAdapter,
    },
    configuration::ConfigurationStore,
    configuration_changes::ConfigurationChangeWatcher,
    configuration_sync::GitConfigurationSyncRepository,
    connections::{
        ExternalRuntimeRegistry, KnownHostsStore, RusshRemoteConnectionDependencies,
        RusshRemoteConnectionService, SshSessionTransportFactory, SystemRdpPlatformLauncher,
        management::HostConfigurationRepository,
        probe::SystemConnectionProbe,
        serial::{SerialRuntimeConfig, SerialSessionTransportFactory, TokioSerialPortRepository},
        telnet::{TelnetRuntimeConfig, TelnetSessionTransportFactory},
    },
    files::{
        JsonFileBookmarkRepository, JsonTransferRecoveryRepository, LocalFileProvider,
        SftpFileProvider, SftpTransferBackend,
    },
    local_pty::LocalPtySessionTransportFactory,
    operations::{
        ContainerFileProvider, ContainerSessionTransportFactory, ContainerTransferBackend,
        PortForwardDefinitionStore, SshPortForwardRuntime, StructuredOperationsCommandExecutor,
        SystemServerInfoCollector,
    },
    platform::updater::{CargoPackagerUpdater, UpdaterSettings},
    platform::{
        configuration_directory::SystemConfigurationDirectoryOpener,
        default_shell::SystemDefaultLocalShellProvider,
        external_launch::SystemExternalProtocolAdapter, file_clipboard::SystemFileClipboard,
        local_shells::SystemLocalShellDiscovery, notification::NotifyRustAdapter,
    },
    session_logs::SessionLogStore,
    sessions::{
        CompositeSessionTransportFactory, SessionManager, SessionRuntimeConfig, SessionTargetKind,
    },
    vault::VaultStore,
    workspace::WorkspaceSessionStore,
};
use kerminal_mcp::{McpRuntimeController, McpServerConfig, McpServices};
use kerminal_ui::AppServices;
use tokio::{
    runtime::Handle,
    task::JoinHandle,
    time::{Instant, timeout_at},
};
use tokio_util::sync::CancellationToken;

#[path = "notification_bridge.rs"]
mod notification_bridge;
use notification_bridge::NotificationBridge;

const STARTUP_CLEANUP_BUDGET: Duration = Duration::from_secs(12);

/// PowerShell 单引号字面量以重复单引号转义，路径中的空格和元字符不会改变命令结构。
///
/// @author kongweiguang
fn powershell_literal_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// 组合根无法解析当前可执行文件时隐藏 validator 复制入口，不生成可能运行错误程序的命令。
///
/// @author kongweiguang
fn configuration_validator_command(executable: &Path, configuration_root: &Path) -> Option<String> {
    let executable = executable.to_str()?;
    let configuration_root = configuration_root.to_str()?;
    Some(format!(
        "& {} --validate-config {}",
        powershell_literal_argument(executable),
        powershell_literal_argument(configuration_root),
    ))
}

/// 桌面唯一服务组合根；同一配置、会话、Vault、连接和 Tokio 执行器在各入口间共享。
pub struct DesktopComposition {
    runtime: Handle,
    app_services: AppServices,
    configuration: ConfigurationStore,
    storage: kerminal_infrastructure::storage::StorageCoordinator,
    preferences: PreferencesService,
    notifications: Arc<DesktopNotificationService>,
    notification_bridge: Option<NotificationBridge>,
    configuration_changes: ConfigurationChangeService,
    watcher: Option<ConfigurationChangeWatcher>,
    settings_refresh_cancellation: CancellationToken,
    settings_refresh_task: Option<JoinHandle<AppResult<()>>>,
    task_executor: ApplicationTaskExecutor,
    sessions: Arc<SessionManager>,
    connection_probe_backend: Arc<SystemConnectionProbe>,
    operations: Arc<OperationsService>,
    transfers: TransferService,
    session_logs: SessionLogStore,
    rdp_platform: Arc<SystemRdpPlatformLauncher>,
    agent_manager: Arc<AgentSessionService>,
    commands: Arc<CommandsService>,
    // 保留同一 vault owner，避免组合根销毁其 credential/key 管理实例。
    _vault: Arc<VaultStore>,
    git_sync_repository: Arc<GitConfigurationSyncRepository>,
    // 临时外部连接注册表与外部启动 service 使用同一共享实例。
    _external_runtime_registry: Arc<ExternalRuntimeRegistry>,
    external_launch: ExternalLaunchService,
    remote_command_suggestions: Arc<RemoteCommandSuggestionProvider>,
    remote_suggestion_policy: RemoteSuggestionPolicy,
    sftp_transfer_policy: SftpTransferPolicyStore,
    update_service: UpdateService,
    mcp_runtime: Arc<dyn McpRuntimeControl>,
}

impl DesktopComposition {
    /// 使用宿主创建的唯一 runtime 和显式数据根装配生产服务，避免触碰进程默认用户目录。
    pub async fn build(runtime: Handle, config_root: impl AsRef<Path>) -> AppResult<Self> {
        Self::build_with_update_service(runtime, config_root, None).await
    }

    /// 允许注入 updater；主机默认值、SFTP 开关及持久恢复仓储复用同一配置根与服务实例。
    /// 命令历史、建议反馈和片段偏好共享 SQLite；连接测试复用正式认证与 transport 而不建会话。
    /// 恢复仓储与配置、主机信任共用 coordinator，避免同进程读取误争跨进程根锁。
    ///
    /// @author kongweiguang
    pub async fn build_with_update_service(
        runtime: Handle,
        config_root: impl AsRef<Path>,
        update_service: Option<UpdateService>,
    ) -> AppResult<Self> {
        let config_root = config_root.as_ref().to_path_buf();
        let configuration = ConfigurationStore::initialize(&config_root).map_err(|source| {
            AppError::infrastructure_with_source("初始化 Kerminal 配置根失败", source)
        })?;
        let update_service = match update_service {
            Some(service) => service,
            None => UpdateService::new(Arc::new(CargoPackagerUpdater::new(
                None::<UpdaterSettings>,
                config_root.join("updates").join("recovery"),
            )?)),
        };
        let storage = configuration.storage_coordinator();
        let writable = configuration.recovery_state().mode == ConfigurationMode::ReadWrite;

        let configuration_repository: Arc<dyn ConfigurationRepository> =
            Arc::new(configuration.clone());
        let preferences = PreferencesService::new(configuration_repository);
        let profile_repository: Arc<dyn ProfileRepository> = Arc::new(configuration.clone());
        let profiles = ProfileService::new(
            profile_repository.clone(),
            Arc::new(SystemDefaultLocalShellProvider::new()),
        )
        .with_shell_discovery(Arc::new(SystemLocalShellDiscovery::new()));
        let configuration_changes =
            ConfigurationChangeService::new(Arc::new(configuration.clone()));

        let vault = Arc::new(VaultStore::open(storage.clone()));
        let vault_credential_resolver: Arc<dyn CredentialResolver> = vault.clone();
        let saved_host_repository =
            Arc::new(HostConfigurationRepository::new(configuration.clone()));
        let saved_hosts: Arc<dyn HostConnectionRepository> = saved_host_repository.clone();
        let external_runtime_registry = Arc::new(ExternalRuntimeRegistry::new(
            saved_hosts.clone(),
            vault_credential_resolver.clone(),
        ));
        let runtime_hosts: Arc<dyn HostConnectionRepository> = external_runtime_registry.clone();
        let runtime_credentials: Arc<dyn CredentialResolver> = external_runtime_registry.clone();
        let known_hosts: Arc<dyn KnownHostsRepository> =
            Arc::new(KnownHostsStore::new(storage.clone(), writable));
        let remote_connections: Arc<dyn RemoteConnectionService> = Arc::new(
            RusshRemoteConnectionService::new(RusshRemoteConnectionDependencies {
                hosts: runtime_hosts,
                credentials: runtime_credentials,
                known_hosts: known_hosts.clone(),
            }),
        );

        let credential_vault: Arc<dyn CredentialVaultRepository> = vault.clone();
        let credential_service = CredentialService::new(credential_vault);
        let vault_key_repository: Arc<dyn VaultKeyRepository> = vault.clone();
        let vault_keys = VaultKeyManagementService::new(vault_key_repository);
        let host_management_repository: Arc<dyn HostManagementRepository> =
            saved_host_repository.clone();
        let host_management = HostManagementService::with_credentials(
            host_management_repository,
            credential_service.clone(),
        );

        let saved_target_lookup: Arc<dyn SavedExternalTargetLookup> = saved_host_repository.clone();
        let temporary_target_registry: Arc<dyn ExternalTemporaryTargetRegistry> =
            external_runtime_registry.clone();
        let (
            external_launch_settings,
            notification_settings,
            remote_suggestion_policy,
            sftp_settings,
            auto_reconnect_enabled,
        ) = initial_runtime_settings(&preferences, writable).await?;
        let sftp_transfer_policy = SftpTransferPolicyStore::new(sftp_settings)?;
        let external_launch = ExternalLaunchService::new(
            saved_target_lookup,
            temporary_target_registry,
            external_launch_settings,
        );
        let notification_port: Arc<dyn NotificationPort> = Arc::new(NotifyRustAdapter);
        let notifications = Arc::new(DesktopNotificationService::new(
            notification_port,
            notification_settings,
        ));

        let rdp_cache_root = dirs::cache_dir().ok_or_else(|| {
            AppError::unsupported("无法确定用户级缓存目录，RDP 临时文件服务不可用")
        })?;
        let rdp_platform = Arc::new(SystemRdpPlatformLauncher::new(
            rdp_cache_root.join("Kerminal").join("runtime").join("rdp"),
        )?);
        let rdp_platform_port: Arc<dyn RdpPlatformLauncher> = rdp_platform.clone();
        let rdp_launch = Arc::new(
            RdpLaunchService::new(
                saved_hosts.clone(),
                vault_credential_resolver.clone(),
                rdp_platform_port,
            )
            .with_host_management(host_management.clone()),
        );

        let session_logs = SessionLogStore::open(storage.clone())?;
        let session_recorder: Arc<dyn SessionOutputRecorder> = Arc::new(session_logs.clone());
        let serial_ports = create_serial_port_service();
        let local_transport: Arc<dyn SessionTransportFactory> =
            Arc::new(LocalPtySessionTransportFactory::new());
        let ssh_transport: Arc<dyn SessionTransportFactory> = Arc::new(
            SshSessionTransportFactory::new(remote_connections.clone())
                .with_host_management(host_management.clone()),
        );
        let telnet_transport: Arc<dyn SessionTransportFactory> = Arc::new(
            TelnetSessionTransportFactory::new(TelnetRuntimeConfig::default())?,
        );
        let serial_transport: Arc<dyn SessionTransportFactory> = Arc::new(
            SerialSessionTransportFactory::new(SerialRuntimeConfig::default())?,
        );
        let container_transport: Arc<dyn SessionTransportFactory> = Arc::new(
            ContainerSessionTransportFactory::new(remote_connections.clone()),
        );
        let session_factory = Arc::new(
            CompositeSessionTransportFactory::new(local_transport)
                .with_provider(SessionTargetKind::Ssh, ssh_transport)
                .with_provider(SessionTargetKind::Telnet, telnet_transport)
                .with_provider(SessionTargetKind::Serial, serial_transport)
                .with_provider(SessionTargetKind::Container, container_transport),
        );
        let probe_transport_factory: Arc<dyn SessionTransportFactory> = session_factory.clone();
        let connection_probe_backend = Arc::new(SystemConnectionProbe::new(
            vault_credential_resolver,
            known_hosts,
            probe_transport_factory,
        ));
        let connection_probe_port: Arc<dyn ConnectionProbeBackend> =
            connection_probe_backend.clone();
        let connection_probe_service = Arc::new(ConnectionProbeService::new(connection_probe_port));
        let sessions = Arc::new(
            SessionManager::new(
                session_factory,
                runtime.clone(),
                SessionRuntimeConfig::default(),
            )
            .with_output_recorder(session_recorder),
        );
        let session_service: Arc<dyn SessionService> = sessions.clone();
        session_service
            .set_auto_reconnect_enabled(auto_reconnect_enabled)
            .await?;

        let command_history = Arc::new(SqliteCommandHistoryRepository::open_or_unavailable(
            storage.clone(),
        ));
        let history_repository: Arc<dyn CommandHistoryRepository> = command_history.clone();
        let suggestion_feedback_repository: Arc<dyn CommandSuggestionFeedbackRepository> =
            command_history.clone();
        let snippet_preference_repository: Arc<dyn SnippetPreferenceRepository> =
            command_history.clone();
        let command_catalog =
            Arc::new(FileCommandCatalogRepository::new(storage.clone(), writable));
        let snippet_repository: Arc<dyn SnippetRepository> = command_catalog.clone();
        let workflow_repository: Arc<dyn WorkflowRepository> = command_catalog;
        let operations_command_executor: Arc<dyn OperationsCommandExecutor> = Arc::new(
            StructuredOperationsCommandExecutor::new(remote_connections.clone()),
        );
        let server_info_collector: Arc<dyn ServerInfoCollector> =
            Arc::new(SystemServerInfoCollector::new(
                operations_command_executor.clone(),
                remote_connections.clone(),
            ));
        let port_forward_runtime: Arc<dyn PortForwardRuntime> =
            Arc::new(SshPortForwardRuntime::new(remote_connections.clone()));
        let bookmark_repository: Arc<dyn FileBookmarkRepository> =
            Arc::new(JsonFileBookmarkRepository::new(config_root.clone()));
        let local_files: Arc<dyn FileAccessProvider> = Arc::new(LocalFileProvider);
        let sftp_files: Arc<dyn FileAccessProvider> = Arc::new(
            SftpFileProvider::new(remote_connections.clone())
                .with_host_management(host_management.clone()),
        );
        let container_files: Arc<dyn FileAccessProvider> = Arc::new(ContainerFileProvider::new(
            operations_command_executor.clone(),
        ));
        let files = FileService::new(
            vec![local_files, sftp_files, container_files],
            bookmark_repository,
        )?;
        let remote_command_suggestions = Arc::new(RemoteCommandSuggestionProvider::new(
            files.clone(),
            remote_connections.clone(),
            remote_suggestion_policy.clone(),
        ));
        let history_suggestions: Arc<dyn CommandSuggestionProvider> =
            Arc::new(HistoryCommandSuggestionProvider::new(
                history_repository.clone(),
                snippet_repository.clone(),
            ));
        let spec_suggestions: Arc<dyn CommandSuggestionProvider> =
            Arc::new(StaticSpecSuggestionProvider::new()?);
        let remote_suggestions: Arc<dyn CommandSuggestionProvider> =
            remote_command_suggestions.clone();
        let suggestions: Arc<dyn CommandSuggestionProvider> =
            Arc::new(CompositeCommandSuggestionProvider::new(vec![
                history_suggestions,
                spec_suggestions,
                remote_suggestions,
            ])?);
        let commands = Arc::new(
            CommandsService::new(
                history_repository,
                snippet_repository,
                workflow_repository,
                suggestions,
                session_service.clone(),
            )
            .with_preferences(preferences.clone())
            .with_configuration_changes(configuration_changes.clone())
            .with_suggestion_feedback_repository(suggestion_feedback_repository)
            .with_snippet_preference_repository(snippet_preference_repository),
        );
        let operations = Arc::new(
            OperationsService::new(
                operations_command_executor.clone(),
                session_service.clone(),
                remote_connections.clone(),
                profile_repository,
                server_info_collector,
                port_forward_runtime,
            )
            .with_port_forward_definition_repository(Arc::new(PortForwardDefinitionStore::new(
                storage.clone(),
            )))
            .with_terminal_proxy(commands.clone()),
        );
        let transfer_backends: Vec<Arc<dyn TransferBackend>> = vec![
            Arc::new(SftpTransferBackend::new(remote_connections.clone())),
            Arc::new(ContainerTransferBackend::new(
                operations_command_executor.clone(),
            )),
        ];
        let transfers = TransferService::new_with_recovery_repository(
            transfer_backends,
            runtime.clone(),
            TransferQueueOptions::default(),
            sftp_transfer_policy.clone(),
            Arc::new(JsonTransferRecoveryRepository::with_storage_coordinator(
                storage.clone(),
            )),
            Some(Arc::new(host_management.clone())),
        )?;
        let file_clipboard_port: Arc<dyn FileClipboardPort> = Arc::new(SystemFileClipboard::new()?);
        let file_clipboard_download = FileClipboardDownloadService::new(
            files.clone(),
            transfers.clone(),
            file_clipboard_port,
        );

        let workspace_storage = WorkspaceSessionStore::new(storage.clone(), writable);
        let workspace_repository: Arc<dyn WorkspaceSessionRepository> = Arc::new(workspace_storage);
        let workspace_sessions = WorkspaceSessionService::new(workspace_repository);
        let configuration_validation =
            ConfigurationValidationService::new(Arc::new(configuration.clone()));
        let configuration_directory = ConfigurationDirectoryService::new(Arc::new(
            SystemConfigurationDirectoryOpener::new(config_root.clone()),
        ));
        let snippet_transfer_files =
            SnippetTransferFileService::new(Arc::new(SystemSnippetTransferFileAdapter));
        let git_sync_repository = Arc::new(GitConfigurationSyncRepository::new(
            configuration.clone(),
            vault.as_ref().clone(),
            operations_command_executor.clone(),
            runtime.clone(),
        )?);
        let sync_repository: Arc<dyn ConfigurationSyncRepository> = git_sync_repository.clone();
        let configuration_sync = ConfigurationSyncService::new(sync_repository);

        let session_log_repository: Arc<dyn SessionLogRepository> = Arc::new(session_logs.clone());
        let session_log_service = SessionLogService::new(session_log_repository);
        let agent_workspace_repository: Arc<dyn AgentWorkspaceRepository> =
            Arc::new(FileAgentWorkspaceRepository::with_storage_coordinator(
                config_root.clone(),
                storage.clone(),
            ));
        let agent_manager = Arc::new(
            AgentSessionService::new(
                session_service.clone(),
                agent_workspace_repository,
                Arc::new(preferences.clone()),
            )
            .with_profile_service(Arc::new(profiles.clone()))
            .with_launcher_installation_probe(Arc::new(
                kerminal_infrastructure::agents::PathAgentLauncherInstallationProbe,
            )),
        );
        let agent_service: Arc<dyn AgentService> = agent_manager.clone();

        let task_executor = ApplicationTaskExecutor::new(runtime.clone());
        let mcp_services = McpServices::new(session_service.clone())
            .with_task_executor(task_executor.clone())
            .with_agents(agent_service.clone())
            .with_connections(
                remote_connections.clone(),
                host_management.clone(),
                credential_service.clone(),
            )
            .with_transfers(transfers.clone())
            .with_operations(operations.as_ref().clone())
            .with_commands(commands.as_ref().clone())
            .with_profiles(profiles.clone())
            .with_configuration_validation(configuration_validation.clone());
        let mcp_runtime: Arc<dyn McpRuntimeControl> = Arc::new(McpRuntimeController::new(
            runtime.clone(),
            mcp_services,
            McpServerConfig::default(),
            notifications.as_ref().clone(),
        ));
        agent_manager.set_mcp_runtime(Arc::downgrade(&mcp_runtime))?;
        let external_protocol =
            ExternalProtocolService::new(Arc::new(SystemExternalProtocolAdapter));

        let initial_file_path = dirs::home_dir()
            .or_else(|| std::env::current_dir().ok())
            .ok_or_else(|| AppError::unsupported("无法确定文件浏览器的本机起始目录"))?;
        let mut app_services = AppServices::new(
            WorkspaceService::new(),
            session_service.clone(),
            task_executor.clone(),
            preferences.clone(),
            profiles.clone(),
        )
        .with_vault_keys(vault_keys)
        .with_screen_color_sampler(Arc::new(
            kerminal_infrastructure::platform::color_sampler::PlatformScreenColorSampler,
        ))
        .with_workspace_sessions(workspace_sessions)
        .with_file_clipboard_download(file_clipboard_download)
        .with_files(
            files.clone(),
            transfers.clone(),
            FileEndpoint::Local {
                path: initial_file_path,
            },
        )
        .with_commands(commands.clone())
        .with_commands_configuration_actions(
            Some(config_root.join("snippets").to_string_lossy().into_owned()),
            std::env::current_exe()
                .ok()
                .and_then(|executable| configuration_validator_command(&executable, &config_root)),
        )
        .with_configuration_directory(configuration_directory)
        .with_snippet_transfer_files(snippet_transfer_files)
        .with_configuration_validation(configuration_validation.clone())
        .with_connections(
            host_management.clone(),
            remote_connections.clone(),
            Some(rdp_launch),
        )
        .with_connection_probe_service(connection_probe_service)
        .with_serial_ports(serial_ports)
        .with_notifications(notifications.clone())
        .with_external_protocol(external_protocol)
        .with_mcp_runtime(mcp_runtime.clone())
        .with_external_launch(external_launch.clone())
        .with_update_service(Some(update_service.clone()))
        .with_rdp_launch_status(rdp_platform.status_service())
        .with_operations(operations.clone())
        .with_agents(agent_service.clone())
        .with_session_logs(session_log_service.clone())
        .with_configuration_sync(configuration_sync.clone())
        .with_configuration_changes(configuration_changes.clone());
        if let Some(locale) = sys_locale::get_locale() {
            app_services = app_services.with_system_locale(locale);
        }

        let notification_bridge = NotificationBridge::start(
            runtime.clone(),
            transfers.clone(),
            host_management.clone(),
            sessions.clone(),
            agent_service.clone(),
            notifications.clone(),
        )
        .await;
        let mut composition = Self {
            runtime,
            app_services,
            configuration,
            storage,
            preferences,
            notifications,
            notification_bridge: Some(notification_bridge),
            configuration_changes,
            watcher: None,
            settings_refresh_cancellation: CancellationToken::new(),
            settings_refresh_task: None,
            task_executor,
            sessions,
            connection_probe_backend,
            operations,
            transfers,
            session_logs,
            rdp_platform,
            agent_manager,
            commands,
            _vault: vault,
            git_sync_repository,
            _external_runtime_registry: external_runtime_registry,
            external_launch,
            remote_command_suggestions,
            remote_suggestion_policy,
            sftp_transfer_policy,
            update_service,
            mcp_runtime,
        };

        // 先注册广播接收器，再启动操作系统 watcher，避免第一次文件变更落入订阅空窗。
        composition.start_settings_refresh_worker();
        match ConfigurationChangeWatcher::start(composition.configuration.clone()) {
            Ok(watcher) => composition.watcher = Some(watcher),
            Err(error) => return Err(composition.cleanup_startup_failure(error).await),
        }
        composition.prune_old_suggestion_data().await;
        Ok(composition)
    }

    /// 返回克隆的界面依赖句柄；所有克隆仍指向同一组 application services。
    pub fn app_services(&self) -> AppServices {
        self.app_services.clone()
    }

    /// 返回与所有持久化适配器共用的配置根协调器，供桌面侧安全装配后续能力。
    pub fn storage_coordinator(&self) -> kerminal_infrastructure::storage::StorageCoordinator {
        self.storage.clone()
    }

    /// 返回同一更新服务；即使当前构建无 feed，也保留恢复记录读取与确认能力。
    pub fn update_service(&self) -> Option<&UpdateService> {
        Some(&self.update_service)
    }

    /// 返回共享 MCP runtime 的已发布快照；桌面启动时默认保持 Stopped。
    pub fn mcp_runtime_snapshot(&self) -> McpRuntimeSnapshot {
        self.mcp_runtime.snapshot()
    }

    /// 先停 MCP、连接探测 worker 与 Agent endpoint，再用同一 deadline 收尾并保留失败 owner。
    ///
    /// @author kongweiguang
    pub async fn shutdown_until(&mut self, deadline: Instant) -> AppResult<()> {
        self.settings_refresh_cancellation.cancel();
        self.mcp_runtime.begin_shutdown();
        if let Some(notification_bridge) = self.notification_bridge.as_ref() {
            notification_bridge.begin_shutdown();
        }
        let mut failures = Vec::new();
        if let Err(error) = self.task_executor.begin_shutdown() {
            failures.push(error);
        }
        self.operations.begin_shutdown(deadline);

        // Agent endpoint clear 使用 Agent operation gate，必须在 Agent intake 关闭前完成。
        let (mcp_result, connection_probe_result) = tokio::join!(
            shutdown_mcp_before_agent_shutdown(self.mcp_runtime.shutdown_until(deadline), || {
                self.agent_manager.begin_shutdown()
            }),
            self.connection_probe_backend.shutdown_until(deadline),
        );
        append_failure(&mut failures, connection_probe_result);
        if let Err(error) = mcp_result {
            failures.push(error);
            return Err(AppError::infrastructure_with_source(
                "MCP 运行时尚未完成退出，保留 Agent endpoint 清理能力以供重试",
                ShutdownFailures(failures),
            ));
        }

        let (
            agent_result,
            operations_result,
            notification_result,
            transfer_result,
            watcher_result,
            suggestion_result,
            git_sync_result,
            (task_executor_result, session_result, rdp_result),
        ) = tokio::join!(
            self.agent_manager.shutdown_until(deadline),
            self.operations.shutdown_until(deadline),
            stop_notification_bridge(&mut self.notification_bridge, deadline),
            self.transfers.shutdown_until(deadline),
            stop_configuration_watcher(
                &mut self.watcher,
                &mut self.settings_refresh_task,
                deadline,
            ),
            self.remote_command_suggestions.shutdown_until(deadline),
            self.git_sync_repository.shutdown_until(deadline),
            join_session_dependent_shutdowns(
                self.task_executor.shutdown_until(deadline),
                self.sessions.close_all_until(deadline),
                shutdown_rdp(&self.rdp_platform, deadline),
            ),
        );
        append_failure(&mut failures, agent_result);
        append_failure(&mut failures, operations_result);
        append_failure(&mut failures, notification_result);
        append_failure(&mut failures, transfer_result);
        append_failure(&mut failures, watcher_result);
        append_failure(&mut failures, suggestion_result);
        append_failure(&mut failures, git_sync_result);
        append_failure(&mut failures, task_executor_result);
        append_failure(&mut failures, session_result);
        append_failure(&mut failures, rdp_result);
        append_failure(
            &mut failures,
            self.session_logs.shutdown_until(deadline.into_std()).await,
        );

        // 只在所有外部目标消费者都已退出后清除暂存密码，失败时保留仍需重试的 owner。
        if failures.is_empty() {
            append_failure(
                &mut failures,
                self.external_launch
                    .clear_temporary_targets_on_shutdown()
                    .await,
            );
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(AppError::infrastructure_with_source(
                "桌面退出时有资源未能完成收尾",
                ShutdownFailures(failures),
            ))
        }
    }

    /// 仅在 MCP 与所有基础设施构造成功后订阅变更，避免启动失败留下脱离 owner 的任务。
    /// @author kongweiguang
    fn start_settings_refresh_worker(&mut self) {
        let changes = self.configuration_changes.clone();
        let preferences = self.preferences.clone();
        let sessions: Arc<dyn SessionService> = self.sessions.clone();
        let external_launch = self.external_launch.clone();
        let notifications = self.notifications.clone();
        let remote_suggestion_policy = self.remote_suggestion_policy.clone();
        let sftp_transfer_policy = self.sftp_transfer_policy.clone();
        let cancellation = self.settings_refresh_cancellation.clone();
        self.settings_refresh_task = Some(self.runtime.spawn(async move {
            refresh_runtime_settings_on_changes(
                changes,
                RuntimeSettingsRefreshContext {
                    preferences,
                    sessions,
                    external_launch,
                    notifications,
                    remote_suggestion_policy,
                    sftp_transfer_policy,
                },
                cancellation,
            )
            .await;
            Ok(())
        }));
    }

    /// 通过统一任务执行器清理过期建议反馈；数据库不可用只影响此清理，不阻止桌面启动。
    async fn prune_old_suggestion_data(&self) {
        let commands = self.commands.clone();
        if let Err(error) = self
            .task_executor
            .run(move |_| async move { commands.prune_suggestion_data().await.map(|_| ()) })
            .await
        {
            tracing::warn!(
                event = "COMMAND_SUGGESTION_RETENTION_CLEANUP_UNAVAILABLE",
                code = error.public_code().as_str()
            );
        }
    }

    /// 启动阶段出错时复用正式 shutdown 顺序，避免 MCP、transfer 或 watcher 留下后台 owner。
    async fn cleanup_startup_failure(&mut self, startup_error: AppError) -> AppError {
        let deadline = Instant::now() + STARTUP_CLEANUP_BUDGET;
        match self.shutdown_until(deadline).await {
            Ok(()) => startup_error,
            Err(cleanup_error) => AppError::infrastructure_with_source(
                "桌面启动失败，且部分后台资源未能完成清理",
                StartupCleanupFailure {
                    startup: startup_error,
                    cleanup: cleanup_error,
                },
            ),
        }
    }
}

/// 串口枚举复用唯一成熟平台 adapter，service 本身不创建 runtime 或线程池。
fn create_serial_port_service() -> Arc<SerialPortService> {
    let repository: Arc<dyn SerialPortRepository> = Arc::new(TokioSerialPortRepository);
    Arc::new(SerialPortService::new(repository))
}

/// 一次读取已验证的设置快照；远端建议策略只映射配置仓储是否可用。
///
/// 用户来源偏好按 application 请求筛选，恢复态仍由 application、composition 和 provider 关闭远端 I/O。
async fn initial_runtime_settings(
    preferences: &PreferencesService,
    writable: bool,
) -> AppResult<(
    ExternalLaunchSettings,
    DesktopNotificationSettings,
    RemoteSuggestionPolicy,
    SftpPerformanceSettings,
    bool,
)> {
    if !writable {
        return Ok((
            disabled_external_launch_settings(),
            disabled_notification_settings(),
            disabled_remote_suggestion_policy(),
            SftpPerformanceSettings::default(),
            false,
        ));
    }
    let settings = preferences.load_settings().await?.settings;
    let remote_suggestion_policy = RemoteSuggestionPolicy::new(true);
    let sftp_settings = settings.sftp;
    let auto_reconnect_enabled = settings.terminal.auto_reconnect;
    Ok((
        settings.external_launch,
        settings.desktop_notifications,
        remote_suggestion_policy,
        sftp_settings,
        auto_reconnect_enabled,
    ))
}

/// 使恢复态或损坏设置不能意外继续接收第三方启动参数。
fn disabled_external_launch_settings() -> ExternalLaunchSettings {
    ExternalLaunchSettings {
        enabled: false,
        accept_vendor_args: false,
        auto_open_sftp: false,
        disabled_tools: Vec::new(),
    }
}

/// 在只读恢复和解析失败时使用禁用状态，不让旧通知偏好继续触发系统弹窗。
fn disabled_notification_settings() -> DesktopNotificationSettings {
    DesktopNotificationSettings::default()
}

/// 配置恢复或校验失败时关闭远端补全，避免损坏配置继续触发 SSH/SFTP 探测。
fn disabled_remote_suggestion_policy() -> RemoteSuggestionPolicy {
    RemoteSuggestionPolicy::new(false)
}

/// 配置刷新 worker 共享同一份依赖快照，避免函数参数列表掩盖这些策略的协同关系。
/// @author kongweiguang
struct RuntimeSettingsRefreshContext {
    preferences: PreferencesService,
    sessions: Arc<dyn SessionService>,
    external_launch: ExternalLaunchService,
    notifications: Arc<DesktopNotificationService>,
    remote_suggestion_policy: RemoteSuggestionPolicy,
    sftp_transfer_policy: SftpTransferPolicyStore,
}

/// 在唯一配置订阅上刷新所有依赖 settings.toml 的桌面策略。
///
/// 每次有效变更只加载一份经过 PreferencesService 校验的快照，避免消费者对磁盘各自解析或短暂分歧。
/// @author kongweiguang
async fn refresh_runtime_settings_on_changes(
    changes: ConfigurationChangeService,
    context: RuntimeSettingsRefreshContext,
    cancellation: CancellationToken,
) {
    let RuntimeSettingsRefreshContext {
        preferences,
        sessions,
        external_launch,
        notifications,
        remote_suggestion_policy,
        sftp_transfer_policy,
    } = context;
    let mut subscription = changes.subscribe();
    let mut current_mode = changes.recovery_state().mode;
    loop {
        let message = subscription.recv(cancellation.child_token()).await;
        match message {
            ConfigurationChangeMessage::Changed(event) => {
                let mode_changed = event.recovery_state.mode != current_mode;
                current_mode = event.recovery_state.mode;
                let settings_changed = event.full_rescan_required
                    || event
                        .relative_paths
                        .iter()
                        .any(|path| path.replace('\\', "/") == "settings.toml");
                if current_mode != ConfigurationMode::ReadWrite {
                    update_auto_reconnect_safely(sessions.as_ref(), false).await;
                    update_external_launch_safely(
                        &external_launch,
                        disabled_external_launch_settings(),
                    );
                    update_notification_settings_safely(
                        &notifications,
                        disabled_notification_settings(),
                    );
                    update_remote_suggestion_policy_safely(
                        &remote_suggestion_policy,
                        disabled_remote_suggestion_policy(),
                    );
                } else if mode_changed || settings_changed {
                    reload_runtime_settings(
                        &preferences,
                        sessions.as_ref(),
                        &external_launch,
                        &notifications,
                        &remote_suggestion_policy,
                        &sftp_transfer_policy,
                    )
                    .await;
                }
            }
            ConfigurationChangeMessage::RescanRequired => {
                current_mode = changes.recovery_state().mode;
                if current_mode == ConfigurationMode::ReadWrite {
                    reload_runtime_settings(
                        &preferences,
                        sessions.as_ref(),
                        &external_launch,
                        &notifications,
                        &remote_suggestion_policy,
                        &sftp_transfer_policy,
                    )
                    .await;
                } else {
                    update_auto_reconnect_safely(sessions.as_ref(), false).await;
                    update_external_launch_safely(
                        &external_launch,
                        disabled_external_launch_settings(),
                    );
                    update_notification_settings_safely(
                        &notifications,
                        disabled_notification_settings(),
                    );
                    update_remote_suggestion_policy_safely(
                        &remote_suggestion_policy,
                        disabled_remote_suggestion_policy(),
                    );
                }
            }
            ConfigurationChangeMessage::Cancelled | ConfigurationChangeMessage::Closed => return,
        }
    }
}

/// 外部文件变化后只加载一次完整快照，错误时所有 settings 驱动的动态策略都失败关闭。
async fn reload_runtime_settings(
    preferences: &PreferencesService,
    sessions: &dyn SessionService,
    external_launch: &ExternalLaunchService,
    notifications: &DesktopNotificationService,
    remote_suggestion_policy: &RemoteSuggestionPolicy,
    sftp_transfer_policy: &SftpTransferPolicyStore,
) {
    match preferences.load_settings().await {
        Ok(snapshot) => {
            update_auto_reconnect_safely(sessions, snapshot.settings.terminal.auto_reconnect).await;
            update_external_launch_safely(external_launch, snapshot.settings.external_launch);
            update_notification_settings_safely(
                notifications,
                snapshot.settings.desktop_notifications,
            );
            update_remote_suggestion_policy_safely(
                remote_suggestion_policy,
                RemoteSuggestionPolicy::new(
                    preferences.recovery_state().mode == ConfigurationMode::ReadWrite,
                ),
            );
            update_sftp_transfer_policy_safely(sftp_transfer_policy, snapshot.settings.sftp);
        }
        Err(error) => {
            update_auto_reconnect_safely(sessions, false).await;
            tracing::warn!(
                event = "DESKTOP_SETTINGS_RELOAD_FAILED",
                code = error.public_code().as_str()
            );
            tracing::warn!(
                event = "SFTP_POLICY_REMAINS_LAST_VALID",
                code = error.public_code().as_str()
            );
            update_external_launch_safely(external_launch, disabled_external_launch_settings());
            update_notification_settings_safely(notifications, disabled_notification_settings());
            update_remote_suggestion_policy_safely(
                remote_suggestion_policy,
                disabled_remote_suggestion_policy(),
            );
        }
    }
}

/// 将完整有效的 SFTP 参数快照提交给队列共享 store，失败时保留可诊断原因。
fn update_sftp_transfer_policy_safely(
    policy: &SftpTransferPolicyStore,
    settings: SftpPerformanceSettings,
) {
    if let Err(error) = policy.update(settings) {
        tracing::error!(
            event = "SFTP_TRANSFER_POLICY_UPDATE_FAILED",
            code = error.public_code().as_str()
        );
    }
}

/// 配置不可用时异步关闭自动重连，并等待策略应用后再处理下一次配置变化。
async fn update_auto_reconnect_safely(sessions: &dyn SessionService, enabled: bool) {
    if let Err(error) = sessions.set_auto_reconnect_enabled(enabled).await {
        tracing::error!(
            event = "TERMINAL_AUTO_RECONNECT_POLICY_UPDATE_FAILED",
            code = error.public_code().as_str()
        );
    }
}

/// 将新的恢复态硬门禁复制到 provider 持有的共享原子值，不改写用户来源偏好。
fn update_remote_suggestion_policy_safely(
    policy: &RemoteSuggestionPolicy,
    updated: RemoteSuggestionPolicy,
) {
    policy.set_runtime_available(updated.runtime_available());
}

/// 将有效通知策略应用到同一个服务实例；同步锁错误通过诊断显式保留。
fn update_notification_settings_safely(
    notifications: &DesktopNotificationService,
    settings: DesktopNotificationSettings,
) {
    if let Err(error) = notifications.update_settings(settings) {
        tracing::error!(
            event = "DESKTOP_NOTIFICATION_SETTINGS_UPDATE_FAILED",
            code = error.public_code().as_str()
        );
    }
}

/// 把策略更新失败写入安全诊断，避免 watcher 线程静默保持过期的启动行为。
fn update_external_launch_safely(
    external_launch: &ExternalLaunchService,
    settings: ExternalLaunchSettings,
) {
    if let Err(error) = external_launch.update_settings(settings) {
        tracing::error!(
            event = "EXTERNAL_LAUNCH_SETTINGS_UPDATE_FAILED",
            code = error.public_code().as_str()
        );
    }
}

/// 超时保留 notification worker handle，后续退出重试继续等待同一桥接任务。
async fn stop_notification_bridge(
    bridge: &mut Option<NotificationBridge>,
    deadline: Instant,
) -> AppResult<()> {
    match bridge.as_mut() {
        Some(bridge) => bridge.shutdown_until(deadline).await,
        None => Ok(()),
    }
}

/// 停止 watcher 并等待 settings 消费者；超时保留仍在运行的 join owner。
async fn stop_configuration_watcher(
    watcher: &mut Option<ConfigurationChangeWatcher>,
    settings_task: &mut Option<JoinHandle<AppResult<()>>>,
    deadline: Instant,
) -> AppResult<()> {
    let (watcher_result, task_result) = tokio::join!(
        stop_watcher(watcher, deadline),
        join_settings_refresh_task(settings_task, deadline),
    );
    let mut failures = Vec::new();
    append_failure(&mut failures, watcher_result);
    append_failure(&mut failures, task_result);
    if failures.is_empty() {
        Ok(())
    } else {
        Err(AppError::infrastructure_with_source(
            "配置变更监听器未能全部退出",
            ShutdownFailures(failures),
        ))
    }
}

/// 停止系统文件观察并保留超时 worker 的句柄，便于共享截止时间内的重试收尾。
async fn stop_watcher(
    watcher: &mut Option<ConfigurationChangeWatcher>,
    deadline: Instant,
) -> AppResult<()> {
    let Some(active) = watcher.as_mut() else {
        return Ok(());
    };
    active.shutdown_until(deadline.into_std()).await?;
    *watcher = None;
    Ok(())
}

/// 等待 settings refresh task 真正完成；超时时不丢弃其 JoinHandle。
async fn join_settings_refresh_task(
    task: &mut Option<JoinHandle<AppResult<()>>>,
    deadline: Instant,
) -> AppResult<()> {
    let Some(active) = task.as_mut() else {
        return Ok(());
    };
    let result = match timeout_at(deadline, active).await {
        Ok(result) => result,
        Err(_) => return Err(AppError::timeout("等待配置策略刷新任务退出")),
    };
    *task = None;
    result.map_err(|source| {
        AppError::infrastructure_with_source("配置策略刷新任务异常终止", source)
    })?
}

/// 将 RDP 正在监管的客户端进程纳入与其它服务相同的绝对退出预算。
async fn shutdown_rdp(rdp: &SystemRdpPlatformLauncher, deadline: Instant) -> AppResult<()> {
    rdp.shutdown_until(deadline).await.map(|_| ())
}

/// 并行等待依赖同一会话资源的后台任务、PTY 与 RDP，避免先 join 写任务而阻塞其所需的 PTY close。
///
/// @author kongweiguang
async fn join_session_dependent_shutdowns<E, S, R>(
    task_executor_shutdown: E,
    session_shutdown: S,
    rdp_shutdown: R,
) -> (AppResult<()>, AppResult<()>, AppResult<()>)
where
    E: Future<Output = AppResult<()>>,
    S: Future<Output = AppResult<()>>,
    R: Future<Output = AppResult<()>>,
{
    tokio::join!(task_executor_shutdown, session_shutdown, rdp_shutdown)
}

/// MCP listener 停止并清除活动 endpoint 后才关闭 Agent 写入口，失败时保留可重试顺序。
async fn shutdown_mcp_before_agent_shutdown<F, B>(
    mcp_shutdown: F,
    begin_agent_shutdown: B,
) -> AppResult<()>
where
    F: Future<Output = AppResult<()>>,
    B: FnOnce(),
{
    mcp_shutdown.await?;
    begin_agent_shutdown();
    Ok(())
}

/// 记录一个子系统错误并继续关闭其余资源，避免首个失败跳过后续清理。
fn append_failure(failures: &mut Vec<AppError>, result: AppResult<()>) {
    if let Err(error) = result {
        failures.push(error);
    }
}

/// 保留全部异步退出错误作为单一安全错误的 source，供诊断继续检查原始 cause。
#[derive(Debug)]
struct ShutdownFailures(Vec<AppError>);

impl fmt::Display for ShutdownFailures {
    /// 只展示失败数量，不把协议端点、文件路径或底层错误文本写入用户提示。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} 个桌面资源清理步骤失败", self.0.len())
    }
}

impl Error for ShutdownFailures {
    /// 将首个有类型的失败保留在标准错误链中；其余失败仍留在聚合 source 中。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.0.first().map(|error| error as &(dyn Error + 'static))
    }
}

/// 启动失败后的清理错误与原始失败共同保留，避免清理异常覆盖根因。
#[derive(Debug)]
struct StartupCleanupFailure {
    startup: AppError,
    cleanup: AppError,
}

impl fmt::Display for StartupCleanupFailure {
    /// 只公开稳定错误码，保留启动与清理两种原因，不泄露底层错误正文。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "桌面启动和后台资源清理均未完成（启动错误码：{}，清理错误码：{}）",
            self.startup.public_code().as_str(),
            self.cleanup.public_code().as_str(),
        )
    }
}

impl Error for StartupCleanupFailure {
    /// 保留启动原始错误作为首要 cause；cleanup cause 仍随本聚合对象保存。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.startup)
    }
}

#[cfg(test)]
#[path = "../tests/support/composition_shutdown_unit.rs"]
mod shutdown_ordering_tests;

#[cfg(test)]
#[path = "../tests/support/serial_port_composition_unit.rs"]
mod serial_port_composition_tests;

#[cfg(test)]
#[path = "../tests/support/mcp_live/mod.rs"]
mod mcp_live_acceptance;
