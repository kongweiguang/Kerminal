//! Quick Open 动作行的可用性与捕获目标测试。
//!
//! @author kongweiguang

use crate::features::quick_open::{QuickOpenMode, QuickOpenOpenContext, QuickOpenSelection};
use crate::keybindings::KeybindingAction;
use kerminal_application::sessions::UiSessionBinding;
use kerminal_domain::{
    commands::SnippetRisk,
    connections::RemoteProtocol,
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};

use super::{action_category, local_search_entries, remote_protocol_label, snippet_risk_label};

/// 构造被 Root 捕获的可见 UI 目标，测试不依赖运行中的 SessionManager。
///
/// @author kongweiguang
fn ui_binding() -> UiSessionBinding {
    UiSessionBinding {
        session_id: SessionId::new(),
        owner: SessionOwner::Ui {
            tab_id: TabId::new(),
            pane_id: PaneId::new(),
        },
        session_generation: 3,
    }
}

/// 终端搜索只要求打开时有可信 binding，不依赖能安全插入历史文本的 clean prompt。
///
/// @author kongweiguang
#[test]
fn terminal_search_remains_enabled_without_a_clean_prompt_context() {
    let binding = ui_binding();
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: Some(binding.clone()),
    };

    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "terminal.search");
    let search = entries
        .iter()
        .find(|entry| {
            entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::TerminalSearch))
        })
        .expect("terminal search action is listed");

    assert!(!search.disabled);
}

/// 没有打开时目标时仍显示禁用原因，避免命令面板默默省略用户可发现的动作。
///
/// @author kongweiguang
#[test]
fn terminal_search_is_disabled_when_no_binding_was_captured() {
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
    };

    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "terminal.search");
    let search = entries
        .iter()
        .find(|entry| {
            entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::TerminalSearch))
        })
        .expect("terminal search action remains discoverable");

    assert!(search.disabled);
    assert!(!search.detail.is_empty());
}

/// 命令面板粘贴动作只在打开时捕获了终端 binding 后可执行。
///
/// @author kongweiguang
#[test]
fn terminal_paste_requires_a_captured_binding() {
    let without_terminal = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
    };
    let unavailable = local_search_entries(
        &without_terminal,
        QuickOpenMode::CommandPalette,
        "terminal.paste",
    )
    .into_iter()
    .find(|entry| {
        entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::TerminalPaste))
    })
    .expect("terminal paste action remains discoverable");
    assert!(unavailable.disabled);
    assert!(!unavailable.label.is_empty());

    let with_terminal = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: Some(ui_binding()),
    };
    let available = local_search_entries(
        &with_terminal,
        QuickOpenMode::CommandPalette,
        "terminal.paste",
    )
    .into_iter()
    .find(|entry| {
        entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::TerminalPaste))
    })
    .expect("terminal paste action appears for captured target");
    assert!(!available.disabled);
}

/// 空查询保留原有首项并把退出动作放到末尾，避免 Enter 默认退出应用。
///
/// @author kongweiguang
#[test]
fn command_palette_empty_query_keeps_quit_after_existing_actions() {
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
    };

    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "");
    let first = entries.first().expect("default action list is not empty");
    let last = entries.last().expect("default action list is not empty");

    assert_eq!(
        first.selection,
        Some(QuickOpenSelection::Action(KeybindingAction::SettingsOpen))
    );
    assert_eq!(
        last.selection,
        Some(QuickOpenSelection::Action(
            KeybindingAction::ApplicationQuit
        ))
    );
}

/// 面板行保留稳定 action ID 供搜索，但显示的分类不得暴露内部 ID 或 Rust 类型名。
///
/// @author kongweiguang
#[test]
fn command_palette_uses_human_details_while_preserving_action_search_ids() {
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: Some(ui_binding()),
    };

    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "");
    let action_entries: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry.selection.as_ref() {
            Some(QuickOpenSelection::Action(action)) => Some((entry, *action)),
            _ => None,
        })
        .collect();

    assert!(!action_entries.is_empty());
    for (entry, action) in action_entries {
        let action_id = action.id();
        assert!(!entry.label.contains(action_id));
        assert!(!entry.detail.contains(action_id));
        assert!(!entry.detail.starts_with("Action ·"));
        assert!(entry.keywords.iter().any(|keyword| keyword == action_id));
    }
}

/// 协议和风险元数据使用人类词汇展示，不把 Rust enum 的 Debug 输出当产品文案。
///
/// @author kongweiguang
#[test]
fn quick_open_metadata_uses_readable_protocol_and_risk_labels() {
    assert_ne!(remote_protocol_label(RemoteProtocol::Ssh), "Host · Ssh");
    assert_ne!(snippet_risk_label(SnippetRisk::Inspect), "Inspect");
    assert_ne!(action_category(KeybindingAction::SettingsOpen), "Action");
}
