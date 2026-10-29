//! Quick Open 的有界检索与工作区目标投影测试。
//!
//! @author kongweiguang

use std::collections::BTreeMap;

use kerminal_application::workspace::{
    WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionPane,
    WorkspaceSessionSnapshot, WorkspaceSessionTab,
};
use kerminal_domain::{
    identity::{PaneId, TabId},
    workspace::TerminalLayoutNode,
};

use crate::features::quick_open::model::workspace_entries;
use crate::features::quick_open::{QuickOpenEntry, QuickOpenSelection};

/// 命令历史只能作为安全的单行草稿插入，字符数与 UTF-8 字节数均受限。
#[test]
fn history_insertability_rejects_controls_and_unbounded_text() {
    assert!(QuickOpenEntry::history_command_is_insertable(
        "git status --short"
    ));
    assert!(QuickOpenEntry::history_command_is_insertable(
        &"界".repeat(8_000)
    ));
    assert!(!QuickOpenEntry::history_command_is_insertable("  \t  "));
    assert!(!QuickOpenEntry::history_command_is_insertable(
        "echo first\necho second"
    ));
    assert!(!QuickOpenEntry::history_command_is_insertable(
        "echo \u{1b}[31mred"
    ));
    assert!(!QuickOpenEntry::history_command_is_insertable(
        &"x".repeat(8_001)
    ));
    assert!(!QuickOpenEntry::history_command_is_insertable(
        &"界".repeat(8_001)
    ));
}

/// 工作区检索仅由真实快照生成结果，选择携带原始 TabId 而非显示行号。
#[test]
fn workspace_search_returns_the_exact_open_tab_identity() {
    let tab_id = TabId::new();
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: None,
        selected_machine_id: Some("host-1".to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::SftpTransfer {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "Staging files".to_owned(),
            machine_id: "host-1".to_owned(),
            left_host_id: None,
            locked_left_host_id: None,
            right_host_id: None,
            external_launch_id: None,
            initial_left_path: None,
            initial_right_path: None,
            initial_right_selection: None,
        }],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };

    let entries = workspace_entries(&snapshot, "STAGING");

    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].label, "Staging files");
    assert!(!entries[0].detail.contains("host-1"));
    assert!(!entries[0].detail.contains('·'));
    assert!(
        entries[0]
            .keywords
            .iter()
            .any(|keyword| keyword == "host-1")
    );
    assert_eq!(entries[0].selection, Some(QuickOpenSelection::Tab(tab_id)));
}

/// 内部连接 ID 仍可搜索，但不占据终端候选的可读副标题；空 cwd 不产生空目录细节。
///
/// @author kongweiguang
#[test]
fn workspace_subtitles_hide_machine_ids_and_omit_blank_cwd() {
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let snapshot = WorkspaceSessionSnapshot {
        pinned_containers: Vec::new(),
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: Some("machine-local".to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id.clone(),
            tab_group_id: None,
            title: "开发示例".to_owned(),
            machine_id: "machine-local".to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![WorkspaceSessionPane {
            id: pane_id,
            title: "PowerShell".to_owned(),
            machine_id: "machine-local".to_owned(),
            mode: WorkspacePaneMode::Local,
            target: None,
            remote_host_id: None,
            container_id: None,
            profile_id: None,
            shell: None,
            args: None,
            cwd: None,
            current_cwd: Some("  \t".to_owned()),
            env: None,
            remote_command: None,
            tmux_binding: None,
            prompt: String::new(),
            status: WorkspaceMachineStatus::Online,
            latency_ms: None,
            local_machine_scope: Some(WorkspaceLocalMachineScope::Workspace),
            output_history: None,
        }],
        tab_groups: BTreeMap::new(),
    };

    let entries = workspace_entries(&snapshot, "");
    let terminal = entries
        .iter()
        .find(|entry| matches!(entry.selection.as_ref(), Some(QuickOpenSelection::Tab(_))))
        .expect("terminal tab appears in search");
    let pane = entries
        .iter()
        .find(|entry| {
            matches!(
                entry.selection.as_ref(),
                Some(QuickOpenSelection::Pane { .. })
            )
        })
        .expect("terminal pane appears in search");

    assert!(!terminal.detail.contains("machine-local"));
    assert!(
        terminal
            .keywords
            .iter()
            .any(|keyword| keyword == "machine-local")
    );
    assert!(matches!(pane.detail.as_str(), "终端分屏" | "Terminal pane"));
    assert!(!pane.detail.ends_with('·'));
    assert!(
        pane.keywords
            .iter()
            .any(|keyword| keyword == "machine-local")
    );

    let id_search = workspace_entries(&snapshot, "machine-local");
    assert_eq!(id_search.len(), 2);
}
