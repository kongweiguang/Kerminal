//! 生产主机导航的真实右键与菜单动作回归，仓储仅保留内存快照。
//!
//! @author kongweiguang

use std::{cell::RefCell, collections::BTreeMap, rc::Rc, sync::Arc};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Entity, Focusable as _, MouseButton, Role, Subscription, TestAppContext,
    VisualTestContext, Window,
};
use kerminal_application::{
    AppResult, ApplicationTaskExecutor,
    connections::{
        host_types::{HostConfigurationCatalog, PluginHost, PluginHostService},
        management::{
            HostDocumentRevision, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
            HostGroupSnapshot, HostManagementRepository, HostManagementService,
        },
    },
    plugins::{PluginId, PluginRegistry, PluginRegistryBuilder},
    workspace::WorkspaceTargetRef,
};
use kerminal_domain::preferences::{ConfigurationMode, ConfigurationRecoveryState};
use tokio::runtime::{Builder, Runtime};

use super::super::HostNavigation;
use super::{ConfigRevision, HostGroup, HostId, HostNavigationEvent, ManagedHost, RemoteProtocol};

/// 目录只返回固定快照；任何写操作都代表菜单测试越过范围。
struct MenuDirectory {
    rows: Vec<ManagedHost>,
    plugin_rows: Vec<PluginHost>,
    groups: Vec<HostGroup>,
}

#[async_trait]
impl HostManagementRepository for MenuDirectory {
    /// 打开菜单只产生意图，不应进入持久化门禁。
    async fn ensure_host_writable(&self) -> AppResult<()> {
        panic!("菜单回归不得申请写入")
    }

    /// 实际导航构造器仍执行异步 reload，数据来自隔离快照。
    async fn list_hosts(&self) -> AppResult<Vec<ManagedHost>> {
        Ok(self.rows.clone())
    }

    /// 按稳定身份读取快照，不打开用户配置。
    async fn load_host(&self, host_id: &HostId) -> AppResult<Option<ManagedHost>> {
        Ok(self.rows.iter().find(|host| &host.id == host_id).cloned())
    }

    /// 扩展主机与内置主机来自同一隔离快照，导航据此复核插件启停。
    async fn list_plugin_hosts(&self) -> AppResult<Vec<PluginHost>> {
        Ok(self.plugin_rows.clone())
    }

    /// 测试不提交任何新建或编辑草稿。
    async fn save_host(
        &self,
        _: HostId,
        _: HostDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<ManagedHost> {
        panic!("菜单回归不得保存主机")
    }

    /// 测试不删除主机，即使菜单中存在删除入口。
    async fn delete_host(&self, _: &HostId, _: &ConfigRevision) -> AppResult<ManagedHost> {
        panic!("菜单回归不得删除主机")
    }

    /// 保持分组和主机来自同一隔离目录，覆盖真实 reload 的组匹配。
    async fn load_groups(&self) -> AppResult<HostGroupSnapshot> {
        Ok(HostGroupSnapshot {
            groups: self.groups.clone(),
            revision: None,
        })
    }

    /// 只验证菜单事件，分组写入必须由其它事务测试覆盖。
    async fn save_group(
        &self,
        _: HostGroupDraft,
        _: Option<ConfigRevision>,
    ) -> AppResult<HostGroupSnapshot> {
        panic!("菜单回归不得保存分组")
    }

    /// 防止错误菜单动作触发配置删除或成员迁移。
    async fn delete_group_and_reassign(
        &self,
        _: String,
        _: Option<String>,
        _: ConfigRevision,
        _: Vec<HostDocumentRevision>,
    ) -> AppResult<HostGroupDeletionReceipt> {
        panic!("菜单回归不得删除分组")
    }
}

/// 在独立 Tokio 执行器和 GPUI 测试窗口中挂载真正导航，并保留订阅直到断言完成。
fn fixture(
    app: &mut TestAppContext,
    rows: Vec<ManagedHost>,
    groups: Vec<HostGroup>,
    mode: ConfigurationMode,
    events: Rc<RefCell<Vec<HostNavigationEvent>>>,
) -> (
    Runtime,
    Entity<HostNavigation>,
    Subscription,
    &mut VisualTestContext,
) {
    fixture_with_plugins(app, rows, Vec::new(), groups, mode, events, None)
}

/// 可注入冻结插件视图的同一挂载入口；插件主机仍经过生产仓储与导航 reload。
fn fixture_with_plugins(
    app: &mut TestAppContext,
    rows: Vec<ManagedHost>,
    plugin_rows: Vec<PluginHost>,
    groups: Vec<HostGroup>,
    mode: ConfigurationMode,
    events: Rc<RefCell<Vec<HostNavigationEvent>>>,
    plugins: Option<Arc<PluginRegistry>>,
) -> (
    Runtime,
    Entity<HostNavigation>,
    Subscription,
    &mut VisualTestContext,
) {
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let hosts = HostManagementService::new(Arc::new(MenuDirectory {
        rows,
        plugin_rows,
        groups,
    }));
    let hosts = match plugins {
        Some(plugins) => hosts.with_plugins(plugins),
        None => hosts,
    };
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let navigation = cx.new(|cx| {
            HostNavigation::new(
                hosts,
                None,
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
    let navigation = slot.borrow_mut().take().expect("生产导航已挂载");
    let subscription = visual.update(|_, app| {
        app.subscribe(&navigation, move |_, event: &HostNavigationEvent, _| {
            events.borrow_mut().push(event.clone());
        })
    });
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
    draw(visual);
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(!navigation.loading, "隔离目录 reload 必须完成");
        assert!(navigation.status.is_none(), "隔离目录读取不能失败");
    });
    (runtime, navigation, subscription, visual)
}

/// 构造只关闭指定内置单元的冻结视图，复用生产声明与归属映射。
fn registry_with_disabled(disabled: &[&str]) -> Arc<PluginRegistry> {
    let mut builder = PluginRegistryBuilder::new();
    builder.register_builtins().expect("内置声明应可注册");
    let states: BTreeMap<PluginId, bool> = disabled
        .iter()
        .map(|id| (PluginId::new(*id).expect("插件 ID 应合法"), false))
        .collect();
    Arc::new(
        builder
            .freeze_with_builtin_states(&states)
            .expect("内置状态应可冻结"),
    )
}

/// 插件主机快照保持最小可运行身份，供导航与 PluginHostService 使用同一 provider/type。
fn plugin_host(provider_id: &str, type_id: &str) -> PluginHost {
    PluginHost {
        id: HostId::new(),
        provider_id: provider_id.to_owned(),
        type_id: type_id.to_owned(),
        name: "Plugin fixture".to_owned(),
        group_id: None,
        sort_order: 0,
        configuration_version: 1,
        configuration: Some(toml::from_str("text = 'fixture'").expect("合法测试配置")),
        runtime_available: true,
        revision: ConfigRevision::from_sha256("plugin-fixture-revision".to_owned()),
    }
}

/// Kit 全局初始化只做一次；参数化场景可在同一测试应用中创建多个窗口。
fn init(app: &mut TestAppContext) {
    app.update(|app| {
        crate::init(app);
        app.set_reduce_motion(true);
    });
}

/// 让 deferred 菜单及其无障碍节点完成真实绘制，再读取当前帧事实。
fn draw(visual: &mut VisualTestContext) {
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
    visual.run_until_parked();
    visual.update(|window, app| window.draw(app).clear(app));
}

/// 使用当前控件边界执行真实右键，确保没有绕过命中测试或菜单事件冒泡。
fn open_menu(visual: &mut VisualTestContext, selector: &str) -> Vec<String> {
    draw(visual);
    let target = visual.update(|window, _| {
        let id = selector.to_owned().into();
        gpui_kit::base::test_support::find(window, &[], &id)
            .expect("导航行必须存在")
            .bounds()
            .center()
    });
    visual.simulate_mouse_down(target, MouseButton::Right, Default::default());
    visual.simulate_mouse_up(target, MouseButton::Right, Default::default());
    draw(visual);
    visual.update(|window, _| {
        let nodes = gpui_kit::base::test_support::snapshots(window);
        assert_eq!(
            nodes
                .iter()
                .filter(|node| node.visible() && node.role() == Some(Role::Menu))
                .count(),
            1,
            "单次右键不得同时打开行菜单与背景菜单",
        );
        nodes
            .into_iter()
            .filter(|node| node.visible() && node.role() == Some(Role::MenuItem))
            .filter_map(|node| node.label().map(str::to_owned))
            .collect()
    })
}

/// 从当前菜单可访问节点取得动作坐标并点击，避免使用与菜单顺序耦合的索引。
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
    visual.simulate_mouse_move(target, None, Default::default());
    visual.simulate_click(target, Default::default());
    draw(visual);
}

/// 验证两侧图标共用外框留白，避免清除按钮内缩；真实点击还须恢复目录与输入焦点。
/// @author kongweiguang
#[gpui_kit::test]
fn clear_search_restores_directory_and_focus(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, &[]);
    let selector = format!("host-navigation-host-{}", row.id.as_str()).into();
    let (_runtime, navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        Vec::new(),
        ConfigurationMode::ReadWrite,
        Rc::new(RefCell::new(Vec::new())),
    );
    let label = crate::locale::text("清除搜索", "Clear search");
    let clear_button = |window: &Window| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .find(|node| node.visible() && node.label() == Some(label.as_ref()))
    };

    visual.update(|window, app| {
        assert!(clear_button(window).is_none());
        navigation
            .read(app)
            .search_input
            .read(app)
            .focus_handle(app)
            .focus(window, app);
    });
    draw(visual);
    visual.simulate_input("unmatched");
    draw(visual);
    let target = visual.update(|window, app| {
        assert_eq!(navigation.read(app).search, "unmatched");
        assert!(gpui_kit::base::test_support::find(window, &[], &selector).is_none());
        let frame =
            gpui_kit::base::test_support::find(window, &[], &"host-navigation-search-frame".into())
                .expect("搜索外框应存在")
                .bounds();
        let icon =
            gpui_kit::base::test_support::find(window, &[], &"host-navigation-search-icon".into())
                .expect("搜索图标应存在")
                .bounds();
        let clear = clear_button(window).expect("有搜索内容时可清除").bounds();
        assert_eq!(icon.left() - frame.left(), frame.right() - clear.right());
        clear.center()
    });
    visual.simulate_click(target, Default::default());
    draw(visual);

    visual.update(|window, app| {
        let navigation = navigation.read(app);
        let input = navigation.search_input.read(app);
        assert!(input.value().is_empty());
        assert!(navigation.search.is_empty());
        assert!(input.focus_handle(app).is_focused(window));
        assert!(clear_button(window).is_none());
        assert!(gpui_kit::base::test_support::find(window, &[], &selector).is_some());
    });
}

/// 分组测试统一使用真实目录中可匹配的 ID，不把树中的显示标签当作分组身份。
fn group(id: &str) -> HostGroup {
    HostGroup {
        id: id.to_owned(),
        name: "Fixture group".to_owned(),
        sort_order: 0,
        created_at: "1".to_owned(),
        updated_at: "1".to_owned(),
    }
}

/// 右键远端行只打开自身菜单，同组新建携带真实组或未分组，空目的列表不显示移动子菜单。
#[gpui_kit::test]
fn remote_menu_new_connection_uses_clicked_group(app: &mut TestAppContext) {
    init(app);
    for (source_group, saved_groups, expected_group) in [
        (None, Vec::new(), None),
        (Some("group:deleted"), Vec::new(), None),
        (
            Some("group:valid"),
            vec![group("group:valid")],
            Some("group:valid"),
        ),
    ] {
        let row = super::host("fixture", "fixture.invalid", source_group, &[]);
        let selector = format!("host-navigation-host-{}", row.id.as_str());
        let events = Rc::new(RefCell::new(Vec::new()));
        let (_runtime, _navigation, _subscription, visual) = fixture(
            app,
            vec![row],
            saved_groups,
            ConfigurationMode::ReadWrite,
            events.clone(),
        );
        let labels = open_menu(visual, &selector);
        assert!(
            !labels.contains(&crate::locale::text("刷新主机列表", "Refresh hosts").to_string())
        );
        assert!(labels.contains(
            &crate::locale::text("添加同组连接…", "Add connection to this group…").to_string()
        ));
        if expected_group.is_none() {
            assert!(
                !labels.contains(&crate::locale::text("移动到分组", "Move to group").to_string())
            );
        }
        click_menu_item(
            visual,
            &crate::locale::text("添加同组连接…", "Add connection to this group…"),
        );
        assert_eq!(
            &*events.borrow(),
            &[HostNavigationEvent::CreateHostRequested {
                group_id: expected_group.map(str::to_owned),
                group_explicit: true,
            }],
        );
    }
}

/// 背景“添加连接…”不携带目标分组并标记为非显式，表单才能沿用上次选择；
/// 行与分组入口始终标记显式，未分组目标不会被默认值改写。
/// @author kongweiguang
#[gpui_kit::test]
fn background_menu_marks_group_not_explicit(app: &mut TestAppContext) {
    init(app);
    let events = Rc::new(RefCell::new(Vec::new()));
    let row = super::host("fixture", "fixture.invalid", None, &[]);
    let (_runtime, _navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events.clone(),
    );
    draw(visual);
    // 侧栏底部空白属于背景菜单；树本身没有独立 ElementId，不能按目录元素定位。
    let target = visual.update(|window, _| {
        let viewport = window.visual_viewport_bounds();
        gpui_kit::point(
            viewport.origin.x + gpui_kit::px(80.),
            viewport.bottom() - gpui_kit::px(80.),
        )
    });
    visual.simulate_mouse_down(target, MouseButton::Right, Default::default());
    visual.simulate_mouse_up(target, MouseButton::Right, Default::default());
    draw(visual);
    let refresh = crate::locale::text("刷新主机列表", "Refresh hosts").to_string();
    let add = crate::locale::text("添加连接…", "Add connection…").to_string();
    let labels = visual.update(|window, _| {
        gpui_kit::base::test_support::snapshots(window)
            .into_iter()
            .filter(|node| node.visible() && node.role() == Some(Role::MenuItem))
            .filter_map(|node| node.label().map(str::to_owned))
            .collect::<Vec<_>>()
    });
    assert!(labels.contains(&refresh), "空白区右键应打开背景菜单");
    assert!(labels.contains(&add));
    click_menu_item(visual, &add);
    assert_eq!(
        &*events.borrow(),
        &[HostNavigationEvent::CreateHostRequested {
            group_id: None,
            group_explicit: false,
        }],
    );
}

/// 协议菜单不再提供主机容器入口；SFTP 转 SSH 的点击事件保留被右击的准确主机身份。
#[gpui_kit::test]
fn protocol_menu_omits_containers_and_routes_sftp_copy(app: &mut TestAppContext) {
    init(app);
    for protocol in [
        RemoteProtocol::Ssh,
        RemoteProtocol::Sftp,
        RemoteProtocol::Serial,
    ] {
        let mut row = super::host("fixture", "fixture.invalid", None, &[]);
        row.protocol = protocol;
        let id = row.id.clone();
        let selector = format!("host-navigation-host-{}", id.as_str());
        let events = Rc::new(RefCell::new(Vec::new()));
        let (_runtime, _navigation, _subscription, visual) = fixture(
            app,
            vec![row],
            Vec::new(),
            ConfigurationMode::ReadWrite,
            events.clone(),
        );
        let labels = open_menu(visual, &selector);
        assert!(
            !labels.contains(&crate::locale::text("容器", "Containers").to_string()),
            "主机右键不得提供容器入口"
        );
        let copy_ssh =
            crate::locale::text("复制为 SSH 主机…", "Duplicate as SSH host…").to_string();
        assert_eq!(labels.contains(&copy_ssh), protocol == RemoteProtocol::Sftp);
        if protocol == RemoteProtocol::Serial {
            assert!(!labels.contains(
                &crate::locale::text("复制主机 IP / 主机名", "Copy host IP / hostname").to_string()
            ));
        }
        if protocol == RemoteProtocol::Sftp {
            click_menu_item(visual, &copy_ssh);
            let events = events.borrow();
            assert_eq!(
                events.first(),
                Some(&HostNavigationEvent::HostSelected {
                    host_id: id.clone()
                }),
            );
            assert_eq!(
                events.last(),
                Some(&HostNavigationEvent::CopyHostAsSshRequested { host_id: id }),
            );
        }
    }
}

/// 快速导航展开命中的分组；失效快照不得清空选择或发出导航事件。
/// @author kongweiguang
#[gpui_kit::test]
fn external_host_selection_rejects_stale_snapshot(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", Some("source"), &[]);
    let host_id = row.id.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (runtime, navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        vec![group("source"), group("other")],
        ConfigurationMode::ReadWrite,
        events.clone(),
    );

    visual.update(|_, app| assert!(navigation.read(app).collapsed_groups.contains("source")));
    let selected = visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.select_host_from_external(&host_id, cx)
        })
    });
    assert!(selected);
    visual.update(|_, app| assert!(!navigation.read(app).collapsed_groups.contains("source")));
    assert_eq!(
        &*events.borrow(),
        &[HostNavigationEvent::HostSelected {
            host_id: host_id.clone()
        }],
    );

    events.borrow_mut().clear();
    let stale_host_id = HostId::new();
    let selected = visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.select_host_from_external(&stale_host_id, cx)
        })
    });
    assert!(!selected);
    assert!(events.borrow().is_empty());
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.selected_host.as_ref(), Some(&host_id));
        assert!(
            navigation
                .feedback
                .as_ref()
                .is_some_and(|feedback| feedback.is_error)
        );
    });
    for _ in 0..4 {
        visual.run_until_parked();
        runtime.block_on(tokio::task::yield_now());
    }
}

/// 只读恢复隐藏配置写入，也不提供已移除的连接详情入口。
#[gpui_kit::test]
fn readonly_host_menu_hides_configuration_writes(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, &[]);
    let selector = format!("host-navigation-host-{}", row.id.as_str());
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, _navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        Vec::new(),
        ConfigurationMode::ReadOnlyRecovery,
        events,
    );
    let labels = open_menu(visual, &selector);
    for (zh, en) in [
        ("添加同组连接…", "Add connection to this group…"),
        ("编辑…", "Edit…"),
        ("复制主机配置…", "Copy host configuration…"),
        ("删除主机…", "Delete host…"),
        ("查看连接详情", "Connection details"),
    ] {
        assert!(!labels.contains(&crate::locale::text(zh, en).to_string()));
    }
}

/// 本地 Profile 取它所在的真实分组，悬空引用与无分组都不能泄露失效的组 ID。
#[gpui_kit::test]
fn local_menu_new_connection_uses_its_displayed_group(app: &mut TestAppContext) {
    init(app);
    for (source_group, saved_groups, expected_group) in [
        (None, Vec::new(), None),
        (Some("group:deleted"), Vec::new(), None),
        (
            Some("group:valid"),
            vec![group("group:valid")],
            Some("group:valid"),
        ),
    ] {
        let profile = super::profile("fixture-local", source_group);
        let events = Rc::new(RefCell::new(Vec::new()));
        let (_runtime, navigation, _subscription, visual) = fixture(
            app,
            Vec::new(),
            saved_groups,
            ConfigurationMode::ReadWrite,
            events.clone(),
        );
        visual.update(|_, app| {
            navigation.update(app, |navigation, cx| {
                navigation.profile_rows = vec![profile];
                cx.notify();
            })
        });
        let labels = open_menu(visual, "host-navigation-profile-fixture-local");
        assert!(
            !labels.contains(&crate::locale::text("刷新主机列表", "Refresh hosts").to_string())
        );
        click_menu_item(
            visual,
            &crate::locale::text("添加同组连接…", "Add connection to this group…"),
        );
        assert_eq!(
            &*events.borrow(),
            &[HostNavigationEvent::CreateHostRequested {
                group_id: expected_group.map(str::to_owned),
                group_explicit: true,
            }],
        );
    }
}

/// 固定容器被嵌套在宿主行下方；每个动作仍须独占菜单并携带同一完整 pin 身份。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_menu_keeps_details_files_and_remove_on_clicked_pin(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, &[]);
    let pin = super::WorkspacePinnedContainer {
        target: super::ContainerTarget {
            host_id: Some(row.id.clone()),
            runtime: super::ContainerRuntime::Podman,
            container_id: "sidebar-container".into(),
        },
        name: "api".into(),
        description: "fixture".into(),
        group_id: None,
    };
    let stable_id = pin.stable_id();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events.clone(),
    );
    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.pinned_containers = vec![pin];
            cx.notify();
        })
    });
    for (zh, en, expected) in [
        (
            "查看容器详情",
            "Container details",
            HostNavigationEvent::PinnedContainerDetailsRequested {
                stable_id: stable_id.clone(),
            },
        ),
        (
            "打开文件",
            "Open files",
            HostNavigationEvent::PinnedContainerFilesRequested {
                stable_id: stable_id.clone(),
            },
        ),
        (
            "从侧栏移除",
            "Remove from sidebar",
            HostNavigationEvent::PinnedContainerRemoveRequested {
                stable_id: stable_id.clone(),
            },
        ),
    ] {
        let labels = open_menu(visual, &format!("host-navigation-container-{stable_id}"));
        let label = crate::locale::text(zh, en);
        assert!(labels.contains(&label.to_string()));
        assert!(
            !labels.contains(&crate::locale::text("刷新主机列表", "Refresh hosts").to_string())
        );
        events.borrow_mut().clear();
        click_menu_item(visual, &label);
        assert_eq!(&*events.borrow(), &[expected]);
    }
}

/// SSH 禁用后固定容器仍保留详情与移除，但“打开文件”经 SSH 执行必然失败，菜单中隐藏。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_menu_hides_files_when_ssh_disabled(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, &[]);
    let pin = super::WorkspacePinnedContainer {
        target: super::ContainerTarget {
            host_id: Some(row.id.clone()),
            runtime: super::ContainerRuntime::Podman,
            container_id: "sidebar-container".into(),
        },
        name: "api".into(),
        description: "fixture".into(),
        group_id: None,
    };
    let stable_id = pin.stable_id();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, navigation, _subscription, visual) = fixture_with_plugins(
        app,
        vec![row],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events,
        Some(registry_with_disabled(&["kerminal.ssh"])),
    );
    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.pinned_containers = vec![pin];
            cx.notify();
        })
    });
    let labels = open_menu(visual, &format!("host-navigation-container-{stable_id}"));
    assert!(labels.contains(&crate::locale::text("查看容器详情", "Container details").to_string()));
    assert!(
        !labels.contains(&crate::locale::text("打开文件", "Open files").to_string()),
        "SSH 禁用时不得保留必然失败的打开文件入口"
    );
    assert!(labels.contains(&crate::locale::text("从侧栏移除", "Remove from sidebar").to_string()));
}

/// 禁用 SSH 后既有主机仍完整列出，但不再进入分屏候选，双击也不产生连接意图。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_protocol_keeps_host_listed_but_rejects_connect_and_split(app: &mut TestAppContext) {
    init(app);
    let registry = registry_with_disabled(&["kerminal.ssh"]);
    let row = super::host("ssh-host", "ssh.invalid", None, &[]);
    let host_id = row.id.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, navigation, _subscription, visual) = fixture_with_plugins(
        app,
        vec![row],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events.clone(),
        Some(registry),
    );

    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.rows.len(), 1, "禁用协议不能删除既有主机行");
        assert!(
            navigation
                .split_target_options("")
                .iter()
                .all(|option| option.target
                    != WorkspaceTargetRef::Ssh {
                        host_id: host_id.clone()
                    }),
            "禁用协议不能成为分屏目标"
        );
    });

    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.request_connect(host_id.clone(), cx)
        });
    });
    draw(visual);
    assert!(
        events.borrow().is_empty(),
        "禁用协议必须在导航层拒绝连接，不创建会话意图"
    );
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(
            navigation
                .feedback
                .as_ref()
                .is_some_and(|feedback| feedback.is_error)
        );
        assert!(
            navigation.selected_host.is_none(),
            "被拒绝的连接不能改变选择"
        );
    });
}

/// SSH 单元禁用时，SSH 主机的文件与传输动作都依赖同一连接能力，必须一并隐藏。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_ssh_hides_sftp_actions_on_ssh_host(app: &mut TestAppContext) {
    init(app);
    let registry = registry_with_disabled(&["kerminal.ssh"]);
    let row = super::host("ssh-host", "ssh.invalid", None, &[]);
    let selector = format!("host-navigation-host-{}", row.id.as_str());
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, _navigation, _subscription, visual) = fixture_with_plugins(
        app,
        vec![row],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events,
        Some(registry),
    );

    let labels = open_menu(visual, &selector);
    for (zh, en) in [
        ("打开文件", "Open files"),
        ("新建传输标签", "New transfer tab"),
        ("复制为 SSH 主机…", "Duplicate as SSH host…"),
    ] {
        assert!(
            !labels.contains(&crate::locale::text(zh, en).to_string()),
            "SSH 禁用时不应出现 {zh}"
        );
    }
}

/// 菜单闭包或旧帧在插件停用后仍可能调用动作入口；SFTP 与独立传输请求必须按当前
/// 启用视图拒绝，且不能改变既有选择。SSH 主机的文件动作依赖 SSH，即使 SFTP 仍启用
/// 也不能绕过硬性连接能力。
/// @author kongweiguang
#[gpui_kit::test]
fn stale_sftp_requests_are_rejected_when_required_units_disabled(app: &mut TestAppContext) {
    init(app);
    for disabled in [&["kerminal.sftp"][..], &["kerminal.ssh"][..]] {
        let row = super::host("fixture", "fixture.invalid", None, &[]);
        let host_id = row.id.clone();
        let unchanged = HostId::new();
        let events = Rc::new(RefCell::new(Vec::new()));
        let (_runtime, navigation, _subscription, visual) = fixture_with_plugins(
            app,
            vec![row],
            Vec::new(),
            Vec::new(),
            ConfigurationMode::ReadWrite,
            events.clone(),
            Some(registry_with_disabled(disabled)),
        );
        for (action, label) in [("sftp", "打开文件"), ("transfer", "新建传输标签")] {
            visual.update(|_, app| {
                navigation.update(app, |navigation, cx| {
                    navigation.selected_host = Some(unchanged.clone());
                    match action {
                        "sftp" => navigation.request_open_sftp(host_id.clone(), cx),
                        _ => navigation.request_create_sftp_transfer_tab(host_id.clone(), cx),
                    }
                });
            });
            assert!(
                events.borrow().is_empty(),
                "disabled {disabled:?}: {label}不得派发事件"
            );
            visual.update(|_, app| {
                let navigation = navigation.read(app);
                assert!(
                    navigation
                        .feedback
                        .as_ref()
                        .is_some_and(|feedback| feedback.is_error),
                    "disabled {disabled:?}: {label}应给出错误状态"
                );
                assert_eq!(
                    navigation.selected_host.as_ref(),
                    Some(&unchanged),
                    "disabled {disabled:?}: {label}不得改变选择"
                );
            });
        }
    }
}

/// 纯 SFTP 主机不依赖 SSH 单元，SSH 禁用时仍保留自己的打开文件与传输入口。
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_ssh_keeps_sftp_only_host_file_actions(app: &mut TestAppContext) {
    init(app);
    let registry = registry_with_disabled(&["kerminal.ssh"]);
    let mut row = super::host("sftp-host", "sftp.invalid", None, &[]);
    row.protocol = RemoteProtocol::Sftp;
    row.sftp_options.enabled = true;
    let host_id = row.id.clone();
    let selector = format!("host-navigation-host-{}", row.id.as_str());
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, navigation, _subscription, visual) = fixture_with_plugins(
        app,
        vec![row],
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events.clone(),
        Some(registry),
    );

    let labels = open_menu(visual, &selector);
    assert!(
        labels.contains(&crate::locale::text("打开文件", "open files").to_string()),
        "纯 SFTP 主机的打开文件入口不应受 SSH 单元影响"
    );
    assert!(
        labels.contains(&crate::locale::text("新建传输标签", "New transfer tab").to_string()),
        "纯 SFTP 主机的传输标签入口不应受 SSH 单元影响"
    );
    assert!(
        !labels.contains(
            &crate::locale::text("复制为 SSH 主机…", "Duplicate as SSH host…").to_string()
        ),
        "SSH 禁用时复制为 SSH 入口应隐藏"
    );

    // 旧帧入口仍需实际派发确认，不能只凭菜单可见推断动作仍可用。
    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.request_open_sftp(host_id.clone(), cx);
        });
    });
    assert!(
        events
            .borrow()
            .contains(&HostNavigationEvent::OpenSftpRequested {
                host_id: host_id.clone(),
            }),
        "纯 SFTP 主机在 SSH 禁用时仍应允许打开文件"
    );
    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            // 连续动作共用重复双击保护；测试显式清空以分别验证两个入口。
            navigation.last_open_request = None;
            navigation.request_create_sftp_transfer_tab(host_id.clone(), cx);
        });
    });
    assert!(
        events
            .borrow()
            .contains(&HostNavigationEvent::CreateSftpTransferTabRequested {
                host_id: host_id.clone(),
            }),
        "纯 SFTP 主机在 SSH 禁用时仍应允许新建传输标签"
    );
}

/// 禁用插件后扩展主机仍保留在树中，但打开和选择入口都不派发运行意图。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_plugin_host_remains_listed_but_cannot_open_or_select(app: &mut TestAppContext) {
    init(app);
    let registry = registry_with_disabled(&["kerminal.sftp"]);
    let host = plugin_host("kerminal", "sftp");
    let host_id = host.id.clone();
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, navigation, _subscription, visual) = fixture_with_plugins(
        app,
        Vec::new(),
        vec![host],
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events.clone(),
        Some(registry),
    );

    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert_eq!(navigation.plugin_rows.len(), 1, "禁用插件不能删除配置行");
    });
    visual.update(|_, app| {
        navigation.update(app, |navigation, cx| {
            navigation.request_plugin_host_open(host_id.clone(), cx);
            navigation.select_plugin_host(host_id.clone(), cx);
        });
    });
    draw(visual);
    assert!(events.borrow().is_empty(), "禁用插件不得发出打开或选择事件");
    visual.update(|_, app| {
        let navigation = navigation.read(app);
        assert!(
            navigation
                .feedback
                .as_ref()
                .is_some_and(|feedback| feedback.is_error)
        );
        assert!(navigation.selected_host.is_none());
    });
}

/// PluginHostService 与主机管理服务复用同一冻结视图，避免打开入口各自维护启停映射。
///
/// @author kongweiguang
#[test]
fn plugin_host_service_reuses_the_same_frozen_gate() {
    let host = plugin_host("kerminal", "sftp");
    let registry = registry_with_disabled(&["kerminal.sftp"]);
    let hosts = HostManagementService::new(Arc::new(MenuDirectory {
        rows: Vec::new(),
        plugin_rows: vec![host.clone()],
        groups: Vec::new(),
    }))
    .with_plugins(registry);
    assert!(!hosts.plugin_host_enabled(&host));

    let plugin_service = PluginHostService::new(
        hosts,
        Arc::new(HostConfigurationCatalog::new(std::iter::empty()).expect("空目录应有效")),
    );
    assert!(!plugin_service.host_enabled(&host));
}

/// SFTP 单元禁用时导航头不再渲染工作台入口；按钮隐藏与 Root 事件门禁形成双层
/// 防护，旧帧点击或其它快捷键路径仍会被拒绝。
///
/// @author kongweiguang
#[gpui_kit::test]
fn disabled_sftp_hides_workbench_entry(app: &mut TestAppContext) {
    init(app);
    let registry = registry_with_disabled(&["kerminal.sftp"]);
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, _navigation, _subscription, visual) = fixture_with_plugins(
        app,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        ConfigurationMode::ReadWrite,
        events,
        Some(registry),
    );
    draw(visual);
    let selector: gpui_kit::ElementId = "host-navigation-sftp-workbench".into();
    let present = visual
        .update(|window, _| gpui_kit::base::test_support::find(window, &[], &selector).is_some());
    assert!(!present, "SFTP 禁用时不应渲染工作台入口");
}
