//! 广播交付回归通过 public CommandsService 验证逐目标写入与取消回执。
//!
//! @author kongweiguang

#[path = "support/broadcast_delivery.rs"]
mod support;

use std::sync::Arc;

use kerminal_application::{
    PublicErrorCode,
    commands::{
        BroadcastExecutionResult, SnippetExecutionService, SnippetRenderResult, SnippetRunHistory,
    },
    sessions::UiSessionBinding,
};
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

/// 片段经群发仍须把首尾空白和换行原样交给 UI writer；trim 仅用于拒绝空白正文，敏感正文不进历史。
///
/// @author kongweiguang
#[tokio::test]
async fn sensitive_snippet_uses_direct_ui_writer_without_history_or_shell_gate() {
    let session_id = SessionId::new();
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let sessions = Arc::new(BroadcastSessions::new(
        vec![ui_session(
            session_id.clone(),
            tab_id.clone(),
            pane_id.clone(),
        )],
        [WriteAction::Succeed],
    ));
    let (target, _) = target_and_binding(
        session_id.clone(),
        pane_id.clone(),
        CommandHistoryTarget::Local,
        "local unknown shell",
    );
    let binding = UiSessionBinding {
        session_id: session_id.clone(),
        owner: kerminal_domain::sessions::SessionOwner::Ui { tab_id, pane_id },
        session_generation: 7,
    };
    let snippet_command = "\n  echo secret \t\r\n ".to_owned();

    let result = commands_service(Arc::clone(&sessions))
        .broadcast_snippet_with_cancellation(
            SnippetRenderResult {
                command: snippet_command.clone(),
                sensitive: true,
            },
            vec![target.clone()],
            vec![binding.clone()],
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await
        .expect("the explicit click is the send action; shell readiness is not required");

    assert_eq!(sessions.writes(), vec![session_id.clone()]);
    assert_eq!(
        sessions.snippet_submissions(),
        vec![
            kerminal_application::sessions::UiSnippetCommandLineRequest {
                binding: binding.clone(),
                command: snippet_command,
            }
        ]
    );
    assert_eq!(sessions.history_writes(), 0);
    assert!(result.deliveries[0].sent);
    assert_eq!(result.deliveries[0].history_recorded, Some(false));
    assert_eq!(
        result.deliveries[0].history_skip_reason.as_deref(),
        Some("敏感片段不会写入命令历史")
    );

    let rejected = commands_service(Arc::clone(&sessions))
        .broadcast_snippet_with_cancellation(
            SnippetRenderResult {
                command: " \r\n\t ".to_owned(),
                sensitive: true,
            },
            vec![target],
            vec![binding],
            CancellationToken::new(),
            CancellationToken::new(),
        )
        .await
        .expect_err("whitespace-only snippets remain unsendable");

    assert_eq!(rejected.public_code(), PublicErrorCode::Invalid);
    assert_eq!(sessions.writes(), vec![session_id]);
    assert_eq!(sessions.snippet_submissions().len(), 1);
}

/// 右栏直发同样只提交一次固定目标；没有目录或 prompt 能力也能发送，未知回执不重试。
///
/// @author kongweiguang
#[tokio::test]
async fn sidebar_snippet_direct_send_uses_one_writer_and_preserves_unknown_result() {
    for (action, expected_unknown) in [
        (WriteAction::Succeed, false),
        (WriteAction::OutcomeUnknown, true),
    ] {
        let session_id = SessionId::new();
        let tab_id = TabId::new();
        let pane_id = PaneId::new();
        let sessions = Arc::new(BroadcastSessions::new(
            vec![ui_session(
                session_id.clone(),
                tab_id.clone(),
                pane_id.clone(),
            )],
            [action],
        ));
        let commands = Arc::new(commands_service(sessions.clone()));
        let service = SnippetExecutionService::new(commands, sessions.clone());
        let result = service
            .send_direct(
                SnippetRenderResult {
                    command: "echo one\necho two".to_owned(),
                    sensitive: true,
                },
                UiSessionBinding {
                    session_id: session_id.clone(),
                    owner: kerminal_domain::sessions::SessionOwner::Ui { tab_id, pane_id },
                    session_generation: 7,
                },
                true,
                CancellationToken::new(),
            )
            .await;

        if expected_unknown {
            assert_eq!(
                result
                    .expect_err("unknown write stays unknown")
                    .public_code(),
                PublicErrorCode::OutcomeUnknown
            );
        } else {
            assert_eq!(
                result.expect("unknown shell does not block a direct send"),
                SnippetRunHistory::SkippedSensitive
            );
        }
        assert_eq!(sessions.writes(), vec![session_id]);
        assert_eq!(sessions.snippet_submissions().len(), 1);
        assert_eq!(sessions.history_writes(), 0);
    }
}

/// 结果条数必须与完整目标集合相同，避免错误或取消后丢失未尝试目标。
///
/// @author kongweiguang
fn assert_delivery_count(result: &BroadcastExecutionResult, expected: usize) {
    assert_eq!(result.deliveries.len(), expected);
}
