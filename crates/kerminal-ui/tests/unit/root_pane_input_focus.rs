//! Regression coverage for terminal history focus transfer after a shell exits.
//!
//! @author kongweiguang

use super::*;

/// 自然退出只应在旧 pane 的真实焦点仍有效且未被菜单、搜索或窗口状态接管时转给历史路由。
///
/// @author kongweiguang
#[test]
fn history_focus_transfer_rejects_stale_or_overlaid_exit_events() {
    assert!(should_focus_terminal_history_after_exit(
        true, false, true, true, false, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        false, false, true, true, false, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        true, true, true, true, false, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        true, false, false, true, false, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        true, false, true, false, false, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        true, false, true, true, true, false
    ));
    assert!(!should_focus_terminal_history_after_exit(
        true, false, true, true, false, true
    ));
}

/// 普通的缺失 handler 不能让新 pane 获得历史焦点资格，且不同代次的旧 binding 必须失效。
///
/// @author kongweiguang
#[test]
fn history_click_requires_the_retained_binding_and_no_live_handler() {
    let binding = UiSessionBinding {
        session_id: SessionId::parse("session-focus").expect("测试 session id 有效"),
        owner: SessionOwner::Ui {
            tab_id: TabId::parse("tab-focus").expect("测试 tab id 有效"),
            pane_id: PaneId::parse("pane-focus").expect("测试 pane id 有效"),
        },
        session_generation: 4,
    };
    let stale_generation = UiSessionBinding {
        session_generation: 3,
        ..binding.clone()
    };

    assert!(is_retired_history_binding(Some(&binding), &binding, false));
    assert!(!is_retired_history_binding(
        Some(&stale_generation),
        &binding,
        false
    ));
    assert!(!is_retired_history_binding(Some(&binding), &binding, true));
}
