//! 默认本地 shell 选择与 target 环境一致性测试。
//!
//! @author kongweiguang

use std::ffi::OsStr;

use kerminal_application::preferences::DefaultLocalShellProvider;

use super::*;

/// 确认 Unix 默认目标采用 portable-pty 解析出的绝对账户 shell，并导出同一可执行路径。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn unix_default_target_exports_the_selected_shell() {
    let target = SystemDefaultLocalShellProvider::new()
        .resolve_default()
        .expect("账户默认 shell 应可解析");

    let selected = target.program.to_str().expect("默认 shell 路径应为 UTF-8");
    assert!(usable_unix_shell(selected));
    assert_eq!(
        target.environment.get(OsStr::new("SHELL")),
        Some(&target.program)
    );
}

/// 确认无效的相对路径与目录不会被当作可启动的账户 shell。
///
/// @author kongweiguang
#[cfg(unix)]
#[test]
fn unix_shell_validation_rejects_relative_path_and_directory() {
    let directory = tempfile::tempdir().expect("创建隔离 shell 测试目录");
    let directory_path = directory.path().to_str().expect("临时目录路径应为 UTF-8");

    assert!(!usable_unix_shell("bin/sh"));
    assert!(!usable_unix_shell(directory_path));
}

/// 确认 Windows 默认 PowerShell target 不伪造 Unix SHELL 环境变量。
///
/// @author kongweiguang
#[cfg(windows)]
#[test]
fn windows_default_target_does_not_define_unix_shell() {
    let target = SystemDefaultLocalShellProvider::new()
        .resolve_default()
        .expect("Windows 默认 shell 应可解析");

    assert!(!target.environment.contains_key(OsStr::new("SHELL")));
}
