//! 验证 workspace v3 缺省字段迁移经过真实 journal、备份和当前读取适配器闭环。
//!
//! @author kongweiguang

use std::{fs, sync::Arc};

use kerminal_application::{
    data_migration::{DataMigrationOutcome, DataMigrationReadiness, DataMigrationRepository},
    workspace::{WorkspaceSessionLoad, WorkspaceSessionService},
};
use kerminal_infrastructure::{
    data_migration::FileDataMigrationRepository, storage::StorageCoordinator,
    workspace::WorkspaceSessionStore,
};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const CURRENT_WORKSPACE_FIXTURE: &str =
    include_str!("../../../tests/fixtures/workspace_session_v3.json");

/// 从完整当前 fixture 生成仅缺少已知旧 v3 pane 字段的原始输入，避免测试越过真实迁移入口。
fn workspace_bytes_missing_legacy_defaults() -> Vec<u8> {
    let mut value: Value = serde_json::from_str(CURRENT_WORKSPACE_FIXTURE)
        .expect("共享 workspace fixture 应是有效 JSON");
    let panes = value["terminalPanes"]
        .as_array_mut()
        .expect("共享 workspace fixture 应含 pane 数组");
    for pane in panes {
        let object = pane.as_object_mut().expect("共享 workspace pane 应是对象");
        assert_eq!(object.remove("prompt"), Some(json!("PS>")));
        assert_eq!(object.remove("status"), Some(json!("offline")));
    }
    serde_json::to_vec(&value).expect("迁移 fixture 应能编码为 JSON")
}

/// 通过公开 migration repository 迁移、检查 marker 和原始备份，再经当前 workspace service 读回。
#[tokio::test]
async fn workspace_migration_commits_journal_and_reads_back_current_document() {
    let root = tempfile::tempdir().expect("创建隔离数据根");
    let session_path = root.path().join("workspace/session.json");
    fs::create_dir_all(session_path.parent().expect("session 有父目录"))
        .expect("创建隔离 workspace 目录");
    let original = workspace_bytes_missing_legacy_defaults();
    fs::write(&session_path, &original).expect("写入人工构造的旧 v3 session");

    let migration = FileDataMigrationRepository::new(root.path());
    let inspection = migration.inspect().expect("只读识别已知旧 v3 session");
    assert_eq!(inspection.readiness, DataMigrationReadiness::Legacy);
    assert!(inspection.blockers.is_empty());

    let report = migration.migrate().expect("通过文件 journal 提交迁移");
    assert_eq!(report.outcome, DataMigrationOutcome::Migrated);
    assert_ne!(
        fs::read(&session_path).expect("读取迁移后 session"),
        original
    );
    assert_eq!(
        fs::read(root.path().join(".kerminal-format.toml")).expect("迁移应提交当前格式 marker"),
        b"data_format = 4\nmigration_contract = 2\n"
    );
    assert_eq!(
        migration
            .inspect()
            .expect("marker 与 journal 应识别为当前格式")
            .readiness,
        DataMigrationReadiness::Current
    );

    let backup_relative = report
        .backup_relative_path
        .expect("迁移报告应保留恢复目录相对路径");
    let backup = root
        .path()
        .join(backup_relative)
        .join("before/managed/workspace/session.json");
    assert_eq!(
        fs::read(backup).expect("journal 备份应保留迁移前原始字节"),
        original
    );

    let workspace = WorkspaceSessionService::new(Arc::new(WorkspaceSessionStore::new(
        StorageCoordinator::new(root.path()),
        true,
    )));
    let loaded = workspace
        .load(CancellationToken::new())
        .await
        .expect("当前 WorkspaceSessionService 应可读回迁移结果");
    let WorkspaceSessionLoad::Loaded { document, .. } = loaded else {
        panic!("迁移后 session 必须被当前 workspace codec 接受");
    };
    let migrated_value = document.as_value();
    for pane_id in ["pane-a", "pane-b"] {
        let pane = migrated_value["terminalPanes"]
            .as_array()
            .and_then(|panes| {
                panes
                    .iter()
                    .find(|pane| pane.get("id").and_then(Value::as_str) == Some(pane_id))
            })
            .unwrap_or_else(|| panic!("迁移后应保留 pane {pane_id}"));
        assert_eq!(pane["prompt"], json!("PS>"));
        assert_eq!(pane["status"], json!("offline"));
    }
    assert_eq!(migrated_value["futureRoot"]["keep"], json!(true));
    assert_eq!(
        migrated_value["terminalPanes"][0]["futurePane"]["value"],
        json!(true)
    );
}
