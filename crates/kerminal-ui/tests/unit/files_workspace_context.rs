//! 文件工作区来源、单目录外壳与任务查询的隔离回归。
//!
//! @author kongweiguang

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use gpui_kit::{MouseButton, TestAppContext, px, size};
use kerminal_application::transfers::{
    InMemoryTransferRecoveryRepository, SftpTransferPolicyStore, TransferBackend,
    TransferExecutionContext, TransferExecutionError, TransferExecutionResult, TransferOrigin,
    TransferQueryScope, TransferQueueOptions, TransferRecoveryInspection, TransferRecoveryPage,
    TransferRecoveryRecord, TransferRecoveryRepository, TransferService,
};
use kerminal_domain::{
    files::{
        ConflictPolicy, ContainerRuntime, FileEndpoint, RemotePath, TransferKind, TransferRequest,
    },
    identity::{HostId, TabId, TransferId},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::features::files::action_test_support as support;

/// 同主机文档激活保留已提交列表和选择；不同主机才撤销旧目录的操作资格。
///
/// @author kongweiguang
#[gpui_kit::test]
fn file_workspace_preserves_same_filesystem_and_invalidates_cross_host(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let item = support::remote_entry(
        &directory,
        "selected",
        kerminal_domain::files::FileEntryKind::File,
    );
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 7, vec![item.clone()]);
        pane.panels[1].selection.select_only(&item.name);
        pane.panels[1].selected = Some(item);
    });
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            let workspace = TabId::new();
            pane.set_file_workspace_target(
                workspace.clone(),
                support::remote_endpoint(&host, "/other-parent"),
                None,
                cx,
            );
            assert_eq!(pane.panels[1].generation, 7);
            assert_eq!(pane.current_directory_endpoint(), &directory);
            assert!(pane.panels[1].selection.contains("selected"));
            assert!(pane.panels[1].selected.is_some());
            assert!(pane.panels[1].listing.is_some());
            let other = support::remote_endpoint(&HostId::new(), "/new");
            pane.set_file_workspace_target(workspace, other.clone(), None, cx);
            assert!(pane.panels[1].generation > 7);
            assert!(pane.panels[1].listing.is_none());
            assert!(pane.panels[1].selected.is_none());
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&other));
        })
    });
}

/// 容器必须保持自己的 endpoint，宿主与容器切换不能借保留浏览缓存串目标。
///
/// @author kongweiguang
#[gpui_kit::test]
fn container_workspace_never_maps_to_its_host(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv");
    let fixture = support::entity_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 4, Vec::new());
    });
    let container = FileEndpoint::Container {
        host_id: Some(host),
        runtime: ContainerRuntime::Docker,
        container_id: "workspace-container".into(),
        path: RemotePath::parse("/srv").unwrap(),
    };
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_file_workspace_target(TabId::new(), container.clone(), None, cx);
            assert_eq!(pane.current_terminal_endpoint.as_ref(), Some(&container));
            assert_eq!(pane.panels[1].requested_endpoint.as_ref(), Some(&container));
            assert!(pane.compact_target_is_available());
            assert!(pane.current_terminal_binding.is_none());
        })
    });
}

/// 真实筛选按钮只改变查询；正在准备的创建来源与目录读取代次不能随按钮切换。
///
/// @author kongweiguang
#[gpui_kit::test]
fn scope_buttons_keep_creation_origin_and_directory(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let (_runtime, pane, mutations, visual) =
        support::window_fixture(app, directory.clone(), |pane| {
            pane.queue_watch_started = true;
            support::seed_listing(pane, 0, 2, Vec::new());
            support::seed_listing(pane, 1, 3, Vec::new());
        });
    visual.simulate_resize(size(px(1200.), px(800.)));
    support::draw(visual);
    for (selector, all) in [
        ("files-transfer-scope-all", true),
        ("files-transfer-scope-current", false),
    ] {
        let bounds = visual
            .debug_bounds(selector)
            .expect("scope button must be rendered");
        visual.simulate_mouse_move(bounds.center(), None, Default::default());
        visual.simulate_mouse_down(bounds.center(), MouseButton::Left, Default::default());
        visual.simulate_mouse_up(bounds.center(), MouseButton::Left, Default::default());
        support::draw(visual);
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            assert_eq!(
                matches!(pane.transfer_query_scope, TransferQueryScope::All),
                all
            );
            assert_eq!(pane.transfer_origin, Some(TransferOrigin::SharedWorkbench));
            assert_eq!(pane.panels[1].endpoint, directory);
            assert_eq!(pane.panels[1].generation, 3);
            assert_eq!(pane.panels[0].generation, 2);
        });
    }
    assert_eq!(
        mutations
            .list_directory
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// 中央目录只挂载同一个 BrowserPanel，不绘制隐藏对侧或传输列表，避免内部标签重复。
///
/// @author kongweiguang
#[gpui_kit::test]
fn central_directory_renders_one_browser_without_transfer_chrome(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let (_runtime, pane, _, visual) = support::window_fixture(app, directory.clone(), |pane| {
        pane.queue_watch_started = true;
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.configure_file_workspace(
                directory,
                None,
                vec![TransferOrigin::WorkspaceFile(TabId::new())],
                cx,
            )
            .unwrap();
            support::seed_listing(pane, 1, 8, Vec::new());
            pane.panels[1].loading = false;
            cx.notify();
        })
    });
    support::draw(visual);
    assert!(visual.debug_bounds("rr-files-browser-1").is_some());
    assert!(visual.debug_bounds("rr-files-browser-0").is_none());
    assert!(visual.debug_bounds("files-transfer-queue").is_none());
    assert!(visual.debug_bounds("files-transfer-scope-all").is_none());
}

/// 已提交写入的 busy 不因对话框被撤销而放行，未决确认也必须保留其明确 owner。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_close_keeps_mutation_receipt_owner(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let fixture = support::entity_fixture(app, directory, |_| {});
    app.update(|cx| {
        fixture.pane.update(cx, |pane, _| {
            assert!(!pane.file_workspace_directory_close_blocked());
            pane.name_action_busy = true;
            assert!(pane.file_workspace_directory_close_blocked());
            pane.name_action_busy = false;
            pane.permissions.busy = true;
            assert!(pane.file_workspace_directory_close_blocked());
            pane.permissions.busy = false;
            pane.delete_action_busy = true;
            assert!(pane.file_workspace_directory_close_blocked());
            pane.delete_action_busy = false;
            pane.name_action = Some(crate::features::files::NameAction::CreateDirectory);
            assert!(pane.file_workspace_directory_close_blocked());
            pane.name_action = None;
            assert!(!pane.file_workspace_directory_close_blocked());
        })
    });
}

/// 使用独立闸门控制归档仓储，确保测试覆盖服务执行与 UI 回执之间的真实在途窗口。
struct GatedRecoveryRepository {
    inner: InMemoryTransferRecoveryRepository,
    started: AtomicBool,
    finished: AtomicBool,
    release: Notify,
    fail: bool,
}

#[async_trait::async_trait]
impl TransferRecoveryRepository for GatedRecoveryRepository {
    /// 普通读取使用真实内存仓储，使筛选后的可见记录与归档证据保持一致。
    async fn list(
        &self,
        cancellation: CancellationToken,
    ) -> AppResult<Vec<TransferRecoveryRecord>> {
        self.inner.list(cancellation).await
    }

    /// 测试种子通过原仓储写入，避免自造不同于应用契约的恢复状态。
    async fn upsert(
        &self,
        record: TransferRecoveryRecord,
        cancellation: CancellationToken,
    ) -> AppResult<()> {
        self.inner.upsert(record, cancellation).await
    }

    /// 未使用的删除入口仍委托真实仓储，避免测试替身悄悄伪造成功。
    async fn remove(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.inner.remove(id, cancellation).await
    }

    /// 固定暂停提交时序，使切换来源和折叠队列发生在真实回执之前。
    async fn archive(&self, id: &TransferId, cancellation: CancellationToken) -> AppResult<()> {
        self.started.store(true, Ordering::Release);
        self.release.notified().await;
        let result = if self.fail {
            Err(AppError::infrastructure("archive fixture failure"))
        } else {
            self.inner.archive(id, cancellation).await
        };
        self.finished.store(true, Ordering::Release);
        result
    }

    /// 沿用真实分页结果，断言成功归档时不会只验证 UI 忙态。
    async fn list_archived(
        &self,
        cursor: Option<TransferId>,
        limit: usize,
        cancellation: CancellationToken,
    ) -> AppResult<TransferRecoveryPage> {
        self.inner.list_archived(cursor, limit, cancellation).await
    }
}

/// 本测试只归档恢复记录，不允许意外启动真实传输。
struct RecoveryOnlyTransferBackend;

#[async_trait::async_trait]
impl TransferBackend for RecoveryOnlyTransferBackend {
    /// 禁止测试把恢复归档误解释成传输准入。
    fn supports(&self, _: &TransferRequest) -> bool {
        false
    }

    /// 意外执行表明测试越过了只读恢复边界，应立即暴露错误。
    async fn execute(
        &self,
        _: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        panic!("recovery fixture must not execute transfers")
    }
}

/// 同一套受控归档覆盖成功与失败，避免仅靠手动修改 busy 标志推断回执生命周期。
///
/// @author kongweiguang
fn assert_recovery_archive_close_guard(app: &mut TestAppContext, fail: bool, rebind: bool) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/recovery-guard");
    let fixture = support::entity_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        support::seed_listing(pane, 1, 7, Vec::new());
    });
    let origin = TransferOrigin::WorkspaceFile(TabId::new());
    let id = TransferId::new();
    let record = TransferRecoveryRecord {
        id: id.clone(),
        origin: Some(origin.clone()),
        request: TransferRequest {
            source: FileEndpoint::Local {
                path: "source".into(),
            },
            destination: FileEndpoint::Local {
                path: "destination".into(),
            },
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        },
        drag_staging: false,
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: false,
        commit_unknown: false,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    };
    let repository = Arc::new(GatedRecoveryRepository {
        inner: InMemoryTransferRecoveryRepository::default(),
        started: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        release: Notify::new(),
        fail,
    });
    fixture
        .runtime
        .block_on(repository.upsert(record, CancellationToken::new()))
        .expect("recovery fixture is stored");
    let transfers = TransferService::new_with_recovery_repository(
        vec![Arc::new(RecoveryOnlyTransferBackend)],
        fixture.runtime.handle().clone(),
        TransferQueueOptions::default(),
        SftpTransferPolicyStore::new(Default::default()).expect("test transfer policy"),
        repository.clone(),
        None,
    )
    .expect("isolated transfer queue");
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.transfers = transfers;
            pane.set_transfer_origin(Some(origin), cx);
            assert!(!pane.file_workspace_directory_close_blocked());
            assert!(pane.capture_file_browser_context(cx).is_some());
            pane.dismiss_transfer_recovery(id.clone(), cx);
            assert!(pane.file_workspace_directory_close_blocked());
        });
    });

    for _ in 0..4_000 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
        if repository.started.load(Ordering::Acquire)
            && app.update(|cx| {
                fixture
                    .pane
                    .read(cx)
                    .transfer_recovery_rows
                    .iter()
                    .any(|row| row.id == id)
            })
        {
            break;
        }
    }
    assert!(repository.started.load(Ordering::Acquire));
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            assert!(pane.transfer_recovery_rows.iter().any(|row| row.id == id));
            pane.toggle_transfer_history(cx);
            assert!(!pane.queue.history_expanded);
            assert!(pane.file_workspace_directory_close_blocked());
            let other_origin = TransferOrigin::WorkspaceFile(TabId::new());
            pane.set_transfer_query_scope(TransferQueryScope::origins([other_origin.clone()]), cx);
            if rebind {
                pane.set_transfer_origin(Some(other_origin), cx);
                pane.status = Some("new context status".to_owned());
            }
            assert!(pane.transfer_recovery_rows.is_empty());
            assert!(pane.file_workspace_directory_close_blocked());
            assert!(pane.capture_file_browser_context(cx).is_none());
        });
    });

    repository.release.notify_one();
    fixture.runtime.block_on(async {
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !repository.finished.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("archive repository returns");
    });
    app.update(|cx| {
        assert!(
            fixture
                .pane
                .read(cx)
                .file_workspace_directory_close_blocked()
        );
    });
    for _ in 0..4_000 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
        if app.update(|cx| !fixture.pane.read(cx).queue.is_dismissing_recovery(&id)) {
            break;
        }
    }
    app.update(|cx| {
        let pane = fixture.pane.read(cx);
        assert!(!pane.file_workspace_directory_close_blocked());
        assert!(pane.capture_file_browser_context(cx).is_some());
        if rebind {
            assert_eq!(pane.status.as_deref(), Some("new context status"));
        } else {
            assert_eq!(pane.status.is_some(), fail);
        }
    });
    let remaining = fixture
        .runtime
        .block_on(repository.list(CancellationToken::new()))
        .expect("active recovery remains readable");
    let archived = fixture
        .runtime
        .block_on(repository.list_archived(None, 10, CancellationToken::new()))
        .expect("archived recovery remains readable");
    assert_eq!(remaining.iter().any(|row| row.id == id), fail);
    assert_eq!(archived.records.iter().any(|row| row.id == id), !fail);
}

/// 归档成功也要等原面板处理回执；折叠和切换范围不能解除关闭门禁。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_close_waits_for_hidden_recovery_archive_success(app: &mut TestAppContext) {
    assert_recovery_archive_close_guard(app, false, false);
}

/// 归档失败回执必须留给原 owner 处理，隐藏恢复行不能提前放行关闭。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_close_waits_for_hidden_recovery_archive_failure(app: &mut TestAppContext) {
    assert_recovery_archive_close_guard(app, true, false);
}

/// 来源切换也不能释放原 owner，失败回执不得覆盖新来源的状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_close_waits_for_recovery_archive_after_origin_switch(app: &mut TestAppContext) {
    assert_recovery_archive_close_guard(app, true, true);
}

/// 多条归档互不合并，任一原回执未到都必须继续保留目录实体。
///
/// @author kongweiguang
#[gpui_kit::test]
fn directory_close_waits_for_every_recovery_archive(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/recovery-guard");
    let fixture = support::entity_fixture(app, directory, |_| {});
    app.update(|cx| {
        fixture.pane.update(cx, |pane, _| {
            let first = TransferId::new();
            let second = TransferId::new();
            assert!(pane.queue.begin_recovery_dismissal(&first));
            assert!(pane.queue.begin_recovery_dismissal(&second));
            assert!(!pane.queue.begin_recovery_dismissal(&first));
            assert!(pane.file_workspace_directory_close_blocked());
            pane.queue.finish_recovery_dismissal(&first);
            assert!(pane.file_workspace_directory_close_blocked());
            pane.queue.finish_recovery_dismissal(&second);
            assert!(!pane.file_workspace_directory_close_blocked());
        });
    });
}

/// 顶层标签更换Entity时复制浏览值而不共享滚动控制器，首个文件绑定不重新读取父目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn browser_snapshot_handoff_preserves_listing_selection_and_scroll(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let entry = support::remote_entry(
        &directory,
        "selected",
        kerminal_domain::files::FileEntryKind::File,
    );
    let source = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 7, vec![entry.clone()]);
        pane.panels[1].selection.select_only(&entry.name);
        pane.panels[1].selected = Some(entry);
        pane.panels[1]
            .list_scroll
            .set_offset(gpui_kit::point(px(0.), px(-96.)));
    });
    let snapshot = app.update(|cx| {
        source
            .pane
            .read(cx)
            .capture_file_browser_context(cx)
            .unwrap()
    });
    let destination =
        support::entity_fixture(app, support::remote_endpoint(&host, "/initial"), |pane| {
            pane.compact_layout = true;
        });
    let owner = TransferOrigin::WorkspaceFile(TabId::new());
    app.update(|cx| {
        destination.pane.update(cx, |pane, cx| {
            pane.set_transfer_origin(Some(owner.clone()), cx);
            assert!(pane.restore_file_browser_context(snapshot, cx));
            assert!(pane.current_terminal_binding.is_none());
            assert!(pane.current_terminal_endpoint.is_none());
            assert!(
                !pane.compact_target_is_available(),
                "快照不授予终端或目标访问能力"
            );
            pane.set_file_workspace_target(
                TabId::new(),
                support::remote_endpoint(&host, "/document-parent"),
                None,
                cx,
            );
            assert_eq!(pane.panels[1].endpoint, directory);
            assert!(pane.panels[1].listing.is_some());
            assert!(pane.panels[1].selection.contains("selected"));
            assert_eq!(pane.panels[1].selected.as_ref().unwrap().name, "selected");
            assert_eq!(pane.panels[1].list_scroll.offset().y, px(-96.));
            assert!(!pane.panels[1].loading);
            assert!(pane.panels[1].requested_endpoint.is_none());
            assert!(pane.panels[1].root_request_cancellation.is_none());
            assert_eq!(pane.transfer_origin, Some(owner));
        })
    });
    app.update(|cx| {
        source.pane.update(cx, |pane, _| {
            pane.panels[1]
                .list_scroll
                .set_offset(gpui_kit::point(px(0.), px(0.)));
        })
    });
    app.update(|cx| {
        assert_eq!(
            destination.pane.read(cx).panels[1].list_scroll.offset().y,
            px(-96.)
        )
    });
    assert_eq!(
        destination
            .mutations
            .list_directory
            .load(std::sync::atomic::Ordering::SeqCst),
        0
    );
}

/// 有效清单必须与精确端点一致；导航、写入和异步树不能作为已完成快照转交。
///
/// @author kongweiguang
#[gpui_kit::test]
fn browser_snapshot_rejects_in_flight_or_inconsistent_context(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let source = support::entity_fixture(app, directory.clone(), |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 7, Vec::new());
    });
    let mut snapshot = app.update(|cx| {
        source
            .pane
            .read(cx)
            .capture_file_browser_context(cx)
            .unwrap()
    });
    snapshot.listing.directory = support::remote_endpoint(&HostId::new(), "/other");
    let destination = support::entity_fixture(app, directory, |pane| pane.compact_layout = true);
    app.update(|cx| {
        destination.pane.update(cx, |pane, cx| {
            assert!(!pane.restore_file_browser_context(snapshot, cx));
            assert!(pane.panels[1].listing.is_none());
        })
    });
    app.update(|cx| {
        source.pane.update(cx, |pane, cx| {
            pane.name_action_busy = true;
            assert!(pane.capture_file_browser_context(cx).is_none());
            pane.name_action_busy = false;
            pane.view_mode = FilesViewMode::Tree;
            assert!(pane.capture_file_browser_context(cx).is_none());
            pane.view_mode = FilesViewMode::List;
            pane.panels[1].loading = true;
            assert!(pane.capture_file_browser_context(cx).is_none());
        })
    });
}

/// 已完成树缓存可转交展开、子项选择和独立滚动；Loading子树仍由源视图独占。
///
/// @author kongweiguang
#[gpui_kit::test]
fn loaded_tree_snapshot_rebuilds_without_sharing_widget_or_requests(app: &mut TestAppContext) {
    use gpui_kit::base::ScrollbarHandle as _;
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/manual");
    let folder = support::remote_entry(
        &directory,
        "folder",
        kerminal_domain::files::FileEntryKind::Directory,
    );
    let child = support::remote_entry(
        &folder.endpoint,
        "child",
        kerminal_domain::files::FileEntryKind::File,
    );
    let source_folder = folder.clone();
    let source = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.compact_layout = true;
        pane.view_mode = FilesViewMode::Tree;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 7, vec![source_folder.clone()]);
        pane.panels[1].tree.rebind_generation(7);
        let root = pane.panels[1].endpoint.clone();
        pane.panels[1]
            .tree
            .set_root_listing(root, 7, vec![source_folder], true);
    });
    let source_scroll = app.update(|cx| {
        source.pane.update(cx, |pane, cx| {
            let (request_id, _) = pane.panels[1]
                .tree
                .begin_directory_request(&folder.endpoint, 7)
                .unwrap();
            assert!(pane.capture_file_browser_context(cx).is_none());
            assert!(pane.panels[1].tree.complete_directory_request(
                &folder.endpoint,
                7,
                request_id,
                true,
                Ok(kerminal_domain::files::DirectoryListing {
                    directory: folder.endpoint.clone(),
                    entries: vec![child.clone()]
                })
            ));
            pane.panels[1].tree.expand(&folder.endpoint);
            pane.panels[1].tree.select_only(&child.endpoint);
            pane.panels[1].selected = Some(child.clone());
            pane.ensure_tree_widget(1, cx);
            let handle = pane.panels[1]
                .tree_widget
                .as_ref()
                .unwrap()
                .state
                .read(cx)
                .scroll_handle()
                .clone();
            handle.set_offset(gpui_kit::point(px(0.), px(-48.)));
            handle
        })
    });
    let snapshot = app.update(|cx| {
        source
            .pane
            .read(cx)
            .capture_file_browser_context(cx)
            .unwrap()
    });
    let destination =
        support::entity_fixture(app, directory.clone(), |pane| pane.compact_layout = true);
    app.update(|cx| {
        destination.pane.update(cx, |pane, cx| {
            assert!(pane.restore_file_browser_context(snapshot, cx));
            pane.set_file_workspace_target(TabId::new(), directory, None, cx);
            assert_eq!(pane.view_mode, FilesViewMode::Tree);
            assert!(
                pane.panels[1]
                    .tree
                    .visible_endpoints(true)
                    .contains(&child.endpoint)
            );
            assert_eq!(pane.panels[1].tree.selected_entries(true), vec![child]);
            assert!(pane.panels[1].root_request_cancellation.is_none());
            assert_eq!(
                pane.panels[1]
                    .tree_widget
                    .as_ref()
                    .unwrap()
                    .state
                    .read(cx)
                    .scroll_handle()
                    .offset()
                    .y,
                px(-48.)
            );
        })
    });
    source_scroll.set_offset(gpui_kit::point(px(0.), px(0.)));
    app.update(|cx| {
        assert_eq!(
            destination.pane.read(cx).panels[1]
                .tree_widget
                .as_ref()
                .unwrap()
                .state
                .read(cx)
                .scroll_handle()
                .offset()
                .y,
            px(-48.)
        )
    });
}

/// 缺少绑定只代表不可用，不能再次依赖 None 推断文件来源并跟随父目录。
///
/// @author kongweiguang
#[gpui_kit::test]
fn missing_terminal_binding_is_not_a_file_workspace(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let fixture = support::entity_fixture(app, directory.clone(), |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        support::seed_listing(pane, 1, 4, Vec::new());
    });
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_current_terminal_target(Some(directory), None, None, cx);
            assert!(!pane.compact_target_is_available());
            assert!(pane.panels[1].listing.is_none());
            assert_eq!(pane.compact_context, CompactFilesContext::Unavailable);
        })
    });
}
