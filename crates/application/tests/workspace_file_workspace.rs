//! 文件工作区 codec 的版本、引用和未知字段保真边界。
//!
//! @author kongweiguang

use kerminal_application::workspace::{
    FileWorkspaceItem, WorkspaceFileSource, WorkspaceSessionDocument,
    WorkspaceSessionInvalidReason, WorkspaceSessionTab, WorkspaceTargetRef,
};
use kerminal_domain::identity::TabId;
use serde_json::{Value, json};

/// 最小 v3 外壳保持不变，内部状态版本单独演进，不借用插件 Extension 绕过生命周期。
/// @author kongweiguang
fn document_value() -> Value {
    json!({
        "version": 3, "sidebarMachines": [], "terminalPanes": [],
        "activeTabId": "files", "focusedPaneId": "",
        "terminalTabs": [{
            "id": "files", "title": "主机 · 文件", "kind": "fileWorkspace",
            "machineId": "host:server", "target": {"kind":"ssh", "hostId":"server", "futureTarget": true},
            "stateVersion": 1, "futureOwner": {"keep": true},
            "state": {
                "futureState": [1,2], "activeItemId":"doc-b", "sourceTabIds":["old-a", "old-b"],
                "items": [
                    {"kind":"directory", "id":"directory", "path":"/etc", "futureDirectory":true},
                    {"kind":"document", "id":"doc-a", "title":"自定义 A", "path":"/etc/a", "rootPath":"/etc", "access":"readonly", "source":"composeYaml", "futureDocument":{"keep":"a"}},
                    {"kind":"document", "id":"doc-b", "title":"B", "path":"/etc/b", "access":"editable", "source":"sftp", "futureDocument":{"keep":"b"}}
                ]
            }
        }]
    })
}

/// 重排、改名及改变活动项只改已知字段，内部项的扩展按 ID 跟随，而非原数组位置。
/// @author kongweiguang
#[test]
fn file_workspace_roundtrip_preserves_nested_unknown_fields_by_identity() {
    let mut document = WorkspaceSessionDocument::try_from_value(document_value()).unwrap();
    let mut snapshot = document.workspace_snapshot().unwrap();
    let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut snapshot.terminal_tabs[0] else {
        panic!("显式文件工作区必须解码为内置 owner");
    };
    assert_eq!(state.active_path(), Some("/etc/b"));
    assert_eq!(state.active_item().unwrap().id().as_str(), "doc-b");
    state.items.swap(1, 2);
    let FileWorkspaceItem::Document { title, .. } = &mut state.items[1] else {
        panic!("文档项");
    };
    *title = "改名 B".into();
    state.active_item_id = Some(TabId::parse("doc-a").unwrap());
    document.update_known_fields(&snapshot).unwrap();
    let row = &document.as_value()["terminalTabs"][0];
    assert_eq!(row["kind"], "fileWorkspace");
    assert_eq!(row["stateVersion"], 1);
    assert_eq!(document.as_value()["version"], 3);
    assert_eq!(row["futureOwner"]["keep"], true);
    assert_eq!(row["target"]["futureTarget"], true);
    assert_eq!(row["state"]["futureState"], json!([1, 2]));
    assert_eq!(row["state"]["items"][0]["futureDirectory"], true);
    assert_eq!(row["state"]["items"][1]["futureDocument"]["keep"], "b");
    assert_eq!(row["state"]["items"][2]["futureDocument"]["keep"], "a");
    assert_eq!(row["state"]["items"][2]["rootPath"], "/etc");
    assert_eq!(row["state"]["items"][2]["access"], "readonly");
    assert_eq!(row["state"]["items"][2]["source"], "composeYaml");
    assert_eq!(row["state"]["activeItemId"], "doc-a");
    assert_eq!(row["state"]["sourceTabIds"], json!(["old-a", "old-b"]));
}

/// 未知内部版本、非法引用和权限绝不退化成可写或空工作区，避免下一次保存覆盖恢复材料。
/// @author kongweiguang
#[test]
fn file_workspace_rejects_future_versions_and_invalid_references() {
    for (pointer, value) in [
        ("/terminalTabs/0/stateVersion", json!(2)),
        ("/terminalTabs/0/stateVersion", Value::Null),
        ("/terminalTabs/0/state/activeItemId", json!("missing")),
        ("/terminalTabs/0/state/items", json!([])),
        ("/terminalTabs/0/state/items/1/id", json!("directory")),
        ("/terminalTabs/0/state/items/1/access", json!("future-edit")),
        (
            "/terminalTabs/0/state/items/1/source",
            json!("future-source"),
        ),
        ("/terminalTabs/0/state/items/1/path", json!("bad\npath")),
        ("/terminalTabs/0/state/items/1/kind", json!("future-item")),
        (
            "/terminalTabs/0/state/sourceTabIds",
            json!(["old-a", "old-a"]),
        ),
    ] {
        let mut value_to_check = document_value();
        *value_to_check.pointer_mut(pointer).unwrap() = value;
        assert!(
            WorkspaceSessionDocument::try_from_value(value_to_check).is_err(),
            "必须拒绝 {pointer}"
        );
    }
}

/// 旧来源别名可以在文档关闭后保留，但不能被另一 owner 或仍存活顶层标签认领。
/// @author kongweiguang
#[test]
fn file_workspace_source_aliases_outlive_documents_without_becoming_cross_owner_references() {
    let mut document = WorkspaceSessionDocument::try_from_value(document_value()).unwrap();
    let mut snapshot = document.workspace_snapshot().unwrap();
    let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut snapshot.terminal_tabs[0] else {
        panic!("文件 owner");
    };
    state.items.truncate(1);
    state.active_item_id = Some(TabId::parse("directory").unwrap());
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(
        document.as_value()["terminalTabs"][0]["state"]["sourceTabIds"],
        json!(["old-a", "old-b"])
    );

    let mut value = document_value();
    let mut other = value["terminalTabs"][0].clone();
    other["id"] = json!("files-other");
    other["state"]["items"] = json!([]);
    other["state"]
        .as_object_mut()
        .unwrap()
        .remove("activeItemId");
    value["terminalTabs"].as_array_mut().unwrap().push(other);
    assert!(WorkspaceSessionDocument::try_from_value(value).is_err());
}

/// 新构造的 typed 状态也经过同一 codec 门禁，不要求先有原始 JSON 才能持久化。
/// @author kongweiguang
#[test]
fn typed_file_workspace_writes_explicit_state_version_without_legacy_top_level_fields() {
    let source = WorkspaceSessionDocument::try_from_value(document_value()).unwrap();
    let snapshot = source.workspace_snapshot().unwrap();
    let mut empty = WorkspaceSessionDocument::empty_v3();
    empty.update_known_fields(&snapshot).unwrap();
    let row = &empty.as_value()["terminalTabs"][0];
    assert_eq!(row["stateVersion"], 1);
    assert!(row.get("path").is_none());
    assert!(row.get("access").is_none());
    assert_eq!(empty.workspace_snapshot().unwrap(), snapshot);
}

/// 测试同时覆盖三种可持久文件系统，避免协议来源校验依赖路径或 machine 标题猜测。
/// @author kongweiguang
fn file_targets() -> [(Value, &'static str, WorkspaceFileSource); 3] {
    [
        (
            json!({"kind":"local", "profileId":"shell", "futureTarget":true}),
            "local",
            WorkspaceFileSource::Local,
        ),
        (
            json!({"kind":"ssh", "hostId":"server", "futureTarget":true}),
            "sftp",
            WorkspaceFileSource::Sftp,
        ),
        (
            json!({"kind":"dockerContainer", "hostId":"server", "containerId":"container", "runtime":"podman", "futureTarget":true}),
            "container",
            WorkspaceFileSource::Container,
        ),
    ]
}

/// 旧单文件输入保留独立扩展，验证迁移前与新宿主共用来源语义。
/// @author kongweiguang
fn legacy_document_value(target: Value, source: &str) -> Value {
    let mut value = document_value();
    value["terminalTabs"][0] = json!({
        "id":"files", "kind":"workspaceFile", "title":"旧文档", "machineId":"host:server",
        "target":target, "path":"/etc/a", "access":"readonly", "source":source,
        "futureLegacy":{"keep":true}
    });
    value
}

/// 协议来源绑定真实目标；workspace 和 composeYaml 表达入口语义，应继续支持三种文件系统。
/// @author kongweiguang
#[test]
fn matching_sources_and_semantic_origins_round_trip_for_every_filesystem() {
    for (target, matching_source, _) in file_targets() {
        for source in [matching_source, "workspace", "composeYaml"] {
            let mut value = document_value();
            value["terminalTabs"][0]["target"] = target.clone();
            value["terminalTabs"][0]["state"]["items"][1]["source"] = json!(source);
            value["terminalTabs"][0]["state"]["items"][2]["source"] = json!("workspace");
            let mut document = WorkspaceSessionDocument::try_from_value(value).unwrap();
            let snapshot = document.workspace_snapshot().unwrap();
            document.update_known_fields(&snapshot).unwrap();
            let row = &document.as_value()["terminalTabs"][0];
            assert_eq!(row["target"], target);
            assert_eq!(row["state"]["items"][1]["source"], source);
            assert_eq!(row["state"]["items"][1]["futureDocument"]["keep"], "a");
            assert_eq!(row["state"]["futureState"], json!([1, 2]));

            let mut legacy = WorkspaceSessionDocument::try_from_value(legacy_document_value(
                target.clone(),
                source,
            ))
            .unwrap();
            let snapshot = legacy.workspace_snapshot().unwrap();
            legacy.update_known_fields(&snapshot).unwrap();
            assert_eq!(legacy.as_value()["terminalTabs"][0]["target"], target);
            assert_eq!(legacy.as_value()["terminalTabs"][0]["source"], source);
            assert_eq!(
                legacy.as_value()["terminalTabs"][0]["futureLegacy"]["keep"],
                true
            );
        }
    }
}

/// 输入、直接构造的快照与旧迁移输入都使用同一门禁；失败不能改变待保存原文。
/// @author kongweiguang
#[test]
fn mismatched_protocol_sources_are_rejected_before_read_or_write() {
    let protocol_sources = [
        ("local", WorkspaceFileSource::Local),
        ("sftp", WorkspaceFileSource::Sftp),
        ("container", WorkspaceFileSource::Container),
    ];
    for (target, matching_source, _) in file_targets() {
        for (wrong_source, wrong_typed_source) in protocol_sources {
            if wrong_source == matching_source {
                continue;
            }
            for index in [1, 2] {
                let mut value = document_value();
                value["terminalTabs"][0]["target"] = target.clone();
                value["terminalTabs"][0]["state"]["items"][1]["source"] = json!("workspace");
                value["terminalTabs"][0]["state"]["items"][2]["source"] = json!("workspace");
                value["terminalTabs"][0]["state"]["items"][index]["source"] = json!(wrong_source);
                assert_eq!(
                    WorkspaceSessionDocument::try_from_value(value.clone()).unwrap_err(),
                    WorkspaceSessionInvalidReason::InvalidSchema,
                    "目标 {target} 不应接受来源 {wrong_source}，内部项 {index}"
                );
                assert_eq!(
                    WorkspaceSessionDocument::try_from_value(legacy_document_value(
                        target.clone(),
                        wrong_source,
                    ))
                    .unwrap_err(),
                    WorkspaceSessionInvalidReason::InvalidSchema
                );

                value["terminalTabs"][0]["state"]["items"][index]["source"] =
                    json!(matching_source);
                let mut document = WorkspaceSessionDocument::try_from_value(value).unwrap();
                let original = document.as_value().clone();
                let mut snapshot = document.workspace_snapshot().unwrap();
                let WorkspaceSessionTab::FileWorkspace { state, .. } =
                    &mut snapshot.terminal_tabs[0]
                else {
                    panic!("文件 owner");
                };
                let FileWorkspaceItem::Document { source, .. } = &mut state.items[index] else {
                    panic!("文档项");
                };
                *source = wrong_typed_source;
                // 运行态结构门禁需要允许被投影排除的本机临时容器占位目标。
                assert!(snapshot.validate().is_ok());
                assert_eq!(
                    document.update_known_fields(&snapshot),
                    Err(WorkspaceSessionInvalidReason::InvalidSchema)
                );
                assert_eq!(document.as_value(), &original);
            }
        }
    }
}

/// 本机容器的真实 endpoint 仅在运行态持有，Local 占位不能误入持久化文档。
/// @author kongweiguang
#[test]
fn transient_local_container_placeholder_remains_runtime_only() {
    let mut value = document_value();
    value["terminalTabs"][0]["target"] = json!({"kind":"local"});
    value["terminalTabs"][0]["state"]["items"][1]["source"] = json!("local");
    value["terminalTabs"][0]["state"]["items"][2]["source"] = json!("workspace");
    let mut document = WorkspaceSessionDocument::try_from_value(value).unwrap();
    let mut snapshot = document.workspace_snapshot().unwrap();
    let WorkspaceSessionTab::FileWorkspace { state, .. } = &mut snapshot.terminal_tabs[0] else {
        panic!("文件 owner");
    };
    let FileWorkspaceItem::Document { source, .. } = &mut state.items[1] else {
        panic!("容器文档");
    };
    *source = WorkspaceFileSource::Container;
    assert!(snapshot.validate().is_ok());
    assert_eq!(
        document.update_known_fields(&snapshot),
        Err(WorkspaceSessionInvalidReason::InvalidSchema)
    );
}

/// 不可用 target 的未知原文和具体原因继续由既有恢复路径处理，不能被伪装为协议不匹配。
/// @author kongweiguang
#[test]
fn unavailable_target_keeps_its_existing_recovery_identity() {
    let mut value = document_value();
    let raw_target = json!({"kind":"futureProtocol", "opaque":{"keep":true}});
    value["terminalTabs"][0]["target"] = raw_target.clone();
    let mut document = WorkspaceSessionDocument::try_from_value(value).unwrap();
    let snapshot = document.workspace_snapshot().unwrap();
    assert!(matches!(
        &snapshot.terminal_tabs[0],
        WorkspaceSessionTab::FileWorkspace {
            target: WorkspaceTargetRef::Unavailable { .. },
            ..
        }
    ));
    document.update_known_fields(&snapshot).unwrap();
    assert_eq!(document.as_value()["terminalTabs"][0]["target"], raw_target);
}
