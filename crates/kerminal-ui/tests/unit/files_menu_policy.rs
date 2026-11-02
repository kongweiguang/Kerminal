//! 列表、树与空白区的菜单能力及快照失效回归。
//!
//! @author kongweiguang

use gpui_kit::{MouseButton, TestAppContext, VisualTestContext, px, size};
use kerminal_domain::{files::FileEntryKind, identity::HostId};

use super::*;
use crate::features::files::action_test_support as support;

/// 构造两种行布局的同一操作快照，验证能力不依赖具体展示组件。
fn row_snapshots(
    directory: &FileEndpoint,
    entry: FileEntry,
) -> (FileMenuSnapshot, FileMenuSnapshot) {
    let selected = Arc::new(vec![entry.clone()]);
    let common = FileMenuSnapshot {
        side: 1,
        generation: 3,
        directory: directory.clone(),
        show_hidden: true,
        target: FileMenuTarget::List {
            ordered_names: Arc::new(vec![entry.name.clone()]),
            entry: entry.clone(),
            selected: Arc::clone(&selected),
            row_selected: true,
        },
    };
    let tree = FileMenuSnapshot {
        target: FileMenuTarget::Tree {
            entry,
            selected,
            row_selected: true,
        },
        ..common.clone()
    };
    (common, tree)
}

/// 目录、文件和多选在列表与树中必须提供相同命令，中央区域只追加对侧复制。
#[test]
fn shared_row_policy_matches_across_tree_list_and_workbench() {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    for kind in [
        FileEntryKind::File,
        FileEntryKind::Directory,
        FileEntryKind::Symlink,
    ] {
        let mut entry = support::remote_entry(&directory, "item", kind);
        entry.permissions = Some(0o644);
        let (list, tree) = row_snapshots(&directory, entry);
        let compact = file_menu_items(&list, true, true, false);
        assert_eq!(compact, file_menu_items(&tree, true, true, false));
        let central = file_menu_items(&list, false, true, false);
        assert_eq!(central, file_menu_items(&tree, false, true, false));
        assert_eq!(central[0].action, FileMenuAction::CopyToOther);
        assert_eq!(central[0].enabled, kind != FileEntryKind::Symlink);
        assert_eq!(&central[1..], compact.as_slice());
    }

    let first = support::remote_entry(&directory, "first", FileEntryKind::File);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let selected = Arc::new(vec![first.clone(), second]);
    let (mut list, mut tree) = row_snapshots(&directory, first);
    if let FileMenuTarget::List {
        selected: entries, ..
    } = &mut list.target
    {
        *entries = Arc::clone(&selected);
    }
    if let FileMenuTarget::Tree {
        selected: entries, ..
    } = &mut tree.target
    {
        *entries = selected;
    }
    assert_eq!(
        file_menu_items(&list, false, true, false),
        file_menu_items(&tree, false, true, false)
    );
    assert_eq!(
        file_menu_items(&list, false, true, false)
            .iter()
            .map(|item| item.action)
            .collect::<Vec<_>>(),
        [
            FileMenuAction::CopyToOther,
            FileMenuAction::Download,
            FileMenuAction::Separator,
            FileMenuAction::Refresh,
            FileMenuAction::ToggleHidden,
            FileMenuAction::Separator,
            FileMenuAction::Delete
        ]
    );
}

/// 未就绪、提交中和不支持的条目都不显示可执行的传输命令。
#[test]
fn copy_and_download_follow_ready_busy_and_entry_kind() {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let file = support::remote_entry(&directory, "file", FileEntryKind::File);
    let (snapshot, _) = row_snapshots(&directory, file);
    let ready = file_menu_items(&snapshot, false, true, false);
    assert!(
        ready
            .iter()
            .find(|item| item.action == FileMenuAction::CopyToOther)
            .unwrap()
            .enabled
    );
    let unready = file_menu_items(&snapshot, false, false, false);
    assert!(
        !unready
            .iter()
            .find(|item| item.action == FileMenuAction::CopyToOther)
            .unwrap()
            .enabled
    );
    let busy = file_menu_items(&snapshot, false, true, true);
    assert!(
        !busy
            .iter()
            .find(|item| item.action == FileMenuAction::CopyToOther)
            .unwrap()
            .enabled
    );
    assert!(
        !busy
            .iter()
            .find(|item| item.action == FileMenuAction::Download)
            .unwrap()
            .enabled
    );

    for kind in [FileEntryKind::Symlink, FileEntryKind::Other] {
        let entry = support::remote_entry(&directory, "unsupported", kind);
        let (snapshot, _) = row_snapshots(&directory, entry);
        let items = file_menu_items(&snapshot, false, true, false);
        assert!(
            !items
                .iter()
                .find(|item| item.action == FileMenuAction::CopyToOther)
                .unwrap()
                .enabled
        );
        assert!(
            !items
                .iter()
                .find(|item| item.action == FileMenuAction::Download)
                .unwrap()
                .enabled
        );
    }
}

/// 禁用原因必须对应真实门禁，避免提示选择主机却实际被忙状态或不支持的类型阻挡。
#[gpui_kit::test]
fn copy_disabled_reason_matches_panel_state(app: &mut TestAppContext) {
    let source = support::remote_endpoint(&HostId::new(), "/source");
    let destination = support::remote_endpoint(&HostId::new(), "/destination");
    let file = support::remote_entry(&source, "file", FileEntryKind::File);
    let link = support::remote_entry(&source, "link", FileEntryKind::Symlink);
    let (snapshot, _) = row_snapshots(&source, file.clone());
    let (unsupported, _) = row_snapshots(&source, link);
    let fixture = support::entity_fixture(app, source, move |pane| {
        pane.host_picker.enabled = true;
        pane.host_picker.target_unselected = [false, false];
        pane.host_picker.target_pending = [false, false];
        pane.panels[0].endpoint = destination;
        support::seed_listing(pane, 0, 2, Vec::new());
        support::seed_listing(pane, 1, 3, vec![file]);
    });
    let _runtime = fixture.runtime;
    app.update(|cx| {
        fixture.pane.update(cx, |pane, _| {
            assert_eq!(copy_unavailable_reason(pane, &snapshot), None);
            assert_eq!(
                copy_unavailable_reason(pane, &unsupported),
                Some(CopyUnavailableReason::UnsupportedItems)
            );

            pane.transfer_submission_busy = true;
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::Busy)
            );
            pane.transfer_submission_busy = false;
            pane.host_picker.target_unselected[0] = true;
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::DestinationUnselected)
            );
            pane.host_picker.target_unselected = [false, true];
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::SourceUnselected)
            );
            pane.host_picker.target_unselected = [false, false];
            pane.panels[0].loading = true;
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::DirectoryNotReady)
            );
        });
    });
}

/// 同目录菜单应解释不可复制的原因；含安全项的混合选择仍可提交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn copy_menu_rejects_same_target_but_allows_a_safe_mixed_selection(app: &mut TestAppContext) {
    let host = HostId::new();
    let source = support::remote_endpoint(&host, "/source");
    let target_inside_folder = support::remote_endpoint(&host, "/source/folder/child");
    let folder = support::remote_entry(&source, "folder", FileEntryKind::Directory);
    let file = support::remote_entry(&source, "file", FileEntryKind::File);
    let (mut snapshot, _) = row_snapshots(&source, folder.clone());
    let source_for_fixture = source.clone();
    let file_for_fixture = file.clone();
    let folder_for_fixture = folder.clone();
    let fixture = support::entity_fixture(app, source, move |pane| {
        pane.host_picker.enabled = true;
        pane.host_picker.target_unselected = [false, false];
        pane.host_picker.target_pending = [false, false];
        pane.panels[0].endpoint = source_for_fixture;
        support::seed_listing(pane, 0, 2, Vec::new());
        support::seed_listing(pane, 1, 3, vec![folder_for_fixture, file_for_fixture]);
    });
    let _runtime = fixture.runtime;
    app.update(|cx| {
        fixture.pane.update(cx, |pane, _| {
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::UnsafeTarget(
                    DropRejection::SameParent
                ))
            );
            pane.panels[0].endpoint = target_inside_folder;
            support::seed_listing(pane, 0, 4, Vec::new());
            assert_eq!(
                copy_unavailable_reason(pane, &snapshot),
                Some(CopyUnavailableReason::UnsafeTarget(
                    DropRejection::TargetInsideSourceDirectory
                ))
            );
            if let FileMenuTarget::List { selected, .. } = &mut snapshot.target {
                *selected = Arc::new(vec![folder, file]);
            }
            assert_eq!(copy_unavailable_reason(pane, &snapshot), None);
        });
    });
}

/// 空白菜单在紧凑栏与工作台共用当前目录动作，避免附加不可见的工作区或剪贴板入口。
#[test]
fn background_menu_has_same_scope_in_both_surfaces() {
    let snapshot = FileMenuSnapshot {
        side: 1,
        generation: 3,
        directory: support::remote_endpoint(&HostId::new(), "/srv"),
        show_hidden: true,
        target: FileMenuTarget::Background,
    };
    let compact = file_menu_items(&snapshot, true, true, false);
    assert_eq!(compact, file_menu_items(&snapshot, false, true, false));
    assert_eq!(
        compact.iter().map(|item| item.action).collect::<Vec<_>>(),
        [
            FileMenuAction::UploadFiles,
            FileMenuAction::UploadDirectories,
            FileMenuAction::NewDirectory,
            FileMenuAction::Separator,
            FileMenuAction::Refresh,
            FileMenuAction::ToggleHidden,
            FileMenuAction::CopyPath,
        ]
    );
}

/// 单行右键允许从空选区建立目标，但菜单打开后选区变化不得操作另一个项目。
#[gpui_kit::test]
fn single_row_action_rejects_changed_selection(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let first = support::remote_entry(&directory, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let seed_first = first.clone();
    let seed_second = second.clone();
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.view_mode = FilesViewMode::List;
        pane.show_hidden_files = true;
        support::seed_listing(pane, 1, 3, vec![seed_first, seed_second]);
        pane.panels[1].selection.clear();
    });
    let _runtime = fixture.runtime;
    let pane = fixture.pane;
    let snapshot = FileMenuSnapshot {
        side: 1,
        generation: 3,
        directory,
        show_hidden: true,
        target: FileMenuTarget::List {
            entry: first.clone(),
            selected: Arc::new(Vec::new()),
            row_selected: false,
            ordered_names: Arc::new(vec![first.name.clone(), second.name.clone()]),
        },
    };
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.panels[1].selection.select_only(&second.name);
            assert!(!select_menu_snapshot(pane, &snapshot, cx));
            assert_eq!(pane.selected_entries(1)[0].endpoint, second.endpoint);

            pane.panels[1].selection.clear();
            assert!(select_menu_snapshot(pane, &snapshot, cx));
            assert_eq!(pane.selected_entries(1)[0].endpoint, first.endpoint);
        })
    });
}

/// 对侧目录在菜单打开后变化时，不得把旧菜单的复制动作投向新目录。
#[gpui_kit::test]
fn copy_action_rejects_a_changed_destination(app: &mut TestAppContext) {
    let source_directory = support::remote_endpoint(&HostId::new(), "/source");
    let destination_directory = support::remote_endpoint(&HostId::new(), "/destination");
    let entry = support::remote_entry(&source_directory, "release", FileEntryKind::Directory);
    let seed_entry = entry.clone();
    let (_runtime, pane, _, visual) =
        support::window_fixture(app, source_directory.clone(), move |pane| {
            pane.view_mode = FilesViewMode::List;
            pane.show_hidden_files = true;
            pane.panels[0].endpoint = destination_directory;
            support::seed_listing(pane, 0, 4, Vec::new());
            support::seed_listing(pane, 1, 3, vec![seed_entry.clone()]);
            pane.panels[1].selection.select_only(&seed_entry.name);
        });
    let snapshot = FileMenuSnapshot {
        side: 1,
        generation: 3,
        directory: source_directory,
        show_hidden: true,
        target: FileMenuTarget::List {
            entry: entry.clone(),
            selected: Arc::new(vec![entry.clone()]),
            row_selected: true,
            ordered_names: Arc::new(vec![entry.name]),
        },
    };
    visual.update(|window, app| {
        pane.update(app, |pane, cx| {
            let target = pane
                .capture_transfer_target_snapshot(0)
                .expect("ready destination");
            pane.panels[0].generation += 1;
            dispatch_file_menu_action(
                pane,
                &snapshot,
                Some(&target),
                FileMenuAction::CopyToOther,
                window,
                cx,
            );
            assert!(!pane.transfer_submission_busy);
            assert!(pane.status.as_deref().is_some_and(|status| {
                status.contains("目标目录") || status.contains("destination")
            }));
        });
    });
}

/// 已打开的旧菜单重复提交时保留正在处理的批次、用户选择与既有反馈。
///
/// @author kongweiguang
#[gpui_kit::test]
fn busy_copy_action_preserves_feedback_and_selection(app: &mut TestAppContext) {
    let source = support::remote_endpoint(&HostId::new(), "/source");
    let destination = support::remote_endpoint(&HostId::new(), "/destination");
    let first = support::remote_entry(&source, "first", FileEntryKind::File);
    let second = support::remote_entry(&source, "second", FileEntryKind::File);
    let seeded_first = first.clone();
    let seeded_second = second.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, source.clone(), move |pane| {
        pane.view_mode = FilesViewMode::List;
        pane.panels[0].endpoint = destination;
        support::seed_listing(pane, 0, 4, Vec::new());
        support::seed_listing(pane, 1, 3, vec![seeded_first, seeded_second]);
        pane.panels[1].selection.select_only("first");
    });
    let (snapshot, _) = row_snapshots(&source, first);
    let transfers = visual.update(|window, app| {
        pane.update(app, |pane, cx| {
            let target = pane
                .capture_transfer_target_snapshot(0)
                .expect("ready destination");
            let (generation, _cancellation) = pane
                .begin_transfer_preparation([true, true], cx)
                .expect("initial batch started");
            pane.status = Some("existing feedback".to_owned());
            pane.panels[1].selection.select_only("second");
            pane.panels[0].generation += 1;
            dispatch_file_menu_action(
                pane,
                &snapshot,
                Some(&target),
                FileMenuAction::CopyToOther,
                window,
                cx,
            );
            assert_eq!(pane.status.as_deref(), Some("existing feedback"));
            assert_eq!(pane.transfer_submission_generation, generation);
            assert!(pane.transfer_submission_busy);
            assert_eq!(pane.selected_entries(1)[0].endpoint, second.endpoint);
            pane.transfers.clone()
        })
    });
    assert!(runtime.block_on(transfers.list()).is_empty());
}

/// 来源目录换代后不执行旧菜单，并明确提示用户重新打开菜单。
#[gpui_kit::test]
fn stale_source_action_explains_how_to_retry(app: &mut TestAppContext) {
    let source = support::remote_endpoint(&HostId::new(), "/source");
    let entry = support::remote_entry(&source, "file", FileEntryKind::File);
    let seed_entry = entry.clone();
    let (_runtime, pane, _, visual) = support::window_fixture(app, source.clone(), move |pane| {
        pane.view_mode = FilesViewMode::List;
        pane.show_hidden_files = true;
        support::seed_listing(pane, 1, 3, vec![seed_entry.clone()]);
        pane.panels[1].selection.select_only(&seed_entry.name);
    });
    let (snapshot, _) = row_snapshots(&source, entry);
    visual.update(|window, app| {
        pane.update(app, |pane, cx| {
            pane.panels[1].generation += 1;
            dispatch_file_menu_action(
                pane,
                &snapshot,
                None,
                FileMenuAction::CopyToOther,
                window,
                cx,
            );
            assert!(!pane.transfer_submission_busy);
            assert!(pane.status.as_deref().is_some_and(|status| {
                status.contains("重新打开菜单") || status.contains("Reopen the menu")
            }));
        });
    });
}

/// 通过实际鼠标事件打开菜单，避免只测试内部选择辅助函数而漏掉绑定错误。
fn right_click_row(visual: &mut VisualTestContext, selector: &'static str) {
    let position = visual
        .debug_bounds(selector)
        .expect("file row is visible")
        .center();
    visual.simulate_mouse_move(position, None, Default::default());
    visual.simulate_mouse_down(position, MouseButton::Right, Default::default());
    visual.simulate_mouse_up(position, MouseButton::Right, Default::default());
    support::draw(visual);
}

/// 同一鼠标操作契约适用于列表和树：已选行保留多选，未选行立即变为单选。
fn verify_pointer_context_selection(app: &mut TestAppContext, tree: bool) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let first = support::remote_entry(&directory, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let third = support::remote_entry(&directory, "third", FileEntryKind::Directory);
    let root = directory.clone();
    let first_for_seed = first.clone();
    let second_for_seed = second.clone();
    let third_for_seed = third.clone();
    let (_runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.show_hidden_files = true;
        let entries = vec![
            first_for_seed.clone(),
            second_for_seed.clone(),
            third_for_seed,
        ];
        support::seed_listing(pane, 1, 3, entries.clone());
        if tree {
            pane.view_mode = FilesViewMode::Tree;
            pane.panels[1].tree.begin_root_request(&root, 3);
            pane.panels[1].tree.set_root_listing(root, 3, entries, true);
            let visible = pane.panels[1].tree.visible_endpoints(true);
            pane.panels[1].tree.apply_selection(
                &first_for_seed.endpoint,
                &visible,
                SelectionGesture::Replace,
            );
            pane.panels[1].tree.apply_selection(
                &second_for_seed.endpoint,
                &visible,
                SelectionGesture::Toggle,
            );
        } else {
            pane.view_mode = FilesViewMode::List;
            let names = vec!["first".to_owned(), "second".to_owned(), "third".to_owned()];
            pane.panels[1].selection.select_only("first");
            pane.panels[1]
                .selection
                .apply("second", &names, SelectionGesture::Toggle);
            pane.panels[1].selected = Some(first_for_seed);
        }
    });
    visual.simulate_resize(size(px(1180.), px(700.)));
    support::draw(visual);
    // GPUI 的测试查询接收静态 selector，夹具用固定行名避免泄漏动态字符串。
    let selector = |first| match (tree, first) {
        (true, true) => "files-tree-entry-1-/srv/first",
        (true, false) => "files-tree-entry-1-/srv/third",
        (false, true) => "central-files-row-1-first",
        (false, false) => "central-files-row-1-third",
    };
    right_click_row(visual, selector(true));
    visual.update(|_, app| {
        assert_eq!(
            pane.read(app)
                .selected_entries(1)
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["first", "second"]
        );
    });
    visual.simulate_keystrokes("escape");
    support::draw(visual);
    right_click_row(visual, selector(false));
    visual.update(|_, app| {
        assert_eq!(
            pane.read(app)
                .selected_entries(1)
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["third"]
        );
    });
}

/// 列表右键必须在菜单出现时更新选择，而不是等到执行动作才更新。
#[gpui_kit::test]
fn list_pointer_context_selection_is_immediate(app: &mut TestAppContext) {
    verify_pointer_context_selection(app, false);
}

/// 树右键与列表保持一致，子路径使用完整 endpoint 选择身份。
#[gpui_kit::test]
fn tree_pointer_context_selection_is_immediate(app: &mut TestAppContext) {
    verify_pointer_context_selection(app, true);
}
