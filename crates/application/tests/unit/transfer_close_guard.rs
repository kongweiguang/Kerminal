//! 传输来源关闭门禁策略回归。
//!
//! @author kongweiguang

use super::{recovery_orphan_blocks_close, transfer_summary_blocks_close};
use crate::transfers::{TransferOrigin, TransferRecoveryInspection, TransferRecoveryRecord};
use kerminal_domain::{
    files::{
        ConflictPolicy, FileEndpoint, TransferCommitSummary, TransferFailureKind, TransferKind,
        TransferRequest, TransferStatus, TransferSummary,
    },
    identity::{PaneId, TransferId},
};
use std::collections::HashSet;

/// 以普通终态作为基线，单测聚焦决定关闭门禁的生命周期字段。
///
/// @author kongweiguang
fn completed_summary() -> TransferSummary {
    TransferSummary {
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
        status: TransferStatus::Succeeded,
        phase: None,
        bytes_transferred: 0,
        total_bytes: Some(0),
        speed_bytes_per_second: 0,
        last_progress_at: None,
        effective_idle_timeout_seconds: None,
        retryable: false,
        resumable: false,
        failure_kind: None,
        safe_error: None,
        actual_destination: None,
        automatic_recovery_used: false,
        retry_successor: None,
        commit_summary: TransferCommitSummary::default(),
        partial_commit: false,
        cleanup_pending: false,
    }
}

/// 恢复夹具保留来源身份，测试 orphan 与 live ID 去重而不启动仓储或 worker。
///
/// @author kongweiguang
fn recovery_record(id: TransferId) -> TransferRecoveryRecord {
    TransferRecoveryRecord {
        id,
        origin: Some(TransferOrigin::TerminalPane(PaneId::new())),
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
        drag_staging: false,
        actual_destination: None,
        checkpoint: None,
        staging_ownership: Vec::new(),
        cleanup_pending: false,
        commit_unknown: false,
        inspection: TransferRecoveryInspection::Insufficient,
        archived: false,
    }
}

/// 已确认结束的任务允许关闭；worker join、可续传暂停、未知提交和拖出交付都保留来源入口。
///
/// @author kongweiguang
#[test]
fn live_transfer_close_policy_keeps_only_actionable_or_unfinished_owners() {
    let summary = completed_summary();
    assert!(!transfer_summary_blocks_close(&summary, true, false));
    assert!(transfer_summary_blocks_close(&summary, false, false));
    assert!(transfer_summary_blocks_close(&summary, true, true));

    let mut paused = summary.clone();
    paused.status = TransferStatus::Cancelled;
    paused.resumable = true;
    assert!(transfer_summary_blocks_close(&paused, true, false));

    let mut failed_with_checkpoint = summary.clone();
    failed_with_checkpoint.status = TransferStatus::Failed;
    failed_with_checkpoint.resumable = true;
    assert!(transfer_summary_blocks_close(
        &failed_with_checkpoint,
        true,
        false
    ));

    let mut unknown = summary.clone();
    unknown.status = TransferStatus::Failed;
    unknown.failure_kind = Some(TransferFailureKind::CommitUnknown);
    assert!(transfer_summary_blocks_close(&unknown, true, false));

    let mut cleanup = summary;
    cleanup.cleanup_pending = true;
    assert!(transfer_summary_blocks_close(&cleanup, true, false));
}

/// 普通成功 live row 吸收同 ID 的旧恢复事实；只有孤儿待办阻止关闭。
///
/// @author kongweiguang
#[test]
fn recovery_close_policy_ignores_live_id_but_blocks_orphan_attention() {
    let id = TransferId::new();
    let record = recovery_record(id.clone());
    let live_ids = HashSet::from([id]);
    assert!(!recovery_orphan_blocks_close(&record, &live_ids));

    let orphan = recovery_record(TransferId::new());
    assert!(recovery_orphan_blocks_close(&orphan, &HashSet::new()));
}
