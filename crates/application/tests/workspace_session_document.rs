//! 工作区 v3 codec 在应用层拒绝危险覆盖并保留未知字段。
//!
//! @author kongweiguang

use kerminal_application::workspace::{
    WorkspaceLocalMachineScope, WorkspaceMachineStatus, WorkspaceSessionDocument,
    WorkspaceSessionInvalidReason,
};
use kerminal_domain::{identity::PaneId, workspace::TerminalLayoutNode};
use serde_json::{Value, json};

/// 生成满足当前版本根结构的最小文档，便于测试只关注一个边界条件。
fn valid_document() -> Value {
    json!({
        "version": 3,
        "sidebarMachines": [],
        "terminalPanes": [],
        "terminalTabs": [],
        "futureExtension": { "opaque": [1, "kept"] }
    })
}

/// 读取包含有效 tab/pane 引用的当前 fixture，避免严格字段测试被布局错误干扰。
///
/// @author kongweiguang
fn workspace_fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../tests/fixtures/workspace_session_v3.json"
    ))
    .expect("workspace fixture should be valid JSON")
}

/// 按稳定 `id` 找 row，避免测试把重排后的数组下标误当成业务身份。
fn row_by_id<'a>(rows: &'a Value, id: &str) -> &'a Value {
    rows.as_array()
        .expect("workspace collection should be an array")
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(id))
        .unwrap_or_else(|| panic!("workspace row {id} should exist"))
}

/// 未知扩展字段必须往返保留，避免原生 UI 保存时擦除旧版本或插件数据。
#[test]
fn v3_document_preserves_unknown_fields() {
    let source = valid_document();
    let decoded = WorkspaceSessionDocument::try_from_value(source.clone())
        .expect("minimal version 3 document should be accepted");

    assert_eq!(decoded.as_value(), &source);
    assert_eq!(decoded.into_value(), source);
}

/// 空文档是合法 v3 首存基线，三个未选择身份与首次写入状态保持 Missing 语义。
#[test]
fn empty_v3_document_starts_with_no_selected_resources() {
    let document = WorkspaceSessionDocument::empty_v3();
    let snapshot = document.workspace_snapshot().unwrap();

    assert_eq!(snapshot.active_tab_id, None);
    assert_eq!(snapshot.focused_pane_id, None);
    assert_eq!(snapshot.selected_machine_id, None);
    assert!(snapshot.terminal_tabs.is_empty());
    assert!(snapshot.terminal_panes.is_empty());
    assert!(snapshot.tab_groups.is_empty());
    assert_eq!(document.as_value()["version"], 3);
}

/// 关键数组缺失或根不是对象时不能退化成空会话并覆盖原文件。
#[test]
fn rejects_invalid_root_and_missing_required_arrays() {
    assert_eq!(
        WorkspaceSessionDocument::try_from_value(json!([])).unwrap_err(),
        WorkspaceSessionInvalidReason::InvalidRoot
    );

    let mut missing_tabs = valid_document();
    missing_tabs
        .as_object_mut()
        .expect("fixture root is an object")
        .remove("terminalTabs");
    assert_eq!(
        WorkspaceSessionDocument::try_from_value(missing_tabs).unwrap_err(),
        WorkspaceSessionInvalidReason::InvalidSchema
    );
}

/// 版本号不匹配交由明确的 legacy/future 分类处理，不能被当成当前 v3 写回。
#[test]
fn rejects_wrong_schema_version() {
    let mut wrong_version = valid_document();
    wrong_version["version"] = json!(4);

    assert_eq!(
        WorkspaceSessionDocument::try_from_value(wrong_version).unwrap_err(),
        WorkspaceSessionInvalidReason::InvalidSchema
    );
}

/// 当前 reader 拒绝缺失/null 字段，但保留显式空 prompt 和已知状态的精确语义。
#[test]
fn current_panes_require_prompt_and_known_status() {
    for prompt in ["missing", "null"] {
        let mut source = workspace_fixture();
        if prompt == "missing" {
            source["terminalPanes"][0]
                .as_object_mut()
                .expect("pane is an object")
                .remove("prompt");
        } else {
            source["terminalPanes"][0]["prompt"] = Value::Null;
        }
        assert_eq!(
            WorkspaceSessionDocument::try_from_value(source).unwrap_err(),
            WorkspaceSessionInvalidReason::InvalidSchema
        );
    }

    for status in ["missing", "null", "unknown"] {
        let mut source = workspace_fixture();
        match status {
            "missing" => {
                source["terminalPanes"][0]
                    .as_object_mut()
                    .expect("pane is an object")
                    .remove("status");
            }
            "null" => source["terminalPanes"][0]["status"] = Value::Null,
            _ => source["terminalPanes"][0]["status"] = json!("future-status"),
        }
        assert_eq!(
            WorkspaceSessionDocument::try_from_value(source).unwrap_err(),
            WorkspaceSessionInvalidReason::InvalidSchema
        );
    }

    let mut source = workspace_fixture();
    source["terminalPanes"][0]["prompt"] = json!("");
    source["terminalPanes"][0]["status"] = json!("warning");
    let document = WorkspaceSessionDocument::try_from_value(source)
        .expect("explicit empty prompt and known status are current data");
    let snapshot = document.workspace_snapshot().unwrap();
    assert_eq!(snapshot.terminal_panes[0].prompt, "");
    assert_eq!(
        snapshot.terminal_panes[0].status,
        WorkspaceMachineStatus::Warning
    );
}

/// localMachineScope 仍支持缺失/null 的可选语义，但未知值不能被伪装成 sidebar。
#[test]
fn local_machine_scope_keeps_optional_current_semantics() {
    for scope in ["missing", "null"] {
        let mut source = workspace_fixture();
        if scope == "missing" {
            source["terminalPanes"][0]
                .as_object_mut()
                .expect("pane is an object")
                .remove("localMachineScope");
        } else {
            source["terminalPanes"][0]["localMachineScope"] = Value::Null;
        }
        let document = WorkspaceSessionDocument::try_from_value(source)
            .expect("optional local scope can be absent");
        assert_eq!(
            document.workspace_snapshot().unwrap().terminal_panes[0].local_machine_scope,
            None
        );
    }

    let mut explicit = workspace_fixture();
    explicit["terminalPanes"][0]["localMachineScope"] = json!("workspace");
    let document = WorkspaceSessionDocument::try_from_value(explicit)
        .expect("known local scope remains valid");
    assert_eq!(
        document.workspace_snapshot().unwrap().terminal_panes[0].local_machine_scope,
        Some(WorkspaceLocalMachineScope::Workspace)
    );

    let mut unknown = workspace_fixture();
    unknown["terminalPanes"][0]["localMachineScope"] = json!("future-scope");
    assert_eq!(
        WorkspaceSessionDocument::try_from_value(unknown).unwrap_err(),
        WorkspaceSessionInvalidReason::InvalidSchema
    );
}

/// 凭据正文禁止进入 workspace，而 encrypted vault 的引用字段仍可保存。
#[test]
fn rejects_credential_material_but_allows_vault_references() {
    let mut with_secret = valid_document();
    with_secret["terminalPanes"] = json!([{
        "id": "p1",
        "title": "local",
        "machineId": "local",
        "mode": "local",
        "prompt": "PS>",
        "status": "offline",
        "password": "secret"
    }]);
    assert_eq!(
        WorkspaceSessionDocument::try_from_value(with_secret).unwrap_err(),
        WorkspaceSessionInvalidReason::CredentialMaterial
    );

    let mut with_reference = valid_document();
    with_reference["terminalPanes"] = json!([{
        "id": "p1",
        "title": "local",
        "machineId": "local",
        "mode": "local",
        "prompt": "PS>",
        "status": "offline",
        "secret_ref": "vault://host/p1"
    }]);
    assert!(WorkspaceSessionDocument::try_from_value(with_reference).is_ok());
}

/// 证明 v3 未知字段随 tab/pane ID 跨数组重排和布局更新保留，而不随旧下标移动。
#[test]
fn typed_v3_snapshot_updates_layout_without_erasing_extensions() {
    let source: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/workspace_session_v3.json"
    ))
    .unwrap();
    let mut document = WorkspaceSessionDocument::try_from_value(source.clone()).unwrap();
    let mut snapshot = document.workspace_snapshot().unwrap();
    assert_eq!(snapshot.terminal_tabs.len(), 3);
    assert_eq!(snapshot.terminal_panes.len(), 2);
    assert_eq!(
        snapshot.active_tab_id.as_ref().unwrap().as_str(),
        "tab-terminal"
    );

    if let kerminal_application::workspace::WorkspaceSessionTab::Terminal { layout, .. } =
        &mut snapshot.terminal_tabs[0]
    {
        *layout = serde_json::from_value(json!({
            "type": "split", "id": "split-a", "direction": "horizontal",
            "children": [{"type": "pane", "paneId": "pane-a"}, {"type": "pane", "paneId": "pane-b"}],
            "sizes": {"pane-a": 0.7, "pane-b": 0.3}
        }))
        .unwrap();
    }
    snapshot.terminal_tabs.rotate_left(1);
    snapshot.terminal_panes.swap(0, 1);
    snapshot.tab_groups.values_mut().next().unwrap().collapsed = true;
    document.update_known_fields(&snapshot).unwrap();
    let updated = document.into_value();

    assert_eq!(updated["futureRoot"]["keep"], json!(true));
    assert_eq!(updated["shellLayout"]["leftPanelWidth"], json!(280));
    let transfer_tab = row_by_id(&updated["terminalTabs"], "tab-transfer");
    let terminal_tab = row_by_id(&updated["terminalTabs"], "tab-terminal");
    let file_tab = row_by_id(&updated["terminalTabs"], "tab-file");
    assert_eq!(transfer_tab["kind"], json!("sftpTransfer"));
    assert_eq!(
        transfer_tab["initialLeftPath"],
        json!("C:/Users/example/Downloads")
    );
    assert_eq!(terminal_tab["futureTab"], json!(["kept"]));
    assert_eq!(terminal_tab["layout"]["futureSplit"], json!("kept"));
    assert_eq!(
        terminal_tab["layout"]["children"][0]["futureLeaf"],
        json!("kept")
    );
    assert_eq!(transfer_tab["futureTransfer"], json!(1));
    assert_eq!(file_tab["futureFile"], json!(2));
    let pane_a = row_by_id(&updated["terminalPanes"], "pane-a");
    assert_eq!(pane_a["futurePane"]["value"], json!(true));
    assert_eq!(pane_a["lines"], json!(["volatile"]));
    assert_eq!(pane_a["target"]["futureTarget"], json!("kept"));
    assert_eq!(
        row_by_id(&updated["terminalPanes"], "pane-b")["futurePane"],
        Value::Null
    );
    assert_eq!(
        updated["terminalTabGroups"]["group-a"]["futureGroup"],
        json!(7)
    );
    assert_eq!(
        updated["terminalTabGroups"]["group-a"]["collapsed"],
        json!(true)
    );
    assert_eq!(terminal_tab["layout"]["sizes"]["pane-a"], json!(0.7));
}

/// 旧版 SFTP transfer tab 缺少本地起始目录时仍解码为空，恢复逻辑继续采用工作区默认目录。
///
/// @author kongweiguang
#[test]
fn legacy_sftp_transfer_without_initial_left_path_uses_existing_default() {
    let mut source = workspace_fixture();
    source["terminalTabs"]
        .as_array_mut()
        .expect("tab rows should be an array")
        .iter_mut()
        .find(|row| row.get("id").and_then(Value::as_str) == Some("tab-transfer"))
        .and_then(Value::as_object_mut)
        .expect("transfer row should exist")
        .remove("initialLeftPath");

    let snapshot = WorkspaceSessionDocument::try_from_value(source)
        .expect("old workspace snapshot remains compatible")
        .workspace_snapshot()
        .expect("old transfer descriptor should decode");
    let transfer = snapshot
        .terminal_tabs
        .iter()
        .find_map(|tab| match tab {
            kerminal_application::workspace::WorkspaceSessionTab::SftpTransfer {
                id,
                initial_left_path,
                ..
            } if id.as_str() == "tab-transfer" => Some(initial_left_path),
            _ => None,
        })
        .expect("transfer tab should be present");

    assert!(transfer.is_none());
}

/// 删除 tab/pane 后不能复活旧 raw 行，新 pane 也不得按数组位置继承别人的未知字段。
#[test]
fn removed_workspace_identity_does_not_resurrect_or_transfer_extensions() {
    let mut source: Value = serde_json::from_str(include_str!(
        "../../../tests/fixtures/workspace_session_v3.json"
    ))
    .unwrap();
    source["terminalPanes"][1]["futurePane"] = json!({"owner": "pane-b"});
    let mut document = WorkspaceSessionDocument::try_from_value(source).unwrap();
    let mut snapshot = document.workspace_snapshot().unwrap();

    snapshot
        .terminal_tabs
        .retain(|tab| tab.id().as_str() != "tab-transfer");
    let terminal = snapshot
        .terminal_tabs
        .iter_mut()
        .find_map(|tab| match tab {
            kerminal_application::workspace::WorkspaceSessionTab::Terminal { layout, .. } => {
                Some(layout)
            }
            _ => None,
        })
        .expect("terminal tab should remain");
    *terminal = serde_json::from_value::<TerminalLayoutNode>(json!({
        "type": "split",
        "id": "split-new",
        "direction": "horizontal",
        "children": [
            {"type": "pane", "paneId": "pane-a"},
            {"type": "pane", "paneId": "pane-c"}
        ],
        "sizes": {"pane-a": 0.5, "pane-c": 0.5}
    }))
    .unwrap();

    let pane_b_index = snapshot
        .terminal_panes
        .iter()
        .position(|pane| pane.id.as_str() == "pane-b")
        .expect("pane-b should exist before replacement");
    let mut pane_c = snapshot.terminal_panes.remove(pane_b_index);
    pane_c.id = PaneId::parse("pane-c").expect("test pane id is valid");
    snapshot.terminal_panes.push(pane_c);

    document.update_known_fields(&snapshot).unwrap();
    let updated = document.into_value();
    let tabs = updated["terminalTabs"].as_array().unwrap();
    let panes = updated["terminalPanes"].as_array().unwrap();

    assert!(!tabs.iter().any(|tab| tab["id"] == "tab-transfer"));
    assert!(!panes.iter().any(|pane| pane["id"] == "pane-b"));
    assert_eq!(
        row_by_id(&updated["terminalPanes"], "pane-a")["futurePane"]["value"],
        json!(true)
    );
    assert_eq!(
        row_by_id(&updated["terminalPanes"], "pane-c")["futurePane"],
        Value::Null
    );
    assert!(tabs.iter().all(|tab| tab.get("futureTransfer").is_none()));
}

/// 未知 target 只作为不可用的原始引用交给 UI，不能回退到 pane 的 mode 或机器 ID。
#[test]
fn unknown_target_is_preserved_as_unavailable_instead_of_falling_back() {
    let mut source = valid_document();
    source["terminalPanes"] = json!([{
        "id": "pane-a", "title": "Terminal", "machineId": "machine-a", "mode": "ssh",
        "target": {"kind": "futureProtocol", "opaque": 3}, "prompt": "PS>", "status": "offline"
    }]);
    source["terminalTabs"] = json!([{
        "id": "tab-a", "title": "Terminal", "machineId": "machine-a",
        "layout": {"type": "pane", "paneId": "pane-a"}
    }]);
    let mut document = WorkspaceSessionDocument::try_from_value(source).unwrap();
    let snapshot = document.workspace_snapshot().unwrap();
    assert!(matches!(
        snapshot.terminal_panes[0].target,
        Some(kerminal_application::workspace::WorkspaceTargetRef::Unavailable { .. })
    ));
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(
        document.as_value()["terminalPanes"][0]["target"]["kind"],
        json!("futureProtocol")
    );
}
