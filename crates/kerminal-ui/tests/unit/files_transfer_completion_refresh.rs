//! 文件传输完成投影必须尊重浏览中的 requested endpoint 和 panel generation。
//!
//! @author kongweiguang

use super::{
    TransferRefreshScope, begin_panel_load_request, completed_transfer_refresh_directories,
    finish_panel_load_request, panel_needs_transfer_refresh, panel_refresh_target,
};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferCommitSummary, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus, TransferSummary, TransferTask,
    },
    identity::{HostId, TransferId},
};
use std::path::PathBuf;

/// 固定 Local endpoint 表达，让导航竞态用例不依赖进程当前目录。
///
/// @author kongweiguang
fn local(path: &str) -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from(path),
    }
}

/// 不同 HostId 即使展示相同路径，也必须保持独立的刷新范围。
///
/// @author kongweiguang
fn remote(host_id: &HostId, path: &str) -> FileEndpoint {
    FileEndpoint::remote(host_id.clone(), path).expect("test remote path must be valid")
}

/// 构造领域摘要的实际公开字段形状，回执数只由测试显式指定。
///
/// @author kongweiguang
fn summary(
    id: TransferId,
    status: TransferStatus,
    kind: TransferKind,
    requested: FileEndpoint,
    actual: Option<FileEndpoint>,
    receipts: u64,
) -> TransferSummary {
    let request = TransferRequest {
        source: local("source/input"),
        destination: requested,
        kind,
        conflict_policy: ConflictPolicy::Rename,
        idle_timeout_seconds: None,
    };
    let mut summary = TransferTask::queued(id, request)
        .expect("test endpoints must differ")
        .summary();
    summary.status = status;
    summary.actual_destination = actual;
    let mut committed = TransferCommitSummary::empty();
    for _ in 0..receipts {
        committed
            .record_confirmed_entry(0)
            .expect("receipt count fits");
    }
    summary.commit_summary = committed;
    summary
}

/// A→B 在途时 A 完成不能覆盖 B；B 后续完成必须命中 B 并重新读取 B。
///
/// @author kongweiguang
#[test]
fn earlier_completion_does_not_regress_navigation_and_requested_completion_refreshes() {
    let a_id = TransferId::new();
    let b_id = TransferId::new();
    let previous = vec![
        summary(
            a_id.clone(),
            TransferStatus::Transferring,
            TransferKind::File,
            local("A/file.txt"),
            None,
            0,
        ),
        summary(
            b_id.clone(),
            TransferStatus::Transferring,
            TransferKind::File,
            local("B/file.txt"),
            None,
            0,
        ),
    ];
    let a_completed = vec![
        summary(
            a_id.clone(),
            TransferStatus::Succeeded,
            TransferKind::File,
            local("A/file.txt"),
            Some(local("A/file.txt")),
            1,
        ),
        summary(
            b_id.clone(),
            TransferStatus::Transferring,
            TransferKind::File,
            local("B/file.txt"),
            None,
            0,
        ),
    ];
    let b_completed = vec![
        a_completed[0].clone(),
        summary(
            b_id,
            TransferStatus::Succeeded,
            TransferKind::File,
            local("B/file.txt"),
            Some(local("B/file.txt")),
            1,
        ),
    ];
    let committed_endpoint = local("A");
    let requested_endpoint = local("B");
    let mut pending_endpoint = None;
    let mut generation = 4;
    let mut loading = false;
    assert_eq!(
        begin_panel_load_request(
            &mut generation,
            &mut loading,
            &mut pending_endpoint,
            &requested_endpoint,
        ),
        5,
    );
    let target = panel_refresh_target(&committed_endpoint, pending_endpoint.as_ref(), loading)
        .expect("loading pane retains the requested endpoint");
    assert_eq!(target, &requested_endpoint);

    let a_directories = completed_transfer_refresh_directories(&previous, &a_completed);
    assert_eq!(
        a_directories,
        vec![TransferRefreshScope::Directory(local("A"))]
    );
    assert!(!panel_needs_transfer_refresh(
        &committed_endpoint,
        pending_endpoint.as_ref(),
        loading,
        true,
        &a_directories,
    ));
    assert_eq!(
        generation, 5,
        "A completion must not invalidate B's list request"
    );
    assert_eq!(pending_endpoint, Some(requested_endpoint.clone()));
    assert_eq!(
        committed_endpoint,
        local("A"),
        "B remains uncommitted until its list succeeds"
    );

    let b_directories = completed_transfer_refresh_directories(&a_completed, &b_completed);
    assert_eq!(
        b_directories,
        vec![TransferRefreshScope::Directory(local("B"))]
    );
    assert!(panel_needs_transfer_refresh(
        &committed_endpoint,
        pending_endpoint.as_ref(),
        loading,
        true,
        &b_directories,
    ));
    let refresh_target =
        panel_refresh_target(&committed_endpoint, pending_endpoint.as_ref(), loading)
            .expect("refresh_panel must preserve the in-flight B navigation")
            .clone();
    assert_eq!(refresh_target, requested_endpoint);

    // transfer completion calls refresh_panel -> load_panel_at, so the
    // refreshed request receives a new generation but keeps the same requested endpoint.
    assert_eq!(
        begin_panel_load_request(
            &mut generation,
            &mut loading,
            &mut pending_endpoint,
            &refresh_target,
        ),
        6,
    );
    assert_eq!(pending_endpoint, Some(refresh_target));
    assert_eq!(committed_endpoint, local("A"));
}

/// 同一队列快照合并多个成功回执时只刷新当前 requested endpoint，重复快照不重复刷新。
///
/// @author kongweiguang
#[test]
fn merged_successes_refresh_requested_endpoint_once() {
    let a_id = TransferId::new();
    let b_id = TransferId::new();
    let previous = vec![
        summary(
            a_id.clone(),
            TransferStatus::Transferring,
            TransferKind::File,
            local("A/file.txt"),
            None,
            0,
        ),
        summary(
            b_id.clone(),
            TransferStatus::Transferring,
            TransferKind::File,
            local("B/file.txt"),
            None,
            0,
        ),
    ];
    let completed = vec![
        summary(
            a_id,
            TransferStatus::Succeeded,
            TransferKind::File,
            local("A/file.txt"),
            Some(local("A/file.txt")),
            1,
        ),
        summary(
            b_id,
            TransferStatus::Succeeded,
            TransferKind::File,
            local("B/file.txt"),
            Some(local("B/file.txt")),
            1,
        ),
    ];
    let committed_endpoint = local("A");
    let requested_endpoint = local("B");
    let mut pending_endpoint = None;
    let mut generation = 12;
    let mut loading = false;
    begin_panel_load_request(
        &mut generation,
        &mut loading,
        &mut pending_endpoint,
        &requested_endpoint,
    );

    let merged_directories = completed_transfer_refresh_directories(&previous, &completed);
    assert_eq!(
        merged_directories,
        vec![
            TransferRefreshScope::Directory(local("A")),
            TransferRefreshScope::Directory(local("B")),
        ],
    );
    assert!(panel_needs_transfer_refresh(
        &committed_endpoint,
        pending_endpoint.as_ref(),
        loading,
        true,
        &merged_directories,
    ));
    let target = panel_refresh_target(&committed_endpoint, pending_endpoint.as_ref(), loading)
        .expect("merged completions refresh the requested endpoint only")
        .clone();
    assert_eq!(target, requested_endpoint);
    assert_eq!(
        begin_panel_load_request(
            &mut generation,
            &mut loading,
            &mut pending_endpoint,
            &target,
        ),
        14,
    );

    let repeated_directories = completed_transfer_refresh_directories(&completed, &completed);
    assert!(repeated_directories.is_empty());
    assert!(!panel_needs_transfer_refresh(
        &committed_endpoint,
        pending_endpoint.as_ref(),
        loading,
        true,
        &repeated_directories,
    ));
    assert_eq!(pending_endpoint, Some(target));
}

/// 刷新同一 B endpoint 后旧代次回执仍不得清 pending；同代次错误 endpoint 也不能结束新请求。
///
/// @author kongweiguang
#[test]
fn stale_load_result_cannot_clear_new_pending_endpoint() {
    let a = local("A");
    let b = local("B");
    let mut pending = None;
    let mut generation = 0;
    let mut loading = false;
    assert_eq!(
        begin_panel_load_request(&mut generation, &mut loading, &mut pending, &a),
        1
    );
    assert_eq!(
        begin_panel_load_request(&mut generation, &mut loading, &mut pending, &b),
        2
    );
    assert_eq!(
        begin_panel_load_request(&mut generation, &mut loading, &mut pending, &b),
        3
    );

    assert!(!finish_panel_load_request(3, 1, &mut pending, &a));
    assert_eq!(pending, Some(b.clone()));
    assert!(!finish_panel_load_request(3, 2, &mut pending, &b));
    assert_eq!(pending, Some(b.clone()));
    assert!(!finish_panel_load_request(3, 3, &mut pending, &a));
    assert_eq!(pending, Some(b.clone()));

    assert!(finish_panel_load_request(3, 3, &mut pending, &b));
    assert!(pending.is_none());
}

/// 失败中的明确提交回执只触发只读刷新，不改变提交未知状态或授权重试。
///
/// @author kongweiguang
#[test]
fn confirmed_terminal_receipts_refresh_without_reinterpreting_outcome() {
    let file = summary(
        TransferId::new(),
        TransferStatus::Succeeded,
        TransferKind::File,
        local("out/name.txt"),
        Some(local("out/name (1).txt")),
        1,
    );
    let mut unknown = summary(
        TransferId::new(),
        TransferStatus::Failed,
        TransferKind::File,
        local("uncertain/unknown"),
        Some(local("uncertain/unknown")),
        1,
    );
    unknown.failure_kind = Some(TransferFailureKind::CommitUnknown);
    let empty_success = summary(
        TransferId::new(),
        TransferStatus::Succeeded,
        TransferKind::Directory,
        local("out/no-receipt"),
        Some(local("out/no-receipt")),
        0,
    );
    let unchanged = summary(
        TransferId::new(),
        TransferStatus::Unchanged,
        TransferKind::Directory,
        local("out/unchanged"),
        Some(local("out/unchanged")),
        0,
    );

    let cancelled = summary(
        TransferId::new(),
        TransferStatus::Cancelled,
        TransferKind::Directory,
        local("partial/tree"),
        Some(local("partial/tree")),
        2,
    );
    let current = vec![file, unknown, cancelled, empty_success, unchanged];
    assert_eq!(
        completed_transfer_refresh_directories(&[], &current),
        vec![
            TransferRefreshScope::Directory(local("out")),
            TransferRefreshScope::Directory(local("uncertain")),
            TransferRefreshScope::Directory(local("partial")),
            TransferRefreshScope::Tree(local("partial/tree")),
        ],
    );
    assert_eq!(current[1].status, TransferStatus::Failed);
    assert_eq!(
        current[1].failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
    assert!(!current[1].retryable);
    assert_eq!(current[2].status, TransferStatus::Cancelled);
    assert!(completed_transfer_refresh_directories(&current, &current).is_empty());
}

/// 队列尚无明确实际目标或提交回执时不能凭请求路径猜测刷新位置。
///
/// @author kongweiguang
#[test]
fn refresh_requires_terminal_status_actual_destination_and_receipt() {
    let id = TransferId::new();
    let requested = local("out/tree");
    let cases = [
        summary(
            id.clone(),
            TransferStatus::Transferring,
            TransferKind::Directory,
            requested.clone(),
            Some(requested.clone()),
            1,
        ),
        summary(
            id.clone(),
            TransferStatus::Failed,
            TransferKind::Directory,
            requested.clone(),
            None,
            1,
        ),
        summary(
            id.clone(),
            TransferStatus::Cancelled,
            TransferKind::Directory,
            requested.clone(),
            Some(requested.clone()),
            0,
        ),
        summary(
            id.clone(),
            TransferStatus::Skipped,
            TransferKind::Directory,
            requested.clone(),
            Some(requested.clone()),
            1,
        ),
        summary(
            id.clone(),
            TransferStatus::Unchanged,
            TransferKind::Directory,
            requested.clone(),
            Some(requested.clone()),
            1,
        ),
    ];
    for case in &cases {
        assert!(completed_transfer_refresh_directories(&[], std::slice::from_ref(case)).is_empty());
    }
    let no_receipt = summary(
        id.clone(),
        TransferStatus::Failed,
        TransferKind::Directory,
        requested.clone(),
        Some(requested.clone()),
        0,
    );
    let with_receipt = summary(
        id,
        TransferStatus::Failed,
        TransferKind::Directory,
        requested.clone(),
        Some(requested),
        1,
    );
    assert_eq!(
        completed_transfer_refresh_directories(&[no_receipt], &[with_receipt]),
        vec![
            TransferRefreshScope::Directory(local("out")),
            TransferRefreshScope::Tree(local("out/tree"))
        ],
    );
}

/// 同名远端路径必须绑定 HostId 和 POSIX 组件边界，目录后代按当前浏览目标回读。
///
/// @author kongweiguang
#[test]
fn directory_scope_refreshes_only_matching_host_and_descendants() {
    let host = HostId::new();
    let other_host = HostId::new();
    let destination = remote(&host, "/data/tree");
    let finished = summary(
        TransferId::new(),
        TransferStatus::Failed,
        TransferKind::Directory,
        destination.clone(),
        Some(destination.clone()),
        1,
    );
    let scopes = completed_transfer_refresh_directories(&[], &[finished]);
    for endpoint in [
        remote(&host, "/data"),
        destination,
        remote(&host, "/data/tree/nested/deep"),
    ] {
        assert!(panel_needs_transfer_refresh(
            &endpoint, None, false, true, &scopes
        ));
    }
    for endpoint in [
        remote(&other_host, "/data"),
        remote(&other_host, "/data/tree/nested"),
        remote(&host, "/data/tree-other"),
        remote(&host, "/data/tree/../outside"),
        local("/data/tree/nested"),
    ] {
        assert!(!panel_needs_transfer_refresh(
            &endpoint, None, false, true, &scopes
        ));
    }
    assert!(!panel_needs_transfer_refresh(
        &remote(&host, "/data/tree"),
        None,
        false,
        false,
        &scopes
    ));
    assert!(!panel_needs_transfer_refresh(
        &remote(&host, "/data/tree/nested"),
        Some(&remote(&other_host, "/data/tree/nested")),
        true,
        true,
        &scopes,
    ));
}

/// 本机路径使用组件而非字符串前缀，并拒绝通过父目录段误判为目标树后代。
///
/// @author kongweiguang
#[test]
fn local_directory_scope_matches_component_boundaries() {
    let scopes = vec![TransferRefreshScope::Tree(local("out/tree"))];
    for endpoint in [local("out/tree"), local("out/tree/nested")] {
        assert!(panel_needs_transfer_refresh(
            &endpoint, None, false, true, &scopes
        ));
    }
    for endpoint in [local("out/tree-other"), local("out/tree/../outside")] {
        assert!(!panel_needs_transfer_refresh(
            &endpoint, None, false, true, &scopes
        ));
    }
    let relative_root = vec![TransferRefreshScope::Tree(local("."))];
    assert!(panel_needs_transfer_refresh(
        &local("nested/child"),
        None,
        false,
        true,
        &relative_root
    ));
    assert!(!panel_needs_transfer_refresh(
        &local("../outside"),
        None,
        false,
        true,
        &relative_root
    ));
}

/// 文件只影响实际目标父目录，不能因相近名称或子目录而触发无关读取。
///
/// @author kongweiguang
#[test]
fn file_scope_does_not_refresh_child_directories() {
    let finished = summary(
        TransferId::new(),
        TransferStatus::Succeeded,
        TransferKind::File,
        local("out/name"),
        Some(local("out/name (1)")),
        1,
    );
    let scopes = completed_transfer_refresh_directories(&[], &[finished]);
    assert!(panel_needs_transfer_refresh(
        &local("out"),
        None,
        false,
        true,
        &scopes
    ));
    assert!(!panel_needs_transfer_refresh(
        &local("out/nested"),
        None,
        false,
        true,
        &scopes
    ));
}

/// 清理或丢帧后无法确定影响路径，已缓存和在途目录都必须重新读取。
///
/// @author kongweiguang
#[test]
fn uncertain_queue_change_refreshes_cached_and_in_flight_directories() {
    let scopes = [TransferRefreshScope::All];
    let current = local("current");
    let requested = local("requested");
    assert!(panel_needs_transfer_refresh(
        &current, None, false, true, &scopes
    ));
    assert!(panel_needs_transfer_refresh(
        &current,
        Some(&requested),
        true,
        false,
        &scopes,
    ));
    assert!(!panel_needs_transfer_refresh(
        &current, None, false, false, &scopes
    ));
}

mod subscription_regressions {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::time::Duration;

    use async_trait::async_trait;
    use gpui_kit::TestAppContext;
    use kerminal_application::transfers::{
        TransferBackend, TransferExecutionContext, TransferExecutionError, TransferExecutionResult,
        TransferPhase, TransferProgressUpdate, TransferQueueEvent, TransferQueueOptions,
        TransferService,
    };
    use kerminal_domain::files::{DirectoryListing, TransferRequest, TransferStatus};
    use tokio::{runtime::Runtime, sync::Notify};
    use tokio_util::sync::CancellationToken;

    use super::super::action_test_support as support;
    use super::*;

    /// 后端使用可靠提交屏障；可选的逐帧握手确保进度确实进入生产广播而非被 watch 合并。
    struct TestTransferBackend {
        flood: bool,
        observed_events: Arc<AtomicUsize>,
        verification_finished: AtomicBool,
        resume_commit: Notify,
    }

    #[async_trait]
    impl TransferBackend for TestTransferBackend {
        /// 测试只使用隔离端点，所有请求都由该唯一内存后端处理。
        ///
        /// @author kongweiguang
        fn supports(&self, _: &TransferRequest) -> bool {
            true
        }

        /// 先逐帧确认校验唤醒，再等待测试释放真实提交，便于观察读目录次数。
        ///
        /// @author kongweiguang
        async fn execute(
            &self,
            context: TransferExecutionContext,
        ) -> Result<TransferExecutionResult, TransferExecutionError> {
            let destination = context.request.destination.clone();
            context
                .progress
                .begin_transfer(Some(1))
                .await
                .expect("begin transfer");
            if self.flood {
                for _ in 0..4_387 {
                    let before = self.observed_events.load(Ordering::Acquire);
                    context.progress.report(TransferProgressUpdate {
                        confirmed_bytes: 0,
                        total_bytes: Some(1),
                        phase: TransferPhase::Verifying,
                        actual_destination: Some(destination.clone()),
                        checkpoint: None,
                        activity_sequence: 0,
                    });
                    while self.observed_events.load(Ordering::Acquire) == before {
                        tokio::task::yield_now().await;
                    }
                }
                self.verification_finished.store(true, Ordering::Release);
                self.resume_commit.notified().await;
            }
            context
                .progress
                .begin_commit(destination.clone(), 1, Some(1), None)
                .await
                .expect("commit permit");
            context
                .progress
                .record_committed_entry(1)
                .await
                .expect("commit receipt");
            context
                .progress
                .finish_commit()
                .await
                .expect("final commit barrier");
            Ok(TransferExecutionResult {
                actual_destination: destination,
                total_bytes: Some(1),
                skipped: false,
                unchanged: false,
            })
        }
    }

    /// 让 GPUI 和其独立 Tokio runtime 交替推进，直到真实文件端口看到指定回读次数。
    ///
    /// @author kongweiguang
    fn pump_until_reads(
        app: &mut TestAppContext,
        runtime: &Runtime,
        reads: &AtomicUsize,
        expected: usize,
    ) {
        for _ in 0..4_000 {
            app.run_until_parked();
            runtime.block_on(tokio::task::yield_now());
            if reads.load(Ordering::Acquire) >= expected {
                break;
            }
        }
        assert_eq!(reads.load(Ordering::Acquire), expected);
    }

    /// 活 worker 的退出与终态发布分离，等待完整收尾后才允许测试历史容量裁剪。
    ///
    /// @author kongweiguang
    async fn wait_settled(service: &TransferService, id: &TransferId) {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if service
                    .get_settled(id)
                    .await
                    .expect("task exists")
                    .is_some()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
            for _ in 0..20 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("transfer settles");
    }

    /// 多千次已确认广播经过 FilesPane 订阅时不读取目录，随后真正提交只读取一次。
    ///
    /// @author kongweiguang
    #[gpui_kit::test]
    fn transfer_verification_lag_does_not_read_directory(app: &mut TestAppContext) {
        let host = HostId::new();
        let directory = remote(&host, "/uploads");
        let destination = remote(&host, "/uploads/file.bin");
        let fixture = support::entity_fixture(app, directory.clone(), |pane| {
            support::seed_listing(pane, 1, 1, Vec::new());
        });
        *fixture
            .mutations
            .directory_listing
            .lock()
            .expect("listing mutex") = Some(DirectoryListing {
            directory: directory.clone(),
            entries: Vec::new(),
        });
        let observed = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(TestTransferBackend {
            flood: true,
            observed_events: observed.clone(),
            verification_finished: AtomicBool::new(false),
            resume_commit: Notify::new(),
        });
        let service = TransferService::new(
            vec![backend.clone()],
            fixture.runtime.handle().clone(),
            TransferQueueOptions::default(),
        )
        .expect("test queue");
        let mut monitor = service.subscribe();
        let monitor_cancel = CancellationToken::new();
        let monitor_token = monitor_cancel.clone();
        let monitor_task = fixture.runtime.spawn(async move {
            while monitor.recv_event(monitor_token.clone()).await.is_some() {
                observed.fetch_add(1, Ordering::AcqRel);
            }
        });
        app.update(|cx| {
            fixture.pane.update(cx, |pane, cx| {
                pane.transfers = service.clone();
                pane.watch_transfer_queue(cx);
            })
        });
        app.run_until_parked();
        let request = TransferRequest {
            source: local("source/input"),
            destination,
            kind: TransferKind::File,
            conflict_policy: ConflictPolicy::Rename,
            idle_timeout_seconds: None,
        };
        let accepted = fixture
            .runtime
            .block_on(service.enqueue(request))
            .expect("enqueue");
        fixture.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(20), async {
                while !backend.verification_finished.load(Ordering::Acquire) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("all verification progress reaches the queue");
        });
        for _ in 0..600 {
            app.run_until_parked();
            fixture.runtime.block_on(tokio::task::yield_now());
        }
        assert_eq!(fixture.mutations.list_directory.load(Ordering::Acquire), 0);

        backend.resume_commit.notify_one();
        fixture
            .runtime
            .block_on(wait_settled(&service, &accepted.id));
        pump_until_reads(app, &fixture.runtime, &fixture.mutations.list_directory, 1);
        for _ in 0..100 {
            app.run_until_parked();
            fixture.runtime.block_on(tokio::task::yield_now());
        }
        assert_eq!(fixture.mutations.list_directory.load(Ordering::Acquire), 1);
        monitor_cancel.cancel();
        fixture
            .runtime
            .block_on(monitor_task)
            .expect("monitor exits");
        fixture
            .runtime
            .block_on(service.shutdown())
            .expect("queue shutdown");
    }

    /// 容量裁剪与新提交在同一快照被观察时，只发起一次保守目录回读。
    ///
    /// @author kongweiguang
    #[gpui_kit::test]
    fn transfer_pruning_and_new_commit_share_one_directory_read(app: &mut TestAppContext) {
        let host = HostId::new();
        let directory = remote(&host, "/uploads");
        let fixture = support::entity_fixture(app, directory.clone(), |pane| {
            support::seed_listing(pane, 1, 1, Vec::new());
        });
        *fixture
            .mutations
            .directory_listing
            .lock()
            .expect("listing mutex") = Some(DirectoryListing {
            directory,
            entries: Vec::new(),
        });
        let backend = Arc::new(TestTransferBackend {
            flood: false,
            observed_events: Arc::new(AtomicUsize::new(0)),
            verification_finished: AtomicBool::new(false),
            resume_commit: Notify::new(),
        });
        let service = TransferService::new(
            vec![backend],
            fixture.runtime.handle().clone(),
            TransferQueueOptions {
                max_history: 1,
                ..TransferQueueOptions::default()
            },
        )
        .expect("bounded test queue");
        app.update(|cx| {
            fixture.pane.update(cx, |pane, cx| {
                pane.transfers = service.clone();
                pane.watch_transfer_queue(cx);
            })
        });
        let mut pruning_monitor = service.subscribe();
        let mut task_ids = Vec::new();
        for name in ["first.bin", "second.bin", "third.bin"] {
            let request = TransferRequest {
                source: local("source/input"),
                destination: remote(&host, &format!("/uploads/{name}")),
                kind: TransferKind::File,
                conflict_policy: ConflictPolicy::Rename,
                idle_timeout_seconds: None,
            };
            let accepted = fixture
                .runtime
                .block_on(service.enqueue(request))
                .expect("enqueue");
            fixture
                .runtime
                .block_on(wait_settled(&service, &accepted.id));
            task_ids.push(accepted.id);
        }
        assert_eq!(
            fixture
                .runtime
                .block_on(pruning_monitor.recv_event(CancellationToken::new())),
            Some(TransferQueueEvent::Resync),
            "the subscription observed the history removal epoch"
        );
        let rows = fixture.runtime.block_on(service.list());
        assert!(
            (1..=2).contains(&rows.len()),
            "settling workers may leave one extra terminal record"
        );
        assert!(
            rows.iter().all(|row| row.id != task_ids[0]),
            "the oldest task was pruned"
        );
        let latest = rows
            .iter()
            .find(|row| row.id == task_ids[2])
            .expect("the latest task remains");
        assert_eq!(latest.status, TransferStatus::Succeeded);
        pump_until_reads(app, &fixture.runtime, &fixture.mutations.list_directory, 1);
        for _ in 0..100 {
            app.run_until_parked();
            fixture.runtime.block_on(tokio::task::yield_now());
        }
        assert_eq!(fixture.mutations.list_directory.load(Ordering::Acquire), 1);
        fixture
            .runtime
            .block_on(service.shutdown())
            .expect("queue shutdown");
    }
}
