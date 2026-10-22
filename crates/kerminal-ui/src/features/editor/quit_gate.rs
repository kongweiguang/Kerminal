//! 文件编辑器关闭流程共享的纯门禁决策。
//!
//! @author kongweiguang

/// 标识退出流程当前必须等待的唯一原因或下一步用户动作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorQuitGate {
    Ready,
    WaitingForLoad,
    WaitingForSave,
    ReadbackRequired,
    ConfirmUnsaved,
}

/// 保存结束后的动作保证仍在编辑的正文不会被“保存并关闭”一并丢弃。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorSaveSuccessAction {
    KeepOpen,
    ConfirmClose,
    ConfirmQuit,
    Close,
    CloseAndQuit,
    ContinueQuit,
}

/// 保存失败时说明应用应继续运行还是清理尚未完成的退出意图。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EditorSaveFailureAction {
    PreserveWorkspace,
    CancelQuitIntent,
    ConfirmOverwrite,
}

/// 先等待读取和保存，再完成未知提交读回，最后确认仍未保存的正文。
pub fn evaluate_editor_quit_gate(
    loading: bool,
    saving: bool,
    readback_required: bool,
    dirty: bool,
) -> EditorQuitGate {
    if loading {
        EditorQuitGate::WaitingForLoad
    } else if saving {
        EditorQuitGate::WaitingForSave
    } else if readback_required {
        EditorQuitGate::ReadbackRequired
    } else if dirty {
        EditorQuitGate::ConfirmUnsaved
    } else {
        EditorQuitGate::Ready
    }
}

/// 保存提交的是开始保存时的快照；若缓冲在等待期间变化，关闭必须回到用户确认。
pub fn editor_save_success_action(
    close_after_success: bool,
    quit_after_success: bool,
    quit_flow: bool,
    dirty_after_success: bool,
) -> EditorSaveSuccessAction {
    let should_quit = quit_after_success || quit_flow;
    if dirty_after_success {
        if should_quit {
            EditorSaveSuccessAction::ConfirmQuit
        } else if close_after_success {
            EditorSaveSuccessAction::ConfirmClose
        } else {
            EditorSaveSuccessAction::KeepOpen
        }
    } else if close_after_success && should_quit {
        EditorSaveSuccessAction::CloseAndQuit
    } else if close_after_success {
        EditorSaveSuccessAction::Close
    } else if should_quit {
        EditorSaveSuccessAction::ContinueQuit
    } else {
        EditorSaveSuccessAction::KeepOpen
    }
}

/// 普通失败取消未完成的退出请求；可显式解决的冲突保留关闭意图并进入覆盖确认。
pub fn editor_save_failure_action(
    conflict: bool,
    close_after_success: bool,
    quit_after_success: bool,
    quit_flow: bool,
) -> EditorSaveFailureAction {
    if conflict && (close_after_success || quit_after_success || quit_flow) {
        EditorSaveFailureAction::ConfirmOverwrite
    } else if quit_flow || quit_after_success {
        EditorSaveFailureAction::CancelQuitIntent
    } else {
        EditorSaveFailureAction::PreserveWorkspace
    }
}
