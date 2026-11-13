//! 左栏布局与右栏数据生命周期隔离回归；文件和会话端口均在内存中运行。
//!
//! @author kongweiguang

use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

use gpui_kit::{px, size};
use kerminal_application::plugins::{PluginId, PluginRegistryBuilder};
use kerminal_application::preferences::{ThemeMode, ToolRailPanelPlacement, ToolRailToolId};
use kerminal_domain::files::{DirectoryListing, FileEntryKind};

// 共享命令服务 fixture 在 crate 根只声明一次，这里用 use 引入，避免重复载入。
use crate::root_commands_service;

/// 宿主命令 fixture 的返回部件较多，用别名保持函数签名可读。
type CommandsFixture<'a> = (
    Runtime,
    Entity<RootView>,
    Arc<CapturingSessions>,
    Arc<TemporaryTargets>,
    &'a mut VisualTestContext,
    Arc<root_commands_service::CatalogCalls>,
);

/// 使用内存命令服务创建真实 CommandsView，验证时不读取用户目录。
///
/// @author kongweiguang
fn fixture_with_commands(app: &mut TestAppContext) -> CommandsFixture<'_> {
    let calls = Rc::new(RefCell::new(None));
    let calls_slot = calls.clone();
    let fixture = fixture_with_configuration_and_services(
        app,
        Arc::new(SidebarConfiguration::default()),
        move |services| {
            let (commands, catalog_calls) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            *calls_slot.borrow_mut() = Some(catalog_calls);
            services
                .with_plugins(Arc::new(
                    kerminal_application::plugins::PluginRegistry::builtin().unwrap(),
                ))
                .with_commands(Arc::new(commands))
        },
    );
    let calls = calls
        .borrow_mut()
        .take()
        .expect("commands fixture attaches catalog counters");
    let (runtime, root, sessions, temporary, visual) = fixture;
    (runtime, root, sessions, temporary, visual, calls)
}

/// 以内置目录的显式启停状态创建真实 Root；仍注入命令服务，
/// 使片段入口若漏过门禁会真正切换到 CenterPanel，而不是因服务缺失被掩盖。
///
/// @author kongweiguang
fn fixture_with_disabled_builtin_units<'a>(
    app: &'a mut TestAppContext,
    disabled_units: &[&str],
) -> (Runtime, Entity<RootView>, &'a mut VisualTestContext) {
    let states = disabled_units
        .iter()
        .map(|unit| (PluginId::new(*unit).expect("valid builtin unit"), false))
        .collect::<BTreeMap<_, _>>();
    let fixture = fixture_with_configuration_and_services(
        app,
        Arc::new(SidebarConfiguration::default()),
        move |services| {
            let mut builder = PluginRegistryBuilder::new();
            builder.register_builtins().expect("valid builtins");
            let registry = builder
                .freeze_with_builtin_states(&states)
                .expect("valid builtin states");
            let (commands, _) =
                root_commands_service::empty_commands_service_with_calls(services.sessions.clone());
            services
                .with_plugins(Arc::new(registry))
                .with_commands(Arc::new(commands))
        },
    );
    let (runtime, root, _, _, visual) = fixture;
    (runtime, root, visual)
}

/// 片段目录与终端会话无关；切换终端只重读会话相关数据，不得重启全局目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn switching_terminal_does_not_reload_snippet_catalog(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual, calls) = fixture_with_commands(app);
    // 初始构造已发起片段目录读取；等待真实回执后再记录基线，避免把首次加载误算到切换动作。
    for _ in 0..10 {
        if calls.snippet_lists.load(Ordering::Acquire) > 0 {
            break;
        }
        settle(&runtime, visual);
    }
    let before = calls.snippet_lists.load(Ordering::Acquire);
    assert!(
        before > 0,
        "fixture must load the snippet catalog before switching terminals"
    );

    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let commands = root.commands_view.clone().expect("commands view");
            commands.update(cx, |view, cx| view.set_session(Some(SessionId::new()), cx));
        });
    });
    settle(&runtime, visual);
    assert_eq!(
        calls.snippet_lists.load(Ordering::Acquire),
        before,
        "switching terminal must not reread the session-independent snippet catalog"
    );
}

/// 全局工作流目录只在首次需要时加载；后续切换终端不得重读，避免 Workflows 分区无内容变化地提示同步。
///
/// @author kongweiguang
#[gpui_kit::test]
fn switching_terminal_does_not_reload_global_catalogs(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual, calls) = fixture_with_commands(app);
    // 片段目录在面板构造时加载；工作流目录此时尚未读取，首次切换终端才会补齐。
    for _ in 0..10 {
        if calls.snippet_lists.load(Ordering::Acquire) > 0 {
            break;
        }
        settle(&runtime, visual);
    }
    let snippets_before = calls.snippet_lists.load(Ordering::Acquire);
    assert!(
        snippets_before > 0,
        "fixture must load the snippet catalog before switching terminals"
    );
    assert_eq!(
        calls.workflow_lists.load(Ordering::Acquire),
        0,
        "workflow catalog must not load before the panel has a session"
    );

    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let commands = root.commands_view.clone().expect("commands view");
            commands.update(cx, |view, cx| view.set_session(Some(SessionId::new()), cx));
        });
    });
    settle(&runtime, visual);
    assert_eq!(
        calls.workflow_lists.load(Ordering::Acquire),
        1,
        "first session change must load the global workflow catalog once"
    );

    visual.update(|_, app| {
        root.update(app, |root, cx| {
            let commands = root.commands_view.clone().expect("commands view");
            commands.update(cx, |view, cx| view.set_session(Some(SessionId::new()), cx));
        });
    });
    settle(&runtime, visual);
    assert_eq!(
        calls.workflow_lists.load(Ordering::Acquire),
        1,
        "switching terminals must not reread the global workflow catalog"
    );
    assert_eq!(calls.snippet_lists.load(Ordering::Acquire), snippets_before);
}

/// 页面动作等待测试平台提交窗口激活回调，再从已绘制的主工作区焦点发起。
/// 与真实鼠标进入工作区一致，保留生产模态门禁，不依赖验收构建的活动态覆盖。
fn activate_commands_surface(root: &Entity<RootView>, visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        window.activate_window();
        window.focus(&root.read(app).focus_handle.clone(), app);
    });
    visual.run_until_parked();
    visual.update(|window, app| {
        window.draw(app).clear(app);
        root.update(app, |root, cx| {
            assert!(
                !root.tool_panel_overlay_focus_is_blocked(window, cx),
                "command fixture must own input: active={}, preferences={}, connections={}, confirmation={}, prompt={}",
                window.is_window_active(),
                root.preferences_modal_open,
                root.connections_modal_open,
                root.root_confirmation_owner.is_some(),
                root.connection_prompt_modal_open,
            );
        });
    });
}

/// 通过实际可访问控件验证正文挂载；普通 ElementId 不属于 GPUI 的 debug_selector 查询域。
/// 检查正文内的可见控件，避免只有中央路由状态正确、实际内容却为空的伪通过。
fn assert_snippets_content_is_visible(visual: &mut VisualTestContext) {
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let host: gpui_kit::ElementId = "command-snippets-view".into();
        assert!(
            gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .any(|node| node.visible() && node.path().contains(&host)),
            "snippet content must have a visible control in its actual host",
        );
    });
}

/// 测试平台没有系统帧循环，显式交替派发下一帧和绘制，覆盖宿主换挂与有界焦点交接。
/// 仅等待任务不能执行 on_next_frame；不放宽焦点断言，也不让测试替生产路径设置焦点。
fn settle_snippets_host_frames(runtime: &Runtime, visual: &mut VisualTestContext) {
    for _ in 0..6 {
        visual.update(|window, app| {
            window.simulate_next_frame(app);
            window.draw(app).clear(app);
        });
        settle(runtime, visual);
    }
}

/// 普通页面动作定位已有实体；工具显式迁移及窗口尺寸变化都不重复挂载。
///
/// @author kongweiguang
#[gpui_kit::test]
fn snippets_navigation_keeps_one_entity_across_hosts_and_resize(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual, _) = fixture_with_commands(app);
    settle(&runtime, visual);
    visual.simulate_resize(size(px(1500.), px(900.)));
    activate_commands_surface(&root, visual);
    let entity_id = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_plugin_page("commands", window, cx).unwrap();
            let commands = root.commands_view.clone().expect("commands view");
            commands.update(cx, |view, cx| view.select_workflows(cx));
            root.open_plugin_page("commands", window, cx).unwrap();
            assert_eq!(root.center_panel, CenterPanel::Commands);
            commands.entity_id()
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| window.draw(app).clear(app));
    assert_snippets_content_is_visible(visual);

    for collapsed in [true, false] {
        visual.update(|_, app| {
            root.update(app, |root, cx| {
                root.set_host_sidebar_collapsed(collapsed, cx);
                assert_eq!(root.center_panel, CenterPanel::Commands);
                assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
            });
        });
        settle(&runtime, visual);
        visual.update(|window, app| window.draw(app).clear(app));
        assert_snippets_content_is_visible(visual);
    }

    for width in [800., 1500.] {
        visual.simulate_resize(size(px(width), px(900.)));
        visual.update(|window, app| window.draw(app).clear(app));
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                assert_eq!(root.center_panel, CenterPanel::Commands);
                assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
                assert!(!root.snippets_mounted_in_tool_rail(window, cx));
            });
        });
    }

    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.toggle_tool_rail_panel(ToolRailToolId::Snippets, window, cx);
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| window.draw(app).clear(app));
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.snippets_mounted_in_tool_rail(window, cx));
            root.open_plugin_page("commands", window, cx).unwrap();
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
            root.right_dock_collapsed = true;
            root.open_plugin_page("commands", window, cx).unwrap();
            assert!(!root.right_dock_collapsed);
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
        });
    });
    visual.update(|window, app| window.draw(app).clear(app));
    assert_snippets_content_is_visible(visual);

    for width in [800., 1500.] {
        visual.simulate_resize(size(px(width), px(900.)));
        visual.update(|window, app| window.draw(app).clear(app));
        settle(&runtime, visual);
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                assert_eq!(root.center_panel, CenterPanel::Terminal);
                assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
                assert!(root.snippets_mounted_in_tool_rail(window, cx));
            });
        });
    }

    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command: None,
    });
    *sessions.live.lock().unwrap() = Some(LiveSession::new(target.clone()));
    let pane_id = open_live_terminal(&root, visual, target);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_plugin_page("commands", window, cx).unwrap();
            let tab_entity = root.commands_view.as_ref().unwrap().entity_id();
            assert_ne!(tab_entity, entity_id);
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .focused_pane_id
                    .as_ref(),
                Some(&pane_id),
            );
            root.open_plugin_page("commands", window, cx).unwrap();
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), tab_entity);
        });
    });
}

/// 使用真实标题栏点击往返两个宿主，保留查询、实体和焦点，并拒绝穿过设置模态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn snippet_host_buttons_preserve_view_and_respect_modal(app: &mut TestAppContext) {
    use crate::features::commands::SnippetHostMoveRequested;

    let (runtime, root, _, _, visual, _) = fixture_with_commands(app);
    settle(&runtime, visual);
    visual.simulate_resize(size(px(1500.), px(900.)));
    activate_commands_surface(&root, visual);
    let (entity_id, search_focus) = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_plugin_page("commands", window, cx).unwrap();
            let commands = root.commands_view.clone().expect("commands view");
            commands.update(cx, |view, cx| {
                view.set_snippet_query("retained query".to_owned(), window, cx);
            });
            let focus = window.focused(cx).expect("snippet search focused");
            (commands.entity_id(), focus)
        })
    });
    settle(&runtime, visual);
    visual.update(|window, app| window.draw(app).clear(app));

    for width in [1500., 800.] {
        visual.simulate_resize(size(px(width), px(900.)));
        visual.update(|window, app| window.draw(app).clear(app));
        settle(&runtime, visual);
        click_root_button(visual, "commands-move-to-rail");
        settle_snippets_host_frames(&runtime, visual);
        visual.update(|window, app| {
            window.draw(app).clear(app);
            root.update(app, |root, cx| {
                assert_ne!(root.center_panel, CenterPanel::Commands);
                assert!(root.snippets_mounted_in_tool_rail(window, cx));
                assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
                assert_eq!(window.focused(cx), Some(search_focus.clone()));
                root.open_plugin_page("commands", window, cx).unwrap();
                assert_ne!(root.center_panel, CenterPanel::Commands);
            });
        });
        settle(&runtime, visual);
        visual.update(|window, app| window.draw(app).clear(app));
        click_root_button(visual, "commands-move-to-center");
        settle_snippets_host_frames(&runtime, visual);
        visual.update(|window, app| {
            window.draw(app).clear(app);
            root.update(app, |root, cx| {
                assert_eq!(root.center_panel, CenterPanel::Commands);
                assert!(!root.snippets_mounted_in_tool_rail(window, cx));
                assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
                assert_eq!(window.focused(cx), Some(search_focus.clone()));
            });
            let query_visible = gpui_kit::base::test_support::snapshots(window)
                .into_iter()
                .any(|node| node.value() == Some("retained query"));
            assert!(query_visible, "search value remains visible after moving");
        });
    }

    visual.update(|window, app| {
        root.update(app, |root, cx| root.open_preferences_modal(window, cx));
        window.draw(app).clear(app);
        root.update(app, |root, cx| {
            let modal_focus = window.focused(cx);
            assert!(modal_focus.is_some());
            assert!(
                root.move_snippets_host(SnippetHostMoveRequested::ToolRail, window, cx)
                    .is_err()
            );
            assert_eq!(root.center_panel, CenterPanel::Commands);
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
            assert_eq!(window.focused(cx), modal_focus);
        });
    });
}

/// 设置重投影恢复底层宿主时保留模态焦点和 CommandsView 身份。
///
/// @author kongweiguang
#[gpui_kit::test]
fn snippets_settings_remap_preserves_modal_focus(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual, _) = fixture_with_commands(app);
    settle(&runtime, visual);
    visual.simulate_resize(size(px(1500.), px(900.)));
    activate_commands_surface(&root, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_plugin_page("commands", window, cx).unwrap();
            root.open_preferences_modal(window, cx);
        });
        window.draw(app).clear(app);
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let entity_id = root.commands_view.as_ref().unwrap().entity_id();
            let focused = window.focused(cx);
            assert!(focused.is_some());
            let mut settings = root.settings_snapshot.clone().expect("settings loaded");
            settings
                .tool_rail
                .panel_placements
                .insert(ToolRailToolId::Snippets, ToolRailPanelPlacement::Left);
            root.tool_rail_state
                .toggle(&settings.tool_rail, ToolRailToolId::Snippets);
            root.apply_settings_snapshot(settings, window, cx);
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert!(root.preferences_modal_open);
            assert_eq!(window.focused(cx), focused);
            assert_eq!(root.commands_view.as_ref().unwrap().entity_id(), entity_id);
        });
        window.draw(app).clear(app);
    });
}

/// 布局回执在内存中确认，真实折叠测试不因故意拒写的认证 fixture 打开无关错误模态。
#[derive(Default)]
struct SidebarConfiguration {
    saves: AtomicUsize,
}

/// 从中央迁出片段时，真实活动标签或空终端页继续占据中央，标签与 pane 身份不变。
///
/// @author kongweiguang
#[gpui_kit::test]
fn opening_snippets_rail_restores_the_underlying_workspace(app: &mut TestAppContext) {
    let (runtime, root, sessions, _, visual) = fixture(app);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.center_panel = CenterPanel::Commands;
            root.toggle_tool_rail_panel(ToolRailToolId::Snippets, window, cx);
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            root.toggle_tool_rail_panel(ToolRailToolId::Snippets, window, cx);
        });
    });

    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id: HostId::new(),
        working_directory: None,
        command: None,
    });
    *sessions.live.lock().unwrap() = Some(LiveSession::new(target.clone()));
    let pane_id = open_live_terminal(&root, visual, target);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let before = root
                .workspace_controller
                .as_ref()
                .unwrap()
                .snapshot()
                .clone();
            root.center_panel = CenterPanel::Commands;
            root.toggle_tool_rail_panel(ToolRailToolId::Snippets, window, cx);
            let after = root.workspace_controller.as_ref().unwrap().snapshot();
            assert_eq!(root.center_panel, CenterPanel::Terminal);
            assert_eq!(after.active_tab_id, before.active_tab_id);
            assert_eq!(after.focused_pane_id.as_ref(), Some(&pane_id));
        });
    });
}

#[async_trait]
impl SettingsRepository for SidebarConfiguration {
    /// 保持与通用 Root fixture 一致的初始设置，不读取用户配置。
    /// @author kongweiguang
    async fn load_settings(&self) -> AppResult<VersionedSettings> {
        EmptyConfiguration.load_settings().await
    }

    /// 确认测试提交的快照，让布局的生产回执订阅完整运行。
    /// @author kongweiguang
    async fn save_settings(
        &self,
        settings: SettingsSnapshot,
        _: Option<ConfigRevision>,
    ) -> AppResult<VersionedSettings> {
        self.saves.fetch_add(1, Ordering::AcqRel);
        Ok(VersionedSettings {
            settings,
            revision: None,
        })
    }
}

#[async_trait]
impl ConfigurationRepository for SidebarConfiguration {
    /// 仅开放内存布局保存，其它配置行为维持原 fixture 的封闭边界。
    /// @author kongweiguang
    fn recovery_state(&self) -> ConfigurationRecoveryState {
        EmptyConfiguration.recovery_state()
    }

    /// 无文件配置读路径，避免测试误读工作区或用户数据。
    /// @author kongweiguang
    async fn read(&self, _: ConfigDocumentPath) -> AppResult<Option<ConfigDocument>> {
        unavailable()
    }

    /// 无文件配置写路径；布局只经过 SettingsRepository。
    /// @author kongweiguang
    async fn write(
        &self,
        _: ConfigDocumentPath,
        _: String,
        _: Option<ConfigRevision>,
    ) -> AppResult<ConfigRevision> {
        unavailable()
    }
}

/// watcher 读回布局仍须恢复几何，但不能重跑终端配色、关键词或命令建议；混合设置保持正常应用。
///
/// @author kongweiguang
#[gpui_kit::test]
fn layout_settings_echo_preserves_terminal_configuration(app: &mut TestAppContext) {
    let (runtime, root, _, _, visual) =
        fixture_with_configuration(app, Arc::new(SidebarConfiguration::default()));
    settle(&runtime, visual);
    for theme_mode in [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System] {
        visual.update(|window, app| {
            root.update(app, |root, cx| {
                let mut settings = root.settings_snapshot.clone().expect("settings loaded");
                assert!(
                    root.workbench_layout_intent.is_none(),
                    "initial layout saved"
                );
                settings.theme_mode = theme_mode;
                root.apply_settings_snapshot(settings.clone(), window, cx);
                let generation = root.palette_update_generation;
                settings.workbench_layout.host_sidebar_collapsed = !root.host_sidebar_collapsed;
                settings.workbench_layout.host_sidebar_width = 260;
                root.apply_settings_snapshot(settings.clone(), window, cx);
                assert_eq!(
                    root.palette_update_generation, generation,
                    "layout readback must not reconfigure terminal sessions"
                );
                assert_eq!(
                    root.host_sidebar_collapsed,
                    settings.workbench_layout.host_sidebar_collapsed
                );
                assert_eq!(root.settings_snapshot.as_ref(), Some(&settings));

                settings.terminal.font_size += 1;
                settings.workbench_layout.host_sidebar_collapsed = !root.host_sidebar_collapsed;
                root.apply_settings_snapshot(settings.clone(), window, cx);
                assert_ne!(
                    root.palette_update_generation, generation,
                    "a real terminal preference must still be applied"
                );
                let current_layout = settings.workbench_layout.clone();
                root.workbench_layout_intent =
                    Some(crate::root::workbench::PendingWorkbenchLayout {
                        request_id: 99,
                        layout: current_layout.clone(),
                    });
                settings.workbench_layout.host_sidebar_collapsed = !root.host_sidebar_collapsed;
                root.apply_settings_snapshot(settings, window, cx);
                assert_eq!(
                    root.host_sidebar_collapsed, current_layout.host_sidebar_collapsed,
                    "an older readback must not overwrite the pending layout intent"
                );
                assert_eq!(
                    root.workbench_layout_intent.as_ref().unwrap().layout,
                    current_layout
                );
                root.accept_workbench_layout_receipt(99, current_layout, cx);
                assert!(root.workbench_layout_intent.is_none());
            });
        });
        settle(&runtime, visual);
    }
}

/// 从真实折叠按钮经历绘制、异步回执和再次展开，目录端口计数捕获短暂解绑导致的隐式重读。
///
/// @author kongweiguang
#[gpui_kit::test]
fn sftp_listing_survives_host_sidebar_toggle(app: &mut TestAppContext) {
    let configuration = Arc::new(SidebarConfiguration::default());
    let (runtime, root, sessions, _, visual) =
        fixture_with_configuration(app, configuration.clone());
    settle(&runtime, visual);
    visual.simulate_resize(size(px(1500.), px(900.)));
    visual.update(|window, _| window.activate_window());
    let host_id = HostId::new();
    let endpoint = FileEndpoint::remote(host_id.clone(), "/current").unwrap();
    let target = SessionTarget::Ssh(SshSessionTarget {
        host_id,
        working_directory: Some("/startup".into()),
        command: None,
    });
    let mut live = LiveSession::new(target.clone());
    live.engine
        .process_output(b"\x1b]7;file://fixture/current\x07");
    *sessions.live.lock().unwrap() = Some(live);
    let pane_id = open_live_terminal(&root, visual, target);
    settle(&runtime, visual);
    let (file_runtime, files, transfers, _, calls) = file_support::services();
    *calls.directory_listing.lock().unwrap() = Some(DirectoryListing {
        directory: endpoint.clone(),
        entries: vec![file_support::remote_entry(
            &endpoint,
            "retained.txt",
            FileEntryKind::File,
        )],
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.services.files = Some(files);
            root.services.transfers = Some(transfers);
            root.services.initial_file_endpoint = Some(FileEndpoint::Local {
                path: "C:/fixture".into(),
            });
            root.toggle_tool_rail_panel(ToolRailToolId::Sftp, window, cx);
        })
    });
    settle(&runtime, visual);
    file_support::draw(visual);
    settle(&runtime, visual);
    let sftp = visual.update(|_, app| {
        root.read(app)
            .sftp_panel
            .clone()
            .expect("SFTP panel created")
    });
    let reads = calls.list_directory.load(Ordering::Acquire);
    assert!(reads > 0, "the fixture must load an actual SFTP listing");
    assert!(visual.debug_bounds("files-row-1-retained.txt").is_some());
    for (index, collapsed) in [true, false, true, false].into_iter().enumerate() {
        if index == 2 {
            visual.update(|_, app| {
                sftp.update(app, |files, cx| {
                    files.set_follow_terminal_directory(true, cx)
                })
            });
            settle(&runtime, visual);
        }
        let previous_columns = sessions
            .live
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .info
            .size
            .columns();
        click_root_button(visual, "workbench-toggle-host-sidebar");
        settle(&runtime, visual);
        file_support::draw(visual);
        settle(&runtime, visual);
        let current_columns = sessions
            .live
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .info
            .size
            .columns();
        assert!(
            if collapsed {
                current_columns > previous_columns
            } else {
                current_columns < previous_columns
            },
            "the actual terminal grid must resize after the sidebar toggle"
        );
        visual.update(|_, app| {
            let root = root.read(app);
            assert_eq!(root.host_sidebar_collapsed, collapsed);
            assert!(
                root.workbench_layout_intent.is_none(),
                "layout receipt is complete"
            );
            assert!(root.workbench_layout_save_error.is_none());
            assert_eq!(root.sftp_panel.as_ref(), Some(&sftp));
            assert_eq!(
                file_support::panel_endpoint(sftp.read(app), FilePaneSide::Right),
                endpoint
            );
            assert_eq!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .focused_pane_id
                    .as_ref(),
                Some(&pane_id)
            );
        });
        assert_eq!(configuration.saves.load(Ordering::Acquire), index + 1);
        assert_eq!(
            calls.list_directory.load(Ordering::Acquire),
            reads,
            "left sidebar layout must not reread the SFTP directory"
        );
        assert!(visual.debug_bounds("files-row-1-retained.txt").is_some());
    }
    drop(file_runtime);
}

/// SSH 关闭后，运维工具族的图标 toggle、快捷键和 compact 直达入口都必须拒绝，
/// 不能只靠 Rail 列表隐藏而留下可被程序化导航打开的空白抽屉。
///
/// @author kongweiguang
#[gpui_kit::test]
fn ssh_disabled_operations_tools_reject_toggle_shortcut_and_compact(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture_with_disabled_builtin_units(app, &["kerminal.ssh"]);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            for tool in [
                ToolRailToolId::Tmux,
                ToolRailToolId::System,
                ToolRailToolId::Containers,
                ToolRailToolId::Ports,
            ] {
                assert!(
                    !root.tool_rail_tool_enabled(tool),
                    "{tool:?} must inherit the SSH unit"
                );
                let settings = root.current_tool_rail_settings();
                let before = root.tool_rail_state.project(&settings).panels;
                let collapsed_before = root.right_dock_collapsed;

                root.toggle_tool_rail_panel(tool, window, cx);
                assert_eq!(
                    root.tool_rail_state.project(&settings).panels,
                    before,
                    "toggle must not open {tool:?} while SSH is disabled"
                );
                assert_eq!(root.right_dock_collapsed, collapsed_before);

                root.activate_tool_rail_panel_from_shortcut(tool, window, cx);
                assert!(
                    !root.compact_tool_drawer_open,
                    "shortcut must not open the compact drawer for {tool:?}"
                );
                assert_eq!(root.tool_rail_state.project(&settings).panels, before);

                root.show_compact_tool_panel(tool, window, cx);
                assert!(
                    !root.compact_tool_drawer_open,
                    "direct compact entry must not activate {tool:?}"
                );
                assert_eq!(root.tool_rail_state.project(&settings).panels, before);
            }

            for tool in [
                ToolRailToolId::Sftp,
                ToolRailToolId::Logs,
                ToolRailToolId::Snippets,
            ] {
                assert!(
                    root.tool_rail_tool_enabled(tool),
                    "{tool:?} does not depend on SSH"
                );
            }
        });
    });
}

/// 关闭 commands 后，终端建议触发的 OpenSnippetPanel 程序化路径必须在入口返回：
/// 原中心页不变，也不会把片段宿主挂到工具栏。
///
/// @author kongweiguang
#[gpui_kit::test]
fn commands_disabled_snippet_entry_keeps_center_panel(app: &mut TestAppContext) {
    let (runtime, root, visual) = fixture_with_disabled_builtin_units(app, &["kerminal.commands"]);
    settle(&runtime, visual);
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(
                root.commands_view.is_some(),
                "fixture keeps the snippet host so only the gate can reject the call"
            );
            let before = root.center_panel;
            root.open_snippets_content(window, cx);
            assert_eq!(
                root.center_panel, before,
                "disabled commands must preserve the original center panel"
            );
            assert_ne!(root.center_panel, CenterPanel::Commands);
            assert!(!root.snippets_mounted_in_tool_rail(window, cx));
        });
    });
}

struct DrawerPluginView {
    input: Entity<gpui_kit::component::input::InputState>,
}

impl gpui_kit::Render for DrawerPluginView {
    /// 使用实际输入控件验证抽屉卸载后的命中和编辑，而不只检查布尔状态。
    fn render(
        &mut self,
        _: &mut Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        use gpui_kit::prelude::*;
        gpui_kit::div().size_full().child(
            gpui_kit::component::input::Input::new(&self.input)
                .id("drawer-plugin-input")
                .w_full(),
        )
    }
}

struct DrawerPluginFactory {
    fail_next: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::plugins::content::ContentFactory for DrawerPluginFactory {
    /// 固定贡献身份使失败和重试始终落在同一内容类型。
    fn content_id(&self) -> &str {
        "fixture.drawer-content"
    }

    /// 测试只涉及持久化标签，不构造其他页面生命周期。
    fn supports(&self, kind: crate::plugins::content::ContentKind) -> bool {
        kind == crate::plugins::content::ContentKind::Tab
    }

    /// 首次失败保留原始状态，重试再用同一状态创建真实输入实体。
    fn create(
        &self,
        _: &crate::plugins::content::ContentScope,
        state: Option<&crate::plugins::content::ContentSavedState>,
        window: &mut Window,
        cx: &mut gpui_kit::App,
    ) -> AppResult<Box<dyn crate::plugins::content::ContentInstance>> {
        if self.fail_next.swap(false, Ordering::SeqCst) {
            return Err(AppError::infrastructure("fixture content unavailable"));
        }
        let text = state
            .and_then(|state| state.value.get("text"))
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| AppError::invalid("fixture text missing"))?
            .to_owned();
        let view = cx.new(|cx| DrawerPluginView {
            input: cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).default_value(text)
            }),
        });
        Ok(Box::new(DrawerPluginContent { view }))
    }
}

struct DrawerPluginContent {
    view: Entity<DrawerPluginView>,
}

impl crate::plugins::content::ContentInstance for DrawerPluginContent {
    /// 保留同一个输入实体，使焦点与草稿不会因重绘重新创建。
    fn view(&self) -> gpui_kit::AnyView {
        self.view.clone().into()
    }

    /// 按实际输入生成快照，避免恢复测试读取预设值冒充用户编辑。
    fn snapshot(
        &self,
        cx: &gpui_kit::App,
    ) -> AppResult<crate::plugins::content::ContentSavedState> {
        Ok(crate::plugins::content::ContentSavedState {
            version: 1,
            value: serde_json::json!({"text": self.view.read(cx).input.read(cx).value().to_string()}),
        })
    }

    /// 模拟具有输入焦点的内容插件，检查关闭抽屉不会抢回该焦点。
    fn focus(&mut self, window: &mut Window, cx: &mut gpui_kit::App) -> AppResult<()> {
        use gpui_kit::Focusable as _;
        let focus = self.view.read(cx).input.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        Ok(())
    }

    /// 内存控件无后台资源，仍遵守宿主的异步关闭契约。
    fn begin_close(
        &mut self,
        _: std::time::Instant,
        _: &mut gpui_kit::App,
    ) -> AppResult<crate::plugins::content::ContentCloseFuture> {
        Ok(Box::pin(async { Ok(()) }))
    }
}

/// 主机内容受理后解除窄窗遮罩；拒绝保持抽屉，恢复失败保留原标签并可实际重试编辑。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_plugin_host_content_releases_drawer_and_preserves_recovery(app: &mut TestAppContext) {
    use kerminal_application::plugins::{
        ContentDescriptor, ContentTypeId, PluginDescriptor, PluginId, PluginRegistryBuilder,
    };

    let provider = PluginId::new("fixture.drawer-provider").unwrap();
    let mut registry = PluginRegistryBuilder::new();
    registry.register_provider(PluginDescriptor::new(provider.clone(), "Fixture", true));
    registry.register_content_type(ContentDescriptor::new(
        ContentTypeId::new("fixture.drawer-content").unwrap(),
        provider,
        "Fixture content",
    ));
    let registry = Arc::new(registry.freeze().unwrap());
    let fail_next = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let factory = Arc::new(DrawerPluginFactory {
        fail_next: fail_next.clone(),
    });
    let (runtime, root, _, _, visual) = fixture_with_configuration_and_services(
        app,
        Arc::new(EmptyConfiguration),
        move |services| {
            services
                .with_plugins(registry)
                .with_content_factories(vec![factory])
        },
    );
    settle(&runtime, visual);
    visual.simulate_resize(size(px(800.), px(600.)));
    visual.update(|window, app| {
        window.activate_window();
        root.update(app, |root, _| {
            root.workspace_controller = Some(WorkspaceController::from_missing().unwrap());
            root.workspace_load_pending = false;
        });
        window.draw(app).clear(app);
    });
    let request = crate::plugins::actions::OpenContentTab {
        content_id: "fixture.drawer-content".into(),
        title: "Fixture".into(),
        state_version: 1,
        state: serde_json::json!({"text": "host-value"}),
    };

    click_root_button(visual, "workbench-toggle-host-sidebar");
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            assert!(root.compact_host_drawer_open);
            assert!(
                root.open_plugin_host_content("other.provider", &request, window, cx)
                    .is_err()
            );
            assert!(root.compact_host_drawer_open);
            assert!(
                root.workspace_controller
                    .as_ref()
                    .unwrap()
                    .snapshot()
                    .terminal_tabs
                    .is_empty()
            );
            let controller = root.workspace_controller.take().unwrap();
            assert!(
                root.open_plugin_host_content("fixture.drawer-provider", &request, window, cx)
                    .is_err()
            );
            assert!(root.compact_host_drawer_open);
            root.workspace_controller = Some(controller);
            root.open_plugin_host_content("fixture.drawer-provider", &request, window, cx)
                .unwrap();
            assert!(!root.compact_host_drawer_open);
        });
    });
    settle(&runtime, visual);
    visual.update(|window, app| {
        window.draw(app).clear(app);
        let root = root.read(app);
        let scope = root.active_content_scope.as_ref().unwrap();
        let view = root
            .content_host
            .view(scope)
            .unwrap()
            .downcast::<DrawerPluginView>()
            .unwrap();
        use gpui_kit::Focusable as _;
        assert!(
            view.read(app)
                .input
                .read(app)
                .focus_handle(app)
                .is_focused(window)
        );
    });
    assert!(
        visual
            .debug_bounds("workbench-compact-host-drawer-overlay")
            .is_none()
    );
    click_root_button(visual, "drawer-plugin-input");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("edited-value");
    visual.run_until_parked();
    root.read_with(visual, |root, app| {
        let scope = root.active_content_scope.as_ref().unwrap();
        assert_eq!(
            root.content_host.snapshot(scope, app).unwrap().value["text"],
            "edited-value"
        );
    });

    fail_next.store(true, Ordering::SeqCst);
    click_root_button(visual, "workbench-toggle-host-sidebar");
    let failed_scope = visual.update(|window, app| {
        root.update(app, |root, cx| {
            root.open_plugin_host_content("fixture.drawer-provider", &request, window, cx)
                .unwrap();
            assert!(!root.compact_host_drawer_open);
            let scope = root.active_content_scope.clone().unwrap();
            assert!(root.content_failures.contains_key(&scope));
            scope
        })
    });
    settle(&runtime, visual);
    click_root_button(visual, "retry-plugin-content");
    settle(&runtime, visual);
    root.read_with(visual, |root, app| {
        assert_eq!(root.active_content_scope.as_ref(), Some(&failed_scope));
        assert!(!root.content_failures.contains_key(&failed_scope));
        assert_eq!(
            root.content_host
                .snapshot(&failed_scope, app)
                .unwrap()
                .value["text"],
            "host-value"
        );
    });
    click_root_button(visual, "drawer-plugin-input");
    visual.simulate_keystrokes("secondary-a backspace");
    visual.simulate_input("recovered-value");
    visual.run_until_parked();
    root.read_with(visual, |root, app| {
        assert_eq!(
            root.content_host
                .snapshot(&failed_scope, app)
                .unwrap()
                .value["text"],
            "recovered-value"
        );
    });
}
