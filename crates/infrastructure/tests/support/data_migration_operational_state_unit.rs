//! 转发稳定定义转换与当前恢复记录的只读预检边界。
//!
//! @author kongweiguang

use std::fs;

use serde_json::json;

use super::*;

/// 按旧正式 DTO 构造完整摘要，包含运行信息以验证这些字段不会进入当前定义。
fn legacy_summary(id: &str) -> Value {
    json!({
        "id": id,
        "hostId": "host-a",
        "hostName": "Test host",
        "name": "Database",
        "kind": "local",
        "origin": "user",
        "bindHost": "127.0.0.1",
        "localBindHost": "127.0.0.1",
        "sourcePort": 15432,
        "targetHost": "127.0.0.1",
        "targetPort": 5432,
        "pid": 98765,
        "status": "running",
        "createdAt": "123",
        "runtime": {
            "mode": "managedSshRuntime",
            "backend": "native-russh",
            "managedSessionId": "obsolete-session",
            "managedTunnelId": "obsolete-tunnel",
            "cleanupStatus": "active"
        }
    })
}

/// 只写测试隔离根中的固定路径，不借预检调用创建正式数据文件。
fn write_legacy(root: &Path, value: Value) -> Vec<u8> {
    let bytes = serde_json::to_vec(&value).expect("旧样例可编码");
    let path = root.join(LEGACY_FORWARDS);
    fs::create_dir_all(path.parent().expect("摘要有父目录")).expect("创建样例目录");
    fs::write(path, &bytes).expect("写入旧摘要");
    bytes
}

/// 迁移计划只有稳定定义和旧源原文哨兵，运行 PID/句柄不保存在新文档也不触发任何启动。
#[test]
fn legacy_running_forward_becomes_a_stable_definition_with_original_source_cas() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let original = write_legacy(
        root.path(),
        json!({
            "schema_version": 1,
            "sessions": [legacy_summary("forward-a")]
        }),
    );
    let edits = plan_operational_state_migration(root.path()).expect("已知旧摘要可规划");
    assert_eq!(edits.len(), 2);
    let source = edits
        .iter()
        .find(|edit| edit.relative_path == Path::new(LEGACY_FORWARDS))
        .expect("旧源加入 CAS/备份计划");
    assert_eq!(source.before.as_deref(), Some(original.as_slice()));
    assert_eq!(source.after, original);
    let current = edits
        .iter()
        .find(|edit| edit.relative_path == Path::new(CURRENT_FORWARDS))
        .expect("生成当前定义");
    let value: Value = serde_json::from_slice(&current.after).expect("当前定义可读");
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["definitions"][0]["id"], "forward-a");
    assert_eq!(value["definitions"][0]["config"]["hostId"], "host-a");
    assert_eq!(value["definitions"][0]["config"]["bind"]["port"], 15432);
    for field in ["pid", "status", "runtime", "actualBind", "createdAt"] {
        assert!(value["definitions"][0].get(field).is_none());
        assert!(value["definitions"][0]["config"].get(field).is_none());
    }
    assert_eq!(
        fs::read(root.path().join(LEGACY_FORWARDS)).expect("预检不改源文件"),
        source.after
    );
    assert!(!root.path().join(CURRENT_FORWARDS).exists());
}

/// 旧版已经明确支持的 remote+SOCKS 无目的地语义转换为显式类型，HTTP 不被降级成 SOCKS。
#[test]
fn legacy_remote_dynamic_is_explicit_and_http_requires_manual_recovery() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let mut summary = legacy_summary("forward-a");
    summary["kind"] = json!("remote");
    summary["proxyProtocol"] = json!("socks5");
    summary["targetHost"] = Value::Null;
    summary["targetPort"] = Value::Null;
    write_legacy(
        root.path(),
        json!({"schema_version":1,"sessions":[summary.clone()]}),
    );
    let edits = plan_operational_state_migration(root.path()).expect("已知 remote SOCKS 可转换");
    let value: Value = serde_json::from_slice(
        &edits
            .iter()
            .find(|edit| edit.relative_path == Path::new(CURRENT_FORWARDS))
            .expect("存在当前定义")
            .after,
    )
    .expect("当前定义可解码");
    assert_eq!(value["definitions"][0]["config"]["kind"], "remoteDynamic");
    summary["proxyProtocol"] = json!("http");
    let original = write_legacy(
        root.path(),
        json!({"schema_version":1,"sessions":[summary]}),
    );
    assert!(plan_operational_state_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join(LEGACY_FORWARDS)).expect("旧 HTTP 记录保留"),
        original
    );
}

/// 不合并两个各自有效但不同的定义集，重复 ID、未来版本与未知字段也不能被覆盖式修复。
#[test]
fn conflicting_or_future_forward_documents_remain_untouched() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let original = write_legacy(
        root.path(),
        json!({
            "schema_version":1,"sessions":[legacy_summary("forward-a")]
        }),
    );
    let edits = plan_operational_state_migration(root.path()).expect("生成对照当前定义");
    let current = edits
        .iter()
        .find(|edit| edit.relative_path == Path::new(CURRENT_FORWARDS))
        .expect("当前定义存在")
        .after
        .clone();
    let mut value: Value = serde_json::from_slice(&current).expect("当前定义可读");
    value["definitions"][0]["config"]["name"] = json!("User changed name");
    let conflict = serde_json::to_vec(&value).expect("冲突定义可编码");
    fs::write(root.path().join(CURRENT_FORWARDS), &conflict).expect("保存外部定义");
    assert!(plan_operational_state_migration(root.path()).is_err());
    assert_eq!(
        fs::read(root.path().join(CURRENT_FORWARDS)).expect("外部定义保留"),
        conflict
    );
    assert_eq!(
        fs::read(root.path().join(LEGACY_FORWARDS)).expect("旧源保留"),
        original
    );
    value["schema_version"] = json!(2);
    assert!(
        validate_forward_definitions(&serde_json::to_vec(&value).expect("未来定义可编码")).is_err()
    );
    value["schema_version"] = json!(1);
    value["definitions"][0]["config"]["futureOption"] = json!(true);
    assert!(
        validate_forward_definitions(&serde_json::to_vec(&value).expect("未来字段可编码")).is_err()
    );
    value["definitions"][0]["config"]
        .as_object_mut()
        .expect("配置对象")
        .remove("futureOption");
    let duplicate = value["definitions"][0].clone();
    value["definitions"]
        .as_array_mut()
        .expect("定义数组")
        .push(duplicate);
    assert!(
        validate_forward_definitions(&serde_json::to_vec(&value).expect("重复定义可编码")).is_err()
    );
}

/// 缺少恢复文件是首次使用，未来或损坏的当前恢复文件则精确指向固定路径并保留内容。
#[test]
fn recovery_preflight_validates_current_format_without_inventing_old_tasks() {
    let root = tempfile::tempdir().expect("创建隔离根");
    validate_current_operational_state(root.path()).expect("缺失当前文件视为空");
    fs::create_dir(root.path().join("data")).expect("创建恢复目录");
    let original = b"{\"schemaVersion\":2,\"records\":[]}";
    fs::write(root.path().join(TRANSFER_RECOVERY), original).expect("写入未来恢复文件");
    assert_eq!(
        validate_current_operational_state(root.path()).expect_err("未来记录必须阻断"),
        DataMigrationBlocker::ManagedDataInvalid {
            relative_path: TRANSFER_RECOVERY.to_owned()
        },
    );
    assert_eq!(
        fs::read(root.path().join(TRANSFER_RECOVERY)).expect("原恢复文件保留"),
        original
    );
}
