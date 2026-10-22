//! 桌面进程日志初始化与非阻塞 writer 所有权。
//!
//! @author kongweiguang

use std::{error::Error, fmt, fs, path::Path};

use tracing::field::{Field, Visit};
use tracing_appender::{
    non_blocking::{NonBlockingBuilder, WorkerGuard},
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{
    EnvFilter,
    field::RecordFields,
    fmt::format::{FormatFields, Writer},
};

const LOG_DIRECTORY: &str = "logs";
const LOG_FILE_PREFIX: &str = "kerminal";
const LOG_FILE_SUFFIX: &str = "log";
const RETAINED_LOG_FILES: usize = 8;
const LOG_QUEUE_CAPACITY: usize = 4096;
const SAFE_EVENTS: &[&str] = &[
    "DESKTOP_APPLICATION_STOPPED_AFTER_ERROR",
    "DESKTOP_APPLICATION_RETURNED",
    "DESKTOP_NATIVE_UI_RETURNED",
    "DESKTOP_HOST_FINALIZE_BEGIN",
    "DESKTOP_HOST_FINALIZE_COMPLETE",
    "DESKTOP_RUNTIME_STOP_BEGIN",
    "DESKTOP_RUNTIME_STOP_COMPLETE",
    "DESKTOP_NATIVE_QUIT_BEGIN",
    "DESKTOP_NATIVE_QUIT_REQUEST_QUEUED",
    "DESKTOP_HOST_RESPONSE_RECEIVED",
    "DESKTOP_QUIT_MAIN_TASK_SAMPLE",
    "HOST_QUIT_SUBMITTED",
    "HOST_QUIT_BEGIN",
    "HOST_QUIT_COMPLETE",
    "HOST_QUIT_RECEIPT_SENT",
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
    "MCP_RUNTIME_INITIAL_UNAVAILABLE",
    "MCP_RUNTIME_INITIAL_STOPPED",
    "MCP_RUNTIME_INITIAL_STARTING",
    "MCP_RUNTIME_INITIAL_SYNCHRONIZING",
    "MCP_RUNTIME_INITIAL_RUNNING",
    "MCP_RUNTIME_INITIAL_STOPPING",
    "MCP_RUNTIME_INITIAL_FAILED",
    "WINDOW_STATE_LOAD_FAILED",
    "NATIVE_TRAY_SETUP_FINISHED",
    "NATIVE_TRAY_CONDITIONAL",
    "NATIVE_TRAY_UNSUPPORTED",
    "NATIVE_TRAY_INSTALL_FAILED",
    "SINGLE_INSTANCE_ACTIVATION_FAILED",
    "EXTERNAL_ACTIVATION_REJECTED_BY_ROOT",
    "EXTERNAL_ACTIVATION_ROOT_DISPATCH_FAILED",
    "MACOS_URL_LAUNCH_TARGET_UNAVAILABLE",
    "MACOS_URL_ACTIVATION_QUEUE_REJECTED",
    "MACOS_URL_ACTIVATION_INVALID",
    "TRAY_WINDOW_ACTIVATION_FAILED",
    "TRAY_WINDOW_MINIMIZE_FAILED",
    "TRAY_BRIDGE_THREAD_STARTED",
    "TRAY_BRIDGE_THREAD_EXITED",
    "TRAY_BRIDGE_STOP_REQUESTED",
    "TRAY_BRIDGE_JOIN_BEGIN",
    "TRAY_BRIDGE_JOIN_COMPLETE",
    "TRAY_BRIDGE_JOIN_PANICKED",
    "TRAY_NATIVE_ICON_RELEASE_BEGIN",
    "TRAY_NATIVE_ICON_RELEASE_COMPLETE",
    "SINGLE_INSTANCE_MESSAGE_INVALID",
    "ACTIVATION_QUEUE_STOPPED",
    "COMMAND_SUGGESTION_RETENTION_CLEANUP_UNAVAILABLE",
    "DESKTOP_SETTINGS_RELOAD_FAILED",
    "SFTP_POLICY_REMAINS_LAST_VALID",
    "SFTP_TRANSFER_POLICY_UPDATE_FAILED",
    "TERMINAL_AUTO_RECONNECT_POLICY_UPDATE_FAILED",
    "DESKTOP_NOTIFICATION_SETTINGS_UPDATE_FAILED",
    "EXTERNAL_LAUNCH_SETTINGS_UPDATE_FAILED",
    "INITIAL_AGENT_NOTIFICATION_SNAPSHOT_FAILED",
    "AGENT_NOTIFICATION_SESSION_SCAN_FAILED",
    "AGENT_COMPLETION_VIEW_FAILED",
    "AGENT_COMPLETION_SESSION_REVALIDATION_FAILED",
    "WINDOW_VISIBILITY_UNAVAILABLE",
    "AGENT_COMPLETION_NOTIFICATION_FAILED",
    "SFTP_NOTIFICATION_HOST_LABEL_FAILED",
    "SFTP_TRANSFER_NOTIFICATION_FAILED",
    // Static typed-history stage diagnostics; payload and all identities remain excluded.
    "TERMINAL_TYPED_HISTORY_DIAGNOSTIC",
    // Opt-in terminal resize and CPR geometry diagnostics; all dynamic text remains excluded.
    "TERMINAL_RESIZE_DIAGNOSTIC",
    "TERMINAL_CPR_DIAGNOSTIC",
    // Opt-in IME callback/route/writer stages; no input text, key values or resource identities.
    "TERMINAL_IME_DIAGNOSTIC",
];
const SAFE_ERROR_CODES: &[&str] = &[
    "invalid",
    "not_found",
    "unsupported",
    "capability_not_supported",
    "conflict",
    "cancelled",
    "infrastructure",
    "outcome_unknown",
    "authentication_failed",
    "host_key_unknown",
    "host_key_changed",
    "host_key_trust_mismatch",
    "timeout",
    "remote_exit",
    "configuration_corrupt",
];
const SAFE_NUMERIC_FIELDS: &[&str] = &[
    "raw_os_error",
    "errno",
    "win32_code",
    "elapsed_ms",
    "duration_ms",
    "source_line",
    "same_poll",
    "bytes",
    "bytes_written",
    "count",
    "item_count",
    "session_count",
    "queue_depth",
    "sequence",
    "first_missing_sequence",
    // Typed-history event: booleans are 0/1; reason 0=available, 1..=11 map to the app enum,
    // 255 means no matching context/snapshot; Kitty flags are the protocol bit mask (0..=31).
    "ticket_present",
    "context_present",
    "context_available",
    "unavailable_reason",
    "candidate_present",
    "kitty_keyboard_flags",
    // Opt-in resize geometry and lifecycle stage codes; no arbitrary text or session IDs.
    "phase",
    "session_sequence",
    "transport_generation",
    "resize_generation",
    "native_process_id",
    "native_resize_call_sequence",
    "reader_sequence",
    "chunk_bytes",
    "old_columns",
    "old_rows",
    "old_pixel_width",
    "old_pixel_height",
    "new_columns",
    "new_rows",
    "new_pixel_width",
    "new_pixel_height",
    "target_columns",
    "target_rows",
    "target_pixel_width",
    "target_pixel_height",
    "cursor_row",
    "cursor_column",
    "engine_revision",
    "elapsed_present",
    "cpr_row",
    "cpr_column",
    // IME stages: 1..5 handler, 20..22 routing, 30..32 writer; reason is a fixed local enum.
    "ime_focus_owned",
    "ime_accepted",
    "ime_reason",
    "ime_session_generation",
];

/// 持有后台日志 worker，直到 desktop 完成宿主清理后显式 flush。
#[must_use = "日志 worker guard 必须保留到宿主退出并显式 flush"]
pub struct DesktopLogGuard {
    worker: WorkerGuard,
}

impl DesktopLogGuard {
    /// 在应用 runtime 和会话资源停止后消费 guard，让 appender 排空有界日志队列。
    pub fn flush(self) {
        drop(self.worker);
    }
}

/// 初始化进程唯一的文件日志 subscriber；目录或全局 subscriber 初始化失败时拒绝静默启动。
pub fn initialize(config_root: &Path) -> Result<DesktopLogGuard, Box<dyn Error + Send + Sync>> {
    let log_directory = config_root.join(LOG_DIRECTORY);
    fs::create_dir_all(&log_directory)?;

    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(LOG_FILE_PREFIX)
        .filename_suffix(LOG_FILE_SUFFIX)
        .max_log_files(RETAINED_LOG_FILES)
        .build(log_directory)?;
    let (writer, worker) = NonBlockingBuilder::default()
        .buffered_lines_limit(LOG_QUEUE_CAPACITY)
        .lossy(true)
        .thread_name("kerminal-log-writer")
        .finish(appender);

    let level = if cfg!(debug_assertions) {
        "debug"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(level))
        .fmt_fields(RedactedFields)
        .with_writer(writer)
        .with_ansi(false)
        .with_target(true)
        .with_file(true)
        .with_line_number(true)
        .try_init()?;

    Ok(DesktopLogGuard { worker })
}

/// 将有限事件码、稳定错误码和数值诊断写入日志，其它值统一脱敏。
struct RedactedFields;

impl<'writer> FormatFields<'writer> for RedactedFields {
    /// 统一审查 span/event 字段，防止新增日志点把凭据、路径或终端输出写入文件。
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        let mut visitor = RedactedFieldVisitor {
            writer,
            first: true,
            error: None,
        };
        fields.record(&mut visitor);
        visitor.error.map_or(Ok(()), Err)
    }
}

struct RedactedFieldVisitor<'writer> {
    writer: Writer<'writer>,
    first: bool,
    error: Option<fmt::Error>,
}

impl Visit for RedactedFieldVisitor<'_> {
    /// 对动态字符串、错误和协议内容全部脱敏，避免信任任意调用点的格式化文本。
    fn record_debug(&mut self, field: &Field, _value: &dyn fmt::Debug) {
        self.record_field(field, "<redacted>");
    }

    /// 仅保留固定事件、错误码与受限 Rust 源码 basename；任务来源由编译器 Location 提取，不放行路径。
    fn record_str(&mut self, field: &Field, value: &str) {
        let safe_value = match field.name() {
            "event" if SAFE_EVENTS.contains(&value) => value,
            "code" if SAFE_ERROR_CODES.contains(&value) => value,
            "task_source" if is_rust_source_basename(value) => value,
            _ => "<redacted>",
        };
        self.record_field(field, safe_value);
    }

    /// 仅记录约定的整数诊断字段，避免错误地把数值型秘密或标识符写入日志。
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record_numeric(field, value);
    }

    /// 仅记录约定的无符号计数和字节数，确保所有可见数值字段都有固定语义。
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record_numeric(field, value);
    }

    /// 128 位整数同样受字段名白名单约束，避免由默认 Debug 路径暴露值。
    fn record_i128(&mut self, field: &Field, value: i128) {
        self.record_numeric(field, value);
    }

    /// 128 位无符号整数同样受字段名白名单约束，避免由默认 Debug 路径暴露值。
    fn record_u128(&mut self, field: &Field, value: u128) {
        self.record_numeric(field, value);
    }
}

/// 源码诊断只允许短 ASCII Rust 文件名，拒绝路径、空白与控制符，防止此例外扩大日志内容边界。
///
/// @author kongweiguang
fn is_rust_source_basename(value: &str) -> bool {
    value.len() <= 96
        && !value.starts_with('.')
        && value.ends_with(".rs")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

impl RedactedFieldVisitor<'_> {
    /// 统一处理字段分隔符与格式化错误，避免各安全分支产生不一致的日志结构。
    fn record_field(&mut self, field: &Field, value: &str) {
        if self.error.is_some() {
            return;
        }
        let separator = if self.first { "" } else { " " };
        self.first = false;
        self.error = write!(self.writer, "{separator}{}={value}", field.name()).err();
    }

    /// 数值仅在字段名表达明确的计数或 OS 错误号时保留，其他情况按秘密处理。
    fn record_numeric(&mut self, field: &Field, value: impl fmt::Display) {
        if SAFE_NUMERIC_FIELDS.contains(&field.name()) {
            if self.error.is_some() {
                return;
            }
            let separator = if self.first { "" } else { " " };
            self.first = false;
            self.error = write!(self.writer, "{separator}{}={value}", field.name()).err();
        } else {
            self.record_field(field, "<redacted>");
        }
    }
}

#[cfg(test)]
#[path = "../tests/support/logging_fields_unit.rs"]
mod tests;
