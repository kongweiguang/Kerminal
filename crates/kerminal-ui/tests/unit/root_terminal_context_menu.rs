//! Root 终端右键菜单不会将旧 pane/generation 绑定改投当前焦点的回归测试。
//!
//! @author kongweiguang

use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use crate::features::workspace_panes::WorkspacePaneEvent;
use crate::root::terminal_context_menu::{
    TerminalContextMenuAction, binding_matches_exactly, clear_screen_input_event,
    split_menu_action_requires_terminal_focus, terminal_right_click_requires_focus,
    terminal_split_menu_actions, workspace_pane_event_for_menu_action,
};
use crate::terminal_interaction::TerminalRightClickAction;
use kerminal_domain::workspace::SplitDirection;
use kerminal_terminal::{input::encode_input, snapshot::TerminalModes};

/// 创建稳定 UI owner 样本；随机 opaque IDs 避免测试依赖生产命名约定。
///
/// @author kongweiguang
fn binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 7,
    }
}

/// 同会话重连后 generation 已变化，旧 context menu action 必须被拒绝。
///
/// @author kongweiguang
#[test]
fn context_menu_rejects_a_binding_from_an_earlier_session_generation() {
    let expected = binding();
    let mut reconnected = expected.clone();
    reconnected.session_generation += 1;

    assert!(!binding_matches_exactly(&expected, &reconnected));
    assert!(binding_matches_exactly(&expected, &expected));
}

/// pane identity 改变时即使 session 文本相同也不能把旧菜单操作送到新 pane。
///
/// @author kongweiguang
#[test]
fn context_menu_rejects_a_different_owner_for_the_same_session() {
    let expected = binding();
    let mut moved = expected.clone();
    moved.owner = SessionOwner::Ui {
        tab_id: TabId::new(),
        pane_id: PaneId::new(),
    };

    assert!(!binding_matches_exactly(&expected, &moved));
}

/// 清屏菜单保持旧版 Ctrl+L 输入语义，不删除本地 scrollback 或伪报视觉清屏。
///
/// @author kongweiguang
#[test]
fn clear_screen_menu_encodes_ctrl_l_for_the_captured_terminal() {
    assert_eq!(
        encode_input(&clear_screen_input_event(), TerminalModes::default()),
        Some(vec![0x0c])
    );
}

/// 普通 Menu 右键不请求切换输入焦点；Paste 与 FocusOnly 才按设置显式改变焦点。
///
/// @author kongweiguang
#[test]
fn menu_right_click_does_not_request_focus() {
    assert!(!terminal_right_click_requires_focus(
        TerminalRightClickAction::Menu
    ));
    assert!(terminal_right_click_requires_focus(
        TerminalRightClickAction::Paste
    ));
    assert!(terminal_right_click_requires_focus(
        TerminalRightClickAction::FocusOnly
    ));
}

/// 右键菜单的四向分屏与四向选主机动作都保留原 pane 和插入位置。
///
/// @author kongweiguang
#[test]
fn split_menu_actions_keep_direction_and_host_choice_intent() {
    let pane_id = PaneId::new();
    let actions = terminal_split_menu_actions();
    assert_eq!(actions.len(), 8);

    for (action, _, _) in actions {
        let event = workspace_pane_event_for_menu_action(action, pane_id.clone());
        match (action, event) {
            (
                TerminalContextMenuAction::Split { direction, before },
                Some(WorkspacePaneEvent::Split {
                    pane_id: event_pane,
                    direction: event_direction,
                    before: event_before,
                }),
            )
            | (
                TerminalContextMenuAction::ChooseSplitTarget { direction, before },
                Some(WorkspacePaneEvent::ChooseSplitTarget {
                    pane_id: event_pane,
                    direction: event_direction,
                    before: event_before,
                }),
            ) => {
                assert_eq!(event_pane, pane_id);
                assert_eq!(event_direction, direction);
                assert_eq!(event_before, before);
            }
            _ => panic!("每个分屏菜单项都应映射到匹配的布局事件"),
        }
    }
    assert!(
        workspace_pane_event_for_menu_action(TerminalContextMenuAction::Paste, pane_id).is_none()
    );
}

/// 分屏动作触发异步焦点接管；其它菜单动作仍按各自已有的焦点策略执行。
///
/// @author kongweiguang
#[test]
fn only_split_menu_actions_anchor_terminal_focus_before_handoff() {
    assert!(split_menu_action_requires_terminal_focus(
        TerminalContextMenuAction::Split {
            direction: SplitDirection::Horizontal,
            before: false,
        }
    ));
    assert!(split_menu_action_requires_terminal_focus(
        TerminalContextMenuAction::ChooseSplitTarget {
            direction: SplitDirection::Vertical,
            before: true,
        }
    ));
    for action in [
        TerminalContextMenuAction::CopySelection,
        TerminalContextMenuAction::ClearSelection,
        TerminalContextMenuAction::SelectAll,
        TerminalContextMenuAction::Paste,
        TerminalContextMenuAction::Search,
        TerminalContextMenuAction::ClearScreen,
    ] {
        assert!(!split_menu_action_requires_terminal_focus(action));
    }
}
