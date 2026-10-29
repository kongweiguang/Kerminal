//! FilesPane 请求回执、目录动作快照和取消生命周期回归。
//!
//! @author kongweiguang

use gpui_kit::{KeyDownEvent, Keystroke, MouseButton, TestAppContext, VisualTestContext};
use kerminal_application::preferences::InterfaceDensity;
use kerminal_domain::{files::FileEntryKind, identity::HostId};

use super::{NameAction, action_test_support as support};

/// 新建文件工作区与 main 一样默认显示隐藏项目，避免首次进入时遗漏配置文件。
///
/// @author kongweiguang
#[gpui_kit::test]
fn files_pane_defaults_to_showing_hidden_entries(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let fixture = support::entity_fixture(app, directory, |_| {});

    app.update(|cx| {
        fixture.pane.read_with(cx, |pane, _| {
            assert!(pane.show_hidden_files);
        });
    });
    let _runtime = fixture.runtime;
}

/// 旧下载回执不得清除或覆盖之后发出的剪贴板请求状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_clipboard_receipt_cannot_complete_the_latest_request(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let fixture = support::entity_fixture(app, directory, |_| {});
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, _| {
            pane.active_clipboard_download_request_id = Some(8);
            pane.status = Some("latest request is pending".to_owned());
            pane.status_details_expanded = true;
        });
    });
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.complete_clipboard_download(7, Err("late failure".to_owned()), cx);
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.active_clipboard_download_request_id, Some(8));
            assert_eq!(pane.status.as_deref(), Some("latest request is pending"));
            assert!(pane.status_details_expanded);
        });
    });

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.complete_clipboard_download(8, Ok(()), cx);
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert_eq!(pane.active_clipboard_download_request_id, None);
            assert_eq!(pane.status, None);
            assert!(!pane.status_details_expanded);
        });
    });
    let _runtime = fixture.runtime;
}

/// 已弹出菜单的代次和目录必须同时匹配当前 FilesPane 面板快照。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_action_snapshot_rejects_old_generation_and_directory(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/current");
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.panels[1].generation = 12;
    });

    app.update(|cx| {
        fixture.pane.read_with(cx, |pane, _| {
            assert!(pane.browser_action_snapshot_is_current(1, 12, &directory));
            assert!(!pane.browser_action_snapshot_is_current(1, 11, &directory));
            assert!(!pane.browser_action_snapshot_is_current(2, 12, &directory));
            assert!(!pane.browser_action_snapshot_is_current(
                1,
                12,
                &support::remote_endpoint(&host, "/srv/previous")
            ));
        });
    });
    let _runtime = fixture.runtime;
}

/// 新目录代次会取代旧 host-key trust 请求；迟到回执不能覆盖新错误或清除新 busy。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_key_trust_receipts_are_bound_to_the_current_directory_request(app: &mut TestAppContext) {
    let host = HostId::new();
    let first_directory = support::remote_endpoint(&host, "/srv/first");
    let fixture = support::entity_fixture(app, first_directory, |pane| {
        pane.compact_layout = true;
        pane.panels[1].generation = 7;
        pane.panels[1].directory_error = Some("old directory error".to_owned());
    });
    let pane = fixture.pane.clone();
    app.update(|cx| {
        pane.update(cx, |pane, cx| pane.request_host_key_trust(cx));
    });
    let first_request = app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            pane.current_host_key_trust_request()
                .expect("trust request was emitted")
        })
    });

    let second_directory = support::remote_endpoint(&host, "/srv/second");
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.panels[1].endpoint = second_directory;
            pane.panels[1].generation = 8;
            pane.panels[1].directory_error = Some("new directory error".to_owned());
            pane.status = Some("current status".to_owned());
            assert!(pane.current_host_key_trust_request().is_none());
            pane.request_host_key_trust(cx);
        });
    });
    let second_request = app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            pane.current_host_key_trust_request()
                .expect("new target has an active trust request")
        })
    });
    assert_ne!(first_request.request_id, second_request.request_id);

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.report_host_key_trust(
                first_request.request_id,
                first_request.host_id.clone(),
                Err("late trust failure".to_owned()),
                cx,
            );
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert_eq!(
                pane.current_host_key_trust_request(),
                Some(second_request.clone())
            );
            assert_eq!(pane.status.as_deref(), Some("current status"));
        });
    });

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.report_host_key_trust(
                second_request.request_id,
                second_request.host_id.clone(),
                Err("current trust failure".to_owned()),
                cx,
            );
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert!(pane.current_host_key_trust_request().is_none());
            assert_eq!(pane.status.as_deref(), Some("current trust failure"));
        });
    });
    let _runtime = fixture.runtime;
}

/// 紧凑右栏创建目录时预填当前目录下的完整 new-folder 路径，便于直接改名或跳转。
///
/// @author kongweiguang
#[gpui_kit::test]
fn create_directory_dialog_defaults_to_current_directory_child(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let (runtime, pane, _, visual) =
        support::window_fixture(app, directory, |pane| pane.compact_layout = true);

    let value = visual.update(|window, cx| {
        pane.update(cx, |pane, cx| pane.prepare_create_directory(window, cx));
        pane.read_with(cx, |pane, cx| {
            pane.name_input
                .as_ref()
                .map(|input| input.read(cx).value().to_string())
        })
    });
    assert_eq!(value.as_deref(), Some("/srv/app/new-folder"));
    let _runtime = runtime;
}

/// 同一密度值必须驱动虚拟行与真实行容器；显式种入当前终端目标以验证真实紧凑栏工具栏几何。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_file_rows_and_toolbar_follow_reference_geometry(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let entry = support::remote_entry(&directory, "density.txt", FileEntryKind::File);
    let entry_for_seed = entry.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.show_hidden_files = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 2, vec![entry_for_seed]);
    });

    let new_folder = visual
        .debug_bounds("files-new-folder-1")
        .expect("folder-plus toolbar button is visible");
    let divider = visual
        .debug_bounds("files-download-divider-1")
        .expect("download group divider is visible for remote hosts");
    let download = visual
        .debug_bounds("files-download-1")
        .expect("download button follows its divider");
    let card = visual
        .debug_bounds("files-browser-entry-card-1")
        .expect("compact list is enclosed in its own card");
    assert_eq!(new_folder.size.width.as_f32(), 32.0);
    assert_eq!(new_folder.size.height.as_f32(), 32.0);
    assert_eq!(divider.size.width.as_f32(), 1.0);
    assert!(download.origin.x > divider.origin.x);
    assert!(card.size.width > download.size.width);

    for (density, expected_height) in [
        (InterfaceDensity::Compact, 36.0),
        (InterfaceDensity::Comfortable, 44.0),
        (InterfaceDensity::Spacious, 48.0),
    ] {
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| pane.set_interface_density(density, cx));
        });
        support::draw(visual);
        let row = visual
            .debug_bounds("files-row-1-density.txt")
            .expect("the directory row remains visible");
        assert_eq!(row.size.height.as_f32(), expected_height);
    }

    let _runtime = runtime;
}

/// 取消新建、重命名、权限和删除对话框只清理待处理状态，不提交文件写操作。
///
/// @author kongweiguang
#[gpui_kit::test]
fn cancelling_global_file_dialogs_never_submits_mutations(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let entry = support::remote_entry(&directory, "report.txt", FileEntryKind::File);
    let fixture = support::entity_fixture(app, directory.clone(), |_| {});
    let pane = fixture.pane.clone();

    for action in [NameAction::CreateDirectory, NameAction::Rename] {
        app.update(|cx| {
            pane.update(cx, |pane, cx| {
                pane.compact_layout = true;
                pane.name_action = Some(action);
                pane.name_action_side = Some(1);
                pane.name_action_target = Some(if action == NameAction::Rename {
                    entry.endpoint.clone()
                } else {
                    directory.clone()
                });
                assert!(pane.pending_global_dialog());
                pane.cancel_global_dialog(cx);
                assert!(!pane.pending_global_dialog());
                assert!(pane.name_action.is_none());
                assert_eq!(pane.name_action_target, None);
            });
        });
    }

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.permission_dialog_target = Some((1, entry.endpoint.clone()));
            assert!(pane.pending_global_dialog());
            pane.cancel_global_dialog(cx);
            assert!(!pane.pending_global_dialog());
            assert_eq!(pane.permission_dialog_target, None);
        });
    });

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.delete_confirmation = Some(vec![entry.clone()]);
            assert!(pane.pending_global_dialog());
            pane.cancel_global_dialog(cx);
            assert!(!pane.pending_global_dialog());
            assert_eq!(pane.delete_confirmation, None);
        });
    });

    assert_eq!(
        fixture
            .mutations
            .create_directory
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        fixture
            .mutations
            .rename
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        fixture
            .mutations
            .chmod
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    assert_eq!(
        fixture
            .mutations
            .delete
            .load(std::sync::atomic::Ordering::Acquire),
        0
    );
    let _runtime = fixture.runtime;
}

/// Delete 和 F2 按键处理后冻结触发时的目标，不读取后来改变的选择。
///
/// @author kongweiguang
#[gpui_kit::test]
fn delete_and_f2_keep_the_entry_selected_when_the_key_was_pressed(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let first = support::remote_entry(&directory, "first", FileEntryKind::Directory);
    let second = support::remote_entry(&directory, "second", FileEntryKind::Directory);
    let second_for_seed = second.clone();
    let first_for_seed = first.clone();
    let (runtime, pane, _mutations, visual) =
        support::window_fixture(app, directory.clone(), move |pane| {
            support::seed_listing(pane, 1, 5, vec![first_for_seed.clone(), second_for_seed]);
            pane.panels[1].selection.select_only(&first_for_seed.name);
            pane.panels[1].selected = Some(first_for_seed);
        });

    click_list(visual);
    send_key(visual, "f2");
    visual.update(|_, cx| {
        pane.read_with(cx, |pane, _| {
            assert!(pane.name_action == Some(NameAction::Rename));
            assert_eq!(pane.name_action_target.as_ref(), Some(&first.endpoint));
        });
        pane.update(cx, |pane, cx| {
            pane.panels[1].selection.select_only("second");
            pane.panels[1].selected = Some(second.clone());
            assert_eq!(pane.name_action_target.as_ref(), Some(&first.endpoint));
            pane.cancel_global_dialog(cx);
            pane.panels[1].selection.select_only("first");
            pane.panels[1].selected = Some(first.clone());
        });
    });

    click_list(visual);
    send_key(visual, "delete");
    visual.update(|_, cx| {
        pane.read_with(cx, |pane, _| {
            let confirmation = pane
                .delete_confirmation
                .as_ref()
                .expect("Delete opens the confirmation for the selected item");
            assert_eq!(confirmation.len(), 1);
            assert_eq!(confirmation[0].endpoint, first.endpoint);
        });
        pane.update(cx, |pane, _| {
            pane.panels[1].selection.select_only("second");
            pane.panels[1].selected = Some(second.clone());
            assert_eq!(
                pane.delete_confirmation
                    .as_ref()
                    .and_then(|entries| entries.first())
                    .map(|entry| &entry.endpoint),
                Some(&first.endpoint)
            );
        });
    });
    let _runtime = runtime;
}

/// 将列表中心的真实鼠标按下/抬起事件分发给 GPUI，以建立生产焦点路由。
///
/// @author kongweiguang
fn click_list(visual: &mut VisualTestContext) {
    let position = visual
        .debug_bounds("files-browser-list-1")
        .expect("FilesPane browser list was rendered")
        .center();
    visual.simulate_mouse_move(position, None, Default::default());
    support::draw(visual);
    visual.simulate_mouse_down(position, MouseButton::Left, Default::default());
    support::draw(visual);
    visual.simulate_mouse_up(position, MouseButton::Left, Default::default());
    support::draw(visual);
}

/// 将指定按键作为原生 GPUI 键盘事件送入当前焦点，覆盖 FilesPane 的实际 handler。
///
/// @author kongweiguang
fn send_key(visual: &mut VisualTestContext, binding: &str) {
    visual.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse(binding).expect("test key binding is valid"),
        is_held: false,
        prefer_character_input: false,
    });
    support::draw(visual);
}
