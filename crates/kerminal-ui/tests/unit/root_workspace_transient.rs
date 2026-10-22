//! Root workspace transient persistence projection tests.
//!
//! @author kongweiguang

use std::collections::{BTreeMap, BTreeSet};

use kerminal_application::workspace::{
    WorkspaceMachineStatus, WorkspacePaneMode, WorkspaceSessionPane, WorkspaceSessionSnapshot,
    WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::{
    identity::{HostId, PaneId, TabId},
    workspace::{SplitDirection, TerminalLayoutNode},
};

use crate::root::workspace_transient::without_transient_resources;

/// 使用明确的 machine/host 身份构造最小 SSH pane，不带启动参数或凭据内容。
fn ssh_pane(id: PaneId, host_id: HostId, machine_id: &str) -> WorkspaceSessionPane {
    WorkspaceSessionPane {
        id,
        title: machine_id.to_owned(),
        machine_id: machine_id.to_owned(),
        mode: WorkspacePaneMode::Ssh,
        target: Some(WorkspaceTargetRef::Ssh {
            host_id: host_id.clone(),
        }),
        remote_host_id: Some(host_id),
        container_id: None,
        profile_id: None,
        shell: None,
        args: None,
        cwd: None,
        current_cwd: None,
        env: None,
        remote_command: None,
        tmux_binding: None,
        prompt: String::new(),
        status: WorkspaceMachineStatus::Offline,
        latency_ms: None,
        local_machine_scope: None,
        output_history: None,
    }
}

/// 创建合法布局基线；不同 tab 使用不同 pane，split 的 size key 必须是稳定 pane ID。
fn mixed_snapshot() -> (WorkspaceSessionSnapshot, TabId, TabId, PaneId, PaneId) {
    let saved_host_id = HostId::new();
    let external_host_id = HostId::parse("external:temporary-target").expect("valid host ID");
    let saved_tab_id = TabId::new();
    let external_tab_id = TabId::new();
    let saved_pane_id = PaneId::new();
    let external_pane_id = PaneId::new();
    let external_tab_pane_id = PaneId::new();
    let layout = TerminalLayoutNode::Split {
        id: "split-test".to_owned(),
        direction: SplitDirection::Horizontal,
        children: vec![
            TerminalLayoutNode::pane(saved_pane_id.clone()),
            TerminalLayoutNode::pane(external_pane_id.clone()),
        ],
        sizes: Some(BTreeMap::from([
            (saved_pane_id.as_str().to_owned(), 0.5),
            (external_pane_id.as_str().to_owned(), 0.5),
        ])),
    };
    let saved_tab = WorkspaceSessionTab::Terminal {
        id: saved_tab_id.clone(),
        tab_group_id: None,
        title: "Saved host".to_owned(),
        machine_id: saved_host_id.as_str().to_owned(),
        layout,
    };
    let external_tab = WorkspaceSessionTab::Terminal {
        id: external_tab_id.clone(),
        tab_group_id: None,
        title: "External request".to_owned(),
        machine_id: external_host_id.as_str().to_owned(),
        layout: TerminalLayoutNode::pane(external_tab_pane_id.clone()),
    };
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(external_tab_id.clone()),
        focused_pane_id: Some(external_tab_pane_id.clone()),
        selected_machine_id: Some(external_host_id.as_str().to_owned()),
        terminal_tabs: vec![saved_tab, external_tab],
        terminal_panes: vec![
            ssh_pane(saved_pane_id.clone(), saved_host_id, "saved-machine"),
            ssh_pane(
                external_pane_id.clone(),
                external_host_id.clone(),
                "external-machine",
            ),
            ssh_pane(
                external_tab_pane_id,
                external_host_id,
                "external-tab-machine",
            ),
        ],
        tab_groups: BTreeMap::new(),
    };
    snapshot
        .validate()
        .expect("fixture has unique layout references and valid child size keys");
    (
        snapshot,
        saved_tab_id,
        external_tab_id,
        saved_pane_id,
        external_pane_id,
    )
}

/// 剔除 external ID 后保留普通标签，并将活动标签与焦点切回仍存在的 pane。
#[test]
fn transient_tab_and_external_host_ids_are_removed_from_persistence() {
    let (snapshot, saved_tab, external_tab, saved_pane, _) = mixed_snapshot();
    let projected = without_transient_resources(
        &snapshot,
        &BTreeSet::from([external_tab.clone()]),
        &BTreeSet::new(),
    )
    .expect("valid snapshot produces a safe persistence projection");

    assert_eq!(projected.terminal_tabs.len(), 1);
    assert_eq!(projected.terminal_tabs[0].id(), &saved_tab);
    assert_eq!(projected.terminal_panes.len(), 1);
    assert_eq!(projected.terminal_panes[0].id, saved_pane);
    assert_eq!(projected.active_tab_id, Some(saved_tab));
    assert_eq!(projected.focused_pane_id, Some(saved_pane));
    assert_eq!(
        projected.selected_machine_id.as_deref(),
        Some("saved-machine")
    );
    assert!(
        !projected
            .terminal_tabs
            .iter()
            .any(|tab| tab.id() == &external_tab)
    );
    assert!(projected.validate().is_ok());
}

/// 单个 transient pane 从 split 中移除后按领域规则折叠布局，不保留过期 size key。
#[test]
fn transient_pane_is_removed_and_split_collapses_to_survivor() {
    let (snapshot, saved_tab, _, saved_pane, external_pane) = mixed_snapshot();
    let projected = without_transient_resources(
        &snapshot,
        &BTreeSet::new(),
        &BTreeSet::from([external_pane]),
    )
    .expect("valid snapshot produces a normalized layout");

    assert_eq!(projected.terminal_tabs.len(), 1);
    assert_eq!(projected.terminal_tabs[0].id(), &saved_tab);
    assert!(matches!(
        projected.terminal_tabs[0].terminal_layout(),
        Some(TerminalLayoutNode::Pane { pane_id }) if pane_id == &saved_pane
    ));
    assert_eq!(projected.terminal_panes.len(), 1);
    assert_eq!(projected.focused_pane_id, Some(saved_pane));
    assert!(projected.validate().is_ok());
}

/// 运行态 owner 标记即使对应正常保存的 HostId，也必须独立阻止临时 pane 落盘。
#[test]
fn explicit_transient_pane_marker_is_authoritative_for_saved_host_targets() {
    let (mut snapshot, saved_tab, _, saved_pane, transient_pane) = mixed_snapshot();
    let saved_host = snapshot.terminal_panes[0]
        .remote_host_id
        .clone()
        .expect("saved host pane has an explicit host id");
    let transient_runtime_pane = snapshot
        .terminal_panes
        .iter_mut()
        .find(|pane| pane.id == transient_pane)
        .expect("transient pane exists in the source snapshot");
    transient_runtime_pane.remote_host_id = Some(saved_host.clone());
    transient_runtime_pane.target = Some(WorkspaceTargetRef::Ssh {
        host_id: saved_host.clone(),
    });
    if let Some(WorkspaceSessionTab::Terminal { machine_id, .. }) =
        snapshot.terminal_tabs.iter_mut().find(|tab| {
            tab.terminal_layout()
                .is_some_and(|layout| layout.contains_pane(&transient_pane))
        })
    {
        *machine_id = saved_host.as_str().to_owned();
    }

    let projected = without_transient_resources(
        &snapshot,
        &BTreeSet::new(),
        &BTreeSet::from([transient_pane]),
    )
    .expect("explicit runtime marker produces a safe persistence projection");

    assert_eq!(projected.terminal_tabs.len(), 1);
    assert_eq!(projected.terminal_tabs[0].id(), &saved_tab);
    assert_eq!(projected.terminal_panes.len(), 1);
    assert_eq!(projected.terminal_panes[0].id, saved_pane);
    assert!(projected.validate().is_ok());
}

/// 删除快照内最后一个 transient 标签后清空所有选择，确保无悬空 active/pane ID。
#[test]
fn removing_last_transient_tab_clears_all_active_ids() {
    let external_host = HostId::parse("external:last-target").expect("valid host ID");
    let tab_id = TabId::new();
    let pane_id = PaneId::new();
    let mut snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: Some(pane_id.clone()),
        selected_machine_id: Some(external_host.as_str().to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::Terminal {
            id: tab_id,
            tab_group_id: None,
            title: "External".to_owned(),
            machine_id: external_host.as_str().to_owned(),
            layout: TerminalLayoutNode::pane(pane_id.clone()),
        }],
        terminal_panes: vec![ssh_pane(pane_id, external_host, "external")],
        tab_groups: BTreeMap::new(),
    };
    let transient_tabs = snapshot
        .terminal_tabs
        .iter()
        .map(|tab| tab.id().clone())
        .collect::<BTreeSet<_>>();

    snapshot = without_transient_resources(&snapshot, &transient_tabs, &BTreeSet::new())
        .expect("empty safe projection is valid");

    assert!(snapshot.terminal_tabs.is_empty());
    assert!(snapshot.terminal_panes.is_empty());
    assert_eq!(snapshot.active_tab_id, None);
    assert_eq!(snapshot.focused_pane_id, None);
    assert_eq!(snapshot.selected_machine_id, None);
    assert!(snapshot.validate().is_ok());
}

/// externalLaunchId 是 opaque 临时启动关联，即使是普通 UUID 也不能写回长期工作区。
#[test]
fn sftp_tab_with_uuid_external_launch_id_is_transient() {
    let id = TabId::new();
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(id.clone()),
        focused_pane_id: None,
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::SftpTransfer {
            id,
            tab_group_id: None,
            title: "External SFTP".to_owned(),
            machine_id: "external-request".to_owned(),
            left_host_id: None,
            locked_left_host_id: None,
            right_host_id: None,
            external_launch_id: Some("a6df95be-2417-41a7-b789-2a21c4bf6b09".to_owned()),
            initial_left_path: None,
            initial_right_path: None,
            initial_right_selection: None,
        }],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };

    let projected = without_transient_resources(&snapshot, &BTreeSet::new(), &BTreeSet::new())
        .expect("UUID external launch association is excluded safely");

    assert!(projected.terminal_tabs.is_empty());
    assert_eq!(projected.active_tab_id, None);
    assert!(projected.validate().is_ok());
}

/// 未知未来版 tab 的原始载荷若带临时主机身份，也不应被无损保留到旧 schema 文件。
#[test]
fn unsupported_tab_with_external_host_reference_is_removed() {
    let external_host = HostId::parse("external:unknown-target").expect("valid host ID");
    let tab_id = TabId::new();
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(tab_id.clone()),
        focused_pane_id: None,
        selected_machine_id: Some(external_host.as_str().to_owned()),
        terminal_tabs: vec![WorkspaceSessionTab::Unsupported {
            id: tab_id,
            tab_group_id: None,
            title: "Future tab".to_owned(),
            kind: "future-resource".to_owned(),
            raw: serde_json::json!({
                "payload": {
                    "hostId": external_host.as_str(),
                    "opaqueField": "kept only if the tab is safe"
                }
            }),
        }],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };

    let projected = without_transient_resources(&snapshot, &BTreeSet::new(), &BTreeSet::new())
        .expect("unknown external tab is excluded without rejecting the rest of the snapshot");

    assert!(projected.terminal_tabs.is_empty());
    assert_eq!(projected.active_tab_id, None);
    assert_eq!(projected.selected_machine_id, None);
    assert!(projected.validate().is_ok());
}

/// 未知 tab 的普通文本字段即使长得像 external ID 也不是目标引用，必须原样保留。
#[test]
fn unsupported_tab_with_unrelated_external_looking_text_is_retained() {
    let id = TabId::new();
    let note = "external:ordinary-description";
    let snapshot = WorkspaceSessionSnapshot {
        active_tab_id: Some(id.clone()),
        focused_pane_id: None,
        selected_machine_id: None,
        terminal_tabs: vec![WorkspaceSessionTab::Unsupported {
            id,
            tab_group_id: None,
            title: "Future tab".to_owned(),
            kind: "future-resource".to_owned(),
            raw: serde_json::json!({"description": note}),
        }],
        terminal_panes: Vec::new(),
        tab_groups: BTreeMap::new(),
    };

    let projected = without_transient_resources(&snapshot, &BTreeSet::new(), &BTreeSet::new())
        .expect("unrelated unknown text does not invalidate the snapshot");

    assert_eq!(projected.terminal_tabs.len(), 1);
    assert!(matches!(
        &projected.terminal_tabs[0],
        WorkspaceSessionTab::Unsupported { raw, .. }
            if raw.get("description").and_then(serde_json::Value::as_str) == Some(note)
    ));
}
