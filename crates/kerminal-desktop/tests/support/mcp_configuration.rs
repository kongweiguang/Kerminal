//! 在隔离组合根验证 MCP 设置替换的端口占用与 CAS 回滚。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PublicErrorCode,
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

/// 使用生产仓储，并在明确的读写边界注入一次故障，避免依赖任务调度时序。
#[derive(Default)]
struct SettingsFaults {
    conflict: AtomicBool,
    cancel_after_read: Mutex<Option<CancellationToken>>,
    concurrent_port: AtomicU16,
}

struct FaultInjectingRepository {
    store: ConfigurationStore,
    faults: SettingsFaults,
}

#[async_trait]
impl SettingsRepository for FaultInjectingRepository {
    /// 在装载完成与提交开始之间取消，固定验证真实的提交门禁。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let result = self.store.load_settings().await;
        if let Some(cancellation) = self.faults.cancel_after_read.lock().unwrap().take() {
            cancellation.cancel();
        }
        result
    }

    /// 在回读前模拟另一笔编辑，区分确定冲突和不可确认的提交结果。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        if self.faults.conflict.swap(false, Ordering::AcqRel) {
            return Err(AppError::conflict("test settings revision conflict"));
        }
        let concurrent_port = self.faults.concurrent_port.swap(0, Ordering::AcqRel);
        if concurrent_port != 0 {
            let mut external = settings;
            external.mcp.port = concurrent_port;
            self.store.save_settings(external, revision).await?;
            return Err(AppError::outcome_unknown(
                "test settings commit unconfirmed",
            ));
        }
        self.store.save_settings(settings, revision).await
    }
}

#[async_trait]
impl ConfigurationRepository for FaultInjectingRepository {
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
    repository: Arc<FaultInjectingRepository>,
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
        .with_configuration_validation(app.configuration_validation.clone().unwrap())
        // 测试控制器复用组合根服务，也必须注入同一冻结插件视图；自行构造时漏掉
        // 会让 validate_complete 把运行门禁判成不可用，从而与生产启动行为分叉。
        .with_plugins(app.plugins.clone());
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

/// 每次都从真实配置文件重新装配生产组合根，模拟用户完全退出后再次打开应用。
async fn build_production_composition(root: &std::path::Path) -> DesktopComposition {
    DesktopComposition::build(Handle::current(), root)
        .await
        .unwrap()
}

/// 组合根关闭必须等到后台 owner 回收，否则同一隔离根的重启回归会与上一周期争用。
async fn shutdown_production_composition(composition: &mut DesktopComposition) {
    composition
        .shutdown_until(Instant::now() + Duration::from_secs(30))
        .await
        .unwrap();
}

/// 读盘期间的取消必须阻止自动启动写入，同时保持监听器未启动。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_manual_start_cancelled_after_read_does_not_save_intent() {
    let root = tempfile::tempdir().unwrap();
    let mut composition = build_production_composition(root.path()).await;
    let repository = Arc::new(FaultInjectingRepository {
        store: composition.configuration.clone(),
        faults: SettingsFaults::default(),
    });
    let runtime = controller(&composition, repository.clone());
    let cancellation = CancellationToken::new();
    *repository.faults.cancel_after_read.lock().unwrap() = Some(cancellation.clone());

    let error = runtime.start_manually(cancellation).await.unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert_eq!(runtime.snapshot().state, McpRuntimeState::Stopped);
    assert!(
        !repository
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    shutdown_production_composition(&mut composition).await;
}

/// 自动启动值相同、端口不同的回读不能让未知请求启动到外部改写的地址。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_manual_start_rejects_unknown_commit_with_other_listener() {
    let root = tempfile::tempdir().unwrap();
    let mut composition = build_production_composition(root.path()).await;
    let repository = Arc::new(FaultInjectingRepository {
        store: composition.configuration.clone(),
        faults: SettingsFaults::default(),
    });
    let runtime = controller(&composition, repository.clone());
    let original_port = repository.load_settings().await.unwrap().settings.mcp.port;
    let port = if original_port == 37658 { 37659 } else { 37658 };
    repository
        .faults
        .concurrent_port
        .store(port, Ordering::Release);

    let error = runtime
        .start_manually(CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(runtime.snapshot().state, McpRuntimeState::Stopped);
    let persisted = repository.load_settings().await.unwrap().settings.mcp;
    assert_eq!(persisted.port, port);
    assert!(persisted.auto_start);
    shutdown_production_composition(&mut composition).await;
}

/// 端口占用不能中断原监听；CAS 冲突在新监听启动后仍需恢复原端口与配置。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_reconfigure_preserves_listener_on_conflict() {
    let root = tempfile::tempdir().unwrap();
    let mut composition = DesktopComposition::build(Handle::current(), root.path())
        .await
        .unwrap();
    let repository = Arc::new(FaultInjectingRepository {
        store: composition.configuration.clone(),
        faults: SettingsFaults::default(),
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
    repository.faults.conflict.store(true, Ordering::Release);
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

/// 手动启停意图必须跨生产组合根重启保留；首次默认、Agent 按需启动和普通退出不得固化。
///
/// 回归刻意使用真实 `settings.toml` 和动态端口反复装配/关闭组合根，避免只验证内存仓储。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_manual_auto_start_survives_production_restart() {
    let root = tempfile::tempdir().unwrap();

    let mut composition = build_production_composition(root.path()).await;
    let initial = composition.preferences.load_settings().await.unwrap();
    assert!(!initial.settings.mcp.auto_start);
    assert_eq!(
        composition.restore_mcp_runtime().await.unwrap().state,
        McpRuntimeState::Stopped
    );
    let port = free_port().await;
    let mut settings = initial.settings;
    settings.mcp.port = port;
    composition
        .preferences
        .save_settings(settings, initial.revision)
        .await
        .unwrap();
    assert!(root.path().join("settings.toml").is_file());

    // Agent 按需启动只影响本次进程，不能把用户一次调用固化成下次自动启动。
    let started = composition
        .mcp_runtime
        .start(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(started.state, McpRuntimeState::Running);
    assert!(
        !composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    shutdown_production_composition(&mut composition).await;

    let mut composition = build_production_composition(root.path()).await;
    assert_eq!(
        composition.restore_mcp_runtime().await.unwrap().state,
        McpRuntimeState::Stopped
    );

    // 手动启动在绑定 listener 前确认意图；正常退出不得把 true 覆盖成 false。
    let started = composition
        .mcp_runtime
        .start_manually(CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(started.state, McpRuntimeState::Running);
    let confirmed = composition.preferences.load_settings().await.unwrap();
    assert!(confirmed.settings.mcp.auto_start);
    assert_eq!(confirmed.settings.mcp.port, port);
    shutdown_production_composition(&mut composition).await;

    let mut composition = build_production_composition(root.path()).await;
    assert!(
        composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    let restored = composition.restore_mcp_runtime().await.unwrap();
    assert_eq!(restored.state, McpRuntimeState::Running);
    assert!(
        restored
            .endpoint
            .as_deref()
            .is_some_and(|endpoint| { endpoint.contains(&format!("127.0.0.1:{port}")) })
    );

    // 监听草稿保存不得覆盖手动启动确认的偏好；即使调用方带回 false，磁盘值仍是权威。
    let current = composition.preferences.load_settings().await.unwrap();
    let mut listener_only = current.settings.mcp.clone();
    listener_only.auto_start = false;
    composition
        .mcp_runtime
        .apply_settings(
            listener_only,
            current.revision.clone(),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        composition.mcp_runtime.snapshot().state,
        McpRuntimeState::Running
    );
    assert!(
        composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );

    // 手动停止先写 false 再收尾，随后重启必须保持关闭，而不是被普通退出重新拉起。
    let stopped = composition
        .mcp_runtime
        .stop_manually_until(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(stopped.state, McpRuntimeState::Stopped);
    assert!(
        !composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    shutdown_production_composition(&mut composition).await;

    let mut composition = build_production_composition(root.path()).await;
    assert!(
        !composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    assert_eq!(
        composition.restore_mcp_runtime().await.unwrap().state,
        McpRuntimeState::Stopped
    );
    shutdown_production_composition(&mut composition).await;
}

/// 端口占用时手动启动保留已确认的意图；重启恢复失败保持 Failed，可手动停止清理。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_occupied_port_keeps_auto_start_until_manual_stop() {
    let root = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let occupied_port = occupied.local_addr().unwrap().port();

    let mut composition = build_production_composition(root.path()).await;
    let loaded = composition.preferences.load_settings().await.unwrap();
    let mut settings = loaded.settings;
    settings.mcp.port = occupied_port;
    composition
        .preferences
        .save_settings(settings, loaded.revision)
        .await
        .unwrap();

    // 绑定失败不能撤销用户刚确认的意图，否则重启后会静默丢失自动启动。
    composition
        .mcp_runtime
        .start_manually(CancellationToken::new())
        .await
        .unwrap_err();
    let failed = composition.mcp_runtime.snapshot();
    assert_eq!(failed.state, McpRuntimeState::Failed);
    assert!(failed.failure_reason.is_some());
    assert!(
        composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    shutdown_production_composition(&mut composition).await;

    let mut composition = build_production_composition(root.path()).await;
    composition.restore_mcp_runtime().await.unwrap_err();
    let failed = composition.mcp_runtime.snapshot();
    assert_eq!(failed.state, McpRuntimeState::Failed);
    assert!(failed.failure_reason.is_some());
    assert!(
        composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );

    // 没有 listener 的 Failed 状态也能先确认 false，再清空失败快照。
    let stopped = composition
        .mcp_runtime
        .stop_manually_until(Instant::now() + Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(stopped.state, McpRuntimeState::Stopped);
    assert!(stopped.failure_reason.is_none());
    assert!(
        !composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );

    // 取消只允许发生在写意图之前，预取消不得把 autoStart 改回 true。
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let error = composition
        .mcp_runtime
        .start_manually(cancelled)
        .await
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    assert!(
        !composition
            .preferences
            .load_settings()
            .await
            .unwrap()
            .settings
            .mcp
            .auto_start
    );
    shutdown_production_composition(&mut composition).await;

    let mut composition = build_production_composition(root.path()).await;
    assert_eq!(
        composition.restore_mcp_runtime().await.unwrap().state,
        McpRuntimeState::Stopped
    );
    shutdown_production_composition(&mut composition).await;
    drop(occupied);
}
