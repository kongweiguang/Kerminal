//! 主机密钥阻断和 RDP 依赖提示的只读契约测试。
//!
//! @author kongweiguang

use kerminal_application::{AppError, connections::HostKeyStatus};
use kerminal_domain::preferences::ConfigurationMode;

use crate::features::connections::panel::{host_key_trust_allowed, localized_error};

/// 变化过的密钥无论配置是否可写都不能进入覆盖信任记录的 UI 流程。
/// @author kongweiguang
#[test]
fn changed_host_key_is_never_trustable_from_connection_prompt() {
    let changed = HostKeyStatus::Changed {
        previous_fingerprint_sha256: "SHA256:old".to_owned(),
    };

    assert!(!host_key_trust_allowed(
        &changed,
        ConfigurationMode::ReadWrite
    ));
    assert!(!host_key_trust_allowed(
        &changed,
        ConfigurationMode::ReadOnlyRecovery
    ));
}

/// 首次信任只在配置允许持久化时开放，恢复只读模式必须提供取消路径。
/// @author kongweiguang
#[test]
fn unknown_host_key_requires_writable_configuration() {
    assert!(host_key_trust_allowed(
        &HostKeyStatus::Unknown,
        ConfigurationMode::ReadWrite
    ));
    assert!(!host_key_trust_allowed(
        &HostKeyStatus::Unknown,
        ConfigurationMode::ReadOnlyRecovery
    ));
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
