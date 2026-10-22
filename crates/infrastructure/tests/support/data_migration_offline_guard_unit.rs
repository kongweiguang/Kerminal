//! 旧版单实例只读探测的合成分类测试。
//!
//! @author kongweiguang

use super::{
    ProbeStatus, classify_linux_name_owner, classify_macos_socket_error, enforce_probe_status,
};

/// 新格式、冲突与未知状态分别保留成功、Conflict 和 Infrastructure 语义。
///
/// @author kongweiguang
#[test]
fn probe_status_maps_to_closed_migration_outcomes() {
    assert!(enforce_probe_status(ProbeStatus::Stopped, "running").is_ok());
    assert!(matches!(
        enforce_probe_status(ProbeStatus::Running, "running"),
        Err(kerminal_application::AppError::Conflict { .. })
    ));
    assert!(matches!(
        enforce_probe_status(ProbeStatus::Unknown, "running"),
        Err(kerminal_application::AppError::Infrastructure { .. })
    ));
}

/// 单实例常量锁定 v0.3.45 的 application identifier 与未启用 semver 的旧插件协议。
///
/// @author kongweiguang
#[test]
fn legacy_instance_names_match_the_locked_tauri_plugin_contract() {
    #[cfg(windows)]
    assert_eq!(
        super::WINDOWS_MUTEX_NAME,
        "io.github.kongweiguang.kerminal-sim"
    );
    #[cfg(target_os = "linux")]
    assert_eq!(
        super::LINUX_DBUS_NAME,
        "io.github.kongweiguang.kerminal.SingleInstance"
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        super::MACOS_SOCKET_PATH,
        "/tmp/io_github_kongweiguang_kerminal_si.sock"
    );
}

/// 只有 macOS socket 路径明确不存在才允许探测通过，其它文件系统错误均为未知。
///
/// @author kongweiguang
#[test]
fn synthetic_macos_path_errors_never_turn_permission_failures_into_absence() {
    use std::io::ErrorKind;

    assert_eq!(
        classify_macos_socket_error(ErrorKind::NotFound),
        ProbeStatus::Stopped
    );
    assert_eq!(
        classify_macos_socket_error(ErrorKind::PermissionDenied),
        ProbeStatus::Unknown
    );
    assert_eq!(
        classify_macos_socket_error(ErrorKind::Other),
        ProbeStatus::Unknown
    );
}

/// Linux 旧版 D-Bus 名称有 owner 时拒绝迁移，没有 owner 才表示本 session 未发现旧版。
///
/// @author kongweiguang
#[test]
fn linux_name_owner_classification_is_explicit() {
    assert_eq!(classify_linux_name_owner(true), ProbeStatus::Running);
    assert_eq!(classify_linux_name_owner(false), ProbeStatus::Stopped);
}

/// Windows 只有明确的“对象不存在”可放行，访问拒绝与异常错误都不能当成已退出。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_mutex_errors_fail_closed() {
    use windows_sys::Win32::Foundation::{ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};

    assert_eq!(
        super::classify_windows_open_error(ERROR_FILE_NOT_FOUND),
        ProbeStatus::Stopped
    );
    assert_eq!(
        super::classify_windows_open_error(ERROR_ACCESS_DENIED),
        ProbeStatus::Unknown
    );
    assert_eq!(
        super::classify_windows_open_error(u32::MAX),
        ProbeStatus::Unknown
    );
}

/// Windows 测试只占用随机合成 mutex，验证只读 OpenMutex 探测不接触旧版真实名称。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_probe_detects_only_a_synthetic_named_mutex() {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::CreateMutexW};

    let name = format!(
        "Kerminal-Migration-Probe-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    );
    let wide_name = std::ffi::OsStr::new(&name)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    assert_eq!(
        super::probe_windows_mutex(&name)
            .expect("missing synthetic mutex is a stopped observation"),
        ProbeStatus::Stopped
    );
    // SAFETY: 使用随机合成名称创建测试私有 mutex，不会与应用或旧版单实例名称冲突。
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide_name.as_ptr()) };
    assert!(!handle.is_null(), "synthetic test mutex is created");
    assert_eq!(
        super::probe_windows_mutex(&name)
            .expect("existing synthetic mutex is a running observation"),
        ProbeStatus::Running
    );
    // SAFETY: 测试刚创建该句柄并在断言后立即关闭。
    assert_ne!(unsafe { CloseHandle(handle) }, 0);
}
