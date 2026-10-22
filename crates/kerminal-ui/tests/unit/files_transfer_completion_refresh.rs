//! 文件传输完成投影必须尊重浏览中的 requested endpoint 和 panel generation。
//!
//! @author kongweiguang

use super::{
    begin_panel_load_request, completed_transfer_refresh_directories, finish_panel_load_request,
    panel_needs_transfer_refresh, panel_refresh_target,
};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferCommitSummary, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus, TransferSummary, TransferTask,
    },
    identity::TransferId,
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
    assert_eq!(a_directories, vec![local("A")]);
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
    assert_eq!(b_directories, vec![local("B")]);
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
    assert_eq!(merged_directories, vec![local("A"), local("B")]);
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

/// 文件与目录都依赖实际提交回执；未知、无回执成功和 Unchanged 不刷新。
///
/// @author kongweiguang
#[test]
fn only_confirmed_success_receipts_produce_refresh_targets() {
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
        local("out/unknown"),
        Some(local("out/unknown")),
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

    assert_eq!(
        completed_transfer_refresh_directories(&[], &[file, unknown, empty_success, unchanged]),
        vec![local("out")],
    );
}
