//! 连接错误恢复提示的展示契约测试。
//!
//! @author kongweiguang

use kerminal_application::AppError;

use crate::features::connections::panel::localized_error;

/// 私钥无法读取、无法解码及服务端拒绝认证需要不同的恢复提示，且不能显示诊断中的秘密。
/// @author kongweiguang
#[test]
fn ssh_key_errors_explain_recovery_without_exposing_source() {
    let cause = || std::io::Error::other("fixture-secret-path");
    let unreadable = localized_error(&AppError::infrastructure_with_source(
        "SSH private-key file could not be read",
        cause(),
    ));
    let invalid = localized_error(&AppError::infrastructure_with_source(
        "SSH private key could not be decoded",
        cause(),
    ));
    let refused = localized_error(&AppError::authentication_failed());

    assert!(unreadable.contains("readable") || unreadable.contains("重新选择"));
    assert!(invalid.contains("passphrase") || invalid.contains("口令"));
    assert!(refused.contains("username and credentials") || refused.contains("用户名和凭据"));
    assert!(!unreadable.contains("fixture-secret-path"));
    assert!(!invalid.contains("fixture-secret-path"));
}

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
