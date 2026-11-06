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

/// 同一帧的虚拟行、绘制和翻页必须使用放大后的行高，才能保留目录位置和键盘上下文。
///
/// @author kongweiguang
#[gpui_kit::test]
fn scaled_file_rows_keep_virtual_geometry_and_page_navigation_aligned(app: &mut TestAppContext) {
    use gpui_kit::{component::Theme, px, size};

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let entries = (0..40)
        .map(|index| {
            support::remote_entry(&directory, &format!("file-{index:02}"), FileEntryKind::File)
        })
        .collect();
    let seeded_directory = directory.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.queue_watch_started = true;
        support::seed_compact_listing(pane, seeded_directory, entries);
    });
    visual.simulate_resize(size(px(360.), px(600.)));
    support::draw(visual);

    let row = visual
        .debug_bounds("files-row-1-file-00")
        .expect("first row");
    assert_eq!(row.size.height, px(36.));
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(24.));
        window.set_rem_size(px(24.));
        pane.update(cx, |_, cx| cx.notify());
    });
    support::draw(visual);
    let first = visual
        .debug_bounds("files-row-1-file-00")
        .expect("first row");
    let second = visual
        .debug_bounds("files-row-1-file-01")
        .expect("second row");
    assert_eq!(first.size.height, px(54.));
    assert_eq!(second.origin.y - first.origin.y, px(54.));

    visual.update(|window, cx| {
        let focus = pane.read(cx).panels[1]
            .list_focus_handle
            .clone()
            .expect("list focus handle");
        window.focus(&focus, cx);
    });
    send_key(visual, "home");
    let visible_rows = visual.update(|_, cx| {
        (pane.read(cx).panels[1]
            .list_scroll
            .bounds()
            .size
            .height
            .as_f32()
            / 54.)
            .floor() as usize
    });
    send_key(visual, "pagedown");
    let expected_index = visible_rows.saturating_sub(1).clamp(1, 39);
    visual.update(|_, cx| {
        assert_eq!(
            pane.read(cx).panels[1]
                .keyboard_focus
                .active_name
                .as_deref(),
            Some(format!("file-{expected_index:02}").as_str())
        );
    });
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(16.));
        window.set_rem_size(px(16.));
    });
    let _runtime = runtime;
}

/// 文件动作在窄栏和大字号下可以换行，但每个主要入口都必须留在容器内。
///
/// @author kongweiguang
#[gpui_kit::test]
fn scaled_file_actions_remain_reachable_in_a_narrow_panel(app: &mut TestAppContext) {
    use gpui_kit::{component::Theme, px, size};

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/app");
    let seeded_directory = directory.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.queue_watch_started = true;
        support::seed_compact_listing(pane, seeded_directory, Vec::new());
    });
    visual.simulate_resize(size(px(296.), px(600.)));
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(24.));
        window.set_rem_size(px(24.));
        pane.update(cx, |_, cx| cx.notify());
    });
    support::draw(visual);
    let bar = visual
        .debug_bounds("rr-files-actions-1")
        .expect("actions bar");
    for selector in [
        "rr-files-upload-menu",
        "files-new-folder-1",
        "files-download-1",
        "rr-files-view-menu",
    ] {
        let bounds = visual.debug_bounds(selector).expect("file action");
        assert!(
            bounds.origin.x >= bar.origin.x
                && bounds.right() <= bar.right()
                && bounds.origin.y >= bar.origin.y
                && bounds.bottom() <= bar.bottom(),
            "{selector} outside toolbar: action={bounds:?}, toolbar={bar:?}"
        );
    }
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(16.));
        window.set_rem_size(px(16.));
    });
    let _runtime = runtime;
}

/// 树的普通行、空目录和失败重试共享缩放高度，Kit 虚拟树才不会产生行内裁切。
///
/// @author kongweiguang
#[gpui_kit::test]
fn scaled_tree_rows_keep_placeholder_and_retry_heights_aligned(app: &mut TestAppContext) {
    use gpui_kit::{component::Theme, px, size};
    use kerminal_domain::files::DirectoryListing;

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let empty = support::remote_entry(&directory, "a-empty", FileEntryKind::Directory);
    let failed = support::remote_entry(&directory, "b-failed", FileEntryKind::Directory);
    let final_entry = support::remote_entry(&directory, "z-file", FileEntryKind::File);
    let root = directory.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        let entries = vec![empty.clone(), failed.clone(), final_entry];
        pane.view_mode = super::view_mode::FilesViewMode::Tree;
        support::seed_listing(pane, 1, 3, entries.clone());
        let tree = &mut pane.panels[1].tree;
        tree.begin_root_request(&root, 3);
        tree.set_root_listing(root, 3, entries, true);
        tree.expand(&empty.endpoint);
        let (request, _) = tree
            .begin_directory_request(&empty.endpoint, 3)
            .expect("empty request");
        assert!(tree.complete_directory_request(
            &empty.endpoint,
            3,
            request,
            true,
            Ok(DirectoryListing {
                directory: empty.endpoint.clone(),
                entries: Vec::new()
            }),
        ));
        tree.expand(&failed.endpoint);
        let (request, _) = tree
            .begin_directory_request(&failed.endpoint, 3)
            .expect("failed request");
        assert!(tree.complete_directory_request(
            &failed.endpoint,
            3,
            request,
            true,
            Err("unavailable".into()),
        ));
    });
    visual.simulate_resize(size(px(1180.), px(700.)));
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(24.));
        window.set_rem_size(px(24.));
        pane.update(cx, |_, cx| cx.notify());
    });
    support::draw(visual);
    for selector in [
        "files-tree-entry-1-/srv/a-empty",
        "files-tree-placeholder",
        "files-tree-entry-1-/srv/b-failed",
        "files-tree-retry",
        "files-tree-entry-1-/srv/z-file",
    ] {
        let row = visual.debug_bounds(selector).expect("tree row");
        assert_eq!(row.size.height, px(48.), "{selector}: {row:?}");
    }
    visual.update(|window, cx| {
        Theme::update(cx, |theme| theme.font_size = px(16.));
        window.set_rem_size(px(16.));
    });
    let _runtime = runtime;
}

/// 短面板展开多条任务时仍需保留可操作的目录区域，避免固定队列高度挤走文件内容。
/// 两种窗口高度共用实际布局约束，任务数量与展开状态不随高度调整而丢失。
///
/// @author kongweiguang
#[gpui_kit::test]
fn short_file_panel_keeps_browser_space_with_expanded_transfer_history(app: &mut TestAppContext) {
    use gpui_kit::{px, size};
    use kerminal_domain::{
        files::{ConflictPolicy, TransferKind, TransferRequest, TransferStatus, TransferTask},
        identity::TransferId,
    };

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/transfer-history-layout");
    let summaries = (0..6)
        .map(|index| {
            let request = TransferRequest {
                source: FileEndpoint::Local {
                    path: std::path::PathBuf::from(format!("source-{index}.txt")),
                },
                destination: support::remote_endpoint(
                    &host,
                    &format!("/srv/transfer-history-layout/target-{index}.txt"),
                ),
                kind: TransferKind::File,
                conflict_policy: ConflictPolicy::Rename,
                idle_timeout_seconds: None,
            };
            let mut summary = TransferTask::queued(TransferId::new(), request)
                .expect("fixture endpoints are distinct")
                .summary();
            summary.status = TransferStatus::Succeeded;
            summary
        })
        .collect();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        pane.queue.history_expanded = true;
        pane.transfer_rows = summaries;
    });
    for height in [600., 900.] {
        visual.simulate_resize(size(px(800.), px(height)));
        support::draw(visual);
        let browser = visual
            .debug_bounds("files-browser-list-1")
            .expect("browser list remains visible");
        let queue = visual
            .debug_bounds("rr-files-transfer-list")
            .expect("expanded transfer list remains visible");
        assert!(
            browser.size.height >= px(108.),
            "file browser must retain at least three default rows: {browser:?}"
        );
        assert!(
            queue.size.height < px(height * 0.45),
            "auxiliary transfer list must leave most height for browsing: {queue:?}"
        );
        visual.update(|_, cx| {
            assert_eq!(pane.read(cx).transfer_rows.len(), 6);
            assert!(pane.read(cx).queue.history_expanded);
        });
    }
    let _runtime = runtime;
}

/// 紧凑栏失去当前目标时不挂载传输区，重新绑定后可以恢复展示。
///
/// @author kongweiguang
#[gpui_kit::test]
fn compact_file_panel_hides_transfers_without_a_current_target(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/target");
    let (_runtime, pane, _, visual) = support::window_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = None;
        pane.queue_watch_started = true;
    });
    assert!(visual.debug_bounds("files-transfer-queue").is_none());
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
            cx.notify();
        });
    });
    support::draw(visual);
    assert!(visual.debug_bounds("files-transfer-queue").is_some());
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.current_terminal_endpoint = None;
            cx.notify();
        });
    });
    support::draw(visual);
    assert!(visual.debug_bounds("files-transfer-queue").is_none());
}

/// 通过真实队列制造已确认检查点，避免把普通取消误当作可继续的暂停。
///
/// @author kongweiguang
struct PausableUiBackend;

#[async_trait::async_trait]
impl kerminal_application::transfers::TransferBackend for PausableUiBackend {
    /// 此测试后端只由隔离队列使用，不访问文件系统或远程端点。
    ///
    /// @author kongweiguang
    fn supports(&self, _: &kerminal_domain::files::TransferRequest) -> bool {
        true
    }

    /// 发布可信检查点后等待取消，让测试覆盖真实的活暂停记录。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: kerminal_application::transfers::TransferExecutionContext,
    ) -> Result<
        kerminal_application::transfers::TransferExecutionResult,
        kerminal_application::transfers::TransferExecutionError,
    > {
        use kerminal_application::transfers::{
            TransferCheckpoint, TransferExecutionError, TransferPhase, TransferProgressUpdate,
            TransferStagingOwnership,
        };
        use kerminal_domain::files::FileRevision;

        let destination = context.request.destination.clone();
        let staging = FileEndpoint::Local {
            path: ".ui-fixture.partial".into(),
        };
        let checkpoint = TransferCheckpoint {
            source_revision: FileRevision {
                size: 10,
                modified: None,
                permissions: None,
                content_digest: Some("b".repeat(64)),
            },
            confirmed_bytes: 5,
            confirmed_prefix_sha256: "c".repeat(64),
            staging_owner: context.transfer_id.clone(),
            staging_destination: staging.clone(),
            staging_token: "a".repeat(32),
            actual_destination: destination.clone(),
        };
        context.progress.begin_transfer(Some(10)).await.unwrap();
        context
            .progress
            .register_staging(TransferStagingOwnership {
                staging_destination: staging,
                actual_destination: destination.clone(),
                staging_owner: context.transfer_id.clone(),
                staging_token: checkpoint.staging_token.clone(),
            })
            .await
            .unwrap();
        context.progress.report(TransferProgressUpdate {
            confirmed_bytes: 5,
            total_bytes: Some(10),
            phase: TransferPhase::Transferring,
            actual_destination: Some(destination.clone()),
            checkpoint: Some(checkpoint.clone()),
            activity_sequence: 1,
        });
        context.cancellation.cancelled().await;
        Err(TransferExecutionError::Cancelled {
            checkpoint: Some(checkpoint),
            actual_destination: Some(destination),
        })
    }
}

/// 暂停任务保留继续入口和可信断点，普通清理不能移除该任务或暴露单项维护入口。
///
/// @author kongweiguang
#[gpui_kit::test]
fn paused_transfer_keeps_resume_button_and_survives_history_clear(app: &mut TestAppContext) {
    use std::time::Duration;

    use gpui_kit::{px, size};
    use kerminal_application::transfers::{TransferOrigin, TransferQueueOptions, TransferService};
    use kerminal_domain::files::{ConflictPolicy, TransferKind, TransferRequest, TransferStatus};
    use tokio_util::sync::CancellationToken;

    let directory = support::remote_endpoint(&HostId::new(), "/srv/target");
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        pane.queue.history_expanded = true;
    });
    let transfers = TransferService::new(
        vec![Arc::new(PausableUiBackend)],
        runtime.handle().clone(),
        TransferQueueOptions::default(),
    )
    .expect("isolated transfer queue");
    let origin = TransferOrigin::SharedWorkbench;
    let paused = runtime.block_on(async {
        let queued = transfers
            .enqueue_for_origin(
                TransferRequest {
                    source: FileEndpoint::Local {
                        path: "source.bin".into(),
                    },
                    destination: FileEndpoint::Local {
                        path: "destination.bin".into(),
                    },
                    kind: TransferKind::File,
                    conflict_policy: ConflictPolicy::Rename,
                    idle_timeout_seconds: None,
                },
                origin.clone(),
                None,
            )
            .await
            .expect("enqueue fixture");
        tokio::time::timeout(Duration::from_secs(3), async {
            while transfers.get(&queued.id).await.unwrap().bytes_transferred != 5 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            transfers.cancel(&queued.id).await.unwrap();
            loop {
                if let Some(summary) = transfers.get_settled(&queued.id).await.unwrap() {
                    break summary;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("pause settles")
    });
    assert_eq!(paused.status, TransferStatus::Cancelled);
    assert!(paused.resumable);
    let paused_id = paused.id.clone();
    assert_eq!(
        runtime
            .block_on(transfers.clear_completed_for_origin(&origin, CancellationToken::new()))
            .unwrap(),
        (0, 1)
    );
    assert!(
        runtime
            .block_on(transfers.get_settled(&paused_id))
            .expect("paused transfer remains in the queue")
            .expect("paused transfer remains settled")
            .resumable
    );
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfers = transfers.clone();
            pane.transfer_rows = vec![paused];
            cx.notify();
        });
    });
    visual.simulate_resize(size(px(800.), px(600.)));
    support::draw(visual);
    visual.update(|window, _| {
        assert!(
            window
                .debug_selector_bounds(&format!("rr-files-transfer-{paused_id}"))
                .is_some(),
            "the paused transfer card is rendered"
        );
        let find = |id: String| gpui_kit::base::test_support::find(window, &[], &id.into());
        assert!(
            find(format!("files-retry-{paused_id}")).is_some_and(|button| button.visible()),
            "the paused transfer retains its resume button"
        );
        for prefix in [
            "files-archive-paused",
            "recovery-inspect",
            "recovery-locate",
            "recovery-archive",
            "recovery-staging",
        ] {
            let id = format!("{prefix}-{paused_id}");
            assert!(
                find(id.clone()).is_none(),
                "unexpected maintenance button: {id}"
            );
        }
    });
    runtime
        .block_on(transfers.shutdown())
        .expect("queue shutdown");
}

/// 单行导航和活动速率适配窄栏；终态撤下过期速率，避免让用户误以为传输仍在进行。
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
    summary.status = TransferStatus::Transferring;
    summary.bytes_transferred = 65;
    summary.total_bytes = Some(65);
    summary.speed_bytes_per_second = 2048;
    let transfer_selector = format!("rr-files-transfer-{}", summary.id);
    let speed_selector = format!("rr-files-transfer-speed-{}", summary.id);
    let percent_selector = format!("rr-files-transfer-percent-{}", summary.id);
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
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
        !percent.is_empty() && percent.right() <= transfer.right() - px(12.),
        "transfer percentage is clipped: percent={percent:?}, row={transfer:?}"
    );

    for status in [TransferStatus::Succeeded, TransferStatus::Failed] {
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.transfer_rows[0].status = status;
                cx.notify();
            });
        });
        support::draw(visual);
        assert!(
            visual
                .update(|window, _| window.debug_selector_bounds(&speed_selector))
                .is_none(),
            "terminal transfers must not retain a stale rate"
        );
        if status == TransferStatus::Succeeded {
            let completed_percent = visual
                .update(|window, _| window.debug_selector_bounds(&percent_selector))
                .expect("completed percentage remains visible");
            assert!(completed_percent.size.width >= px(24.));
            assert!(completed_percent.right() <= transfer.right() - px(12.));
        }
    }

    let _runtime = runtime;
}

/// 路径组合期间的 Enter 和跳转不提交，Escape 先结束组合，再允许用户放弃路径草稿。
///
/// @author kongweiguang
#[gpui_kit::test]
fn path_edit_respects_ime_before_navigation_or_cancel(app: &mut TestAppContext) {
    use gpui_kit::{EntityInputHandler as _, Focusable as _, px, size};

    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let seeded_directory = directory.clone();
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.queue_watch_started = true;
        support::seed_compact_listing(pane, seeded_directory, Vec::new());
    });
    visual.simulate_resize(size(px(360.), px(600.)));
    support::draw(visual);
    let input = visual.update(|_, app| {
        pane.read(app).panels[1]
            .path_input
            .as_ref()
            .expect("rendered path input")
            .clone()
    });
    visual.update(|window, app| {
        let focus = input.read(app).focus_handle(app);
        window.focus(&focus, app);
        input.update(app, |input, cx| {
            input.set_value("/srv/", window, cx);
            input.replace_and_mark_text_in_range(None, "组合目录", None, window, cx);
            assert!(input.marked_text_range(window, cx).is_some());
        });
    });
    support::draw(visual);
    let before_generation = visual.update(|_, app| pane.read(app).panels[1].generation);
    visual.simulate_event(KeyDownEvent {
        keystroke: Keystroke::parse("enter").expect("valid enter key"),
        is_held: false,
        prefer_character_input: false,
    });
    support::draw(visual);
    visual.update(|_, app| {
        pane.update(app, |pane, cx| pane.commit_path_edit(1, cx));
        assert_eq!(pane.read(app).panels[1].generation, before_generation);
        assert!(input.read(app).is_composing());
    });

    visual.simulate_keystrokes("escape");
    support::draw(visual);
    visual.update(|_, app| {
        assert!(!input.read(app).is_composing());
        assert_ne!(input.read(app).value().as_ref(), "/srv");
        assert_eq!(pane.read(app).panels[1].generation, before_generation);
    });
    visual.simulate_keystrokes("escape");
    support::draw(visual);
    visual.update(|_, app| {
        assert_eq!(input.read(app).value().as_ref(), "/srv");
        assert_eq!(pane.read(app).panels[1].generation, before_generation);
    });
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
