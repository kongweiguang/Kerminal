//! RDP 依赖提示的只读契约测试。
//!
//! @author kongweiguang

use kerminal_application::AppError;

use crate::features::connections::panel::localized_error;

/// RDP 依赖错误展示固定安装提示，不把底层执行路径或诊断 source 透给用户。
/// @author kongweiguang
#[test]
fn rdp_missing_client_errors_keep_actionable_safe_messages() {
    let windows = localized_error(&AppError::not_found(
        "Windows 远程桌面客户端 mstsc.exe 不可用",
    ));
    let macos = localized_error(&AppError::not_found(
        "macOS LaunchServices open 客户端不可用",
    ));
    let linux = localized_error(&AppError::not_found(
        "未找到 FreeRDP 图形客户端，请安装 xfreerdp 或 wlfreerdp",
    ));

    assert!(windows.contains("mstsc.exe"));
    assert!(macos.contains("Remote Desktop") || macos.contains("远程桌面"));
    assert!(linux.contains("xfreerdp") || linux.contains("wlfreerdp"));
    for message in [windows, macos, linux] {
        assert!(!message.contains("C:\\"));
        assert!(!message.contains("/usr/bin/"));
    }
}
