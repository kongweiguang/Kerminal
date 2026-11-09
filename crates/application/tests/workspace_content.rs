//! 扩展标签的状态与未知原文不依赖提供者是否在当前进程注册。
//!
//! @author kongweiguang

use kerminal_application::workspace::{WorkspaceSessionDocument, WorkspaceSessionTab};
use serde_json::{Value, json};

/// 构造只有非终端标签的合法会话，避免引入与插件无关的 pane 依赖。
/// @author kongweiguang
fn document_with_tab(tab: Value) -> WorkspaceSessionDocument {
    WorkspaceSessionDocument::try_from_value(json!({
        "version": 3,
        "activeTabId": "tab-extension",
        "focusedPaneId": "",
        "selectedMachineId": "",
        "sidebarMachines": [],
        "terminalPanes": [],
        "terminalTabs": [tab]
    }))
    .expect("extension fixture must be a valid session")
}

/// 提供者缺席时仍可读写其他工作区字段，扩展的状态、额外字段和显式 null 不受损。
/// @author kongweiguang
#[test]
fn extension_state_and_unknown_fields_survive_session_roundtrip() {
    let original = json!({
        "kind": "extension",
        "id": "tab-extension",
        "tabGroupId": null,
        "title": "Notes",
        "contentId": "example.notes",
        "stateVersion": 1,
        "state": {"text": "saved", "future": [1, {"opaque": true}]},
        "providerMetadata": {"version": 2},
        "machineId": {"opaque": "not a machine binding"}
    });
    let mut document = document_with_tab(original.clone());
    let mut snapshot = document.workspace_snapshot().unwrap();
    assert!(matches!(
        &snapshot.terminal_tabs[0],
        WorkspaceSessionTab::Extension { content_id, state, .. }
            if content_id == "example.notes" && state["text"] == "saved"
    ));
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(document.as_value()["terminalTabs"][0], original);
    assert_eq!(document.workspace_snapshot().unwrap(), snapshot);

    let WorkspaceSessionTab::Extension { state, .. } = &mut snapshot.terminal_tabs[0] else {
        panic!("extension must remain typed");
    };
    state["text"] = json!("updated");
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(
        document.as_value()["terminalTabs"][0]["state"]["text"],
        "updated"
    );
    assert_eq!(
        document.as_value()["terminalTabs"][0]["providerMetadata"],
        original["providerMetadata"]
    );
    assert_eq!(document.workspace_snapshot().unwrap(), snapshot);
}

/// 损坏或未来格式只能作为不可用原文保留，不能伪造状态或阻止整个会话加载。
/// @author kongweiguang
#[test]
fn malformed_extension_is_preserved_as_unavailable() {
    let original = json!({
        "kind": "extension",
        "id": "tab-extension",
        "title": "Future extension",
        "contentId": "example.future",
        "futureState": {"version": 9}
    });
    let mut document = document_with_tab(original.clone());
    let snapshot = document.workspace_snapshot().unwrap();
    assert!(matches!(
        &snapshot.terminal_tabs[0],
        WorkspaceSessionTab::Unsupported { kind, raw, .. }
            if kind == "extension" && raw == &original
    ));
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(document.as_value()["terminalTabs"][0], original);
}
