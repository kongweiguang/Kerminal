//! 日志格式器的敏感字段回归测试。
//!
//! @author kongweiguang

use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use tracing_subscriber::{EnvFilter, fmt::MakeWriter};

use super::RedactedFields;

/// 固定宿主阶段及回执两端经真实格式器仍可检索；动态错误正文保持脱敏，不扩张敏感边界。
///
/// @author kongweiguang
#[test]
fn host_shutdown_events_survive_without_dynamic_error_text() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .fmt_fields(RedactedFields)
        .with_writer(BufferFactory(Arc::clone(&output)))
        .finish();
    let events = [
        "HOST_QUIT_SUBMITTED",
        "HOST_QUIT_BEGIN",
        "HOST_QUIT_COMPLETE",
        "HOST_QUIT_RECEIPT_SENT",
        "DESKTOP_HOST_RESPONSE_RECEIVED",
        "HOST_QUIT_FAILED",
        "HOST_INSTALL_SUBMITTED",
        "HOST_INSTALL_BEGIN",
        "HOST_INSTALL_COMPLETE",
        "HOST_INSTALL_QUIESCE_FAILED",
        "HOST_INSTALL_FAILED",
        "HOST_INSTALL_OUTCOME_UNKNOWN",
        "HOST_FINALIZE_SUBMITTED",
        "HOST_FINALIZE_BEGIN",
        "HOST_FINALIZE_COMPLETE",
        "HOST_FINALIZE_FAILED",
        "HOST_COMMAND_SEND_FAILED",
        "HOST_COMMAND_RECEIPT_LOST",
        "HOST_COMPOSITION_STOP_BEGIN",
        "HOST_COMPOSITION_STOP_COMPLETE",
        "HOST_COMPOSITION_STOP_FAILED",
        "HOST_LISTENER_STOP_BEGIN",
        "HOST_LISTENER_STOP_COMPLETE",
        "HOST_LISTENER_STOP_FAILED",
    ];
    tracing::subscriber::with_default(subscriber, || {
        for event in events {
            tracing::warn!(
                event,
                code = "timeout",
                message = "synthetic private cleanup detail"
            );
        }
    });
    let output = String::from_utf8(output.lock().expect("log buffer").clone()).expect("UTF-8 logs");
    for event in events {
        assert!(
            output.contains(&format!("event={event}")),
            "fixed event must remain visible: {event}"
        );
    }
    assert!(output.contains("code=timeout"));
    assert!(output.contains("message=<redacted>"));
    assert!(!output.contains("synthetic private cleanup detail"));
}

/// 退出采样仅公开编译期 basename、行号和有界数值，完整路径、正文及控制字符仍需被脱敏。
///
/// @author kongweiguang
#[test]
fn quit_task_sample_preserves_source_basename_without_paths_or_terminal_text() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .fmt_fields(RedactedFields)
        .with_writer(BufferFactory(Arc::clone(&output)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE",
            task_source = "actions.rs",
            source_line = 86_u64,
            sequence = 2_u64,
            phase = 1_u64,
            duration_ms = 1828_u128,
            same_poll = 1_u64,
            message = "synthetic private terminal body",
        );
        tracing::info!(
            event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE",
            task_source = "C:\\private-build\\src\\secret.rs",
        );
        tracing::info!(
            event = "DESKTOP_QUIT_MAIN_TASK_SAMPLE",
            task_source = "private\nsource.rs",
        );
    });
    let output = String::from_utf8(output.lock().expect("log buffer").clone()).expect("UTF-8 logs");
    for field in [
        "event=DESKTOP_QUIT_MAIN_TASK_SAMPLE",
        "task_source=actions.rs",
        "source_line=86",
        "sequence=2",
        "phase=1",
        "duration_ms=1828",
        "same_poll=1",
    ] {
        assert!(output.contains(field), "fixed diagnostic missing: {field}");
    }
    assert!(output.contains("task_source=<redacted>"));
    assert!(!output.contains("private-build"));
    assert!(!output.contains("private\nsource.rs"));
    assert!(!output.contains("synthetic private terminal body"));
}

/// IME 排查必须能看见固定阶段和计数，同时新开关不放行正文、键值、目标身份或任意数值。
///
/// @author kongweiguang
#[test]
fn ime_diagnostics_preserve_stages_without_text_or_resource_identity() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .fmt_fields(RedactedFields)
        .with_writer(BufferFactory(Arc::clone(&output)))
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            event = "TERMINAL_IME_DIAGNOSTIC",
            phase = 31_u64,
            count = 1_u64,
            ime_focus_owned = 1_u64,
            ime_accepted = 1_u64,
            ime_reason = 0_u64,
            ime_session_generation = 7_u64,
            kitty_keyboard_flags = 0_u64,
            bytes = 3_u64,
            text = "synthetic-private-ime-text",
            key = "synthetic-private-key",
            session_id = "synthetic-private-ime-session",
            target = "synthetic-private-target",
            secret_number = 987654_u64,
        );
    });
    let output = String::from_utf8(output.lock().expect("log buffer").clone())
        .expect("formatter should write UTF-8");
    for field in [
        "event=TERMINAL_IME_DIAGNOSTIC",
        "phase=31",
        "count=1",
        "ime_focus_owned=1",
        "ime_accepted=1",
        "ime_reason=0",
        "ime_session_generation=7",
        "kitty_keyboard_flags=0",
        "bytes=3",
    ] {
        assert!(output.contains(field), "safe IME field missing: {field}");
    }
    for field in ["text", "key", "session_id", "target", "secret_number"] {
        assert!(output.contains(&format!("{field}=<redacted>")));
    }
    assert!(!output.contains("synthetic-private"));
    assert!(!output.contains("987654"));
}

#[derive(Clone)]
struct BufferFactory(Arc<Mutex<Vec<u8>>>);

struct BufferWriter(Arc<Mutex<Vec<u8>>>);

impl Write for BufferWriter {
    /// 将测试日志写入内存缓冲，避免碰触真实用户日志目录。
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut buffer = self
            .0
            .lock()
            .map_err(|_| io::Error::other("test log buffer lock poisoned"))?;
        buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// 内存 writer 没有外部缓冲，保留接口以构造标准 fmt layer。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> MakeWriter<'writer> for BufferFactory {
    type Writer = BufferWriter;

    /// 为每条记录复用同一个内存缓冲，使测试无需安装全局 subscriber。
    fn make_writer(&'writer self) -> Self::Writer {
        BufferWriter(Arc::clone(&self.0))
    }
}

/// 验证有限事件码与诊断数值可检索，同时动态消息、凭据、路径和终端正文均脱敏。
/// 输入诊断仅公开资格状态，不因排查历史记录而扩大正文与标识符的日志边界。
///
/// @author kongweiguang
#[test]
fn safe_diagnostics_survive_and_sensitive_values_are_redacted() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("trace"))
        .fmt_fields(RedactedFields)
        .with_writer(BufferFactory(Arc::clone(&output)))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        tracing::warn!(
            event = "WINDOW_STATE_LOAD_FAILED",
            code = "infrastructure",
            raw_os_error = 5_i64,
            elapsed_ms = 42_u64,
            message = "synthetic startup error with private value",
            error = "synthetic raw error",
            path = "C:/Users/example/private.toml",
            credential = "synthetic-secret-value",
            terminal_payload = "synthetic-terminal-content",
        );
        tracing::warn!(
            event = "UNLISTED_EVENT_WITH_PRIVATE_TEXT",
            code = "unlisted-error-code",
            count = 3_u64,
        );
        tracing::debug!(
            event = "TERMINAL_TYPED_HISTORY_DIAGNOSTIC",
            ticket_present = 1_i64,
            context_present = 1_i64,
            context_available = 0_i64,
            unavailable_reason = 8_i64,
            candidate_present = 0_i64,
            kitty_keyboard_flags = 0_u64,
            command = "synthetic-private-command",
            session_id = "synthetic-private-session",
        );
    });

    let output = String::from_utf8(
        output
            .lock()
            .expect("test log buffer lock should remain available")
            .clone(),
    )
    .expect("formatter should write UTF-8");
    assert!(output.contains("event=WINDOW_STATE_LOAD_FAILED"));
    assert!(output.contains("code=infrastructure"));
    assert!(output.contains("raw_os_error=5"));
    assert!(output.contains("elapsed_ms=42"));
    assert!(output.contains("credential=<redacted>"));
    assert!(output.contains("terminal_payload=<redacted>"));
    assert!(output.contains("message=<redacted>"));
    assert!(output.contains("error=<redacted>"));
    assert!(output.contains("path=<redacted>"));
    assert!(output.contains("event=<redacted>"));
    assert!(output.contains("code=<redacted>"));
    assert!(output.contains("count=3"));
    assert!(output.contains("event=TERMINAL_TYPED_HISTORY_DIAGNOSTIC"));
    assert!(output.contains("ticket_present=1"));
    assert!(output.contains("context_present=1"));
    assert!(output.contains("context_available=0"));
    assert!(output.contains("unavailable_reason=8"));
    assert!(output.contains("candidate_present=0"));
    assert!(output.contains("kitty_keyboard_flags=0"));
    assert!(output.contains("command=<redacted>"));
    assert!(output.contains("session_id=<redacted>"));
    assert!(!output.contains("synthetic-private-command"));
    assert!(!output.contains("synthetic-private-session"));
    assert!(!output.contains("synthetic-secret-value"));
    assert!(!output.contains("synthetic-terminal-content"));
    assert!(!output.contains("synthetic startup error"));
    assert!(!output.contains("synthetic raw error"));
    assert!(!output.contains("C:/Users/example/private.toml"));
    assert!(!output.contains("UNLISTED_EVENT_WITH_PRIVATE_TEXT"));
    assert!(!output.contains("unlisted-error-code"));
}

/// 验证 opt-in resize/CPR 仅暴露固定事件、几何与 reader 序号/长度，便于关联时序而不放行正文。
/// 未知数值、session 标识、命令与 VT 文本继续脱敏，不能因诊断需要扩大文本日志边界。
///
/// @author kongweiguang
#[test]
fn resize_diagnostics_preserve_only_allowlisted_geometry() {
    let output = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new("trace"))
        .fmt_fields(RedactedFields)
        .with_writer(BufferFactory(Arc::clone(&output)))
        .finish();

    tracing::subscriber::with_default(subscriber, || {
        tracing::info!(
            event = "TERMINAL_RESIZE_DIAGNOSTIC",
            phase = 9_u64,
            session_sequence = 3_u64,
            transport_generation = 2_u64,
            resize_generation = 7_u64,
            reader_sequence = 21_u64,
            chunk_bytes = 4096_u64,
            native_process_id = 52596_u64,
            native_resize_call_sequence = 4_u64,
            old_columns = 112_u64,
            old_rows = 33_u64,
            old_pixel_width = 1005_u64,
            old_pixel_height = 594_u64,
            new_columns = 75_u64,
            new_rows = 33_u64,
            new_pixel_width = 675_u64,
            new_pixel_height = 594_u64,
            target_columns = 75_u64,
            target_rows = 33_u64,
            target_pixel_width = 675_u64,
            target_pixel_height = 594_u64,
            cursor_row = 31_u64,
            cursor_column = 14_u64,
            engine_revision = 88_u64,
            elapsed_present = 1_u64,
            elapsed_ms = 6_u64,
            secret_number = 987654_u64,
            session_id = "synthetic-private-session",
            command = "synthetic-private-command",
            raw_vt = "synthetic-private-vt",
            error = "synthetic-private-error",
        );
        tracing::info!(
            event = "TERMINAL_CPR_DIAGNOSTIC",
            session_sequence = 3_u64,
            transport_generation = 2_u64,
            resize_generation = 7_u64,
            cpr_row = 31_u64,
            cpr_column = 14_u64,
            session_id = "synthetic-private-session",
            payload = "synthetic-private-cpr-payload",
        );
    });

    let output = String::from_utf8(
        output
            .lock()
            .expect("test log buffer should remain available")
            .clone(),
    )
    .expect("formatter should write UTF-8");
    for field in [
        "phase=9",
        "session_sequence=3",
        "transport_generation=2",
        "resize_generation=7",
        "reader_sequence=21",
        "chunk_bytes=4096",
        "native_process_id=52596",
        "native_resize_call_sequence=4",
        "old_columns=112",
        "old_rows=33",
        "old_pixel_width=1005",
        "old_pixel_height=594",
        "new_columns=75",
        "new_rows=33",
        "new_pixel_width=675",
        "new_pixel_height=594",
        "target_columns=75",
        "target_rows=33",
        "target_pixel_width=675",
        "target_pixel_height=594",
        "cursor_row=31",
        "cursor_column=14",
        "engine_revision=88",
        "elapsed_present=1",
        "elapsed_ms=6",
        "cpr_row=31",
        "cpr_column=14",
    ] {
        assert!(
            output.contains(field),
            "allowlisted numeric field missing: {field}"
        );
    }
    assert!(output.contains("event=TERMINAL_RESIZE_DIAGNOSTIC"));
    assert!(output.contains("event=TERMINAL_CPR_DIAGNOSTIC"));
    for field in ["session_id", "command", "raw_vt", "error", "payload"] {
        assert!(output.contains(&format!("{field}=<redacted>")));
    }
    assert!(output.contains("secret_number=<redacted>"));
    assert!(!output.contains("secret_number=987654"));
    for private_value in [
        "synthetic-private-session",
        "synthetic-private-command",
        "synthetic-private-vt",
        "synthetic-private-error",
        "synthetic-private-cpr-payload",
    ] {
        assert!(!output.contains(private_value));
    }
}
