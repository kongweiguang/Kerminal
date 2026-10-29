//! 冲突预检计数只处理已读取目标快照，不接触真实文件系统。
//!
//! @author kongweiguang

use std::{collections::HashSet, path::PathBuf};

use gpui_kit::TestAppContext;
use kerminal_domain::identity::HostId;

use super::{action_test_support as support, *};

/// 让状态测试共享同一冻结目标和单项请求，避免因测试夹具差异掩盖代次门禁。
///
/// @author kongweiguang
fn pending_conflict(endpoint: FileEndpoint, generation: u64) -> PendingTransferConflict {
    let source_host = HostId::new();
    let source = support::remote_endpoint(&source_host, "/source/report.txt");
    let destination =
        child_endpoint(&endpoint, "report.txt").expect("test destination supports a child file");

    PendingTransferConflict {
        plan: TransferBatchPlan {
            items: vec![TransferEnqueueItem {
                name: "report.txt".to_owned(),
                request: TransferRequest {
                    source,
                    destination,
                    kind: TransferKind::File,
                    conflict_policy: ConflictPolicy::Rename,
                    idle_timeout_seconds: None,
                },
                external_target_lease: None,
            }],
            total: 1,
            included_by_selected_directories: 0,
            issues: Vec::new(),
            labels: BatchEnqueueLabels {
                action_zh: "测试传输已入队",
                action_en: "Test transfer queued",
            },
            target: TransferTargetSnapshot {
                side: 1,
                endpoint,
                generation,
            },
        },
        conflict_count: 1,
    }
}

/// 唯一的不存在目标不应触发决策弹框。
#[test]
fn absent_unique_target_is_not_a_conflict() {
    let target = FileEndpoint::Local {
        path: PathBuf::from("target/new-file.txt"),
    };

    assert_eq!(
        count_existing_transfer_conflicts(&[target], &HashSet::new()),
        0
    );
}

/// 已存在目标的每个请求都冲突；不存在目标的同名后续请求也必须先进入策略选择。
#[test]
fn counts_existing_and_repeated_targets() {
    let existing_target = FileEndpoint::Local {
        path: PathBuf::from("target/existing.txt"),
    };
    let repeated_target = FileEndpoint::Local {
        path: PathBuf::from("target/new.txt"),
    };
    let existing = HashSet::from([existing_target.clone()]);

    assert_eq!(
        count_existing_transfer_conflicts(
            &[
                existing_target.clone(),
                existing_target,
                repeated_target.clone(),
                repeated_target,
            ],
            &existing,
        ),
        3
    );
}

/// 目录代次变化后确认旧计划必须失败关闭，不能把冻结请求投向更新后的浏览状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn stale_target_generation_rejects_conflict_resolution(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/target");
    let seeded_directory = directory.clone();
    let fixture = support::entity_fixture(app, directory, move |pane| {
        support::seed_listing(pane, 1, 7, Vec::new());
        pane.transfer_submission_busy = true;
        pane.pending_transfer_conflict = Some(pending_conflict(seeded_directory, 7));
    });
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, _| pane.panels[1].generation = 8);
    });
    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.resolve_transfer_conflict(ConflictPolicy::Overwrite, cx);
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert!(!pane.transfer_submission_busy);
            assert!(pane.pending_transfer_conflict.is_none());
            assert!(pane.transfer_rows.is_empty());
            assert_eq!(pane.queue.conflict_policy, ConflictPolicy::Rename);
            assert!(
                pane.status
                    .as_deref()
                    .is_some_and(|status| status.contains("目标目录已变化")
                        || status.contains("The destination changed"))
            );
        });
    });
    let _runtime = fixture.runtime;
}

/// 关闭冲突决策只丢弃未提交批次，并释放 busy；下次传输仍使用原队列默认策略。
///
/// @author kongweiguang
#[gpui_kit::test]
fn cancel_conflict_releases_busy_without_changing_default_policy(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/target");
    let seeded_directory = directory.clone();
    let fixture = support::entity_fixture(app, directory, move |pane| {
        support::seed_listing(pane, 1, 4, Vec::new());
        pane.queue.conflict_policy = ConflictPolicy::Overwrite;
        pane.transfer_submission_busy = true;
        pane.pending_transfer_conflict = Some(pending_conflict(seeded_directory, 4));
    });
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| pane.cancel_transfer_conflict(cx));
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert!(!pane.transfer_submission_busy);
            assert!(pane.pending_transfer_conflict.is_none());
            assert_eq!(pane.queue.conflict_policy, ConflictPolicy::Overwrite);
            assert!(
                pane.status
                    .as_deref()
                    .is_some_and(|status| status.contains("已取消冲突处理")
                        || status.contains("Conflict handling was canceled"))
            );
        });
    });
    let _runtime = fixture.runtime;
}

/// 选择策略用于待提交计划，不能悄悄改写队列设置中后续批次使用的默认策略。
///
/// @author kongweiguang
#[gpui_kit::test]
fn resolving_conflict_keeps_the_next_batch_default_policy(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/target");
    let seeded_directory = directory.clone();
    let fixture = support::entity_fixture(app, directory, move |pane| {
        support::seed_listing(pane, 1, 11, Vec::new());
        pane.queue.conflict_policy = ConflictPolicy::Rename;
        pane.transfer_submission_busy = true;
        pane.pending_transfer_conflict = Some(pending_conflict(seeded_directory, 11));
    });
    let pane = fixture.pane.clone();

    app.update(|cx| {
        pane.update(cx, |pane, cx| {
            pane.resolve_transfer_conflict(ConflictPolicy::Skip, cx);
        });
    });
    app.update(|cx| {
        pane.read_with(cx, |pane, _| {
            assert!(pane.pending_transfer_conflict.is_none());
            assert_eq!(pane.queue.conflict_policy, ConflictPolicy::Rename);
        });
    });
    let _runtime = fixture.runtime;
}
