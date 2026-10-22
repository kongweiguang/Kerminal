//! 终端日志视图对活动、保存和失败回执的有限状态投影测试。
//!
//! @author kongweiguang

use super::{LogStatusSummary, log_status_summary};
use kerminal_application::sessions::logging::{SessionLogFailureReason, SessionLogState};

/// 活动录制持续展示字节计数，并且不会被已有文件路径误投影为完成。
#[test]
fn active_recording_keeps_the_live_byte_count() {
    assert_eq!(
        log_status_summary(&state(true, Some("private/path.log"), 42, false, None)),
        LogStatusSummary::Recording { bytes_written: 42 }
    );
}

/// 自动关闭或手动停止后依据文件回执显示已保存摘要，不暴露完整路径。
#[test]
fn completed_recording_projects_saved_summary() {
    assert_eq!(
        log_status_summary(&state(false, Some("private/path.log"), 128, false, None)),
        LogStatusSummary::Saved { bytes_written: 128 }
    );
}

/// 以 path 回执而非字节数识别成功完成，空录制仍是已保存结果。
#[test]
fn completed_empty_recording_projects_saved_summary() {
    assert_eq!(
        log_status_summary(&state(false, Some("private/path.log"), 0, false, None)),
        LogStatusSummary::Saved { bytes_written: 0 }
    );
}

/// 有缺失输出时优先显示截断结果，同时由视图继续展示稳定失败原因。
#[test]
fn truncated_recording_keeps_partial_byte_count() {
    assert_eq!(
        log_status_summary(&state(
            false,
            Some("private/path.log"),
            64,
            true,
            Some(SessionLogFailureReason::SequenceGap),
        )),
        LogStatusSummary::Truncated { bytes_written: 64 }
    );
}

/// 当前录制触发脱敏或队列安全限制后仍标明正在记录，避免误导用户停止按钮状态。
#[test]
fn active_truncated_recording_remains_distinct_from_stopped_truncation() {
    assert_eq!(
        log_status_summary(&state(
            true,
            Some("private/path.log"),
            64,
            true,
            Some(SessionLogFailureReason::QueueFull),
        )),
        LogStatusSummary::RecordingTruncated { bytes_written: 64 }
    );
}

/// 失败但未标记截断的异常状态仍明确呈现失败，不伪装为已保存。
#[test]
fn non_truncated_failure_does_not_look_saved() {
    assert_eq!(
        log_status_summary(&state(
            false,
            Some("private/path.log"),
            12,
            false,
            Some(SessionLogFailureReason::WriterIo),
        )),
        LogStatusSummary::Failed { bytes_written: 12 }
    );
}

/// 未录制状态保留 legacy inactive 快照含义，避免将空数据称为保存成功。
#[test]
fn inactive_without_a_log_file_stays_not_recording() {
    assert_eq!(
        log_status_summary(&SessionLogState::inactive()),
        LogStatusSummary::NotRecording
    );
}

/// 构造受控状态快照，测试只验证状态边界而不依赖 locale、GPUI 或真实磁盘路径。
fn state(
    active: bool,
    path: Option<&str>,
    bytes_written: u64,
    truncated: bool,
    failure_reason: Option<SessionLogFailureReason>,
) -> SessionLogState {
    SessionLogState {
        active,
        path: path.map(str::to_owned),
        started_at: Some("2026-01-01T00:00:00Z".to_owned()),
        bytes_written,
        truncated,
        first_missing_sequence: None,
        failure_reason,
    }
}
