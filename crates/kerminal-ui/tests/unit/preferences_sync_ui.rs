//! 同步页使用真实 GPUI 控件和隔离仓储的恢复交互测试。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, ElementId, Entity, MouseButton, TestAppContext, VisualTestContext,
    component::WindowExt as _, px, size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    configuration_directory::{
        ConfigurationDirectoryKind, ConfigurationDirectoryPort, ConfigurationDirectoryService,
    },
    configuration_sync::{
        ConfigurationSyncOperationKind, ConfigurationSyncOperationResult, ConfigurationSyncOutcome,
        ConfigurationSyncRepository, ConfigurationSyncService, ConfigurationSyncStatus,
        SyncRepositoryState,
    },
    preferences::{
        ConfigurationRepository, DefaultLocalShellProvider, PreferencesService, ProfileDraft,
        ProfileRepository, ProfileRevision, ProfileService, SettingsRepository, SettingsSnapshot,
        VersionedProfile, VersionedSettings,
    },
};
use kerminal_domain::{
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState, TerminalProfile,
    },
    sessions::LocalSessionTarget,
};
use tokio::runtime::{Builder, Runtime};
use tokio_util::sync::CancellationToken;

use super::super::panel::{PreferencesPanel, PreferencesPanelServices, PreferencesSection};

struct EmptyConfiguration;

#[async_trait]
impl SettingsRepository for EmptyConfiguration {
    /// 页面使用默认设置，避免测试访问实际配置文件。
    ///
    /// @author kongweiguang
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        Ok(VersionedSettings {
            settings: SettingsSnapshot::default(),
            revision: None,
        })
    }

    /// 本测试没有设置写入意图，任何意外保存都应失败。
    ///
    /// @author kongweiguang
    async fn save_settings(
        &self,
        _: SettingsSnapshot,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        Err(AppError::unsupported(
            "sync UI fixture does not save settings",
        ))
    }
}

#[async_trait]
impl ConfigurationRepository for EmptyConfiguration {
    /// 仓库恢复态可写，使 Git 流程由独立测试状态控制。
    ///
    /// @author kongweiguang
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 同步页不直接读取配置文档。
    ///
    /// @author kongweiguang
    async fn read(&self, _: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported(
            "sync UI fixture does not read documents",
        ))
    }

    /// 同步页不直接修改配置文档。
    ///
    /// @author kongweiguang
    async fn write(
        &self,
        _: ConfigDocumentPath,
        _: String,
        _: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported(
            "sync UI fixture does not write documents",
        ))
    }
}

#[async_trait]
impl ProfileRepository for EmptyConfiguration {
    /// 同步测试没有 Profile，避免与本机启动程序产生关联。
    ///
    /// @author kongweiguang
    async fn list_profiles(&self) -> AppResult<Vec<TerminalProfile>> {
        Ok(Vec::new())
    }
    /// 版本列表同样为空，保持两个读取接口一致。
    ///
    /// @author kongweiguang
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(Vec::new())
    }
    /// 不从本机解析 Profile 文件。
    ///
    /// @author kongweiguang
    async fn get_profile(&self, _: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(None)
    }
    /// 此 fixture 不允许意外写入 Profile。
    ///
    /// @author kongweiguang
    async fn save_profile(
        &self,
        _: ProfileDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("profile write"))
    }
    /// 此 fixture 不允许意外删除 Profile。
    ///
    /// @author kongweiguang
    async fn delete_profile(&self, _: &str, _: ConfigRevision) -> AppResult<bool> {
        Err(AppError::unsupported("profile delete"))
    }
    /// 此 fixture 不允许改变默认 Profile。
    ///
    /// @author kongweiguang
    async fn set_default_profile(
        &self,
        _: &str,
        _: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        Err(AppError::unsupported("profile default"))
    }
}

impl DefaultLocalShellProvider for EmptyConfiguration {
    /// 同步页不启动本地终端，因此不探测机器上的 Shell。
    ///
    /// @author kongweiguang
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Err(AppError::unsupported("shell unavailable"))
    }
}

struct SyncFixture {
    status: Mutex<ConfigurationSyncStatus>,
    actions: Mutex<Vec<ConfigurationSyncOperationKind>>,
    continue_unknown: AtomicBool,
}

#[async_trait]
impl ConfigurationSyncRepository for SyncFixture {
    /// 状态由测试显式推进，模拟外部工具解决冲突后的下一次读取。
    ///
    /// @author kongweiguang
    async fn status(&self) -> AppResult<ConfigurationSyncStatus> {
        Ok(self.status.lock().unwrap().clone())
    }
    /// 恢复控件不得隐式 fetch 或执行完整同步。
    ///
    /// @author kongweiguang
    async fn fetch(&self, _: CancellationToken) -> AppResult<ConfigurationSyncOperationResult> {
        Err(AppError::unsupported("unexpected fetch"))
    }
    /// 恢复控件不得隐式 pull。
    ///
    /// @author kongweiguang
    async fn pull(&self, _: CancellationToken) -> AppResult<ConfigurationSyncOperationResult> {
        Err(AppError::unsupported("unexpected pull"))
    }
    /// 恢复控件不得重复创建本地提交。
    ///
    /// @author kongweiguang
    async fn commit(&self, _: CancellationToken) -> AppResult<ConfigurationSyncOperationResult> {
        Err(AppError::unsupported("unexpected commit"))
    }
    /// 恢复控件不得在用户确认前推送远端。
    ///
    /// @author kongweiguang
    async fn push(&self, _: CancellationToken) -> AppResult<ConfigurationSyncOperationResult> {
        Err(AppError::unsupported("unexpected push"))
    }
    /// 继续时记录唯一动作并将仓库投影为已恢复。
    ///
    /// @author kongweiguang
    async fn continue_rebase(
        &self,
        _: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.finish_rebase(ConfigurationSyncOperationKind::ContinueRebase)
    }
    /// 中止仅由确认按钮触发，使用相同的状态回执。
    ///
    /// @author kongweiguang
    async fn abort_rebase(
        &self,
        _: CancellationToken,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.finish_rebase(ConfigurationSyncOperationKind::AbortRebase)
    }
}

impl SyncFixture {
    /// 保存精确动作及恢复状态，防止把一次恢复误当作完整同步。
    ///
    /// @author kongweiguang
    fn finish_rebase(
        &self,
        kind: ConfigurationSyncOperationKind,
    ) -> AppResult<ConfigurationSyncOperationResult> {
        self.actions.lock().unwrap().push(kind);
        if kind == ConfigurationSyncOperationKind::ContinueRebase
            && self.continue_unknown.swap(false, Ordering::SeqCst)
        {
            return Err(AppError::outcome_unknown("test rebase outcome unknown"));
        }
        let mut status = self.status.lock().unwrap();
        status.rebase_in_progress = false;
        status.repository = SyncRepositoryState::Ready;
        Ok(ConfigurationSyncOperationResult {
            kind,
            outcome: ConfigurationSyncOutcome::Succeeded,
            summary: "done".into(),
            affected_paths: Vec::new(),
            commit_hash: None,
        })
    }
}

#[derive(Default)]
struct DirectoryFixture(AtomicUsize);

#[async_trait]
impl ConfigurationDirectoryPort for DirectoryFixture {
    /// 只记录打开固定配置根的请求，不启动真实文件管理器。
    ///
    /// @author kongweiguang
    async fn request_open(
        &self,
        kind: ConfigurationDirectoryKind,
        _: CancellationToken,
    ) -> AppResult<()> {
        assert_eq!(kind, ConfigurationDirectoryKind::Root);
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// 绘制生产控件并推进隔离 Tokio 运行时，确保断言发生在真实异步回执之后。
///
/// @author kongweiguang
fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..5 {
        visual
            .cx
            .background_executor
            .advance_clock(Duration::from_millis(400));
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        visual.update(|window, app| window.draw(app).clear(app));
    }
}

/// 根据生产控件的实际布局派发鼠标事件，避免调用面板内部方法替代用户操作。
///
/// @author kongweiguang
fn click(visual: &mut VisualTestContext, selector: &str) {
    let id: ElementId = selector.to_owned().into();
    let bounds = visual
        .update(|window, _| {
            gpui_kit::base::test_support::find(window, &[], &id)
                .or_else(|| {
                    gpui_kit::base::test_support::snapshots(window)
                        .into_iter()
                        .filter(|snapshot| snapshot.path().contains(&id))
                        .min_by_key(|snapshot| snapshot.path().len())
                })
                .map(|snapshot| snapshot.bounds())
                .or_else(|| window.debug_selector_bounds(selector))
        })
        .unwrap_or_else(|| panic!("missing production control: {selector}"));
    let position = bounds.center();
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
}

/// 未解决冲突时禁止继续；刷新后真实按钮只调用对应恢复动作，中止需要确认。
///
/// @author kongweiguang
#[gpui_kit::test]
fn sync_recovery_controls_require_current_status_and_abort_confirmation(app: &mut TestAppContext) {
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let fixture = Arc::new(SyncFixture {
        status: Mutex::new(ConfigurationSyncStatus {
            git_available: true,
            repository: SyncRepositoryState::Detached,
            writable: true,
            branch: None,
            upstream: Some("origin/main".into()),
            ahead: Some(1),
            behind: Some(1),
            local_changes: Vec::new(),
            protected_changes: Vec::new(),
            conflicts: vec!["settings.toml".into()],
            rebase_in_progress: true,
            missing_ignore_rules: Vec::new(),
        }),
        actions: Mutex::new(Vec::new()),
        continue_unknown: AtomicBool::new(false),
    });
    let directory = Arc::new(DirectoryFixture::default());
    let configuration = Arc::new(EmptyConfiguration);
    let services = PreferencesPanelServices {
        preferences: PreferencesService::new(configuration.clone()),
        profiles: ProfileService::new(configuration.clone(), configuration),
        vault_keys: None,
        configuration_sync: Some(ConfigurationSyncService::new(fixture.clone())),
        configuration_changes: None,
        external_protocol: None,
        configuration_directory: Some(ConfigurationDirectoryService::new(directory.clone())),
        mcp_runtime_available: false,
        mcp_runtime: None,
        executor: ApplicationTaskExecutor::new(runtime.handle().clone()),
    };
    let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted_panel = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel: Entity<PreferencesPanel> =
            cx.new(|cx| PreferencesPanel::new(services, window, cx));
        *mounted_panel.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted.borrow_mut().take().unwrap();
    visual.simulate_resize(size(px(1280.), px(960.)));
    settle(&runtime, visual);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.section = PreferencesSection::ConfigurationSync;
            cx.notify();
        })
    });
    settle(&runtime, visual);

    click(visual, "workspace-sync-open-directory");
    settle(&runtime, visual);
    assert_eq!(directory.0.load(Ordering::SeqCst), 1);
    click(visual, "workspace-sync-rebase-continue");
    settle(&runtime, visual);
    assert!(fixture.actions.lock().unwrap().is_empty());

    fixture.status.lock().unwrap().conflicts.clear();
    click(visual, "workspace-sync-refresh-status");
    settle(&runtime, visual);
    click(visual, "workspace-sync-rebase-continue");
    settle(&runtime, visual);
    assert_eq!(
        *fixture.actions.lock().unwrap(),
        vec![ConfigurationSyncOperationKind::ContinueRebase]
    );

    fixture.status.lock().unwrap().rebase_in_progress = true;
    click(visual, "workspace-sync-refresh-status");
    settle(&runtime, visual);
    click(visual, "workspace-sync-rebase-abort");
    settle(&runtime, visual);
    assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
    visual.simulate_keystrokes("escape");
    settle(&runtime, visual);
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(fixture.actions.lock().unwrap().len(), 1);
    click(visual, "workspace-sync-rebase-abort");
    settle(&runtime, visual);
    assert!(visual.update(|window, cx| window.has_active_dialog(cx)));
    visual.update(|_, cx| {
        let panel = panel.read(cx);
        assert!(panel.sync_status.as_ref().unwrap().rebase_in_progress);
        assert!(!panel.sync_loading && !panel.connectivity.sync_running);
        assert!(!panel.connectivity.key_loading && !panel.connectivity.key_saving);
        assert!(!panel.sync_outcome_unknown);
    });
    click(visual, "ok");
    settle(&runtime, visual);
    assert!(!visual.update(|window, cx| window.has_active_dialog(cx)));
    assert_eq!(
        *fixture.actions.lock().unwrap(),
        vec![
            ConfigurationSyncOperationKind::ContinueRebase,
            ConfigurationSyncOperationKind::AbortRebase
        ]
    );

    {
        let mut status = fixture.status.lock().unwrap();
        status.rebase_in_progress = true;
        status.repository = SyncRepositoryState::Detached;
        status.upstream = None;
    }
    fixture.continue_unknown.store(true, Ordering::SeqCst);
    click(visual, "workspace-sync-refresh-status");
    settle(&runtime, visual);
    click(visual, "workspace-sync-rebase-continue");
    settle(&runtime, visual);
    assert!(visual.update(|_, cx| panel.read(cx).sync_outcome_unknown));
    click(visual, "workspace-sync-run");
    settle(&runtime, visual);
    assert!(!visual.update(|_, cx| panel.read(cx).sync_outcome_unknown));
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .sync_status
            .as_ref()
            .unwrap()
            .rebase_in_progress
    }));
    assert_eq!(fixture.actions.lock().unwrap().len(), 3);
}
