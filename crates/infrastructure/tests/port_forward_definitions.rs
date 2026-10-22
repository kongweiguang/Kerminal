//! 当前转发定义仓储的隔离持久化、CAS 与损坏数据保护回归。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    PublicErrorCode,
    operations::{PortForwardDefinition, PortForwardDefinitionRepository},
};
use kerminal_domain::{
    identity::HostId,
    operations::{ForwardEndpoint, ForwardKind, PortForwardConfig, ProxyApplyScope},
};
use kerminal_infrastructure::{
    operations::PortForwardDefinitionStore, storage::StorageCoordinator,
};

struct IsolatedRoot(PathBuf);

impl IsolatedRoot {
    /// 仅分配测试自有随机根，不读取或迁移真实用户配置。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-forward-definitions-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("create isolated root");
        Self(path)
    }

    /// 测试全部围绕单一当前文件路径，避免夹具不小心覆盖旧运行态样本。
    fn path(&self) -> PathBuf {
        self.0.join("data/port-forwards/definitions.json")
    }

    /// 每次打开新仓储模拟应用重启，不复用内存定义清单。
    fn store(&self) -> PortForwardDefinitionStore {
        PortForwardDefinitionStore::new(StorageCoordinator::new(self.0.clone()))
    }
}

impl Drop for IsolatedRoot {
    /// 只回收当前 fixture 明确创建的随机目录，不触碰用户根。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 定义中没有运行态字段，重开仓储仍返回同一 ID 与配置。
#[tokio::test]
async fn definitions_roundtrip_without_runtime_state() {
    let root = IsolatedRoot::new();
    let definition = definition();
    assert!(root.store().list().await.unwrap().is_empty());
    root.store().create(definition.clone()).await.unwrap();
    assert_eq!(root.store().list().await.unwrap(), vec![definition]);
    let value: serde_json::Value = serde_json::from_slice(&fs::read(root.path()).unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    assert!(value.get("schemaVersion").is_none());
    let record = value["definitions"][0].as_object().unwrap();
    assert_eq!(record.len(), 2);
    assert!(record.contains_key("id") && record.contains_key("config"));
}

/// 新建、编辑和删除使用不同前置条件，迟到表单不能覆盖或删除别人的新配置。
#[tokio::test]
async fn create_update_and_delete_require_exact_expected_definition() {
    let root = IsolatedRoot::new();
    let store = root.store();
    let initial = definition();
    store.create(initial.clone()).await.unwrap();
    let duplicate = store.create(initial.clone()).await.unwrap_err();
    assert_eq!(duplicate.public_code(), PublicErrorCode::Conflict);
    let mut updated = initial.clone();
    updated.config.name = "updated".to_owned();
    store
        .update(initial.clone(), updated.clone())
        .await
        .unwrap();
    let bytes = fs::read(root.path()).unwrap();
    assert_eq!(
        store
            .update(initial.clone(), initial.clone())
            .await
            .unwrap_err()
            .public_code(),
        PublicErrorCode::Conflict,
    );
    assert_eq!(
        store.delete(initial).await.unwrap_err().public_code(),
        PublicErrorCode::Conflict,
    );
    assert_eq!(fs::read(root.path()).unwrap(), bytes);
    assert!(store.delete(updated.clone()).await.unwrap());
    assert!(!store.delete(updated).await.unwrap());
    assert!(store.list().await.unwrap().is_empty());
}

/// 损坏、未来 schema、嵌套秘密和重复 ID 都必须阻断读取与写入并逐字保留原文。
#[tokio::test]
async fn invalid_documents_never_become_empty_or_get_overwritten() {
    let root = IsolatedRoot::new();
    let definition = definition();
    let record = serde_json::to_value(&definition).unwrap();
    let mut secret_record = record.clone();
    secret_record["config"]["password"] = serde_json::json!("not-a-real-secret");
    let cases = vec![
        b"not-json".to_vec(),
        serde_json::to_vec(&serde_json::json!({"schema_version":2,"definitions":[]})).unwrap(),
        serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"definitions":[record.clone(),record]}),
        )
        .unwrap(),
        serde_json::to_vec(&serde_json::json!({"schema_version":1,"definitions":[secret_record]}))
            .unwrap(),
    ];
    fs::create_dir_all(root.path().parent().unwrap()).unwrap();
    for bytes in cases {
        fs::write(root.path(), &bytes).unwrap();
        let store = root.store();
        assert!(store.list().await.is_err());
        assert!(store.create(definition.clone()).await.is_err());
        assert_eq!(fs::read(root.path()).unwrap(), bytes);
    }
}

/// 端点与身份全部使用隔离常量，无凭据或外部 listener。
fn definition() -> PortForwardDefinition {
    PortForwardDefinition {
        id: "saved-forward".to_owned(),
        config: PortForwardConfig {
            host_id: HostId::new(),
            name: "fixture".to_owned(),
            kind: ForwardKind::Local,
            bind: ForwardEndpoint::new("127.0.0.1", 55001).unwrap(),
            destination: Some(ForwardEndpoint::new("127.0.0.1", 5432).unwrap()),
            proxy_protocol: None,
            remote_access_scope: None,
            proxy_apply_scope: ProxyApplyScope::None,
        },
    }
}
