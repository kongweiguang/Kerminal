//! 命令行输入投影只消费同一 UI owner 的 FIFO ACK，不从终端回显推断正文。
//!
//! @author kongweiguang

use super::*;
use kerminal_domain::{
    connections::RemoteShellDialect,
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

/// 构造用于隔离 UI pane 与 session generation 的稳定绑定。
fn ui_binding(session: &str, generation: u64) -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse(session).expect("测试 session id 有效"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-main").expect("测试 tab id 有效"),
            pane_id: PaneId::parse("pane-main").expect("测试 pane id 有效"),
        },
        session_generation: generation,
    }
}

/// 构造服务明确报告的可用 shell prompt，测试不会借终端屏幕文本建立信任。
fn available_context(
    binding: &UiSessionBinding,
    prompt_epoch: u64,
    input_generation: u64,
) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: binding.session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation,
        prompt_epoch,
        input_generation,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    }
}

/// 建立与票据相同的绑定；expected context 由 FIFO 出队时的真实缓存提供。
fn input_request(
    binding: &UiSessionBinding,
    event: TerminalInputEvent,
    projection: UiInputProjection,
) -> UiTerminalInputRequest {
    UiTerminalInputRequest {
        binding: binding.clone(),
        expected_context: None,
        projection,
        event,
    }
}

/// 构造无修饰的单键事件，覆盖 readline 常用光标编辑路径。
fn key_event(key: TerminalKey) -> TerminalInputEvent {
    TerminalInputEvent::Key(TerminalKeyEvent {
        key,
        phase: KeyPhase::Press,
        modifiers: InputModifiers::default(),
    })
}

/// Enter 的 Invalidates 标记不覆盖明确 Enter 识别，测试可验证它仍有独立提交票据。
fn enter_request(binding: &UiSessionBinding) -> UiTerminalInputRequest {
    input_request(
        binding,
        key_event(TerminalKey::Enter),
        UiInputProjection::Invalidates,
    )
}

/// 用真实 ACK 的 generation 推进一个输入；ACK 后再观察相同 context 验证调用顺序约束。
fn acknowledge_edit(
    projection: &mut CommandLineProjection,
    binding: &UiSessionBinding,
    request: &UiTerminalInputRequest,
    ticket: &CommandLineInputTicket,
    mut context: TerminalCommandContext,
) -> TerminalCommandContext {
    context.input_generation = context.input_generation.saturating_add(1);
    context.prompt_line_empty = false;
    projection.apply_ack(ticket, request, &context);
    projection.observe_context(binding, &context);
    context
}

/// 字符与 Backspace 的精确投影只在同一 writer ACK context 上成立，拒绝旧版本和错光标。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
#[test]
fn acceptance_probe_projection_confirms_character_and_cleanup_cas() {
    let binding = ui_binding("session-line-acceptance", 6);
    let mut context = available_context(&binding, 4, 20);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);
    assert!(projection.confirms_exact_line(&binding, "", 0, context.version()));

    let character = input_request(
        &binding,
        TerminalInputEvent::Text("x".to_owned()),
        UiInputProjection::Tracked,
    );
    let character_ticket = projection
        .register_input(&character)
        .expect("tracked single character receives normal FIFO ticket");
    let initial_version = context.version();
    context = acknowledge_edit(
        &mut projection,
        &binding,
        &character,
        &character_ticket,
        context,
    );
    assert_eq!(
        context.input_generation,
        initial_version.input_generation + 1
    );
    assert!(projection.confirms_exact_line(&binding, "x", 1, context.version()));
    assert!(!projection.confirms_exact_line(&binding, "x", 0, context.version()));
    assert!(!projection.confirms_exact_line(&binding, "x", 1, initial_version,));

    let cleanup = input_request(
        &binding,
        key_event(TerminalKey::Backspace),
        UiInputProjection::Tracked,
    );
    let cleanup_ticket = projection
        .register_input(&cleanup)
        .expect("Backspace remains on the regular tracked input path");
    let before_cleanup = context.version();
    context = acknowledge_edit(
        &mut projection,
        &binding,
        &cleanup,
        &cleanup_ticket,
        context,
    );
    assert_eq!(
        context.input_generation,
        before_cleanup.input_generation + 1
    );
    assert!(projection.confirms_exact_line(&binding, "", 0, context.version()));
    let stale_binding = ui_binding("session-line-acceptance", 5);
    assert!(!projection.confirms_exact_line(&stale_binding, "", 0, context.version()));
}

/// 正文只在前序 FIFO 输入 ACK 后可提交，且票据 Debug 不含命令内容。
#[test]
fn submission_waits_for_prior_acks_and_ticket_hides_text() {
    let binding = ui_binding("session-line-a", 4);
    let mut context = available_context(&binding, 3, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let first = input_request(
        &binding,
        TerminalInputEvent::Text("echo ".to_owned()),
        UiInputProjection::Tracked,
    );
    let first_ticket = projection.register_input(&first).expect("可追踪首段输入");
    let second = input_request(
        &binding,
        TerminalInputEvent::Text("你好".to_owned()),
        UiInputProjection::Tracked,
    );
    let second_ticket = projection
        .register_input(&second)
        .expect("可追踪 Unicode 输入");
    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 独立保留 FIFO 票据");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .is_none()
    );
    assert!(!format!("{first_ticket:?}").contains("echo"));

    context = acknowledge_edit(&mut projection, &binding, &first, &first_ticket, context);
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .is_none()
    );
    context = acknowledge_edit(&mut projection, &binding, &second, &second_ticket, context);
    projection.apply_failure(&second_ticket);

    let submission = projection
        .prepare_submission(&enter_ticket, &enter, &context)
        .expect("前序输入 ACK 后才捕获正文");
    assert_eq!(submission.command, "echo 你好");
    assert_eq!(submission.expected_context, context.version());
}

/// Unicode 文本追加不需要猜测 grapheme 光标，前序 ACK 后仍可提交完整正文。
#[test]
fn unicode_append_remains_trackable_without_cursor_edits() {
    let binding = ui_binding("session-line-b", 2);
    let mut context = available_context(&binding, 1, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let text = input_request(
        &binding,
        TerminalInputEvent::Text("echo 中🙂".to_owned()),
        UiInputProjection::Tracked,
    );
    let text_ticket = projection.register_input(&text).expect("初始文本可追踪");
    context = acknowledge_edit(&mut projection, &binding, &text, &text_ticket, context);

    let enter = enter_request(&binding);
    let enter_ticket = projection.register_input(&enter).expect("Enter 有提交票据");
    let submission = projection
        .prepare_submission(&enter_ticket, &enter, &context)
        .expect("Unicode 文本追加没有依赖光标推断");
    assert_eq!(submission.command, "echo 中🙂");
}

/// shell 对组合字符和 ZWJ grapheme 的光标/删除行为未被确认，逐 code point 推断会伪造正文。
#[test]
fn grapheme_navigation_and_deletion_invalidate_non_ascii_lines() {
    for line in ["echo e\u{301}", "echo 👩‍💻"] {
        for key in [
            TerminalKey::Backspace,
            TerminalKey::Delete,
            TerminalKey::Left,
            TerminalKey::Right,
            TerminalKey::Home,
            TerminalKey::End,
        ] {
            let binding = ui_binding("session-line-grapheme", 3);
            let mut context = available_context(&binding, 1, 0);
            let mut projection = CommandLineProjection::default();
            projection.observe_context(&binding, &context);

            let text = input_request(
                &binding,
                TerminalInputEvent::Text(line.to_owned()),
                UiInputProjection::Tracked,
            );
            let text_ticket = projection
                .register_input(&text)
                .expect("Unicode 追加可跟踪");
            context = acknowledge_edit(&mut projection, &binding, &text, &text_ticket, context);

            let edit = input_request(&binding, key_event(key), UiInputProjection::Tracked);
            let edit_ticket = projection
                .register_input(&edit)
                .expect("编辑先有真实 ACK 票据");
            context = acknowledge_edit(&mut projection, &binding, &edit, &edit_ticket, context);

            let enter = enter_request(&binding);
            let enter_ticket = projection
                .register_input(&enter)
                .expect("Enter 仍照常送往终端");
            assert!(
                projection
                    .prepare_submission(&enter_ticket, &enter, &context)
                    .is_none(),
                "shell grapheme 行为未知时必须拒绝推断提交正文: {line:?}, {key:?}"
            );
        }
    }
}

/// Paste 或失败写入会立即封锁当前行；只有新的 prompt epoch 才能重新建立空基线。
#[test]
fn paste_and_failed_write_invalidate_until_fresh_prompt() {
    let binding = ui_binding("session-line-c", 7);
    let mut context = available_context(&binding, 2, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let text = input_request(
        &binding,
        TerminalInputEvent::Text("stale".to_owned()),
        UiInputProjection::Tracked,
    );
    let text_ticket = projection.register_input(&text).expect("初始输入可追踪");
    context = acknowledge_edit(&mut projection, &binding, &text, &text_ticket, context);

    let paste = input_request(
        &binding,
        TerminalInputEvent::Paste("replacement".to_owned()),
        UiInputProjection::Invalidates,
    );
    assert!(projection.register_input(&paste).is_none());
    let enter = enter_request(&binding);
    let enter_ticket = projection.register_input(&enter).expect("Enter 仍交给终端");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .is_none()
    );

    projection.apply_failure(&enter_ticket);
    projection.apply_failure(&enter_ticket);
    context = available_context(&binding, 3, context.input_generation);
    projection.observe_context(&binding, &context);

    let fresh = input_request(
        &binding,
        TerminalInputEvent::Text("fresh".to_owned()),
        UiInputProjection::Tracked,
    );
    let fresh_ticket = projection
        .register_input(&fresh)
        .expect("新 prompt 建立可信空行");
    context = acknowledge_edit(&mut projection, &binding, &fresh, &fresh_ticket, context);
    let fresh_enter = enter_request(&binding);
    let fresh_enter_ticket = projection
        .register_input(&fresh_enter)
        .expect("新 prompt 的 Enter 有票据");
    assert_eq!(
        projection
            .prepare_submission(&fresh_enter_ticket, &fresh_enter, &context)
            .expect("新 prompt 行可提交")
            .command,
        "fresh"
    );
}

/// 失败的普通编辑不保留旧行；迟到成功 ACK 不能把未确认字符拼回可信正文。
#[test]
fn failed_edit_keeps_buffer_unknown() {
    let binding = ui_binding("session-line-failed", 5);
    let mut context = available_context(&binding, 1, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let first = input_request(
        &binding,
        TerminalInputEvent::Text("base".to_owned()),
        UiInputProjection::Tracked,
    );
    let first_ticket = projection.register_input(&first).expect("基线文本可追踪");
    context = acknowledge_edit(&mut projection, &binding, &first, &first_ticket, context);

    let failed = input_request(
        &binding,
        TerminalInputEvent::Text("!".to_owned()),
        UiInputProjection::Tracked,
    );
    let failed_ticket = projection
        .register_input(&failed)
        .expect("编辑先有待确认票据");
    projection.apply_failure(&failed_ticket);
    let mut late_ack = context.clone();
    late_ack.input_generation += 1;
    late_ack.prompt_line_empty = false;
    projection.apply_ack(&failed_ticket, &failed, &late_ack);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 继续正常写入终端");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .is_none()
    );
}

/// Enter 成功 ACK 后旧行立即作废，只有服务报告的新 prompt 能开始下一行。
#[test]
fn successful_submission_closes_old_line_generation() {
    let binding = ui_binding("session-line-submit", 6);
    let mut context = available_context(&binding, 4, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let text = input_request(
        &binding,
        TerminalInputEvent::Text("first".to_owned()),
        UiInputProjection::Tracked,
    );
    let text_ticket = projection.register_input(&text).expect("首条命令可追踪");
    context = acknowledge_edit(&mut projection, &binding, &text, &text_ticket, context);
    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 已排在正文后");
    assert_eq!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .expect("已 ACK 的行可提交")
            .command,
        "first"
    );

    let mut submit_ack = context.clone();
    submit_ack.input_generation += 1;
    submit_ack.prompt_line_empty = false;
    submit_ack.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);
    projection.apply_submission_ack(&enter_ticket, &enter, &submit_ack);
    let before_prompt = enter_request(&binding);
    let before_prompt_ticket = projection
        .register_input(&before_prompt)
        .expect("旧 prompt 下 Enter 仍交给终端");
    assert!(
        projection
            .prepare_submission(&before_prompt_ticket, &before_prompt, &submit_ack)
            .is_none()
    );

    let next_prompt = available_context(&binding, 5, submit_ack.input_generation);
    projection.observe_context(&binding, &next_prompt);
    let next_text = input_request(
        &binding,
        TerminalInputEvent::Text("second".to_owned()),
        UiInputProjection::Tracked,
    );
    let next_ticket = projection
        .register_input(&next_text)
        .expect("新 prompt 可建立下一条输入");
    let next_ack = acknowledge_edit(
        &mut projection,
        &binding,
        &next_text,
        &next_ticket,
        next_prompt,
    );
    let next_enter = enter_request(&binding);
    let next_enter_ticket = projection
        .register_input(&next_enter)
        .expect("新命令 Enter 票据");
    assert_eq!(
        projection
            .prepare_submission(&next_enter_ticket, &next_enter, &next_ack)
            .expect("下一行独立于已提交命令")
            .command,
        "second"
    );
}

/// OSC A 的 PromptNotReady 短暂状态不要求跳过整个 epoch，OSC B 到达后同一空行可安全初始化。
#[test]
fn prompt_not_ready_can_seed_after_prompt_end_same_epoch() {
    let binding = ui_binding("session-line-prompt", 8);
    let mut context = available_context(&binding, 9, 0);
    context.prompt_ready = false;
    context.prompt_line_empty = false;
    context.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    context.prompt_ready = true;
    context.prompt_line_empty = true;
    context.availability = TerminalCommandAvailability::Available;
    projection.observe_context(&binding, &context);
    let text = input_request(
        &binding,
        TerminalInputEvent::Text("ready".to_owned()),
        UiInputProjection::Tracked,
    );
    let ticket = projection
        .register_input(&text)
        .expect("OSC B 后同一 prompt 建立空行");
    let ack = acknowledge_edit(&mut projection, &binding, &text, &ticket, context);
    let enter = enter_request(&binding);
    let enter_ticket = projection.register_input(&enter).expect("Enter 票据");
    assert_eq!(
        projection
            .prepare_submission(&enter_ticket, &enter, &ack)
            .expect("可信输入可提交")
            .command,
        "ready"
    );
}

/// 首个字符可能在 OSC A/B 后到达，但 Root 尚未观察到新 epoch；仅首条 tracked ACK 的 epoch+1/delta=1 可建行。
#[test]
fn first_tracked_input_ack_can_seed_after_initial_prompt_becomes_ready() {
    let binding = ui_binding("session-line-first-prompt-input", 3);
    let mut initial = available_context(&binding, 8, 0);
    initial.prompt_ready = false;
    initial.prompt_line_empty = false;
    initial.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &initial);

    let first = input_request(
        &binding,
        TerminalInputEvent::Text("p".to_owned()),
        UiInputProjection::Tracked,
    );
    let first_ticket = projection
        .register_input(&first)
        .expect("未知行的 tracked 输入仍需无正文 ACK 票据");

    // 该 ACK 将 prompt A/B 的新 epoch 与唯一已接受字符一起交给 projection。
    let mut first_ack = available_context(&binding, 9, 1);
    first_ack.prompt_line_empty = false;
    projection.apply_ack(&first_ticket, &first, &first_ack);
    projection.observe_context(&binding, &first_ack);

    for character in ["w", "d"] {
        let remaining = input_request(
            &binding,
            TerminalInputEvent::Text(character.to_owned()),
            UiInputProjection::Tracked,
        );
        let remaining_ticket = projection
            .register_input(&remaining)
            .expect("首个 ACK 已建立可信行，后续 tracked 输入可继续");
        first_ack = acknowledge_edit(
            &mut projection,
            &binding,
            &remaining,
            &remaining_ticket,
            first_ack,
        );
    }

    let enter = enter_request(&binding);
    let enter_ticket = projection.register_input(&enter).expect("Enter 有独立票据");
    assert_eq!(
        projection
            .prepare_submission(&enter_ticket, &enter, &first_ack)
            .expect("真实首字符 ACK 与后续 ACK 形成完整行")
            .command,
        "pwd"
    );
}

/// 同 epoch 的未知行不因一个 tracked ACK 变成可信行；该事件不能倒推出此前未观测的正文。
#[test]
fn same_epoch_unknown_line_stays_unknown_after_tracked_ack() {
    let binding = ui_binding("session-line-same-epoch-unknown", 2);
    let mut dirty_context = available_context(&binding, 4, 3);
    dirty_context.prompt_line_empty = false;
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &dirty_context);

    let guessed = input_request(
        &binding,
        TerminalInputEvent::Text("guessed".to_owned()),
        UiInputProjection::Tracked,
    );
    let guessed_ticket = projection
        .register_input(&guessed)
        .expect("tracked 输入可取得无正文票据，但尚无可提交正文");
    let mut ack = dirty_context.clone();
    ack.input_generation += 1;
    projection.apply_ack(&guessed_ticket, &guessed, &ack);
    projection.observe_context(&binding, &ack);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 仍可交给终端");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &ack)
            .is_none(),
        "同 epoch 的 unknown line 不得从单次 ACK 恢复"
    );
}

/// Paste 与外部输入必须等新 prompt；未知行不能借单次 ACK 猜测恢复。
#[test]
fn untracked_or_external_input_cannot_recover_unknown_line_in_same_epoch() {
    for (session_id, invalidation) in [
        ("session-line-unknown-paste", "paste"),
        ("session-line-unknown-external", "external"),
    ] {
        let binding = ui_binding(session_id, 5);
        let context = available_context(&binding, 6, 0);
        let mut projection = CommandLineProjection::default();
        projection.observe_context(&binding, &context);

        let existing = input_request(
            &binding,
            TerminalInputEvent::Text("base".to_owned()),
            UiInputProjection::Tracked,
        );
        let existing_ticket = projection
            .register_input(&existing)
            .expect("已确认空 prompt 可正常追踪初始正文");
        let context = acknowledge_edit(
            &mut projection,
            &binding,
            &existing,
            &existing_ticket,
            context,
        );

        match invalidation {
            "paste" => {
                let paste = input_request(
                    &binding,
                    TerminalInputEvent::Paste("replacement".to_owned()),
                    UiInputProjection::Invalidates,
                );
                assert!(projection.register_input(&paste).is_none());
            }
            "external" => projection.invalidate_binding(&binding),
            _ => unreachable!("测试只声明三类未知输入"),
        }

        let later = input_request(
            &binding,
            TerminalInputEvent::Text("later".to_owned()),
            UiInputProjection::Tracked,
        );
        let later_ticket = projection
            .register_input(&later)
            .expect("后续 tracked 编辑仍可送达，但 ACK 不得猜测未知基线");
        let mut later_ack = context.clone();
        later_ack.input_generation += 1;
        later_ack.prompt_line_empty = false;
        projection.apply_ack(&later_ticket, &later, &later_ack);
        projection.observe_context(&binding, &later_ack);

        let enter = enter_request(&binding);
        let enter_ticket = projection.register_input(&enter).expect("Enter 仍交给终端");
        assert!(
            projection
                .prepare_submission(&enter_ticket, &enter, &later_ack)
                .is_none(),
            "同 epoch 的未知行不能跨过 invalidation: {session_id}"
        );
    }
}

/// 已知 prompt 上的 committed composition 也必须经过 FIFO ACK 才能参与命令候选。
///
/// @author kongweiguang
#[test]
fn committed_composition_on_known_line_requires_ack() {
    let binding = ui_binding("session-line-ime-known", 5);
    let context = available_context(&binding, 6, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let base = input_request(
        &binding,
        TerminalInputEvent::Text("base".to_owned()),
        UiInputProjection::Tracked,
    );
    let base_ticket = projection.register_input(&base).expect("base ticket");
    let context = acknowledge_edit(&mut projection, &binding, &base, &base_ticket, context);

    let mut composition = input_request(
        &binding,
        TerminalInputEvent::Text("composition".to_owned()),
        UiInputProjection::Invalidates,
    );
    let composition_ticket = projection
        .register_input(&composition)
        .expect("known line may register committed UI Text");
    let mut wrong_binding = composition.clone();
    wrong_binding.binding = ui_binding("session-line-ime-other", 5);
    projection.promote_registered_committed_text(&mut wrong_binding, Some(&composition_ticket));
    assert_eq!(wrong_binding.projection, UiInputProjection::Invalidates);
    projection.promote_registered_committed_text(&mut composition, Some(&composition_ticket));
    assert_eq!(composition.projection, UiInputProjection::Tracked);
    let context = acknowledge_edit(
        &mut projection,
        &binding,
        &composition,
        &composition_ticket,
        context,
    );

    let enter = enter_request(&binding);
    let enter_ticket = projection.register_input(&enter).expect("Enter ticket");
    let submission = projection
        .prepare_submission(&enter_ticket, &enter, &context)
        .expect("only the acknowledged text is eligible");
    assert_eq!(submission.command, "basecomposition");
}

/// 未知行上的 composition commit 不能借单个 ACK 猜出此前被隐藏的命令前缀。
///
/// @author kongweiguang
#[test]
fn committed_composition_does_not_recover_an_unknown_line() {
    let binding = ui_binding("session-line-ime-unknown", 5);
    let mut context = available_context(&binding, 6, 0);
    context.prompt_line_empty = false;
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let mut composition = input_request(
        &binding,
        TerminalInputEvent::Text("pwd".to_owned()),
        UiInputProjection::Invalidates,
    );
    let ticket = projection.register_input(&composition);
    assert!(ticket.is_none());
    projection.promote_registered_committed_text(&mut composition, ticket.as_ref());
    assert_eq!(composition.projection, UiInputProjection::Invalidates);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter still reaches the shell");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &context)
            .is_none()
    );
}

/// 多项未 ACK 输入导致新 prompt 首次回执跨过多个 generation 时，不能把不连续增量猜成可信正文。
#[test]
fn multiple_pending_inputs_with_noncontiguous_fresh_prompt_delta_stay_unknown() {
    let binding = ui_binding("session-line-noncontiguous-first-ack", 4);
    let mut initial = available_context(&binding, 2, 0);
    initial.prompt_ready = false;
    initial.prompt_line_empty = false;
    initial.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &initial);

    let first = input_request(
        &binding,
        TerminalInputEvent::Text("p".to_owned()),
        UiInputProjection::Tracked,
    );
    let first_ticket = projection
        .register_input(&first)
        .expect("未知行首项取得无正文票据");
    let second = input_request(
        &binding,
        TerminalInputEvent::Text("w".to_owned()),
        UiInputProjection::Tracked,
    );
    let second_ticket = projection
        .register_input(&second)
        .expect("同一 FIFO 后项保留原始序号等待严格 ACK");

    let mut skipped_ack = available_context(&binding, 3, 2);
    skipped_ack.prompt_line_empty = false;
    projection.apply_ack(&first_ticket, &first, &skipped_ack);
    // 第一 ACK 已因 delta=2 使 line generation 失效，迟到的第二 ACK 不得补全这行。
    projection.apply_ack(&second_ticket, &second, &skipped_ack);
    projection.observe_context(&binding, &skipped_ack);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 仍送达 shell");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &skipped_ack)
            .is_none(),
        "fresh prompt delta 必须恰为一个已确认输入"
    );
}

/// 当前输入 ACK 恰逢新 prompt 时保留同一 FIFO 后续票据，不丢掉已经排队的字符。
#[test]
fn first_ack_in_new_prompt_preserves_later_fifo_edits() {
    let binding = ui_binding("session-line-d", 1);
    let context = available_context(&binding, 1, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);

    let first = input_request(
        &binding,
        TerminalInputEvent::Text("a".to_owned()),
        UiInputProjection::Tracked,
    );
    let first_ticket = projection.register_input(&first).expect("首字母可追踪");
    let second = input_request(
        &binding,
        TerminalInputEvent::Text("b".to_owned()),
        UiInputProjection::Tracked,
    );
    let second_ticket = projection.register_input(&second).expect("后续字符可追踪");

    let mut new_prompt_ack = available_context(&binding, 2, 1);
    new_prompt_ack.prompt_line_empty = false;
    projection.apply_ack(&first_ticket, &first, &new_prompt_ack);
    let second_ack = available_context(&binding, 2, 2);
    let mut second_ack = second_ack;
    second_ack.prompt_line_empty = false;
    projection.apply_ack(&second_ticket, &second, &second_ack);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("新 prompt Enter 有票据");
    assert_eq!(
        projection
            .prepare_submission(&enter_ticket, &enter, &second_ack)
            .expect("连续 ACK 形成完整行")
            .command,
        "ab"
    );
}

/// 连接代次替换后，旧 pane 的迟到 ACK 不能污染新绑定的输入正文。
#[test]
fn reconnect_generation_rejects_old_ack() {
    let old_binding = ui_binding("session-line-e", 1);
    let mut old_context = available_context(&old_binding, 2, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&old_binding, &old_context);
    let old_request = input_request(
        &old_binding,
        TerminalInputEvent::Text("old".to_owned()),
        UiInputProjection::Tracked,
    );
    let old_ticket = projection
        .register_input(&old_request)
        .expect("旧绑定文本可追踪");

    let new_binding = ui_binding("session-line-e", 2);
    let new_context = available_context(&new_binding, 1, 0);
    projection.observe_context(&new_binding, &new_context);
    old_context.input_generation = 1;
    old_context.prompt_line_empty = false;
    projection.apply_ack(&old_ticket, &old_request, &old_context);

    let enter = enter_request(&new_binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("新连接仍接收 Enter");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &new_context)
            .is_none()
    );
}

/// 命令长度达到协议上限时整行转 Unknown，而不是继续分配无界投影缓冲。
#[test]
fn oversized_input_cannot_become_a_submission() {
    let binding = ui_binding("session-line-f", 3);
    let context = available_context(&binding, 1, 0);
    let mut projection = CommandLineProjection::default();
    projection.observe_context(&binding, &context);
    let request = input_request(
        &binding,
        TerminalInputEvent::Text("x".repeat(MAX_COMMAND_BLOCK_TEXT_BYTES + 1)),
        UiInputProjection::Tracked,
    );
    let ticket = projection
        .register_input(&request)
        .expect("输入操作仍可写入终端");
    let mut ack = available_context(&binding, 1, 1);
    ack.prompt_line_empty = false;
    projection.apply_ack(&ticket, &request, &ack);

    let enter = enter_request(&binding);
    let enter_ticket = projection
        .register_input(&enter)
        .expect("Enter 仍发送到终端");
    assert!(
        projection
            .prepare_submission(&enter_ticket, &enter, &ack)
            .is_none()
    );
}
