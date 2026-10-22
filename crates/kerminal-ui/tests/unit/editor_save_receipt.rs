//! 编辑器保存回执与取消后的关闭决策契约。
//!
//! @author kongweiguang

use super::super::{EditorSaveFailureAction, EditorSaveSuccessAction};
use super::{save_failure_decision, save_success_action};

/// 提交前安全取消只保留工作区，关闭和退出意图等待真实取消回执后撤销。
#[test]
fn before_commit_cancel_preserves_the_editor_and_cancels_close_followups() {
    let decision = save_failure_decision(true, false, false, true, true, true);

    assert_eq!(decision.action, EditorSaveFailureAction::PreserveWorkspace);
    assert!(!decision.arm_close_after_readback);
    assert!(decision.cancel_followup_intents);
}

/// 已提交写入即使先收到取消请求也采用真实成功回执，但不能自动关闭标签或应用。
#[test]
fn after_commit_success_keeps_the_editor_open_after_cancel_request() {
    assert_eq!(
        save_success_action(true, true, true, true, false),
        EditorSaveSuccessAction::KeepOpen
    );
}

/// 结果未知时取消不授权自动读回关闭，用户仍须在可见编辑器内主动处理结果。
#[test]
fn after_commit_unknown_does_not_reuse_cancelled_close_intent() {
    let decision = save_failure_decision(true, false, true, true, true, true);

    assert_eq!(decision.action, EditorSaveFailureAction::PreserveWorkspace);
    assert!(!decision.arm_close_after_readback);
    assert!(decision.cancel_followup_intents);
}

/// 未取消的显式保存并关闭遇到未知结果时，保留 owner 并等待用户确认读回。
#[test]
fn uncancelled_unknown_close_requires_readback_before_removal() {
    let decision = save_failure_decision(false, false, true, true, false, false);

    assert_eq!(decision.action, EditorSaveFailureAction::PreserveWorkspace);
    assert!(decision.arm_close_after_readback);
    assert!(!decision.cancel_followup_intents);
}

/// 用户取消后即使回执报告冲突，也不能弹出覆盖确认并继续旧的关闭意图。
#[test]
fn cancelled_conflict_does_not_continue_with_overwrite_confirmation() {
    let decision = save_failure_decision(true, true, false, true, false, false);

    assert_eq!(decision.action, EditorSaveFailureAction::PreserveWorkspace);
    assert!(!decision.arm_close_after_readback);
    assert!(decision.cancel_followup_intents);
}
