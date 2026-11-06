// @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, FilePathError, RemotePath, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus, TransferTask, TransferTransitionError,
    },
    identity::{HostId, TransferId},
};

/// 构造隔离的本机路径样本，避免领域测试依赖真实用户目录。
fn local(path: &str) -> FileEndpoint {
    FileEndpoint::Local {
        path: PathBuf::from(path),
    }
}

/// 每个状态机测试使用全新传输聚合，避免测试间共享可变状态。
fn task() -> TransferTask {
    let request = TransferRequest {
        source: local("source.txt"),
        destination: local("destination.txt"),
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    TransferTask::queued(TransferId::new(), request).expect("test fixture uses distinct endpoints")
}

/// 验证远端路径父级只按 POSIX 分隔符解析，Windows 盘符语义不能改变 SFTP 导航。
#[test]
fn remote_parent_uses_posix_segments_on_windows() {
    assert_eq!(
        RemotePath::parse("/var/lib")
            .unwrap()
            .parent()
            .unwrap()
            .as_str(),
        "/var"
    );
    assert_eq!(
        RemotePath::parse("/var")
            .unwrap()
            .parent()
            .unwrap()
            .as_str(),
        "/"
    );
    assert_eq!(
        RemotePath::parse("relative/child")
            .unwrap()
            .parent()
            .unwrap()
            .as_str(),
        "relative"
    );
    assert_eq!(
        RemotePath::parse("relative-file")
            .unwrap()
            .parent()
            .unwrap()
            .as_str(),
        "."
    );
    assert_eq!(RemotePath::parse("/").unwrap().parent(), None);
}

/// 验证目录项名称不能借助点段或分隔符逃离当前目录。
#[test]
fn join_name_rejects_parent_segments_and_separators() {
    let root = RemotePath::parse("/home/user").unwrap();
    assert_eq!(root.join_name(".."), Err(FilePathError::InvalidName));
    assert_eq!(root.join_name("."), Err(FilePathError::InvalidName));
    assert_eq!(
        root.join_name("child/other"),
        Err(FilePathError::InvalidName)
    );
    assert_eq!(
        root.join_name("child").unwrap().as_str(),
        "/home/user/child"
    );
}

/// 同一名称在远端必须无损表示，在本机按平台规则处理，不能因宿主路径解释改变层级。
///
/// @author kongweiguang
#[test]
fn endpoint_child_names_preserve_posix_names_and_enforce_local_boundaries() {
    let remote = FileEndpoint::remote(HostId::new(), "/srv/data").unwrap();
    let child = remote.join_name(r"backup\2026.txt").unwrap();
    assert_eq!(
        child.remote_path().unwrap().as_str(),
        r"/srv/data/backup\2026.txt"
    );
    assert!(
        RemotePath::parse("/srv/data")
            .unwrap()
            .join_name(r"backup\2026.txt")
            .is_err()
    );
    for name in ["", ".", "..", "../escape", "/absolute", "line\nname"] {
        assert!(remote.join_name(name).is_err(), "remote accepted {name:?}");
        assert!(
            local("destination").join_name(name).is_err(),
            "local accepted {name:?}"
        );
    }
    assert_eq!(
        local("destination").join_name("safe.txt").unwrap(),
        local("destination/safe.txt")
    );
    if cfg!(windows) {
        for name in [
            r"backup\2026.txt",
            r"..\escape",
            "C:relative",
            "file:stream",
            "NUL.txt",
            "com1",
            "LPT¹.log",
            "trailing.",
            "trailing ",
        ] {
            assert!(
                local("destination").join_name(name).is_err(),
                "Windows accepted {name:?}"
            );
        }
    } else {
        assert_eq!(
            local("destination").join_name(r"backup\2026.txt").unwrap(),
            local(r"destination/backup\2026.txt")
        );
    }
}

/// 容器路径固定以容器根为基准，并拒绝折叠后仍越过根目录的输入。
#[test]
fn container_path_normalizes_separators_and_rejects_root_escape() {
    assert_eq!(
        RemotePath::parse_container(r"var\\lib\./app/../data")
            .unwrap()
            .as_str(),
        "/var/lib/data"
    );
    assert_eq!(RemotePath::parse_container("/").unwrap().as_str(), "/");
    assert_eq!(
        RemotePath::parse_container("../../etc/passwd"),
        Err(FilePathError::InvalidName)
    );
    assert_eq!(
        RemotePath::parse_container("a/../../escape"),
        Err(FilePathError::InvalidName)
    );
    assert_eq!(
        RemotePath::parse_container("a\0b"),
        Err(FilePathError::ControlCharacter)
    );
}

/// 验证遗漏本次 total 时仍沿用先前总量，避免进度帧绕过大小边界。
#[test]
fn progress_without_total_still_checks_previous_total() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(10)).unwrap();
    task.record_progress(8, None).unwrap();
    assert_eq!(
        task.record_progress(11, None),
        Err(TransferTransitionError::ProgressExceededTotal)
    );
}

/// 首次排队没有可信断点，直接取消后不得获得重试或续传资格。
///
/// @author kongweiguang
#[test]
fn initial_queued_cancellation_is_not_resumable() {
    let mut task = task();
    assert!(task.request_cancel());
    let summary = task.summary();
    assert_eq!(summary.status, TransferStatus::Cancelled);
    assert!(!summary.retryable && !summary.resumable);
    assert_eq!(
        task.resume_from_checkpoint(0, Some(100)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
}

/// 续传仍在排队时取消应保留断点；连接后则必须等待 worker 确认资格。
///
/// @author kongweiguang
#[test]
fn queued_resume_cancellation_preserves_checkpoint_for_next_resume() {
    let mut task = task();
    let original_id = task.id().clone();
    let destination = local("destination (1).txt");
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.select_actual_destination(destination.clone()).unwrap();
    task.record_progress(40, Some(100)).unwrap();
    assert!(task.request_cancel());
    task.confirm_cancelled(true).unwrap();

    for _ in 0..2 {
        task.resume_from_checkpoint(40, Some(100)).unwrap();
        let queued = task.summary();
        assert_eq!(queued.id, original_id);
        assert_eq!(queued.status, TransferStatus::Queued);
        assert_eq!(queued.actual_destination, Some(destination.clone()));
        assert_eq!(
            (queued.bytes_transferred, queued.total_bytes),
            (40, Some(100))
        );
        assert!(!queued.retryable && !queued.resumable);

        assert!(task.request_cancel());
        let cancelled = task.summary();
        assert_eq!(cancelled.id, original_id);
        assert_eq!(cancelled.status, TransferStatus::Cancelled);
        assert_eq!(cancelled.actual_destination, Some(destination.clone()));
        assert_eq!(
            (cancelled.bytes_transferred, cancelled.total_bytes),
            (40, Some(100))
        );
        assert!(cancelled.retryable && cancelled.resumable);
    }

    task.resume_from_checkpoint(40, Some(100)).unwrap();
    task.begin_connecting().unwrap();
    assert!(task.request_cancel());
    task.confirm_cancelled(false).unwrap();
    assert!(!task.summary().resumable);
}

/// 自动恢复回退到旧断点后，后端报告断点偏移不能被当成进度倒退。
///
/// @author kongweiguang
#[test]
fn automatic_recovery_restarts_progress_at_checkpoint() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.record_progress(40, Some(100)).unwrap();
    assert_eq!(
        task.begin_automatic_recovery_from_checkpoint(41, Some(100)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
    assert_eq!(
        task.begin_automatic_recovery_from_checkpoint(30, Some(99)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
    assert_eq!(task.status(), TransferStatus::Transferring);
    task.begin_automatic_recovery_from_checkpoint(30, Some(100))
        .unwrap();
    assert_eq!(task.status(), TransferStatus::WaitingForRecovery);
    assert_eq!(task.bytes_transferred(), 30);
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.record_progress(30, Some(100)).unwrap();
    task.record_progress(40, Some(100)).unwrap();
}

/// 取消收尾可推进可信断点，但必须保持确认进度单调且不能越过总量。
///
/// @author kongweiguang
#[test]
fn cancellation_checkpoint_progress_respects_state_and_bounds() {
    let mut task = task();
    assert_eq!(
        task.record_cancellation_checkpoint_progress(1, Some(100)),
        Err(TransferTransitionError::InvalidTransition)
    );
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.record_progress(40, Some(100)).unwrap();
    assert!(task.request_cancel());
    assert_eq!(
        task.record_cancellation_checkpoint_progress(39, Some(100)),
        Err(TransferTransitionError::ProgressRegressed)
    );
    assert_eq!(
        task.record_cancellation_checkpoint_progress(50, Some(99)),
        Err(TransferTransitionError::InvalidTransition)
    );
    assert_eq!(
        task.record_cancellation_checkpoint_progress(101, Some(100)),
        Err(TransferTransitionError::ProgressExceededTotal)
    );
    task.record_cancellation_checkpoint_progress(60, Some(100))
        .unwrap();
    task.confirm_cancelled(true).unwrap();
    let summary = task.summary();
    assert_eq!(summary.status, TransferStatus::Cancelled);
    assert_eq!(summary.bytes_transferred, 60);
    assert!(summary.resumable);
}

/// 从可信断点继续必须保留原身份与目标，并允许确认进度回退到最后持久化前缀。
///
/// @author kongweiguang
#[test]
fn resume_reuses_identity_and_resets_attempt_at_checkpoint() {
    let mut task = task();
    let original_id = task.id().clone();
    let destination = local("destination (1).txt");
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.select_actual_destination(destination.clone()).unwrap();
    task.record_progress(70, Some(100)).unwrap();
    task.begin_automatic_recovery(true).unwrap();
    assert!(task.request_cancel());
    task.confirm_cancelled(true).unwrap();

    task.resume_from_checkpoint(40, Some(100)).unwrap();
    let summary = task.summary();
    assert_eq!(summary.id, original_id);
    assert_eq!(summary.status, TransferStatus::Queued);
    assert_eq!(summary.actual_destination, Some(destination));
    assert_eq!(
        (summary.bytes_transferred, summary.total_bytes),
        (40, Some(100))
    );
    assert!(!summary.automatic_recovery_used);
    assert!(!summary.retryable && !summary.resumable);
    assert!(summary.failure_kind.is_none() && summary.safe_error.is_none());
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(100)).unwrap();
    task.record_progress(100, Some(100)).unwrap();
}

/// 暂停曾发生在无副作用提交期间时，新一轮必须清除旧提交协议的取消标志。
///
/// @author kongweiguang
#[test]
fn resume_clears_deferred_cancellation_before_next_commit() {
    let mut task = task();
    let destination = local("destination.txt");
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(10)).unwrap();
    task.record_progress(10, Some(10)).unwrap();
    task.begin_commit(destination.clone()).unwrap();
    assert!(task.request_cancel());
    task.confirm_commit_item_unchanged().unwrap();
    task.confirm_cancelled(true).unwrap();
    task.resume_from_checkpoint(10, Some(10)).unwrap();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(10)).unwrap();
    task.begin_commit(destination).unwrap();
}

/// 可信断点失败与暂停共用同一原任务迁移，并清除上一轮的错误展示。
///
/// @author kongweiguang
#[test]
fn resume_reopens_resumable_failure_on_same_task() {
    let mut task = task();
    let original_id = task.id().clone();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(10)).unwrap();
    task.record_progress(6, Some(10)).unwrap();
    task.fail(
        TransferFailureKind::Connection,
        "连接已中断".to_owned(),
        true,
        true,
    )
    .unwrap();
    task.resume_from_checkpoint(5, Some(10)).unwrap();
    let summary = task.summary();
    assert_eq!(summary.id, original_id);
    assert_eq!(summary.status, TransferStatus::Queued);
    assert_eq!(summary.bytes_transferred, 5);
    assert!(summary.failure_kind.is_none() && summary.safe_error.is_none());
}

/// 已有后继、未确认提交或越界断点都不能重新打开原始任务。
///
/// @author kongweiguang
#[test]
fn resume_rejects_invalid_state_successor_and_checkpoint_size() {
    let mut paused = task();
    assert_eq!(
        paused.resume_from_checkpoint(0, Some(10)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
    paused.begin_connecting().unwrap();
    paused.begin_transferring(Some(10)).unwrap();
    assert!(paused.request_cancel());
    paused.confirm_cancelled(true).unwrap();
    assert_eq!(
        paused.resume_from_checkpoint(11, Some(10)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
    paused.set_retry_successor(TransferId::new()).unwrap();
    assert_eq!(
        paused.resume_from_checkpoint(5, Some(10)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );

    let mut unknown = task();
    unknown.begin_connecting().unwrap();
    unknown.begin_transferring(Some(10)).unwrap();
    unknown.begin_commit(local("destination.txt")).unwrap();
    unknown
        .fail(
            TransferFailureKind::CommitUnknown,
            "未知".to_owned(),
            true,
            true,
        )
        .unwrap();
    assert_eq!(
        unknown.resume_from_checkpoint(5, Some(10)),
        Err(TransferTransitionError::RecoveryNotAllowed)
    );
}

/// 验证提交项期间只登记取消意图，缺少回执时仍必须落为 CommitUnknown。
#[test]
fn commit_item_defers_cancel_until_receipt_or_unknown_result() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(1)).unwrap();
    task.record_progress(1, Some(1)).unwrap();
    task.begin_commit(local("destination.txt")).unwrap();

    assert!(task.request_cancel());
    assert_eq!(task.status(), TransferStatus::Committing);
    task.fail(
        TransferFailureKind::CommitUnknown,
        "无法确认目标文件提交结果".to_owned(),
        true,
        false,
    )
    .unwrap();
    assert!(!task.retryable());
    assert_eq!(task.summary().commit_summary.committed_entries(), 0);
}

/// 已确认的目录项在项间取消时保留部分结果，并禁止将重试重复应用到目标。
#[test]
fn cancellation_between_items_preserves_summary_and_blocks_retry() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(None).unwrap();
    task.begin_commit(local("destination.txt")).unwrap();
    task.record_committed_entry(17).unwrap();

    assert!(task.request_cancel());
    assert_eq!(task.status(), TransferStatus::Cancelling);
    task.confirm_cancelled(false).unwrap();

    let summary = task.summary();
    assert_eq!(summary.status, TransferStatus::Cancelled);
    assert_eq!(summary.commit_summary.committed_entries(), 1);
    assert_eq!(summary.commit_summary.committed_bytes(), 17);
    assert!(summary.partial_commit);
    assert!(!summary.retryable);
}

/// 最后一项收到回执后即使取消迟到，最终屏障仍能发布完整成功结果。
#[test]
fn final_receipt_barrier_wins_over_late_cancel() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(Some(0)).unwrap();
    task.begin_commit(local("destination.txt")).unwrap();
    task.record_committed_entry(0).unwrap();

    assert!(task.request_cancel());
    task.finish_commit().unwrap();
    assert!(!task.summary().partial_commit);
    task.complete().unwrap();

    let summary = task.summary();
    assert_eq!(summary.status, TransferStatus::Succeeded);
    assert_eq!(summary.commit_summary.committed_entries(), 1);
    assert!(!summary.partial_commit);
}

/// 最终回执屏障只能发布一次，屏障后不得再接纳额外文件项。
#[test]
fn final_commit_barrier_rejects_duplicate_finalize_and_late_item() {
    let mut task = task();
    task.begin_connecting().unwrap();
    task.begin_transferring(None).unwrap();
    task.begin_commit(local("destination.txt")).unwrap();
    task.record_committed_entry(0).unwrap();
    task.finish_commit().unwrap();

    assert!(task.finish_commit().is_err());
    assert!(task.begin_commit(local("destination.txt")).is_err());
}

/// 目录遍历后无目标改动可显式完成，但不能伪造回执、跳过未完成流程或覆盖取消。
#[test]
fn unchanged_directory_requires_complete_transfer_without_receipts_or_cancellation() {
    let mut directory = task();
    // 用有效目录请求替换测试工厂的文件类型，保留同一隔离端点与状态路径。
    let mut request = directory.request().clone();
    request.kind = TransferKind::Directory;
    directory = TransferTask::queued(TransferId::new(), request).unwrap();
    directory.begin_connecting().unwrap();
    directory.begin_transferring(None).unwrap();

    let commit_summary = directory.finish_no_changes().unwrap();
    assert_eq!(commit_summary.committed_entries(), 0);
    assert_eq!(commit_summary.committed_bytes(), 0);
    assert_eq!(directory.status(), TransferStatus::Unchanged);
    assert!(!directory.summary().partial_commit);
    assert!(!directory.retryable());
    assert!(!directory.request_cancel());

    let mut file = task();
    file.begin_connecting().unwrap();
    file.begin_transferring(Some(0)).unwrap();
    assert!(file.finish_no_changes().is_err());

    let mut cancelled = task();
    let mut request = cancelled.request().clone();
    request.kind = TransferKind::Directory;
    cancelled = TransferTask::queued(TransferId::new(), request).unwrap();
    cancelled.begin_connecting().unwrap();
    cancelled.begin_transferring(None).unwrap();
    assert!(cancelled.request_cancel());
    assert!(cancelled.finish_no_changes().is_err());
}

/// 已经确认目录项后不能再声称整次遍历没有产生变化。
#[test]
fn unchanged_directory_rejects_existing_commit_receipt() {
    let mut request = task().request().clone();
    request.kind = TransferKind::Directory;
    let mut task = TransferTask::queued(TransferId::new(), request).unwrap();
    task.begin_connecting().unwrap();
    task.begin_transferring(None).unwrap();
    task.begin_commit(local("destination.txt")).unwrap();
    assert!(task.finish_no_changes().is_err());
    task.record_committed_entry(0).unwrap();
    assert!(task.finish_no_changes().is_err());
}

/// 单项目标明确未改动时释放提交许可，不增加 receipt 且仍可完成整次无变更遍历。
#[test]
fn explicit_no_effect_item_releases_commit_without_receipt() {
    let mut request = task().request().clone();
    request.kind = TransferKind::Directory;
    let mut task = TransferTask::queued(TransferId::new(), request).unwrap();
    task.begin_connecting().unwrap();
    task.begin_transferring(None).unwrap();
    task.begin_commit(local("already-present")).unwrap();

    let summary = task.confirm_commit_item_unchanged().unwrap();
    assert_eq!(summary.committed_entries(), 0);
    assert_eq!(summary.committed_bytes(), 0);
    assert!(!task.commit_item_in_progress());
    assert_eq!(task.status(), TransferStatus::Transferring);
    task.finish_no_changes().unwrap();
    assert_eq!(task.status(), TransferStatus::Unchanged);
}

/// 固定同一目录目标根来验证后续无副作用项保留 receipt，避免把子项路径误当任务目标。
#[test]
fn explicit_no_effect_item_preserves_previous_receipts_and_deferred_cancel() {
    let mut request = task().request().clone();
    request.kind = TransferKind::Directory;
    let mut task = TransferTask::queued(TransferId::new(), request).unwrap();
    let destination_root = local("destination-root");
    task.begin_connecting().unwrap();
    task.begin_transferring(None).unwrap();
    task.begin_commit(destination_root.clone()).unwrap();
    task.record_committed_entry(7).unwrap();
    task.begin_commit(destination_root).unwrap();

    assert!(task.request_cancel());
    let summary = task.confirm_commit_item_unchanged().unwrap();
    assert_eq!(summary.committed_entries(), 1);
    assert_eq!(summary.committed_bytes(), 7);
    assert_eq!(task.status(), TransferStatus::Cancelling);
    task.confirm_cancelled(false).unwrap();
    assert!(task.summary().partial_commit);
    assert!(!task.retryable());
}

/// 验证仅同一 SSH 主机或同一容器内可重命名，不能混淆连接和文件系统边界。
#[test]
fn rename_scope_tracks_filesystem_identity() {
    let first_host = HostId::new();
    let other_host = HostId::new();
    let first = FileEndpoint::remote(first_host.clone(), "/a").unwrap();
    let same_host = FileEndpoint::remote(first_host, "/b").unwrap();
    let other = FileEndpoint::remote(other_host, "/b").unwrap();

    assert!(first.same_filesystem(&same_host));
    assert!(!first.same_filesystem(&other));
    assert!(!first.same_filesystem(&local("b")));
}
