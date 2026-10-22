//! 传输队列折叠历史时对未知提交与待清理项的投影回归。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest,
        TransferStatus, TransferSummary, TransferTask,
    },
    identity::TransferId,
};

use super::{
    is_clearable_history, needs_attention_or_is_active, project_transfer_queue,
    recovery_actions_row,
};

/// 通过真实领域摘要保留 UI 投影依赖的字段形状，避免手工构造不一致 DTO。
///
/// @author kongweiguang
fn summary(
    status: TransferStatus,
    failure_kind: Option<TransferFailureKind>,
    cleanup_pending: bool,
) -> TransferSummary {
    let request = TransferRequest {
        source: FileEndpoint::Local {
            path: PathBuf::from("source.txt"),
        },
        destination: FileEndpoint::Local {
            path: PathBuf::from("destination.txt"),
        },
        kind: TransferKind::File,
        conflict_policy: ConflictPolicy::Overwrite,
        idle_timeout_seconds: None,
    };
    let mut summary = TransferTask::queued(TransferId::new(), request)
        .expect("test endpoints are distinct")
        .summary();
    summary.status = status;
    summary.failure_kind = failure_kind;
    summary.cleanup_pending = cleanup_pending;
    summary
}

/// 折叠时 CommitUnknown 必须继续显示并计入注意项，不计入普通已完成历史。
///
/// @author kongweiguang
#[test]
fn collapsed_history_keeps_unknown_commit_visible_as_attention() {
    let ordinary = summary(TransferStatus::Succeeded, None, false);
    let unknown = summary(
        TransferStatus::Failed,
        Some(TransferFailureKind::CommitUnknown),
        false,
    );
    let summaries = [ordinary, unknown];
    let projection = project_transfer_queue(&summaries, false);

    assert_eq!(projection.clearable_history_count, 1);
    assert_eq!(projection.active_or_attention_count, 1);
    assert_eq!(projection.visible_rows.len(), 1);
    assert_eq!(
        projection.visible_rows[0].failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
}

/// 未完成清理的终态不属于普通历史，折叠后仍保留待办入口。
///
/// @author kongweiguang
#[test]
fn collapsed_history_keeps_cleanup_owner_visible_and_actionable() {
    let pending = summary(TransferStatus::Failed, None, true);

    assert!(!is_clearable_history(&pending));
    assert!(needs_attention_or_is_active(&pending));
    let summaries = [pending];
    let projection = project_transfer_queue(&summaries, false);
    assert_eq!(projection.clearable_history_count, 0);
    assert_eq!(projection.active_or_attention_count, 1);
    assert_eq!(projection.visible_rows.len(), 1);
    assert!(projection.visible_rows[0].cleanup_pending);
}

/// 展开历史仅增加普通终态；未知提交和活动任务按注意项优先呈现。
///
/// @author kongweiguang
#[test]
fn expanded_history_orders_attention_before_active_and_ordinary_rows() {
    let ordinary = summary(TransferStatus::Succeeded, None, false);
    let unknown = summary(
        TransferStatus::Failed,
        Some(TransferFailureKind::CommitUnknown),
        false,
    );
    let active = summary(TransferStatus::Transferring, None, false);
    let summaries = [ordinary, active, unknown];
    let projection = project_transfer_queue(&summaries, true);

    assert_eq!(projection.visible_rows.len(), 3);
    assert_eq!(
        projection.visible_rows[0].failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
    assert_eq!(
        projection.visible_rows[1].status,
        TransferStatus::Transferring
    );
    assert_eq!(projection.visible_rows[2].status, TransferStatus::Succeeded);
    assert_eq!(projection.clearable_history_count, 1);
    assert_eq!(projection.active_or_attention_count, 2);
}

/// 操作区独立占满内容行并允许换行，新增按钮不能再次挤占恢复目标的标题行。
/// 这里只断言实际绘制容器的样式投影；窄栏文字和未知状态仍须当前候选实窗确认。
///
/// @author kongweiguang
#[test]
fn recovery_actions_keep_an_independent_wrappable_row() {
    use gpui_kit::{AlignItems, FlexDirection, FlexWrap, Styled, px};

    let mut actions = recovery_actions_row();
    let style = actions.style();

    assert_eq!(style.flex_direction, Some(FlexDirection::Row));
    assert_eq!(style.flex_wrap, Some(FlexWrap::Wrap));
    assert_eq!(style.align_items, Some(AlignItems::Center));
    assert_eq!(style.min_size.width, Some(px(0.).into()));
    assert!(
        style.size.width.is_some(),
        "actions reserve their own full row"
    );
}
