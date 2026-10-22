//! Data migration DTO serialization contract.
//!
//! @author kongweiguang

use crate::data_migration::DataMigrationBlocker;

#[test]
/// 固定安全阻断码的字段序列化，避免桌面 API 把 snake_case 泄漏成另一份契约。
fn blocker_fields_serialize_as_camel_case() {
    let blocker = DataMigrationBlocker::UnsupportedCommandHistorySchema { schema_version: 2 };
    let value = serde_json::to_value(blocker).expect("blocker DTO must serialize");
    assert_eq!(
        value,
        serde_json::json!({
            "kind": "unsupportedCommandHistorySchema",
            "schemaVersion": 2
        })
    );
}

#[test]
/// 限定旧恢复材料提示只带受控相对路径，不序列化本机绝对根路径。
fn blocker_paths_serialize_as_camel_case() {
    let blocker = DataMigrationBlocker::PendingLegacyTransaction {
        relative_path: ".storage-transactions/profile-default.json".to_owned(),
    };
    let value = serde_json::to_value(blocker).expect("blocker DTO must serialize");
    assert_eq!(
        value,
        serde_json::json!({
            "kind": "pendingLegacyTransaction",
            "relativePath": ".storage-transactions/profile-default.json"
        })
    );
}
