//! 隔离的 Agent 面板服务，使界面测试不访问真实配置、CLI 或用户工作区。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult, PreferencesService,
    agents::{
        AgentHistoryProjection, AgentHistoryRequest, AgentLaunchResolver, AgentLauncherCatalog,
        AgentService, AgentSessionView, AgentTargetContext, AgentTargetResolution,
        AgentTerminalSnapshotRequest, AgentTextPreview, AgentTextSubmissionReceipt,
        CreateAgentSessionFromHistoryRequest, CreateAgentSessionRequest,
        RenameAgentSessionTitleRequest, SendAgentCommandRequest,
    },
    preferences::{
        ConfigurationRepository, SettingsRepository, SettingsSnapshot, VersionedSettings,
    },
};
use kerminal_domain::{
    agents::{AgentLauncherKey, AgentTargetBinding},
    identity::AgentSessionId,
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState,
    },
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

/// 默认设置由真实解析器消费，避免手工伪造不完整的启动器目录。
struct ReadOnlySettings;

#[async_trait]
impl SettingsRepository for ReadOnlySettings {
    /// 默认配置是隔离内存快照，不从用户工作区读取。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        Ok(VersionedSettings {
            settings: SettingsSnapshot::default(),
            revision: None,
        })
    }

    /// 界面测试只读取目录，不允许将配置保存伪装成成功。
    async fn save_settings(
        &self,
        _: SettingsSnapshot,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        Err(AppError::unsupported(
            "Agent visual fixture cannot save settings",
        ))
    }
}

#[async_trait]
impl ConfigurationRepository for ReadOnlySettings {
    /// 测试配置在内存中保持可读写恢复态，但任何实际写入都被拒绝。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 启动器目录不依赖配置文档读取。
    async fn read(&self, _: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported(
            "Agent visual fixture does not read documents",
        ))
    }

    /// 不修改真实配置，意外写入必须显式失败。
    async fn write(
        &self,
        _: ConfigDocumentPath,
        _: String,
        _: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported(
            "Agent visual fixture cannot write documents",
        ))
    }
}

/// 历史请求可暂时阻塞，使测试观察真实刷新中状态，而不依赖计时竞争。
pub(super) struct FixtureAgentService {
    view: AgentSessionView,
    catalog: AgentLauncherCatalog,
    hold_history: AtomicBool,
    fail_history: AtomicBool,
    history_gate: Semaphore,
}

impl FixtureAgentService {
    /// 使用默认设置解析真实 Launcher 目录；测试会话独立于任何本地路径。
    pub(super) async fn new(view: AgentSessionView) -> Self {
        let preferences = Arc::new(PreferencesService::new(Arc::new(ReadOnlySettings)));
        let resolver = AgentLaunchResolver::new(preferences);
        let catalog = resolver
            .launcher_catalog(&CancellationToken::new())
            .await
            .expect("default launcher catalog");
        Self {
            view,
            catalog,
            hold_history: AtomicBool::new(false),
            fail_history: AtomicBool::new(false),
            history_gate: Semaphore::new(0),
        }
    }

    /// 阻塞下一次历史查询，使测试可以检查已确认记录在刷新期间的真实布局。
    pub(super) fn hold_history(&self) {
        self.hold_history.store(true, Ordering::SeqCst);
    }

    /// 释放阻塞中的查询后允许普通查询继续完成。
    pub(super) fn release_history(&self) {
        self.hold_history.store(false, Ordering::SeqCst);
        self.history_gate.add_permits(1);
    }

    /// 通过真实查询回执模拟故障和恢复，不直接篡改面板的已核验状态。
    pub(super) fn set_history_failure(&self, fail: bool) {
        self.fail_history.store(fail, Ordering::SeqCst);
    }
}

macro_rules! impl_fixture_service {
    ($( $name:ident ($($arg:ident : $ty:ty),*) -> $result:ty; )*) => {
        #[async_trait]
        impl AgentService for FixtureAgentService {
    $(
        /// 仅实现本测试需观察的查询，未覆盖的操作不能制造虚假成功。
        async fn $name(&self, $($arg: $ty),*) -> AppResult<$result> {
            $(let _ = $arg;)*
            Err(AppError::unsupported("unused Agent visual fixture operation"))
        }
    )*

    /// 使用已解析的目录，确保菜单行与实际产品目录相同。
    async fn launcher_catalog(&self, _: CancellationToken) -> AppResult<AgentLauncherCatalog> {
        Ok(self.catalog.clone())
    }

    /// 受控回执始终保留同一个运行中会话，等待期间接受取消避免遗留任务。
    async fn history(&self, _: AgentHistoryRequest, cancellation: CancellationToken) -> AppResult<AgentHistoryProjection> {
        if self.hold_history.load(Ordering::SeqCst) {
            tokio::select! {
                permit = self.history_gate.acquire() => { permit.expect("fixture gate remains open").forget(); }
                _ = cancellation.cancelled() => return Err(AppError::cancelled("fixture history cancelled")),
            }
        }
        if self.fail_history.load(Ordering::SeqCst) {
            return Err(AppError::infrastructure("isolated history read failed"));
        }
        Ok(AgentHistoryProjection {
            sessions: vec![self.view.clone()],
            visible_session_ids: vec![self.view.session.agent_session_id.clone()],
            current_target_count: 1,
            all_count: 1,
            archived_count: 0,
        })
    }

        }
    };
}

impl_fixture_service! {
    configure_mcp_endpoint(endpoint: Option<String>, cancellation: CancellationToken) -> ();
    select_launcher(key: AgentLauncherKey, cancellation: CancellationToken) -> AgentLauncherCatalog;
    create(request: CreateAgentSessionRequest, cancellation: CancellationToken) -> AgentSessionView;
    list(cancellation: CancellationToken) -> Vec<AgentSessionView>;
    create_from_history(request: CreateAgentSessionFromHistoryRequest, cancellation: CancellationToken) -> AgentSessionView;
    rename_title(request: RenameAgentSessionTitleRequest, cancellation: CancellationToken) -> AgentSessionView;
    get(id: AgentSessionId, cancellation: CancellationToken) -> AgentSessionView;
    resume(id: AgentSessionId, cancellation: CancellationToken) -> AgentSessionView;
    archive(id: AgentSessionId, cancellation: CancellationToken) -> AgentSessionView;
    bind_preferred_target(id: AgentSessionId, binding: AgentTargetBinding, cancellation: CancellationToken) -> AgentSessionView;
    resolve_preferred_target(id: AgentSessionId, cancellation: CancellationToken) -> AgentTargetResolution;
    target_context(id: AgentSessionId, max_snapshot_bytes: usize, cancellation: CancellationToken) -> AgentTargetContext;
    preview_text(id: AgentSessionId, text: String, max_bytes: usize, cancellation: CancellationToken) -> AgentTextPreview;
    send_command_block_text(request: SendAgentCommandRequest, cancellation: CancellationToken) -> AgentTextSubmissionReceipt;
    record_terminal_snapshot(request: AgentTerminalSnapshotRequest, cancellation: CancellationToken) -> ();
}
