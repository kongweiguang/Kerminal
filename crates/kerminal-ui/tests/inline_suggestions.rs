//! Inline suggestion controller generation, safety, and replacement tests.
//!
//! @author kongweiguang

use kerminal_application::{
    commands::TerminalSuggestionBatch,
    sessions::{
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandContextVersion,
        TerminalCommandUnavailableReason, UiSessionBinding,
    },
};
use kerminal_domain::{
    commands::{
        CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionReplacementRange,
        CommandSuggestionSensitivity, SuggestionPresentation, SuggestionProviderKind,
        SuggestionQueryMode,
    },
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::input::{KeyPhase, TerminalInputEvent, TerminalKey};
use kerminal_ui::features::commands::inline_suggestions::InlineSuggestionController;

/// 构造只包含 runtime 可验证数据的 UI owner context，避免测试用 screen text 伪造 prompt。
fn available_context(
    session_name: &str,
    session_generation: u64,
    prompt_epoch: u64,
    input_generation: u64,
) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: SessionId::parse(session_name).expect("valid session ID"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-main").expect("valid tab ID"),
            pane_id: PaneId::parse("pane-main").expect("valid pane ID"),
        },
        session_generation,
        prompt_epoch,
        input_generation,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    }
}

/// 创建允许 inline 和 menu 展示的普通建议，测试可聚焦 controller 而非 provider 排序。
fn candidate() -> CommandSuggestionCandidate {
    CommandSuggestionCandidate {
        id: "spec:git:checkout".to_owned(),
        provider: SuggestionProviderKind::Spec,
        activation: CommandSuggestionActivation::Insert,
        display_text: "checkout".to_owned(),
        replacement_text: "checkout".to_owned(),
        replacement_range: CommandSuggestionReplacementRange { start: 4, end: 7 },
        suffix: "ckout".to_owned(),
        score: 0.9,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: None,
        merged_source_explanations: Vec::new(),
        source_id: Some("git".to_owned()),
        allowed_presentations: vec![SuggestionPresentation::Inline, SuggestionPresentation::Menu],
        accept_boundaries: vec![4, 8],
        context_key: Some("current-query".to_owned()),
    }
}

/// 绑定版本中的全部三个 generation，迟到 batch 不覆盖新输入或重连后的同 ID 会话。
fn batch(
    query: &kerminal_application::commands::TerminalSuggestionQuery,
    candidates: Vec<CommandSuggestionCandidate>,
) -> TerminalSuggestionBatch {
    TerminalSuggestionBatch {
        binding: query.binding.clone(),
        expected_context: query.expected_context,
        request_generation: query.request_generation,
        candidates,
    }
}

/// 逐步接受沿候选声明的 Unicode boundary 替换文本，输入 suffix 和光标位置保持正确。
#[test]
fn partial_accept_builds_a_versioned_edit_plan() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(
            &context,
            "git che --force".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt should allow a suggestion query");
    assert!(controller.apply_batch(batch(&query, vec![candidate()])));

    let acceptance = controller
        .accept("spec:git:checkout", true)
        .expect("current candidate should be accepted");

    assert_eq!(acceptance.binding.session_id, context.session_id);
    assert_eq!(acceptance.expected_context.session_generation, 3);
    assert_eq!(acceptance.expected_context.prompt_epoch, 12);
    assert_eq!(acceptance.expected_context.input_generation, 4);
    assert_eq!(acceptance.replacement_text, "chec");
    assert_eq!(acceptance.input_after, "git chec --force");
    assert_eq!(acceptance.cursor_after, 8);
    assert_eq!(acceptance.events.len(), 4);
    assert!(acceptance.events[..3].iter().all(|event| matches!(
        event,
        TerminalInputEvent::Key(key)
            if key.key == TerminalKey::Backspace && key.phase == KeyPhase::Press
    )));
    assert_eq!(
        acceptance.events[3],
        TerminalInputEvent::Paste("chec".to_owned())
    );
    assert!(controller.accept("spec:git:checkout", false).is_none());
}

/// 新输入代次取消旧查询，并拒绝该查询之后才返回的 provider batch。
#[test]
fn newer_input_cancels_and_rejects_stale_batch() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (old_query, old_cancellation) = controller
        .update_input(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt should allow a query");
    let late_batch = batch(&old_query, vec![candidate()]);

    let mut next_context = context;
    next_context.input_generation += 1;
    let (current_query, _) = controller
        .update_input(
            &next_context,
            "git check".to_owned(),
            9,
            SuggestionQueryMode::Inline,
        )
        .expect("tracked input should keep the prompt available");

    assert!(old_cancellation.is_cancelled());
    assert!(!controller.apply_batch(late_batch));
    assert!(controller.apply_batch(batch(&current_query, vec![candidate()])));
}

/// session 切换、raw 输入或不可信 prompt 状态清除所有候选与临时输入。
#[test]
fn unavailable_context_cannot_accept_a_previous_candidate() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt should allow a query");
    assert!(controller.apply_batch(batch(&query, vec![candidate()])));

    let mut unavailable = context;
    unavailable.input_generation += 1;
    unavailable.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::ExternalInputObserved,
    );
    assert!(!controller.observe_context(&unavailable));

    assert!(controller.candidates().is_empty());
    assert!(controller.accept("spec:git:checkout", false).is_none());
}

/// session_generation 变化会隔离相同 SessionId 的重连结果。
#[test]
fn reconnect_generation_invalidates_a_batch() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt should allow a query");
    let late_batch = batch(&query, vec![candidate()]);

    let reconnected = available_context("session-test", 4, 12, 4);
    assert!(controller.observe_context(&reconnected));
    assert!(!controller.apply_batch(late_batch));
    assert!(controller.accept("spec:git:checkout", false).is_none());
}

/// 不把输入缓冲光标之外的控制字符或超长候选转成可写终端事件。
#[test]
fn invalid_input_and_control_candidate_are_rejected() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    assert!(
        controller
            .update_input(
                &context,
                "git\ncheckout".to_owned(),
                12,
                SuggestionQueryMode::Inline,
            )
            .is_none()
    );

    let (query, _) = controller
        .update_input(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("valid input can be queried");
    let mut unsafe_candidate = candidate();
    unsafe_candidate.replacement_text = "checkout\r\n".to_owned();
    assert!(controller.apply_batch(batch(&query, vec![unsafe_candidate])));
    assert!(controller.candidates().is_empty());
}

/// 即使 provider 的展示白名单有误，危险候选也只能出现在用户主动打开的菜单。
#[test]
fn dangerous_candidate_is_never_inline() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt should allow a query");
    let mut dangerous = candidate();
    dangerous.sensitivity = CommandSuggestionSensitivity::Dangerous;
    assert!(controller.apply_batch(batch(&query, vec![dangerous])));
    assert!(controller.candidates().is_empty());
}

/// 建议范围位于当前光标之前时先移动到 range 末尾，再替换并保留后缀。
#[test]
fn replacement_moves_to_range_end_and_preserves_suffix() {
    let context = available_context("session-test", 3, 12, 4);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(
            &context,
            "git che --force".to_owned(),
            9,
            SuggestionQueryMode::Menu,
        )
        .expect("trusted prompt should allow a query");
    assert!(controller.apply_batch(batch(&query, vec![candidate()])));

    let acceptance = controller
        .accept("spec:git:checkout", false)
        .expect("current candidate should be accepted");

    assert_eq!(acceptance.input_after, "git checkout --force");
    assert_eq!(acceptance.cursor_after, 12);
    assert!(matches!(
        acceptance.events.first(),
        Some(TerminalInputEvent::Key(key)) if key.key == TerminalKey::Left
    ));
    assert!(matches!(
        acceptance.events.get(1),
        Some(TerminalInputEvent::Key(key)) if key.key == TerminalKey::Left
    ));
}

/// 建议查询使用的 context version 与终端 UI 写请求共享同一字段顺序和值类型。
#[test]
fn binding_context_version_is_the_acceptance_write_version() {
    let context = available_context("session-test", 7, 5, 9);
    let mut controller = InlineSuggestionController::new();
    let (query, _) = controller
        .update_input(&context, "git che".to_owned(), 7, SuggestionQueryMode::Menu)
        .expect("trusted prompt should allow a query");
    assert_eq!(
        query.expected_context,
        TerminalCommandContextVersion {
            session_generation: 7,
            prompt_epoch: 5,
            input_generation: 9,
        }
    );
    assert_eq!(
        query.binding,
        UiSessionBinding {
            session_id: context.session_id,
            owner: context.owner,
            session_generation: 7,
        }
    );
}
