//! 生产主机导航的真实右键与菜单动作回归，仓储仅保留内存快照。
//!
//! @author kongweiguang

use std::{cell::RefCell, rc::Rc, sync::Arc};

use async_trait::async_trait;
use gpui_kit::{
    AppContext as _, Entity, MouseButton, Role, Subscription, TestAppContext, VisualTestContext,
};
use kerminal_application::{
    AppResult, ApplicationTaskExecutor,
    connections::management::{
        HostDocumentRevision, HostDraft, HostGroupDeletionReceipt, HostGroupDraft,
        HostGroupSnapshot, HostManagementRepository, HostManagementService,
    },
};
use kerminal_domain::preferences::{ConfigurationMode, ConfigurationRecoveryState};
use tokio::runtime::{Builder, Runtime};

use super::super::HostNavigation;
use super::{ConfigRevision, HostGroup, HostId, HostNavigationEvent, ManagedHost, RemoteProtocol};

/// 目录只返回固定快照；任何写操作都代表菜单测试越过范围。
struct MenuDirectory {
    rows: Vec<ManagedHost>,
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
    let runtime = Builder::new_current_thread().enable_all().build().unwrap();
    let executor = ApplicationTaskExecutor::new(runtime.handle().clone());
    let hosts = HostManagementService::new(Arc::new(MenuDirectory { rows, groups }));
    let slot = Rc::new(RefCell::new(None));
    let mounted = slot.clone();
    let (_, visual) = app.add_window_view(move |window, cx| {
        let navigation = cx.new(|cx| {
            HostNavigation::new(
                hosts,
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
        let row = super::host("fixture", "fixture.invalid", source_group, false, &[]);
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
                group_id: expected_group.map(str::to_owned)
            }],
        );
    }
}

/// 协议菜单按能力裁剪，容器与 SFTP 转 SSH 的点击事件保留被右击的准确主机身份。
#[gpui_kit::test]
fn protocol_menu_actions_route_the_clicked_host(app: &mut TestAppContext) {
    init(app);
    for protocol in [
        RemoteProtocol::Ssh,
        RemoteProtocol::Sftp,
        RemoteProtocol::Serial,
    ] {
        let mut row = super::host("fixture", "fixture.invalid", None, false, &[]);
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
        let container = crate::locale::text("容器", "Containers").to_string();
        let copy_ssh =
            crate::locale::text("复制为 SSH 主机…", "Duplicate as SSH host…").to_string();
        assert_eq!(labels.contains(&container), protocol == RemoteProtocol::Ssh);
        assert_eq!(labels.contains(&copy_ssh), protocol == RemoteProtocol::Sftp);
        if protocol == RemoteProtocol::Serial {
            assert!(!labels.contains(
                &crate::locale::text("复制主机 IP / 主机名", "Copy host IP / hostname").to_string()
            ));
            continue;
        }
        let (label, expected) = if protocol == RemoteProtocol::Ssh {
            (
                container,
                HostNavigationEvent::OpenHostContainersRequested { host_id: id },
            )
        } else {
            (
                copy_ssh,
                HostNavigationEvent::CopyHostAsSshRequested { host_id: id },
            )
        };
        click_menu_item(visual, &label);
        assert!(
            events.borrow().contains(&expected),
            "菜单动作必须携带原行身份"
        );
    }
}

/// 只读恢复隐藏配置写入，但保留可查看的详情，并通过真实点击验证事件仍可送达。
#[gpui_kit::test]
fn readonly_host_menu_retains_only_usable_read_actions(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, false, &[]);
    let id = row.id.clone();
    let selector = format!("host-navigation-host-{}", id.as_str());
    let events = Rc::new(RefCell::new(Vec::new()));
    let (_runtime, _navigation, _subscription, visual) = fixture(
        app,
        vec![row],
        Vec::new(),
        ConfigurationMode::ReadOnlyRecovery,
        events.clone(),
    );
    let labels = open_menu(visual, &selector);
    for (zh, en) in [
        ("添加同组连接…", "Add connection to this group…"),
        ("编辑…", "Edit…"),
        ("复制主机配置…", "Copy host configuration…"),
        ("删除主机…", "Delete host…"),
    ] {
        assert!(!labels.contains(&crate::locale::text(zh, en).to_string()));
    }
    let details = crate::locale::text("查看连接详情", "Connection details");
    click_menu_item(visual, &details);
    assert!(
        events
            .borrow()
            .contains(&HostNavigationEvent::ShowHostDetailsRequested { host_id: id })
    );
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
                group_id: expected_group.map(str::to_owned)
            }],
        );
    }
}

/// 固定容器被嵌套在宿主行下方；每个动作仍须独占菜单并携带同一完整 pin 身份。
/// @author kongweiguang
#[gpui_kit::test]
fn pinned_container_menu_keeps_details_files_and_remove_on_clicked_pin(app: &mut TestAppContext) {
    init(app);
    let row = super::host("fixture", "fixture.invalid", None, false, &[]);
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
