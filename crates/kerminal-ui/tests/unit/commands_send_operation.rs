//! 发送状态回归覆盖操作启动、目标切换、旧回执和新操作的完整状态迁移。
//!
//! @author kongweiguang

use super::send_state::{SendOperationKind, SendOperationState};

/// session 切换使旧令牌失效；其迟到完成不能解除新命令的 busy 状态。
///
/// @author kongweiguang
#[test]
fn stale_completion_after_session_switch_keeps_the_new_send_active() {
    let mut state = SendOperationState::default();
    let old_command = state
        .begin(SendOperationKind::Command)
        .expect("the first command should acquire the send slot");

    state.invalidate();
    let new_command = state
        .begin(SendOperationKind::Command)
        .expect("the new session should accept another command");

    assert!(!state.finish(old_command));
    assert!(state.active.is_some());
    assert!(state.finish(new_command));
    assert!(state.active.is_none());
}

/// 同一会话重连时保留在途写入锁；旧回执先收敛，随后新 generation 才能提交。
///
/// @author kongweiguang
#[test]
fn reconnect_keeps_the_old_write_busy_until_its_receipt() {
    let mut state = SendOperationState::default();
    let old_command = state
        .begin(SendOperationKind::Command)
        .expect("the command should acquire the send slot");

    assert!(state.begin(SendOperationKind::Command).is_none());
    assert!(state.finish(old_command));
    let reconnected_command = state
        .begin(SendOperationKind::Command)
        .expect("a new binding can submit after the old writer receipt converges");
    assert!(state.finish(reconnected_command));
}

/// 普通命令和广播不可并发，避免任一回执隐藏另一操作的忙状态或取消入口。
///
/// @author kongweiguang
#[test]
fn command_and_broadcast_cannot_own_the_send_slot_together() {
    let mut state = SendOperationState::default();
    let command = state
        .begin(SendOperationKind::Command)
        .expect("the command should acquire an idle send slot");

    assert!(state.begin(SendOperationKind::Broadcast).is_none());
    assert!(state.finish(command));

    let broadcast = state
        .begin(SendOperationKind::Broadcast)
        .expect("broadcast should start after the command receipt");
    assert!(state.begin(SendOperationKind::Command).is_none());
    assert!(state.finish(broadcast));
    assert!(state.active.is_none());
}
