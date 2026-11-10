//! 显式文件工作区迁移的聚合、保真、只读和拒绝边界。
//!
//! @author kongweiguang

use super::{plan_workspace_migration, validate_current_workspace_bytes};
use crate::data_migration::FileDataMigrationRepository;
use kerminal_application::{
    AppError,
    data_migration::{DataMigrationBlocker, DataMigrationReadiness, DataMigrationRepository},
};
use serde_json::{Value, json};
use std::{collections::BTreeMap, fs};

/// 只使用人工描述，不访问真实文件系统或连接，文件系统身份由持久 target 决定。
/// @author kongweiguang
fn file(id: &str, host: &str, path: &str) -> Value {
    json!({"id":id,"kind":"workspaceFile","title":path,"machineId":format!("host:{host}"),
        "target":{"kind":"ssh","hostId":host},"path":path,"access":"editable","source":"sftp"})
}

/// 组本身及未知数据保持原样，迁移只改变旧文件 owner 的必要字段。
/// @author kongweiguang
fn source(tabs: Vec<Value>, active: &str) -> Value {
    json!({"version":3,"sidebarMachines":[],"terminalPanes":[],"terminalTabs":tabs,
        "activeTabId":active,"focusedPaneId":"","futureRoot":{"keep":true},
        "terminalTabGroups":{"one":{"title":"一","futureGroup":1},"two":{"title":"二"}}})
}

/// 复用真实 planner 获取 after，后续断言不绕过 codec 或 CAS 编辑生成过程。
/// @author kongweiguang
fn planned(value: Value) -> Value {
    let before = serde_json::to_vec(&value).unwrap();
    let edit = plan_workspace_migration(Some(before.clone()), &BTreeMap::new())
        .unwrap()
        .unwrap();
    assert_eq!(edit.before, Some(before));
    assert_eq!(edit.relative_path.to_str(), Some("workspace/session.json"));
    serde_json::from_slice(&edit.after).unwrap()
}

/// 首位置、活动 owner 身份和分组分别遵守原顺序与原活动项，不按主机排序重排顶层。
/// @author kongweiguang
#[test]
fn aggregates_by_filesystem_with_active_identity_group_and_original_internal_order() {
    let mut first = file("a", "server", "/etc/a");
    first["tabGroupId"] = json!("one");
    first["title"] = json!("自定义 A");
    first["rootPath"] = json!("/etc");
    first["futureFile"] = json!({"keep":"a"});
    let other = file("other", "another", "/else/other");
    let mut active = file("b", "server", "/var/log/b");
    active["tabGroupId"] = json!("two");
    active["access"] = json!("readonly");
    active["source"] = json!("composeYaml");
    active["futureFile"] = json!({"keep":"b"});
    let after = planned(source(vec![first.clone(), other, active.clone()], "b"));
    let tabs = after["terminalTabs"].as_array().unwrap();
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0]["id"], "b");
    assert_eq!(tabs[0]["tabGroupId"], "two");
    assert_eq!(tabs[1]["id"], "other");
    assert_eq!(after["activeTabId"], "b");
    assert_eq!(tabs[0]["kind"], "fileWorkspace");
    assert_eq!(tabs[0]["title"], "server · 文件");
    assert_eq!(tabs[0]["stateVersion"], 1);
    let state = &tabs[0]["state"];
    assert_eq!(state["items"][0]["kind"], "directory");
    assert_eq!(state["items"][0]["path"], "/var/log");
    assert_eq!(state["items"][1]["id"], "a");
    assert_eq!(state["items"][2]["id"], "b");
    assert_eq!(state["items"][1]["title"], "自定义 A");
    assert_eq!(state["items"][1]["rootPath"], "/etc");
    assert_eq!(state["items"][1]["futureFile"], first["futureFile"]);
    assert_eq!(state["items"][2]["futureFile"], active["futureFile"]);
    assert_eq!(state["items"][2]["source"], "composeYaml");
    assert_eq!(state["items"][2]["access"], "readonly");
    assert_eq!(state["sourceTabIds"], json!(["a", "b"]));
    assert_eq!(state["activeItemId"], "b");
    assert_eq!(after["futureRoot"]["keep"], true);
    assert_eq!(after["terminalTabGroups"]["one"]["futureGroup"], 1);
    let bytes = serde_json::to_vec(&after).unwrap();
    assert!(validate_current_workspace_bytes(&bytes).is_ok());
    assert!(
        plan_workspace_migration(Some(bytes), &BTreeMap::new())
            .unwrap()
            .is_none()
    );
}

/// 无组内活动项时保留首成员的宿主与分组，内部默认活动文档也来自首成员。
/// @author kongweiguang
#[test]
fn inactive_filesystem_uses_first_member_without_stealing_outer_selection() {
    let mut first = file("a", "server", "/a/file");
    first["tabGroupId"] = json!("one");
    let mut second = file("b", "server", "/b/file");
    second["tabGroupId"] = json!("two");
    let after = planned(source(
        vec![first, file("other", "other", "/other/file"), second],
        "other",
    ));
    assert_eq!(after["activeTabId"], "other");
    assert_eq!(after["terminalTabs"][0]["id"], "a");
    assert_eq!(after["terminalTabs"][0]["tabGroupId"], "one");
    assert_eq!(after["terminalTabs"][0]["state"]["activeItemId"], "a");
    assert_eq!(after["terminalTabs"][0]["state"]["items"][0]["path"], "/a");
}

/// 相容重复项收紧为只读并保留全部来源别名，活动引用改指向保留的内部项。
/// @author kongweiguang
#[test]
fn compatible_duplicates_merge_without_relaxing_readonly_or_losing_source_aliases() {
    let first = file("a", "server", "/etc/file");
    let mut duplicate = file("b", "server", "/etc/file");
    duplicate["access"] = json!("readonly");
    let after = planned(source(vec![first, duplicate], "b"));
    let state = &after["terminalTabs"][0]["state"];
    assert_eq!(after["terminalTabs"][0]["id"], "b");
    assert_eq!(state["items"].as_array().unwrap().len(), 2);
    assert_eq!(state["items"][1]["id"], "a");
    assert_eq!(state["items"][1]["access"], "readonly");
    assert_eq!(state["activeItemId"], "a");
    assert_eq!(state["sourceTabIds"], json!(["a", "b"]));
}

/// 不相容元数据保留原文并阻断转换，不能借保守副本制造两个同端点文档。
/// @author kongweiguang
#[test]
fn incompatible_duplicate_metadata_is_preserved_conservatively() {
    let first = file("a", "server", "/etc/file");
    let mut duplicate = file("b", "server", "/etc/file");
    duplicate["title"] = json!("自定义只读");
    duplicate["source"] = json!("composeYaml");
    duplicate["access"] = json!("readonly");
    duplicate["future"] = json!({"keep":2});
    let value = source(vec![first, duplicate], "b");
    let original = serde_json::to_vec(&value).unwrap();
    assert!(plan_workspace_migration(Some(original.clone()), &BTreeMap::new()).is_err());
    assert_eq!(original, serde_json::to_vec(&value).unwrap());
}

/// Local 忽略 profile 身份；容器必须按宿主、运行时与容器 ID 隔离，不能混为宿主文件。
/// @author kongweiguang
#[test]
fn local_profiles_share_filesystem_and_containers_remain_isolated() {
    let mut local_a = file("a", "unused", "relative-a");
    local_a["target"] = json!({"kind":"local","profileId":"profile-a"});
    local_a["source"] = json!("local");
    let mut local_b = file("b", "unused", "relative-b");
    local_b["target"] = json!({"kind":"local","profileId":"profile-b"});
    local_b["source"] = json!("local");
    let mut docker = file("docker", "server", "/app/file");
    docker["target"] =
        json!({"kind":"dockerContainer","hostId":"server","containerId":"c","runtime":"docker"});
    docker["source"] = json!("container");
    let mut podman = docker.clone();
    podman["id"] = json!("podman");
    podman["target"]["runtime"] = json!("podman");
    let mut other_host = docker.clone();
    other_host["id"] = json!("host");
    other_host["target"]["hostId"] = json!("other");
    let mut other_container = docker.clone();
    other_container["id"] = json!("container");
    other_container["target"]["containerId"] = json!("other");
    let after = planned(source(
        vec![
            local_a,
            docker,
            local_b,
            podman,
            other_host,
            other_container,
        ],
        "b",
    ));
    assert_eq!(after["terminalTabs"].as_array().unwrap().len(), 5);
    assert_eq!(
        after["terminalTabs"][0]["state"]["sourceTabIds"],
        json!(["a", "b"])
    );
    assert_eq!(after["terminalTabs"][0]["state"]["items"][0]["path"], ".");
}

/// 不确定目标和未来内部版本进入恢复状态；不回退本机、不消费或改写输入字节。
/// @author kongweiguang
#[test]
fn unknown_targets_future_versions_and_current_marker_with_legacy_tabs_are_blocked() {
    let legacy = source(vec![file("a", "server", "/a")], "a");
    let original = serde_json::to_vec(&legacy).unwrap();
    assert!(validate_current_workspace_bytes(&original).is_err());
    for target in [
        json!({"kind":"future","hostId":"server"}),
        json!({"kind":"local","profileId":42}),
        json!({"kind":"telnet","hostId":"server"}),
    ] {
        let mut value = legacy.clone();
        value["terminalTabs"][0]["target"] = target;
        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(plan_workspace_migration(Some(bytes.clone()), &BTreeMap::new()).is_err());
        assert_eq!(bytes, serde_json::to_vec(&value).unwrap());
    }
    let mut future = planned(legacy);
    future["terminalTabs"][0]["stateVersion"] = json!(2);
    assert!(
        plan_workspace_migration(Some(serde_json::to_vec(&future).unwrap()), &BTreeMap::new())
            .is_err()
    );
    assert_eq!(
        original,
        serde_json::to_vec(&source(vec![file("a", "server", "/a")], "a")).unwrap()
    );
}

/// 旧标签和当前宿主的协议来源不一致时均须阻断预检与迁移，并保留原始文件及格式标记。
/// @author kongweiguang
#[test]
fn mismatched_source_blocks_legacy_and_current_migration_without_changing_data() {
    let cases = [
        (json!({"kind":"local"}), "local", "sftp"),
        (json!({"kind":"local"}), "local", "container"),
        (json!({"kind":"ssh","hostId":"server"}), "sftp", "local"),
        (json!({"kind":"ssh","hostId":"server"}), "sftp", "container"),
        (
            json!({"kind":"dockerContainer","hostId":"server","containerId":"c","runtime":"podman"}),
            "container",
            "local",
        ),
        (
            json!({"kind":"dockerContainer","hostId":"server","containerId":"c","runtime":"podman"}),
            "container",
            "sftp",
        ),
    ];
    for (target, matching_source, wrong_source) in cases {
        let mut first = file("a", "server", "/etc/a");
        first["target"] = target.clone();
        first["source"] = json!(matching_source);
        let mut second = file("b", "server", "/etc/b");
        second["target"] = target.clone();
        second["source"] = json!(matching_source);
        second["futureFile"] = json!({"keep":true});
        let mut legacy = source(vec![first, second], "a");
        let mut current = planned(legacy.clone());
        legacy["terminalTabs"][1]["source"] = json!(wrong_source);
        current["terminalTabs"][0]["state"]["items"][2]["source"] = json!(wrong_source);

        for (contract, value) in [(3, legacy), (4, current)] {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(matches!(
                plan_workspace_migration(Some(bytes.clone()), &BTreeMap::new()),
                Err(AppError::ConfigurationCorrupt { .. })
            ));
            if contract == 4 {
                assert!(matches!(
                    validate_current_workspace_bytes(&bytes),
                    Err(AppError::ConfigurationCorrupt { .. })
                ));
            }

            let root = tempfile::tempdir().unwrap();
            let session_path = root.path().join("workspace/session.json");
            let marker_path = root.path().join(".kerminal-format.toml");
            fs::create_dir_all(session_path.parent().unwrap()).unwrap();
            fs::write(&session_path, &bytes).unwrap();
            let marker = format!("data_format = 4\nmigration_contract = {contract}\n");
            fs::write(&marker_path, marker.as_bytes()).unwrap();
            let repository = FileDataMigrationRepository::new(root.path());
            let inspection = repository.inspect().unwrap();
            assert_eq!(inspection.readiness, DataMigrationReadiness::Blocked);
            assert!(
                inspection
                    .blockers
                    .contains(&DataMigrationBlocker::ManagedDataInvalid {
                        relative_path: "workspace/session.json".to_owned(),
                    })
            );
            assert!(matches!(
                repository.migrate(),
                Err(AppError::Conflict { .. })
            ));
            assert_eq!(fs::read(session_path).unwrap(), bytes);
            assert_eq!(fs::read(marker_path).unwrap(), marker.into_bytes());
            assert!(!root.path().join(".kerminal-migration").exists());
            assert!(!root.path().join(".storage.lock").exists());
        }
    }
}
