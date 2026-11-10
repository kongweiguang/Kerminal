//! 文件宿主右栏交接的有效期与目标状态回归；fixture 不访问真实文件或网络。
//!
//! @author kongweiguang

use super::*;

use std::sync::atomic::Ordering;

use gpui_kit::TestAppContext;
use kerminal_application::{
    files::editor::FileEditorService,
    workspace::{WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionPane},
};
use kerminal_domain::{identity::PaneId, workspace::TerminalLayoutNode};

use crate::features::{
    files::{FileBrowserContextSnapshot, FilePaneSide, action_test_support as support},
    workspace_controller::WorkspaceController,
    workspace_tabs::WorkspaceTabEvent,
};

/// 用明确的文档和目录身份构造宿主，避免让测试把全局导航代次误当作用户操作。
///
/// @author kongweiguang
fn install_host(
    runtime: &mut WorkspaceFileRuntime,
    host: &HostId,
    directory: &str,
    document: &str,
) -> (TabId, FileEndpoint) {
    let owner = TabId::new();
    let document_id = TabId::new();
    let directory_endpoint = support::remote_endpoint(host, directory);
    let document_endpoint = support::remote_endpoint(host, document);
    runtime.install_host_descriptor(
        WorkspaceSessionTab::FileWorkspace {
            id: owner.clone(),
            tab_group_id: None,
            title: "fixture".into(),
            machine_id: host.as_str().to_owned(),
            target: WorkspaceTargetRef::Ssh {
                host_id: host.clone(),
            },
            state: FileWorkspaceState {
                items: vec![
                    FileWorkspaceItem::Directory {
                        id: TabId::new(),
                        path: directory.to_owned(),
                    },
                    FileWorkspaceItem::Document {
                        id: document_id.clone(),
                        title: "fixture.txt".into(),
                        path: document.to_owned(),
                        root_path: None,
                        access: WorkspaceFileAccess::Editable,
                        source: WorkspaceFileSource::Sftp,
                    },
                ],
                active_item_id: Some(document_id),
                source_tab_ids: Vec::new(),
            },
        },
        Some(directory_endpoint),
        None,
        false,
    );
    (owner, document_endpoint)
}

/// 快照由真实紧凑面板捕获，保证测试不伪造清单、导航历史或文件系统身份。
///
/// @author kongweiguang
fn snapshot(app: &mut TestAppContext, directory: FileEndpoint) -> FileBrowserContextSnapshot {
    let seeded_directory = directory.clone();
    let source = support::entity_fixture(app, directory, move |pane| {
        support::seed_compact_listing(pane, seeded_directory, Vec::new());
    });
    app.update(|cx| {
        source.pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
        });
        source
            .pane
            .read(cx)
            .capture_file_browser_context(cx)
            .expect("源面板具有已提交的浏览快照")
    })
}

/// 同一活动文档首次绑定时只消费一次纯浏览值，不应重新读取目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn first_handoff_preserves_browser_without_rescan(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let context = snapshot(app, directory.clone());
    let destination = support::entity_fixture(app, directory.clone(), |_| {});
    let mut runtime = WorkspaceFileRuntime::default();
    let (owner, document) = install_host(&mut runtime, &host, "/manual", "/manual/first.txt");
    runtime.replace_right_browser_context(&owner, &document, Some(context));
    let handoff = runtime
        .take_right_browser_context(&owner)
        .expect("原文档和目录仍有效");
    assert!(runtime.take_right_browser_context(&owner).is_none());

    app.update(|cx| {
        destination.pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            assert!(pane.restore_file_browser_context(handoff, cx));
            pane.set_file_workspace_target(owner, directory.clone(), None, cx);
            assert_eq!(
                support::panel_endpoint(pane, FilePaneSide::Right),
                directory
            );
            assert!(pane.capture_file_browser_context(cx).is_some());
        });
    });
    assert_eq!(
        destination.mutations.list_directory.load(Ordering::SeqCst),
        0
    );
}

/// 目标已有自己的浏览位置时，拒绝交接并消费旧快照，而不是覆盖或稍后重试。
///
/// @author kongweiguang
#[gpui_kit::test]
fn occupied_target_keeps_its_own_directory(app: &mut TestAppContext) {
    let host = HostId::new();
    let original = support::remote_endpoint(&host, "/manual");
    let current = support::remote_endpoint(&host, "/user-selected");
    let context = snapshot(app, original.clone());
    let seeded = current.clone();
    let destination = support::entity_fixture(app, current.clone(), move |pane| {
        support::seed_compact_listing(pane, seeded, Vec::new());
    });
    let mut runtime = WorkspaceFileRuntime::default();
    let (owner, document) = install_host(&mut runtime, &host, "/manual", "/manual/first.txt");
    runtime.replace_right_browser_context(&owner, &document, Some(context));
    let handoff = runtime
        .take_right_browser_context(&owner)
        .expect("交接尚未被消费");

    app.update(|cx| {
        destination.pane.update(cx, |pane, cx| {
            pane.set_compact_layout(true, cx);
            assert!(!pane.restore_file_browser_context(handoff, cx));
            pane.set_file_workspace_target(owner.clone(), original, None, cx);
            assert_eq!(support::panel_endpoint(pane, FilePaneSide::Right), current);
            assert!(pane.capture_file_browser_context(cx).is_some());
        });
    });
    assert!(runtime.take_right_browser_context(&owner).is_none());
    assert_eq!(
        destination.mutations.list_directory.load(Ordering::SeqCst),
        0
    );
}

/// 文档或宿主目录改变只丢弃对应 owner 的旧值，不影响另一主机等待交接的状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn changed_host_state_discards_only_its_own_handoff(app: &mut TestAppContext) {
    let host_a = HostId::new();
    let host_b = HostId::new();
    let context_a = snapshot(app, support::remote_endpoint(&host_a, "/manual"));
    let context_b = snapshot(app, support::remote_endpoint(&host_b, "/manual"));
    let mut runtime = WorkspaceFileRuntime::default();
    let (owner_a, document_a) = install_host(&mut runtime, &host_a, "/manual", "/manual/first.txt");
    let (owner_b, document_b) = install_host(&mut runtime, &host_b, "/manual", "/manual/first.txt");
    runtime.replace_right_browser_context(&owner_a, &document_a, Some(context_a.clone()));
    runtime.replace_right_browser_context(&owner_b, &document_b, Some(context_b));

    let WorkspaceSessionTab::FileWorkspace { state, .. } =
        &mut runtime.tabs.get_mut(&owner_a).expect("主机 A").tab
    else {
        panic!("必须是文件宿主");
    };
    state.active_item_id = None;
    assert!(runtime.take_right_browser_context(&owner_a).is_none());
    assert!(runtime.take_right_browser_context(&owner_a).is_none());
    assert!(runtime.take_right_browser_context(&owner_b).is_some());

    let WorkspaceSessionTab::FileWorkspace { state, .. } =
        &mut runtime.tabs.get_mut(&owner_a).expect("主机 A").tab
    else {
        panic!("必须是文件宿主");
    };
    state.active_item_id = Some(state.items[1].id().clone());
    runtime.replace_right_browser_context(&owner_a, &document_a, Some(context_a));
    let WorkspaceSessionTab::FileWorkspace { state, .. } =
        &mut runtime.tabs.get_mut(&owner_a).expect("主机 A").tab
    else {
        panic!("必须是文件宿主");
    };
    let FileWorkspaceItem::Directory { path, .. } = &mut state.items[0] else {
        panic!("首项必须是目录");
    };
    *path = "/new-directory".into();
    assert!(runtime.take_right_browser_context(&owner_a).is_none());
}

/// 真正切回来源标签后区分失败和忙态受理，避免缓存面板事件被拒绝时产生假阳性。
///
/// @author kongweiguang
#[gpui_kit::test]
fn accepted_open_without_snapshot_clears_previous_handoff(app: &mut TestAppContext) {
    let host = HostId::new();
    let source_tab = TabId::new();
    let source_pane = PaneId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let first = support::remote_endpoint(&host, "/manual/first.txt");
    let second = support::remote_endpoint(&host, "/manual/second.txt");
    let seeded = directory.clone();
    let source = support::entity_fixture(app, directory.clone(), move |pane| {
        support::seed_compact_listing(pane, seeded, Vec::new());
    });
    let (_file_runtime, files, _, _, _) = support::services();
    let (_root_runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, move |mut services| {
            services.file_editor = Some(FileEditorService::new(files));
            services
        });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let mut controller = WorkspaceController::from_missing().expect("空工作区");
            let mut snapshot = controller.snapshot().clone();
            snapshot.active_tab_id = Some(source_tab.clone());
            snapshot.focused_pane_id = Some(source_pane.clone());
            snapshot.selected_machine_id = Some(host.as_str().to_owned());
            snapshot.terminal_tabs.push(WorkspaceSessionTab::Terminal {
                id: source_tab.clone(),
                tab_group_id: None,
                title: "source".into(),
                machine_id: host.as_str().to_owned(),
                layout: TerminalLayoutNode::pane(source_pane.clone()),
            });
            snapshot.terminal_panes.push(WorkspaceSessionPane {
                id: source_pane.clone(),
                title: "source".into(),
                machine_id: host.as_str().to_owned(),
                mode: WorkspacePaneMode::Ssh,
                target: Some(WorkspaceTargetRef::Ssh {
                    host_id: host.clone(),
                }),
                remote_host_id: Some(host.clone()),
                container_id: None,
                profile_id: None,
                shell: None,
                args: None,
                cwd: None,
                current_cwd: None,
                env: None,
                remote_command: None,
                tmux_binding: None,
                prompt: String::new(),
                status: WorkspaceMachineStatus::Offline,
                latency_ms: None,
                local_machine_scope: None,
                output_history: None,
            });
            controller
                .synchronize_snapshot(snapshot)
                .expect("来源标签有效");
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            root.reconcile_tab_tool_contents(window, cx);
            root.sftp_panel = Some(source.pane.clone());
            root._sftp_panel_subscriptions = Some(RootView::subscribe_files_pane_events(
                &source.pane,
                window,
                cx,
            ));
        });
        source.pane.update(app, |pane, cx| {
            pane.set_compact_layout(true, cx);
            assert!(pane.capture_file_browser_context(cx).is_some());
            support::request_fixture_open(pane, first.clone(), cx);
        });
    });
    let owner = root.read_with(visual, |root, _| {
        let owner = root
            .workspace_files
            .host_workspace_for_endpoint(&first)
            .expect("首次文件打开已受理");
        assert!(root.is_cached_sftp_panel(&source.pane));
        assert!(
            root.workspace_files
                .right_browser_contexts
                .contains_key(&owner)
        );
        owner
    });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let event = WorkspaceTabEvent::Activate(source_tab.clone());
            assert!(root.handle_workspace_tab_event(&event, window, cx));
            root.activate_workspace_tab(&source_tab, window, cx);
            root.reconcile_tab_tool_contents(window, cx);
            assert!(root.tab_tool_contents_are_current());
            assert!(
                root.sftp_panel
                    .as_ref()
                    .is_some_and(|pane| pane == &source.pane)
            );
            assert!(!root.is_cached_sftp_panel(&source.pane));
        });
    });
    let editor = root.update(visual, |root, _| {
        assert!(
            root.sftp_panel
                .as_ref()
                .is_some_and(|pane| pane == &source.pane)
        );
        assert!(!root.is_cached_sftp_panel(&source.pane));
        root.status = None;
        root.file_editor_workspace.take().expect("文件编辑器")
    });
    visual.update(|_, app| {
        source.pane.update(app, |pane, cx| {
            // 隔离 fixture 没有在线终端；按原目录重建已提交列表，不伪造连接能力。
            support::seed_compact_listing(pane, directory.clone(), Vec::new());
            assert!(pane.capture_file_browser_context(cx).is_some());
            support::request_fixture_open(pane, second.clone(), cx);
        });
    });
    root.read_with(visual, |root, _| {
        assert_eq!(root.status.as_deref(), Some("文件编辑服务尚未配置"));
        assert_eq!(
            root.workspace_files.host_active_document(&owner),
            Some(first.clone())
        );
        assert!(
            root.workspace_files
                .right_browser_contexts
                .contains_key(&owner)
        );
        assert!(!root.is_cached_sftp_panel(&source.pane));
    });
    root.update(visual, |root, _| {
        assert!(
            root.sftp_panel
                .as_ref()
                .is_some_and(|pane| pane == &source.pane)
        );
        assert!(!root.is_cached_sftp_panel(&source.pane));
        root.file_editor_workspace = Some(editor);
    });
    visual.update(|_, app| {
        source.pane.update(app, |pane, cx| {
            support::seed_compact_listing(pane, directory.clone(), Vec::new());
            pane.set_transfer_origin(Some(TransferOrigin::TerminalPane(source_pane.clone())), cx);
            assert!(pane.capture_file_browser_context(cx).is_some());
            support::begin_enqueue_barrier(pane, cx);
            assert!(pane.capture_file_browser_context(cx).is_none());
            support::request_fixture_open(pane, second.clone(), cx);
        });
    });
    root.read_with(visual, |root, _| {
        assert_eq!(
            root.workspace_files.host_active_document(&owner),
            Some(second)
        );
        assert!(
            !root
                .workspace_files
                .right_browser_contexts
                .contains_key(&owner)
        );
    });
    visual.update(|_, app| {
        source
            .pane
            .update(app, |pane, _| support::finish_enqueue_barrier(pane));
    });
}

/// 迟到或不一致的来源事件不能把主机 A 的纯浏览清单交给主机 B。
///
/// @author kongweiguang
#[gpui_kit::test]
fn mismatched_source_cannot_handoff_to_another_host(app: &mut TestAppContext) {
    let host_a = HostId::new();
    let host_b = HostId::new();
    let directory = support::remote_endpoint(&host_a, "/manual");
    let other_document = support::remote_endpoint(&host_b, "/manual/other.txt");
    let seeded = directory.clone();
    let source = support::entity_fixture(app, directory, move |pane| {
        support::seed_compact_listing(pane, seeded, Vec::new());
    });
    let (_file_runtime, files, _, _, _) = support::services();
    let (_root_runtime, root, visual) =
        crate::root::external_auth::tests::host_file_workspace_fixture(app, move |mut services| {
            services.file_editor = Some(FileEditorService::new(files));
            services
        });
    visual.update(|window, app| {
        root.update(app, |root, cx| {
            let controller = WorkspaceController::from_missing().expect("空工作区");
            root.workspace_files = WorkspaceFileRuntime::from_snapshot(controller.snapshot());
            root.workspace_controller = Some(controller);
            root.workspace_load_pending = false;
            root.center_panel = CenterPanel::Terminal;
            root.sftp_panel = Some(source.pane.clone());
            root._sftp_panel_subscriptions = Some(RootView::subscribe_files_pane_events(
                &source.pane,
                window,
                cx,
            ));
        });
        source.pane.update(app, |pane, cx| {
            pane.set_compact_layout(true, cx);
            assert!(pane.capture_file_browser_context(cx).is_some());
            support::request_fixture_open(pane, other_document.clone(), cx);
        });
    });
    root.read_with(visual, |root, _| {
        let owner = root
            .workspace_files
            .host_workspace_for_endpoint(&other_document)
            .expect("主机 B 的文件请求已受理");
        assert!(
            !root
                .workspace_files
                .right_browser_contexts
                .contains_key(&owner)
        );
    });
}
