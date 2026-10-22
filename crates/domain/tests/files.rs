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
