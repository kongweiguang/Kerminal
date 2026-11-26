//! 设置生产面板的真实 GPUI 鼠标、键盘与持久化交互回归。
//!
//! @author kongweiguang

#[path = "preferences_color_sampling.rs"]
mod color_sampling;

#[path = "preferences_mcp.rs"]
mod mcp;

#[path = "preferences_keyword_regressions.rs"]
mod keyword_regressions;

#[path = "preferences_initial_scroll.rs"]
mod initial_scroll;

#[path = "preferences_save_recovery.rs"]
mod save_recovery;

use std::{
    collections::BTreeMap,
    ffi::OsString,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Bounds, ElementId, Entity, MouseButton, Pixels, ScrollDelta, ScrollWheelEvent,
    TestAppContext, VisualTestContext, hsla, point, px, size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    extensions::{
        CatalogSelection, CatalogSnapshot, CatalogSource, DEFAULT_PLUGIN_CATALOG_URL,
        DropInInspection, ExtensionChange, ExtensionIcon, ExtensionReceipt, ExtensionsService,
        InstallProgress, LocalPackageInspection, PluginPackageStore,
    },
    preferences::{
        ConfigurationGuideStatus, ConfigurationRepository, DefaultLocalShellProvider,
        KeybindingScope, PreferencesService, ProfileDraft, ProfileRepository, ProfileRevision,
        ProfileService, SettingsRepository, SettingsSnapshot, VersionedProfile, VersionedSettings,
    },
};
use kerminal_domain::{
    plugins::{PluginId, PluginStoreState},
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState,
    },
    sessions::LocalSessionTarget,
};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::render_desktop_about::{
    MAIN_SHORTCUT_PRESENTATIONS, display_repository_url, display_shortcut,
    main_settings_shortcut_visible, main_shortcut_order, main_shortcut_presentation,
    shortcut_display_scope,
};
use super::{KeyboardPlatform, PreferencesPanel, PreferencesPanelServices};

/// 附加动作的预期清单属于回归输入，不在生产渲染模块保留测试专用常量。
const GPUI_ONLY_SHORTCUT_ACTIONS: [&str; 5] = [
    "terminal.search",
    "workspace.quickOpen",
    "workspace.commandPalette",
    "application.quit",
    "terminal.paste",
];

#[derive(Clone)]
struct MemorySettingsRepository {
    state: Arc<Mutex<MemorySettingsState>>,
}

struct MemorySettingsState {
    settings: SettingsSnapshot,
    revision: Option<ConfigRevision>,
    next_revision: u64,
    save_count: usize,
    save_attempt_count: usize,
    fail_next_settings_save: bool,
    /// 指定第几次真实保存尝试失败，用于验证来源成功后的无关 follow-up 失败。
    fail_settings_save_attempt: Option<usize>,
    /// 挂起下一次保存直到测试放行，用于确定性复现保存中的来源请求。
    hold_next_settings_save: Option<oneshot::Receiver<()>>,
    unknown_after_settings_save: bool,
    /// 写入后返回未知但允许回读成功，用于验证未知结果的确认分支。
    unknown_after_settings_save_confirmable: bool,
    fail_next_settings_read: bool,
    settings_read_count: usize,
    /// 下一次回读返回与磁盘不同的来源地址，用于验证回读不一致时不切换运行源。
    override_next_read_url: Option<String>,
    profiles: Vec<VersionedProfile>,
    profile_saves: usize,
    unknown_after_profile_save: bool,
    unknown_after_default_save: bool,
}

impl MemorySettingsRepository {
    /// 以 typed 默认设置创建隔离仓储，使测试只覆盖面板到 application 端口的真实调用链。
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(MemorySettingsState {
                settings: SettingsSnapshot::default(),
                revision: None,
                next_revision: 1,
                save_count: 0,
                save_attempt_count: 0,
                fail_next_settings_save: false,
                fail_settings_save_attempt: None,
                hold_next_settings_save: None,
                unknown_after_settings_save: false,
                unknown_after_settings_save_confirmable: false,
                fail_next_settings_read: false,
                settings_read_count: 0,
                override_next_read_url: None,
                profiles: Vec::new(),
                profile_saves: 0,
                unknown_after_profile_save: false,
                unknown_after_default_save: false,
            })),
        }
    }

    /// 返回仓储已确认值与真实保存次数，用于证明 UI 操作最终进入持久化端口。
    fn snapshot(&self) -> (SettingsSnapshot, usize) {
        let state = self.state.lock().expect("settings state is not poisoned");
        (state.settings.clone(), state.save_count)
    }

    /// 只让下一次真实保存请求失败，以验证界面保留草稿并可通过重试恢复。
    ///
    /// @author kongweiguang
    fn fail_next_settings_save(&self) {
        self.state.lock().unwrap().fail_next_settings_save = true;
    }

    /// 将失败与成功请求一起计数，防止自动重试或重复点击被误判为一次提交。
    ///
    /// @author kongweiguang
    fn save_attempt_count(&self) -> usize {
        self.state.lock().unwrap().save_attempt_count
    }

    /// 只让下一次设置回读失败，用于区分“已保存但回读失败”和保存失败。
    ///
    /// @author kongweiguang
    fn fail_next_settings_read(&self) {
        self.state.lock().unwrap().fail_next_settings_read = true;
    }

    /// 回读次数用于证明切换运行源前确实重新读取了权威设置。
    ///
    /// @author kongweiguang
    fn settings_read_count(&self) -> usize {
        self.state.lock().unwrap().settings_read_count
    }

    /// 下一次回读返回指定来源，模拟磁盘内容与提交值不一致。
    ///
    /// @author kongweiguang
    fn override_next_settings_read_url(&self, url: &str) {
        self.state.lock().unwrap().override_next_read_url = Some(url.to_owned());
    }

    /// 挂起下一次保存并返回放行句柄，让测试在保存真正在途时注入来源请求或编辑。
    ///
    /// 使用 oneshot 而不是 sleep：保存任务在锁外等待，测试可以确定地观察
    /// Saving 状态后再放行，避免定时器抖动造成偶发通过。
    ///
    /// @author kongweiguang
    fn hold_next_settings_save(&self) -> oneshot::Sender<()> {
        let (sender, receiver) = oneshot::channel();
        self.state.lock().unwrap().hold_next_settings_save = Some(receiver);
        sender
    }

    /// 让指定序号的保存尝试失败，用于只失败 follow-up 而保留来源提交成功。
    ///
    /// @author kongweiguang
    fn fail_settings_save_attempt(&self, attempt: usize) {
        self.state.lock().unwrap().fail_settings_save_attempt = Some(attempt);
    }

    /// 下一次保存写入磁盘后返回未知结果，但回读保持成功，用于验证未知确认分支。
    ///
    /// @author kongweiguang
    fn return_unknown_after_next_save(&self) {
        self.state
            .lock()
            .unwrap()
            .unknown_after_settings_save_confirmable = true;
    }
}

#[async_trait]
impl SettingsRepository for MemorySettingsRepository {
    /// 返回唯一的测试快照和 CAS revision；读回可独立失败并计数，证明恢复没有重放写入。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        state.settings_read_count += 1;
        if std::mem::take(&mut state.fail_next_settings_read) {
            return Err(AppError::infrastructure("模拟设置读回失败"));
        }
        let mut settings = state.settings.clone();
        if let Some(url) = state.override_next_read_url.take() {
            settings.plugin_catalog_url = url;
        }
        Ok(VersionedSettings {
            settings,
            revision: state.revision.clone(),
        })
    }

    /// 每次请求先记录真实尝试，再按可控故障和 revision 决定提交；未知结果可伴随首次读回失败。
    ///
    /// 可选的 hold 在锁外等待，让测试确定性地制造“保存真正在途”的窗口；放行后
    /// 重新校验 revision，避免测试期间的并发修改被旧快照覆盖。
    ///
    /// @author kongweiguang
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let hold = {
            let mut state = self.state.lock().expect("settings state is not poisoned");
            state.save_attempt_count += 1;
            let attempt = state.save_attempt_count;
            if std::mem::take(&mut state.fail_next_settings_save) {
                return Err(AppError::infrastructure("模拟设置写入失败"));
            }
            if state.fail_settings_save_attempt == Some(attempt) {
                state.fail_settings_save_attempt = None;
                return Err(AppError::infrastructure("模拟后续设置写入失败"));
            }
            if state.revision != expected_revision {
                return Err(AppError::conflict("测试设置 revision 已变化"));
            }
            state.hold_next_settings_save.take()
        };
        if let Some(hold) = hold {
            let _ = hold.await;
        }
        let mut state = self.state.lock().expect("settings state is not poisoned");
        if state.revision != expected_revision {
            return Err(AppError::conflict("测试设置 revision 已变化"));
        }
        let revision = ConfigRevision::from_sha256(format!("settings-ui-{}", state.next_revision));
        state.next_revision += 1;
        state.save_count += 1;
        state.settings = settings.clone();
        state.revision = Some(revision.clone());
        if std::mem::take(&mut state.unknown_after_settings_save) {
            state.fail_next_settings_read = true;
            return Err(AppError::outcome_unknown("模拟设置提交结果未知"));
        }
        if std::mem::take(&mut state.unknown_after_settings_save_confirmable) {
            return Err(AppError::outcome_unknown("模拟设置提交结果未知"));
        }
        Ok(VersionedSettings {
            settings,
            revision: Some(revision),
        })
    }
}

#[async_trait]
impl ConfigurationRepository for MemorySettingsRepository {
    /// 该仓储只服务隔离测试，默认配置可写且不携带恢复诊断。
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        }
    }

    /// 设置交互测试不编辑普通 TOML 文档，避免绕过 typed 设置用例。
    async fn read(&self, _path: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        Err(AppError::unsupported("设置 UI fixture 不读取文档"))
    }

    /// 配置文件写入由 typed CAS 设置端口负责；文档编辑不在此 fixture 中使用。
    async fn write(
        &self,
        _path: ConfigDocumentPath,
        _source: String,
        _expected_revision: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        Err(AppError::unsupported("设置 UI fixture 不写文档"))
    }

    /// 不播种或更新外部 Agent 指南，保持 UI 测试服务范围只含设置快照。
    fn configuration_guide_status(&self) -> ConfigurationGuideStatus {
        ConfigurationGuideStatus::Current
    }
}

#[async_trait]
impl ProfileRepository for MemorySettingsRepository {
    /// 两种读取接口共用内存快照，确保测试与机器上的本地主机资料隔离。
    async fn list_profiles(&self) -> AppResult<Vec<kerminal_domain::preferences::TerminalProfile>> {
        Ok(self
            .list_versioned_profiles()
            .await?
            .into_iter()
            .map(|row| row.profile)
            .collect())
    }

    /// 返回隔离仓储中的实际版本，供设置页和主机弹框共同读取。
    async fn list_versioned_profiles(&self) -> AppResult<Vec<VersionedProfile>> {
        Ok(self
            .state
            .lock()
            .expect("settings state is not poisoned")
            .profiles
            .clone())
    }

    /// 按稳定 ID 查找同一个测试仓储，避免测试路径依赖另一份假数据。
    async fn get_profile(&self, id: &str) -> AppResult<Option<VersionedProfile>> {
        Ok(self
            .state
            .lock()
            .expect("settings state is not poisoned")
            .profiles
            .iter()
            .find(|row| row.profile.id == id)
            .cloned())
    }

    /// 模拟新建和修改的 CAS，未知回执发生在真实内存快照更新之后。
    async fn save_profile(
        &self,
        draft: ProfileDraft,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedProfile> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        let id = draft
            .id
            .clone()
            .expect("profile editor supplies a stable ID");
        let index = state.profiles.iter().position(|row| row.profile.id == id);
        if index.map(|index| state.profiles[index].revision.clone()) != expected_revision {
            return Err(AppError::conflict("Profile revision changed"));
        }
        let is_default = index.is_some_and(|index| state.profiles[index].profile.is_default);
        state.next_revision += 1;
        state.profile_saves += 1;
        let row = VersionedProfile {
            profile: kerminal_domain::preferences::TerminalProfile {
                id,
                name: draft.name,
                shell: draft.shell,
                args: draft.args,
                cwd: draft.cwd,
                env: draft.env,
                is_default,
                sidebar_group_id: draft.sidebar_group_id,
                sort_order: draft.sort_order,
                created_at: "test".into(),
                updated_at: "test".into(),
            },
            revision: ConfigRevision::from_sha256(format!("profile-ui-{}", state.next_revision)),
        };
        if let Some(index) = index {
            state.profiles[index] = row.clone();
        } else {
            state.profiles.push(row.clone());
        }
        if std::mem::take(&mut state.unknown_after_profile_save) {
            return Err(AppError::outcome_unknown("Profile save outcome unknown"));
        }
        Ok(row)
    }

    /// 按原 revision 删除隔离 Profile，令确认、取消与实际结果可走完整 UI 路径。
    ///
    /// @author kongweiguang
    async fn delete_profile(&self, id: &str, expected_revision: ConfigRevision) -> AppResult<bool> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        let Some(index) = state.profiles.iter().position(|row| row.profile.id == id) else {
            return Ok(false);
        };
        if state.profiles[index].revision != expected_revision {
            return Err(AppError::conflict("Profile revision changed"));
        }
        state.profiles.remove(index);
        Ok(true)
    }

    /// 对完整列表 revision 做 CAS，模拟成功后回执未知的可恢复事务。
    async fn set_default_profile(
        &self,
        id: &str,
        expected_revisions: Vec<ProfileRevision>,
    ) -> AppResult<VersionedProfile> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        if expected_revisions.len() != state.profiles.len()
            || state.profiles.iter().any(|row| {
                !expected_revisions.iter().any(|revision| {
                    revision.profile_id == row.profile.id && revision.revision == row.revision
                })
            })
        {
            return Err(AppError::conflict("Profile revisions changed"));
        }
        if !state.profiles.iter().any(|row| row.profile.id == id) {
            return Err(AppError::not_found("Profile missing"));
        }
        for row in &mut state.profiles {
            row.profile.is_default = row.profile.id == id;
        }
        let row = state
            .profiles
            .iter()
            .find(|row| row.profile.id == id)
            .expect("validated profile")
            .clone();
        if std::mem::take(&mut state.unknown_after_default_save) {
            return Err(AppError::outcome_unknown("Default profile outcome unknown"));
        }
        Ok(row)
    }
}

struct TestDefaultShell;

impl DefaultLocalShellProvider for TestDefaultShell {
    /// 只有 Profile 页解析默认 shell 才会调用此方法；返回固定虚构程序避免探测真实机器。
    fn resolve_default(&self) -> AppResult<LocalSessionTarget> {
        Ok(LocalSessionTarget {
            program: OsString::from("kerminal-settings-test-shell"),
            args: Vec::new(),
            working_directory: None,
            environment: BTreeMap::new(),
        })
    }
}

/// 只实现来源切换的目录端口；目录内容返回空，避免设置测试依赖网络或缓存格式。
#[derive(Default)]
struct SourceOnlyCatalog {
    source_url: Mutex<String>,
}

#[async_trait]
impl CatalogSource for SourceOnlyCatalog {
    /// 空串按官方默认回显，与设置快照“留空 = 官方”的语义保持一致。
    fn source_url(&self) -> String {
        let url = self.source_url.lock().unwrap().clone();
        if url.is_empty() {
            DEFAULT_PLUGIN_CATALOG_URL.to_owned()
        } else {
            url
        }
    }

    /// 真实保存切换结果并复用 application 的 URL 校验，非法地址显式失败。
    fn set_source_url(&self, source_url: &str) -> AppResult<()> {
        let trimmed = source_url.trim();
        if !trimmed.is_empty() {
            // 公开错误文案只能来自源码常量，测试也不把动态输入拼进错误。
            kerminal_plugin_protocol::validate_asset_url(trimmed)
                .map_err(|_| AppError::invalid("测试目录地址无效"))?;
        }
        *self.source_url.lock().unwrap() = trimmed.to_owned();
        Ok(())
    }

    async fn cached(&self) -> AppResult<Option<CatalogSnapshot>> {
        Ok(None)
    }

    async fn readme(
        &self,
        _id: &PluginId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<String>> {
        Ok(None)
    }

    /// 设置来源测试不展示插件图标，固定返回空让页面走统一占位。
    ///
    /// @author kongweiguang
    async fn icon(
        &self,
        _id: &PluginId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<Arc<[u8]>>> {
        Ok(None)
    }

    /// 设置来源测试不访问网络；失败结果足以让页面进入可操作的离线状态。
    async fn refresh(&self, _cancellation: CancellationToken) -> AppResult<CatalogSnapshot> {
        Err(AppError::invalid("测试不请求目录"))
    }
}

/// 来源保存测试不触碰安装与文件预检；只有权威状态和 drop-in 观察参与页面激活。
struct SourceOnlyStore;

#[async_trait]
impl PluginPackageStore for SourceOnlyStore {
    async fn state(&self) -> AppResult<PluginStoreState> {
        Ok(PluginStoreState::default())
    }

    async fn install_catalog(
        &self,
        _selection: CatalogSelection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("来源测试不安装目录包"))
    }

    async fn inspect_file(
        &self,
        _path: std::path::PathBuf,
        _cancellation: CancellationToken,
    ) -> AppResult<LocalPackageInspection> {
        Err(AppError::unsupported("来源测试不预检文件"))
    }

    async fn install_file(
        &self,
        _path: std::path::PathBuf,
        _inspected: LocalPackageInspection,
        _progress: watch::Sender<InstallProgress>,
        _cancellation: CancellationToken,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("来源测试不安装文件"))
    }

    async fn change(&self, _change: ExtensionChange) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("来源测试不修改插件"))
    }

    async fn inspect_drop_ins(
        &self,
        _cancellation: CancellationToken,
    ) -> AppResult<Vec<DropInInspection>> {
        Ok(Vec::new())
    }

    async fn confirm_drop_in(
        &self,
        _id: PluginId,
        _expected_fingerprint: String,
        _expected_revision: u64,
    ) -> AppResult<ExtensionReceipt> {
        Err(AppError::unsupported("来源测试不确认 drop-in"))
    }

    async fn delete_data(&self, _id: PluginId) -> AppResult<()> {
        Err(AppError::unsupported("来源测试不删除数据"))
    }

    /// 设置来源测试不加载已安装图标；空结果让页面使用统一占位。
    ///
    /// @author kongweiguang
    async fn icon(
        &self,
        _id: &PluginId,
        _cancellation: CancellationToken,
    ) -> AppResult<Option<ExtensionIcon>> {
        Ok(None)
    }
}

/// 使用生产 PreferencesPanel 与隔离 application 仓储构造窗口，覆盖事件订阅和保存回执。
fn fixture(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    &mut VisualTestContext,
) {
    fixture_with_mcp(app, None)
}

/// 运行态由测试显式注入，仍使用生产面板和真实按钮，避免触碰用户机器上的 MCP listener。
fn fixture_with_mcp(
    app: &mut TestAppContext,
    mcp_runtime: Option<Arc<dyn kerminal_application::mcp_runtime::McpRuntimeControl>>,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    &mut VisualTestContext,
) {
    fixture_with_extensions_and_mcp(app, mcp_runtime, None)
}

/// 附加隔离扩展服务，验证来源保存确实经过设置草稿、仓储 CAS 与权威回读。
fn fixture_with_extensions(
    app: &mut TestAppContext,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    Arc<SourceOnlyCatalog>,
    &mut VisualTestContext,
) {
    let catalog = Arc::new(SourceOnlyCatalog::default());
    let service = Arc::new(ExtensionsService::new(
        catalog.clone(),
        Arc::new(SourceOnlyStore),
    ));
    let (runtime, panel, repository, visual) =
        fixture_with_extensions_and_mcp(app, None, Some(service));
    (runtime, panel, repository, catalog, visual)
}

/// 面板构造的唯一入口；扩展服务可选，避免所有既有设置测试都创建插件页面。
fn fixture_with_extensions_and_mcp(
    app: &mut TestAppContext,
    mcp_runtime: Option<Arc<dyn kerminal_application::mcp_runtime::McpRuntimeControl>>,
    extensions: Option<Arc<ExtensionsService>>,
) -> (
    Runtime,
    Entity<PreferencesPanel>,
    MemorySettingsRepository,
    &mut VisualTestContext,
) {
    app.update(crate::init);
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("settings UI test runtime");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let repository = MemorySettingsRepository::new();
    let preferences = PreferencesService::new(Arc::new(repository.clone()));
    let profiles = ProfileService::new(Arc::new(repository.clone()), Arc::new(TestDefaultShell));
    let services = PreferencesPanelServices {
        preferences,
        profiles,
        vault_keys: None,
        configuration_sync: None,
        configuration_changes: None,
        external_protocol: None,
        configuration_directory: None,
        mcp_runtime_available: mcp_runtime.is_some(),
        mcp_runtime,
        executor,
    };
    let mounted = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted_panel = mounted.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let panel = cx.new(|cx| {
            let panel = PreferencesPanel::new(services, window, cx);
            match extensions {
                Some(service) => panel.with_extensions(
                    Some(service),
                    kerminal_application::plugins::HOST_API_VERSION.into(),
                ),
                None => panel,
            }
        });
        *mounted_panel.borrow_mut() = Some(panel.clone());
        gpui_kit::base::Root::new(panel, window, cx)
    });
    let panel = mounted
        .borrow_mut()
        .take()
        .expect("production PreferencesPanel mounted");
    pump_runtime(&runtime, visual);
    (runtime, panel, repository, visual)
}

/// 推进 GPUI 虚拟时钟越过 autosave 防抖，再交替处理通知与 application 回执。
fn pump_runtime(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..4 {
        visual
            .cx
            .background_executor
            .advance_clock(Duration::from_millis(400));
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        draw(visual);
    }
}

/// 绘制完整帧以刷新观察注册表中的生产控件 bounds 与可见状态。
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    for _ in 0..2 {
        visual.update(|window, app| window.draw(app).clear(app));
    }
}

/// 在观察快照中按稳定 ID 查找控件，必要时选取 wrapper 内最近的输入子控件。
fn observed_element(
    visual: &mut VisualTestContext,
    selector: &str,
) -> gpui_kit::base::test_support::ElementSnapshot {
    draw(visual);
    let id: ElementId = selector.to_owned().into();
    visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id)
            .or_else(|| {
                gpui_kit::base::test_support::snapshots(window)
                    .into_iter()
                    .filter(|snapshot| snapshot.path().contains(&id))
                    .min_by_key(|snapshot| snapshot.path().len())
            })
            .unwrap_or_else(|| {
                let paths = gpui_kit::base::test_support::snapshots(window)
                    .iter()
                    .map(|snapshot| format!("{:?}", snapshot.path()))
                    .collect::<Vec<_>>()
                    .join(", ");
                panic!("production control {selector} has no observation snapshot; paths: {paths}")
            })
    })
}

/// 关键词控件必须完整进入所属滚动区，避免 Kit 的部分可见状态使中心点击落在裁剪区外。
/// 其它控件保留 snapshot 语义；无 snapshot 时只使用生产 debug selector 的真实布局 bounds。
///
/// @author kongweiguang
fn target_geometry(visual: &mut VisualTestContext, selector: &str) -> (Bounds<Pixels>, bool) {
    draw(visual);
    let id: ElementId = selector.to_owned().into();
    visual.update(|window, _| {
        let snapshot = gpui_kit::base::test_support::find(window, &[], &id).or_else(|| {
            gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .filter(|snapshot| snapshot.path().contains(&id))
                .min_by_key(|snapshot| snapshot.path().len())
        });
        if let Some(snapshot) = snapshot {
            let mut visible = snapshot.visible();
            for (container, selector) in [
                ("keyword-rule-list-scroll", "keyword-rule-list-viewport"),
                (
                    "keyword-rule-editor-content",
                    "keyword-rule-editor-viewport",
                ),
            ] {
                if snapshot.path().contains(&ElementId::from(container))
                    && let Some(clip) = window.debug_selector_bounds(selector)
                {
                    let clip = clip.intersect(&window.visual_viewport_bounds());
                    visible &= bounds_within_viewport(snapshot.bounds(), clip);
                }
            }
            return (snapshot.bounds(), visible);
        }

        let bounds = window.debug_selector_bounds(selector).unwrap_or_else(|| {
            panic!("production control {selector} has no snapshot or debug bounds")
        });
        let viewport = window.visual_viewport_bounds();
        (bounds, bounds_within_viewport(bounds, viewport))
    })
}

/// 比较真实 selector bounds 与 GPUI 可视视口，容忍分数像素布局的轻微舍入。
///
/// @author kongweiguang
fn bounds_within_viewport(bounds: Bounds<Pixels>, viewport: Bounds<Pixels>) -> bool {
    const EDGE_EPSILON: f64 = 0.75;
    bounds.left().to_f64() + EDGE_EPSILON >= viewport.left().to_f64()
        && bounds.top().to_f64() + EDGE_EPSILON >= viewport.top().to_f64()
        && bounds.right().to_f64() <= viewport.right().to_f64() + EDGE_EPSILON
        && bounds.bottom().to_f64() <= viewport.bottom().to_f64() + EDGE_EPSILON
}

/// 从生产 snapshot 或 debug selector 的实际 bounds 派发事件，避免构造替代观察快照。
///
/// @author kongweiguang
fn click(visual: &mut VisualTestContext, selector: &str) {
    let (bounds, visible) = target_geometry(visual, selector);
    assert!(visible, "production control {selector} is visible");
    let position = bounds.center();
    visual.simulate_mouse_move(position, None, Default::default());
    draw(visual);
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    draw(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    draw(visual);
}

/// 每步按当前帧重取滚轮落点，避免外层滚动后继续向旧坐标发事件；不直接改写滚动偏移。
///
/// @author kongweiguang
fn scroll_until_visible(
    visual: &mut VisualTestContext,
    selector: &str,
    anchor: &str,
    delta_y: f32,
) {
    for _ in 0..32 {
        if target_geometry(visual, selector).1 {
            return;
        }
        let bounds = target_geometry(visual, anchor).0;
        let viewport = visual.update(|window, _| window.visual_viewport_bounds());
        let clipped = bounds.intersect(&viewport);
        let position = if clipped.size.width > px(0.) && clipped.size.height > px(0.) {
            clipped.center()
        } else {
            viewport.center()
        };
        visual.simulate_event(ScrollWheelEvent {
            position,
            delta: ScrollDelta::Lines(point(0., delta_y)),
            ..Default::default()
        });
        draw(visual);
    }
    assert_rendered(visual, selector);
}

/// Kit 的 visible 允许部分相交；本用例必须把目标完整滚入生产内容裁剪区，且滚轮点保持在视口内。
/// @author kongweiguang
fn scroll_target_fully_into_content_clip(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    selector: &str,
) -> (Bounds<Pixels>, Bounds<Pixels>) {
    for _ in 0..48 {
        let bounds = target_geometry(visual, selector).0;
        let clip = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
        assert!(
            clip.size.width.to_f64() > 0. && clip.size.height.to_f64() > 0.,
            "preferences content clip must be laid out before scrolling {selector}: {clip:?}"
        );
        if bounds_within_viewport(bounds, clip) {
            return (bounds, clip);
        }

        visual.simulate_event(ScrollWheelEvent {
            position: clip.center(),
            delta: ScrollDelta::Lines(point(0., -1.)),
            ..Default::default()
        });
        draw(visual);
    }

    let bounds = target_geometry(visual, selector).0;
    let clip = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
    panic!("{selector} did not fully enter content clip: bounds={bounds:?}, clip={clip:?}");
}

/// 通过输入事件替换规则字段；滚动只在目标被裁剪时发生，保留正常控件焦点路径。
fn replace_keyword_field(
    visual: &mut VisualTestContext,
    selector: &str,
    anchor: &str,
    delta_y: f32,
    value: &str,
) {
    scroll_until_visible(visual, selector, anchor, delta_y);
    click(visual, selector);
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input(value);
    draw(visual);
}

/// 使用生产按钮和输入控件创建已保存规则，确保测试覆盖新增规则的完整 UI 流程。
fn create_keyword_rule(
    runtime: &Runtime,
    visual: &mut VisualTestContext,
    pattern: &str,
    note: &str,
    previous_anchor: Option<&str>,
) {
    if let Some(anchor) = previous_anchor {
        scroll_until_visible(visual, "keyword-new", anchor, 8.);
    }
    click(visual, "keyword-new");
    replace_keyword_field(visual, "keyword-pattern-field", "keyword-new", -8., pattern);
    replace_keyword_field(
        visual,
        "keyword-note-field",
        "keyword-pattern-field",
        -8.,
        note,
    );
    scroll_until_visible(visual, "keyword-save", "keyword-note-field", -8.);
    click(visual, "keyword-save");
    pump_runtime(runtime, visual);
}

/// 断言 snapshot 的裁剪可见状态，或无 snapshot 控件的真实 debug bounds 位于窗口视口内。
///
/// @author kongweiguang
fn assert_rendered(visual: &mut VisualTestContext, selector: &str) {
    let (_, visible) = target_geometry(visual, selector);
    assert!(
        visible,
        "expected production control {selector} to be visible"
    );
}

/// 确认折叠后目标控件及其后代都已从当前 GPUI 帧移除。
fn assert_not_rendered(visual: &mut VisualTestContext, selector: &str) {
    draw(visual);
    let id: ElementId = selector.to_owned().into();
    assert!(
        visual.update(|window, _| {
            !gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|snapshot| snapshot.path().contains(&id))
        }),
        "expected production control {selector} to be absent"
    );
}

/// 真实导航按钮依次挂载全部设置页面，Profile 与其它页面使用同一滚动区域。
/// @author kongweiguang
#[gpui_kit::test]
fn production_navigation_opens_all_settings_pages(app: &mut TestAppContext) {
    let (_runtime, _panel, _repository, visual) = fixture(app);
    for (navigation, control) in [
        (
            "preferences-nav-appearance",
            "settings-interface-language-select",
        ),
        ("preferences-nav-terminal", "terminal-scheme-kerminal"),
        ("preferences-nav-profiles", "profile-create"),
        ("preferences-nav-keywords", "keyword-new"),
        ("preferences-nav-suggestions", "suggestions-enabled"),
        ("preferences-nav-sftp", "preferences-sftp-advanced"),
        ("preferences-nav-external-launch", "external-launch-enabled"),
        ("preferences-nav-mcp", "mcp-settings-toggle"),
        ("preferences-nav-sync", "workspace-sync-key-save"),
        ("preferences-nav-desktop", "desktop-notifications-enabled"),
        (
            "preferences-nav-shortcuts",
            "settings-shortcut-platform-windows",
        ),
        ("preferences-nav-about", "settings-about-github"),
    ] {
        scroll_until_visible(visual, navigation, "preferences-navigation-viewport", -8.);
        click(visual, navigation);
        assert_rendered(visual, control);
    }
}

/// 用户经实际控件创建和修改同一 Profile，再切换默认项；服务读取必须观察到完整新值。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_page_creates_edits_and_selects_the_shared_default(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Test PowerShell");
    click(visual, "profile-shell-field");
    visual.simulate_input("pwsh.exe");
    scroll_until_visible(
        visual,
        "profile-save",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-save");
    pump_runtime(&runtime, visual);
    let id = {
        let state = repository.state.lock().expect("test repository");
        assert_eq!(state.profiles.len(), 1);
        assert_eq!(state.profiles[0].profile.shell, "pwsh.exe");
        state.profiles[0].profile.id.clone()
    };
    scroll_until_visible(
        visual,
        "profile-set-default",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-set-default");
    pump_runtime(&runtime, visual);
    assert!(
        repository.state.lock().expect("test repository").profiles[0]
            .profile
            .is_default
    );

    scroll_until_visible(
        visual,
        "profile-name-field",
        "preferences-profile-form-scroll",
        8.,
    );
    click(visual, "profile-name-field");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("Updated PowerShell");
    scroll_until_visible(
        visual,
        "profile-save",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-save");
    pump_runtime(&runtime, visual);
    let profiles = ProfileService::new(Arc::new(repository.clone()), Arc::new(TestDefaultShell));
    let current = runtime
        .block_on(profiles.get_profile(&id))
        .expect("read Profile")
        .expect("saved Profile");
    assert_eq!(current.profile.name, "Updated PowerShell");
    assert!(current.profile.is_default);
    let target = runtime
        .block_on(profiles.default_local_session_target())
        .expect("resolve default");
    assert_eq!(target.program, OsString::from("pwsh.exe"));
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).selected_profile_id.clone()),
        Some(id)
    );
}

/// 交付确认弹层的下一帧焦点回调，使断言覆盖生产焦点交接而不是测试直接赋值。
///
/// @author kongweiguang
fn deliver_profile_confirmation_frame(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, cx| {
        assert!(window.simulate_next_frame(cx) > 0);
        window.draw(cx).clear(cx);
    });
    visual.run_until_parked();
}

/// 两个真实按钮构成闭合的键盘路径，保证焦点不能穿过确认层触及设置正文。
///
/// @author kongweiguang
fn assert_profile_confirmation_tab_cycle(
    visual: &mut VisualTestContext,
    cancel: &gpui_kit::FocusHandle,
    confirm: &gpui_kit::FocusHandle,
) {
    assert!(visual.update(|window, _| cancel.is_focused(window)));
    for (key, expected) in [
        ("tab", confirm),
        ("tab", cancel),
        ("shift-tab", confirm),
        ("shift-tab", cancel),
    ] {
        visual.simulate_keystrokes(key);
        draw(visual);
        assert!(
            visual.update(|window, _| expected.is_focused(window)),
            "{key}"
        );
    }
}

/// 切换确认通过真实按钮打开，Escape 恢复触发前焦点并保留输入中的 Profile。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_switch_confirmation_traps_focus_and_preserves_draft(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Unfinished profile");
    draw(visual);
    click(visual, "profile-create");
    assert!(visual.update(|_, cx| panel.read(cx).profile_switch_confirmation));
    let previous = visual.update(|window, cx| window.focused(cx));
    deliver_profile_confirmation_frame(visual);
    let (cancel, confirm) = visual.update(|_, cx| {
        let focus = &panel.read(cx).profile_switch_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    assert_profile_confirmation_tab_cycle(visual, &cancel, &confirm);
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).profile_switch_confirmation));
    assert_eq!(visual.update(|window, cx| window.focused(cx)), previous);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .profile_draft
            .as_ref()
            .unwrap()
            .value()
            .name
            .clone()),
        "Unfinished profile"
    );
}

/// 监听回执移除目标时保留当前草稿，不能先丢弃输入再静默停留在原页面。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_switch_preserves_draft_when_target_disappears(app: &mut TestAppContext) {
    let (runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Target profile");
    click(visual, "profile-shell-field");
    visual.simulate_input("pwsh.exe");
    scroll_until_visible(
        visual,
        "profile-save",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-save");
    pump_runtime(&runtime, visual);
    let target_id = visual.update(|_, cx| panel.read(cx).selected_profile_id.clone().unwrap());

    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Preserved draft");
    click(visual, &format!("profile-select-{target_id}"));
    deliver_profile_confirmation_frame(visual);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.profile_rows.retain(|row| row.profile.id != target_id);
            cx.notify();
        })
    });
    click(visual, "profile-switch-discard");
    assert!(!visual.update(|_, cx| panel.read(cx).profile_switch_confirmation));
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .profile_draft
            .as_ref()
            .unwrap()
            .value()
            .name
            .clone()),
        "Preserved draft"
    );
    assert!(visual.update(|_, cx| panel.read(cx).profile_draft.as_ref().unwrap().is_dirty()));
}

/// 退出确认取消后回到原草稿；确认退出时清除已失效的 Profile 切换意图。
///
/// @author kongweiguang
#[gpui_kit::test]
fn quitting_over_profile_confirmation_restores_the_original_confirmation(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Unfinished profile");
    draw(visual);
    click(visual, "profile-create");
    deliver_profile_confirmation_frame(visual);

    visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_close(cx)));
    assert!(!visual.update(|_, cx| panel.read(cx).close_confirmation));
    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    deliver_profile_confirmation_frame(visual);
    assert_not_rendered(visual, "profile-switch-cancel");
    let (cancel, confirm) = visual.update(|_, cx| {
        let focus = &panel.read(cx).quit_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    assert_profile_confirmation_tab_cycle(visual, &cancel, &confirm);

    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(!visual.update(|_, cx| panel.read(cx).quit_confirmation));
    assert!(visual.update(|_, cx| panel.read(cx).profile_switch_confirmation));
    assert_rendered(visual, "profile-switch-cancel");
    let original_cancel = visual.update(|_, cx| {
        panel
            .read(cx)
            .profile_switch_confirmation_focus
            .keep_editing
            .clone()
    });
    assert!(visual.update(|window, _| original_cancel.is_focused(window)));
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .profile_draft
            .as_ref()
            .unwrap()
            .value()
            .name
            .clone()),
        "Unfinished profile"
    );

    assert!(!visual.update(|_, cx| panel.update(cx, |panel, cx| panel.request_quit(cx))));
    deliver_profile_confirmation_frame(visual);
    click(visual, "preferences-quit-confirm");
    assert!(!visual.update(|_, cx| panel.read(cx).profile_switch_confirmation));
    assert!(visual.update(|_, cx| panel.read(cx).pending_profile_selection.is_none()));
    assert_not_rendered(visual, "profile-switch-cancel");
}

/// 重读确认关闭后不丢弃草稿，并能从原触发器再次打开和取消。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_reload_confirmation_preserves_draft_and_can_reopen(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Reload draft");
    draw(visual);
    for _ in 0..2 {
        click(visual, "profile-reload");
        assert!(visual.update(|_, cx| panel.read(cx).profile_reload_confirmation));
        deliver_profile_confirmation_frame(visual);
        let (cancel, confirm) = visual.update(|_, cx| {
            let focus = &panel.read(cx).profile_reload_confirmation_focus;
            (focus.keep_editing.clone(), focus.discard.clone())
        });
        assert_profile_confirmation_tab_cycle(visual, &cancel, &confirm);
        visual.simulate_keystrokes("escape");
        draw(visual);
        assert!(!visual.update(|_, cx| panel.read(cx).profile_reload_confirmation));
        assert_eq!(
            visual.update(|_, cx| panel
                .read(cx)
                .profile_draft
                .as_ref()
                .unwrap()
                .value()
                .name
                .clone()),
            "Reload draft"
        );
    }
}

/// 删除确认先给取消焦点，目标变化时不提交；重新确认后才删除原 Profile。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_delete_confirmation_traps_focus_and_requires_explicit_confirmation(
    app: &mut TestAppContext,
) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Profile to delete");
    click(visual, "profile-shell-field");
    visual.simulate_input("pwsh.exe");
    scroll_until_visible(
        visual,
        "profile-save",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-save");
    pump_runtime(&runtime, visual);
    assert_eq!(repository.state.lock().unwrap().profiles.len(), 1);

    click(visual, "profile-delete");
    deliver_profile_confirmation_frame(visual);
    let (cancel, confirm) = visual.update(|_, cx| {
        let focus = &panel.read(cx).profile_delete_confirmation_focus;
        (focus.keep_editing.clone(), focus.discard.clone())
    });
    assert_profile_confirmation_tab_cycle(visual, &cancel, &confirm);
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert_eq!(repository.state.lock().unwrap().profiles.len(), 1);

    let original_id = visual.update(|_, cx| panel.read(cx).selected_profile_id.clone());
    click(visual, "profile-delete");
    deliver_profile_confirmation_frame(visual);
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.selected_profile_id = Some("changed-profile".to_owned());
            cx.notify();
        });
    });
    click(visual, "profile-delete-confirm");
    assert!(!visual.update(|_, cx| panel.read(cx).profile_operation_running));
    assert_eq!(repository.state.lock().unwrap().profiles.len(), 1);
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).status.clone()),
        Some(
            crate::locale::text(
                "Profile 已变化，请核对后重新删除。",
                "The profile changed. Review it before deleting again.",
            )
            .to_string()
        )
    );
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.selected_profile_id = original_id;
            cx.notify();
        });
    });

    click(visual, "profile-delete");
    deliver_profile_confirmation_frame(visual);
    click(visual, "profile-delete-confirm");
    pump_runtime(&runtime, visual);
    assert!(repository.state.lock().unwrap().profiles.is_empty());
    assert!(!visual.update(|_, cx| panel.read(cx).profile_delete_confirmation));
}

/// 未知保存回执须由用户触发读回；匹配时确认，不重放写入。
///
/// @author kongweiguang
#[gpui_kit::test]
fn profile_unknown_save_requires_matching_readback(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-profiles");
    click(visual, "profile-create");
    click(visual, "profile-name-field");
    visual.simulate_input("Unknown result");
    click(visual, "profile-shell-field");
    visual.simulate_input("cmd.exe");
    repository
        .state
        .lock()
        .expect("test repository")
        .unknown_after_profile_save = true;
    scroll_until_visible(
        visual,
        "profile-save",
        "preferences-profile-form-scroll",
        -8.,
    );
    click(visual, "profile-save");
    pump_runtime(&runtime, visual);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .profile_draft
            .as_ref()
            .map(|draft| draft.status())),
        Some(super::DraftStatus::OutcomeUnknown)
    );
    click(visual, "profile-reload");
    pump_runtime(&runtime, visual);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .profile_draft
            .as_ref()
            .map(|draft| draft.status())),
        Some(super::DraftStatus::Ready)
    );
    assert_eq!(
        repository
            .state
            .lock()
            .expect("test repository")
            .profile_saves,
        1
    );
}

/// 搜索输入保持当前正文；点选 SFTP 结果后切页、定位并清除过滤条件。
/// @author kongweiguang
#[gpui_kit::test]
fn search_changes_page_only_after_a_real_result_click(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    assert_rendered(visual, "settings-interface-language-select");

    click(visual, "preferences-search");
    visual.simulate_input("SFTP");
    draw(visual);
    assert_rendered(visual, "settings-interface-language-select");
    assert_rendered(visual, "preferences-search-clear");

    click(visual, "preferences-search-clear");
    assert!(visual.update(|_, cx| { panel.read(cx).navigation_search_query.is_empty() }));
    click(visual, "preferences-search");
    visual.simulate_input("SFTP");
    draw(visual);
    assert_rendered(visual, "preferences-nav-settings-sftp-panel");

    click(visual, "preferences-nav-settings-sftp-panel");
    assert_rendered(visual, "preferences-sftp-advanced");
    assert_rendered(visual, "preferences-nav-appearance");
    assert!(visual.update(|_, cx| { panel.read(cx).navigation_search_query.is_empty() }));
}

/// 不同窗口尺寸都能选择统一终端主题，旧 renderer 控件不再出现在生产页面。
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_theme_selector_works_without_renderer_disclosure(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);

    for window_size in [size(px(1440.), px(900.)), size(px(800.), px(600.))] {
        visual.simulate_resize(window_size);
        draw(visual);
        click(visual, "preferences-nav-appearance");
        click(visual, "preferences-nav-terminal");
        visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
        draw(visual);

        assert_not_rendered(visual, "terminal-renderer-disclosure");
        scroll_target_fully_into_content_clip(visual, &panel, "terminal-scheme-github");
        click(visual, "terminal-scheme-github");
        pump_runtime(&runtime, visual);
        assert_eq!(
            repository.snapshot().0.terminal.color_scheme,
            kerminal_application::preferences::TerminalColorScheme::Github
        );
    }
}

/// 观察预览事件而非只比较最终布尔值，防止开关与整行冒泡各触发一次却恰好写入同一值。
struct TerminalPreviewObserver {
    _subscription: gpui_kit::Subscription,
}

/// 指针直接点击、行点击和键盘激活各只能产生一次预览，并在防抖后写入同一设置仓储。
///
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_selection_copy_switch_row_and_keyboard_activate_once(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");

    let previews = std::rc::Rc::new(std::cell::Cell::new(0));
    let _observer = visual.update(|_, cx| {
        let previews = previews.clone();
        cx.new(|cx| {
            let subscription = cx.subscribe(
                &panel,
                move |_: &mut TerminalPreviewObserver, _, event: &super::PreferencesEvent, _| {
                    if matches!(event, super::PreferencesEvent::SettingsPreview(_)) {
                        previews.set(previews.get() + 1);
                    }
                },
            );
            TerminalPreviewObserver {
                _subscription: subscription,
            }
        })
    });

    let original = repository.snapshot().0.terminal.selection_copy;
    click(visual, "terminal-selection-copy");
    assert_eq!(previews.get(), 1, "switch click emits exactly one preview");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .selection_copy),
        !original
    );
    assert_eq!(
        observed_element(visual, "terminal-selection-copy").focused(),
        Some(true)
    );
    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.terminal.selection_copy, !original);

    // 开关在释放 Space 时提交，测试完整按键序列并保留真实焦点路径。
    let keystroke = gpui_kit::Keystroke::parse("space").unwrap();
    visual.simulate_event(gpui_kit::KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(gpui_kit::KeyUpEvent { keystroke });
    draw(visual);
    assert_eq!(previews.get(), 2, "keyboard emits exactly one preview");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .selection_copy),
        original
    );
    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.terminal.selection_copy, original);

    let row = visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-selection-copy-row")
            .expect("selection copy row exposes its actual bounds")
    });
    let switch = target_geometry(visual, "terminal-selection-copy").0;
    let position = point(row.left() + px(8.), row.center().y);
    assert!(row.contains(&position) && !switch.contains(&position));
    visual.simulate_mouse_move(position, None, Default::default());
    draw(visual);
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    draw(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    draw(visual);
    assert_eq!(previews.get(), 3, "row click emits exactly one preview");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .selection_copy),
        !original
    );
    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.terminal.selection_copy, !original);
}

/// 光标形态和闪烁必须进入同一草稿，避免连续修改时较早的保存回执覆盖另一项。
///
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_cursor_choices_and_blink_share_persisted_draft(app: &mut TestAppContext) {
    use kerminal_application::preferences::TerminalCursorStyle;

    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    let original_blink = repository.snapshot().0.terminal.cursor_blink;
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-cursor-blink");
    click(visual, "terminal-cursor-blink");

    for (selector, style) in [
        ("terminal-cursor-block", TerminalCursorStyle::Block),
        ("terminal-cursor-bar", TerminalCursorStyle::Bar),
        ("terminal-cursor-underline", TerminalCursorStyle::Underline),
    ] {
        visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
        draw(visual);
        scroll_target_fully_into_content_clip(visual, &panel, selector);
        click(visual, selector);
        let draft = visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().terminal);
        assert_eq!(draft.cursor_style, style);
        assert_eq!(draft.cursor_blink, !original_blink);
        pump_runtime(&runtime, visual);
        let saved = repository.snapshot().0.terminal;
        assert_eq!(saved.cursor_style, style);
        assert_eq!(saved.cursor_blink, !original_blink);
    }
}

/// 通过真实设置控件验证两种模式都保存到共享仓储，避免开关只改变界面状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn sidebar_assistant_command_switch_persists_both_modes(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    let switch = "terminal-agent-commands-in-terminal";
    assert!(repository.snapshot().0.terminal.agent_commands_in_terminal);

    for expected in [false, true] {
        scroll_target_fully_into_content_clip(visual, &panel, switch);
        click(visual, switch);
        pump_runtime(&runtime, visual);
        assert_eq!(
            repository.snapshot().0.terminal.agent_commands_in_terminal,
            expected
        );
    }
}

/// 只读恢复期间拒绝终端控件操作并保留已有草稿；恢复写入后明确编辑才重新提交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_switch_preserves_draft_during_read_only_recovery(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");
    let original = repository.snapshot().0.terminal.selection_copy;
    click(visual, "terminal-selection-copy");

    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.recovery.mode = ConfigurationMode::ReadOnlyRecovery;
            cx.notify();
        });
    });
    visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
    draw(visual);
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");
    click(visual, "terminal-selection-copy");
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .selection_copy),
        !original
    );
    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.terminal.selection_copy, original);

    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.recovery.mode = ConfigurationMode::ReadWrite;
            cx.notify();
        });
    });
    visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
    draw(visual);
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");
    click(visual, "terminal-selection-copy");
    click(visual, "terminal-selection-copy");
    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.terminal.selection_copy, !original);
}

/// 数值控件同时有加减按钮和输入框，按父 ID 查找真实输入叶子以免点击到步进按钮。
///
/// @author kongweiguang
fn click_terminal_number_input(
    visual: &mut VisualTestContext,
    panel: &Entity<PreferencesPanel>,
    selector: &str,
) -> Bounds<Pixels> {
    visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
    draw(visual);
    scroll_target_fully_into_content_clip(visual, panel, selector);
    let selector_id = ElementId::from(selector.to_owned());
    let bounds = visual.update(|window, _| {
        let inputs = gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|snapshot| {
                snapshot.path().contains(&selector_id)
                    && matches!(
                        snapshot.path().last(),
                        Some(ElementId::Name(name) | ElementId::NamedInteger(name, _))
                            if name.as_ref() == "input"
                    )
            })
            .collect::<Vec<_>>();
        assert_eq!(inputs.len(), 1, "{selector} must own one text input");
        assert!(inputs[0].visible());
        inputs[0].bounds()
    });
    let position = bounds.center();
    visual.simulate_mouse_move(position, None, Default::default());
    draw(visual);
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    draw(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    draw(visual);
    bounds
}

/// 格式错误和超出范围都必须在字段旁可见并阻止提交；修正后保留的其他设置一起保存。
///
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_invalid_numbers_remain_visible_and_recover(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    let (original, initial_saves) = repository.snapshot();
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");
    click(visual, "terminal-selection-copy");

    let font_bounds = click_terminal_number_input(visual, &panel, "terminal-font-size");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("abc");
    draw(visual);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_input_errors
            .contains_key("font_size")
    }));
    let error_bounds = visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-font_size-error")
            .expect("font-size error is rendered beside its input")
    });
    assert!(error_bounds.top() >= font_bounds.bottom());
    assert!(visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-save-status")
            .is_some()
    }));
    pump_runtime(&runtime, visual);
    let (saved, saves) = repository.snapshot();
    assert_eq!(saves, initial_saves);
    assert_eq!(
        saved.terminal.selection_copy,
        original.terminal.selection_copy
    );
    assert_eq!(saved.terminal.font_size, original.terminal.font_size);
    assert_eq!(
        visual.update(|_, cx| panel
            .read(cx)
            .settings_snapshot()
            .unwrap()
            .terminal
            .selection_copy),
        !original.terminal.selection_copy
    );

    visual.simulate_input("18");
    draw(visual);
    assert!(!visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_input_errors
            .contains_key("font_size")
    }));
    assert!(visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-font_size-error")
            .is_none()
    }));
    pump_runtime(&runtime, visual);
    let (saved, saves) = repository.snapshot();
    assert_eq!(saves, initial_saves + 1);
    assert_eq!(saved.terminal.font_size, 18);
    assert_eq!(
        saved.terminal.selection_copy,
        !original.terminal.selection_copy
    );

    let line_bounds = click_terminal_number_input(visual, &panel, "terminal-line-height");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("2.0");
    draw(visual);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_issues
            .iter()
            .any(|issue| issue.kind == super::SettingsIssueKind::InvalidLineHeight)
    }));
    let error_bounds = visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-line_height-error")
            .expect("line-height error is rendered beside its input")
    });
    assert!(error_bounds.top() >= line_bounds.bottom());
    pump_runtime(&runtime, visual);
    let (still_saved, still_saves) = repository.snapshot();
    assert_eq!(still_saves, saves);
    assert_eq!(still_saved.terminal.line_height, saved.terminal.line_height);

    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("1.5");
    draw(visual);
    assert!(visual.update(|window, _| {
        window
            .debug_selector_bounds("terminal-line_height-error")
            .is_none()
    }));
    pump_runtime(&runtime, visual);
    let (saved, final_saves) = repository.snapshot();
    assert_eq!(final_saves, saves + 1);
    assert_eq!(saved.terminal.line_height, 1.5);
}

/// 模拟真实仓储失败后从固定状态栏重试，必须保留原草稿并只再提交一次。
///
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_failed_save_retains_draft_and_retry_persists(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    scroll_target_fully_into_content_clip(visual, &panel, "terminal-selection-copy");
    let (original, initial_saves) = repository.snapshot();
    let initial_attempts = repository.save_attempt_count();
    repository.fail_next_settings_save();
    click(visual, "terminal-selection-copy");
    pump_runtime(&runtime, visual);

    assert_eq!(repository.save_attempt_count(), initial_attempts + 1);
    assert_eq!(repository.snapshot().1, initial_saves);
    assert_eq!(
        repository.snapshot().0.terminal.selection_copy,
        original.terminal.selection_copy
    );
    assert!(visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        draft.status() == super::DraftStatus::Failed
            && draft.is_dirty()
            && draft.value().terminal.selection_copy != original.terminal.selection_copy
    }));
    assert_rendered(visual, "terminal-retry-save");

    click(visual, "terminal-retry-save");
    pump_runtime(&runtime, visual);
    assert_eq!(repository.save_attempt_count(), initial_attempts + 2);
    let (saved, final_saves) = repository.snapshot();
    assert_eq!(final_saves, initial_saves + 1);
    assert_eq!(
        saved.terminal.selection_copy,
        !original.terminal.selection_copy
    );
    assert!(visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        draft.status() == super::DraftStatus::Ready && !draft.is_dirty()
    }));
    assert_not_rendered(visual, "terminal-retry-save");
}

/// 生产视图密度项必须对应 main 的纵向全宽选项，避免退化为水平分段按钮。
/// @author kongweiguang
#[gpui_kit::test]
fn appearance_density_options_stack_full_width_in_main_order(app: &mut TestAppContext) {
    let (_runtime, _panel, _repository, visual) = fixture(app);
    let bounds = ["density-compact", "density-comfortable", "density-spacious"]
        .map(|selector| observed_element(visual, selector).bounds());

    assert_eq!(bounds[0].origin.x, bounds[1].origin.x);
    assert_eq!(bounds[1].origin.x, bounds[2].origin.x);
    assert_eq!(bounds[0].size.width, bounds[1].size.width);
    assert_eq!(bounds[1].size.width, bounds[2].size.width);
    assert!(bounds[0].origin.y < bounds[1].origin.y);
    assert!(bounds[1].origin.y < bounds[2].origin.y);
    assert!(bounds[0].origin.y + bounds[0].size.height <= bounds[1].origin.y);
    assert!(bounds[1].origin.y + bounds[1].size.height <= bounds[2].origin.y);
}

/// 外观主题按钮先更新生产面板，再由 application CAS 保存，仓储回读必须得到同一值。
/// @author kongweiguang
#[gpui_kit::test]
fn appearance_theme_click_is_applied_and_persisted(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "theme-light");

    let in_memory = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .theme_mode
    });
    assert_eq!(in_memory, super::ThemeMode::Light);

    pump_runtime(&runtime, visual);
    let (persisted, saves) = repository.snapshot();
    assert!(saves >= 1, "the production save use case must be called");
    assert_eq!(persisted.theme_mode, super::ThemeMode::Light);
}

/// 连续选择应用色板保留最后一个草稿并通过真实保存端口写入，不改变已有终端配置。
///
/// @author kongweiguang
#[gpui_kit::test]
fn app_color_scheme_selection_autosaves_without_changing_terminal(app: &mut TestAppContext) {
    use kerminal_application::preferences::AppColorScheme;

    let (runtime, panel, repository, visual) = fixture(app);
    let terminal_before = repository.snapshot().0.terminal;
    for selector in ["app-color-fleet", "app-color-claude", "app-color-vscode"] {
        scroll_target_fully_into_content_clip(visual, &panel, selector);
        click(visual, selector);
    }

    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::VsCode
    );
    pump_runtime(&runtime, visual);
    let (saved, saves) = repository.snapshot();
    assert!(saves >= 1);
    assert_eq!(saved.app_color_scheme, AppColorScheme::VsCode);
    assert_eq!(saved.terminal, terminal_before);
}

/// 在生产单选组内发送方向键，验证焦点导航会选择下一项并在末项循环到首项。
///
/// @author kongweiguang
#[gpui_kit::test]
fn app_color_scheme_keyboard_navigation_changes_selection(app: &mut TestAppContext) {
    use kerminal_application::preferences::AppColorScheme;

    let (_runtime, panel, _repository, visual) = fixture(app);
    // Base 单选的 Space 在按键释放时提交；dispatch_keystroke 只产生按下，必须派发完整物理序列。
    let press = |visual: &mut VisualTestContext, key: &str| {
        let keystroke = gpui_kit::Keystroke::parse(key).unwrap();
        visual.simulate_event(gpui_kit::KeyDownEvent {
            keystroke: keystroke.clone(),
            is_held: false,
            prefer_character_input: false,
        });
        visual.simulate_event(gpui_kit::KeyUpEvent { keystroke });
        draw(visual);
    };
    let radio_focused = |visual: &mut VisualTestContext, name: &str| {
        draw(visual);
        visual.update(|window, _| {
            gpui_kit::base::test_support::snapshots(window)
                .iter()
                .any(|snapshot| {
                    snapshot.role() == Some(gpui_kit::Role::RadioButton)
                        && snapshot.label().is_some_and(|label| label.ends_with(name))
                        && snapshot.focused() == Some(true)
                })
        })
    };

    // 独立面板夹具没有窗口级初始焦点；先点击真实输入，再从其 dispatch path 走 Tab。
    click(visual, "preferences-search");
    // Radio 点击保留原焦点；只使用真实 Tab 路径进入目标单选项。
    let mut reached_jetbrains = false;
    for _ in 0..128 {
        if radio_focused(visual, "JetBrains") {
            reached_jetbrains = true;
            break;
        }
        press(visual, "tab");
    }
    assert!(reached_jetbrains, "Tab must reach the JetBrains radio");
    press(visual, "space");
    draw(visual);
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::JetBrains
    );

    press(visual, "right");
    draw(visual);
    assert!(radio_focused(visual, "Fleet"));
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::Fleet
    );

    press(visual, "right");
    press(visual, "right");
    draw(visual);
    assert!(radio_focused(visual, "Claude"));
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::Claude
    );

    press(visual, "right");
    draw(visual);
    assert!(radio_focused(visual, "VS Code"));
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::VsCode
    );

    press(visual, "right");
    draw(visual);
    assert!(radio_focused(visual, "Kerminal"));
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::Kerminal
    );

    press(visual, "tab");
    draw(visual);
    assert!(radio_focused(visual, "JetBrains"));
    press(visual, "space");
    draw(visual);
    assert_eq!(
        visual.update(|_, cx| panel.read(cx).settings_snapshot().unwrap().app_color_scheme),
        AppColorScheme::JetBrains
    );
}

/// 缩略卡随设置正文宽度均分为三列两行：不越过内容区两侧，且末列贴住整行控件右边界。
///
/// @author kongweiguang
#[gpui_kit::test]
fn app_color_scheme_choices_fit_the_settings_content_width(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    for window_size in [size(px(720.), px(600.)), size(px(1440.), px(900.))] {
        visual.simulate_resize(window_size);
        draw(visual);
        let clip = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
        // 同一表单里整行控件（语言选择框）的右边界即该列应有边界。
        let column = target_geometry(visual, "settings-interface-language-select").0;
        let mut widest_right = f64::MIN;
        let mut leftmost = f64::MAX;
        let mut widths = Vec::new();
        let mut rows = Vec::new();
        for selector in [
            "app-color-kerminal",
            "app-color-jetbrains",
            "app-color-fleet",
            "app-color-xcode",
            "app-color-claude",
            "app-color-vscode",
        ] {
            let bounds = target_geometry(visual, selector).0;
            assert!(
                bounds.left().to_f64() + 0.75 >= clip.left().to_f64()
                    && bounds.right().to_f64() <= clip.right().to_f64() + 0.75,
                "{selector} exceeds the content width at {window_size:?}: {bounds:?}, {clip:?}"
            );
            widest_right = widest_right.max(bounds.right().to_f64());
            leftmost = leftmost.min(bounds.left().to_f64());
            widths.push(bounds.size.width.to_f64());
            let top = bounds.top().to_f64();
            if !rows.iter().any(|row: &f64| (row - top).abs() <= 0.75) {
                rows.push(top);
            }
        }
        // 三列按行均分剩余宽度：最右列必须贴住整行容器的右边界，否则会出现俗称的
        // “配色卡片右侧留白”，与上方整行控件错位。
        assert!(
            (leftmost - column.left().to_f64()).abs() <= 0.75
                && (widest_right - column.right().to_f64()).abs() <= 0.75,
            "palette columns do not fill the row at {window_size:?}: left {leftmost}, right {widest_right}, column {column:?}"
        );
        // 六个色板固定排成三列两行；列宽一致才不会再出现末列被挤窄的观感。
        let narrowest = widths.iter().copied().fold(f64::MAX, f64::min);
        let widest = widths.iter().copied().fold(f64::MIN, f64::max);
        assert!(
            rows.len() == 2 && widest - narrowest <= 0.75,
            "palette columns are uneven at {window_size:?}: widths {widths:?}, rows {rows:?}"
        );
    }
}

/// main 保留关闭背景后的配置入口；开关切换仍即时应用并写回真实设置端口。
/// @author kongweiguang
#[gpui_kit::test]
fn background_controls_remain_available_and_toggle_persists(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    let initial_enabled = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .background_enabled
    });
    assert!(
        !initial_enabled,
        "fixture starts with the background disabled"
    );

    for selector in [
        "background-path-control",
        "preferences-choose-background",
        "background-opacity-control",
        "background-fit-controls",
        "fit-cover",
        "fit-contain",
        "fit-tile",
    ] {
        assert_rendered(visual, selector);
    }

    click(visual, "preferences-background-enabled");
    pump_runtime(&runtime, visual);
    assert!(repository.snapshot().0.background_enabled);

    click(visual, "preferences-background-enabled");
    pump_runtime(&runtime, visual);
    assert!(!repository.snapshot().0.background_enabled);
    for selector in [
        "background-path-control",
        "preferences-choose-background",
        "background-opacity-control",
        "background-fit-controls",
    ] {
        assert_rendered(visual, selector);
    }
}

/// 命令提示总开关、远端只读策略与 Provider 开关应同步到同一草稿并持久化。
/// @author kongweiguang
#[gpui_kit::test]
fn command_suggestion_policy_and_provider_changes_persist(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-suggestions");
    assert_rendered(visual, "suggestions-enabled");

    click(visual, "suggestions-enabled");
    click(visual, "suggestion-remote-probe");
    click(visual, "suggestion-provider-git");

    let edited = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .terminal
            .inline_suggestion
    });
    assert!(!edited.enabled);
    assert_eq!(
        edited.presentation,
        kerminal_application::preferences::TerminalCommandSuggestionPresentation::Off
    );
    assert!(!edited.remote_probe_enabled);
    assert_eq!(
        edited.remote_refresh,
        kerminal_application::preferences::TerminalCommandSuggestionRemoteRefresh::Off
    );
    assert!(!edited.providers.git);

    pump_runtime(&runtime, visual);
    let persisted = repository.snapshot().0.terminal.inline_suggestion;
    assert!(!persisted.enabled);
    assert_eq!(
        persisted.presentation,
        kerminal_application::preferences::TerminalCommandSuggestionPresentation::Off
    );
    assert!(!persisted.remote_probe_enabled);
    assert_eq!(
        persisted.remote_refresh,
        kerminal_application::preferences::TerminalCommandSuggestionRemoteRefresh::Off
    );
    assert!(!persisted.providers.git);
}

/// 三个命令提示 Select 通过真实键盘事件确认选项，并将值保存回设置仓储。
/// @author kongweiguang
#[gpui_kit::test]
fn command_suggestion_selects_update_and_persist_values(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-suggestions");

    click(visual, "suggestion-presentation");
    visual.simulate_keystrokes("down enter");
    draw(visual);
    let presentation = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .terminal
            .inline_suggestion
    });
    assert_eq!(
        presentation.presentation,
        kerminal_application::preferences::TerminalCommandSuggestionPresentation::Inline
    );
    assert!(presentation.enabled);

    click(visual, "suggestion-accept-key");
    visual.simulate_keystrokes("down enter");
    draw(visual);
    let accept_key = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings remain loaded")
            .terminal
            .inline_suggestion
            .accept_key
    });
    assert_eq!(
        accept_key,
        kerminal_application::preferences::TerminalInlineSuggestionAcceptKey::Disabled
    );

    click(visual, "suggestion-remote-refresh");
    visual.simulate_keystrokes("down enter");
    draw(visual);
    let remote_refresh = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings remain loaded")
            .terminal
            .inline_suggestion
    });
    assert_eq!(
        remote_refresh.remote_refresh,
        kerminal_application::preferences::TerminalCommandSuggestionRemoteRefresh::Off
    );
    assert!(!remote_refresh.remote_probe_enabled);

    pump_runtime(&runtime, visual);
    let persisted = repository.snapshot().0.terminal.inline_suggestion;
    assert_eq!(
        persisted.presentation,
        kerminal_application::preferences::TerminalCommandSuggestionPresentation::Inline
    );
    assert!(persisted.enabled);
    assert_eq!(
        persisted.accept_key,
        kerminal_application::preferences::TerminalInlineSuggestionAcceptKey::Disabled
    );
    assert_eq!(
        persisted.remote_refresh,
        kerminal_application::preferences::TerminalCommandSuggestionRemoteRefresh::Off
    );
    assert!(!persisted.remote_probe_enabled);
}

/// 关键词规则先以页面草稿编辑；取消不应把未保存的规则写入设置快照。
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_editor_cancel_discards_only_the_local_rule_draft(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    let initial_rules = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .terminal
            .keyword_highlights
            .rules
            .len()
    });
    let (_, initial_saves) = repository.snapshot();

    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    assert_rendered(visual, "keyword-pattern-field");
    click(visual, "keyword-pattern-field");
    visual.simulate_input("settings-test-pattern");
    draw(visual);
    click(visual, "keyword-cancel");

    let rules_after_cancel = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings remain loaded")
            .terminal
            .keyword_highlights
            .rules
            .len()
    });
    assert_eq!(rules_after_cancel, initial_rules);
    assert_eq!(repository.snapshot().1, initial_saves);

    click(visual, "keyword-new");
    click(visual, "keyword-pattern-field");
    visual.simulate_input("settings-test-pattern");
    draw(visual);
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);

    let (persisted, saves) = repository.snapshot();
    assert_eq!(saves, initial_saves + 1);
    assert_eq!(
        persisted.terminal.keyword_highlights.rules.len(),
        initial_rules + 1
    );
}

/// 删除第一条规则后，编辑器的实际输入值和后续保存必须继续对应剩余规则。
/// @author kongweiguang
#[gpui_kit::test]
fn deleting_keyword_rule_syncs_remaining_inputs_before_the_next_save(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-keywords");

    let first_pattern = "delete-regression-first";
    let first_note = "first rule note";
    let remaining_pattern = "delete-regression-remaining";
    let remaining_note = "remaining rule note";
    let edited_pattern = "delete-regression-edited";
    let edited_note = "edited remaining rule note";
    create_keyword_rule(&runtime, visual, first_pattern, first_note, None);
    create_keyword_rule(
        &runtime,
        visual,
        remaining_pattern,
        remaining_note,
        Some("keyword-save"),
    );

    let saved_rules = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(saved_rules.len(), 2);
    let first_id = saved_rules
        .iter()
        .find(|rule| rule.pattern == first_pattern)
        .expect("first rule was saved")
        .id
        .clone();
    let remaining_id = saved_rules
        .iter()
        .find(|rule| rule.pattern == remaining_pattern)
        .expect("remaining rule was saved")
        .id
        .clone();

    let first_row = format!("keyword-rule-{first_id}");
    scroll_until_visible(visual, &first_row, "keyword-save", 8.);
    click(visual, &first_row);
    scroll_until_visible(visual, "keyword-delete", &first_row, -8.);
    click(visual, "keyword-delete");

    let displayed_pattern = observed_element(visual, "keyword-pattern-field");
    let displayed_note = observed_element(visual, "keyword-note-field");
    assert_eq!(displayed_pattern.value(), Some(remaining_pattern));
    assert_eq!(displayed_note.value(), Some(remaining_note));

    pump_runtime(&runtime, visual);
    let after_delete = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(after_delete.len(), 1);
    assert_eq!(after_delete[0].id, remaining_id);

    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-delete",
        8.,
        edited_pattern,
    );
    replace_keyword_field(
        visual,
        "keyword-note-field",
        "keyword-pattern-field",
        -8.,
        edited_note,
    );
    scroll_until_visible(visual, "keyword-save", "keyword-note-field", -8.);
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);

    let final_rules = repository.snapshot().0.terminal.keyword_highlights.rules;
    assert_eq!(final_rules.len(), 1);
    assert_eq!(final_rules[0].id, remaining_id);
    assert_eq!(final_rules[0].pattern, edited_pattern);
    assert_eq!(final_rules[0].note, edited_note);
    assert!(!final_rules.iter().any(|rule| rule.id == first_id));
    let panel_rules = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings remain loaded")
            .terminal
            .keyword_highlights
            .rules
            .clone()
    });
    assert_eq!(panel_rules, final_rules);
}

/// 通过实际输入、拾色器状态事件和重置按钮验证四个主题色字段彼此隔离后再持久化读回。
///
/// @author kongweiguang
#[gpui_kit::test]
fn keyword_custom_colors_route_hex_picker_and_reset_to_distinct_targets(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    let initial_saves = repository.snapshot().1;
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-new",
        -8.,
        "color-routing-test",
    );
    replace_keyword_field(
        visual,
        "keyword-note-field",
        "keyword-pattern-field",
        -8.,
        "independent theme colors",
    );
    scroll_until_visible(visual, "keyword-mode-WholeWord", "keyword-note-field", -8.);
    click(visual, "keyword-mode-WholeWord");
    click(visual, "keyword-case-sensitive");
    scroll_until_visible(
        visual,
        "keyword-style-Custom",
        "keyword-case-sensitive",
        -8.,
    );
    click(visual, "keyword-style-Custom");

    replace_keyword_field(
        visual,
        "keyword-color-field-light-foreground",
        "keyword-style-Custom",
        -8.,
        "#112233",
    );
    replace_keyword_field(
        visual,
        "keyword-color-field-light-background",
        "keyword-color-field-light-foreground",
        -8.,
        "#445566",
    );
    replace_keyword_field(
        visual,
        "keyword-color-field-dark-foreground",
        "keyword-color-field-light-background",
        -8.,
        "#778899",
    );

    let dark_background_picker = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .colors
            .iter()
            .find(|field| field.target.id_part() == "dark-background")
            .expect("dark background ColorPicker exists")
            .picker
            .clone()
    });
    scroll_until_visible(
        visual,
        "keyword-color-picker-dark-background",
        "keyword-color-field-dark-foreground",
        -8.,
    );
    click(visual, "keyword-color-picker-dark-background");
    assert!(visual.update(|_, cx| dark_background_picker.read(cx).is_open()));
    let picker_color = hsla(0., 1., 0.5, 1.);
    visual.update(|window, cx| {
        dark_background_picker.update(cx, |picker, cx| {
            picker.select_color(picker_color, window, cx);
        });
    });
    draw(visual);
    let dark_background_value = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .colors
            .iter()
            .find(|field| field.target.id_part() == "dark-background")
            .expect("dark background color input exists")
            .input
            .read(cx)
            .value()
    });
    assert_eq!(dark_background_value, "#FF0000");

    scroll_until_visible(
        visual,
        "keyword-color-reset-light-foreground",
        "keyword-color-field-dark-background",
        8.,
    );
    click(visual, "keyword-color-reset-light-foreground");
    let draft = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .expect("rule editor remains open")
            .draft
            .clone()
    });
    let draft_colors = draft.custom_colors.as_ref().expect("custom colors exist");
    assert_eq!(draft_colors.light.foreground, None);
    assert_eq!(draft_colors.light.background.as_deref(), Some("#445566"));
    assert_eq!(draft_colors.dark.foreground.as_deref(), Some("#778899"));
    assert_eq!(draft_colors.dark.background.as_deref(), Some("#FF0000"));

    scroll_until_visible(
        visual,
        "keyword-save",
        "keyword-color-field-light-foreground",
        -8.,
    );
    click(visual, "keyword-save");
    pump_runtime(&runtime, visual);

    let (persisted, save_count) = repository.snapshot();
    assert_eq!(save_count, initial_saves + 1);
    let rules = &persisted.terminal.keyword_highlights.rules;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].pattern, "color-routing-test");
    assert_eq!(
        rules[0].match_mode,
        kerminal_application::preferences::TerminalKeywordHighlightMatchMode::WholeWord
    );
    assert!(rules[0].case_sensitive);
    assert_eq!(
        rules[0].style,
        kerminal_application::preferences::TerminalKeywordHighlightStyle::Custom
    );
    let saved_colors = rules[0]
        .custom_colors
        .as_ref()
        .expect("saved custom colors");
    assert_eq!(saved_colors.light.foreground, None);
    assert_eq!(saved_colors.light.background.as_deref(), Some("#445566"));
    assert_eq!(saved_colors.dark.foreground.as_deref(), Some("#778899"));
    assert_eq!(saved_colors.dark.background.as_deref(), Some("#FF0000"));
}

/// Escape 在颜色拾色器拥有焦点时只关闭最上层弹层，保留未保存的关键词编辑草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn escape_dismisses_keyword_color_picker_without_cancelling_rule_editor(app: &mut TestAppContext) {
    let (_runtime, panel, repository, visual) = fixture(app);
    let initial_saves = repository.snapshot().1;
    click(visual, "preferences-nav-keywords");
    click(visual, "keyword-new");
    replace_keyword_field(
        visual,
        "keyword-pattern-field",
        "keyword-new",
        -8.,
        "escape-preserves-draft",
    );
    scroll_until_visible(visual, "keyword-style-Custom", "keyword-pattern-field", -8.);
    click(visual, "keyword-style-Custom");
    replace_keyword_field(
        visual,
        "keyword-color-field-light-foreground",
        "keyword-style-Custom",
        -8.,
        "#AABBCC",
    );

    let dark_background_picker = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .colors
            .iter()
            .find(|field| field.target.id_part() == "dark-background")
            .expect("dark background ColorPicker exists")
            .picker
            .clone()
    });
    scroll_until_visible(
        visual,
        "keyword-color-picker-dark-background",
        "keyword-color-field-light-foreground",
        -8.,
    );
    click(visual, "keyword-color-picker-dark-background");
    assert!(visual.update(|_, cx| dark_background_picker.read(cx).is_open()));

    visual.simulate_keystrokes("escape");
    draw(visual);

    assert!(!visual.update(|_, cx| dark_background_picker.read(cx).is_open()));
    let draft = visual.update(|_, cx| {
        panel
            .read(cx)
            .keyword_editor
            .session
            .as_ref()
            .expect("Escape must leave the rule editor open")
            .draft
            .clone()
    });
    assert_eq!(draft.pattern, "escape-preserves-draft");
    assert_eq!(
        draft
            .custom_colors
            .as_ref()
            .and_then(|colors| colors.light.foreground.as_deref()),
        Some("#AABBCC")
    );
    assert_eq!(repository.snapshot().1, initial_saves);
    assert!(
        repository
            .snapshot()
            .0
            .terminal
            .keyword_highlights
            .rules
            .is_empty()
    );
}

/// 三个平台标签共用录制操作；每个标签只写入自己的字段，恢复默认也不覆盖其他平台。
/// @author kongweiguang
#[gpui_kit::test]
fn shortcut_platform_tabs_record_and_reset_their_own_binding(app: &mut TestAppContext) {
    let (runtime, _panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-shortcuts");
    for platform in ["windows", "linux", "macos"] {
        assert_rendered(visual, &format!("settings-shortcut-platform-{platform}"));
    }

    let settings_open = |persisted: &SettingsSnapshot| {
        persisted
            .keybindings
            .iter()
            .find(|binding| binding.action == "settings.open")
            .cloned()
            .expect("settings.open binding exists")
    };
    let defaults = SettingsSnapshot::default();
    let default_settings_open = settings_open(&defaults);

    for platform in ["linux", "macos"] {
        let platform_selector = format!("settings-shortcut-platform-{platform}");
        let record = format!("shortcut-record-settings.open-{platform}");
        let confirm = format!("shortcut-confirm-settings.open-{platform}");
        scroll_until_visible(visual, &platform_selector, "settings-shortcut-header", -8.);
        click(visual, &platform_selector);
        let before = settings_open(&repository.snapshot().0);
        scroll_until_visible(visual, &record, &platform_selector, -8.);
        assert_rendered(visual, &record);
        click(visual, &record);
        visual.simulate_keystrokes("ctrl-shift-f12");
        draw(visual);
        click(visual, &confirm);
        pump_runtime(&runtime, visual);

        let recorded = settings_open(&repository.snapshot().0);
        match platform {
            "linux" => {
                assert_eq!(recorded.linux_binding.as_deref(), Some("ctrl-shift-f12"));
                assert_eq!(recorded.windows_binding, before.windows_binding);
                assert_eq!(recorded.mac_binding, before.mac_binding);
            }
            _ => {
                assert_eq!(recorded.mac_binding, "ctrl-shift-f12");
                assert_eq!(recorded.windows_binding, before.windows_binding);
                assert_eq!(recorded.linux_binding, before.linux_binding);
            }
        }
    }
    assert!(repository.snapshot().1 >= 2);

    for platform in ["linux", "macos"] {
        let platform_selector = format!("settings-shortcut-platform-{platform}");
        scroll_until_visible(visual, &platform_selector, "settings-shortcut-header", -8.);
        click(visual, &platform_selector);
        let before_reset = settings_open(&repository.snapshot().0);
        click(visual, "shortcut-reset-settings.open");
        pump_runtime(&runtime, visual);
        let reset = settings_open(&repository.snapshot().0);
        match platform {
            "linux" => {
                assert_eq!(reset.linux_binding, default_settings_open.linux_binding);
                assert_eq!(reset.mac_binding, before_reset.mac_binding);
            }
            _ => {
                assert_eq!(reset.mac_binding, default_settings_open.mac_binding);
                assert_eq!(reset.linux_binding, before_reset.linux_binding);
            }
        }
        assert_eq!(reset.windows_binding, before_reset.windows_binding);
    }

    scroll_until_visible(
        visual,
        "settings-shortcut-platform-windows",
        "settings-shortcut-header",
        8.,
    );
    click(visual, "settings-shortcut-platform-windows");
    assert_rendered(visual, "shortcut-record-settings.open-windows");
}

/// 录制和追加 chord 只在确认后写入；Escape 取消后仍保留上次生效的绑定。
///
/// @author kongweiguang
#[gpui_kit::test]
fn shortcut_recording_confirms_chords_and_preserves_binding_on_cancel(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-shortcuts");
    let (platform, first, second) = if cfg!(target_os = "macos") {
        ("macos", "cmd-shift-f12", "cmd-shift-f11")
    } else if cfg!(target_os = "linux") {
        ("linux", "ctrl-shift-f12", "ctrl-shift-f11")
    } else {
        ("windows", "ctrl-shift-f12", "ctrl-shift-f11")
    };
    let record = format!("shortcut-record-settings.open-{platform}");
    let add = format!("shortcut-add-stroke-settings.open-{platform}");
    let confirm = format!("shortcut-confirm-settings.open-{platform}");
    scroll_until_visible(visual, &record, "settings-shortcut-header", -8.);
    click(visual, &record);
    visual.simulate_keystrokes(first);
    draw(visual);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .shortcut_capture
            .as_ref()
            .is_some_and(|capture| !capture.recording)
    }));
    assert_eq!(
        repository.snapshot().1,
        0,
        "recording is not a settings edit"
    );
    click(visual, &add);
    visual.simulate_keystrokes(second);
    draw(visual);
    click(visual, &confirm);
    pump_runtime(&runtime, visual);
    let selected_binding = |settings: &SettingsSnapshot| {
        let binding = settings
            .keybindings
            .iter()
            .find(|binding| binding.action == "settings.open")
            .unwrap();
        match platform {
            "macos" => binding.mac_binding.clone(),
            "linux" => binding.linux_binding.clone().unwrap_or_default(),
            _ => binding.windows_binding.clone(),
        }
    };
    let expected = format!("{first} {second}");
    assert_eq!(selected_binding(&repository.snapshot().0), expected);

    scroll_until_visible(visual, &record, "settings-shortcut-header", -8.);
    click(visual, &record);
    visual.simulate_keystrokes(first);
    visual.simulate_keystrokes("tab");
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_some()));
    visual.simulate_keystrokes("escape");
    draw(visual);
    assert!(visual.update(|_, cx| panel.read(cx).shortcut_capture.is_none()));
    assert_eq!(selected_binding(&repository.snapshot().0), expected);
}

/// 冲突组合只显示错误并保留旧绑定，不会进入设置预览或触发自动保存。
///
/// @author kongweiguang
#[gpui_kit::test]
fn shortcut_recording_rejects_existing_binding_without_saving(app: &mut TestAppContext) {
    let (_runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-shortcuts");
    let (platform, conflict) = if cfg!(target_os = "macos") {
        ("macos", "cmd-shift-t")
    } else if cfg!(target_os = "linux") {
        ("linux", "ctrl-shift-t")
    } else {
        ("windows", "ctrl-shift-t")
    };
    let record = format!("shortcut-record-settings.open-{platform}");
    let confirm = format!("shortcut-confirm-settings.open-{platform}");
    scroll_until_visible(visual, &record, "settings-shortcut-header", -8.);
    click(visual, &record);
    visual.simulate_keystrokes(conflict);
    draw(visual);
    click(visual, &confirm);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .shortcut_capture
            .as_ref()
            .is_some_and(|capture| capture.error.is_some())
    }));
    assert_eq!(repository.snapshot().1, 0);
}

/// 已知动作按真实作用域排列，GPUI 专属动作也必须出现在可编辑目录中。
/// @author kongweiguang
#[gpui_kit::test]
fn shortcut_list_matches_main_catalog_and_preserves_gpui_bindings(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    click(visual, "preferences-nav-shortcuts");

    let expected_order = [
        "settings.open",
        "settings.keybindings",
        "terminal.focus",
        "tool.agentLauncher",
        "tool.system",
        "tool.sftp",
        "tool.ports",
        "tool.snippets",
        "tool.logs",
        "terminal.closePane",
        "terminal.newTab",
        "terminal.closeTab",
        "terminal.splitHorizontal",
        "terminal.splitVertical",
        "terminal.previousTab",
        "terminal.nextTab",
    ];
    let row_tops = expected_order
        .iter()
        .map(|action| {
            observed_element(visual, &format!("shortcut-row-{action}"))
                .bounds()
                .top()
                .to_f64()
        })
        .collect::<Vec<_>>();
    assert!(
        row_tops.windows(2).all(|pair| pair[0] < pair[1]),
        "main shortcut rows follow grouped catalog order"
    );

    for action in GPUI_ONLY_SHORTCUT_ACTIONS {
        let _ = observed_element(visual, &format!("shortcut-row-{action}"));
    }
    let configured_actions = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .keybindings
            .iter()
            .map(|binding| binding.action.clone())
            .collect::<Vec<_>>()
    });
    assert!(GPUI_ONLY_SHORTCUT_ACTIONS.iter().all(|action| {
        configured_actions
            .iter()
            .any(|configured| configured.as_str() == *action)
    }));
}

/// main 的动作文案保持原顺序，GPUI 专属和自定义动作都允许在页面显示。
/// @author kongweiguang
#[test]
fn main_shortcut_projection_preserves_catalog_and_runtime_actions() {
    let expected = [
        "settings.open",
        "settings.keybindings",
        "terminal.focus",
        "terminal.newTab",
        "terminal.closeTab",
        "terminal.closePane",
        "terminal.splitHorizontal",
        "terminal.splitVertical",
        "terminal.previousTab",
        "terminal.nextTab",
        "tool.agentLauncher",
        "tool.system",
        "tool.sftp",
        "tool.ports",
        "tool.snippets",
        "tool.logs",
    ];
    let actual = MAIN_SHORTCUT_PRESENTATIONS
        .iter()
        .map(|presentation| presentation.action)
        .collect::<Vec<_>>();

    assert_eq!(actual, expected.to_vec());
    for (index, action) in expected.iter().copied().enumerate() {
        assert_eq!(main_shortcut_order(action), Some(index));
    }
    for action in GPUI_ONLY_SHORTCUT_ACTIONS {
        assert!(main_settings_shortcut_visible(action));
        assert!(
            SettingsSnapshot::default()
                .keybindings
                .iter()
                .any(|binding| binding.action == action)
        );
    }
    assert!(main_settings_shortcut_visible("custom.provider.action"));
    assert_eq!(main_shortcut_order("custom.provider.action"), None);

    let agent_launcher =
        main_shortcut_presentation("tool.agentLauncher").expect("main includes Agent Launcher");
    assert_eq!(agent_launcher.label_zh, "打开 Agent Launcher");
    assert_eq!(agent_launcher.description_zh, "打开 Agent Launcher。");
    let command_history =
        main_shortcut_presentation("tool.logs").expect("main includes command history");
    assert_eq!(command_history.label_zh, "打开命令历史");
    assert_eq!(command_history.description_zh, "打开当前终端的命令历史。");
}

/// 展示层统一按键写法，持久化值仍保持 GPUI 规范串。
/// @author kongweiguang
#[test]
fn shortcut_display_uses_consistent_platform_notation() {
    assert_eq!(
        display_shortcut("ctrl-shift-f12", KeyboardPlatform::Windows),
        "Ctrl+Shift+F12"
    );
    assert_eq!(
        display_shortcut("Cmd+Shift+[", KeyboardPlatform::Mac),
        "Cmd+Shift+["
    );
    assert_eq!(
        display_shortcut("ctrl-alt-down", KeyboardPlatform::Linux),
        "Ctrl+Alt+Down"
    );
    assert_eq!(
        display_shortcut("Option+F12", KeyboardPlatform::Mac),
        "Option+F12"
    );
    assert_eq!(
        display_shortcut("ctrl-space", KeyboardPlatform::Windows),
        "Ctrl+Space"
    );
    assert_eq!(
        display_shortcut("ctrl-shift-f11 ctrl-shift-f12", KeyboardPlatform::Windows),
        "Ctrl+Shift+F11 Ctrl+Shift+F12"
    );
    assert_eq!(
        display_shortcut("not-a-key", KeyboardPlatform::Windows),
        "not-a-key"
    );
}

/// 已知和自定义动作都按运行态实际使用的配置作用域分组。
/// @author kongweiguang
#[test]
fn shortcut_scope_projection_only_overrides_catalogued_actions() {
    assert!(matches!(
        shortcut_display_scope("terminal.newTab", KeybindingScope::Global),
        KeybindingScope::Global
    ));
    assert!(matches!(
        shortcut_display_scope("custom.provider.action", KeybindingScope::Terminal),
        KeybindingScope::Terminal
    ));
}

/// About 仓库文字保留 HTTPS 前缀，仅去掉 Git clone 后缀。
/// @author kongweiguang
#[test]
fn repository_label_hides_transport_and_clone_suffix() {
    assert_eq!(
        display_repository_url("https://github.com/kongweiguang/Kerminal.git"),
        "https://github.com/kongweiguang/Kerminal"
    );
}

/// 桌面通知三个开关即时保存，高级区展开和收起必须对应阈值及节流输入的生命周期。
/// @author kongweiguang
#[gpui_kit::test]
fn desktop_notification_controls_and_advanced_disclosure_work(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-desktop");
    assert_rendered(visual, "desktop-notifications-enabled");
    assert_not_rendered(visual, "desktop-duration-threshold");

    click(visual, "desktop-policy-row-desktop-notifications-enabled");
    click(visual, "desktop-notifications-background-only");
    click(visual, "desktop-notifications-important-only");
    click(visual, "desktop-notifications-advanced-toggle");
    assert_rendered(visual, "desktop-duration-threshold");
    assert_rendered(visual, "desktop-event-throttle");

    let edited = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .desktop_notifications
    });
    assert!(edited.enabled);
    assert!(!edited.background_only);
    assert!(edited.important_only);

    pump_runtime(&runtime, visual);
    let persisted = repository.snapshot().0.desktop_notifications;
    assert!(persisted.enabled);
    assert!(!persisted.background_only);
    assert!(persisted.important_only);

    click(visual, "desktop-notifications-advanced-toggle");
    assert_not_rendered(visual, "desktop-duration-threshold");
    assert_not_rendered(visual, "desktop-event-throttle");
}

/// SFTP 并发输入经真实控件更新并持久化，高级 disclosure 显示后可再次收起。
/// @author kongweiguang
#[gpui_kit::test]
fn sftp_numeric_input_persists_and_advanced_controls_toggle(app: &mut TestAppContext) {
    let (runtime, panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-sftp");

    click(visual, "sftp-global-transfers");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("6");
    draw(visual);
    let global_transfers = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings loaded")
            .sftp
            .global_transfers
    });
    assert_eq!(global_transfers, 6);

    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.sftp.global_transfers, 6);

    click(visual, "preferences-sftp-advanced");
    assert_rendered(visual, "sftp-pipeline-depth");
    click(visual, "sftp-packet-bytes");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("0.125");
    draw(visual);
    let packet_bytes = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_snapshot()
            .expect("settings remain loaded")
            .sftp
            .packet_bytes
    });
    assert_eq!(packet_bytes, 131_072);

    pump_runtime(&runtime, visual);
    assert_eq!(repository.snapshot().0.sftp.packet_bytes, 131_072);

    click(visual, "preferences-sftp-advanced");
    assert_not_rendered(visual, "sftp-pipeline-depth");
}

/// 商店来源保存必须经过设置草稿与仓储 CAS，并在真实回读确认后才切换运行源。
#[gpui_kit::test]
fn catalog_source_save_uses_settings_cas_and_reads_back_before_switch(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let url = "https://mirror.example.com/catalog.json";
    let saves_before = repository.snapshot().1;
    let reads_before = repository.settings_read_count();
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    let (saved, saves_after) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url, "来源必须写入权威设置快照");
    assert_eq!(saves_after, saves_before + 1);
    assert!(
        repository.settings_read_count() > reads_before,
        "切换运行源前必须真实回读设置"
    );
    assert_eq!(catalog.source_url(), url, "回读一致后才切换运行态来源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert_eq!(page.current_source_url(), url);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none());
    });
}

/// 设置保存失败时不得切换来源，也不得把失败伪装成成功或进入回读。
#[gpui_kit::test]
fn catalog_source_save_failure_keeps_effective_source(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    let reads_before = repository.settings_read_count();
    repository.fail_next_settings_save();
    let url = "https://broken.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, "", "失败不得写入仓储");
    assert_eq!(
        repository.settings_read_count(),
        reads_before,
        "保存失败不得进入回读"
    );
    assert_eq!(catalog.source_url(), original, "失败不得切换运行态来源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert_eq!(page.current_source_url(), original);
        assert!(page.catalog_source_error().is_some(), "失败必须显示原因");
        assert!(!page.catalog_source_saving());
    });
    // 失败地址只存在于弹层输入，草稿必须回到基线，避免之后无关保存静默落盘。
    let draft_url = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .value()
            .plugin_catalog_url
            .clone()
    });
    assert_eq!(draft_url, "", "失败后草稿来源字段必须恢复基线");
    // 模拟一次无关设置编辑并保存：草稿里若仍留着失败地址，它会被静默持久化。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            let draft = panel.settings.as_mut().expect("settings loaded");
            let mut value = draft.value().clone();
            value.sftp.global_transfers = 6;
            draft
                .replace_value(value)
                .expect("ready draft accepts edit");
        });
    });
    assert!(visual.update(|_, cx| panel.update(cx, |panel, cx| panel.save_settings(cx))));
    pump_runtime(&runtime, visual);
    let (saved_after, _) = repository.snapshot();
    assert_eq!(
        saved_after.plugin_catalog_url, "",
        "无关保存不得静默写入失败来源"
    );
    assert_eq!(saved_after.sftp.global_transfers, 6);
}

/// 已保存但回读失败时必须保留旧运行源，并明确呈现为回读失败而非保存失败。
#[gpui_kit::test]
fn catalog_source_readback_failure_keeps_effective_source(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    let reads_before = repository.settings_read_count();
    repository.fail_next_settings_read();
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url, "保存本身必须成功");
    assert!(
        repository.settings_read_count() > reads_before,
        "必须尝试回读"
    );
    assert_eq!(catalog.source_url(), original, "回读失败不得切换运行源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert_eq!(page.current_source_url(), original);
        assert!(!page.catalog_source_saving());
        let error = page.catalog_source_error().expect("回读失败必须显示原因");
        assert!(
            error.contains("回读") || error.contains("read"),
            "文案必须区分已保存但回读失败: {error}"
        );
    });
    // 保存已成功，磁盘基线就是提交地址；回读失败不能把它回退成旧来源。
    let draft_url = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .value()
            .plugin_catalog_url
            .clone()
    });
    assert_eq!(draft_url, url, "回读失败不得回退已成功保存的磁盘基线");
}

/// 回读值与提交地址不一致时不得切换运行源，即使保存回执声称成功。
#[gpui_kit::test]
fn catalog_source_readback_mismatch_keeps_effective_source(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    repository.override_next_settings_read_url("https://other.example.com/catalog.json");
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url, "保存本身必须成功");
    assert_eq!(catalog.source_url(), original, "回读不一致不得切换运行源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert_eq!(page.current_source_url(), original);
        assert!(!page.catalog_source_saving());
        let error = page.catalog_source_error().expect("不一致必须显示原因");
        assert!(
            error.contains("不一致") || error.contains("differs"),
            "文案必须说明回读不一致: {error}"
        );
    });
}

/// clean-ready 重试只做回读、不重复写入；回读仍是切换运行源的必要条件。
#[gpui_kit::test]
fn catalog_source_clean_ready_retry_reads_back_without_rewrite(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), url, "首次保存回读后切换");
    let saves_after_first = repository.snapshot().1;
    let reads_after_first = repository.settings_read_count();
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.snapshot().1,
        saves_after_first,
        "clean-ready 重试不得盲重写设置"
    );
    assert!(
        repository.settings_read_count() > reads_after_first,
        "clean-ready 分支仍必须真实回读"
    );
    assert_eq!(catalog.source_url(), url);
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none());
    });
}

/// 保存未启动（其它字段输入错误）时，来源字段必须恢复基线且不进入回读。
#[gpui_kit::test]
fn catalog_source_save_not_started_restores_field(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.simulate_resize(size(px(1440.), px(900.)));
    draw(visual);
    click(visual, "preferences-nav-terminal");
    let _ = click_terminal_number_input(visual, &panel, "terminal-font-size");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("abc");
    draw(visual);
    assert!(visual.update(|_, cx| {
        panel
            .read(cx)
            .settings_input_errors
            .contains_key("font_size")
    }));

    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let saves_before = repository.snapshot().1;
    let reads_before = repository.settings_read_count();
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);

    assert_eq!(
        repository.snapshot().1,
        saves_before,
        "输入错误时不得启动保存"
    );
    assert_eq!(
        repository.settings_read_count(),
        reads_before,
        "未保存不得进入回读"
    );
    assert_eq!(catalog.source_url(), DEFAULT_PLUGIN_CATALOG_URL);
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(
            page.catalog_source_error().is_some(),
            "必须显示未能启动的原因"
        );
        assert!(!page.catalog_source_saving());
    });
    let draft_url = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .value()
            .plugin_catalog_url
            .clone()
    });
    assert_eq!(draft_url, "", "保存未启动时来源字段必须恢复基线");
}

/// 替换后草稿恰好回到基线时只真实回读，不空写，也不误报 NoChanges。
#[gpui_kit::test]
fn catalog_source_replace_to_baseline_reads_back_without_write(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let url = "https://mirror.example.com/catalog.json";
    // 构造“上次来源写入已知失败、草稿保留未落盘地址”的状态：值含 X，基线仍是官方来源。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            let draft = panel.settings.as_mut().expect("settings loaded");
            let mut value = draft.value().clone();
            value.plugin_catalog_url = url.to_owned();
            draft
                .replace_value(value)
                .expect("ready draft accepts value");
            draft.begin_save().expect("dirty draft starts save");
            draft.save_failed().expect("simulated known failure");
        });
    });
    let saves_before = repository.snapshot().1;
    let reads_before = repository.settings_read_count();
    // 用户把来源改回已保存的官方来源：替换后草稿恰好变 clean。
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test("", window, cx)
        });
    });
    pump_runtime(&runtime, visual);

    assert_eq!(repository.snapshot().1, saves_before, "回到基线不得空写");
    assert!(
        repository.settings_read_count() > reads_before,
        "必须真实回读"
    );
    assert_eq!(catalog.source_url(), DEFAULT_PLUGIN_CATALOG_URL);
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none(), "不得误报 NoChanges");
    });
}

/// 未知保存结果不得擅自回退来源字段，也不得切换运行源。
#[gpui_kit::test]
fn catalog_source_unknown_result_does_not_rollback_field(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    repository
        .state
        .lock()
        .expect("test repository")
        .unknown_after_settings_save = true;
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);

    let (status, draft_url) = visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().expect("settings loaded");
        (draft.status(), draft.value().plugin_catalog_url.clone())
    });
    assert_eq!(status, super::DraftStatus::OutcomeUnknown);
    assert_eq!(draft_url, url, "未知结果不得擅自回退来源字段");
    assert_eq!(catalog.source_url(), original, "未知结果不得切换运行源");
    // 未知结果的恢复读回失败时必须显式结束来源意图并显示原因，不能永久 saving。
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(
            !page.catalog_source_saving(),
            "回读失败必须结束来源 pending"
        );
        assert!(page.catalog_source_error().is_some(), "回读失败必须可见");
    });
}

/// CAS 冲突不得擅自回退来源字段，也不得切换运行源。
#[gpui_kit::test]
fn catalog_source_conflict_does_not_rollback_field(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    // 外部写入推进 revision，使来源保存 CAS 冲突；同时让冲突后的核对读回失败，
    // 固定草稿在冲突状态，避免异步三方合并改变断言对象。
    repository.state.lock().expect("test repository").revision = Some(ConfigRevision::from_sha256(
        "external-settings-revision".to_owned(),
    ));
    repository.fail_next_settings_read();
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);

    let (status, draft_url) = visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().expect("settings loaded");
        (draft.status(), draft.value().plugin_catalog_url.clone())
    });
    assert_eq!(status, super::DraftStatus::ExternalConflict);
    assert_eq!(draft_url, url, "冲突不得擅自回退来源字段");
    assert_eq!(catalog.source_url(), original, "冲突不得切换运行源");
}

/// 保存真正在途且保存无法启动时，未提交 URL 必须撤回，不能被无关保存带走。
#[gpui_kit::test]
fn catalog_source_request_during_saving_with_input_error_withdraws_field(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    let release = repository.hold_next_settings_save();
    // 启动一次与来源无关的设置保存，并保持其在途。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            let draft = panel.settings.as_mut().expect("settings loaded");
            let mut value = draft.value().clone();
            value.sftp.global_transfers = 6;
            draft
                .replace_value(value)
                .expect("ready draft accepts edit");
        });
    });
    assert!(visual.update(|_, cx| panel.update(cx, |panel, cx| panel.save_settings(cx))));
    pump_runtime(&runtime, visual);
    let status = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .status()
    });
    assert_eq!(status, super::DraftStatus::Saving, "保存必须真正在途");
    assert_eq!(repository.save_attempt_count(), 1);
    // 保存中注入输入错误：来源请求无法启动保存，只能撤回未提交字段。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel
                .settings_input_errors
                .insert("font_size", "invalid".to_owned());
        });
    });
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(
            page.catalog_source_error().is_some(),
            "必须显示未能启动的原因"
        );
        assert!(!page.catalog_source_saving());
    });
    let draft_url = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .value()
            .plugin_catalog_url
            .clone()
    });
    assert_eq!(draft_url, "", "未提交的 URL 必须从草稿撤回");
    // 放行在途保存：它使用旧快照（URL 为空）成功，不得包含失败地址。
    release.send(()).expect("release held save");
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.snapshot().0.plugin_catalog_url,
        "",
        "在途保存不得写入失败来源"
    );
    // 清除输入错误后做一次无关保存，证明失败地址不会被后续 autosave 带走。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel.settings_input_errors.clear();
            let draft = panel.settings.as_mut().expect("settings loaded");
            let mut value = draft.value().clone();
            value.sftp.global_transfers = 8;
            draft
                .replace_value(value)
                .expect("draft accepts edit after save");
        });
    });
    assert!(visual.update(|_, cx| panel.update(cx, |panel, cx| panel.save_settings(cx))));
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, "", "无关保存不得静默写入失败来源");
    assert_eq!(saved.sftp.global_transfers, 8);
    assert_eq!(catalog.source_url(), original);
}

/// 已确认 URL 的重试只做权威回读，不因其它字段脏值或输入错误重写整份设置。
#[gpui_kit::test]
fn catalog_source_confirmed_url_reads_back_without_rewriting_dirty_draft(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), url);
    let saves_after_first = repository.snapshot().1;
    let reads_after_first = repository.settings_read_count();
    // 其它字段变脏并有输入错误：重试同一已保存 URL 不应重写整份设置。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            let draft = panel.settings.as_mut().expect("settings loaded");
            let mut value = draft.value().clone();
            value.sftp.global_transfers = 6;
            draft
                .replace_value(value)
                .expect("ready draft accepts edit");
            panel
                .settings_input_errors
                .insert("font_size", "invalid".to_owned());
        });
    });
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.snapshot().1,
        saves_after_first,
        "已确认 URL 重试不得重写无关设置"
    );
    assert!(
        repository.settings_read_count() > reads_after_first,
        "必须真实回读"
    );
    assert_eq!(catalog.source_url(), url);
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none());
    });
    let (draft_url, transfers, has_error) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        (
            draft.value().plugin_catalog_url.clone(),
            draft.value().sftp.global_transfers,
            panel.settings_input_errors.contains_key("font_size"),
        )
    });
    assert_eq!(draft_url, url);
    assert_eq!(transfers, 6, "其它脏字段必须保留");
    assert!(has_error, "其它输入错误必须保留");
}

/// 来源成功后 follow-up 被输入校验拒绝，不得取消来源意图或丢失较新编辑。
#[gpui_kit::test]
fn catalog_source_follow_up_validation_rejection_keeps_source_intent(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let release = repository.hold_next_settings_save();
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(
        visual.update(|_, cx| {
            panel
                .read(cx)
                .settings
                .as_ref()
                .expect("settings loaded")
                .status()
        }),
        super::DraftStatus::Saving
    );
    // 保存期间走真实编辑入口产生较新编辑，并让防抖计时器把 follow-up 排入单飞队列。
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(|settings| settings.sftp.global_transfers = 6, cx);
        });
    });
    pump_runtime(&runtime, visual);
    // 队列已建立后再注入输入错误，使 follow-up 真正被校验拒绝。
    visual.update(|_, cx| {
        panel.update(cx, |panel, _| {
            panel
                .settings_input_errors
                .insert("font_size", "invalid".to_owned());
        });
    });
    release.send(()).expect("release held save");
    pump_runtime(&runtime, visual);
    assert_eq!(
        repository.save_attempt_count(),
        1,
        "输入错误应阻止 follow-up 启动"
    );
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url);
    assert_eq!(
        catalog.source_url(),
        url,
        "来源成功后 follow-up 失败不得取消切源"
    );
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(
            page.catalog_source_error().is_none(),
            "follow-up 失败不是来源失败"
        );
    });
    let (status, transfers, has_error) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        (
            draft.status(),
            draft.value().sftp.global_transfers,
            panel.settings_input_errors.contains_key("font_size"),
        )
    });
    assert_eq!(status, super::DraftStatus::Ready, "输入错误不改变草稿状态");
    assert_eq!(transfers, 6, "较新编辑必须保留");
    assert!(has_error, "输入错误必须保留");
}

/// 来源成功后 follow-up 被已知失败拒绝，不得把无关失败当成来源失败。
#[gpui_kit::test]
fn catalog_source_follow_up_known_failure_keeps_source_intent(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let release = repository.hold_next_settings_save();
    repository.fail_settings_save_attempt(2);
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    assert_eq!(
        visual.update(|_, cx| {
            panel
                .read(cx)
                .settings
                .as_ref()
                .expect("settings loaded")
                .status()
        }),
        super::DraftStatus::Saving
    );
    // 保存期间走真实编辑入口产生较新编辑，并让防抖计时器把 follow-up 排入单飞队列。
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(|settings| settings.sftp.global_transfers = 6, cx);
        });
    });
    pump_runtime(&runtime, visual);
    release.send(()).expect("release held save");
    pump_runtime(&runtime, visual);
    assert_eq!(repository.save_attempt_count(), 2, "必须尝试 follow-up");
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url, "首次保存已写入来源");
    assert_eq!(
        catalog.source_url(),
        url,
        "follow-up 已知失败不得取消已确认来源"
    );
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none());
    });
    let (status, transfers) = visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().expect("settings loaded");
        (draft.status(), draft.value().sftp.global_transfers)
    });
    assert_eq!(
        status,
        super::DraftStatus::Failed,
        "follow-up 失败仍在草稿中可见"
    );
    assert_eq!(transfers, 6, "较新编辑必须保留");
}

/// 冲突恢复必须走既有三方合并与续写，并在确认后切换来源。
#[gpui_kit::test]
fn catalog_source_conflict_recovery_resaves_and_switches(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    repository.state.lock().expect("test repository").revision = Some(ConfigRevision::from_sha256(
        "external-settings-revision".to_owned(),
    ));
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(repository.save_attempt_count(), 2, "冲突后必须重写一次");
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url, "冲突合并后必须写入来源");
    assert_eq!(catalog.source_url(), url, "冲突恢复确认后必须切换来源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(
            page.catalog_source_error().is_none(),
            "成功恢复不得显示来源错误"
        );
    });
    let (status, dirty) = visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().expect("settings loaded");
        (draft.status(), draft.is_dirty())
    });
    assert_eq!(status, super::DraftStatus::Ready);
    assert!(!dirty, "恢复后草稿应与写入一致");
}

/// 未知结果经一致回读确认后必须切换来源，不能停在未知状态。
#[gpui_kit::test]
fn catalog_source_unknown_confirmation_switches_source(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    repository.return_unknown_after_next_save();
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, url);
    assert_eq!(
        catalog.source_url(),
        url,
        "未知结果经一致回读确认后必须切换"
    );
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(page.catalog_source_error().is_none());
    });
    let (status, dirty) = visual.update(|_, cx| {
        let draft = panel.read(cx).settings.as_ref().expect("settings loaded");
        (draft.status(), draft.is_dirty())
    });
    assert_eq!(status, super::DraftStatus::Ready);
    assert!(!dirty);
}

/// 未知结果回读不一致时必须失败可见且不切换来源，不能盲目重写。
#[gpui_kit::test]
fn catalog_source_unknown_mismatch_fails_visibly(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    repository.return_unknown_after_next_save();
    repository.override_next_settings_read_url("https://other.example.com/catalog.json");
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), original, "未知未确认不得切换");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving(), "失败必须结束来源 pending");
        let error = page.catalog_source_error().expect("必须显示可恢复错误");
        assert!(!error.is_empty());
    });
    let status = visual.update(|_, cx| {
        panel
            .read(cx)
            .settings
            .as_ref()
            .expect("settings loaded")
            .status()
    });
    assert_eq!(
        status,
        super::DraftStatus::OutcomeUnknown,
        "不一致时草稿保持未知"
    );
}

/// 让同一份外部设置在每个平台都触发 UI 保存校验，避免宿主快捷键差异绕过冲突分支。
///
/// 从默认动作复制生效字段，保留 Linux 跟随 Windows 配置的语义。
///
/// @author kongweiguang
fn make_ui_blocked_shortcut_collision(settings: &mut SettingsSnapshot) {
    let open = settings
        .keybindings
        .iter()
        .find(|binding| binding.action == "settings.open")
        .cloned()
        .expect("默认设置必须包含 settings.open");
    let search = settings
        .keybindings
        .iter_mut()
        .find(|binding| binding.action == "terminal.search")
        .expect("默认设置必须包含 terminal.search");
    search.binding = open.binding;
    search.windows_binding = open.windows_binding;
    search.mac_binding = open.mac_binding;
    search.linux_binding = open.linux_binding;
}

/// 冲突恢复续写被 UI 校验拒绝时，来源必须显式失败而不是永久 saving。
///
/// 外部设置写入 application 接受、UI 阻塞的快捷键冲突；三方合并保留本地来源
/// 与远端冲突值后，续写因 UI 校验无法启动。未提交来源必须撤回并失败可见，
/// 修复阻塞后仍可重试切源，失败地址不能被无关保存带入。
///
/// @author kongweiguang
#[gpui_kit::test]
fn catalog_source_conflict_resave_blocked_by_ui_validation_fails_visibly(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let original = catalog.source_url();
    {
        let mut state = repository.state.lock().expect("test repository");
        let mut external = state.settings.clone();
        make_ui_blocked_shortcut_collision(&mut external);
        state.settings = external;
        state.revision = Some(ConfigRevision::from_sha256(
            "external-ui-blocked".to_owned(),
        ));
    }
    let url = "https://mirror.example.com/catalog.json";
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), original, "续写未启动不得切源");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving(), "必须结束来源 saving");
        let error = page.catalog_source_error().expect("必须显示来源失败原因");
        assert!(!error.is_empty());
    });
    let (draft_url, has_collision) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        (
            draft.value().plugin_catalog_url.clone(),
            draft.value().keybindings.iter().any(|binding| {
                binding.action == "terminal.search" && binding.windows_binding == "Ctrl+Alt+S"
            }),
        )
    });
    assert_eq!(draft_url, "", "未提交来源必须撤回");
    assert!(has_collision, "外部冲突值必须保留");
    // 修复 UI 阻塞后做一次无关保存：失败 URL 不得被带入。
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(
                |settings| {
                    settings.keybindings = SettingsSnapshot::default().keybindings;
                    settings.sftp.global_transfers = 8;
                },
                cx,
            );
        });
    });
    assert!(visual.update(|_, cx| panel.update(cx, |panel, cx| panel.save_settings(cx))));
    pump_runtime(&runtime, visual);
    let (saved, _) = repository.snapshot();
    assert_eq!(saved.plugin_catalog_url, "", "无关保存不得带入失败来源");
    assert_eq!(saved.sftp.global_transfers, 8);
    // 修复阻塞后可重试来源并切源。
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), url, "修复阻塞后重试必须切源");
    assert_eq!(repository.snapshot().0.plugin_catalog_url, url);
}

/// 冲突恢复续写被 UI 阻塞、但来源已在远端基线时，必须回读确认而不是误报失败。
///
/// 远端设置已包含来源 X 并带 UI 阻塞的快捷键冲突，本地另有脏字段；三方合并后
/// 续写无法启动。来源已在基线，撤回不得清掉它，必须真实回读确认并保留本地脏值。
///
/// @author kongweiguang
#[gpui_kit::test]
fn catalog_source_conflict_resave_blocked_with_baseline_url_reads_back(app: &mut TestAppContext) {
    let (runtime, panel, repository, catalog, visual) = fixture_with_extensions(app);
    visual.update(|window, cx| {
        panel.update(cx, |panel, cx| panel.open_extensions_section(window, cx));
    });
    pump_runtime(&runtime, visual);
    let url = "https://mirror.example.com/catalog.json";
    {
        let mut state = repository.state.lock().expect("test repository");
        let mut external = state.settings.clone();
        external.plugin_catalog_url = url.to_owned();
        make_ui_blocked_shortcut_collision(&mut external);
        state.settings = external;
        state.revision = Some(ConfigRevision::from_sha256(
            "external-baseline-source".to_owned(),
        ));
    }
    visual.update(|_, cx| {
        panel.update(cx, |panel, cx| {
            panel.edit_settings(|settings| settings.sftp.global_transfers = 6, cx);
        });
    });
    visual.update(|window, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        page.update(cx, |page, cx| {
            page.submit_source_url_for_test(url, window, cx)
        });
    });
    pump_runtime(&runtime, visual);
    pump_runtime(&runtime, visual);
    assert_eq!(catalog.source_url(), url, "基线已确认来源必须切换");
    visual.update(|_, cx| {
        let page = panel.read(cx).extensions_page().expect("扩展页面已创建");
        let page = page.read(cx);
        assert!(!page.catalog_source_saving());
        assert!(
            page.catalog_source_error().is_none(),
            "基线已确认不得误报来源失败"
        );
    });
    let (draft_url, transfers, has_collision) = visual.update(|_, cx| {
        let panel = panel.read(cx);
        let draft = panel.settings.as_ref().expect("settings loaded");
        (
            draft.value().plugin_catalog_url.clone(),
            draft.value().sftp.global_transfers,
            draft.value().keybindings.iter().any(|binding| {
                binding.action == "terminal.search" && binding.windows_binding == "Ctrl+Alt+S"
            }),
        )
    });
    assert_eq!(draft_url, url);
    assert_eq!(transfers, 6, "本地脏字段必须保留");
    assert!(has_collision, "外部冲突值必须保留");
}
