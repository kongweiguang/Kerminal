//! 传输历史清理的面板 busy 与取消状态回归。
//!
//! @author kongweiguang

use super::TransferQueueUi;
use kerminal_application::{
    AppError,
    transfers::{TransferRecoveryInspection, TransferRecoveryPage, TransferRecoveryRecord},
};
use kerminal_domain::{
    files::{ConflictPolicy, FileEndpoint, TransferKind, TransferRequest},
    identity::TransferId,
};

/// 冷档样本只携带非秘密证据，不创建目标文件或真实队列 worker。
/// @author kongweiguang
fn archive_record() -> TransferRecoveryRecord {
    TransferRecoveryRecord {
        id: TransferId::new(),
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
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: false,
        commit_unknown: true,
        inspection: TransferRecoveryInspection::DestinationMissing,
        archived: true,
    }
}

/// 刷新来源不拥有独立门禁；归档完成后的刷新必须同时拒绝旧活任务、恢复材料及迟到错误。
/// @author kongweiguang
#[test]
fn shared_transfer_refresh_generation_rejects_pre_archive_results() {
    let mut queue = TransferQueueUi::default();
    let manual_before_archive = queue.begin_transfer_refresh();
    let event_before_archive = queue.begin_transfer_refresh();
    let after_archive = queue.begin_transfer_refresh();
    assert!(queue.accepts_transfer_refresh(after_archive));

    let mut visible_live = Vec::<TransferId>::new();
    let mut visible_recovery = Vec::<TransferRecoveryRecord>::new();
    let mut old_unknown = archive_record();
    old_unknown.archived = false;
    for old_generation in [manual_before_archive, event_before_archive] {
        if queue.accepts_transfer_refresh(old_generation) {
            visible_live = vec![old_unknown.id.clone()];
            visible_recovery = vec![old_unknown.clone()];
        }
    }
    assert!(visible_live.is_empty());
    assert!(visible_recovery.is_empty());
    assert!(queue.accepts_transfer_refresh(after_archive));
}

/// 读取锁竞争保留在恢复列表状态；迟到失败不覆盖重试，只有当前成功回执清除错误。
/// @author kongweiguang
#[test]
fn recovery_list_error_survives_until_current_retry_succeeds() {
    let mut queue = TransferQueueUi::default();
    let failed_generation = queue.begin_transfer_refresh();
    assert!(queue.recovery_list_loading);
    assert!(queue.finish_recovery_refresh(
        failed_generation,
        Err(AppError::infrastructure("storage lock busy")),
    ));
    assert!(!queue.recovery_list_loading);
    assert!(queue.recovery_list_error.is_some());

    let retry_generation = queue.begin_transfer_refresh();
    assert!(queue.recovery_list_loading);
    assert!(queue.recovery_list_error.is_some());
    assert!(!queue.finish_recovery_refresh(
        failed_generation,
        Err(AppError::infrastructure("late failure")),
    ));
    assert!(queue.recovery_list_error.is_some());
    assert!(queue.finish_recovery_refresh(retry_generation, Ok(())));
    assert!(!queue.recovery_list_loading);
    assert!(queue.recovery_list_error.is_none());
}

/// 冷档翻页与活队列刷新是独立读取，任何一方的新代次都不应误丢另一方当前回执。
/// @author kongweiguang
#[test]
fn transfer_refresh_and_archive_page_generations_remain_independent() {
    let mut queue = TransferQueueUi::default();
    let refresh = queue.begin_transfer_refresh();
    let page = queue.begin_archive_page();
    assert!(queue.accepts_transfer_refresh(refresh));
    let next_refresh = queue.begin_transfer_refresh();
    assert!(queue.finish_archive_page(
        page,
        Ok(TransferRecoveryPage {
            records: Vec::new(),
            next_cursor: None,
        })
    ));
    assert!(!queue.accepts_transfer_refresh(refresh));
    assert!(queue.accepts_transfer_refresh(next_refresh));
}

/// 新读取已开始后，旧页失败或成功均不得解除 busy、覆盖新游标或原核对结论。
/// @author kongweiguang
#[test]
fn archive_page_generation_rejects_late_results() {
    let mut queue = TransferQueueUi::default();
    let first = queue.begin_archive_page();
    let second = queue.begin_archive_page();
    assert!(!queue.finish_archive_page(first, Err(AppError::infrastructure("late failure"))));
    assert!(queue.archive_loading);
    assert!(!queue.archive_loaded);
    assert!(queue.archive_error.is_none());
    let confirmed = archive_record();
    assert!(queue.finish_archive_page(
        second,
        Ok(TransferRecoveryPage {
            records: vec![confirmed.clone()],
            next_cursor: Some(confirmed.id.clone()),
        })
    ));
    assert!(!queue.finish_archive_page(
        first,
        Ok(TransferRecoveryPage {
            records: Vec::new(),
            next_cursor: None,
        })
    ));
    assert_eq!(queue.archive_records, vec![confirmed.clone()]);
    assert_eq!(queue.archive_next_cursor, Some(confirmed.id));
    assert!(!queue.archive_loading);
    assert!(queue.archive_loaded);
}

/// 下一页失败保留上页供查看和重试，重试成功只替换一页，不能无限累加历史。
/// @author kongweiguang
#[test]
fn archive_page_failure_retains_confirmed_evidence_and_retry_replaces_page() {
    let mut queue = TransferQueueUi::default();
    let first = archive_record();
    let generation = queue.begin_archive_page();
    assert!(queue.finish_archive_page(
        generation,
        Ok(TransferRecoveryPage {
            records: vec![first.clone()],
            next_cursor: Some(first.id.clone()),
        })
    ));
    let generation = queue.begin_archive_page();
    assert!(queue.finish_archive_page(generation, Err(AppError::infrastructure("page failed"))));
    assert_eq!(queue.archive_records, vec![first.clone()]);
    assert_eq!(queue.archive_next_cursor, Some(first.id));
    assert!(queue.archive_error.is_some());
    let generation = queue.begin_archive_page();
    assert!(queue.archive_error.is_none());
    let next = archive_record();
    assert!(queue.finish_archive_page(
        generation,
        Ok(TransferRecoveryPage {
            records: vec![next.clone()],
            next_cursor: None,
        })
    ));
    assert_eq!(queue.archive_records, vec![next]);
    assert!(queue.archive_next_cursor.is_none());
}

/// 一次只允许一个 clear 用例；取消后在服务返回前仍保持 busy，结束后可以重试。
///
/// @author kongweiguang
#[test]
fn clear_history_state_guards_reentry_until_completion() {
    let mut queue = TransferQueueUi::default();
    let cancellation = queue
        .begin_clear_completed_history()
        .expect("first clear request starts");

    assert!(queue.is_clearing_completed_history());
    assert!(queue.begin_clear_completed_history().is_none());
    assert!(queue.cancel_clear_completed_history());
    assert!(cancellation.is_cancelled());
    assert!(queue.is_clearing_completed_history());

    assert!(queue.finish_clear_completed_history());
    assert!(!queue.is_clearing_completed_history());
    assert!(
        queue
            .begin_clear_completed_history()
            .is_some_and(|next| !next.is_cancelled())
    );
}

/// 卸载队列视图时通知在途请求取消，避免无 UI 观察者时继续删除历史。
///
/// @author kongweiguang
#[test]
fn dropping_queue_view_cancels_in_flight_history_cleanup() {
    let mut queue = TransferQueueUi::default();
    let cancellation = queue
        .begin_clear_completed_history()
        .expect("clear request starts");

    drop(queue);

    assert!(cancellation.is_cancelled());
}
