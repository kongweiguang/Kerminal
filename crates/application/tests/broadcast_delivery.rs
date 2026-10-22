//! 广播交付回归通过 public CommandsService 验证逐目标写入与取消回执。
//!
//! @author kongweiguang

#[path = "support/broadcast_delivery.rs"]
mod support;

use std::sync::Arc;

use kerminal_application::{PublicErrorCode, commands::BroadcastExecutionResult};
use kerminal_domain::{
    commands::CommandHistoryTarget,
    identity::{PaneId, SessionId, TabId},
};
use support::{BroadcastSessions, WriteAction, commands_service, target_and_binding, ui_session};
use tokio_util::sync::CancellationToken;

/// 两个固定目标中一个写入结果未知时，保留已尝试状态并继续处理后续目标。
///
/// @author kongweiguang
#[tokio::test]
async fn broadcast_keeps_attempted_unknown_failure_and_delivers_later_target() {
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let sessions = Arc::new(BroadcastSessions::new(
        vec![
            ui_session(
                first_session_id.clone(),
                TabId::new(),
                first_pane_id.clone(),
            ),
            ui_session(
                second_session_id.clone(),
                TabId::new(),
                second_pane_id.clone(),
            ),
        ],
        [WriteAction::OutcomeUnknown, WriteAction::Succeed],
    ));
    let (first_target, first_binding) = target_and_binding(
        first_session_id.clone(),
        first_pane_id,
        CommandHistoryTarget::Local,
        "first",
    );
    let (second_target, second_binding) = target_and_binding(
        second_session_id.clone(),
        second_pane_id,
        CommandHistoryTarget::Local,
        "second",
    );

    let result = commands_service(Arc::clone(&sessions))
        .broadcast_with_cancellation(
            "echo delivery".to_owned(),
            vec![first_target, second_target],
            vec![first_binding, second_binding],
            true,
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await
        .expect("per-target failures are returned as delivery results");

    assert_delivery_count(&result, 2);
    assert_eq!(
        sessions.writes(),
        vec![first_session_id.clone(), second_session_id]
    );
    assert!(result.deliveries[0].attempted);
    assert!(!result.deliveries[0].sent);
    assert!(!result.deliveries[0].cancelled);
    assert_eq!(
        result.deliveries[0].error_code,
        Some(PublicErrorCode::OutcomeUnknown)
    );
    assert!(result.deliveries[1].attempted);
    assert!(result.deliveries[1].sent);
    assert!(!result.deliveries[1].cancelled);
    assert_eq!(result.deliveries[1].error_code, None);
}

/// 首个 writer 触发用户取消后，保留该次尝试并将后续目标标为未尝试取消。
///
/// @author kongweiguang
#[tokio::test]
async fn broadcast_cancellation_after_first_write_marks_remaining_target_unattempted() {
    let first_session_id = SessionId::new();
    let second_session_id = SessionId::new();
    let first_pane_id = PaneId::new();
    let second_pane_id = PaneId::new();
    let user_cancellation = CancellationToken::new();
    let sessions = Arc::new(BroadcastSessions::new(
        vec![
            ui_session(
                first_session_id.clone(),
                TabId::new(),
                first_pane_id.clone(),
            ),
            ui_session(
                second_session_id.clone(),
                TabId::new(),
                second_pane_id.clone(),
            ),
        ],
        [WriteAction::CancelThenSucceed(user_cancellation.clone())],
    ));
    let (first_target, first_binding) = target_and_binding(
        first_session_id.clone(),
        first_pane_id,
        CommandHistoryTarget::Local,
        "first",
    );
    let (second_target, second_binding) = target_and_binding(
        second_session_id.clone(),
        second_pane_id,
        CommandHistoryTarget::Local,
        "second",
    );

    let result = commands_service(Arc::clone(&sessions))
        .broadcast_with_cancellation(
            "echo cancel".to_owned(),
            vec![first_target, second_target],
            vec![first_binding, second_binding],
            true,
            user_cancellation,
            CancellationToken::new(),
        )
        .await
        .expect("cancellation retains the per-target result list");

    assert_delivery_count(&result, 2);
    assert_eq!(sessions.writes(), vec![first_session_id]);
    assert!(result.deliveries[0].attempted);
    assert!(result.deliveries[0].sent);
    assert!(!result.deliveries[0].cancelled);
    assert!(!result.deliveries[1].attempted);
    assert!(!result.deliveries[1].sent);
    assert!(result.deliveries[1].cancelled);
    assert_eq!(result.deliveries[1].error_code, None);
}

/// Pane binding 在写入前必须与分析目标一致；非法绑定不得触及任一 session writer。
///
/// @author kongweiguang
#[tokio::test]
async fn invalid_broadcast_binding_is_rejected_without_session_writes() {
    let session_id = SessionId::new();
    let target_pane_id = PaneId::new();
    let owner_pane_id = PaneId::new();
    let sessions = Arc::new(BroadcastSessions::new(
        vec![ui_session(
            session_id.clone(),
            TabId::new(),
            owner_pane_id.clone(),
        )],
        [WriteAction::Succeed],
    ));
    let (target, mut binding) = target_and_binding(
        session_id,
        target_pane_id,
        CommandHistoryTarget::Local,
        "invalid binding",
    );
    binding.pane_id = owner_pane_id;

    let result = commands_service(Arc::clone(&sessions))
        .broadcast_with_cancellation(
            "echo invalid".to_owned(),
            vec![target],
            vec![binding],
            true,
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await;

    assert!(result.is_err());
    assert!(sessions.writes().is_empty());
    assert_eq!(sessions.list_calls(), 0);
}

/// 结果条数必须与完整目标集合相同，避免错误或取消后丢失未尝试目标。
///
/// @author kongweiguang
fn assert_delivery_count(result: &BroadcastExecutionResult, expected: usize) {
    assert_eq!(result.deliveries.len(), expected);
}
