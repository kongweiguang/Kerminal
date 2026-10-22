//! 验证隔离终端单字符探针的输入与清理边界。
//!
//! @author kongweiguang

use super::*;
use crate::acceptance_metrics::{
    TERMINAL_ECHO_TEST_CHARACTER, TerminalEchoAdmissionRejection, TerminalEchoPaintObservation,
    TerminalEchoProbeRegistry,
};
use kerminal_application::sessions::TerminalCommandUnavailableReason;
use kerminal_domain::{connections::RemoteShellDialect, identity::PaneId};
use kerminal_domain::{identity::SessionId, sessions::TerminalSize};
use kerminal_terminal::{
    engine::{TerminalEngine, TerminalLimits},
    snapshot::{RenderSnapshot, RenderSnapshotRequest, TerminalTextCellSpan},
};
use std::time::{Duration, Instant};

/// 构造与 fresh UI pane 对应的完整会话绑定，用于固定 context 分类优先级。
///
/// @author kongweiguang
fn ui_binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::parse("acceptance-session").expect("valid session id"),
        owner: SessionOwner::Ui {
            tab_id: kerminal_domain::identity::TabId::parse("acceptance-tab")
                .expect("valid tab id"),
            pane_id: PaneId::parse("acceptance-pane").expect("valid pane id"),
        },
        session_generation: 4,
    }
}

/// 构造服务明确报告的可用 prompt，不从 Canvas 文本或 shell 输出推断可写状态。
///
/// @author kongweiguang
fn ready_context(binding: &UiSessionBinding) -> TerminalCommandContext {
    TerminalCommandContext {
        session_id: binding.session_id.clone(),
        owner: binding.owner.clone(),
        session_generation: binding.session_generation,
        prompt_epoch: 2,
        input_generation: 0,
        shell_dialect: RemoteShellDialect::Posix,
        prompt_ready: true,
        prompt_line_empty: true,
        alternate_screen: false,
        availability: TerminalCommandAvailability::Available,
    }
}

/// 构造由真实 Alacritty grid 渲染的快照，使空白断言保留引擎生成的每个 cell span。
///
/// @author kongweiguang
fn engine_snapshot(columns: u16, rows: u16, output: &[u8]) -> RenderSnapshot {
    let size = TerminalSize::new(columns, rows, 0, 0).expect("positive terminal grid");
    let mut engine = TerminalEngine::new(SessionId::new(), size, TerminalLimits::default())
        .expect("bounded test terminal");
    if !output.is_empty() {
        engine.process_output(output);
    }
    engine.render_snapshot(RenderSnapshotRequest::default())
}

/// 从真实渲染快照读出光标 glyph 与列范围，防止拒绝用例因输出未进入 grid 而虚假通过。
///
/// @author kongweiguang
fn cursor_glyph(snapshot: &RenderSnapshot) -> Option<(&str, std::ops::Range<u16>)> {
    let row = snapshot
        .rows
        .iter()
        .find(|row| row.viewport_row == snapshot.cursor.row)?;
    let (run, span) = row
        .runs
        .iter()
        .flat_map(|run| run.cell_spans.iter().map(move |span| (run, span)))
        .find(|(_, span)| span.column_range.contains(&snapshot.cursor.column))?;
    Some((
        run.text.get(span.byte_range.clone())?,
        span.column_range.clone(),
    ))
}

/// 只改真实快照中 cursor 对应 span 的一个坐标字段，用于验证损坏坐标不会被解释为空格。
///
/// @author kongweiguang
fn mutate_cursor_span(
    snapshot: &RenderSnapshot,
    mutate: impl FnOnce(&mut TerminalTextCellSpan),
) -> RenderSnapshot {
    let row_index = usize::from(snapshot.cursor.row);
    let mut rows = snapshot.rows.to_vec();
    let mut row = rows[row_index].clone();
    let mut runs = row.runs.to_vec();
    let run_index = runs
        .iter()
        .position(|run| {
            run.cell_spans
                .iter()
                .any(|span| span.column_range.contains(&snapshot.cursor.column))
        })
        .expect("real blank grid includes the cursor cell span");
    let mut run = runs[run_index].clone();
    let mut spans = run.cell_spans.to_vec();
    let span = spans
        .iter_mut()
        .find(|span| span.column_range.contains(&snapshot.cursor.column))
        .expect("cursor span exists in selected run");
    mutate(span);
    run.cell_spans = spans.into();
    runs[run_index] = run;
    row.runs = runs.into();
    rows[row_index] = row;
    let mut mutated = snapshot.clone();
    mutated.rows = rows.into();
    mutated
}

/// 验证仅允许安全的交互 shell 参数，命令行参数不能把单字符送入任意程序 stdin。
///
/// @author kongweiguang
#[test]
fn shell_allowlist_rejects_command_arguments() {
    assert!(supported_interactive_shell(
        Path::new("pwsh").as_os_str(),
        &[]
    ));
    assert!(supported_interactive_shell(
        Path::new("bash").as_os_str(),
        &[]
    ));
    assert!(supported_interactive_shell(
        Path::new("cmd.exe").as_os_str(),
        &[]
    ));
    assert!(!powershell_arguments_are_interactive(&[OsString::from(
        "-Command"
    )]));
    assert!(!posix_shell_arguments_are_interactive(
        "bash",
        &[OsString::from("-c")]
    ));
    assert!(!cmd_arguments_are_interactive(&[OsString::from("/c")]));
}

/// 验证探针只经 Tracked 单字符输入且没有 Enter；清理使用同一精确 context 的普通 Backspace 事件。
///
/// @author kongweiguang
#[test]
fn character_and_cleanup_requests_use_the_regular_writer_gate() {
    let binding = ui_binding();
    let context = ready_context(&binding);
    let request = terminal_echo_input_request(
        &binding,
        &context,
        TerminalInputEvent::Text(TERMINAL_ECHO_TEST_CHARACTER.to_owned()),
    );
    assert_eq!(request.binding, binding);
    assert_eq!(request.expected_context, Some(context.version()));
    assert_eq!(request.projection, UiInputProjection::Tracked);
    assert_eq!(
        request.event,
        TerminalInputEvent::Text("x".to_owned()),
        "synthetic probe sends one character and never submits Enter"
    );

    let cleanup = terminal_echo_input_request(
        &binding,
        &context,
        TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Backspace,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        }),
    );
    assert_eq!(cleanup.binding, binding);
    assert_eq!(cleanup.expected_context, Some(context.version()));
    assert_eq!(cleanup.projection, UiInputProjection::Tracked);
    assert_eq!(
        cleanup.event,
        TerminalInputEvent::Key(TerminalKeyEvent {
            key: TerminalKey::Backspace,
            phase: KeyPhase::Press,
            modifiers: InputModifiers::default(),
        })
    );
}

/// 验证空行标志不能覆盖 invalidated availability；只有服务层 PromptStart 恢复为 Available。
///
/// @author kongweiguang
#[test]
fn projection_invalidated_context_is_countable_and_stays_ineligible() {
    let binding = ui_binding();
    let mut context = ready_context(&binding);
    context.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::UiInputProjectionInvalidated,
    );
    assert!(context.prompt_ready);
    assert!(context.prompt_line_empty);
    assert_eq!(
        terminal_echo_context_rejection(&binding, &context, None),
        Some(TerminalEchoAdmissionRejection::UiInputProjectionInvalidated)
    );
    assert!(terminal_echo_context_rejection(&binding, &context, None).is_some());
}

/// 验证 echo probe 仍要求完整 binding 和服务确认的可写 prompt。
///
/// @author kongweiguang
#[test]
fn available_context_is_the_only_admitted_state() {
    let binding = ui_binding();
    let context = ready_context(&binding);
    assert_eq!(
        terminal_echo_context_rejection(&binding, &context, None),
        None
    );
    assert!(terminal_echo_context_rejection(&binding, &context, None).is_none());
}

/// 验证实际 registry 流程：paint 与 cleanup ACK 都闭环才发放凭证；超时后的 fallback 清理不能授权复测。
///
/// @author kongweiguang
#[test]
fn clean_prompt_receipt_requires_registry_canvas_and_cleanup_ack() {
    let binding = ui_binding();
    let target = TerminalEchoTarget {
        viewport_row: 3,
        column: 9,
    };
    let registry = TerminalEchoProbeRegistry::default();
    let probe = registry.issue_probe(
        binding.session_id.to_string(),
        binding.session_generation,
        target,
    );
    let initial = TerminalCommandContextVersion {
        session_generation: binding.session_generation,
        prompt_epoch: 2,
        input_generation: 40,
    };
    let character_ack = TerminalCommandContextVersion {
        input_generation: 41,
        ..initial
    };
    let cleanup_ack = TerminalCommandContextVersion {
        input_generation: 42,
        ..initial
    };
    let submitted_at = Instant::now();
    assert!(registry.record_submission(
        &probe,
        binding.session_id.as_str(),
        binding.session_generation,
        initial,
        50,
        submitted_at,
    ));
    assert!(registry.record_enqueued(&probe));
    registry.record_canvas_character_paint(
        binding.session_id.as_str(),
        binding.session_generation,
        51,
        target,
        TerminalEchoPaintObservation {
            target_cell_is_character: true,
            painted_run_covers_target: true,
            cursor_visible: true,
            cursor_row: target.viewport_row,
            cursor_column: target.column + 1,
            at_bottom: true,
            window_active_at_paint: true,
        },
        submitted_at + Duration::from_millis(1),
    );
    assert!(registry.record_writer_ack(
        &probe,
        character_ack,
        true,
        true,
        submitted_at + Duration::from_millis(2),
    ));
    let has_canvas_sample = registry.sample_is_ready(&probe);
    assert!(has_canvas_sample);
    assert!(registry.authorize_cleanup(&probe, character_ack, true, true));
    assert!(registry.record_cleanup_enqueued(&probe));
    let cleanup_recorded = registry.record_cleanup_ack(&probe, cleanup_ack, true, true);
    assert!(cleanup_recorded);
    let exact_cleanup_version =
        terminal_echo_clean_prompt_version(cleanup_recorded, has_canvas_sample, cleanup_ack)
            .expect("a registry sample plus the exact cleanup ACK creates a next-loop proof");
    assert_eq!(exact_cleanup_version, cleanup_ack);
    let mut dirty_service_context = ready_context(&binding);
    dirty_service_context.input_generation = cleanup_ack.input_generation;
    dirty_service_context.prompt_line_empty = false;
    assert_eq!(
        terminal_echo_context_rejection(
            &binding,
            &dirty_service_context,
            Some(exact_cleanup_version),
        ),
        None,
        "the exact service receipt allows another probe after the known tracked backspace"
    );
    let mut raced_context = dirty_service_context.clone();
    raced_context.input_generation += 1;
    assert_eq!(
        terminal_echo_context_rejection(&binding, &raced_context, Some(exact_cleanup_version)),
        Some(TerminalEchoAdmissionRejection::PromptLineNotEmpty),
        "a later input generation invalidates the cleanup receipt"
    );
    let mut unavailable_context = dirty_service_context;
    unavailable_context.availability = TerminalCommandAvailability::Unavailable(
        TerminalCommandUnavailableReason::ShellIntegrationNotObserved,
    );
    assert_eq!(
        terminal_echo_context_rejection(
            &binding,
            &unavailable_context,
            Some(exact_cleanup_version),
        ),
        Some(TerminalEchoAdmissionRejection::ShellIntegrationNotObserved),
        "the receipt cannot bypass the service availability gate"
    );

    // Paint 超时后仍完成已发送字符的 Backspace；这次 fallback 不能升级成复测凭证。
    let fallback_registry = TerminalEchoProbeRegistry::default();
    let fallback = fallback_registry.issue_probe(
        binding.session_id.to_string(),
        binding.session_generation,
        target,
    );
    let fallback_start = Instant::now();
    assert!(fallback_registry.record_submission(
        &fallback,
        binding.session_id.as_str(),
        binding.session_generation,
        initial,
        60,
        fallback_start,
    ));
    assert!(fallback_registry.record_enqueued(&fallback));
    assert!(fallback_registry.record_writer_ack(
        &fallback,
        character_ack,
        true,
        true,
        fallback_start + Duration::from_millis(1),
    ));
    assert_eq!(
        fallback_registry.expire_probes(fallback_start + Duration::from_secs(60)),
        1,
        "missing Canvas paint enters the timeout path"
    );
    let fallback_has_canvas_sample = fallback_registry.sample_is_ready(&fallback);
    assert!(!fallback_has_canvas_sample);
    assert!(fallback_registry.authorize_cleanup(&fallback, character_ack, true, true));
    assert!(fallback_registry.record_cleanup_enqueued(&fallback));
    let fallback_cleanup_recorded =
        fallback_registry.record_cleanup_ack(&fallback, cleanup_ack, true, true);
    assert!(
        fallback_cleanup_recorded,
        "safe cleanup still closes the writer ACK"
    );
    assert_eq!(
        terminal_echo_clean_prompt_version(
            fallback_cleanup_recorded,
            fallback_has_canvas_sample,
            cleanup_ack,
        ),
        None,
        "cleanup without matched Canvas paint cannot authorize another character"
    );
}

/// 验证真实会话数未达到 workload target 时会跳过本轮 probe，缺省配置不增加准入限制。
///
/// @author kongweiguang
#[test]
fn expected_session_count_gate_rejects_incomplete_workload() {
    assert_eq!(
        terminal_echo_session_count_rejection(Some(50), 49),
        Some(TerminalEchoAdmissionRejection::SessionCountMismatch)
    );
    assert_eq!(terminal_echo_session_count_rejection(Some(50), 50), None);
    assert_eq!(terminal_echo_session_count_rejection(None, 49), None);
}

/// 验证 probe admission 将已通过服务端门禁的同一 prompt context 同步到投影，且不越过失效屏障。
///
/// @author kongweiguang
#[cfg(feature = "acceptance-metrics")]
#[test]
fn probe_admission_reconciles_late_available_prompt_into_empty_projection() {
    let binding = ui_binding();
    let mut pending_prompt = ready_context(&binding);
    pending_prompt.prompt_epoch = 2;
    pending_prompt.prompt_ready = false;
    pending_prompt.prompt_line_empty = false;
    pending_prompt.availability =
        TerminalCommandAvailability::Unavailable(TerminalCommandUnavailableReason::PromptNotReady);

    let mut projection = super::super::command_line_projection::CommandLineProjection::default();
    projection.observe_context(&binding, &pending_prompt);
    assert!(!projection.confirms_exact_line(&binding, "", 0, pending_prompt.version(),));

    let mut available_prompt = pending_prompt;
    available_prompt.prompt_ready = true;
    available_prompt.prompt_line_empty = true;
    available_prompt.availability = TerminalCommandAvailability::Available;
    assert_eq!(
        terminal_echo_context_rejection(&binding, &available_prompt, None),
        None
    );
    assert!(!projection.confirms_exact_line(&binding, "", 0, available_prompt.version(),));
    assert!(observe_and_confirm_empty_terminal_echo_projection(
        &mut projection,
        &binding,
        &available_prompt,
    ));

    projection.invalidate_binding(&binding);
    assert!(!observe_and_confirm_empty_terminal_echo_projection(
        &mut projection,
        &binding,
        &available_prompt,
    ));
}

/// 验证仅真实 ASCII 单列空格 span 可作为探针目标，缺失、损坏或非空 glyph 均安全拒绝。
///
/// @author kongweiguang
#[test]
fn snapshot_target_requires_an_exact_ascii_space_cell_span() {
    let blank = engine_snapshot(8, 2, &[]);
    let (blank_glyph, blank_columns) =
        cursor_glyph(&blank).expect("real blank grid includes every visible cell span");
    assert_eq!(blank_glyph, " ");
    assert_eq!(blank_columns, blank.cursor.column..blank.cursor.column + 1);
    assert_eq!(
        terminal_echo_snapshot_target(&blank),
        Some(TerminalEchoTarget {
            viewport_row: blank.cursor.row,
            column: blank.cursor.column,
        })
    );

    let occupied = engine_snapshot(8, 2, b"x\x1b[1;1H");
    assert_eq!(cursor_glyph(&occupied).map(|(glyph, _)| glyph), Some("x"));
    assert_eq!(terminal_echo_snapshot_target(&occupied), None);
    let wide = engine_snapshot(8, 2, "界\x1b[1;1H".as_bytes());
    let (wide_glyph, wide_columns) = cursor_glyph(&wide).expect("wide glyph is in rendered grid");
    assert_eq!(wide_glyph, "界");
    assert_eq!(wide_columns, 0..2);
    assert_eq!(terminal_echo_snapshot_target(&wide), None);
    let combining = engine_snapshot(8, 2, " \u{301}\x1b[1;1H".as_bytes());
    let (combining_glyph, combining_columns) =
        cursor_glyph(&combining).expect("combined glyph is in rendered grid");
    assert!(combining_glyph.contains('\u{301}'));
    assert_eq!(combining_columns, 0..1);
    assert_eq!(terminal_echo_snapshot_target(&combining), None);

    let invalid_byte_range = mutate_cursor_span(&blank, |span| span.byte_range.end += 1);
    assert_eq!(terminal_echo_snapshot_target(&invalid_byte_range), None);
    let invalid_column_width = mutate_cursor_span(&blank, |span| span.column_range.end += 1);
    assert_eq!(terminal_echo_snapshot_target(&invalid_column_width), None);
    let missing_target_span = mutate_cursor_span(&blank, |span| span.column_range.start += 1);
    assert_eq!(terminal_echo_snapshot_target(&missing_target_span), None);
}

/// 保留探针的视口边界：末列无换行余量、滚动历史和隐藏光标都不能获准。
///
/// @author kongweiguang
#[test]
fn snapshot_target_keeps_boundary_scrollback_and_hidden_cursor_guards() {
    let blank = engine_snapshot(8, 2, &[]);

    let mut boundary = blank.clone();
    boundary.cursor.column = boundary.size.columns() - 1;
    assert_eq!(terminal_echo_snapshot_target(&boundary), None);

    let mut scrolled = blank.clone();
    scrolled.scrollback_offset = 1;
    assert_eq!(terminal_echo_snapshot_target(&scrolled), None);

    let mut hidden_cursor = blank;
    hidden_cursor.cursor.visible = false;
    assert_eq!(terminal_echo_snapshot_target(&hidden_cursor), None);
}
