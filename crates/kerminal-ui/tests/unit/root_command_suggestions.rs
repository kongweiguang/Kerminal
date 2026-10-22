//! Root command-suggestion bridge cancellation, filtering, ghost, and CAS contract tests.
//!
//! @author kongweiguang

use kerminal_application::{
    commands::{TerminalSuggestionBatch, TerminalSuggestionQuery},
    preferences::{
        TerminalCommandSuggestionPresentation, TerminalInlineSuggestionAcceptKey,
        TerminalInlineSuggestionSettings,
    },
    sessions::{
        TerminalCommandAvailability, TerminalCommandContext, TerminalCommandUnavailableReason,
        UiInputProjection,
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
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};

use super::{
    AcceptanceInputProgress, TerminalCommandSuggestionBridge, TerminalSuggestionQueryState,
};

/// 使用可信 OSC context 创建测试目标，避免屏幕文字充当 shell prompt 证据。
fn available_context(
    session_name: &str,
    session_generation: u64,
    prompt_epoch: u64,
    input_generation: u64,
) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: SessionId::parse(session_name).expect("valid session id"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-main").expect("valid tab id"),
            pane_id: PaneId::parse("pane-main").expect("valid pane id"),
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

/// 创建可 inline 插入的普通候选，测试关注 bridge 是否遵守 controller 的安全过滤。
fn candidate(query: &TerminalSuggestionQuery) -> CommandSuggestionCandidate {
    let typed_length = query.cursor.saturating_sub(4).min(8);
    let replacement = "checkout";
    CommandSuggestionCandidate {
        id: "spec:git:checkout".to_owned(),
        provider: SuggestionProviderKind::Spec,
        activation: CommandSuggestionActivation::Insert,
        display_text: "checkout".to_owned(),
        replacement_text: replacement.to_owned(),
        replacement_range: CommandSuggestionReplacementRange {
            start: 4,
            end: 4 + typed_length,
        },
        suffix: replacement.chars().skip(typed_length).collect(),
        score: 0.9,
        sensitivity: CommandSuggestionSensitivity::Normal,
        description: None,
        source_explanation: None,
        merged_source_explanations: Vec::new(),
        source_id: Some("git".to_owned()),
        allowed_presentations: vec![SuggestionPresentation::Inline, SuggestionPresentation::Menu],
        accept_boundaries: vec![typed_length.saturating_add(1), replacement.chars().count()],
        context_key: Some("query".to_owned()),
    }
}

/// 把 job 查询字段回填成 provider batch，确保测试覆盖 batch 与请求的真实关联契约。
fn batch(query: &TerminalSuggestionQuery) -> TerminalSuggestionBatch {
    TerminalSuggestionBatch {
        binding: query.binding.clone(),
        expected_context: query.expected_context,
        request_generation: query.request_generation,
        candidates: vec![candidate(query)],
    }
}

/// 启用默认偏好，供建议生命周期测试共享同一产品设置。
fn enabled_bridge() -> TerminalCommandSuggestionBridge {
    let mut bridge = TerminalCommandSuggestionBridge::new();
    bridge.set_settings(Some(TerminalInlineSuggestionSettings::default()));
    bridge
}

/// 未加载设置、设置关闭和切离 session 都不会留下可绘制或可接受的候选。
/// 禁用偏好在构造时固定，其余沿用产品默认值，确保取消由偏好切换触发。
///
/// @author kongweiguang
#[test]
fn settings_and_context_loss_cancel_and_clear_suggestions() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = TerminalCommandSuggestionBridge::new();
    assert!(
        bridge
            .request(
                &context,
                "git che".to_owned(),
                7,
                SuggestionQueryMode::Inline
            )
            .is_none()
    );

    bridge.set_settings(Some(TerminalInlineSuggestionSettings::default()));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("enabled settings allow a trusted request");
    let cancellation = job.request_cancellation.clone();
    assert!(bridge.apply_batch(batch(&job.query)));
    assert!(bridge.inline_ghost().is_some());

    let disabled = TerminalInlineSuggestionSettings {
        enabled: false,
        presentation: TerminalCommandSuggestionPresentation::Off,
        ..TerminalInlineSuggestionSettings::default()
    };
    bridge.set_settings(Some(disabled));
    assert!(cancellation.is_cancelled());
    assert!(bridge.inline_ghost().is_none());
    assert!(
        bridge
            .request(
                &context,
                "git che".to_owned(),
                7,
                SuggestionQueryMode::Inline
            )
            .is_none()
    );

    bridge.set_settings(Some(TerminalInlineSuggestionSettings::default()));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("settings may be enabled again");
    let cancellation = job.request_cancellation.clone();
    let mut unavailable = context;
    unavailable.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::ExternalInputObserved,
    );
    assert!(!bridge.observe_context(Some(&unavailable)));
    assert!(cancellation.is_cancelled());
    assert!(bridge.inline_ghost().is_none());
    assert!(!bridge.observe_context(None));
}

/// 新输入会取消旧 provider 令牌，并拒绝旧 generation 返回的 batch。
#[test]
fn newer_request_cancels_old_query_and_rejects_its_batch() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    let old_job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted input allows a query");
    let old_cancellation = old_job.request_cancellation.clone();
    let late_batch = batch(&old_job.query);

    let current_job = bridge
        .request(
            &context,
            "git check".to_owned(),
            9,
            SuggestionQueryMode::Inline,
        )
        .expect("new input replaces the query");

    assert!(old_cancellation.is_cancelled());
    assert!(!bridge.apply_batch(late_batch));
    assert!(bridge.apply_batch(batch(&current_job.query)));
    assert_eq!(
        bridge
            .inline_ghost()
            .expect("current candidate has a suffix")
            .text,
        "out"
    );
    assert!(bridge.is_request_current(current_job.request_generation()));
    assert!(!bridge.is_request_current(old_job.request_generation()));
}

/// Ghost 只在光标位于普通候选替换尾部时出现，危险或非 inline 候选不覆盖终端内容。
#[test]
fn ghost_requires_inline_safe_candidate_at_cursor() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt allows request");
    assert!(bridge.apply_batch(batch(&job.query)));
    let ghost = bridge.inline_ghost().expect("suffix is drawable");
    assert_eq!(ghost.binding.session_id, context.session_id);
    assert_eq!(ghost.expected_context, context.version());
    assert_eq!(ghost.request_generation, job.request_generation());
    assert_eq!(ghost.candidate_id, "spec:git:checkout");

    let mut menu_only = candidate(&job.query);
    menu_only.allowed_presentations = vec![SuggestionPresentation::Menu];
    let mut menu_batch = batch(&job.query);
    menu_batch.candidates = vec![menu_only];
    assert!(bridge.apply_batch(menu_batch));
    assert!(bridge.inline_ghost().is_none());
}

/// 显式固定接受键与局部接受，其余沿用默认值，验证接受事件仍绑定原 session/context。
///
/// @author kongweiguang
#[test]
fn configured_accept_key_builds_per_event_cas_requests() {
    let context = available_context("session-a", 4, 10, 11);
    let settings = TerminalInlineSuggestionSettings {
        accept_key: TerminalInlineSuggestionAcceptKey::RightArrow,
        partial_accept: true,
        ..TerminalInlineSuggestionSettings::default()
    };
    let mut bridge = TerminalCommandSuggestionBridge::new();
    bridge.set_settings(Some(settings));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("enabled inline settings allow request");
    assert!(bridge.apply_batch(batch(&job.query)));

    let acceptance = bridge
        .accept_on_key(
            TerminalKeyEvent {
                key: TerminalKey::Right,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            },
            &context,
        )
        .expect("configured key accepts current ghost");
    assert_eq!(acceptance.binding.session_id, context.session_id);
    assert_eq!(acceptance.expected_context, context.version());
    assert_eq!(acceptance.input_after, "git chec");

    let request = bridge
        .acceptance_input_request(&acceptance, 0, &context)
        .expect("first event is bound to the accepted session and version");
    assert_eq!(request.binding, acceptance.binding);
    assert_eq!(request.expected_context, Some(context.version()));
    assert_eq!(request.projection, UiInputProjection::Tracked);
    assert_eq!(request.event, acceptance.events[0]);
    assert!(
        bridge
            .acceptance_input_request(&acceptance, 1, &context)
            .is_none()
    );
    assert!(bridge.inline_ghost().is_none());
}

/// 接受计划逐事件推进同一 session 的 input generation，中途不查询，最后一步才重新查询。
/// 只覆盖接受键，其余沿用默认配置，避免测试自行替代逐事件回执的推进规则。
///
/// @author kongweiguang
#[test]
fn acceptance_events_require_fifo_context_compare_and_swap() {
    let mut context = available_context("session-a", 4, 10, 11);
    let settings = TerminalInlineSuggestionSettings {
        accept_key: TerminalInlineSuggestionAcceptKey::RightArrow,
        ..TerminalInlineSuggestionSettings::default()
    };
    let mut bridge = TerminalCommandSuggestionBridge::new();
    bridge.set_settings(Some(settings));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt allows request");
    assert!(bridge.apply_batch(batch(&job.query)));
    let acceptance = bridge
        .accept_on_key(
            TerminalKeyEvent {
                key: TerminalKey::Right,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            },
            &context,
        )
        .expect("current ghost can be accepted");
    let mut observed_request_generations = Vec::new();

    for (event_index, expected_event) in acceptance.events.iter().enumerate() {
        let request = bridge
            .acceptance_input_request(&acceptance, event_index, &context)
            .expect("only the next event can be requested");
        assert_eq!(&request.event, expected_event);
        assert_eq!(request.expected_context, Some(context.version()));
        assert!(
            bridge
                .acceptance_input_request(&acceptance, event_index + 1, &context)
                .is_none()
        );

        context.input_generation += 1;
        context.prompt_line_empty = false;
        match bridge.acceptance_input_written(
            &acceptance,
            event_index,
            &context,
            SuggestionQueryMode::Inline,
        ) {
            AcceptanceInputProgress::Next { event_index: next } => {
                assert_eq!(next, event_index + 1);
                observed_request_generations.push(context.input_generation);
            }
            AcceptanceInputProgress::Complete { follow_up } => {
                assert_eq!(event_index + 1, acceptance.events.len());
                assert!(follow_up.is_some(), "final text should trigger one refresh");
                observed_request_generations.push(context.input_generation);
            }
            AcceptanceInputProgress::Invalidated => panic!("matching CAS result must advance"),
        }
    }

    assert_eq!(
        observed_request_generations,
        (12..=11 + acceptance.events.len() as u64).collect::<Vec<_>>()
    );
    assert!(
        bridge
            .acceptance_input_request(&acceptance, acceptance.events.len(), &context)
            .is_none()
    );
}

/// 活动 pane 改变后使已接受计划失效，不能把延迟候选写入旧或新会话。
#[test]
fn switching_session_invalidates_pending_acceptance() {
    let context = available_context("session-a", 4, 10, 11);
    let mut bridge = enabled_bridge();
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt allows request");
    assert!(bridge.apply_batch(batch(&job.query)));
    let acceptance = bridge
        .accept_on_key(
            TerminalKeyEvent {
                key: TerminalKey::Right,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            },
            &context,
        )
        .expect("current ghost can be accepted");

    let next_context = available_context("session-b", 1, 1, 1);
    assert!(!bridge.observe_context(Some(&next_context)));
    assert!(
        bridge
            .acceptance_input_request(&acceptance, 0, &context)
            .is_none()
    );
    assert!(bridge.inline_ghost().is_none());
}

/// 显式固定接受键，其余沿用默认值；修饰键与重复键必须继续交给正常终端输入。
///
/// @author kongweiguang
#[test]
fn accept_key_only_consumes_unmodified_press() {
    let context = available_context("session-a", 1, 2, 3);
    let settings = TerminalInlineSuggestionSettings {
        accept_key: TerminalInlineSuggestionAcceptKey::RightArrow,
        ..TerminalInlineSuggestionSettings::default()
    };
    let mut bridge = TerminalCommandSuggestionBridge::new();
    bridge.set_settings(Some(settings));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted input allows request");
    assert!(bridge.apply_batch(batch(&job.query)));

    assert!(
        bridge
            .accept_on_key(
                TerminalKeyEvent {
                    key: TerminalKey::Right,
                    phase: KeyPhase::Repeat,
                    modifiers: InputModifiers::default(),
                },
                &context
            )
            .is_none()
    );
    assert!(
        bridge
            .accept_on_key(
                TerminalKeyEvent {
                    key: TerminalKey::Right,
                    phase: KeyPhase::Press,
                    modifiers: InputModifiers {
                        shift: true,
                        ..InputModifiers::default()
                    },
                },
                &context
            )
            .is_none()
    );
    assert!(bridge.inline_ghost().is_some());
}

/// 左移到 `e` 前再插入 `c` 应得到 `git chce`，防止测试把光标编辑误当成追加输入。
/// @author kongweiguang
#[test]
fn tracked_input_events_update_one_projection_and_cancel_old_work() {
    let mut context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    assert!(bridge.observe_context(Some(&context)));

    context.input_generation += 1;
    context.prompt_line_empty = false;
    let first = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Text("git che".to_owned()),
            SuggestionQueryMode::Inline,
        )
        .expect("tracked text creates a query");
    assert_eq!(first.query.input, "git che");
    assert_eq!(first.query.cursor, 7);
    let first_cancellation = first.request_cancellation.clone();

    context.input_generation += 1;
    let key_release = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Key(TerminalKeyEvent {
                key: TerminalKey::Left,
                phase: KeyPhase::Release,
                modifiers: InputModifiers::default(),
            }),
            SuggestionQueryMode::Inline,
        )
        .expect("a key release that reached the writer refreshes the versioned query");
    assert!(first_cancellation.is_cancelled());
    assert_eq!(key_release.query.input, "git che");
    assert_eq!(key_release.query.cursor, 7);
    let key_release_cancellation = key_release.request_cancellation.clone();

    context.input_generation += 1;
    let moved = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Key(TerminalKeyEvent {
                key: TerminalKey::Left,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            }),
            SuggestionQueryMode::Inline,
        )
        .expect("known cursor movement remains trackable");
    assert!(key_release_cancellation.is_cancelled());
    assert_eq!(moved.query.input, "git che");
    assert_eq!(moved.query.cursor, 6);

    context.input_generation += 1;
    let inserted = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Text("c".to_owned()),
            SuggestionQueryMode::Inline,
        )
        .expect("tracked text updates the cursor projection");
    assert_eq!(inserted.query.input, "git chce");
    assert_eq!(inserted.query.cursor, 7);
}

/// 不跟踪 Enter 等会改变 shell 交互状态的按键，避免下一行或历史输入伪装成当前命令。
/// @author kongweiguang
#[test]
fn unsupported_edit_invalidates_projection_and_query() {
    let mut context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    assert!(bridge.observe_context(Some(&context)));
    context.input_generation += 1;
    context.prompt_line_empty = false;
    let job = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Text("git che".to_owned()),
            SuggestionQueryMode::Inline,
        )
        .expect("tracked text creates a query");
    let cancellation = job.request_cancellation.clone();

    context.input_generation += 1;
    context.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);
    assert!(
        bridge
            .tracked_input_written(
                &context,
                &TerminalInputEvent::Key(TerminalKeyEvent {
                    key: TerminalKey::Enter,
                    phase: KeyPhase::Press,
                    modifiers: InputModifiers::default(),
                }),
                SuggestionQueryMode::Inline,
            )
            .is_none()
    );
    assert!(cancellation.is_cancelled());
    assert!(bridge.inline_ghost().is_none());
    assert!(
        bridge
            .request_current(&context, SuggestionQueryMode::Inline)
            .is_none()
    );
    assert!(
        bridge
            .request(
                &context,
                "git che".to_owned(),
                7,
                SuggestionQueryMode::Inline,
            )
            .is_none(),
        "a submitted prompt cannot accept a replacement query"
    );

    context.prompt_epoch += 1;
    context.input_generation += 1;
    context.prompt_ready = true;
    context.prompt_line_empty = true;
    context.availability = TerminalCommandAvailability::Available;
    assert!(
        bridge
            .request_current(&context, SuggestionQueryMode::Inline)
            .is_some(),
        "a fresh empty prompt may start a new query"
    );
}

/// 菜单快捷键只在有效设置、按下阶段和可信空提示符上创建可取消的 Menu 查询。
#[test]
fn menu_shortcut_opens_generation_scoped_query_and_failure_can_retry() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    let shortcut = TerminalKeyEvent {
        key: TerminalKey::Character(' '),
        phase: KeyPhase::Press,
        modifiers: InputModifiers {
            control: true,
            ..InputModifiers::default()
        },
    };

    let first = bridge
        .request_menu_on_key(shortcut, &context)
        .expect("default Ctrl+Space opens a menu query at an empty prompt");
    assert_eq!(first.query.mode, SuggestionQueryMode::Menu);
    assert!(bridge.menu_is_open());
    assert_eq!(
        bridge
            .menu_projection()
            .expect("open menu has a projection")
            .state,
        TerminalSuggestionQueryState::Loading
    );
    assert!(bridge.menu_candidates().is_empty());

    assert!(bridge.fail_request(first.request_generation()));
    assert_eq!(
        bridge
            .menu_projection()
            .expect("failed menu remains visible")
            .state,
        TerminalSuggestionQueryState::Failed
    );
    assert!(bridge.menu_is_open());
    assert!(!bridge.fail_request(first.request_generation().wrapping_sub(1)));

    let retry = bridge
        .request_menu(&context)
        .expect("the same trusted input can start a new menu generation");
    assert_ne!(retry.request_generation(), first.request_generation());
    assert_eq!(
        bridge
            .menu_projection()
            .expect("retry menu has a projection")
            .state,
        TerminalSuggestionQueryState::Loading
    );
}

/// 菜单仅呈现 controller 当前 Menu batch，关闭时取消请求且不丢失输入投影。
#[test]
fn menu_batch_is_projected_and_dismissal_cancels_without_losing_input() {
    let mut context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    assert!(bridge.observe_context(Some(&context)));
    context.input_generation += 1;
    let input = bridge
        .tracked_input_written(
            &context,
            &TerminalInputEvent::Text("git ".to_owned()),
            SuggestionQueryMode::Inline,
        )
        .expect("tracked input establishes the command projection");
    let input_cancellation = input.request_cancellation.clone();

    let menu = bridge
        .request_menu(&context)
        .expect("menu uses the same controller input projection");
    assert!(input_cancellation.is_cancelled());
    assert!(bridge.menu_candidates().is_empty());
    let menu_cancellation = menu.request_cancellation.clone();
    assert!(bridge.apply_batch(batch(&menu.query)));
    assert_eq!(
        bridge
            .menu_projection()
            .expect("ready menu has a projection")
            .state,
        TerminalSuggestionQueryState::Ready
    );
    assert_eq!(bridge.menu_candidates().len(), 1);

    assert!(bridge.dismiss_menu());
    assert!(menu_cancellation.is_cancelled());
    assert!(!bridge.menu_is_open());
    assert!(bridge.menu_candidates().is_empty());
    let reopened = bridge
        .request_menu(&context)
        .expect("dismissal keeps the trusted input available for reopening");
    assert_eq!(reopened.query.input, "git ");
}

/// 显式开启 Tab 菜单选项，其余沿用默认值；仅首次按下创建查询，其它按键仍留给终端。
///
/// @author kongweiguang
#[test]
fn tab_menu_trigger_respects_preference_and_key_phase() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    let tab = TerminalKeyEvent {
        key: TerminalKey::Tab,
        phase: KeyPhase::Press,
        modifiers: InputModifiers::default(),
    };
    assert!(bridge.request_menu_on_key(tab, &context).is_none());

    let settings = TerminalInlineSuggestionSettings {
        tab_opens_menu: true,
        ..TerminalInlineSuggestionSettings::default()
    };
    bridge.set_settings(Some(settings));
    assert!(bridge.request_menu_on_key(tab, &context).is_some());

    let release = TerminalKeyEvent {
        phase: KeyPhase::Release,
        ..tab
    };
    assert!(bridge.request_menu_on_key(release, &context).is_none());
}
