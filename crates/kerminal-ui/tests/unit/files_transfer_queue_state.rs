//! 传输历史清理的面板 busy 与取消状态回归。
//!
//! @author kongweiguang

use super::TransferQueueUi;
use kerminal_application::{
    AppError,
    transfers::{TransferRecoveryInspection, TransferRecoveryRecord},
};
use kerminal_domain::{
    files::{ConflictPolicy, FileEndpoint, TransferKind, TransferRequest},
    identity::TransferId,
};

/// 恢复样本只携带非秘密证据，不创建目标文件或真实队列 worker。
/// @author kongweiguang
fn recovery_record() -> TransferRecoveryRecord {
    TransferRecoveryRecord {
        id: TransferId::new(),
        origin: None,
        drag_staging: false,
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
        archived: false,
    }
}

/// 活队列与恢复列表共用读取代次，迟到结果不能重新插入已被新快照替换的记录。
/// @author kongweiguang
#[test]
fn shared_transfer_refresh_generation_rejects_stale_results() {
    let mut queue = TransferQueueUi::default();
    let manual_before_archive = queue.begin_transfer_refresh();
    let event_before_archive = queue.begin_transfer_refresh();
    let after_archive = queue.begin_transfer_refresh();
    assert!(queue.accepts_transfer_refresh(after_archive));

    let mut visible_live = Vec::<TransferId>::new();
    let mut visible_recovery = Vec::<TransferRecoveryRecord>::new();
    let mut old_unknown = recovery_record();
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

/// 删除补偿不属于单次读取代次；旧快照失效后，新快照仍须接管且只能消费一次。
///
/// @author kongweiguang
#[test]
fn uncertain_transfer_refresh_survives_stale_generation() {
    let mut queue = TransferQueueUi::default();
    queue.mark_uncertain_transfer_change();
    queue.mark_uncertain_transfer_change();
    let stale = queue.begin_transfer_refresh();
    let current = queue.begin_transfer_refresh();
    assert!(!queue.accepts_transfer_refresh(stale));
    assert!(queue.accepts_transfer_refresh(current));
    assert!(queue.take_uncertain_transfer_change());
    assert!(!queue.take_uncertain_transfer_change());
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

/// 一次只允许一个 clear 用例；取消后在服务返回前仍保持 busy，结束后可以重试。
///
/// @author kongweiguang
#[test]
fn clear_history_state_guards_reentry_until_completion() {
    let mut queue = TransferQueueUi::default();
    let cancellation = queue
        .begin_clear_completed_history()
        .expect("first clear request starts");

    assert!(queue.is_clearing_for_origin(None));
    assert!(queue.begin_clear_completed_history().is_none());
    assert!(queue.cancel_clear_completed_history());
    assert!(cancellation.is_cancelled());
    assert!(queue.is_clearing_for_origin(None));

    assert!(queue.finish_clear_completed_history());
    assert!(!queue.is_clearing_for_origin(None));
    assert!(
        queue
            .begin_clear_completed_history()
            .is_some_and(|next| !next.is_cancelled())
    );
}

/// 两种操作可能移除同一原任务，互斥直到真实回执；取消请求不能提前放行重试。
///
/// @author kongweiguang
#[test]
fn history_cleanup_and_retry_are_mutually_exclusive() {
    let mut queue = TransferQueueUi::default();
    let id = TransferId::new();
    assert!(queue.begin_transfer_retry(id.clone()));
    assert!(queue.begin_clear_completed_history().is_none());
    queue.finish_transfer_retry(&id);
    assert!(queue.begin_clear_completed_history().is_some());
    assert!(!queue.begin_transfer_retry(id.clone()));
    assert!(queue.cancel_clear_completed_history());
    assert!(!queue.begin_transfer_retry(id.clone()));
    queue.finish_clear_completed_history();
    assert!(queue.begin_transfer_retry(id));
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
