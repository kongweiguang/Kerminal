//! 工作区保存恢复的回执分类、显式动作与 CAS 基线回归。
//!
//! @author kongweiguang

// 真实控件回归单独保留夹具，避免把状态机断言误认为用户交互覆盖。
#[path = "workspace_persistence_recovery_ui.rs"]
mod ui_interactions;

use kerminal_application::{
    AppError, PublicErrorCode,
    workspace::{
        WorkspaceSessionDocument, WorkspaceSessionExpectedRevision, WorkspaceSessionInvalidReason,
        WorkspaceSessionLoad, WorkspaceSessionRevision, WorkspaceSessionSaveOutcome,
    },
};

use super::terminal_open::{
    WorkspaceRecoveryAction, normalize_workspace_save_result, workspace_recovery_actions,
};
use crate::features::workspace_controller::{
    WorkspaceController, WorkspacePersistenceState, WorkspaceSaveRequest,
};

/// 使用真实状态机生成待提交请求，确保测试不会绕过 Missing 基线和在途门禁。
///
/// @author kongweiguang
fn pending_save() -> (WorkspaceController, WorkspaceSaveRequest) {
    let mut controller = WorkspaceController::from_missing().unwrap();
    let mut snapshot = controller.snapshot().clone();
    snapshot.selected_machine_id = Some("working".to_owned());
    controller.synchronize_snapshot(snapshot).unwrap();
    let request = controller.begin_save().unwrap().unwrap();
    (controller, request)
}

/// executor 外层未知回执必须和仓储未知回执一样封锁重写，直到只读核对与用户明确重试。
///
/// @author kongweiguang
#[test]
fn executor_unknown_requires_readback_and_explicit_cas_retry() {
    let (mut controller, request) = pending_save();
    let outcome = normalize_workspace_save_result(Err(AppError::outcome_unknown("receipt lost")))
        .expect("outer unknown must enter the typed unknown state");
    assert_eq!(outcome, WorkspaceSessionSaveOutcome::OutcomeUnknown);
    controller
        .finish_save(&request.request_id, outcome)
        .unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), false),
        [WorkspaceRecoveryAction::Readback],
    );
    assert!(controller.begin_save().unwrap().is_none());
    assert!(controller.ensure_no_pending_resource().is_err());
    assert!(controller.retry_after_readback().is_err());
    assert_eq!(controller.snapshot(), &request.source_snapshot);

    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Missing)
        .unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), false),
        [WorkspaceRecoveryAction::RetryUncommitted],
    );
    assert!(controller.begin_save().unwrap().is_none());
    assert!(controller.ensure_no_pending_resource().is_err());
    controller.retry_after_readback().unwrap();
    let retry = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        retry.expected_revision,
        WorkspaceSessionExpectedRevision::Missing
    );
    assert_eq!(retry.desired_snapshot, request.desired_snapshot);
    controller
        .finish_save(
            &retry.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("confirmed"),
                document: retry.candidate_document.clone(),
            },
        )
        .unwrap();
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Clean
    ));
    assert_eq!(controller.snapshot(), &request.source_snapshot);
}

/// 回读确认旧布局未提交时仍须使用磁盘最新 revision，并保留该版本附加的未知字段。
///
/// @author kongweiguang
#[test]
fn uncommitted_loaded_readback_retries_against_latest_revision() {
    let base = WorkspaceSessionDocument::empty_v3();
    let mut controller = WorkspaceController::from_loaded(
        base.clone(),
        WorkspaceSessionRevision::new("old-revision"),
    )
    .unwrap();
    let mut desired = controller.snapshot().clone();
    desired.selected_machine_id = Some("working".into());
    controller.synchronize_snapshot(desired.clone()).unwrap();
    let request = controller.begin_save().unwrap().unwrap();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::OutcomeUnknown,
        )
        .unwrap();
    let mut external = base.into_value();
    external["futureRoot"] = serde_json::json!({"keep": true});
    let latest = WorkspaceSessionDocument::try_from_value(external).unwrap();
    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
            document: latest.clone(),
            revision: WorkspaceSessionRevision::new("latest-revision"),
        })
        .unwrap();
    assert!(controller.begin_save().unwrap().is_none());
    controller.retry_after_readback().unwrap();
    let retry = controller.begin_save().unwrap().unwrap();
    assert_eq!(retry.base_document, latest);
    assert_eq!(
        retry.expected_revision,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
            "latest-revision"
        )),
    );
    assert_eq!(
        retry.candidate_document.as_value()["futureRoot"]["keep"],
        true
    );
    assert_eq!(retry.desired_snapshot, desired);
}

/// 回读只确认已提交请求，保存期间的新状态必须继续 Dirty 并使用回读的 revision。
///
/// @author kongweiguang
#[test]
fn committed_readback_preserves_later_work_and_advances_revision() {
    let (mut controller, request) = pending_save();
    let mut newer = request.source_snapshot.clone();
    newer.selected_machine_id = Some("later".to_owned());
    controller.synchronize_snapshot(newer.clone()).unwrap();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::OutcomeUnknown,
        )
        .unwrap();
    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Loaded {
            document: request.candidate_document.clone(),
            revision: WorkspaceSessionRevision::new("readback-revision"),
        })
        .unwrap();
    assert_eq!(controller.snapshot(), &newer);
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::Dirty
    ));
    let next = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        next.expected_revision,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
            "readback-revision"
        )),
    );
    assert_eq!(next.desired_snapshot, newer);
}

/// 无效或未来版本只允许重新核对；修复后也仍是冲突，直到用户明确选择覆盖。
///
/// @author kongweiguang
#[test]
fn invalid_conflict_requires_repair_readback_and_explicit_overwrite() {
    let (mut controller, request) = pending_save();
    controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Conflict {
                latest: WorkspaceSessionLoad::Invalid {
                    reason: WorkspaceSessionInvalidReason::MalformedJson,
                },
            },
        )
        .unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), false),
        [WorkspaceRecoveryAction::Readback]
    );
    assert!(controller.confirm_overwrite_latest().is_err());
    assert!(controller.begin_save().unwrap().is_none());
    assert_eq!(controller.snapshot(), &request.source_snapshot);

    controller
        .refresh_conflicting_readback(WorkspaceSessionLoad::Unsupported { version: 42 })
        .unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), false),
        [WorkspaceRecoveryAction::Readback]
    );
    let mut external = WorkspaceSessionDocument::empty_v3();
    let mut external_snapshot = external.workspace_snapshot().unwrap();
    external_snapshot.selected_machine_id = Some("external".to_owned());
    external.update_known_fields(&external_snapshot).unwrap();
    controller
        .refresh_conflicting_readback(WorkspaceSessionLoad::Loaded {
            document: external,
            revision: WorkspaceSessionRevision::new("repaired-revision"),
        })
        .unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), false),
        [
            WorkspaceRecoveryAction::Readback,
            WorkspaceRecoveryAction::ConfirmOverwrite
        ],
    );
    assert!(controller.begin_save().unwrap().is_none());
    assert!(controller.retry_after_readback().is_err());
    assert_eq!(controller.snapshot(), &request.source_snapshot);
    controller.confirm_overwrite_latest().unwrap();
    let overwrite = controller.begin_save().unwrap().unwrap();
    assert_eq!(
        overwrite.expected_revision,
        WorkspaceSessionExpectedRevision::Revision(WorkspaceSessionRevision::new(
            "repaired-revision"
        )),
    );
    assert_eq!(overwrite.desired_snapshot, request.source_snapshot);
}

/// 确定取消仍是可重试失败，不能错误地显示为提交未知或要求只读核对。
///
/// @author kongweiguang
#[test]
fn definite_failure_is_not_reclassified_as_unknown() {
    let error = normalize_workspace_save_result(Err(AppError::cancelled("cancelled"))).unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::Cancelled);
    let (mut controller, request) = pending_save();
    controller.finish_save_failure(&request.request_id).unwrap();
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), true),
        [WorkspaceRecoveryAction::RetryFailed]
    );
    assert!(controller.retry_after_readback().is_err());
    assert!(controller.begin_save().unwrap().is_some());
}

/// 声称成功却携带其他布局的回执不可作为基线；保留原请求并强制只读核对。
///
/// @author kongweiguang
#[test]
fn mismatched_committed_document_keeps_unknown_request_for_readback() {
    let (mut controller, request) = pending_save();
    let error = controller
        .finish_save(
            &request.request_id,
            WorkspaceSessionSaveOutcome::Saved {
                revision: WorkspaceSessionRevision::new("untrusted-revision"),
                document: WorkspaceSessionDocument::empty_v3(),
            },
        )
        .unwrap_err();
    assert_eq!(error.public_code(), PublicErrorCode::OutcomeUnknown);
    assert_eq!(
        workspace_recovery_actions(controller.persistence_state(), true),
        [WorkspaceRecoveryAction::Readback]
    );
    assert!(controller.begin_save().unwrap().is_none());
    assert_eq!(controller.snapshot(), &request.source_snapshot);
    controller
        .resolve_unknown_readback(WorkspaceSessionLoad::Missing)
        .unwrap();
    assert!(matches!(
        controller.persistence_state(),
        WorkspacePersistenceState::ReadbackConfirmedUncommitted { .. }
    ));
}
