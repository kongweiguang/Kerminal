//! Root command-suggestion bridge cancellation, filtering, ghost, and CAS contract tests.
//!
//! @author kongweiguang

use kerminal_application::{
    commands::{TerminalSuggestionBatch, TerminalSuggestionProgress, TerminalSuggestionQuery},
    preferences::{
        TerminalCommandSuggestionPresentation, TerminalInlineSuggestionAcceptKey,
        TerminalInlineSuggestionSettings,
    },
    sessions::{
        ShellEditingAvailability, ShellEditingShell, ShellEditingSnapshot,
        ShellEditingUnavailableReason, TerminalCommandAvailability, TerminalCommandContext,
        TerminalCommandUnavailableReason, UiInputProjection,
    },
};
use kerminal_domain::{
    commands::{
        CommandSuggestionActivation, CommandSuggestionCandidate, CommandSuggestionReplacementRange,
        CommandSuggestionSensitivity, SuggestionPresentation, SuggestionProviderKind,
        SuggestionQueryMode, SuggestionSourceState, SuggestionSourceStatus,
    },
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use kerminal_terminal::input::{
    InputModifiers, KeyPhase, TerminalInputEvent, TerminalKey, TerminalKeyEvent,
};

use super::{
    AcceptanceInputProgress, TerminalCommandSuggestionBridge, TerminalSuggestionJob,
    TerminalSuggestionQueryState,
};

impl TerminalCommandSuggestionBridge {
    /// 回归旧投影状态机时复用真实 controller，但不把测试入口编译进生产模块。
    pub fn tracked_input_written(
        &mut self,
        context: &TerminalCommandContext,
        event: &TerminalInputEvent,
        mode: SuggestionQueryMode,
    ) -> Option<TerminalSuggestionJob> {
        if self.pending_acceptance.is_some() {
            self.clear();
            return None;
        }
        if !self.mode_enabled(mode) {
            self.clear();
            return None;
        }
        let Some((query, request_cancellation)) =
            self.controller.apply_tracked_input(context, event, mode)
        else {
            self.query_state = TerminalSuggestionQueryState::Closed;
            return None;
        };
        self.query_state = TerminalSuggestionQueryState::Loading;
        Some(TerminalSuggestionJob {
            query,
            request_cancellation,
            snapshot: None,
        })
    }

    /// 保留快捷键到测试查询的关联，确保迁出辅助函数不改变已有断言语义。
    pub fn request_menu_on_key(
        &mut self,
        event: TerminalKeyEvent,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionJob> {
        if !self.is_menu_shortcut(event) {
            return None;
        }
        self.request_current(context, SuggestionQueryMode::Menu)
    }

    /// 既有测试通过同一控制器投影请求菜单，不在产品代码保留专用入口。
    pub fn request_menu(
        &mut self,
        context: &TerminalCommandContext,
    ) -> Option<TerminalSuggestionJob> {
        self.request_current(context, SuggestionQueryMode::Menu)
    }

    /// 保持测试 batch 原有的 binding 与查询代次校验，避免绕过控制器。
    pub fn apply_batch(&mut self, batch: TerminalSuggestionBatch) -> bool {
        if !self.is_enabled() || !self.controller.apply_batch(batch) {
            return false;
        }
        self.query_state = if self.controller.candidates().is_empty() {
            TerminalSuggestionQueryState::Empty
        } else {
            TerminalSuggestionQueryState::Ready
        };
        true
    }
}

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

/// 即使启用分段接受，普通右方向键也完整追加，并绑定原 session/context。
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
    assert_eq!(acceptance.input_after, "git checkout");
    assert_eq!(
        acceptance.events,
        vec![TerminalInputEvent::Paste("ckout".to_owned())]
    );

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

/// Alt+右方向只在用户启用分段时截获，且后缀以当前输入而非 provider 的展示字段推导。
#[test]
fn alt_right_accepts_one_segment_and_ghost_ignores_untrusted_suffix() {
    let context = available_context("session-a", 4, 10, 11);
    let mut bridge = TerminalCommandSuggestionBridge::new();
    bridge.set_settings(Some(TerminalInlineSuggestionSettings {
        partial_accept: true,
        ..TerminalInlineSuggestionSettings::default()
    }));
    let job = bridge
        .request(
            &context,
            "git che".to_owned(),
            7,
            SuggestionQueryMode::Inline,
        )
        .expect("trusted prompt");
    let mut result = batch(&job.query);
    result.candidates[0].suffix = "misleading".to_owned();
    assert!(bridge.apply_batch(result));
    assert_eq!(bridge.inline_ghost().expect("valid ghost").text, "ckout");
    let acceptance = bridge
        .accept_on_key(
            TerminalKeyEvent {
                key: TerminalKey::Right,
                phase: KeyPhase::Press,
                modifiers: InputModifiers {
                    alt: true,
                    ..InputModifiers::default()
                },
            },
            &context,
        )
        .expect("enabled partial acceptance");
    assert_eq!(
        acceptance.events,
        vec![TerminalInputEvent::Paste("c".to_owned())]
    );
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

/// 连接级降级必须保留真实原因，不能把无法恢复的探测包装成可重试的来源查询。
#[test]
fn snapshot_failure_preserves_unavailable_reason_without_stale_menu_replacement() {
    let context = available_context("session-a", 1, 2, 3);
    let binding = super::binding_for_context(&context);
    let mut bridge = enabled_bridge();
    for reason in [
        ShellEditingUnavailableReason::Unsupported,
        ShellEditingUnavailableReason::IntegrationNotObserved,
        ShellEditingUnavailableReason::KeyBindingConflict,
        ShellEditingUnavailableReason::PromptNotReady,
        ShellEditingUnavailableReason::ProbeTimedOut,
        ShellEditingUnavailableReason::SessionChanged,
    ] {
        let job = bridge
            .begin_snapshot(binding.clone(), SuggestionQueryMode::Menu)
            .unwrap();
        assert!(bridge.snapshot_failed(
            job.request_generation,
            &binding,
            Some(ShellEditingAvailability::Unavailable(reason))
        ));
        let projection = bridge
            .menu_projection()
            .expect("unavailable menu stays dismissible");
        assert_eq!(
            projection.state,
            TerminalSuggestionQueryState::Unavailable(reason)
        );
        assert!(projection.candidates.is_empty());
        let next = bridge
            .begin_snapshot(binding.clone(), SuggestionQueryMode::Menu)
            .unwrap();
        assert!(!bridge.snapshot_failed(
            job.request_generation,
            &binding,
            Some(ShellEditingAvailability::Unavailable(reason))
        ));
        assert_eq!(
            bridge.menu_projection().unwrap().state,
            TerminalSuggestionQueryState::Loading
        );
        assert!(bridge.snapshot_failed(
            next.request_generation,
            &binding,
            Some(ShellEditingAvailability::Available)
        ));
        assert_eq!(
            bridge.menu_projection().unwrap().state,
            TerminalSuggestionQueryState::Failed
        );
        assert!(bridge.dismiss_menu());
    }
}

/// 未收到握手可能是启动中或集成被用户配置阻止；主动菜单解释能力缺失，不诱导重试来源。
/// 自动探测保持安静，稍后重新打开仍可发起新一代请求，不能把 Pending 永久缓存为不支持。
#[test]
fn missing_handshake_is_explained_without_blocking_later_menu_requests() {
    let context = available_context("session-a", 1, 2, 3);
    let binding = super::binding_for_context(&context);
    let mut bridge = enabled_bridge();
    let inline = bridge
        .begin_snapshot(binding.clone(), SuggestionQueryMode::Inline)
        .unwrap();
    assert!(bridge.snapshot_failed(
        inline.request_generation,
        &binding,
        Some(ShellEditingAvailability::Pending)
    ));
    assert!(!bridge.menu_is_open());
    assert!(bridge.inline_ghost().is_none());

    let menu = bridge
        .begin_snapshot(binding.clone(), SuggestionQueryMode::Menu)
        .unwrap();
    assert!(bridge.snapshot_failed(
        menu.request_generation,
        &binding,
        Some(ShellEditingAvailability::Pending)
    ));
    assert_eq!(
        bridge.menu_projection().unwrap().state,
        TerminalSuggestionQueryState::Unavailable(
            ShellEditingUnavailableReason::IntegrationNotObserved
        )
    );
    assert!(bridge.dismiss_menu());
    let next = bridge
        .begin_snapshot(binding.clone(), SuggestionQueryMode::Menu)
        .unwrap();
    assert!(next.request_generation > menu.request_generation);
    assert!(!bridge.snapshot_failed(
        menu.request_generation,
        &binding,
        Some(ShellEditingAvailability::Pending)
    ));
    assert_eq!(
        bridge.menu_projection().unwrap().state,
        TerminalSuggestionQueryState::Loading
    );
}

/// 自动提示降级不弹错误菜单；下一次明确菜单请求仍能解释该连接的实际能力。
#[test]
fn unavailable_inline_probe_stays_quiet_and_can_be_reopened_explicitly() {
    let context = available_context("session-a", 1, 2, 3);
    let binding = super::binding_for_context(&context);
    let mut bridge = enabled_bridge();
    let unavailable = Some(ShellEditingAvailability::Unavailable(
        ShellEditingUnavailableReason::ProbeTimedOut,
    ));
    let job = bridge
        .begin_snapshot(binding.clone(), SuggestionQueryMode::Inline)
        .unwrap();
    assert!(bridge.snapshot_failed(job.request_generation, &binding, unavailable));
    assert!(!bridge.menu_is_open());
    assert!(bridge.inline_ghost().is_none());
    let menu = bridge
        .begin_snapshot(binding.clone(), SuggestionQueryMode::Menu)
        .unwrap();
    assert!(bridge.snapshot_failed(menu.request_generation, &binding, unavailable));
    assert_eq!(
        bridge.menu_projection().unwrap().state,
        TerminalSuggestionQueryState::Unavailable(ShellEditingUnavailableReason::ProbeTimedOut)
    );
}

/// 当前输入超限不等于连接失效；缩短后的新代探测可直接进入正常候选流程。
#[test]
fn oversized_snapshot_failure_can_recover_after_shorter_input() {
    let context = available_context("session-too-long", 1, 2, 3);
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let job = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .unwrap();
    assert!(bridge.snapshot_input_too_long(job.request_generation, &snapshot.binding));
    assert_eq!(
        bridge.menu_projection().unwrap().state,
        TerminalSuggestionQueryState::InputTooLong
    );
    let next = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .unwrap();
    assert!(!bridge.snapshot_input_too_long(job.request_generation, &snapshot.binding));
    assert!(
        bridge
            .snapshot_received(next.request_generation, &context, snapshot)
            .is_some()
    );
    assert_eq!(
        bridge.menu_projection().unwrap().state,
        TerminalSuggestionQueryState::Loading
    );
}

/// Alt+/ 为输入法可能占用 Ctrl+Space 的用户提供独立入口，不截获 AltGr 组合键。
#[test]
fn alt_slash_opens_menu_but_altgr_is_not_intercepted() {
    let context = available_context("session-a", 1, 2, 3);
    let mut bridge = enabled_bridge();
    let alt_slash = TerminalKeyEvent {
        key: TerminalKey::Character('/'),
        phase: KeyPhase::Press,
        modifiers: InputModifiers {
            alt: true,
            ..InputModifiers::default()
        },
    };
    let altgr_slash = TerminalKeyEvent {
        modifiers: InputModifiers {
            control: true,
            alt: true,
            ..InputModifiers::default()
        },
        ..alt_slash
    };
    assert!(bridge.request_menu_on_key(altgr_slash, &context).is_none());
    assert!(bridge.request_menu_on_key(alt_slash, &context).is_some());
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

/// 使用明确的会话内读回构造样本，不把屏幕文字或用户输入模拟当作可信状态。
fn editing_snapshot(context: &TerminalCommandContext) -> ShellEditingSnapshot {
    ShellEditingSnapshot {
        binding: super::binding_for_context(context),
        context: context.version(),
        revision: 7,
        input: "git che".to_owned(),
        cursor: 7,
        cwd: Some("/work/project".to_owned()),
        shell: ShellEditingShell::Bash,
        shell_version: Some("5.2".to_owned()),
        shell_prediction_visible: Some(false),
    }
}

/// 固定快照并生成当前代的合法候选，供异步顺序和回读测试共用。
fn snapshot_query(
    bridge: &mut TerminalCommandSuggestionBridge,
    context: &TerminalCommandContext,
    snapshot: &ShellEditingSnapshot,
    mode: SuggestionQueryMode,
) -> super::TerminalSuggestionJob {
    let pending = bridge
        .begin_snapshot(snapshot.binding.clone(), mode)
        .expect("enabled mode can request a snapshot");
    bridge
        .snapshot_received(pending.request_generation, context, snapshot.clone())
        .expect("matching snapshot starts a query")
}

/// 快照恢复不依赖旧输入投影；无效 prompt 和被新请求取消的读回不能启动查询。
#[test]
fn shell_snapshot_recovers_unknown_line_and_rejects_stale_binding() {
    let mut context = available_context("session-snapshot", 3, 5, 7);
    context.prompt_line_empty = false;
    context.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::ExternalInputObserved,
    );
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let old = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("first snapshot request");
    let current = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("new snapshot request");
    assert!(old.cancellation.is_cancelled());
    assert!(
        bridge
            .snapshot_received(old.request_generation, &context, snapshot.clone())
            .is_none()
    );

    let mut stale = snapshot.clone();
    stale.context.prompt_epoch += 1;
    assert!(
        bridge
            .snapshot_received(current.request_generation, &context, stale)
            .is_none()
    );
    let retry = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("retry after rejected snapshot");
    let job = bridge
        .snapshot_received(retry.request_generation, &context, snapshot)
        .expect("shell readback restores input despite old projection failure");
    assert_eq!(job.query.input, "git che");
    assert_eq!(job.query.cursor, 7);
}

/// 被输入打断的菜单保持打开但撤销旧读回，队列恢复后可安全创建新的查询代次。
#[test]
fn suspended_menu_can_restart_after_input_queue_recovery() {
    let context = available_context("session-resume", 1, 2, 3);
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let old = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .expect("menu begins a snapshot request");
    bridge.suspend_for_pending_input();
    assert!(old.cancellation.is_cancelled());
    assert!(bridge.menu_is_open());
    assert!(!bridge.snapshot_is_current(old.request_generation, &snapshot.binding));
    let replacement = bridge
        .begin_snapshot(snapshot.binding.clone(), bridge.active_mode())
        .expect("menu can restart after the queue drains");
    assert!(
        bridge
            .snapshot_received(old.request_generation, &context, snapshot.clone())
            .is_none()
    );
    let job = bridge
        .snapshot_received(replacement.request_generation, &context, snapshot)
        .expect("only the replacement readback starts a query");
    assert_eq!(job.query.mode, SuggestionQueryMode::Menu);
}

/// 部分结果可先展示；最终结果一旦提交，迟到增量和其他快照修订不得覆盖。
#[test]
fn snapshot_progress_rejects_stale_revision_and_late_partial_result() {
    let context = available_context("session-progress", 1, 2, 3);
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let job = snapshot_query(&mut bridge, &context, &snapshot, SuggestionQueryMode::Menu);
    let mut progress = TerminalSuggestionProgress {
        batch: batch(&job.query),
        snapshot_revision: snapshot.revision + 1,
        sources: vec![SuggestionSourceStatus {
            provider: SuggestionProviderKind::Spec,
            state: SuggestionSourceState::Ready,
        }],
    };
    assert!(!bridge.apply_progress(progress.clone(), false));
    progress.snapshot_revision = snapshot.revision;
    assert!(bridge.apply_progress(progress.clone(), false));
    assert_eq!(bridge.menu_candidates().len(), 1);
    progress.sources.push(SuggestionSourceStatus {
        provider: SuggestionProviderKind::RemotePath,
        state: SuggestionSourceState::Failed,
    });
    assert!(bridge.apply_progress(progress.clone(), true));
    let projection = bridge.menu_projection().expect("menu remains open");
    assert_eq!(projection.state, TerminalSuggestionQueryState::Ready);
    assert!(projection.partial_failure);
    let mut late = progress;
    late.batch.candidates.clear();
    assert!(!bridge.apply_progress(late, false));
    assert_eq!(bridge.menu_candidates().len(), 1);
}

/// 未知预测由终端网格另行让位；Shell 明确声明存在预测时不显示或接受 ghost。
#[test]
fn shell_prediction_only_hides_ghost_when_explicitly_visible() {
    for (prediction, visible) in [(None, true), (Some(false), true), (Some(true), false)] {
        let context = available_context("session-prediction", 1, 2, 3);
        let mut snapshot = editing_snapshot(&context);
        snapshot.shell_prediction_visible = prediction;
        let mut bridge = enabled_bridge();
        let job = snapshot_query(
            &mut bridge,
            &context,
            &snapshot,
            SuggestionQueryMode::Inline,
        );
        assert!(bridge.apply_progress(
            TerminalSuggestionProgress {
                batch: batch(&job.query),
                snapshot_revision: snapshot.revision,
                sources: Vec::new(),
            },
            true
        ));
        assert_eq!(bridge.inline_ghost().is_some(), visible);
        assert_eq!(
            bridge
                .accept_on_key(
                    TerminalKeyEvent {
                        key: TerminalKey::Right,
                        phase: KeyPhase::Press,
                        modifiers: InputModifiers::default(),
                    },
                    &context
                )
                .is_some(),
            visible
        );
    }
}

/// 使用真实读回和完整 provider 代次准备 ghost，避免测试依赖 UI 投影模拟。
fn prepare_snapshot_ghost(
    bridge: &mut TerminalCommandSuggestionBridge,
    context: &TerminalCommandContext,
    snapshot: &ShellEditingSnapshot,
) {
    let job = snapshot_query(bridge, context, snapshot, SuggestionQueryMode::Inline);
    assert!(bridge.apply_progress(
        TerminalSuggestionProgress {
            batch: batch(&job.query),
            snapshot_revision: snapshot.revision,
            sources: Vec::new(),
        },
        true
    ));
    assert!(bridge.inline_ghost().is_some());
}

/// 同一 Shell 正文与光标不会因无效按键产生的新版本重新展示，手动菜单仍可重开。
#[test]
fn dismissed_ghost_stays_closed_for_the_same_input_and_allows_explicit_menu() {
    let mut context = available_context("session-dismissed", 1, 2, 3);
    let mut snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    prepare_snapshot_ghost(&mut bridge, &context, &snapshot);
    assert!(bridge.dismiss_inline());
    assert!(!bridge.dismiss_inline());

    for _ in 0..2 {
        bridge.suspend_for_pending_input();
        context.input_generation += 1;
        snapshot.context = context.version();
        snapshot.revision += 1;
        let pending = bridge
            .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
            .expect("automatic refresh can inspect the new readback");
        assert!(
            bridge
                .snapshot_received(pending.request_generation, &context, snapshot.clone())
                .is_none()
        );
        assert!(!bridge.is_request_current(pending.request_generation));
        assert!(bridge.inline_ghost().is_none());
    }

    let pending = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .expect("explicit menu bypasses dismissal");
    assert!(bridge.is_menu_projection_current(pending.request_generation));
    assert!(bridge.dismiss_menu());
    let automatic = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("refresh after closing the loading menu");
    assert!(
        bridge
            .snapshot_received(automatic.request_generation, &context, snapshot.clone())
            .is_none()
    );
    let reopened = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .expect("menu can be explicitly reopened again");
    assert!(
        bridge
            .snapshot_received(reopened.request_generation, &context, snapshot)
            .is_some()
    );
}

/// 任一真实输入身份变化都应解除关闭标记，不能把一次 Esc 扩展到其它 prompt 或会话。
#[test]
fn dismissed_ghost_recovers_when_actual_input_identity_changes() {
    for change in 0..5 {
        let mut context = available_context("session-before", 1, 2, 3);
        let mut snapshot = editing_snapshot(&context);
        let mut bridge = enabled_bridge();
        prepare_snapshot_ghost(&mut bridge, &context, &snapshot);
        assert!(bridge.dismiss_inline());
        bridge.suspend_for_pending_input();
        context.input_generation += 1;
        match change {
            0 => {
                snapshot.input.push('x');
                snapshot.cursor += 1;
            }
            1 => snapshot.cursor -= 1,
            2 => context.prompt_epoch += 1,
            3 => context.session_id = SessionId::parse("session-after").unwrap(),
            4 => context.session_generation += 1,
            _ => unreachable!(),
        }
        snapshot.context = context.version();
        snapshot.binding = super::binding_for_context(&context);
        snapshot.revision += 1;
        let pending = bridge
            .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
            .expect("new input identity can request a snapshot");
        assert!(
            bridge
                .snapshot_received(pending.request_generation, &context, snapshot)
                .is_some(),
            "identity change {change} must restore suggestions"
        );
    }
}

/// 暂停时只保留菜单关闭权限，旧候选和迟到结果不得恢复接受能力。
#[test]
fn suspended_menu_remains_dismissible_but_its_candidate_is_stale() {
    let context = available_context("session-dismiss-loading", 1, 2, 3);
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let job = snapshot_query(&mut bridge, &context, &snapshot, SuggestionQueryMode::Menu);
    assert!(bridge.apply_progress(
        TerminalSuggestionProgress {
            batch: batch(&job.query),
            snapshot_revision: snapshot.revision,
            sources: Vec::new(),
        },
        true
    ));
    let generation = job.query.request_generation;
    bridge.suspend_for_pending_input();
    assert!(bridge.is_menu_projection_current(generation));
    assert!(!bridge.is_request_current(generation));
    assert!(
        bridge
            .accept_candidate_for_generation(generation, "spec:git:checkout", false, &context)
            .is_none()
    );
    assert!(bridge.dismiss_menu());
    assert!(!bridge.is_menu_projection_current(generation));

    let automatic = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("the already pending input can be read back");
    assert!(
        bridge
            .snapshot_received(automatic.request_generation, &context, snapshot.clone())
            .is_none()
    );
    let reopened = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .expect("explicit reopening is always available");
    assert!(!bridge.is_menu_projection_current(generation));
    assert!(bridge.is_menu_projection_current(reopened.request_generation));
}

/// Esc 之后旧输入 ACK 或无变化按键不能重开；先固定真实关闭基线，后续实际变化才能恢复。
#[test]
fn interrupted_first_snapshot_can_be_dismissed_and_new_input_recovers() {
    let mut context = available_context("session-first-loading", 1, 2, 3);
    let mut snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let pending = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Menu)
        .expect("initial menu opens without a previous snapshot");
    bridge.suspend_for_pending_input();
    assert!(!bridge.snapshot_is_current(pending.request_generation, &snapshot.binding));
    assert!(!bridge.is_request_current(pending.request_generation));
    assert!(bridge.is_menu_projection_current(pending.request_generation));
    assert!(bridge.dismiss_menu());
    assert!(
        bridge
            .snapshot_received(pending.request_generation, &context, snapshot.clone())
            .is_none()
    );

    bridge.suspend_for_pending_input();
    let next = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .expect("a later input requests a new snapshot");
    assert!(
        bridge
            .snapshot_received(next.request_generation, &context, snapshot.clone())
            .is_none()
    );
    bridge.suspend_for_pending_input();
    let unchanged = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .unwrap();
    assert!(
        bridge
            .snapshot_received(unchanged.request_generation, &context, snapshot.clone())
            .is_none()
    );
    context.input_generation += 1;
    snapshot.context = context.version();
    snapshot.revision += 1;
    snapshot.input.push('x');
    snapshot.cursor += 1;
    bridge.suspend_for_pending_input();
    let changed = bridge
        .begin_snapshot(snapshot.binding.clone(), SuggestionQueryMode::Inline)
        .unwrap();
    assert!(
        bridge
            .snapshot_received(changed.request_generation, &context, snapshot)
            .is_some()
    );
}

/// 每次从真实快照创建单次追加，避免回读失败样本重用已消费的接受状态。
fn prepared_snapshot_acceptance() -> (
    TerminalCommandSuggestionBridge,
    TerminalCommandContext,
    super::TerminalSuggestionAcceptance,
    ShellEditingSnapshot,
) {
    let context = available_context("session-accept", 1, 2, 3);
    let snapshot = editing_snapshot(&context);
    let mut bridge = enabled_bridge();
    let job = snapshot_query(
        &mut bridge,
        &context,
        &snapshot,
        SuggestionQueryMode::Inline,
    );
    assert!(bridge.apply_progress(
        TerminalSuggestionProgress {
            batch: batch(&job.query),
            snapshot_revision: snapshot.revision,
            sources: Vec::new(),
        },
        true
    ));
    let acceptance = bridge
        .accept_on_key(
            TerminalKeyEvent {
                key: TerminalKey::Right,
                phase: KeyPhase::Press,
                modifiers: InputModifiers::default(),
            },
            &context,
        )
        .expect("valid ghost can be accepted");
    let request = bridge
        .acceptance_input_request(&acceptance, 0, &context)
        .expect("acceptance enters the session FIFO once");
    assert_eq!(request.event, TerminalInputEvent::Paste("ckout".to_owned()));
    (bridge, context, acceptance, snapshot)
}

/// 确认必须同时证明同一 prompt、一次输入代次增长以及精确读回正文和光标。
#[test]
fn acceptance_requires_exact_new_readback_and_is_single_use() {
    for mismatch in 0..6 {
        let (mut bridge, mut context, acceptance, mut after) = prepared_snapshot_acceptance();
        context.input_generation += 1;
        after.context = context.version();
        after.revision += 1;
        after.input = acceptance.input_after.clone();
        after.cursor = acceptance.cursor_after;
        match mismatch {
            0 => {}
            1 => {
                context.input_generation -= 1;
                after.context = context.version();
            }
            2 => after.revision -= 1,
            3 => after.input.push('x'),
            4 => after.cursor -= 1,
            5 => {
                context.prompt_epoch += 1;
                after.context = context.version();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            bridge.confirm_shell_acceptance(&acceptance, &after, &context),
            mismatch == 0
        );
        assert!(!bridge.confirm_shell_acceptance(&acceptance, &after, &context));
    }
}

/// 反馈只承载已确认候选所属的固定本地目标、真实目录和 Shell，不从启动参数猜测。
#[test]
fn accepted_feedback_uses_original_snapshot_and_rejects_inconsistent_acceptance() {
    use kerminal_domain::{
        commands::{CommandHistoryTarget, CommandSuggestionFeedbackAction},
        sessions::{LocalSessionTarget, SessionTarget},
    };

    let (_, _, mut acceptance, _) = prepared_snapshot_acceptance();
    let target = SessionTarget::Local(LocalSessionTarget {
        program: "bash".into(),
        args: Vec::new(),
        working_directory: None,
        environment: Default::default(),
    });
    let feedback = super::accepted_feedback_request(&acceptance, &target)
        .expect("matching session may record feedback after confirmation");
    assert_eq!(feedback.action, CommandSuggestionFeedbackAction::Accepted);
    assert_eq!(feedback.target, CommandHistoryTarget::Local);
    assert_eq!(feedback.input, "git che");
    assert_eq!(feedback.replacement_text, "checkout");
    assert_eq!(feedback.cwd.as_deref(), Some("/work/project"));
    assert_eq!(feedback.shell.as_deref(), Some("bash"));
    assert_eq!(feedback.remote_host_id, None);
    acceptance.input_before.clear();
    assert!(super::accepted_feedback_request(&acceptance, &target).is_none());
}

/// Debug 日志不能意外泄露命令前缀、待追加内容或用户工作目录。
#[test]
fn acceptance_debug_redacts_shell_input_and_paths() {
    let (_, _, acceptance, _) = prepared_snapshot_acceptance();
    let diagnostic = format!("{acceptance:?}");
    assert!(!diagnostic.contains("git che"));
    assert!(!diagnostic.contains("checkout"));
    assert!(!diagnostic.contains("/work/project"));
    assert!(diagnostic.contains("[redacted]"));
}
