//! 固定片段运行弹层的取消、未知结果与迟到回执边界。
//!
//! @author kongweiguang

use crate::features::commands::{
    SnippetDraftValues,
    snippet_run::{
        SnippetPreviewIdentity, SnippetRunPhase, SnippetRunUiState, definition_matches_saved,
    },
};
use kerminal_application::{
    AppError,
    commands::{
        SnippetRunConfirmationInfo, SnippetRunHistory, SnippetRunResult, SnippetRunTargetSummary,
        SnippetRunUsage,
    },
    sessions::{ShellCommandUnknownReason, UiSessionBinding},
};
use kerminal_domain::{
    commands::{
        SnippetCatalogOrigin, SnippetContextBinding, SnippetContextBindingKind,
        SnippetDefaultAction, SnippetDefinition, SnippetPreferenceKey, SnippetRisk, SnippetScope,
    },
    identity::{PaneId, SessionId, TabId},
    preferences::ConfigRevision,
    sessions::SessionOwner,
};

/// 使用安全的最小目标快照构造器验证 UI 状态，不创建真实 session 或运行命令。
fn confirmation_info() -> SnippetRunConfirmationInfo {
    SnippetRunConfirmationInfo {
        key: SnippetPreferenceKey {
            origin: SnippetCatalogOrigin::Builtin,
            snippet_id: "inspection".to_owned(),
        },
        title: "只读检查".to_owned(),
        session_id: SessionId::parse("00000000-0000-4000-8000-000000000001")
            .expect("fixed session id is valid"),
        owner: SessionOwner::Mcp,
        risk: SnippetRisk::Inspect,
        sensitive: false,
        requires_confirmation: false,
        target: SnippetRunTargetSummary::Local,
        command_preview: "pwd".to_owned(),
    }
}

/// Builtin 的空 binding 与编辑器默认呈现的 Global(None) 等价，但真实范围变化仍须拦截。
///
/// @author kongweiguang
#[test]
fn run_definition_gate_normalizes_implicit_global_binding() {
    let saved = SnippetDefinition {
        id: "builtin-current-directory".to_owned(),
        title: "当前目录".to_owned(),
        command: "pwd && ls -la".to_owned(),
        description: Some("查看当前目录".to_owned()),
        tags: Vec::new(),
        sort_order: 0,
        scope: SnippetScope::Any,
        category: Some("检查".to_owned()),
        risk: SnippetRisk::Inspect,
        default_action: SnippetDefaultAction::Insert,
        variables: Vec::new(),
        context_bindings: Vec::new(),
        derived_from: None,
    };
    let mut draft = SnippetDraftValues {
        title: saved.title.clone(),
        command: saved.command.clone(),
        description: saved.description.clone(),
        tags: saved.tags.clone(),
        sort_order: saved.sort_order,
        scope: saved.scope,
        category: saved.category.clone(),
        risk: saved.risk,
        default_action: saved.default_action,
        variables: saved.variables.clone(),
        context_bindings: vec![SnippetContextBinding {
            kind: SnippetContextBindingKind::Global,
            target_id: None,
        }],
    };

    assert!(definition_matches_saved(&saved, &draft));

    draft.context_bindings = vec![SnippetContextBinding {
        kind: SnippetContextBindingKind::Host,
        target_id: Some("host-changed".to_owned()),
    }];
    assert!(!definition_matches_saved(&saved, &draft));
}

/// 关闭弹层只改变可见性，不取消仍在准备中的应用任务。
#[test]
fn closing_modal_does_not_cancel_preparation() {
    let mut state = SnippetRunUiState::default();
    let (generation, cancellation) = state.begin_preparation().expect("initial run can start");

    state.close_modal();

    assert_eq!(state.phase(), SnippetRunPhase::Preparing);
    assert!(state.is_busy());
    assert!(!state.is_open());
    assert!(!cancellation.is_cancelled());
    state.finish_cancelled(generation);
}

/// 只有相同来源 revision 和完整 UI binding 才能复用准备态预览，重连或改版后回退为模板。
///
/// @author kongweiguang
#[test]
fn inline_preview_identity_rejects_changed_revision_or_terminal_generation() {
    let key = SnippetPreferenceKey {
        origin: SnippetCatalogOrigin::User,
        snippet_id: "inspection".to_owned(),
    };
    let revision = ConfigRevision::from_sha256("revision-one".to_owned());
    let binding = UiSessionBinding {
        session_id: SessionId::parse("00000000-0000-4000-8000-000000000001")
            .expect("fixed session id is valid"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-main").expect("fixed tab id is valid"),
            pane_id: PaneId::parse("pane-main").expect("fixed pane id is valid"),
        },
        session_generation: 7,
    };
    let identity =
        SnippetPreviewIdentity::new(key.clone(), Some(revision.clone()), binding.clone());

    assert!(identity.matches(&key, Some(&revision), &binding));
    assert!(!identity.matches(
        &key,
        Some(&ConfigRevision::from_sha256("revision-two".to_owned())),
        &binding,
    ));
    let mut reconnected = binding.clone();
    reconnected.session_generation += 1;
    assert!(!identity.matches(&key, Some(&revision), &reconnected));
}

/// Unknown 不能被普通 Run 覆盖，只有用户明确回看原 session 后才能开启新代次。
#[test]
fn unknown_result_requires_explicit_terminal_review() {
    let mut state = SnippetRunUiState::default();
    let (generation, _) = state.begin_preparation().expect("initial run can start");
    let original_session = confirmation_info().session_id;
    assert!(state.begin_execution(generation, confirmation_info()));

    state.finish_result(
        generation,
        SnippetRunResult::Unknown {
            command_id: "command-receipt".to_owned(),
            reason: ShellCommandUnknownReason::Disconnected,
        },
    );

    assert_eq!(state.phase(), SnippetRunPhase::OutcomeUnknown);
    state.close_modal();
    assert!(state.recovery_visible());
    assert!(!state.can_start());
    assert!(!state.blocks_editing());
    assert_eq!(state.unknown_session_id(), Some(&original_session));
    assert!(state.acknowledge_unknown_after_review());
    assert_eq!(state.phase(), SnippetRunPhase::OutcomeReviewed);
    assert!(state.can_start());
    assert_eq!(state.unknown_session_id(), None);
}

/// 取消只发 token；在应用 future 返回明确取消前仍保持忙状态并拒绝下一次运行。
#[test]
fn cancellation_stays_busy_until_the_application_receipt_arrives() {
    let mut state = SnippetRunUiState::default();
    let (generation, cancellation) = state.begin_preparation().expect("initial run can start");
    assert!(state.begin_execution(generation, confirmation_info()));

    assert!(state.request_cancel());
    assert!(cancellation.is_cancelled());
    assert_eq!(state.phase(), SnippetRunPhase::CancellationRequested);
    assert!(state.is_busy());
    assert!(!state.can_start());

    state.finish_run_result(generation, Err(AppError::cancelled("test cancellation")));

    assert_eq!(state.phase(), SnippetRunPhase::Cancelled);
    assert!(!state.is_busy());
    assert!(state.can_start());
}

/// 旧代任务即使迟到，也不能覆盖后来已开始的运行状态。
#[test]
fn stale_run_receipt_cannot_replace_a_newer_generation() {
    let mut state = SnippetRunUiState::default();
    let (old_generation, _) = state.begin_preparation().expect("initial run can start");
    state.finish_cancelled(old_generation);
    let (new_generation, _) = state
        .begin_preparation()
        .expect("cancelled run is terminal");

    state.finish_result(
        old_generation,
        SnippetRunResult::Unknown {
            command_id: "late-command".to_owned(),
            reason: ShellCommandUnknownReason::MissingCompletion,
        },
    );

    assert_eq!(state.phase(), SnippetRunPhase::Preparing);
    assert!(state.is_busy());
    assert_ne!(new_generation, old_generation);
}

/// 完成、失败和取消均先留在固定恢复区，用户收起后才隐藏，不随 busy 状态结束而消失。
#[test]
fn terminal_run_outcomes_remain_visible_until_explicitly_closed() {
    let mut finished = SnippetRunUiState::default();
    let (generation, _) = finished.begin_preparation().expect("run can start");
    assert!(finished.begin_execution(generation, confirmation_info()));
    finished.finish_result(
        generation,
        SnippetRunResult::Finished {
            command_id: "finished-command".to_owned(),
            exit_code: 0,
            history: SnippetRunHistory::NotRequested,
            usage: SnippetRunUsage::NotRecorded {
                error_code: AppError::unsupported("usage is not needed for visibility")
                    .public_code(),
            },
        },
    );

    let mut failed = SnippetRunUiState::default();
    let (generation, _) = failed.begin_preparation().expect("run can start");
    assert!(failed.begin_execution(generation, confirmation_info()));
    failed.finish_error(
        generation,
        AppError::unsupported("controlled test failure"),
        true,
    );

    let mut cancelled = SnippetRunUiState::default();
    let (generation, _) = cancelled.begin_preparation().expect("run can start");
    assert!(cancelled.begin_execution(generation, confirmation_info()));
    cancelled.finish_error(
        generation,
        AppError::cancelled("controlled test cancellation"),
        true,
    );

    for state in [&mut finished, &mut failed, &mut cancelled] {
        assert!(!state.blocks_navigation());
        assert!(state.recovery_visible());
        state.close_modal();
        assert!(!state.recovery_visible());
    }
}
