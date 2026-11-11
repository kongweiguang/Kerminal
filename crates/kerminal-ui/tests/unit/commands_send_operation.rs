//! 发送状态回归覆盖操作启动、目标切换、旧回执和新操作的完整状态迁移；
//! 另覆盖读取回执只结算自己写过的状态条，不能抹掉用户刚看到的目标切换或操作结果。
//!
//! @author kongweiguang

use super::send_state::{SendOperationKind, SendOperationState};
use super::{ReadStatusSlot, settle_panel_load_status};

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

/// 回归：set_session 写入“旧命令结果可能未知、草稿已保留”后立即 reload_panel，
/// 整页读取成功不能把它覆盖成 None 或配置冲突，否则用户看不到旧目标风险。
///
/// @author kongweiguang
#[test]
fn panel_load_success_keeps_the_target_switch_warning() {
    let mut status = Some("终端目标已切换；旧命令结果可能未知，草稿已保留".to_owned());
    let mut slot = ReadStatusSlot::default();

    settle_panel_load_status(&mut status, Vec::new(), None, &mut slot);

    assert_eq!(
        status.as_deref(),
        Some("终端目标已切换；旧命令结果可能未知，草稿已保留"),
        "a successful read must not erase the warning the switch just published"
    );
}

/// 新交互接管状态条后，先前读取的迟到成功回执不能再清除它；A→B→A 也按此规则。
///
/// @author kongweiguang
#[test]
fn late_panel_success_does_not_clear_a_newer_operation_status() {
    let mut status = None;
    let mut slot = ReadStatusSlot::default();
    // 第一次读取先发布失败并取得所有权。
    settle_panel_load_status(&mut status, vec!["读取失败".to_owned()], None, &mut slot);
    assert_eq!(status.as_deref(), Some("读取失败"));

    // 目标切换 A→B 接管状态条，随后的 B→A 又写入新的切换提示。
    status = Some("终端目标已切换，旧广播已请求取消".to_owned());
    status = Some("终端目标已切换；旧命令结果可能未知，草稿已保留".to_owned());
    settle_panel_load_status(&mut status, Vec::new(), None, &mut slot);
    assert_eq!(
        status.as_deref(),
        Some("终端目标已切换；旧命令结果可能未知，草稿已保留"),
        "neither read may overwrite the newest switch feedback"
    );
}

/// 本读取自己的失败提示可在重试成功后清除；清除后不能连带清掉配置冲突回退提示。
///
/// @author kongweiguang
#[test]
fn panel_retry_clears_only_its_own_failure_and_restores_the_conflict_fallback() {
    let mut status = None;
    let mut slot = ReadStatusSlot::default();
    settle_panel_load_status(&mut status, vec!["读取失败".to_owned()], None, &mut slot);
    settle_panel_load_status(&mut status, Vec::new(), None, &mut slot);
    assert_eq!(status, None, "the retry owns the failure it replaces");

    settle_panel_load_status(
        &mut status,
        Vec::new(),
        Some("配置冲突".to_owned()),
        &mut slot,
    );
    assert_eq!(
        status.as_deref(),
        Some("配置冲突"),
        "a clean load must still surface the panel's own conflict notice"
    );
    // 冲突提示由本读取回退写入，后续成功可再次保持它而不是清空。
    settle_panel_load_status(
        &mut status,
        Vec::new(),
        Some("配置冲突".to_owned()),
        &mut slot,
    );
    assert_eq!(status.as_deref(), Some("配置冲突"));
}

/// 读取失败属于当前目标的真实结果，必须覆盖并接管切换警告；之后的成功也不能复活旧警告。
///
/// @author kongweiguang
#[test]
fn panel_failure_replaces_the_switch_warning_and_keeps_ownership() {
    let mut status = Some("终端目标已切换；旧命令结果可能未知".to_owned());
    let mut slot = ReadStatusSlot::default();
    settle_panel_load_status(
        &mut status,
        vec!["历史读取失败".to_owned()],
        None,
        &mut slot,
    );
    assert_eq!(status.as_deref(), Some("历史读取失败"));

    settle_panel_load_status(&mut status, Vec::new(), None, &mut slot);
    assert_eq!(
        status, None,
        "after the failure is retried away the stale switch warning must not return"
    );
}

/// 历史读取成功只结算自己写过的提示；发送、广播、工作流或删除/清理刚写入的确认必须保留。
///
/// @author kongweiguang
#[test]
fn history_read_success_preserves_send_and_broadcast_confirmations() {
    for confirmation in [
        "命令已提交；终端退出状态未跟踪",
        "广播结束：1 个已发送，0 个失败",
        "工作流已完成",
        "历史记录已删除",
        "已清理 3 条历史记录",
    ] {
        let mut status = Some(confirmation.to_owned());
        let mut slot = ReadStatusSlot::default();
        slot.settle(&mut status, None);
        assert_eq!(
            status.as_deref(),
            Some(confirmation),
            "a history refresh must not hide the operation result that triggered it"
        );
    }
}

/// 历史读取自己的失败仍覆盖显示；重试成功后清除该失败但保留其后新写入的操作结果。
///
/// @author kongweiguang
#[test]
fn history_read_settles_its_own_failure_only() {
    let mut status = None;
    let mut slot = ReadStatusSlot::default();
    slot.publish(&mut status, "命令历史查询失败".to_owned());
    assert_eq!(status.as_deref(), Some("命令历史查询失败"));

    slot.settle(&mut status, None);
    assert_eq!(status, None);

    status = Some("历史记录已删除".to_owned());
    slot.settle(&mut status, None);
    assert_eq!(status.as_deref(), Some("历史记录已删除"));
}

/// 用户主动搜索仍是新交互，允许替换旧提示；自动回读不能借同一成功路径清空目标切换警告。
///
/// @author kongweiguang
#[test]
fn explicit_history_search_replaces_status_but_automatic_reload_does_not() {
    let mut status = Some("终端目标已切换；旧命令结果可能未知".to_owned());
    let mut slot = ReadStatusSlot::default();
    slot.replace(&mut status);
    assert_eq!(
        status, None,
        "a user-initiated search may dismiss the previous prompt"
    );

    let mut switch_status = Some("终端目标已切换；旧命令结果可能未知".to_owned());
    let mut settle_slot = ReadStatusSlot::default();
    settle_slot.settle(&mut switch_status, None);
    assert_eq!(
        switch_status.as_deref(),
        Some("终端目标已切换；旧命令结果可能未知"),
        "automatic reloads must keep the switch warning visible"
    );
}

/// 整页读取成功只保持配置冲突回退；配置冲突属于面板自身提示，不能被读取成功清空。
///
/// @author kongweiguang
#[test]
fn panel_conflict_fallback_is_not_cleared_by_a_clean_read() {
    let mut status = Some("配置冲突".to_owned());
    let mut slot = ReadStatusSlot::default();
    slot.owned = Some("配置冲突".to_owned());

    settle_panel_load_status(
        &mut status,
        Vec::new(),
        Some("配置冲突".to_owned()),
        &mut slot,
    );

    assert_eq!(
        status.as_deref(),
        Some("配置冲突"),
        "a clean read must keep the panel's own conflict notice"
    );
}
