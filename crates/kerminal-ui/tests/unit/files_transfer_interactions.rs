//! 传输按钮的等待反馈、来源切换与空队列布局回归。
//!
//! @author kongweiguang

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use gpui_kit::{MouseButton, TestAppContext, VisualTestContext, px, size};
use kerminal_application::{
    AppError, AppResult,
    transfers::{
        TransferBackend, TransferCleanupState, TransferExecutionContext, TransferExecutionError,
        TransferExecutionFailure, TransferExecutionResult, TransferOrigin, TransferQueueOptions,
        TransferRecoveryInspection, TransferRecoveryRecord, TransferResourceCleanup,
        TransferService,
    },
};
use kerminal_domain::{
    files::{
        ConflictPolicy, DirectoryListing, FileEndpoint, FileEntryKind, TransferFailureKind,
        TransferKind, TransferRequest, TransferStatus, TransferTask,
    },
    identity::{HostId, TransferId},
};
use tokio::runtime::Runtime;

use super::action_test_support as support;

/// 推进独立 runtime 与 UI 帧直到权威回执可见，超时直接失败而不以固定延迟冒充完成。
///
/// @author kongweiguang
fn wait_until(
    runtime: &Runtime,
    visual: &mut VisualTestContext,
    mut ready: impl FnMut(&mut VisualTestContext) -> bool,
) {
    for _ in 0..200 {
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(2)).await });
        support::draw(visual);
        if ready(visual) {
            return;
        }
    }
    panic!("transfer UI receipt did not settle");
}

/// 使用真实按钮命中区域派发点击，确保测试覆盖绘制与事件绑定。
///
/// @author kongweiguang
fn click_button(visual: &mut VisualTestContext, id: &str) {
    let bounds = visual.update(|window, _| {
        gpui_kit::base::test_support::find(window, &[], &id.to_owned().into())
            .filter(|button| button.visible())
            .expect("transfer button is visible")
            .bounds()
    });
    visual.simulate_mouse_move(bounds.center(), None, Default::default());
    visual.simulate_mouse_down(bounds.center(), MouseButton::Left, Default::default());
    visual.simulate_mouse_up(bounds.center(), MouseButton::Left, Default::default());
    support::draw(visual);
}

/// 同目录定位缺失目标时清掉旧多选，发布后可选中目标；目录读取失败仍显示真实错误。
///
/// @author kongweiguang
#[gpui_kit::test]
fn locate_destination_selects_only_existing_target_and_keeps_read_errors(app: &mut TestAppContext) {
    let parent = support::remote_endpoint(&HostId::new(), "/srv/uploads");
    let first = support::remote_entry(&parent, "old-one", FileEntryKind::File);
    let second = support::remote_entry(&parent, "old-two", FileEntryKind::File);
    let target = support::remote_entry(&parent, "jdk-21.0.2", FileEntryKind::Directory);
    let id = TransferId::new();
    let mut summary = TransferTask::queued(
        id.clone(),
        TransferRequest {
            source: FileEndpoint::Local {
                path: "jdk-21.0.2".into(),
            },
            destination: target.endpoint.clone(),
            kind: TransferKind::Directory,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        },
    )
    .expect("valid transfer request")
    .summary();
    summary.status = TransferStatus::Transferring;
    let initial_first = first.clone();
    let initial_second = second.clone();
    let (runtime, pane, mutations, visual) =
        support::window_fixture(app, parent.clone(), move |pane| {
            pane.compact_layout = true;
            pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
            pane.queue_watch_started = true;
            pane.transfer_rows = vec![summary];
            support::seed_listing(pane, 1, 1, vec![initial_first.clone(), initial_second]);
            pane.panels[1].selected = Some(initial_first);
            pane.panels[1]
                .selection
                .select_all(&["old-one".into(), "old-two".into()]);
        });
    *mutations.directory_listing.lock().unwrap() = Some(DirectoryListing {
        directory: parent.clone(),
        entries: vec![first.clone(), second.clone()],
    });
    let button = format!("files-locate-destination-{id}");
    click_button(visual, &button);
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            pane.read(cx).panels[1].generation > 1 && !pane.read(cx).panels[1].loading
        })
    });
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        let panel = &pane.panels[1];
        assert_eq!(panel.endpoint, parent);
        assert!(panel.selected.is_none());
        assert!(!panel.selection.contains("old-one"));
        assert!(!panel.selection.contains("old-two"));
        assert!(pane.status.is_none());
    });

    *mutations.directory_listing.lock().unwrap() = Some(DirectoryListing {
        directory: parent.clone(),
        entries: vec![first, second, target.clone()],
    });
    let generation = visual.update(|_, cx| pane.read(cx).panels[1].generation);
    click_button(visual, &button);
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            pane.read(cx).panels[1].generation > generation && !pane.read(cx).panels[1].loading
        })
    });
    visual.update(|_, cx| {
        let panel = &pane.read(cx).panels[1];
        assert_eq!(
            panel.selected.as_ref().map(|entry| &entry.endpoint),
            Some(&target.endpoint)
        );
        assert!(panel.selection.contains("jdk-21.0.2"));
        assert!(!panel.selection.contains("old-one"));
        assert!(!panel.selection.contains("old-two"));
    });

    *mutations.directory_listing.lock().unwrap() = None;
    let generation = visual.update(|_, cx| pane.read(cx).panels[1].generation);
    click_button(visual, &button);
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            pane.read(cx).panels[1].generation > generation && !pane.read(cx).panels[1].loading
        })
    });
    visual.update(|_, cx| {
        let panel = &pane.read(cx).panels[1];
        assert!(panel.directory_error.is_some());
        assert_eq!(panel.failed_directory.as_ref(), Some(&parent));
    });
}

/// 工作区恢复仍要求选中已保存项目，目标缺失时保留原有恢复提示。
///
/// @author kongweiguang
#[gpui_kit::test]
fn missing_restored_selection_still_reports_unavailable(app: &mut TestAppContext) {
    let parent = support::remote_endpoint(&HostId::new(), "/srv/restored");
    let (runtime, pane, mutations, visual) = support::window_fixture(app, parent.clone(), |_| {});
    *mutations.directory_listing.lock().unwrap() = Some(DirectoryListing {
        directory: parent.clone(),
        entries: Vec::new(),
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.open_workspace_transfer(
                FileEndpoint::Local {
                    path: "left".into(),
                },
                Some(parent),
                None,
                Some("missing".into()),
                cx,
            )
            .expect("restore valid workspace");
        });
    });
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).panels[1].loading)
    });
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(matches!(
            pane.status.as_deref(),
            Some("恢复的选择项当前不可用。" | "The restored selection is no longer available.")
        ));
    });
}

/// 可控清理 owner 允许测试失败后重试，不访问真实连接或文件。
struct RetryCleanupOwner {
    allowed: AtomicBool,
    attempts: AtomicUsize,
    released: AtomicBool,
}

#[async_trait::async_trait]
impl TransferResourceCleanup for RetryCleanupOwner {
    /// 首次失败后保留同一个 owner，显式允许时才确认资源已经释放。
    ///
    /// @author kongweiguang
    async fn cleanup_until(&self, _deadline: tokio::time::Instant) -> AppResult<()> {
        self.attempts.fetch_add(1, Ordering::AcqRel);
        if !self.allowed.load(Ordering::Acquire) {
            return Err(AppError::timeout("fixture cleanup"));
        }
        self.released.store(true, Ordering::Release);
        Ok(())
    }
}

/// 首次传输携带待清理 owner，后继执行必须观察到 owner 已释放。
struct RetryCleanupBackend {
    owner: Arc<RetryCleanupOwner>,
    attempts: AtomicUsize,
    retryable: bool,
}

#[async_trait::async_trait]
impl TransferBackend for RetryCleanupBackend {
    /// 独立测试队列只使用本机占位端点，不访问系统文件。
    ///
    /// @author kongweiguang
    fn supports(&self, _: &TransferRequest) -> bool {
        true
    }

    /// 每次尝试均可失败并重试，用于验证清理前置门禁与重试后动作转交给最新后继。
    ///
    /// @author kongweiguang
    async fn execute(
        &self,
        context: TransferExecutionContext,
    ) -> Result<TransferExecutionResult, TransferExecutionError> {
        let attempt = self.attempts.fetch_add(1, Ordering::AcqRel);
        let cleanup = if attempt == 0 {
            TransferCleanupState::pending(self.owner.clone())
        } else {
            if self.retryable {
                assert!(self.owner.released.load(Ordering::Acquire));
            }
            TransferCleanupState::Released
        };
        Err(TransferExecutionError::Failed(
            TransferExecutionFailure::new(
                TransferFailureKind::Connection,
                self.retryable,
                false,
                None,
                Some(context.request.destination),
                cleanup,
            )
            .with_safe_message("fixture transfer failure"),
        ))
    }
}

/// 清理失败不得创建后继；成功后旧按钮消失，后继再次失败仍能从自己的卡片重试。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_transfer_retry_waits_for_cleanup_and_can_retry_after_cleanup_failure(
    app: &mut TestAppContext,
) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/retry-cleanup");
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
    });
    let owner = Arc::new(RetryCleanupOwner {
        allowed: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        released: AtomicBool::new(false),
    });
    let backend = Arc::new(RetryCleanupBackend {
        owner: owner.clone(),
        attempts: AtomicUsize::new(0),
        retryable: true,
    });
    let transfers = TransferService::new(
        vec![backend.clone()],
        runtime.handle().clone(),
        TransferQueueOptions::default(),
    )
    .expect("isolated transfer queue");
    let origin = TransferOrigin::SharedWorkbench;
    let failed = runtime.block_on(async {
        let task = transfers
            .enqueue_for_origin(
                TransferRequest {
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
                origin.clone(),
                None,
            )
            .await
            .expect("enqueue fixture");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(summary) = transfers.get_settled(&task.id).await.unwrap() {
                    break summary;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("fixture transfer settles")
    });
    assert!(failed.retryable && failed.cleanup_pending);
    let id = failed.id.clone();
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfers = transfers.clone();
            pane.transfer_origin = Some(origin);
            pane.transfer_rows = vec![failed];
            cx.notify();
        });
    });
    support::draw(visual);
    let retry = format!("files-retry-{id}");
    click_button(visual, &retry);
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_retrying_transfer(&id))
    });
    assert_eq!(owner.attempts.load(Ordering::Acquire), 1);
    assert_eq!(backend.attempts.load(Ordering::Acquire), 1);
    let failed_again = runtime.block_on(transfers.get(&id)).unwrap();
    assert!(failed_again.cleanup_pending);
    assert!(failed_again.retry_successor.is_none());

    owner.allowed.store(true, Ordering::Release);
    click_button(visual, &retry);
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_retrying_transfer(&id))
    });
    assert!(owner.released.load(Ordering::Acquire));
    let original = runtime.block_on(transfers.get(&id)).unwrap();
    assert!(!original.cleanup_pending);
    let successor = original.retry_successor.expect("retry creates successor");
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), async {
            while transfers.get_settled(&successor).await.unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("successor settles");
    });
    visual.update(|_, cx| pane.update(cx, |pane, cx| pane.reload_transfers(cx)));
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            pane.read(cx)
                .transfer_rows
                .iter()
                .any(|row| row.id == successor && row.status == TransferStatus::Failed)
        })
    });
    visual.update(|window, _| {
        assert!(gpui_kit::base::test_support::find(window, &[], &retry.into()).is_none());
    });
    click_button(visual, &format!("files-retry-{successor}"));
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_retrying_transfer(&successor))
    });
    assert_eq!(backend.attempts.load(Ordering::Acquire), 3);
}

/// 清理失败保留原 owner；成功只移除本来源且不再渲染恢复副本，完整证据仍在归档。
///
/// @author kongweiguang
#[gpui_kit::test]
fn history_clear_retries_terminal_cleanup_and_preserves_other_origins(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/history-cleanup");
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
    });
    let owner = Arc::new(RetryCleanupOwner {
        allowed: AtomicBool::new(false),
        attempts: AtomicUsize::new(0),
        released: AtomicBool::new(false),
    });
    let backend = Arc::new(RetryCleanupBackend {
        owner: owner.clone(),
        attempts: AtomicUsize::new(0),
        retryable: false,
    });
    let transfers = TransferService::new(
        vec![backend],
        runtime.handle().clone(),
        TransferQueueOptions::default(),
    )
    .expect("isolated transfer queue");
    let origin = TransferOrigin::SharedWorkbench;
    let (failed, other) = runtime.block_on(async {
        let request = TransferRequest {
            source: FileEndpoint::Local {
                path: "source".into(),
            },
            destination: FileEndpoint::Local {
                path: "destination".into(),
            },
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        };
        let task = transfers
            .enqueue_for_origin(request.clone(), origin.clone(), None)
            .await
            .expect("enqueue fixture");
        let settled = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(summary) = transfers.get_settled(&task.id).await.unwrap() {
                    break summary;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("fixture settles");
        // 另一来源沿用无 IO 的占位后端；本次清理不能因相同端点而误选它。
        let other = transfers
            .enqueue(request)
            .await
            .expect("enqueue other origin");
        (settled, other)
    });
    assert!(!failed.retryable && failed.cleanup_pending);
    let id = failed.id.clone();
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfers = transfers.clone();
            pane.transfer_origin = Some(origin.clone());
            pane.transfer_rows = vec![failed];
            cx.notify();
        });
    });
    support::draw(visual);
    click_button(visual, "files-transfer-clear-completed");
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_clearing_for_origin(Some(&origin)))
    });
    assert_eq!(owner.attempts.load(Ordering::Acquire), 1);
    assert!(
        runtime
            .block_on(transfers.get(&id))
            .unwrap()
            .cleanup_pending
    );
    visual.update(|_, cx| assert!(pane.read(cx).status.is_some()));

    owner.allowed.store(true, Ordering::Release);
    click_button(visual, "files-transfer-clear-completed");
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            !pane.queue.is_clearing_for_origin(Some(&origin)) && !pane.queue.recovery_list_loading
        })
    });
    assert!(owner.released.load(Ordering::Acquire));
    assert!(runtime.block_on(transfers.get(&id)).is_err());
    assert!(runtime.block_on(transfers.get(&other.id)).is_ok());
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(pane.status.is_none());
        assert!(pane.transfer_rows.is_empty());
        assert!(pane.transfer_recovery_rows.is_empty());
    });
    let archived = runtime
        .block_on(transfers.list_archived_recovery(
            None,
            10,
            tokio_util::sync::CancellationToken::new(),
        ))
        .unwrap();
    assert_eq!(archived.records.len(), 1);
    assert_eq!(archived.records[0].id, id);
    assert!(!archived.records[0].cleanup_pending);

    // 兼容此前仅移除 live 的清理路径：无待办证据的恢复副本仍能从同一顶部入口收起。
    let orphan = runtime.block_on(async {
        let task = transfers
            .enqueue_for_origin(archived.records[0].request.clone(), origin.clone(), None)
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while transfers.get_settled(&task.id).await.unwrap().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        transfers
            .clear_completed_for_origin(&origin, tokio_util::sync::CancellationToken::new())
            .await
            .unwrap();
        task.id
    });
    visual.update(|_, cx| pane.update(cx, |pane, cx| pane.reload_transfers(cx)));
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.recovery_list_loading)
    });
    visual.update(|_, cx| assert_eq!(pane.read(cx).transfer_recovery_rows[0].id, orphan));
    click_button(visual, "files-transfer-clear-completed");
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            !pane.queue.is_clearing_history() && !pane.queue.recovery_list_loading
        })
    });
    visual.update(|_, cx| assert!(pane.read(cx).transfer_recovery_rows.is_empty()));
}

/// 失败卡片只暴露可用重试，未知提交不能显示重试或恢复材料操作。
///
/// @author kongweiguang
#[gpui_kit::test]
fn failed_card_hides_maintenance_actions_and_unknown_commit_retry(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/failed-card");
    let id = TransferId::new();
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: "source".into(),
        },
        destination: support::remote_endpoint(&HostId::new(), "/srv/failed-card/target"),
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Rename,
        idle_timeout_seconds: None,
    };
    let mut summary = TransferTask::queued(id.clone(), request.clone())
        .unwrap()
        .summary();
    summary.status = TransferStatus::Failed;
    summary.retryable = true;
    summary.cleanup_pending = true;
    summary.safe_error = Some("fixture transfer failure".into());
    let recovery = TransferRecoveryRecord {
        id: id.clone(),
        origin: Some(TransferOrigin::SharedWorkbench),
        request,
        drag_staging: false,
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: true,
        commit_unknown: false,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    };
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        pane.transfer_rows = vec![summary];
        pane.transfer_recovery_rows = vec![recovery];
    });
    visual.simulate_resize(size(px(800.), px(600.)));
    support::draw(visual);
    visual.update(|window, _| {
        let visible = |id: String| {
            gpui_kit::base::test_support::find(window, &[], &id.into())
                .filter(|element| element.visible())
                .is_some()
        };
        assert!(visible(format!("files-retry-{id}")));
        for removed in [
            format!("files-retry-cleanup-{id}"),
            format!("files-locate-destination-{id}"),
            format!("recovery-inspect-{id}"),
            format!("recovery-locate-{id}"),
            format!("recovery-archive-{id}"),
            format!("recovery-staging-{id}"),
        ] {
            assert!(!visible(removed));
        }
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.transfer_rows[0].failure_kind = Some(TransferFailureKind::CommitUnknown);
            pane.transfer_recovery_rows[0].commit_unknown = true;
            cx.notify();
        });
    });
    support::draw(visual);
    visual.update(|window, _| {
        assert!(
            gpui_kit::base::test_support::find(window, &[], &format!("files-retry-{id}").into(),)
                .is_none()
        );
    });
    let _runtime = runtime;
}

/// 空队列刷新与清理只改变按钮状态，不应临时增加正文提示或让后续错误自动展开旧详情。
///
/// @author kongweiguang
#[gpui_kit::test]
fn empty_transfer_refresh_and_cleanup_keep_header_height(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/empty-transfer-queue");
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
    });
    for width in [800., 1400.] {
        visual.simulate_resize(size(px(width), px(600.)));
        support::draw(visual);
        let idle = visual.debug_bounds("files-transfer-queue").unwrap();
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.queue.recovery_list_loading = true;
                cx.notify();
            });
        });
        support::draw(visual);
        let refreshing = visual.debug_bounds("files-transfer-queue").unwrap();
        assert_eq!(refreshing.size.height, idle.size.height);
        visual.update(|_, cx| {
            pane.update(cx, |pane, cx| {
                pane.queue.recovery_list_loading = false;
                cx.notify();
            });
        });
        support::draw(visual);
    }
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            pane.status = Some("previous cleanup details".into());
            pane.status_details_expanded = true;
            pane.clear_completed_transfer_history(cx);
        });
    });
    support::draw(visual);
    let clearing = visual.debug_bounds("files-transfer-queue").unwrap();
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| {
            let pane = pane.read(cx);
            !pane
                .queue
                .is_clearing_for_origin(pane.transfer_origin.as_ref())
                && !pane.queue.recovery_list_loading
        })
    });
    assert_eq!(
        clearing.size.height,
        visual
            .debug_bounds("files-transfer-queue")
            .unwrap()
            .size
            .height
    );
    visual.update(|_, cx| {
        let pane = pane.read(cx);
        assert!(pane.status.is_none());
        assert!(!pane.status_details_expanded);
        assert!(pane.transfer_rows.is_empty());
    });
}

/// 在服务回执前真实点击两次，随后切换来源；旧失败不能污染新页，原按钮门禁必须释放。
///
/// @author kongweiguang
#[gpui_kit::test]
fn retry_button_is_busy_until_receipt_even_after_origin_changes(app: &mut TestAppContext) {
    let id = TransferId::new();
    let mut summary = TransferTask::queued(
        id.clone(),
        TransferRequest {
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
    )
    .unwrap()
    .summary();
    // 仅投影按钮；应用队列不存在这个 ID，真实用例将返回可观察的失败而不执行文件 IO。
    summary.status = TransferStatus::Cancelled;
    summary.resumable = true;
    let directory = support::remote_endpoint(&HostId::new(), "/srv/retry-button");
    let (runtime, pane, _, visual) = support::window_fixture(app, directory, move |pane| {
        pane.compact_layout = true;
        pane.current_terminal_endpoint = Some(pane.panels[1].endpoint.clone());
        pane.queue_watch_started = true;
        pane.transfer_rows = vec![summary];
    });
    let button_id = format!("files-retry-{id}");
    for _ in 0..2 {
        let bounds = visual.update(|window, _| {
            gpui_kit::base::test_support::find(window, &[], &button_id.clone().into())
                .filter(|button| button.visible())
                .unwrap()
                .bounds()
        });
        visual.simulate_mouse_move(bounds.center(), None, Default::default());
        visual.simulate_mouse_down(bounds.center(), MouseButton::Left, Default::default());
        visual.simulate_mouse_up(bounds.center(), MouseButton::Left, Default::default());
        support::draw(visual);
        visual.update(|_, cx| assert!(pane.read(cx).queue.is_retrying_transfer(&id)));
    }
    visual.update(|_, cx| {
        pane.update(cx, |pane, _| {
            assert!(!pane.queue.begin_transfer_retry(id.clone()));
            pane.transfer_origin = None;
            pane.status = Some("new source feedback".into());
        });
    });
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_retrying_transfer(&id))
    });
    visual.update(|_, cx| {
        pane.update(cx, |pane, cx| {
            assert_eq!(pane.status.as_deref(), Some("new source feedback"));
            pane.transfer_origin = Some(TransferOrigin::SharedWorkbench);
            pane.retry_transfer(id.clone(), true, cx);
            assert!(pane.queue.is_retrying_transfer(&id));
        });
    });
    wait_until(&runtime, visual, |visual| {
        visual.update(|_, cx| !pane.read(cx).queue.is_retrying_transfer(&id))
    });
    visual.update(|_, cx| {
        assert!(pane.read(cx).status.is_some());
        assert_ne!(pane.read(cx).status.as_deref(), Some("new source feedback"));
    });
}
