//! 冲突预检计数只处理已读取目标快照，不接触真实文件系统。
//!
//! @author kongweiguang

use std::{collections::HashSet, path::PathBuf};

use gpui_kit::TestAppContext;
use kerminal_domain::identity::HostId;

use super::{action_test_support as support, *};

/// 完整受理已在队列可见，包括目录覆盖后代；不能再插入挤动浏览区域的成功消息。
///
/// @author kongweiguang
#[test]
fn accepted_batch_is_quiet_including_covered_descendants() {
    for included in [0, 2] {
        assert!(
            batch_enqueue_message(
                "上传任务已入队",
                "Upload tasks queued",
                BatchEnqueueOutcome {
                    total: 1 + included,
                    planned_tasks: 1,
                    queued: 1,
                    included_by_selected_directories: included,
                    ..Default::default()
                },
            )
            .is_none()
        );
    }
}

/// 精简成功反馈不能吞掉未受理和部分取消，否则用户会误以为整批已提交。
///
/// @author kongweiguang
#[test]
fn incomplete_batch_retains_actionable_feedback() {
    for outcome in [
        BatchEnqueueOutcome {
            total: 2,
            planned_tasks: 2,
            queued: 1,
            issues: vec!["blocked.txt: permission denied".to_string()],
            ..Default::default()
        },
        BatchEnqueueOutcome {
            total: 2,
            planned_tasks: 2,
            queued: 1,
            cancelled_remaining: 1,
            ..Default::default()
        },
    ] {
        assert!(batch_enqueue_message("上传任务已入队", "Upload tasks queued", outcome).is_some());
    }
}

/// 重复提交和主动取消都必须保持原错误与可重试的批次状态，不制造短暂提示条。
///
/// @author kongweiguang
#[gpui_kit::test]
fn quiet_submission_and_cancel_preserve_existing_failure(app: &mut TestAppContext) {
    let host = HostId::new();
    let directory = support::remote_endpoint(&host, "/srv/target");
    let fixture = support::entity_fixture(app, directory, |pane| {
        pane.status = Some("Existing failure".to_string());
    });
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            assert!(pane.begin_transfer_submission(cx));
            assert!(!pane.begin_transfer_submission(cx));
            pane.transfer_selected(cx);
            assert_eq!(pane.status.as_deref(), Some("Existing failure"));
            pane.finish_cancelled_transfer(cx);
            assert!(!pane.transfer_submission_busy);
            assert!(pane.pending_transfer_conflict.is_none());
            assert_eq!(pane.status.as_deref(), Some("Existing failure"));
            assert!(pane.begin_transfer_submission(cx));
        });
    });
    let _runtime = fixture.runtime;
}

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

/// 旧冲突夹具同时安装真实提交身份，使确认回归与生产生命周期保持一致。
///
/// @author kongweiguang
fn seed_submission(pane: &mut FilesPane) {
    pane.transfer_submission_busy = true;
    pane.transfer_submission_generation = 1;
    pane.transfer_submission = Some(TransferSubmission {
        generation: 1,
        cancellation: CancellationToken::new(),
        related_sides: [true, true],
        phase: TransferSubmissionPhase::Preparing,
    });
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
        seed_submission(pane);
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

/// 关闭冲突决策静默丢弃未提交批次并释放 busy；下次传输仍使用原队列默认策略。
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
        seed_submission(pane);
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
            assert!(pane.status.is_none());
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
        seed_submission(pane);
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

/// 相关目录改变立即撤销准备，另一侧或已经开始受理的批次不受影响。
///
/// @author kongweiguang
#[gpui_kit::test]
fn preparation_cancellation_is_side_scoped_and_stops_at_enqueue(app: &mut TestAppContext) {
    let endpoint = support::remote_endpoint(&HostId::new(), "/srv");
    let fixture = support::entity_fixture(app, endpoint, |_| {});
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            let (old_generation, old_token) = pane
                .begin_transfer_preparation([false, true], cx)
                .expect("prepare upload");
            pane.cancel_transfer_preparation_for_side(0, cx);
            assert!(pane.transfer_preparation_is_current(old_generation));
            assert!(!old_token.is_cancelled());
            pane.cancel_transfer_preparation_for_side(1, cx);
            assert!(old_token.is_cancelled());
            assert!(!pane.transfer_submission_busy);
            assert!(pane.status.is_some());

            let (new_generation, new_token) = pane
                .begin_transfer_preparation([true, true], cx)
                .expect("prepare copy");
            assert!(!pane.transfer_preparation_is_current(old_generation));
            assert!(pane.transfer_preparation_is_current(new_generation));
            pane.transfer_submission.as_mut().expect("submission").phase =
                TransferSubmissionPhase::Enqueuing;
            pane.cancel_transfer_preparation_for_side(0, cx);
            pane.cancel_transfer_preparation(cx);
            assert!(pane.transfer_enqueue_in_progress());
            assert!(!new_token.is_cancelled());
            pane.end_transfer_submission();
            assert!(!pane.transfer_enqueue_in_progress());
        });
    });
    let _runtime = fixture.runtime;
}

/// 实际主机标签切换只影响关联侧，关闭右侧最后标签会取消准备并保留原因。
///
/// @author kongweiguang
#[gpui_kit::test]
fn host_tab_changes_cancel_only_the_affected_preparation(app: &mut TestAppContext) {
    let endpoint = support::remote_endpoint(&HostId::new(), "/srv");
    let fixture = support::entity_fixture(app, endpoint, |_| {});
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.set_host_picker_enabled(true, cx);
            let right = pane.host_picker.host_tabs.active[1].expect("right host tab");
            let (generation, token) = pane
                .begin_transfer_preparation([false, true], cx)
                .expect("prepare upload");
            pane.add_host_tab(
                FilePaneSide::Left,
                support::remote_endpoint(&HostId::new(), "/other"),
                cx,
            );
            assert!(pane.transfer_preparation_is_current(generation));
            assert!(!token.is_cancelled());
            pane.close_host_tab(FilePaneSide::Right, right, cx);
            assert!(token.is_cancelled());
            assert!(!pane.transfer_submission_busy);
            assert!(pane.status.is_some());
            assert!(pane.host_picker.target_unselected[1]);
        });
    });
    let _runtime = fixture.runtime;
}

/// 同一目录在左右同时打开时，右侧目录行上传必须绑定右侧，左侧导航不能误取消。
///
/// @author kongweiguang
#[gpui_kit::test]
fn same_directory_upload_keeps_the_active_side_and_validates_row(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv");
    let child = support::remote_entry(&directory, "nested", FileEntryKind::Directory);
    let deep = support::remote_entry(&child.endpoint, "deep", FileEntryKind::Directory);
    let child_for_seed = child.clone();
    let deep_for_seed = deep.clone();
    let directory_for_seed = directory.clone();
    let fixture = support::entity_fixture(app, directory.clone(), move |pane| {
        pane.panels[0].endpoint = directory_for_seed.clone();
        support::seed_listing(pane, 0, 3, vec![child_for_seed.clone()]);
        support::seed_listing(pane, 1, 7, vec![child_for_seed.clone()]);
        let tree = &mut pane.panels[1].tree;
        tree.begin_root_request(&directory_for_seed, 7);
        tree.set_root_listing(directory_for_seed, 7, vec![child_for_seed.clone()], true);
        tree.expand(&child_for_seed.endpoint);
        let (request_id, _) = tree
            .begin_directory_request(&child_for_seed.endpoint, 7)
            .expect("load nested tree directory");
        assert!(tree.complete_directory_request(
            &child_for_seed.endpoint,
            7,
            request_id,
            true,
            Ok(DirectoryListing {
                directory: child_for_seed.endpoint.clone(),
                entries: vec![deep_for_seed],
            }),
        ));
        pane.active_side = 1;
    });
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            assert_eq!(
                pane.capture_transfer_target_for_directory(&directory)
                    .expect("current directory")
                    .side,
                1
            );
            assert_eq!(
                pane.capture_transfer_target_for_directory(&child.endpoint)
                    .expect("current directory row")
                    .side,
                1
            );
            let unrelated = support::remote_endpoint(&HostId::new(), "/unrelated");
            assert!(
                pane.capture_transfer_target_for_directory(&unrelated)
                    .is_none()
            );
            pane.view_mode = view_mode::FilesViewMode::Tree;
            assert_eq!(
                pane.capture_transfer_target_for_directory(&deep.endpoint)
                    .expect("visible nested tree directory")
                    .side,
                1
            );
            pane.panels[1].tree.collapse(&child.endpoint);
            assert!(
                pane.capture_transfer_target_for_directory(&deep.endpoint)
                    .is_none()
            );
            pane.prompt_upload_files_to(child.endpoint.clone(), cx);
            let submission = pane.transfer_submission.as_ref().expect("upload started");
            assert_eq!(submission.related_sides, [false, true]);
            let token = submission.cancellation.clone();
            pane.cancel_transfer_preparation_for_side(0, cx);
            assert!(pane.transfer_submission_busy);
            assert!(!token.is_cancelled());
            pane.cancel_transfer_preparation_for_side(1, cx);
            assert!(!pane.transfer_submission_busy);
            assert!(token.is_cancelled());
        });
    });
    assert!(app.did_prompt_for_paths());
    app.simulate_path_prompt_response(|_| None);
    for _ in 0..8 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
    }
}

/// 已返回的旧系统选择器响应在导航后不能提交，也不能释放下一次选择器的忙态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn late_upload_picker_cannot_release_new_submission(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/target");
    let fixture = support::entity_fixture(app, directory.clone(), |pane| {
        support::seed_listing(pane, 1, 7, Vec::new());
        pane.active_side = 1;
    });
    app.update(|cx| {
        fixture
            .pane
            .update(cx, |pane, cx| pane.prompt_upload_files(cx));
    });
    assert!(app.did_prompt_for_paths());
    app.run_until_parked();
    fixture.runtime.block_on(tokio::task::yield_now());
    app.simulate_path_prompt_response(|_| Some(vec![PathBuf::from("late.txt")]));
    fixture.runtime.block_on(tokio::task::yield_now());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.cancel_transfer_preparation_for_side(1, cx);
            pane.status = Some("new submission status".to_owned());
            pane.prompt_upload_files(cx);
        });
    });
    for _ in 0..8 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
    }
    app.update(|cx| {
        fixture.pane.read_with(cx, |pane, _| {
            assert!(pane.transfer_submission_busy);
            assert_eq!(pane.status.as_deref(), Some("new submission status"));
            assert!(pane.pending_transfer_conflict.is_none());
            assert!(pane.transfer_rows.is_empty());
        });
    });
    app.simulate_path_prompt_response(|_| None);
    for _ in 0..8 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
    }
    app.update(|cx| {
        fixture.pane.read_with(cx, |pane, _| {
            assert!(!pane.transfer_submission_busy);
            assert!(pane.transfer_rows.is_empty());
        });
    });
}

/// 冲突预检的旧错误回执不能关闭更新后批次或覆盖后者的状态。
///
/// @author kongweiguang
#[gpui_kit::test]
fn late_conflict_preflight_error_cannot_release_new_submission(app: &mut TestAppContext) {
    let directory = support::remote_endpoint(&HostId::new(), "/srv/target");
    let fixture = support::entity_fixture(app, directory.clone(), |pane| {
        support::seed_listing(pane, 1, 7, Vec::new());
    });
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.enqueue_transfer_batch(pending_conflict(directory, 7).plan, cx);
        });
    });
    app.run_until_parked();
    fixture.runtime.block_on(tokio::task::yield_now());
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            pane.cancel_transfer_preparation_for_side(0, cx);
            pane.status = Some("new submission status".to_owned());
            assert!(pane.begin_transfer_submission(cx));
        });
    });
    for _ in 0..8 {
        app.run_until_parked();
        fixture.runtime.block_on(tokio::task::yield_now());
    }
    app.update(|cx| {
        fixture.pane.update(cx, |pane, cx| {
            assert!(pane.transfer_submission_busy);
            assert_eq!(pane.status.as_deref(), Some("new submission status"));
            assert!(pane.pending_transfer_conflict.is_none());
            assert!(pane.transfer_rows.is_empty());
            pane.finish_cancelled_transfer(cx);
        });
    });
}
