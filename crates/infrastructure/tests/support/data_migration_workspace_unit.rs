//! 工作区缺省字段迁移的输入、保真和拒绝边界。
//!
//! @author kongweiguang

use serde_json::{Value, json};

use super::plan_workspace_migration;

/// fixture 有完整布局引用，测试只改变迁移字段并保持其余 schema 有效。
///
/// @author kongweiguang
fn current_fixture() -> Value {
    serde_json::from_str(include_str!(
        "../../../../tests/fixtures/workspace_session_v3.json"
    ))
    .expect("workspace fixture should be valid JSON")
}

/// 已知 v3 缺失/null 字段只进入 CAS 计划，未知扩展与显式 local scope 保持不变。
///
/// @author kongweiguang
#[test]
fn missing_and_null_fields_are_planned_without_mutating_source() {
    let mut value = current_fixture();
    let panes = value["terminalPanes"]
        .as_array_mut()
        .expect("fixture panes are an array");
    let first = panes[0].as_object_mut().expect("pane is an object");
    first.remove("prompt");
    first.remove("status");
    panes[1]["prompt"] = Value::Null;
    panes[1]["status"] = Value::Null;
    panes[0]["migrationExtension"] = json!({"keep": [1, 2]});
    let before = serde_json::to_vec(&value).expect("encode source fixture");

    let edit = plan_workspace_migration(Some(before.clone()))
        .expect("known missing fields are migratable")
        .expect("migration should produce one edit");
    assert_eq!(edit.relative_path.to_str(), Some("workspace/session.json"));
    assert_eq!(edit.before.as_deref(), Some(before.as_slice()));
    assert_ne!(edit.after, before);
    let migrated: Value = serde_json::from_slice(&edit.after).expect("planned JSON");
    assert_eq!(migrated["terminalPanes"][0]["prompt"], json!("PS>"));
    assert_eq!(migrated["terminalPanes"][0]["status"], json!("offline"));
    assert_eq!(migrated["terminalPanes"][1]["prompt"], json!("PS>"));
    assert_eq!(migrated["terminalPanes"][1]["status"], json!("offline"));
    assert_eq!(
        migrated["terminalPanes"][0]["migrationExtension"],
        json!({"keep": [1, 2]})
    );
    assert!(
        migrated["terminalPanes"][0]
            .get("localMachineScope")
            .is_none()
    );
    assert_eq!(value["terminalPanes"][0].get("prompt"), None);
}

/// 当前显式值不应产生重写，空 prompt 及 known statuses 属于有效当前语义。
///
/// @author kongweiguang
#[test]
fn current_explicit_values_need_no_migration() {
    let mut value = current_fixture();
    value["terminalPanes"][0]["prompt"] = json!("");
    value["terminalPanes"][0]["status"] = json!("online");
    value["terminalPanes"][1]["status"] = json!("warning");
    let before = serde_json::to_vec(&value).expect("encode source fixture");

    assert!(
        plan_workspace_migration(Some(before))
            .expect("current document validates")
            .is_none()
    );
    assert!(
        plan_workspace_migration(None)
            .expect("missing workspace is a no-op")
            .is_none()
    );
}

/// 未知状态、错误字段类型、旧版本和损坏 JSON 都阻断，绝不猜测为 offline。
///
/// @author kongweiguang
#[test]
fn unknown_or_invalid_workspace_values_are_not_normalized() {
    let mut unknown_status = current_fixture();
    unknown_status["terminalPanes"][0]["status"] = json!("connected-ish");
    unknown_status["terminalPanes"][1]
        .as_object_mut()
        .unwrap()
        .remove("prompt");
    let source = serde_json::to_vec(&unknown_status).expect("encode invalid fixture");
    assert!(plan_workspace_migration(Some(source.clone())).is_err());

    for field in ["prompt", "status"] {
        let mut invalid = current_fixture();
        invalid["terminalPanes"][0][field] = json!(42);
        assert!(
            plan_workspace_migration(Some(
                serde_json::to_vec(&invalid).expect("encode invalid fixture")
            ))
            .is_err()
        );
    }

    for version in [1, 2, 4] {
        let mut unsupported = current_fixture();
        unsupported["version"] = json!(version);
        assert!(
            plan_workspace_migration(Some(
                serde_json::to_vec(&unsupported).expect("encode version fixture")
            ))
            .is_err()
        );
    }
    let mut unversioned = current_fixture();
    unversioned.as_object_mut().unwrap().remove("version");
    assert!(
        plan_workspace_migration(Some(
            serde_json::to_vec(&unversioned).expect("encode unversioned fixture")
        ))
        .is_err()
    );
    assert!(plan_workspace_migration(Some(b"{broken".to_vec())).is_err());
    assert_eq!(source, serde_json::to_vec(&unknown_status).unwrap());
}
