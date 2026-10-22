//! 终端工具异步回执的来源与代次边界测试。
//!
//! @author kongweiguang

use super::*;

/// 具备稳定 pane/session 和实体身份的请求 owner fixture。
fn owner(source: u64, pane_generation: u64) -> TerminalToolsOwner {
    TerminalToolsOwner {
        source_id: EntityId::from(source),
        session_id: SessionId::parse("session-a").expect("valid fixture session id"),
        scope: TerminalToolsOwnerScope::Workspace {
            pane_id: PaneId::parse("pane-a").expect("valid fixture pane id"),
            generation: pane_generation,
        },
    }
}

/// 新搜索请求必须淘汰旧 requestId；迟到的命中不能再通过 owner 检查。
///
/// @author kongweiguang
#[test]
fn replacement_search_rejects_stale_request_id() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(1, 3);
    let first = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Search,
            Some(41),
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("first search begins");
    let first_cancellation = runtime
        .cancellation_token(&first)
        .expect("search carries a cancellation token");
    let first_commit_gate = runtime
        .search_commit_gate(&first)
        .expect("search carries a viewport commit gate");
    let second = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Search,
            Some(42),
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("a new search replaces a cancellable request");

    assert!(!runtime.is_current(&first));
    assert!(runtime.is_current(&second));
    assert!(first_cancellation.is_cancelled());
    assert!(first_commit_gate.is_cancelled());
    assert!(
        !runtime
            .cancellation_token(&second)
            .expect("replacement search carries its own token")
            .is_cancelled()
    );
    assert!(!runtime.is_search_current(&owner, 41));
    assert!(runtime.is_search_current(&owner, 42));
}

/// 清除或关闭搜索先关闭提交门再取消扫描，迟到查询不再占用 session 引擎锁或滚动。
///
/// @author kongweiguang
#[test]
fn cancel_search_signals_backend_cancellation() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(2, 1);
    let request = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Search,
            Some(7),
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("search begins");
    let cancellation = runtime
        .cancellation_token(&request)
        .expect("search carries a cancellation token");
    let commit_gate = runtime
        .search_commit_gate(&request)
        .expect("search carries a viewport commit gate");

    runtime.cancel(&owner, TerminalToolsRequestKind::Search);

    assert!(cancellation.is_cancelled());
    assert!(commit_gate.is_cancelled());
    assert!(!commit_gate.try_commit());
    assert!(!runtime.is_current(&request));
}

/// 取消与导航只允许一个终态先提交，避免关闭和迟到搜索同时改变 viewport。
///
/// @author kongweiguang
#[test]
fn search_commit_gate_linearizes_cancel_and_reveal() {
    let cancelled = UiTerminalSearchCommitGate::new();
    assert!(cancelled.cancel());
    assert!(cancelled.is_cancelled());
    assert!(!cancelled.try_commit());

    let committed = UiTerminalSearchCommitGate::new();
    assert!(committed.try_commit());
    assert!(!committed.cancel());
    assert!(!committed.is_cancelled());
}

/// Copy 读取前必须看到最后一次 SetSelection 的 service ACK，替换旧拖选则唤醒并拒绝旧等待者。
#[test]
fn selection_copy_gate_waits_for_latest_ack() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(12, 2);
    let first = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("selection begins");
    let first_ack = runtime
        .selection_ack(&owner)
        .expect("selection exposes its completion gate");
    assert_eq!(first_ack.result(), None);
    assert!(runtime.has_pending_selection(&owner));

    runtime.finish_selection(&first, true);
    assert_eq!(first_ack.result(), Some(true));
    assert!(!runtime.has_pending_selection(&owner));

    let stale = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("next drag endpoint replaces the previous one");
    let stale_ack = runtime
        .selection_ack(&owner)
        .expect("replacement owns a fresh completion gate");
    assert_eq!(stale_ack.result(), None);
    assert!(runtime.has_pending_selection(&owner));

    let newest = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("latest endpoint supersedes the stale request");
    assert_eq!(stale_ack.result(), Some(false));
    let newest_ack = runtime.selection_ack(&owner).expect("new gate");
    assert_eq!(newest_ack.result(), None);
    runtime.finish_selection(&newest, true);
    assert_eq!(newest_ack.result(), Some(true));
    assert!(!runtime.is_current(&stale));
}

/// 最终选择服务回执到快照回投之间继续保留可复制标记，避免 Ctrl+C 被误判为中断。
///
/// @author kongweiguang
#[test]
fn confirmed_selection_survives_ack_until_snapshot_reconciles_it() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(17, 2);
    runtime.set_confirmed_selection_present(&owner, true);
    let request = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("selection begins");

    assert!(runtime.selection_may_be_present(&owner));
    runtime.finish_selection(&request, true);

    assert!(!runtime.has_pending_selection(&owner));
    assert!(runtime.selection_may_be_present(&owner));
    runtime.set_confirmed_selection_present(&owner, false);
    assert!(!runtime.selection_may_be_present(&owner));
}

/// Anchor ACK 与 Move request 分离后保持可用；新手势取消旧 ID，Cancel 只撤销当前 ID。
#[test]
fn selection_anchor_ack_is_stable_across_moves_and_scoped_to_gesture() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(13, 4);
    let anchor = runtime
        .begin_with_gesture_id(
            owner.clone(),
            TerminalToolsRequestKind::SelectionAnchor,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
            Some(77),
        )
        .expect("mouse down anchor begins");
    let anchor_ack = runtime
        .selection_anchor_ack(&owner, 77)
        .expect("matching gesture can wait for its anchor");
    let anchor_cancellation = runtime
        .cancellation_token(&anchor)
        .expect("anchor has a cancellation token");
    runtime.finish_selection(&anchor, true);

    let first_move = runtime
        .begin_with_gesture_id(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
            Some(77),
        )
        .expect("first move begins");
    assert_eq!(
        runtime.selection_ack(&owner).and_then(|ack| ack.result()),
        None
    );
    let second_move = runtime
        .begin_with_gesture_id(
            owner.clone(),
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
            Some(77),
        )
        .expect("new move replaces the pending move");
    let second_move_cancellation = runtime
        .cancellation_token(&second_move)
        .expect("move has a cancellation token");
    assert_eq!(
        runtime.selection_ack(&owner).and_then(|ack| ack.result()),
        None
    );
    assert!(!runtime.is_current(&first_move));
    assert!(!anchor_cancellation.is_cancelled());
    assert_eq!(anchor_ack.result(), Some(true));
    assert_eq!(
        runtime
            .selection_anchor_ack(&owner, 77)
            .and_then(|ack| ack.result()),
        Some(true)
    );

    let next_anchor = runtime
        .begin_with_gesture_id(
            owner.clone(),
            TerminalToolsRequestKind::SelectionAnchor,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
            Some(88),
        )
        .expect("a later mouse down replaces the previous anchor");
    assert!(runtime.selection_anchor_ack(&owner, 77).is_none());
    assert!(!runtime.is_current(&second_move));
    assert!(second_move_cancellation.is_cancelled());
    assert_eq!(
        runtime
            .selection_anchor_ack(&owner, 88)
            .and_then(|ack| ack.result()),
        None
    );
    runtime.cancel_selection_gesture(&owner, 88);
    assert!(!runtime.is_current(&next_anchor));
    assert!(runtime.selection_anchor_ack(&owner, 88).is_none());
}

/// 同一实体被 Pane 新 generation 复用时，旧资源回执必须失效。
#[test]
fn reused_tool_entity_rejects_old_pane_generation() {
    let mut runtime = TerminalToolsRuntime::default();
    let old_owner = owner(7, 2);
    let new_owner = owner(7, 3);
    let old = runtime
        .begin(
            old_owner,
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("selection begins");
    let new = runtime
        .begin(
            new_owner,
            TerminalToolsRequestKind::Selection,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("rebound tool view accepts a new generation");

    assert!(!runtime.is_current(&old));
    assert!(runtime.is_current(&new));
}

/// 日志写入请求不能因重复动作被 drop；收到完成回执前只允许一项状态变更。
#[test]
fn log_mutation_rejects_duplicate_until_completion() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(9, 1);
    let first = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::LogMutation,
            None,
            TerminalToolsRequestPolicy::RejectWhileRunning,
        )
        .expect("log mutation begins");

    assert!(
        runtime
            .begin(
                owner.clone(),
                TerminalToolsRequestKind::LogMutation,
                None,
                TerminalToolsRequestPolicy::RejectWhileRunning,
            )
            .is_none()
    );
    runtime.finish(&first);
    assert!(
        runtime
            .begin(
                owner,
                TerminalToolsRequestKind::LogMutation,
                None,
                TerminalToolsRequestPolicy::RejectWhileRunning,
            )
            .is_some()
    );
}

/// 关闭 session 取消可读操作，但状态型日志写入仍保留结果所有者。
#[test]
fn session_cleanup_cancels_reads_and_preserves_log_receipt_owner() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(10, 4);
    let search = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::Search,
            Some(2),
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("search begins");
    let log = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::LogMutation,
            None,
            TerminalToolsRequestPolicy::RejectWhileRunning,
        )
        .expect("log write begins");

    runtime.cancel_session(&owner.session_id);

    assert!(!runtime.is_current(&search));
    assert!(runtime.is_current(&log));
}

/// 待写入日志状态的请求不能与旧状态读取并行，避免旧读回覆盖新状态。
#[test]
fn log_mutation_has_priority_over_state_refresh() {
    let mut runtime = TerminalToolsRuntime::default();
    let owner = owner(11, 5);
    let refresh = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::LogRead,
            None,
            TerminalToolsRequestPolicy::ReplaceCancellable,
        )
        .expect("refresh begins");

    runtime.cancel(&owner, TerminalToolsRequestKind::LogRead);
    let mutation = runtime
        .begin(
            owner.clone(),
            TerminalToolsRequestKind::LogMutation,
            None,
            TerminalToolsRequestPolicy::RejectWhileRunning,
        )
        .expect("mutation begins after refresh is invalidated");

    assert!(!runtime.is_current(&refresh));
    assert!(runtime.is_running(&owner, TerminalToolsRequestKind::LogMutation));
    runtime.finish(&mutation);
    assert!(!runtime.is_running(&owner, TerminalToolsRequestKind::LogMutation));
}
