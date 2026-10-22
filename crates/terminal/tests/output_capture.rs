//! OSC 133 受界文本转录的事件关联、分块边界、失败和容量合同。
//!
//! @author kongweiguang

use kerminal_domain::identity::SessionId;
use kerminal_domain::sessions::TerminalSize;
use kerminal_terminal::engine::{TerminalEngine, TerminalLimits};
use kerminal_terminal::output_capture::{
    TRANSCRIPT_BLOCK_BYTES_LIMIT, TRANSCRIPT_BLOCK_LIMIT, TerminalOutputEvent,
    TerminalTranscriptState, TranscriptCaptureId, TranscriptIncompleteReason,
    TranscriptUnavailableReason,
};

/// 创建小型但真实的 Alacritty engine；测试仍走生产的 OSC scanner 与 PTY parser 入口。
///
/// @author kongweiguang
fn engine() -> TerminalEngine {
    let size = TerminalSize::new(80, 24, 0, 0).expect("valid terminal size");
    TerminalEngine::new(SessionId::new(), size, TerminalLimits::default()).expect("bounded engine")
}

/// 从有序 output event 中读取 C/D 所关联的 opaque capture ID。
///
/// @author kongweiguang
fn capture_id(events: &[TerminalOutputEvent], start: bool) -> TranscriptCaptureId {
    events
        .iter()
        .find_map(|event| match event {
            TerminalOutputEvent::ShellMarker {
                marker: kerminal_terminal::ShellIntegrationMarker::CommandStart,
                capture_id: Some(id),
            } if start => Some(*id),
            TerminalOutputEvent::ShellMarker {
                marker: kerminal_terminal::ShellIntegrationMarker::CommandFinished { .. },
                capture_id: Some(id),
            } if !start => Some(*id),
            _ => None,
        })
        .expect("capture ID attached to real OSC 133 marker")
}

/// OSC C..D 之外的文本不进入转录，VT control sequences 被过滤且事件保持 wire 顺序。
///
/// @author kongweiguang
#[test]
fn captures_only_command_interval_as_clean_text() {
    let mut engine = engine();
    let output = engine.process_output(
        b"prompt\x1b]133;C\x07hello \x1b[31mred\x1b[0m\r\n\x1b]133;D;7\x07next prompt",
    );
    let start_id = capture_id(&output.events, true);
    let finish_id = capture_id(&output.events, false);

    assert_eq!(start_id, finish_id);
    assert_eq!(output.shell_integration.len(), 2);
    let transcript = engine
        .transcript_snapshot(start_id, usize::MAX)
        .expect("capture retained");
    assert_eq!(transcript.text.as_ref(), "hello red\n");
    assert_eq!(
        transcript.state,
        TerminalTranscriptState::Complete { exit_code: Some(7) }
    );
    assert!(!transcript.text.contains("prompt"));
    assert!(!transcript.text.contains("\x1b"));
}

/// Fragmented C/D OSC and fragmented UTF-8 are handled by persistent protocol parsers without guessed boundaries.
///
/// @author kongweiguang
#[test]
fn fragmented_markers_and_utf8_keep_the_same_capture() {
    let mut engine = engine();
    assert!(engine.process_output(b"\x1b]133;C").events.is_empty());

    let begin = engine.process_output(b"\x07caf\xc3");
    let id = capture_id(&begin.events, true);
    assert_eq!(
        engine
            .transcript_snapshot(id, usize::MAX)
            .expect("active capture")
            .text
            .as_ref(),
        "caf"
    );

    assert!(
        engine
            .process_output(b"\xa9\x1b]133;D;0\x1b")
            .events
            .iter()
            .all(|event| !matches!(
                event,
                TerminalOutputEvent::ShellMarker {
                    marker: kerminal_terminal::ShellIntegrationMarker::CommandFinished { .. },
                    ..
                }
            ))
    );
    let finish = engine.process_output(b"\\after");
    assert_eq!(capture_id(&finish.events, false), id);
    assert_eq!(
        engine
            .transcript_snapshot(id, usize::MAX)
            .expect("completed capture")
            .text
            .as_ref(),
        "café"
    );
}

/// 一个 PTY read 内多条命令按 marker 顺序获得不同 ID，不把前一条内容关联给后一条。
///
/// @author kongweiguang
#[test]
fn multiple_commands_in_one_read_keep_ordered_ids() {
    let mut engine = engine();
    let output = engine
        .process_output(b"\x1b]133;C\x07one\x1b]133;D;0\x07\x1b]133;C\x07two\x1b]133;D;2\x07");
    let starts: Vec<_> = output
        .events
        .iter()
        .filter_map(|event| match event {
            TerminalOutputEvent::ShellMarker {
                marker: kerminal_terminal::ShellIntegrationMarker::CommandStart,
                capture_id: Some(id),
            } => Some(*id),
            _ => None,
        })
        .collect();
    let finishes: Vec<_> = output
        .events
        .iter()
        .filter_map(|event| match event {
            TerminalOutputEvent::ShellMarker {
                marker: kerminal_terminal::ShellIntegrationMarker::CommandFinished { .. },
                capture_id: Some(id),
            } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(starts, finishes);
    assert_eq!(starts.len(), 2);
    assert_ne!(starts[0], starts[1]);
    assert_eq!(
        engine
            .transcript_snapshot(starts[0], 32)
            .unwrap()
            .text
            .as_ref(),
        "one"
    );
    assert_eq!(
        engine
            .transcript_snapshot(starts[1], 32)
            .unwrap()
            .text
            .as_ref(),
        "two"
    );
}

/// 生命周期中断与无 D 的新 C 都保留部分文本并显式标记 incomplete。
///
/// @author kongweiguang
#[test]
fn missing_finished_marker_is_never_reported_complete() {
    let mut engine = engine();
    let started = engine.process_output(b"\x1b]133;C\x07partial");
    let id = capture_id(&started.events, true);
    let invalidation =
        engine.invalidate_active_transcript(TranscriptIncompleteReason::Disconnected);
    assert!(matches!(
        invalidation.as_slice(),
        [TerminalOutputEvent::TranscriptChanged { capture_id, .. }] if *capture_id == id
    ));
    let snapshot = engine
        .transcript_snapshot(id, 64)
        .expect("partial capture retained");
    assert_eq!(snapshot.text.as_ref(), "partial");
    assert_eq!(
        snapshot.state,
        TerminalTranscriptState::Incomplete(TranscriptIncompleteReason::Disconnected)
    );
}

/// 重连使活动块不完整但保留旧完成内容，以免旧 CommandBlock ID 变成空成功。
///
/// @author kongweiguang
#[test]
fn reconnect_preserves_old_captures_and_marks_active_incomplete() {
    let mut engine = engine();
    let first = engine.process_output(b"\x1b]133;C\x07done\x1b]133;D;0\x07");
    let completed = capture_id(&first.events, true);
    let second = engine.process_output(b"\x1b]133;C\x07partial");
    let active = capture_id(&second.events, true);
    engine
        .reset_for_reconnect()
        .expect("same-size reconnect reset");

    assert_eq!(
        engine.transcript_snapshot(completed, 32).unwrap().state,
        TerminalTranscriptState::Complete { exit_code: Some(0) }
    );
    assert_eq!(
        engine.transcript_snapshot(active, 32).unwrap().state,
        TerminalTranscriptState::Incomplete(TranscriptIncompleteReason::Reconnected)
    );
}

/// 单块、会话总预算和显式读取上限都按 UTF-8 安全边界截断。
///
/// @author kongweiguang
#[test]
fn transcript_budgets_truncate_and_evict_explicitly() {
    let mut engine = engine();
    let mut ids = Vec::new();
    let payload = vec![b'x'; TRANSCRIPT_BLOCK_BYTES_LIMIT + 10];
    for _ in 0..9 {
        let started = engine.process_output(b"\x1b]133;C\x07");
        let id = capture_id(&started.events, true);
        ids.push(id);
        engine.process_output(&payload);
        engine.process_output(b"\x1b]133;D;0\x07");
    }

    assert_eq!(
        engine.transcript_snapshot(ids[0], usize::MAX),
        Err(TranscriptUnavailableReason::Evicted)
    );
    let retained = engine
        .transcript_snapshot(ids[8], 19)
        .expect("newest block retained");
    assert_eq!(retained.text.len(), 19);
    assert_eq!(retained.returned_bytes, 19);
    assert_eq!(retained.captured_bytes, TRANSCRIPT_BLOCK_BYTES_LIMIT);
    assert!(retained.truncated, "the output exceeded the block byte cap");
}

/// 命令块数量超过 128 时移除最旧条目，查询返回 Evicted 而不是空快照。
///
/// @author kongweiguang
#[test]
fn transcript_retention_is_bounded_to_128_blocks() {
    let mut engine = engine();
    let mut first_id = None;
    let mut latest_id = None;
    for _ in 0..=TRANSCRIPT_BLOCK_LIMIT {
        let output = engine.process_output(b"\x1b]133;C\x07x\x1b]133;D;0\x07");
        let id = capture_id(&output.events, true);
        first_id.get_or_insert(id);
        latest_id = Some(id);
    }

    assert_eq!(
        engine.transcript_snapshot(first_id.expect("first ID"), 16),
        Err(TranscriptUnavailableReason::Evicted)
    );
    assert_eq!(
        engine
            .transcript_snapshot(latest_id.expect("latest ID"), 16)
            .expect("latest transcript retained")
            .text
            .as_ref(),
        "x"
    );
}

/// 输出查询的 max_bytes 不切断多字节字形，并报告本次响应是否较完整转录短。
///
/// @author kongweiguang
#[test]
fn transcript_read_budget_preserves_utf8_boundaries() {
    let mut engine = engine();
    let output = engine.process_output("\x1b]133;C\x07猫xy\x1b]133;D;0\x07".as_bytes());
    let id = capture_id(&output.events, true);
    let clipped = engine.transcript_snapshot(id, 2).expect("bounded read");
    assert_eq!(clipped.text.as_ref(), "");
    assert_eq!(clipped.returned_bytes, 0);
    assert!(clipped.truncated);
    let enough = engine.transcript_snapshot(id, 4).expect("bounded read");
    assert_eq!(enough.text.as_ref(), "猫x");
    assert_eq!(enough.returned_bytes, 4);
    assert!(enough.truncated);
}
