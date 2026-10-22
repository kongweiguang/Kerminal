//! Terminal Canvas 右键入口策略回归测试。
//!
//! @author kongweiguang

use kerminal_application::preferences::TerminalRightClickBehavior;
use kerminal_terminal::snapshot::MouseTrackingMode;

use gpui_kit::MouseButton;

use crate::terminal_canvas::{
    context_menu_should_attach, mouse_down_should_focus_target, right_click_action,
};
use crate::terminal_interaction::TerminalRightClickAction;

/// 校验三种用户设置保持不同动作，不把 Menu/Paste 错误收敛为同一分支。
///
/// @author kongweiguang
#[test]
fn right_click_behavior_maps_to_the_configured_action() {
    assert_eq!(
        right_click_action(TerminalRightClickBehavior::None),
        TerminalRightClickAction::FocusOnly
    );
    assert_eq!(
        right_click_action(TerminalRightClickBehavior::Paste),
        TerminalRightClickAction::Paste
    );
    assert_eq!(
        right_click_action(TerminalRightClickBehavior::Menu),
        TerminalRightClickAction::Menu
    );
}

/// 先激活 terminal pane 再投递 TUI 鼠标 press；普通菜单右键保持原焦点和视口位置。
///
/// @author kongweiguang
#[test]
fn mouse_down_focuses_only_interactions_that_need_an_active_terminal() {
    let menu = TerminalRightClickAction::Menu;
    let focus = TerminalRightClickAction::FocusOnly;
    let paste = TerminalRightClickAction::Paste;

    assert!(mouse_down_should_focus_target(
        MouseButton::Left,
        false,
        menu
    ));
    assert!(!mouse_down_should_focus_target(
        MouseButton::Right,
        false,
        menu
    ));
    assert!(mouse_down_should_focus_target(
        MouseButton::Right,
        false,
        focus
    ));
    assert!(mouse_down_should_focus_target(
        MouseButton::Right,
        false,
        paste
    ));
    assert!(!mouse_down_should_focus_target(
        MouseButton::Middle,
        false,
        menu
    ));

    assert!(mouse_down_should_focus_target(
        MouseButton::Left,
        true,
        menu
    ));
    assert!(mouse_down_should_focus_target(
        MouseButton::Right,
        true,
        menu
    ));
    assert!(mouse_down_should_focus_target(
        MouseButton::Middle,
        true,
        menu
    ));
}

/// 只在有可见 view binding 的非 TUI 终端装载 Kit context menu，避免抢占 DEC mouse tracking。
///
/// @author kongweiguang
#[test]
fn context_menu_requires_menu_policy_snapshot_and_view_binding() {
    assert!(context_menu_should_attach(
        TerminalRightClickBehavior::Menu,
        Some(MouseTrackingMode::None),
        true
    ));
    assert!(!context_menu_should_attach(
        TerminalRightClickBehavior::Menu,
        Some(MouseTrackingMode::Click),
        true
    ));
    assert!(!context_menu_should_attach(
        TerminalRightClickBehavior::Menu,
        None,
        true
    ));
    assert!(!context_menu_should_attach(
        TerminalRightClickBehavior::Paste,
        Some(MouseTrackingMode::None),
        true
    ));
    assert!(!context_menu_should_attach(
        TerminalRightClickBehavior::Menu,
        Some(MouseTrackingMode::None),
        false
    ));
}
