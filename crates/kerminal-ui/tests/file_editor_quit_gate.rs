//! 文件编辑器退出门禁的纯状态契约，不启动窗口或执行文件 I/O。
//!
//! @author kongweiguang

use kerminal_ui::features::editor::{
    EditorQuitGate, EditorSaveFailureAction, EditorSaveSuccessAction, editor_save_failure_action,
    editor_save_success_action, evaluate_editor_quit_gate,
};

/// 加载中必须阻止立即清理标签，直到加载成功或失败回调推进退出流程。
#[test]
fn loading_blocks_fast_quit() {
    assert_eq!(
        evaluate_editor_quit_gate(true, false, false, false),
        EditorQuitGate::WaitingForLoad
    );
}

/// 保存进行中不能绕过结果回调，以免关窗时丢失明确的提交结果。
#[test]
fn saving_waits_for_a_definite_result() {
    assert_eq!(
        evaluate_editor_quit_gate(false, true, false, false),
        EditorQuitGate::WaitingForSave
    );
}

/// 保存失败后仍保留脏正文，退出流程应回到逐文件确认而不是自动关闭。
#[test]
fn failed_save_keeps_dirty_content_on_the_confirmation_path() {
    assert_eq!(
        evaluate_editor_quit_gate(false, false, false, true),
        EditorQuitGate::ConfirmUnsaved
    );
}

/// 可继续确认的 revision 冲突保留关闭意图；其它退出保存失败清除 Root pending intent。
#[test]
fn save_failure_preserves_only_explicit_conflict_resolution_intent() {
    assert_eq!(
        editor_save_failure_action(false, false, false, true),
        EditorSaveFailureAction::CancelQuitIntent
    );
    assert_eq!(
        editor_save_failure_action(false, false, true, false),
        EditorSaveFailureAction::CancelQuitIntent
    );
    assert_eq!(
        editor_save_failure_action(false, false, false, false),
        EditorSaveFailureAction::PreserveWorkspace
    );
    assert_eq!(
        editor_save_failure_action(true, true, false, false),
        EditorSaveFailureAction::ConfirmOverwrite
    );
    assert_eq!(
        editor_save_failure_action(true, false, true, false),
        EditorSaveFailureAction::ConfirmOverwrite
    );
    assert_eq!(
        editor_save_failure_action(true, false, false, true),
        EditorSaveFailureAction::ConfirmOverwrite
    );
    assert_eq!(
        editor_save_failure_action(true, false, false, false),
        EditorSaveFailureAction::PreserveWorkspace
    );
}

/// 所有读取和保存均结束且没有脏正文时允许立即关闭工作区。
#[test]
fn clean_idle_workspace_can_quit_immediately() {
    assert_eq!(
        evaluate_editor_quit_gate(false, false, false, false),
        EditorQuitGate::Ready
    );
}

/// 即使本地缓冲已干净，未知提交也必须先显式读回，不能通过退出快路丢掉状态。
#[test]
fn unknown_save_result_requires_readback_before_quit() {
    assert_eq!(
        evaluate_editor_quit_gate(false, false, true, false),
        EditorQuitGate::ReadbackRequired
    );
    assert_eq!(
        evaluate_editor_quit_gate(true, false, true, false),
        EditorQuitGate::WaitingForLoad
    );
}

/// 保存中的后续编辑会转回关闭或退出确认，而不会随已提交快照一起清除。
#[test]
fn edits_made_during_save_require_a_fresh_close_decision() {
    assert_eq!(
        editor_save_success_action(true, false, false, true),
        EditorSaveSuccessAction::ConfirmClose
    );
    assert_eq!(
        editor_save_success_action(true, true, true, true),
        EditorSaveSuccessAction::ConfirmQuit
    );
    assert_eq!(
        editor_save_success_action(false, false, true, true),
        EditorSaveSuccessAction::ConfirmQuit
    );
}

/// 编辑缓冲未变化时仍按原有关闭或退出请求完成，普通保存始终留在当前标签。
#[test]
fn clean_save_completion_honors_the_requested_flow() {
    assert_eq!(
        editor_save_success_action(true, false, false, false),
        EditorSaveSuccessAction::Close
    );
    assert_eq!(
        editor_save_success_action(true, true, true, false),
        EditorSaveSuccessAction::CloseAndQuit
    );
    assert_eq!(
        editor_save_success_action(false, false, true, false),
        EditorSaveSuccessAction::ContinueQuit
    );
    assert_eq!(
        editor_save_success_action(false, false, false, false),
        EditorSaveSuccessAction::KeepOpen
    );
}
