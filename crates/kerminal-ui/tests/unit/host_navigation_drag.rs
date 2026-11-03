//! 主机树真实 GPUI 拖放、取消、边缘滚动和移动读回次序回归。
//!
//! @author kongweiguang

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Context, Entity, Focusable as _, InteractiveElement as _, IntoElement,
    Modifiers, MouseButton, ParentElement as _, Render, Role, StatefulInteractiveElement as _,
    Styled as _, Subscription, TestAppContext, VisualTestContext, Window,
    base::TestSupportExt as _,
    component::{Icon, IconName},
    point, px, size,
};
use kerminal_application::{
    AppError, AppResult, ApplicationTaskExecutor,
    connections::management::{
        HostAuthenticationMode, HostDocumentRevision, HostDraft, HostGroup,
        HostGroupDeletionReceipt, HostGroupDraft, HostGroupSnapshot, HostManagementRepository,
        HostManagementService, ManagedHost, ManagedLocalHost,
    },
};
use kerminal_domain::{
    connections::{HostEndpoint, RemoteProtocol},
    identity::{HostId, TabId},
    preferences::{ConfigRevision, ConfigurationMode, ConfigurationRecoveryState},
};
use tokio::{runtime::Runtime, sync::Notify};

use super::{HostDragPreview, HostNavigation, HostNavigationEvent, next_host_drag_scroll_offset};

/// 内存目录实现真实读写语义和 revision；异步门禁只控制测试时序，不回显未提交状态。
///
/// @author kongweiguang
struct DragDirectory {
    state: Mutex<DirectoryState>,
    move_calls: AtomicUsize,
    committed_moves: AtomicUsize,
    remove_after_commit: AtomicBool,
    block_move: AtomicBool,
    move_started: Notify,
    move_release: Notify,
    block_read: AtomicBool,
    read_release: Notify,
    fail_next_read: AtomicBool,
    outcomes: Mutex<VecDeque<MoveOutcome>>,
}

#[derive(Default)]
struct DirectoryState {
    hosts: Vec<ManagedHost>,
    local_hosts: Vec<ManagedLocalHost>,
    groups: Vec<HostGroup>,
}

enum MoveOutcome {
    Fail(AppError),
    Unknown { commit: bool },
}

impl DragDirectory {
    fn new(
        hosts: Vec<ManagedHost>,
        local_hosts: Vec<ManagedLocalHost>,
        groups: Vec<HostGroup>,
    ) -> Self {
        Self {
            state: Mutex::new(DirectoryState {
                hosts,
                local_hosts,
                groups,
            }),
            move_calls: AtomicUsize::new(0),
            committed_moves: AtomicUsize::new(0),
            remove_after_commit: AtomicBool::new(false),
            block_move: AtomicBool::new(false),
            move_started: Notify::new(),
            move_release: Notify::new(),
            block_read: AtomicBool::new(false),
            read_release: Notify::new(),
            fail_next_read: AtomicBool::new(false),
            outcomes: Mutex::new(VecDeque::new()),
        }
    }

    fn move_calls(&self) -> usize {
        self.move_calls.load(Ordering::SeqCst)
    }

    fn committed_moves(&self) -> usize {
        self.committed_moves.load(Ordering::SeqCst)
    }

    fn remove_host_after_next_commit(&self) {
        self.remove_after_commit.store(true, Ordering::SeqCst);
    }

    fn group_of(&self, host_id: &str) -> Option<Option<String>> {
        let state = self.state.lock().expect("读取测试目录");
        state
            .hosts
            .iter()
            .find(|host| host.id.as_str() == host_id)
            .map(|host| host.group_id.clone())
            .or_else(|| {
                state
                    .local_hosts
                    .iter()
                    .find(|host| host.id.as_str() == host_id)
                    .map(|host| host.group_id.clone())
            })
    }

    fn replace_revision(&self, host_id: &str) {
        let mut state = self.state.lock().expect("修改源 revision");
        if let Some(host) = state
            .hosts
            .iter_mut()
            .find(|host| host.id.as_str() == host_id)
        {
            host.revision = ConfigRevision::from_sha256("externally-changed".to_owned());
        }
        if let Some(host) = state
            .local_hosts
            .iter_mut()
            .find(|host| host.id.as_str() == host_id)
        {
            host.revision = ConfigRevision::from_sha256("externally-changed".to_owned());
        }
    }

    fn remove_group(&self, group_id: &str) {
        self.state
            .lock()
            .expect("删除目标组")
            .groups
            .retain(|group| group.id != group_id);
    }
}

#[async_trait]
impl HostManagementRepository for DragDirectory {
    async fn ensure_host_writable(&self) -> AppResult<()> {
        Ok(())
    }

    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        if self.block_read.swap(false, Ordering::SeqCst) {
            self.read_release.notified().await;
        }
        if self.fail_next_read.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("injected catalog read failure"));
        }
        Ok(self.state.lock().expect("读取远端目录").hosts.clone())
    }

    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self
            .state
            .lock()
            .expect("读取远端主机")
            .hosts
            .iter()
            .find(|host| &host.id == host_id)
            .cloned())
    }

    async fn list_local_hosts(&self) -> AppResult<Vec<ManagedLocalHost>> {
        Ok(self.state.lock().expect("读取本地目录").local_hosts.clone())
    }

    async fn load_local_host(&self, host_id: &HostId) -> AppResult<Option<ManagedLocalHost>> {
        Ok(self
            .state
            .lock()
            .expect("读取本地主机")
            .local_hosts
            .iter()
            .find(|host| &host.id == host_id)
            .cloned())
    }

    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        panic!("拖拽用例不得保存完整草稿")
    }

    async fn move_host_to_group(
        &self,
        host_id: HostId,
        group_id: Option<String>,
        expected_revision: ConfigRevision,
    ) -> AppResult<()> {
        self.move_calls.fetch_add(1, Ordering::SeqCst);
        if self.block_move.swap(false, Ordering::SeqCst) {
            self.move_started.notify_one();
            self.move_release.notified().await;
        }
        let outcome = self.outcomes.lock().expect("读取移动结果").pop_front();
        if let Some(MoveOutcome::Fail(error)) = &outcome {
            return Err(error.clone());
        }
        if matches!(outcome, Some(MoveOutcome::Unknown { commit: false })) {
            return Err(AppError::outcome_unknown("injected unknown move"));
        }
        {
            let mut state = self.state.lock().expect("提交测试移动");
            if group_id
                .as_ref()
                .is_some_and(|id| !state.groups.iter().any(|group| &group.id == id))
            {
                return Err(AppError::conflict("destination missing"));
            }
            let revision = ConfigRevision::from_sha256(format!(
                "moved-{}",
                self.move_calls.load(Ordering::SeqCst)
            ));
            let update = |current_group: &mut Option<String>,
                          current_revision: &mut ConfigRevision,
                          updated_at: &mut String| {
                if current_revision != &expected_revision {
                    return Err(AppError::conflict("stale source"));
                }
                *current_group = group_id.clone();
                *current_revision = revision.clone();
                *updated_at = "2".to_owned();
                Ok(())
            };
            let mut found = false;
            for host in &mut state.hosts {
                if host.id == host_id {
                    found = true;
                    update(&mut host.group_id, &mut host.revision, &mut host.updated_at)?;
                }
            }
            for host in &mut state.local_hosts {
                if host.id == host_id {
                    found = true;
                    update(&mut host.group_id, &mut host.revision, &mut host.updated_at)?;
                }
            }
            if !found {
                return Err(AppError::conflict("host missing"));
            }
            if self.remove_after_commit.swap(false, Ordering::SeqCst) {
                state.hosts.retain(|host| host.id != host_id);
                state.local_hosts.retain(|host| host.id != host_id);
            }
        }
        self.committed_moves.fetch_add(1, Ordering::SeqCst);
        match outcome {
            Some(MoveOutcome::Fail(error)) => Err(error),
            Some(MoveOutcome::Unknown { .. }) => {
                Err(AppError::outcome_unknown("injected unknown move"))
            }
            None => Ok(()),
        }
    }

    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        panic!("拖拽用例不得删除主机")
    }

    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: self.state.lock().expect("读取分组").groups.clone(),
            revision: Some(ConfigRevision::from_sha256("groups".to_owned())),
        })
    }

    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        panic!("拖拽用例不得保存分组")
    }

    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        panic!("拖拽用例不得删除分组")
    }
}

/// 构造完整远端目录快照，凭据与环境均不参与导航移动。
fn remote(id: &str, group_id: Option<&str>) -> ManagedHost {
    ManagedHost {
        id: HostId::parse(id).expect("合法远端 ID"),
        group_id: group_id.map(str::to_owned),
        name: format!("Remote {id}"),
        note: None,
        protocol: RemoteProtocol::Ssh,
        endpoint: HostEndpoint {
            host: "example.invalid".to_owned(),
            port: 22,
            username: "tester".to_owned(),
        },
        authentication: None,
        authentication_mode: HostAuthenticationMode::Agent,
        rdp_password: None,
        route: Default::default(),
        serial: None,
        tags: Vec::new(),
        is_favorite: false,
        sort_order: 0,
        connect_timeout_ms: 30_000,
        keepalive_interval_ms: Some(60_000),
        terminal_options: Default::default(),
        sftp_options: Default::default(),
        rdp_display: Default::default(),
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256(format!("revision-{id}")),
    }
}

/// 本地列表只暴露启动元数据，移动不需要环境 resolver。
fn local(id: &str, group_id: Option<&str>) -> ManagedLocalHost {
    ManagedLocalHost {
        id: HostId::parse(id).expect("合法本地 ID"),
        group_id: group_id.map(str::to_owned),
        name: format!("Local {id}"),
        shell: "pwsh".to_owned(),
        args: Vec::new(),
        cwd: None,
        environment_ref: None,
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
        revision: ConfigRevision::from_sha256(format!("revision-{id}")),
        migration_origin: None,
    }
}

fn group(id: &str) -> HostGroup {
    HostGroup {
        id: id.to_owned(),
        name: format!("Group {id}"),
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

/// 只负责把生产导航与外部 payload 源挂进同一真实窗口，不实现任何移动逻辑。
#[derive(Clone, Copy, Eq, PartialEq)]
enum ForeignPayload {
    None,
    Tab,
    File,
}

struct ForeignDragHarness {
    navigations: Vec<Entity<HostNavigation>>,
    foreign_payload: ForeignPayload,
}

impl Render for ForeignDragHarness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let mut content = gpui_kit::div().size_full().flex();
        if self.foreign_payload != ForeignPayload::None || self.navigations.len() <= 1 {
            content = content.flex_col();
        }
        if self.foreign_payload != ForeignPayload::None {
            let source = gpui_kit::div()
                .id("foreign-tab-drag")
                .test_support()
                .w_full()
                .h(px(40.));
            let source = match self.foreign_payload {
                ForeignPayload::Tab => source.on_drag(
                    TabId::parse("foreign-tab").expect("合法 Tab ID"),
                    |_, cursor_offset, _, app| {
                        app.new(|_| HostDragPreview {
                            title: "Foreign tab".into(),
                            icon: Icon::new(IconName::SquareTerminal),
                            cursor_offset,
                        })
                    },
                ),
                ForeignPayload::File => {
                    let mut paths = gpui_kit::ExternalPaths::default();
                    paths.0.push(std::path::PathBuf::from("foreign-file.txt"));
                    source.on_drag(paths, |_, cursor_offset, _, app| {
                        app.new(|_| HostDragPreview {
                            title: "Foreign file".into(),
                            icon: Icon::new(IconName::FolderOpen),
                            cursor_offset,
                        })
                    })
                }
                ForeignPayload::None => source,
            };
            content = content.child(source.child("Foreign payload"));
        }
        for navigation in &self.navigations {
            content = content.child(navigation.clone());
        }
        content
    }
}

type HostFixture<'a> = (
    Runtime,
    Arc<DragDirectory>,
    Entity<HostNavigation>,
    std::rc::Rc<std::cell::RefCell<Vec<HostNavigationEvent>>>,
    Subscription,
    &'a mut VisualTestContext,
);

/// 挂载生产导航并等待真实目录读取完成；订阅只用于证明没有创建连接。
fn fixture(
    app: &mut TestAppContext,
    hosts: Vec<ManagedHost>,
    local_hosts: Vec<ManagedLocalHost>,
    groups: Vec<HostGroup>,
    mode: ConfigurationMode,
) -> HostFixture<'_> {
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试执行器");
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let directory = Arc::new(DragDirectory::new(hosts, local_hosts, groups));
    let service = HostManagementService::new(directory.clone());
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let navigation = cx.new(|cx| {
            HostNavigation::new(
                service,
                executor,
                ConfigurationRecoveryState {
                    mode,
                    issues: Vec::new(),
                },
                window,
                cx,
            )
        });
        *mounted.borrow_mut() = Some(navigation.clone());
        gpui_kit::base::Root::new(navigation, window, cx)
    });
    let navigation = slot.borrow_mut().take().expect("导航已挂载");
    let events = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let recorded = events.clone();
    let subscription = visual.update(|_, app| {
        app.subscribe(&navigation, move |_, event: &HostNavigationEvent, _| {
            recorded.borrow_mut().push(event.clone());
        })
    });
    settle(&runtime, visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(!navigation.loading, "隔离目录读取必须完成");
        assert!(navigation.status.is_none(), "隔离目录读取不能失败");
    });
    (runtime, directory, navigation, events, subscription, visual)
}

fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
}

fn settle(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..8 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    draw(visual);
}

fn bounds(visual: &mut VisualTestContext, selector: &str) -> gpui_kit::Bounds<gpui_kit::Pixels> {
    visual.update(|window, _| {
        let id = selector.to_owned().into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .unwrap_or_else(|| {
                panic!(
                    "控件不存在: {selector}; 当前节点: {:?}",
                    gpui_kit::base::test_support::snapshots(window)
                )
            })
            .bounds()
    })
}

fn center(visual: &mut VisualTestContext, selector: &str) -> gpui_kit::Point<gpui_kit::Pixels> {
    bounds(visual, selector).center()
}

fn click(visual: &mut VisualTestContext, selector: &str) {
    let target = center(visual, selector);
    visual.simulate_click(target, Modifiers::default());
    draw(visual);
}

fn drag_to(visual: &mut VisualTestContext, source: &str, target: &str) {
    drag_to_expect(visual, source, target, true);
}

fn begin_host_drag(
    visual: &mut VisualTestContext,
    source: &str,
    expected_active: bool,
) -> gpui_kit::Point<gpui_kit::Pixels> {
    let start = center(visual, source);
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(
        start + point(px(8.), px(0.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    draw(visual);
    assert_eq!(
        visual.update(|_, app| app.has_active_drag()),
        expected_active,
        "越过阈值后的主机拖动状态不符合预期: {source}"
    );
    start
}

fn drag_to_expect(
    visual: &mut VisualTestContext,
    source: &str,
    target: &str,
    expected_active: bool,
) {
    begin_host_drag(visual, source, expected_active);
    let end = center(visual, target);
    visual.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    draw(visual);
}

fn click_menu_item(visual: &mut VisualTestContext, label: &str) {
    let target = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| {
                node.visible() && node.role() == Some(Role::MenuItem) && node.label() == Some(label)
            })
            .unwrap_or_else(|| panic!("菜单项不存在: {label}"))
            .bounds()
            .center()
    });
    visual.simulate_mouse_move(target, None, Modifiers::default());
    visual.simulate_click(target, Modifiers::default());
    draw(visual);
}

fn assert_no_connection(events: &[HostNavigationEvent]) {
    assert!(
        !events.iter().any(|event| matches!(
            event,
            HostNavigationEvent::ConnectRequested { .. }
                | HostNavigationEvent::LocalProfileConnectRequested { .. }
                | HostNavigationEvent::OpenSftpRequested { .. }
        )),
        "移动不能创建连接或终端"
    );
}

/// 远端到空组、本地到折叠组以及最后一台主机回空根层都通过真实鼠标落点提交。
#[gpui_kit::test]
fn remote_local_and_empty_root_drop_commit_once(app: &mut TestAppContext) {
    let (runtime, directory, navigation, events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        vec![local("local-a", None)],
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("target".to_owned()))
    );
    assert_eq!(directory.move_calls(), 1);

    click(visual, "host-navigation-group-target");
    drag_to(
        visual,
        "host-navigation-profile-local-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    assert_eq!(
        directory.group_of("local-a"),
        Some(Some("target".to_owned()))
    );
    assert_eq!(directory.move_calls(), 2);
    visual.update(|_, app| {
        assert!(navigation.read(app).collapsed_groups.contains("target"));
    });
    click(visual, "host-navigation-group-target");

    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-root-drop",
    );
    settle(&runtime, visual);
    assert_eq!(directory.group_of("remote-a"), Some(None));
    assert_eq!(directory.move_calls(), 3);
    assert_no_connection(&events.borrow());
}

/// 同组、树外释放和只读均不写入；普通单击与双击仍沿用既有选择和打开行为。
#[gpui_kit::test]
fn invalid_gestures_never_write_and_click_behavior_is_preserved(app: &mut TestAppContext) {
    let (runtime, directory, _navigation, events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("source"))],
        Vec::new(),
        vec![group("source"), group("target")],
        ConfigurationMode::ReadWrite,
    );
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-source",
    );
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);

    let start = center(visual, "host-navigation-host-remote-a");
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(
        point(px(2.), px(2.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    visual.simulate_mouse_up(
        point(px(2.), px(2.)),
        MouseButton::Left,
        Modifiers::default(),
    );
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);

    click(visual, "host-navigation-host-remote-a");
    let target = center(visual, "host-navigation-host-remote-a");
    visual.simulate_event(gpui_kit::MouseDownEvent {
        position: target,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
        first_mouse: false,
    });
    visual.simulate_event(gpui_kit::MouseUpEvent {
        position: target,
        modifiers: Modifiers::default(),
        button: MouseButton::Left,
        click_count: 2,
    });
    settle(&runtime, visual);
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| matches!(event, HostNavigationEvent::ConnectRequested { .. }))
            .count(),
        1
    );
    let (runtime, _directory, _navigation, events, _subscription, visual) = fixture(
        app,
        vec![remote("keyboard-a", None)],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
    );
    click(visual, "host-navigation-host-keyboard-a");
    visual.update(|window, _| {
        let id = "host-navigation-host-keyboard-a".to_owned().into();
        let row = gpui_kit::base::test_support::find(window, &[], &id).expect("键盘主机行存在");
        assert_eq!(row.focused(), Some(true), "单击后键盘焦点必须在主机行");
    });
    assert!(
        events
            .borrow()
            .iter()
            .any(|event| matches!(event, HostNavigationEvent::HostSelected { .. }))
    );
    let enter = gpui_kit::Keystroke::parse("enter").expect("合法 Enter 键");
    visual.simulate_event(gpui_kit::KeyDownEvent {
        keystroke: enter.clone(),
        is_held: false,
        prefer_character_input: false,
    });
    visual.simulate_event(gpui_kit::KeyUpEvent { keystroke: enter });
    settle(&runtime, visual);
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|event| matches!(event, HostNavigationEvent::ConnectRequested { .. }))
            .count(),
        1,
        "Enter 只打开一次"
    );

    let (_runtime, directory, _navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("readonly-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadOnlyRecovery,
    );
    drag_to_expect(
        visual,
        "host-navigation-host-readonly-a",
        "host-navigation-group-target",
        false,
    );
    assert_eq!(directory.move_calls(), 0);
}

/// 根层提示只对真能改变归属的拖动显示，未分组源不应承诺一次无效移动。
#[gpui_kit::test]
fn root_drop_hint_is_limited_to_actionable_sources(app: &mut TestAppContext) {
    let expected = crate::locale::text("移到未分组", "Move to Ungrouped").to_string();
    let (_runtime, _directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
    );
    begin_host_drag(visual, "host-navigation-host-remote-a", true);
    let ungrouped_label = visual.update(|window, _| {
        let id = "host-navigation-root-drop".to_owned().into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("根层落点存在")
            .label()
            .map(str::to_owned)
    });
    assert_eq!(ungrouped_label, None);
    visual.update(|window, app| {
        navigation.update(app, |navigation, cx| {
            let _ = navigation.cancel_host_drag(window, cx);
        });
    });

    let (_runtime, _directory, _navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-b", Some("source"))],
        Vec::new(),
        vec![group("source")],
        ConfigurationMode::ReadWrite,
    );
    begin_host_drag(visual, "host-navigation-host-remote-b", true);
    let grouped_label = visual.update(|window, _| {
        let id = "host-navigation-root-drop".to_owned().into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("根层落点存在")
            .label()
            .map(str::to_owned)
    });
    assert_eq!(grouped_label.as_deref(), Some(expected.as_str()));
}

/// 过期源 revision 与已删除目标都在释放时拒绝提交，不能误转到未分组。
#[gpui_kit::test]
fn stale_source_and_deleted_destination_do_not_move(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    let start = center(visual, "host-navigation-host-remote-a");
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(
        start + point(px(8.), px(0.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    draw(visual);
    let target = center(visual, "host-navigation-section-target");
    visual.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
    directory.replace_revision("remote-a");
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    settle(&runtime, visual);
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);

    let start = center(visual, "host-navigation-host-remote-a");
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(
        start + point(px(8.), px(0.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    draw(visual);
    let target = center(visual, "host-navigation-section-target");
    visual.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
    directory.remove_group("target");
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    settle(&runtime, visual);
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);
    assert_eq!(directory.group_of("remote-a"), Some(None));
}

/// 30 个组的窄视口通过真实底部边缘计时滚动到达末组，纯几何同时覆盖反向和夹紧。
#[gpui_kit::test]
fn edge_scroll_reaches_offscreen_group_and_clamps_offsets(app: &mut TestAppContext) {
    let groups = (0..30)
        .map(|index| group(&format!("group-{index:02}")))
        .collect::<Vec<_>>();
    let (runtime, directory, navigation, events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("group-00"))],
        Vec::new(),
        groups,
        ConfigurationMode::ReadWrite,
    );
    visual.simulate_resize(size(px(440.), px(330.)));
    draw(visual);
    let start = center(visual, "host-navigation-host-remote-a");
    let (viewport, initial_offset) = visual.update(|_, app| {
        let navigation = navigation.read(app);
        (
            navigation.host_tree_scroll_handle.bounds(),
            navigation.host_tree_scroll_handle.offset(),
        )
    });
    let edge = point(viewport.center().x, viewport.bottom() - px(2.));
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
    assert!(visual.update(|_, app| app.has_active_drag()));
    for _ in 0..200 {
        visual.executor().advance_clock(Duration::from_millis(60));
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
        draw(visual);
        let Some(target) = visual.debug_bounds("host-navigation-section-group-29") else {
            continue;
        };
        if target.origin.y >= viewport.origin.y && target.bottom() <= viewport.bottom() {
            break;
        }
    }
    let target_bounds = bounds(visual, "host-navigation-section-group-29");
    let final_offset =
        visual.update(|_, app| navigation.read(app).host_tree_scroll_handle.offset());
    assert!(
        final_offset.y < initial_offset.y,
        "底部边缘必须推进真实滚动: viewport={viewport:?}, initial={initial_offset:?}, final={final_offset:?}"
    );
    assert!(
        target_bounds.origin.y >= viewport.origin.y && target_bounds.bottom() <= viewport.bottom(),
        "末组落点必须进入视口: viewport={viewport:?}, target={target_bounds:?}, offset={final_offset:?}"
    );
    let target = target_bounds.center();
    visual.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    settle(&runtime, visual);
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("group-29".to_owned()))
    );
    assert_eq!(directory.move_calls(), 1);
    visual.update(|_, app| {
        assert!(navigation.read(app).host_tree_scroll_handle.offset().y < initial_offset.y);
    });
    assert_no_connection(&events.borrow());

    let viewport = gpui_kit::Bounds {
        origin: point(px(0.), px(0.)),
        size: size(px(200.), px(200.)),
    };
    let current = point(px(0.), px(-100.));
    let max = point(px(0.), px(100.));
    assert_eq!(
        next_host_drag_scroll_offset(point(px(100.), px(1.)), viewport, current, max),
        Some(point(px(0.), px(-100. + 8. * (1. - 1. / 28.))))
    );
    assert_eq!(
        next_host_drag_scroll_offset(point(px(100.), px(199.)), viewport, current, max),
        None
    );
    assert_eq!(
        next_host_drag_scroll_offset(point(px(100.), px(100.)), viewport, current, max),
        None
    );
    assert_eq!(
        next_host_drag_scroll_offset(point(px(100.), px(250.)), viewport, current, max),
        None
    );
}

/// 取消同步释放唯一计时任务和预览，随后推进时钟及 MouseUp 都不会提交。
#[gpui_kit::test]
fn cancellation_releases_drag_resources_without_a_late_drop(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("group-0"))],
        Vec::new(),
        (0..12)
            .map(|index| group(&format!("group-{index}")))
            .collect(),
        ConfigurationMode::ReadWrite,
    );
    visual.simulate_resize(size(px(440.), px(300.)));
    draw(visual);
    let start = center(visual, "host-navigation-host-remote-a");
    let viewport = visual.update(|_, app| navigation.read(app).host_tree_scroll_handle.bounds());
    let edge = point(viewport.center().x, viewport.bottom() - px(2.));
    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    visual.simulate_mouse_move(
        start + point(px(8.), px(0.)),
        Some(MouseButton::Left),
        Modifiers::default(),
    );
    draw(visual);
    assert!(
        visual.update(|_, app| app.has_active_drag()),
        "越过阈值后必须仍有主机拖动"
    );
    visual.simulate_mouse_move(edge, Some(MouseButton::Left), Modifiers::default());
    assert!(visual.update(|_, app| app.has_active_drag()));
    assert!(
        visual
            .debug_bounds("host-navigation-drag-preview")
            .is_some()
    );

    let search_focus = visual.update(|_, app| {
        navigation
            .read(app)
            .search_input
            .read(app)
            .focus_handle(app)
    });
    visual.update(|window, app| window.focus(&search_focus, app));
    let cancelled = visual.update(|window, app| {
        navigation.update(app, |navigation, cx| {
            navigation.cancel_host_drag(window, cx)
        })
    });
    assert!(cancelled);
    assert!(!visual.update(|_, app| app.has_active_drag()));
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.active_host_drag.is_none());
        assert!(navigation.host_drag_task.is_none());
    });
    let offset = visual.update(|_, app| navigation.read(app).host_tree_scroll_handle.offset());
    visual.executor().advance_clock(Duration::from_millis(250));
    visual.run_until_parked();
    assert_eq!(
        visual.update(|_, app| navigation.read(app).host_tree_scroll_handle.offset()),
        offset
    );
    visual.simulate_mouse_up(edge, MouseButton::Left, Modifiers::default());
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);
    let cancelled_again = visual.update(|window, app| {
        navigation.update(app, |navigation, cx| {
            navigation.cancel_host_drag(window, cx)
        })
    });
    assert!(!cancelled_again);
}

/// 保存期间的旧读取即使迟到成功也不能覆盖目录或解锁，随后当前代次读回才结算。
#[gpui_kit::test]
fn stale_readback_cannot_overwrite_or_unlock_a_pending_move(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    directory.block_move.store(true, Ordering::SeqCst);
    directory.block_read.store(true, Ordering::SeqCst);
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    runtime.block_on(directory.move_started.notified());
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    runtime.block_on(tokio::task::yield_now());
    directory.move_release.notify_one();
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("target".to_owned()))
    );
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.rows[0].group_id, None);
        assert!(navigation.pending_host_move.is_some());
    });
    directory.read_release.notify_one();
    settle(&runtime, visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.rows[0].group_id.as_deref(), Some("target"));
        assert!(navigation.pending_host_move.is_none());
    });
    assert_eq!(directory.move_calls(), 1);
}

/// 保存后读失败保留缓存和 pending；显式重试成功后才解锁，未知结果不自动重写。
#[gpui_kit::test]
fn failed_and_unknown_readback_require_explicit_successful_retry(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    let before_feedback_bounds = bounds(visual, "host-navigation-host-remote-a");
    directory.fail_next_read.store(true, Ordering::SeqCst);
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.rows[0].group_id, None);
        assert!(navigation.pending_host_move.is_some());
        assert!(navigation.status.is_some());
    });
    let cached_row_bounds = bounds(visual, "host-navigation-host-remote-a");
    assert_eq!(
        cached_row_bounds, before_feedback_bounds,
        "反馈浮层不得挤动主机行"
    );
    let retry_bounds = bounds(visual, "host-navigation-retry-read");
    assert!(retry_bounds.size.width > px(0.));
    click(visual, "host-navigation-retry-read");
    settle(&runtime, visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.rows[0].group_id.as_deref(), Some("target"));
        assert!(navigation.pending_host_move.is_none());
    });
    assert_eq!(directory.move_calls(), 1);

    directory
        .outcomes
        .lock()
        .expect("设置未知结果")
        .push_back(MoveOutcome::Unknown { commit: false });
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-root-drop",
    );
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 2);
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("target".to_owned()))
    );
    visual.update(|_, app| assert!(navigation.read(app).pending_host_move.is_none()));
}

/// 拖动中目录开始刷新时，释放必须解释为何未移动，不能静默吞掉手势。
#[gpui_kit::test]
fn drop_during_refresh_reports_no_move(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("source"))],
        Vec::new(),
        vec![group("source"), group("target")],
        ConfigurationMode::ReadWrite,
    );
    begin_host_drag(visual, "host-navigation-host-remote-a", true);
    directory.block_read.store(true, Ordering::SeqCst);
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    let target = center(visual, "host-navigation-section-target");
    visual.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    draw(visual);
    directory.read_release.notify_one();
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("source".to_owned()))
    );
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.pending_host_move.is_none());
        let feedback = navigation.feedback.as_ref().expect("刷新中释放必须反馈");
        assert!(feedback.is_error);
        assert!(feedback.message.contains("正在刷新") || feedback.message.contains("refreshing"));
    });
}

/// 读取失败后投放必须提示先刷新，不能沿用“正在刷新”或静默失败。
#[gpui_kit::test]
fn drop_after_read_failure_reports_refresh_required(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("source"))],
        Vec::new(),
        vec![group("source"), group("target")],
        ConfigurationMode::ReadWrite,
    );
    begin_host_drag(visual, "host-navigation-host-remote-a", true);
    directory.fail_next_read.store(true, Ordering::SeqCst);
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    settle(&runtime, visual);
    let target = center(visual, "host-navigation-section-target");
    visual.simulate_mouse_move(target, Some(MouseButton::Left), Modifiers::default());
    visual.simulate_mouse_up(target, MouseButton::Left, Modifiers::default());
    draw(visual);
    assert_eq!(directory.move_calls(), 0);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        let feedback = navigation
            .feedback
            .as_ref()
            .expect("读取失败后释放必须反馈");
        assert!(feedback.is_error);
        assert!(
            feedback.message.contains("读取失败") || feedback.message.contains("could not be read")
        );
    });
}

/// 提交后主机被删除时，即使目标为未分组也不能把“缺失”误判成成功落点。
#[gpui_kit::test]
fn deleted_host_after_move_is_not_reported_as_ungrouped_success(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", Some("source"))],
        Vec::new(),
        vec![group("source")],
        ConfigurationMode::ReadWrite,
    );
    directory.remove_host_after_next_commit();
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-root-drop",
    );
    settle(&runtime, visual);
    assert_eq!(directory.committed_moves(), 1);
    assert_eq!(directory.group_of("remote-a"), None);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.pending_host_move.is_none());
        let feedback = navigation.feedback.as_ref().expect("主机消失必须反馈");
        assert!(feedback.is_error);
        assert!(
            feedback.message.contains("移动后已变化")
                || feedback.message.contains("changed after the move")
        );
    });
}

/// 普通冲突读回旧归属并显示错误，不能把写入尝试当作真实提交。
#[gpui_kit::test]
fn ordinary_conflict_preserves_source_and_does_not_commit(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    directory
        .outcomes
        .lock()
        .expect("设置普通冲突")
        .push_back(MoveOutcome::Fail(AppError::conflict("ordinary conflict")));
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    assert_eq!(directory.committed_moves(), 0);
    assert_eq!(directory.group_of("remote-a"), Some(None));
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.pending_host_move.is_none());
        assert_eq!(navigation.rows[0].group_id, None);
        assert!(
            navigation
                .feedback
                .as_ref()
                .is_some_and(|feedback| feedback.is_error)
        );
    });
}

/// 未知结果但实际已提交时只核对真实位置，不能自动重写或重复提交。
#[gpui_kit::test]
fn unknown_committed_move_is_verified_without_replay(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    directory
        .outcomes
        .lock()
        .expect("设置未知已提交")
        .push_back(MoveOutcome::Unknown { commit: true });
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 1);
    assert_eq!(directory.committed_moves(), 1);
    assert_eq!(
        directory.group_of("remote-a"),
        Some(Some("target".to_owned()))
    );
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.pending_host_move.is_none());
        let feedback = navigation
            .feedback
            .as_ref()
            .expect("未知结果必须给出核对反馈");
        assert!(!feedback.is_error);
        assert!(
            feedback.message.contains("当前位置已核对")
                || feedback.message.contains("Current location verified")
        );
    });
}

/// 保存前旧读取即使迟到失败，也不能设置新代次错误或解除移动门禁。
#[gpui_kit::test]
fn stale_failed_readback_is_ignored_until_current_read_succeeds(app: &mut TestAppContext) {
    let (runtime, directory, navigation, _events, _subscription, visual) = fixture(
        app,
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    directory.block_move.store(true, Ordering::SeqCst);
    directory.block_read.store(true, Ordering::SeqCst);
    directory.fail_next_read.store(true, Ordering::SeqCst);
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    runtime.block_on(directory.move_started.notified());
    visual.update(|_, app| navigation.update(app, |navigation, cx| navigation.reload(cx)));
    runtime.block_on(tokio::task::yield_now());
    directory.move_release.notify_one();
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    directory.read_release.notify_one();
    settle(&runtime, visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(navigation.status.is_none());
        assert!(navigation.pending_host_move.is_none());
        assert_eq!(navigation.rows[0].group_id.as_deref(), Some("target"));
    });
    assert_eq!(directory.move_calls(), 1);
}

/// 两个真实导航实体之间拖动时，目标拒绝其它实体的 payload，双方均无写入。
#[gpui_kit::test]
fn another_navigation_payload_is_not_accepted(app: &mut TestAppContext) {
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试执行器");
    let first_directory = Arc::new(DragDirectory::new(
        vec![remote("remote-a", None)],
        Vec::new(),
        Vec::new(),
    ));
    let second_directory = Arc::new(DragDirectory::new(
        Vec::new(),
        Vec::new(),
        vec![group("target")],
    ));
    let first_service = HostManagementService::new(first_directory.clone());
    let second_service = HostManagementService::new(second_directory.clone());
    let first_executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let second_executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let slot = std::rc::Rc::new(std::cell::RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let recovery = ConfigurationRecoveryState {
            mode: ConfigurationMode::ReadWrite,
            issues: Vec::new(),
        };
        let first = cx.new(|cx| {
            HostNavigation::new(first_service, first_executor, recovery.clone(), window, cx)
        });
        let second =
            cx.new(|cx| HostNavigation::new(second_service, second_executor, recovery, window, cx));
        *mounted.borrow_mut() = Some((first.clone(), second.clone()));
        let harness = cx.new(|_| ForeignDragHarness {
            navigations: vec![first, second],
            foreign_payload: ForeignPayload::None,
        });
        gpui_kit::base::Root::new(harness, window, cx)
    });
    let (first, second) = slot.borrow_mut().take().expect("两个导航已挂载");
    settle(&runtime, visual);
    drag_to(
        visual,
        "host-navigation-host-remote-a",
        "host-navigation-section-target",
    );
    settle(&runtime, visual);
    assert_eq!(first_directory.move_calls(), 0);
    assert_eq!(second_directory.move_calls(), 0);
    visual.update(|_, app| {
        assert!(first.read(app).active_host_drag.is_none());
        assert!(second.read(app).pending_host_move.is_none());
    });
}

/// Tab 类型 payload 可真实拖动，但主机分组只接受 HostNavigationDrag，不会误提交。
#[gpui_kit::test]
fn tab_payload_is_not_accepted_by_host_groups(app: &mut TestAppContext) {
    foreign_payload_is_not_accepted_by_host_groups(app, ForeignPayload::Tab);
}

/// 外部文件路径同样经过真实 mouse down/move/up，不得被解释成主机 ID。
#[gpui_kit::test]
fn file_payload_is_not_accepted_by_host_groups(app: &mut TestAppContext) {
    foreign_payload_is_not_accepted_by_host_groups(app, ForeignPayload::File);
}

fn foreign_payload_is_not_accepted_by_host_groups(
    app: &mut TestAppContext,
    foreign_payload: ForeignPayload,
) {
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试执行器");
    let directory = Arc::new(DragDirectory::new(
        vec![remote("remote-a", None)],
        Vec::new(),
        vec![group("target")],
    ));
    let service = HostManagementService::new(directory.clone());
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let (_, visual) = app.add_window_view(move |window, cx| {
        let navigation = cx.new(|cx| {
            HostNavigation::new(
                service,
                executor,
                ConfigurationRecoveryState {
                    mode: ConfigurationMode::ReadWrite,
                    issues: Vec::new(),
                },
                window,
                cx,
            )
        });
        let harness = cx.new(|_| ForeignDragHarness {
            navigations: vec![navigation],
            foreign_payload,
        });
        gpui_kit::base::Root::new(harness, window, cx)
    });
    settle(&runtime, visual);
    drag_to(visual, "foreign-tab-drag", "host-navigation-section-target");
    settle(&runtime, visual);
    assert_eq!(directory.move_calls(), 0);
    assert_eq!(directory.group_of("remote-a"), Some(None));
}

/// 本地与远端菜单使用同一移动用例；菜单打开后的源 revision 变化不能覆盖新配置。
#[gpui_kit::test]
fn local_menu_shares_move_path_and_rejects_stale_revision(app: &mut TestAppContext) {
    let (runtime, directory, _navigation, _events, _subscription, visual) = fixture(
        app,
        Vec::new(),
        vec![local("local-a", None)],
        vec![group("target")],
        ConfigurationMode::ReadWrite,
    );
    let target = center(visual, "host-navigation-profile-local-a");
    visual.simulate_mouse_down(target, MouseButton::Right, Modifiers::default());
    visual.simulate_mouse_up(target, MouseButton::Right, Modifiers::default());
    draw(visual);
    click_menu_item(visual, &crate::locale::text("移动到分组", "Move to group"));
    draw(visual);
    click_menu_item(visual, "Group target");
    settle(&runtime, visual);
    assert_eq!(
        directory.group_of("local-a"),
        Some(Some("target".to_owned()))
    );
    assert_eq!(directory.move_calls(), 1);

    directory
        .outcomes
        .lock()
        .expect("设置冲突")
        .push_back(MoveOutcome::Fail(AppError::conflict("stale")));
    let target = center(visual, "host-navigation-profile-local-a");
    visual.simulate_mouse_down(target, MouseButton::Right, Modifiers::default());
    visual.simulate_mouse_up(target, MouseButton::Right, Modifiers::default());
    draw(visual);
    click_menu_item(visual, &crate::locale::text("移动到分组", "Move to group"));
    draw(visual);
    directory.replace_revision("local-a");
    click_menu_item(visual, &crate::locale::text("未分组", "Ungrouped"));
    settle(&runtime, visual);
    assert_eq!(directory.committed_moves(), 1);
    assert_eq!(
        directory.group_of("local-a"),
        Some(Some("target".to_owned()))
    );
}
