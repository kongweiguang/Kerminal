//! storage owner PID 探测的私有平台语义测试。
//!
//! @author kongweiguang

use super::{TargetPidStatus, probe_target_pid};

/// 当前进程可被内核探测到，确认成功路径不会误判为缺失。
#[cfg(any(windows, unix))]
#[test]
fn target_pid_probe_recognizes_current_process() {
    assert_eq!(
        probe_target_pid(std::process::id()),
        TargetPidStatus::Running
    );
}

#[cfg(unix)]
/// 只有 ESRCH 证明不存在；权限拒绝、信号中断和无 errno 都保持未知。
#[test]
fn unix_probe_error_classification_is_conservative() {
    use super::classify_unix_kill_error;

    assert_eq!(
        classify_unix_kill_error(Some(libc::ESRCH)),
        TargetPidStatus::Missing
    );
    assert_eq!(
        classify_unix_kill_error(Some(libc::EPERM)),
        TargetPidStatus::Unknown
    );
    assert_eq!(
        classify_unix_kill_error(Some(libc::EINTR)),
        TargetPidStatus::Unknown
    );
    assert_eq!(classify_unix_kill_error(None), TargetPidStatus::Unknown);
}

#[cfg(unix)]
/// 无法安全转换为 pid_t 的 metadata 不能成为删除旧 owner 的证据。
#[test]
fn unix_probe_keeps_out_of_range_pid_unknown() {
    assert_eq!(probe_target_pid(u32::MAX), TargetPidStatus::Unknown);
}

#[cfg(unix)]
/// 内核明确报告不存在时才允许三态探测给出 Missing。
#[test]
fn unix_probe_recognizes_an_unused_pid() {
    assert_eq!(probe_target_pid(i32::MAX as u32), TargetPidStatus::Missing);
}

#[cfg(windows)]
/// OpenProcess 的明确无效 PID 错误可回收，访问拒绝和其它错误不能回收。
#[test]
fn windows_probe_error_classification_is_conservative() {
    use super::classify_windows_open_process_error;
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER};

    assert_eq!(
        classify_windows_open_process_error(ERROR_INVALID_PARAMETER),
        TargetPidStatus::Missing
    );
    assert_eq!(
        classify_windows_open_process_error(ERROR_ACCESS_DENIED),
        TargetPidStatus::Unknown
    );
    assert_eq!(
        classify_windows_open_process_error(u32::MAX),
        TargetPidStatus::Unknown
    );
}

#[cfg(windows)]
/// OpenProcess 对不存在的高位 PID 给出无效参数，证明 Windows 缺失分支可运行。
#[test]
fn windows_probe_recognizes_an_unused_pid() {
    assert_eq!(probe_target_pid(u32::MAX), TargetPidStatus::Missing);
}
