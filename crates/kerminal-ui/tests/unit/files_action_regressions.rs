//! FilesPane 请求回执、目录动作快照和取消生命周期回归。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use gpui_kit::{KeyDownEvent, Keystroke, MouseButton, TestAppContext, VisualTestContext};
use kerminal_application::{
    AppError, AppResult,
    platform::external_launch::{ExternalTargetLease, ExternalTargetLeaseOwner},
    preferences::InterfaceDensity,
};
use kerminal_domain::{
    files::{FileEndpoint, FileEntryKind},
    identity::HostId,
};

use super::{NameAction, action_test_support as support};

struct FileLeaseOwner {
    scopes: AtomicUsize,
    accepting: AtomicBool,
}

impl ExternalTargetLeaseOwner for FileLeaseOwner {
    /// 模拟临时目标关闭门禁，使测试能区分独立 owner 与普通克隆。
    ///
    /// @author kongweiguang
    fn retain_scope(&self, _host_id: &HostId) -> AppResult<()> {
        if !self.accepting.load(Ordering::Acquire) {
            return Err(AppError::conflict("临时连接已关闭"));
        }
        self.scopes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// 每个独立 scope 最后一次释放只减少一次计数，反映真实租约生命周期。
    ///
    /// @author kongweiguang
    fn release_scope(&self, _host_id: &HostId) {
        self.scopes.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 文件打开必须从同一临时目标取得独立租约，来源关闭不影响新标签。
///
/// @author kongweiguang
#[test]
fn file_open_retains_the_matching_external_target() {
    let owner = Arc::new(FileLeaseOwner {
        scopes: AtomicUsize::new(1),
        accepting: AtomicBool::new(true),
    });
    let host = HostId::parse("external:file-fixture").expect("valid external id");
    let source = ExternalTargetLease::new_registered(host.clone(), owner.clone())
        .expect("registered fixture lease");
    let endpoint = FileEndpoint::remote(host, "/tmp/file.txt").expect("valid remote path");
    let retained = super::retain_file_target_lease(&endpoint, Some(&source))
        .expect("matching scope can be retained")
        .expect("external file requires an owner");
    assert_eq!(owner.scopes.load(Ordering::Acquire), 2);
    drop(source);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 1);
    drop(retained);
    assert_eq!(owner.scopes.load(Ordering::Acquire), 0);
}

/// 缺失、错主机或已失效租约不能创建文件目标，保存主机不依赖临时 scope。
///
/// @author kongweiguang
#[test]
fn file_open_rejects_missing_mismatched_and_expired_external_lease() {
    let owner = Arc::new(FileLeaseOwner {
        scopes: AtomicUsize::new(1),
        accepting: AtomicBool::new(true),
    });
    let host = HostId::parse("external:file-fixture").expect("valid external id");
    let source = ExternalTargetLease::new_registered(host.clone(), owner.clone())
        .expect("registered fixture lease");
    let endpoint = FileEndpoint::remote(host, "/tmp/file.txt").expect("valid remote path");
    let other = FileEndpoint::remote(
        HostId::parse("external:other-fixture").expect("valid other id"),
        "/tmp/file.txt",
    )
    .expect("valid remote path");
    assert!(super::retain_file_target_lease(&endpoint, None).is_err());
    assert!(super::retain_file_target_lease(&other, Some(&source)).is_err());
    owner.accepting.store(false, Ordering::Release);
    assert!(super::retain_file_target_lease(&endpoint, Some(&source)).is_err());
    let saved = FileEndpoint::remote(HostId::new(), "/tmp/file.txt").expect("valid remote path");
    assert!(
        super::retain_file_target_lease(&saved, None)
            .expect("saved host needs no lease")
            .is_none()
    );
    assert_eq!(owner.scopes.load(Ordering::Acquire), 1);
}

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

/// 单行导航让路径可编辑且次要控件不换行；传输元数据不越过窄栏边界。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_sftp_path_and_transfer_rate_fit_narrow_panel(app: &mut TestAppContext) {
    use gpui_kit::{px, size};
    use kerminal_domain::{
        files::{ConflictPolicy, TransferKind, TransferRequest, TransferStatus, TransferTask},
        identity::TransferId,
    };

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/a-meaningful-directory");
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: std::path::PathBuf::from("source.txt"),
        },
        destination: support::remote_endpoint(&host, "/srv/a-meaningful-directory/destination.txt"),
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Rename,
        idle_timeout_seconds: None,
    };
    let mut summary = TransferTask::queued(TransferId::new(), request)
        .expect("fixture endpoints are distinct")
        .summary();
    summary.status = TransferStatus::Succeeded;
    summary.bytes_transferred = 65;
    summary.total_bytes = Some(65);
    summary.speed_bytes_per_second = 2048;
    let transfer_selector = format!("rr-files-transfer-{}", summary.id);
    let speed_selector = format!("rr-files-transfer-speed-{}", summary.id);
    let percent_selector = format!("rr-files-transfer-percent-{}", summary.id);
    let (runtime, _, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        pane.transfer_rows = vec![summary];
    });
    visual.simulate_resize(size(px(296.), px(600.)));
    support::draw(visual);

    let path_input = visual
        .debug_bounds("rr-files-path-input-1")
        .expect("remote path input is visible");
    let bookmarks = visual
        .debug_bounds("rr-files-bookmarks-1")
        .expect("bookmark menu remains visible on the navigation row");
    assert!(
        (bookmarks.center().y - path_input.center().y).abs() <= px(1.),
        "navigation controls wrapped: path={path_input:?}, bookmarks={bookmarks:?}"
    );
    let transfer = visual
        .update(|window, _| window.debug_selector_bounds(&transfer_selector))
        .expect("transfer row is visible");
    let speed = visual
        .update(|window, _| window.debug_selector_bounds(&speed_selector))
        .expect("transfer rate is visible");
    let percent = visual
        .update(|window, _| window.debug_selector_bounds(&percent_selector))
        .expect("transfer percentage is visible");
    assert!(
        path_input.size.width >= px(80.),
        "path input is too narrow: {path_input:?}"
    );
    assert!(
        speed.size.width > px(40.),
        "transfer rate has no readable width: {speed:?}"
    );
    assert!(
        speed.right() <= transfer.right() - px(12.),
        "transfer rate exceeds the padded row: rate={speed:?}, row={transfer:?}"
    );
    assert!(
        percent.size.width >= px(24.) && percent.right() <= transfer.right() - px(12.),
        "transfer percentage is clipped: percent={percent:?}, row={transfer:?}"
    );

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

/// 工作台与右栏使用不同 selector；此宽布局夹具点击工作台列表以建立生产焦点路由。
///
/// @author kongweiguang
fn click_list(visual: &mut VisualTestContext) {
    let position = visual
        .debug_bounds("central-files-browser-list-1")
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

/// 路径复制不产生成功状态栏，也不覆盖用户尚待处理的错误；无损复制失败仍必须可见。
///
/// @author kongweiguang
#[gpui_kit::test]
fn copy_path_is_quiet_and_preserves_existing_feedback(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let fixture = support::entity_fixture(app, directory.clone(), |_| {});
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.status = None;
            pane.copy_path_to_clipboard(&directory, cx);
            assert_eq!(
                cx.read_from_clipboard().and_then(|item| item.text()),
                Some("/srv".to_owned())
            );
            assert!(pane.status.is_none());
            pane.status = Some("existing transfer error".to_owned());
            pane.copy_path_to_clipboard(&directory, cx);
            assert_eq!(pane.status.as_deref(), Some("existing transfer error"));
            let invalid = FileEndpoint::Local {
                path: "invalid\npath".into(),
            };
            pane.copy_path_to_clipboard(&invalid, cx);
            assert_ne!(pane.status.as_deref(), Some("existing transfer error"));
        });
    });
    let _runtime = fixture.runtime;
}

/// 窄宽工作区都保留单行动作栏；宽布局复制、计数和更多入口在栏内，窄布局优先保留复制与更多。
///
/// @author kongweiguang
#[gpui_kit::test]
fn wide_file_actions_keep_core_controls_on_one_row(app: &mut TestAppContext) {
    use gpui_kit::{px, size};

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let (runtime, _, _, visual) = support::window_fixture(app, directory, |_| {});
    visual.simulate_resize(size(px(1180.), px(700.)));
    support::draw(visual);

    let actions = visual
        .debug_bounds("rr-files-actions-0")
        .expect("wide action toolbar is visible");
    assert!(
        actions.size.height <= px(41.5),
        "normal split must keep the action toolbar on one row: {actions:?}"
    );
    let view_modes = visual
        .debug_bounds("rr-files-more-menu-0")
        .expect("more actions stay visible");
    assert!(
        view_modes.origin.x >= actions.origin.x
            && view_modes.right() <= actions.right()
            && view_modes.bottom() <= actions.bottom(),
        "more actions escaped the toolbar: actions={actions:?}, more={view_modes:?}"
    );
    for selector in ["files-copy-to-other-0", "files-toolbar-count-0"] {
        let control = visual
            .debug_bounds(selector)
            .unwrap_or_else(|| panic!("missing wide toolbar control {selector}"));
        assert!(
            control.origin.x >= actions.origin.x
                && control.right() <= actions.right()
                && control.bottom() <= actions.bottom(),
            "core control escaped the toolbar: selector={selector}, actions={actions:?}, control={control:?}"
        );
    }
    let copy_bounds = visual
        .debug_bounds("files-copy-to-other-0")
        .expect("copy action remains visible");
    let count_bounds = visual
        .debug_bounds("files-toolbar-count-0")
        .expect("file count remains visible");
    assert!(
        count_bounds.origin.x >= copy_bounds.right(),
        "file count must sit to the right of the actions: copy={copy_bounds:?}, count={count_bounds:?}"
    );

    visual.simulate_resize(size(px(760.), px(700.)));
    support::draw(visual);
    let narrow_actions = visual
        .debug_bounds("rr-files-actions-0")
        .expect("narrow action toolbar is visible");
    let narrow_more = visual
        .debug_bounds("rr-files-more-menu-0")
        .expect("narrow more actions stay visible");
    assert!(
        narrow_actions.size.height <= px(41.5),
        "narrow action toolbar must remain on one row: {narrow_actions:?}"
    );
    assert!(
        narrow_more.origin.x >= narrow_actions.origin.x
            && narrow_more.right() <= narrow_actions.right()
            && narrow_more.bottom() <= narrow_actions.bottom(),
        "more actions escaped the toolbar: actions={narrow_actions:?}, more={narrow_more:?}"
    );
    let copy = visual
        .debug_bounds("files-copy-to-other-0")
        .expect("copy action remains visible");
    assert!(
        copy.origin.x >= narrow_actions.origin.x
            && copy.right() <= narrow_actions.right()
            && copy.bottom() <= narrow_actions.bottom(),
        "copy action escaped the toolbar: actions={narrow_actions:?}, copy={copy:?}"
    );

    let _runtime = runtime;
}

/// 宽布局分栏必须响应真实拖拽，左右两栏尺寸同时变化且保持可见。
///
/// @author kongweiguang
#[gpui_kit::test]
fn dragging_file_workbench_divider_resizes_both_panels(app: &mut TestAppContext) {
    use gpui_kit::{Modifiers, point, px, size};

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let (runtime, _, _, visual) = support::window_fixture(app, directory, |_| {});
    visual.simulate_resize(size(px(1180.), px(800.)));
    support::draw(visual);

    let left = visual
        .debug_bounds("rr-files-browser-0")
        .expect("left browser is visible");
    let right = visual
        .debug_bounds("rr-files-browser-1")
        .expect("right browser is visible");
    let boundary = right.left();
    let y = left.center().y;
    let start = point(boundary - px(2.), y);
    let end = point(boundary + px(80.), y);

    visual.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    support::draw(visual);
    visual.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    support::draw(visual);
    visual.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    support::draw(visual);

    let left_after = visual
        .debug_bounds("rr-files-browser-0")
        .expect("left browser remains visible");
    let right_after = visual
        .debug_bounds("rr-files-browser-1")
        .expect("right browser remains visible");
    assert!(
        left_after.size.width > left.size.width + px(60.),
        "left pane did not grow after dragging: before={left:?}, after={left_after:?}"
    );
    assert!(
        right_after.size.width < right.size.width - px(60.),
        "right pane did not shrink after dragging: before={right:?}, after={right_after:?}"
    );

    let _runtime = runtime;
}

/// 选择文件只更新行状态，不再在底部打开正文预览面板。
///
/// @author kongweiguang
#[gpui_kit::test]
fn selecting_a_file_does_not_open_a_preview_panel(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let entry = support::remote_entry(&directory, "upload.txt", FileEntryKind::File);
    let (runtime, _, _, visual) = support::window_fixture(app, directory, move |pane| {
        support::seed_listing(pane, 1, 2, vec![entry.clone()]);
        pane.active_side = 1;
        pane.panels[1].selected = Some(entry);
    });
    support::draw(visual);

    assert!(
        visual.debug_bounds("rr-files-preview").is_none(),
        "selecting a file must not open a bottom preview panel"
    );

    let _runtime = runtime;
}
