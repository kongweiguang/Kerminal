//! 窗口关闭策略与平台状态契约测试。
//!
//! @author kongweiguang

use kerminal_application::platform::window_state::{ClosePolicy, close_policy};

/// 只有系统托盘确实完成安装时，普通窗口关闭才转为隐藏。
#[test]
fn ordinary_close_hides_only_with_an_installed_tray_owner() {
    assert_eq!(close_policy(true), ClosePolicy::HideToTray);
    assert_eq!(close_policy(false), ClosePolicy::RequestQuit);
}
