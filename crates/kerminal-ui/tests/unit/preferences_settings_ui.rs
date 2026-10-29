//! 设置生产面板的真实 GPUI 鼠标、键盘与持久化交互回归。
//!
//! @author kongweiguang

#[path = "preferences_color_sampling.rs"]
mod color_sampling;

#[path = "preferences_mcp.rs"]
mod mcp;

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
    preferences::{
        ConfigurationGuideStatus, ConfigurationRepository, DefaultLocalShellProvider,
        KeybindingScope, PreferencesService, ProfileDraft, ProfileRepository, ProfileRevision,
        ProfileService, SettingsRepository, SettingsSnapshot, VersionedProfile, VersionedSettings,
    },
};
use kerminal_domain::{
    preferences::{
        ConfigDocument, ConfigDocumentPath, ConfigRevision, ConfigurationMode,
        ConfigurationRecoveryState,
    },
    sessions::LocalSessionTarget,
};
use tokio::runtime::{Builder, Runtime};

use super::render_desktop_about::{
    GPUI_ONLY_SHORTCUT_ACTIONS, MAIN_SHORTCUT_PRESENTATIONS, display_repository_url,
    main_settings_shortcut_visible, main_shortcut_order, main_shortcut_presentation,
    shortcut_display_scope,
};
use super::{PreferencesPanel, PreferencesPanelServices};

#[derive(Clone)]
struct MemorySettingsRepository {
    state: Arc<Mutex<MemorySettingsState>>,
}

struct MemorySettingsState {
    settings: SettingsSnapshot,
    revision: Option<ConfigRevision>,
    next_revision: u64,
    save_count: usize,
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
}

#[async_trait]
impl SettingsRepository for MemorySettingsRepository {
    /// 返回唯一的测试快照和 CAS revision，不让测试自行复刻配置文件解码。
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        let state = self.state.lock().expect("settings state is not poisoned");
        Ok(VersionedSettings {
            settings: state.settings.clone(),
            revision: state.revision.clone(),
        })
    }

    /// 按 revision 接受设置写入，任何旧基线覆盖都会在测试仓储显式失败。
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        expected_revision: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        let mut state = self.state.lock().expect("settings state is not poisoned");
        if state.revision != expected_revision {
            return Err(AppError::conflict("测试设置 revision 已变化"));
        }
        let revision = ConfigRevision::from_sha256(format!("settings-ui-{}", state.next_revision));
        state.next_revision += 1;
        state.save_count += 1;
        state.settings = settings.clone();
        state.revision = Some(revision.clone());
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

    /// 设置 UI fixture 不允许删除主机配置。
    async fn delete_profile(
        &self,
        _id: &str,
        _expected_revision: ConfigRevision,
    ) -> AppResult<bool> {
        Err(AppError::unsupported("设置 UI fixture 不删除 Profile"))
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
        let panel = cx.new(|cx| PreferencesPanel::new(services, window, cx));
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

/// 保留 snapshot 的裁剪可见性；无 snapshot 时只使用生产 debug selector 记录的真实布局 bounds。
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
            return (snapshot.bounds(), snapshot.visible());
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

/// 仅在真实 snapshot 或 debug bounds 尚未进入视口时滚动；起点也允许使用 debug selector，不改写滚动偏移。
///
/// @author kongweiguang
fn scroll_until_visible(
    visual: &mut VisualTestContext,
    selector: &str,
    anchor: &str,
    delta_y: f32,
) {
    let position = target_geometry(visual, anchor).0.center();
    for _ in 0..32 {
        if target_geometry(visual, selector).1 {
            return;
        }
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
        ("preferences-nav-terminal", "terminal-scheme-light-kerminal"),
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

/// 窗口尺寸变化与纵向滚动不能挤掉 renderer 折叠的 44px 命中区或阻断真实展开事件。
/// @author kongweiguang
#[gpui_kit::test]
fn terminal_renderer_disclosure_keeps_its_hit_area_and_scroll_interaction(
    app: &mut TestAppContext,
) {
    let (_runtime, panel, _repository, visual) = fixture(app);

    for window_size in [size(px(1440.), px(900.)), size(px(800.), px(600.))] {
        visual.simulate_resize(window_size);
        draw(visual);
        click(visual, "preferences-nav-appearance");
        click(visual, "preferences-nav-terminal");
        visual.update(|_, cx| panel.read(cx).content_scroll.scroll_to_top_of_item(0));
        draw(visual);

        let (bounds, clip) =
            scroll_target_fully_into_content_clip(visual, &panel, "terminal-renderer-disclosure");
        assert!(
            bounds.size.height.to_f64() >= 44.,
            "renderer disclosure must retain its 44px hit area at {window_size:?}; bounds={bounds:?}, clip={clip:?}"
        );
        assert!(
            bounds_within_viewport(bounds, clip),
            "renderer disclosure center must be inside the production content clip at {window_size:?}; bounds={bounds:?}, clip={clip:?}"
        );

        click(visual, "terminal-renderer-disclosure");
        assert!(
            visual.update(|_, cx| panel.read(cx).terminal_renderer_open),
            "renderer disclosure click did not expand at {window_size:?}; bounds={bounds:?}, clip={clip:?}"
        );
        click(visual, "terminal-renderer-disclosure");
        assert!(
            !visual.update(|_, cx| panel.read(cx).terminal_renderer_open),
            "renderer disclosure click did not collapse at {window_size:?}; bounds={bounds:?}, clip={clip:?}"
        );
    }
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

/// 缩略卡随设置正文的实际宽度换行，窄窗口和宽窗口都不能越过内容区两侧。
///
/// @author kongweiguang
#[gpui_kit::test]
fn app_color_scheme_choices_fit_the_settings_content_width(app: &mut TestAppContext) {
    let (_runtime, panel, _repository, visual) = fixture(app);
    for window_size in [size(px(720.), px(600.)), size(px(1440.), px(900.))] {
        visual.simulate_resize(window_size);
        draw(visual);
        let clip = visual.update(|_, cx| panel.read(cx).content_scroll.bounds());
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
        }
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

/// 快捷键平台切换只改变当前列表投影，Windows/macOS 控件均保持可操作。
/// @author kongweiguang
#[gpui_kit::test]
fn shortcut_platform_can_switch_between_windows_and_macos(app: &mut TestAppContext) {
    let (runtime, _panel, repository, visual) = fixture(app);
    click(visual, "preferences-nav-shortcuts");
    assert_rendered(visual, "settings-shortcut-platform-windows");

    click(visual, "settings-shortcut-platform-macos");
    scroll_until_visible(
        visual,
        "shortcut-input-settings.open-macos",
        "settings-shortcut-platform-macos",
        -8.,
    );
    click(visual, "shortcut-input-settings.open-macos");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("Cmd+Shift+F12");
    draw(visual);
    pump_runtime(&runtime, visual);
    let (persisted, saves) = repository.snapshot();
    let settings_open = persisted
        .keybindings
        .iter()
        .find(|binding| binding.action == "settings.open")
        .expect("settings.open binding exists");
    assert_eq!(settings_open.mac_binding, "Cmd+Shift+F12");
    assert_eq!(settings_open.windows_binding, "Ctrl+Alt+S");
    assert!(saves >= 1);

    click(visual, "shortcut-reset-settings.open");
    pump_runtime(&runtime, visual);
    let default_settings_open = SettingsSnapshot::default()
        .keybindings
        .into_iter()
        .find(|binding| binding.action == "settings.open")
        .expect("settings.open default exists");
    let reset_settings_open = repository
        .snapshot()
        .0
        .keybindings
        .into_iter()
        .find(|binding| binding.action == "settings.open")
        .expect("settings.open binding exists after reset");
    assert_eq!(
        reset_settings_open.mac_binding,
        default_settings_open.mac_binding
    );
    assert_eq!(
        reset_settings_open.windows_binding,
        default_settings_open.windows_binding
    );

    scroll_until_visible(
        visual,
        "settings-shortcut-platform-windows",
        "shortcut-input-settings.open-macos",
        8.,
    );
    click(visual, "settings-shortcut-platform-windows");
    scroll_until_visible(
        visual,
        "shortcut-input-settings.open-windows",
        "settings-shortcut-platform-windows",
        -8.,
    );
    assert_rendered(visual, "shortcut-input-settings.open-windows");
}

/// 生产快捷键视图按 main 分组顺序展示 16 项，同时 GPUI 专属动作仍保留在设置快照。
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
        assert_not_rendered(visual, &format!("shortcut-row-{action}"));
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

/// main 的 16 个动作保持原顺序，GPUI 专属展示过滤不删除运行态绑定或自定义动作。
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
        assert!(!main_settings_shortcut_visible(action));
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

/// main 已登记动作使用固定作用域，自定义动作继续保留自己的配置作用域。
/// @author kongweiguang
#[test]
fn shortcut_scope_projection_only_overrides_catalogued_actions() {
    assert!(matches!(
        shortcut_display_scope("terminal.newTab", KeybindingScope::Global),
        KeybindingScope::Workspace
    ));
    assert!(matches!(
        shortcut_display_scope("custom.provider.action", KeybindingScope::Terminal),
        KeybindingScope::Terminal
    ));
}

/// About 仓库文字隐藏传输协议和 clone 后缀，打开动作仍使用原始地址。
/// @author kongweiguang
#[test]
fn repository_label_hides_transport_and_clone_suffix() {
    assert_eq!(
        display_repository_url("https://github.com/kongweiguang/Kerminal.git"),
        "github.com/kongweiguang/Kerminal"
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
