//! 最近传输折叠及未知提交、待清理项的投影回归。
//!
//! @author kongweiguang

use std::path::PathBuf;

use kerminal_application::transfers::{TransferRecoveryInspection, TransferRecoveryRecord};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferFailureKind, TransferKind, TransferRequest,
        TransferStatus, TransferSummary, TransferTask,
    },
    identity::TransferId,
};

use super::{
    hidden_attention_count, is_clearable_history, needs_attention_or_is_active,
    project_transfer_queue, recovery_actions_row, transfer_percent,
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

/// 折叠隐藏所有任务行，但不改变可清理任务和进行中任务的统计。
///
/// @author kongweiguang
#[test]
fn collapsed_history_hides_rows_without_changing_counts() {
    let summaries = [
        summary(TransferStatus::Succeeded, None, false),
        summary(TransferStatus::Transferring, None, false),
        summary(TransferStatus::Failed, None, false),
        summary(TransferStatus::Succeeded, None, false),
    ];
    for count in 0..=summaries.len() {
        let projection = project_transfer_queue(&summaries[..count], false, |_| false);
        let expanded = project_transfer_queue(&summaries[..count], true, |_| false);
        assert!(projection.visible_rows.is_empty());
        assert_eq!(
            projection.clearable_history_count,
            expanded.clearable_history_count
        );
        assert_eq!(
            projection.active_or_attention_count,
            expanded.active_or_attention_count
        );
    }
}

/// 隐藏的未知提交与恢复材料按任务去重提示，仍不得计入可清理历史。
///
/// @author kongweiguang
#[test]
fn collapsed_history_reports_hidden_attention_without_clearing_it() {
    let pending = summary(
        TransferStatus::Failed,
        Some(TransferFailureKind::CommitUnknown),
        true,
    );
    let recovery = TransferRecoveryRecord {
        id: pending.id.clone(),
        drag_staging: false,
        request: pending.request.clone(),
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: true,
        commit_unknown: true,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    };
    assert!(!is_clearable_history(&pending));
    assert!(needs_attention_or_is_active(&pending));
    let summaries = [
        summary(TransferStatus::Transferring, None, false),
        summary(TransferStatus::Succeeded, None, false),
        summary(TransferStatus::Succeeded, None, false),
        pending,
    ];
    let projection = project_transfer_queue(&summaries, false, |_| false);
    assert_eq!(projection.clearable_history_count, 2);
    assert_eq!(projection.active_or_attention_count, 2);
    assert!(projection.visible_rows.is_empty());
    assert_eq!(
        hidden_attention_count(&summaries, &[recovery], |_| false),
        2
    );
}

/// 暂存写入成功并不表示系统目标已释放副本；折叠仍提示它，垃圾桶只能清理普通历史。
///
/// @author kongweiguang
#[test]
fn pending_system_delivery_remains_attention_until_the_target_releases_it() {
    let pending = summary(TransferStatus::Succeeded, None, false);
    let id = pending.id.clone();
    let summaries = [pending, summary(TransferStatus::Succeeded, None, false)];
    let projection = project_transfer_queue(&summaries, false, |candidate| candidate == &id);
    assert_eq!(projection.clearable_history_count, 1);
    assert_eq!(projection.active_or_attention_count, 1);
    assert_eq!(
        hidden_attention_count(&summaries, &[], |candidate| candidate == &id),
        1
    );
    let released = project_transfer_queue(&summaries, false, |_| false);
    assert_eq!(released.clearable_history_count, 2);
    assert_eq!(released.active_or_attention_count, 0);
}

/// 提交之前不能将完整字节数误报为完成；未知长度、空文件和超大文件都保持一致。
///
/// @author kongweiguang
#[test]
fn transfer_percentage_requires_a_successful_commit() {
    let mut task = summary(TransferStatus::Transferring, None, false);
    assert_eq!(transfer_percent(&task), None);
    task.total_bytes = Some(0);
    assert_eq!(transfer_percent(&task), None);
    task.total_bytes = Some(200);
    task.bytes_transferred = 123;
    assert_eq!(transfer_percent(&task), Some(61));
    task.bytes_transferred = 200;
    task.status = TransferStatus::Committing;
    assert_eq!(transfer_percent(&task), Some(99));
    task.status = TransferStatus::Failed;
    assert_eq!(transfer_percent(&task), Some(99));
    task.total_bytes = Some(u64::MAX);
    task.bytes_transferred = u64::MAX;
    assert_eq!(transfer_percent(&task), Some(99));
    task.status = TransferStatus::Succeeded;
    task.total_bytes = Some(0);
    assert_eq!(transfer_percent(&task), Some(100));
}

/// 展开后按同一时间顺序显示全部记录，状态不会改变排序或隐藏。
///
/// @author kongweiguang
#[test]
fn expanded_history_preserves_all_queue_rows_in_order() {
    let ordinary = summary(TransferStatus::Succeeded, None, false);
    let unknown = summary(
        TransferStatus::Failed,
        Some(TransferFailureKind::CommitUnknown),
        false,
    );
    let active = summary(TransferStatus::Transferring, None, false);
    let summaries = [ordinary, active, unknown];
    let projection = project_transfer_queue(&summaries, true, |_| false);

    assert_eq!(projection.visible_rows.len(), 3);
    assert_eq!(
        projection.visible_rows[2].failure_kind,
        Some(TransferFailureKind::CommitUnknown)
    );
    assert_eq!(
        projection.visible_rows[1].status,
        TransferStatus::Transferring
    );
    assert_eq!(projection.visible_rows[0].status, TransferStatus::Succeeded);
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
