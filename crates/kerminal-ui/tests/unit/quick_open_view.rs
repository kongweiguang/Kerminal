//! Quick Open 动作行的可用性与捕获目标测试。
//!
//! @author kongweiguang

use std::collections::BTreeSet;

use crate::features::quick_open::{QuickOpenMode, QuickOpenOpenContext, QuickOpenSelection};
use crate::keybindings::KeybindingAction;
use kerminal_application::{preferences::ToolRailToolId, sessions::UiSessionBinding};
use kerminal_domain::{
    commands::SnippetRisk,
    connections::RemoteProtocol,
    identity::{PaneId, SessionId, TabId},
    sessions::SessionOwner,
};
use tokio_util::sync::CancellationToken;

use super::{
    action_category, action_label, append_command_catalogs, command_catalogs_enabled,
    command_history_enabled, local_search_entries, remote_protocol_label, snippet_risk_label,
};

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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
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

/// 命令面板与工具栏必须指向同一命令历史入口，内部 action ID 仍可用于快捷键检索。
///
/// @author kongweiguang
#[test]
fn command_history_action_matches_tool_rail_name_and_id() {
    let label = action_label(KeybindingAction::ToolLogs);
    assert!(label == "打开命令历史" || label == "Open command history");
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::new(),
    };
    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, &label);
    assert!(entries.iter().any(|entry| {
        entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::ToolLogs))
            && entry
                .keywords
                .iter()
                .any(|keyword| keyword == KeybindingAction::ToolLogs.id())
    }));
}

/// 禁用工具后命令面板不再列出对应动作，但其它工具动作仍可发现；空集合维持原行为。
///
/// @author kongweiguang
#[test]
fn command_palette_skips_actions_of_disabled_tool_units() {
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
        plugin_commands: Vec::new(),
        disabled_tools: BTreeSet::from([ToolRailToolId::Sftp]),
    };
    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "");
    assert!(
        !entries.iter().any(|entry| {
            entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::ToolSftp))
        }),
        "disabled SFTP action must not be listed"
    );
    assert!(
        entries.iter().any(|entry| {
            entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::ToolLogs))
        }),
        "other enabled tool actions remain discoverable"
    );

    let enabled = QuickOpenOpenContext {
        disabled_tools: BTreeSet::new(),
        ..context
    };
    let entries = local_search_entries(&enabled, QuickOpenMode::CommandPalette, "");
    assert!(
        entries.iter().any(|entry| {
            entry.selection == Some(QuickOpenSelection::Action(KeybindingAction::ToolSftp))
        }),
        "the same registry snapshot keeps SFTP listed when nothing is disabled"
    );
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

/// 过滤后的插件行仍携带面板打开时的完整身份和安全目标，而不是确认时重新查询当前标签。
#[test]
fn plugin_palette_entry_keeps_captured_command_and_context() {
    use crate::features::quick_open::QuickOpenPluginCommand;
    let command = QuickOpenPluginCommand {
        plugin: "fixture.commands".into(),
        command: "open".into(),
        title: "Open fixture".into(),
        tab: Some("original-tab".into()),
        host: Some("original-host".into()),
    };
    let context = QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
        plugin_commands: vec![command.clone()],
        disabled_tools: BTreeSet::new(),
    };
    let entries = local_search_entries(&context, QuickOpenMode::CommandPalette, "fixture.commands");
    assert_eq!(entries.len(), 1);
    assert_eq!(
        entries[0].selection,
        Some(QuickOpenSelection::PluginCommand(command))
    );
    assert!(!entries[0].disabled);
}

/// 构造只带冻结禁用集合的上下文；其余字段保持离线搜索的缺省值，
/// 测试因此不需要运行中的会话或插件目录。
///
/// @author kongweiguang
fn context_with_disabled(
    disabled: impl IntoIterator<Item = ToolRailToolId>,
) -> QuickOpenOpenContext {
    QuickOpenOpenContext {
        workspace: None,
        terminal: None,
        terminal_binding: None,
        plugin_commands: Vec::new(),
        disabled_tools: disabled.into_iter().collect(),
    }
}

/// commands 与 logs 各自独立由冻结的禁用工具集合表达；空集合代表离线测试，
/// 必须保持原有“视为启用”的语义。
///
/// @author kongweiguang
#[test]
fn frozen_disabled_tools_gate_commands_and_history_independently() {
    let enabled = context_with_disabled([]);
    assert!(command_catalogs_enabled(&enabled));
    assert!(command_history_enabled(&enabled));

    let commands_off = context_with_disabled([ToolRailToolId::Snippets]);
    assert!(!command_catalogs_enabled(&commands_off));
    assert!(
        command_history_enabled(&commands_off),
        "禁用片段不能连带关闭历史命令"
    );

    let logs_off = context_with_disabled([ToolRailToolId::Logs]);
    assert!(
        command_catalogs_enabled(&logs_off),
        "禁用历史不能连带关闭片段与工作流"
    );
    assert!(!command_history_enabled(&logs_off));
}

/// 两类来源都禁用时必须在查询服务句柄之前返回：不读取历史、片段或工作流，
/// 也不为缺失的离线服务追加“目录未配置”提示，避免空态挤压布局。
///
/// @author kongweiguang
#[test]
fn disabled_command_units_skip_catalog_reads_and_entries() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试运行时");
    let context = context_with_disabled([ToolRailToolId::Snippets, ToolRailToolId::Logs]);
    let mut entries = Vec::new();
    let mut statuses = Vec::new();

    runtime.block_on(append_command_catalogs(
        None,
        &context,
        "",
        &CancellationToken::new(),
        &mut entries,
        &mut statuses,
    ));

    assert!(entries.is_empty(), "禁用目录不得产生历史、片段或工作流行");
    assert!(statuses.is_empty(), "禁用来源不得追加目录状态或占位提示");
}

/// 未装配插件服务的离线环境仍沿用旧行为：缺少命令服务时保留可见状态，
/// 与空禁用集合“视为启用”的约定一致。
///
/// @author kongweiguang
#[test]
fn offline_empty_registry_keeps_legacy_catalog_status() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("测试运行时");
    let context = context_with_disabled([]);
    let mut entries = Vec::new();
    let mut statuses = Vec::new();

    runtime.block_on(append_command_catalogs(
        None,
        &context,
        "",
        &CancellationToken::new(),
        &mut entries,
        &mut statuses,
    ));

    assert!(entries.is_empty());
    assert_eq!(statuses.len(), 1, "离线测试仍需保留命令目录未配置的旧状态");
}
