//! 在隔离组合根验证 MCP 设置替换的端口占用与 CAS 回滚。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    connections::credentials::{CredentialService, CredentialVaultRepository},
    mcp_runtime::{McpRuntimeControl, McpRuntimeState, McpTokenStore},
    preferences::{
        ConfigurationRepository, PreferencesService, SettingsRepository, SettingsSnapshot,
        VersionedSettings, settings::McpBindMode,
    },
};
use kerminal_domain::preferences::{
    ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationRecoveryState,
};
use kerminal_infrastructure::configuration::ConfigurationStore;
use kerminal_mcp::{McpRuntimeController, McpServices};
use tokio::{
    net::{TcpListener, TcpStream},
    runtime::Handle,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

use super::DesktopComposition;

/// 只在指定一次保存时注入确定的 CAS 冲突，其余操作仍走生产文件仓储。
struct ConflictOnceRepository {
    store: ConfigurationStore,
    conflict: AtomicBool,
}

#[async_trait]
impl SettingsRepository for ConflictOnceRepository {
    /// 加载不改变仓储版本，保证故障只发生在 listener 替换之后。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        self.store.load_settings().await
    }

    /// 显式制造一次保存冲突，以验证新 listener 启动后也能恢复旧端口。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        if self.conflict.swap(false, Ordering::AcqRel) {
            return Err(AppError::conflict("test settings revision conflict"));
        }
        self.store.save_settings(settings, revision).await
    }
}

#[async_trait]
impl ConfigurationRepository for ConflictOnceRepository {
    /// 复用生产恢复状态，避免测试假装损坏配置可写。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        self.store.recovery_state()
    }

    /// 这个用例不直接读原文，保留生产配置读取语义。
    async fn read(&self, path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        self.store.read(path).await
    }

    /// 这个用例不直接写原文，仍通过生产仓储执行版本门禁。
    async fn write(
        &self,
        path: ConfigDocumentPath,
        source: String,
        revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        self.store.write(path, source, revision).await
    }
}

/// 仅用当前组合根的真实服务构造第二个控制器，不重复创建会话或业务 service。
fn controller(
    composition: &DesktopComposition,
    repository: Arc<ConflictOnceRepository>,
) -> McpRuntimeController {
    let vault: Arc<dyn CredentialVaultRepository> = composition._vault.clone();
    let app = &composition.app_services;
    let services = McpServices::new(app.sessions.clone())
        .with_task_executor(composition.task_executor.clone())
        .with_agents(composition.agent_manager.clone())
        .with_connections(
            app.remote_connections.clone().unwrap(),
            app.hosts.clone().unwrap(),
            CredentialService::new(vault),
        )
        .with_transfers(composition.transfers.clone())
        .with_operations(composition.operations.as_ref().clone())
        .with_commands(composition.commands.as_ref().clone())
        .with_profiles(app.profiles.clone())
        .with_configuration_validation(app.configuration_validation.clone().unwrap());
    let token_store: Arc<dyn McpTokenStore> = composition._vault.clone();
    McpRuntimeController::with_preferences(
        Handle::current(),
        services,
        PreferencesService::new(repository),
        token_store,
        composition.notifications.as_ref().clone(),
    )
}

/// 选择隔离临时端口供下一步绑定；真实生产端口不在测试范围内。
async fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap().port()
}

/// 端口占用不能中断原监听；CAS 冲突在新监听启动后仍需恢复原端口与配置。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_reconfigure_preserves_listener_on_conflict() {
    let root = tempfile::tempdir().unwrap();
    let mut composition = DesktopComposition::build(Handle::current(), root.path())
        .await
        .unwrap();
    let repository = Arc::new(ConflictOnceRepository {
        store: composition.configuration.clone(),
        conflict: AtomicBool::new(false),
    });
    let control = controller(&composition, repository.clone());
    let loaded = composition.preferences.load_settings().await.unwrap();
    let mut initial = loaded.settings;
    initial.mcp.port = free_port().await;
    let saved = composition
        .preferences
        .save_settings(initial, loaded.revision)
        .await
        .unwrap();
    let old_port = saved.settings.mcp.port;
    let started = control.start(CancellationToken::new()).await.unwrap();
    assert_eq!(started.state, McpRuntimeState::Running);
    let old_endpoint = started.endpoint.clone();

    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut requested = saved.settings.mcp.clone();
    requested.port = occupied.local_addr().unwrap().port();
    let occupied_result = control
        .apply_settings(requested, saved.revision.clone(), CancellationToken::new())
        .await;
    let after_occupied = control.snapshot();
    let old_still_accepts = TcpStream::connect(("127.0.0.1", old_port)).await.is_ok();

    let next_port = free_port().await;
    let mut requested = saved.settings.mcp.clone();
    requested.port = next_port;
    repository.conflict.store(true, Ordering::Release);
    let conflict_result = control
        .apply_settings(requested, saved.revision.clone(), CancellationToken::new())
        .await;
    let after_conflict = control.snapshot();
    let restored_accepts = TcpStream::connect(("127.0.0.1", old_port)).await.is_ok();
    let persisted = composition.preferences.load_settings().await.unwrap();

    let mut requested = saved.settings.mcp.clone();
    requested.bind_mode = McpBindMode::Lan;
    let switched = control
        .apply_settings(
            requested,
            persisted.revision.clone(),
            CancellationToken::new(),
        )
        .await;
    let after_switch = control.snapshot();
    let switched_mode = switched
        .as_ref()
        .ok()
        .map(|value| value.settings.mcp.bind_mode);
    let loopback_still_accepts = TcpStream::connect(("127.0.0.1", old_port)).await.is_ok();

    drop(occupied);
    let controller_shutdown = control
        .shutdown_until(Instant::now() + Duration::from_secs(10))
        .await;
    let composition_shutdown = composition
        .shutdown_until(Instant::now() + Duration::from_secs(30))
        .await;
    assert!(controller_shutdown.is_ok());
    assert!(composition_shutdown.is_ok());
    assert!(occupied_result.is_err());
    assert_eq!(after_occupied.state, McpRuntimeState::Running);
    assert_eq!(after_occupied.endpoint, old_endpoint);
    assert!(old_still_accepts);
    assert!(conflict_result.is_err());
    assert_eq!(after_conflict.state, McpRuntimeState::Running);
    assert_eq!(after_conflict.endpoint, old_endpoint);
    assert!(restored_accepts);
    assert_eq!(persisted.settings.mcp.port, old_port);
    assert_eq!(switched_mode, Some(McpBindMode::Lan));
    assert_eq!(after_switch.state, McpRuntimeState::Running);
    assert_eq!(after_switch.endpoint, old_endpoint);
    assert!(loopback_still_accepts);
}
