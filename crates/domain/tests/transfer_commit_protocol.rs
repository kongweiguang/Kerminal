//! 多项提交的 receipt、取消边界和最终结果屏障回归测试。
//! 通过领域库已公开的协议 API 验证状态边界，避免在集成测试中重复编译实现。
//!
//! @author kongweiguang

use kerminal_domain::files::transfer_commit_protocol::{
    TransferCommitProtocol, TransferCommitTransitionError,
};

/// 提交未决时暂缓取消，回执明确后允许在下一项之前取消并保留已确认统计。
#[test]
fn confirmed_receipt_opens_cancellation_window_between_items() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("first item starts");
    assert!(protocol.cancellation_is_deferred());
    protocol
        .record_confirmed_receipt(64)
        .expect("receipt is recorded");

    assert!(!protocol.cancellation_is_deferred());
    assert_eq!(protocol.summary().committed_entries(), 1);
    assert_eq!(protocol.summary().committed_bytes(), 64);
}

/// 当前项收到取消后等待 receipt，确认项完成即开放取消并拒绝开始下一项。
#[test]
fn cancel_waits_for_current_item_then_stops_following_items() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("item starts");
    protocol.request_cancel();
    assert!(protocol.cancellation_requested());
    assert!(protocol.cancellation_is_deferred());

    protocol
        .record_confirmed_receipt(11)
        .expect("current item receipt resolves commit boundary");
    assert!(!protocol.cancellation_is_deferred());
    assert_eq!(
        protocol.begin_item(),
        Err(TransferCommitTransitionError::CancellationRequested),
    );
    assert_eq!(protocol.summary().committed_bytes(), 11);
}

/// 最后一项已回执后 finalize 声明所有目标改动完成，迟到取消不能改写成功状态。
#[test]
fn final_receipts_win_over_late_cancellation() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("last item starts");
    protocol
        .record_confirmed_receipt(23)
        .expect("last item receipt");
    protocol.request_cancel();

    protocol
        .finish_receipts()
        .expect("backend confirms the full receipt set");
    assert!(protocol.is_finalized());
    assert!(protocol.cancellation_is_deferred());
    assert_eq!(protocol.summary().committed_bytes(), 23);
}

/// 最终屏障只在有明确回执后关闭目录项接纳并保护最终结果。
#[test]
fn final_receipt_barrier_rejects_empty_and_late_items() {
    let mut empty = TransferCommitProtocol::default();
    assert_eq!(
        empty.finish_receipts(),
        Err(TransferCommitTransitionError::NoConfirmedReceipt),
    );

    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("item starts");
    protocol
        .record_confirmed_receipt(0)
        .expect("empty file still has a commit receipt");
    protocol
        .finish_receipts()
        .expect("all receipts are complete");
    assert!(protocol.is_finalized());
    assert!(protocol.cancellation_is_deferred());
    assert_eq!(
        protocol.begin_item(),
        Err(TransferCommitTransitionError::AlreadyFinalized),
    );
    assert_eq!(
        protocol.finish_receipts(),
        Err(TransferCommitTransitionError::AlreadyFinalized),
    );
}

/// 未知回执保留之前已知的部分摘要，但当前项不计数且不能安全自动重试。
#[test]
fn unknown_receipt_does_not_count_current_item_or_allow_retry() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("first item starts");
    protocol
        .record_confirmed_receipt(10)
        .expect("first item receipt");
    protocol.begin_item().expect("second item starts");
    protocol
        .mark_outcome_unknown()
        .expect("second item has unknown receipt");

    assert!(protocol.is_outcome_unknown());
    assert!(!protocol.can_retry_safely());
    assert_eq!(protocol.summary().committed_entries(), 1);
    assert_eq!(protocol.summary().committed_bytes(), 10);
    assert_eq!(
        protocol.finish_receipts(),
        Err(TransferCommitTransitionError::OutcomeUnknown),
    );
}

/// 未获得提交许可或已结束当前项时，receipt 请求不能伪造领域结果。
#[test]
fn receipt_requires_an_open_item_and_cannot_be_repeated() {
    let mut protocol = TransferCommitProtocol::default();
    assert_eq!(
        protocol.record_confirmed_receipt(8),
        Err(TransferCommitTransitionError::NoItemInProgress),
    );
    protocol.begin_item().expect("item starts");
    protocol
        .record_confirmed_receipt(8)
        .expect("first receipt accepted");
    assert_eq!(
        protocol.record_confirmed_receipt(8),
        Err(TransferCommitTransitionError::NoItemInProgress),
    );
}

/// 控制通道确认后端尚未收到许可时，回滚 reservation 不应伪造未知提交。
#[test]
fn undelivered_commit_permission_can_be_aborted_without_unknown_result() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("item reservation starts");
    protocol
        .abort_unacknowledged_item()
        .expect("undelivered permission is safely revoked");

    assert!(!protocol.item_in_progress());
    assert!(protocol.can_retry_safely());
    assert_eq!(protocol.summary().committed_entries(), 0);
}

/// 许可送达失败期间收到取消时，回滚 reservation 后保留取消并拒绝重试。
#[test]
fn undelivered_permission_abort_preserves_racing_cancellation() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("item reservation starts");
    protocol.request_cancel();
    protocol
        .abort_unacknowledged_item()
        .expect("no mutation was allowed before permission delivery");

    assert!(protocol.cancellation_requested());
    assert!(!protocol.item_in_progress());
    assert!(!protocol.can_retry_safely());
    assert_eq!(protocol.summary().committed_entries(), 0);
}

/// 同一时刻只能有一项进入不可取消提交区。
#[test]
fn concurrent_item_commit_is_rejected() {
    let mut protocol = TransferCommitProtocol::default();
    protocol.begin_item().expect("first item starts");
    assert_eq!(
        protocol.begin_item(),
        Err(TransferCommitTransitionError::ItemAlreadyInProgress),
    );
}
