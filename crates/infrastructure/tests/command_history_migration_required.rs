//! 旧命令数据库必须进入独立迁移流程，普通服务不能隐式改写。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    AppError,
    commands::{
        CommandHistoryAvailability, CommandHistoryRepository, CommandHistoryUnavailableReason,
        SnippetPreferenceAvailability, SnippetPreferenceRepository,
        SnippetPreferenceUnavailableReason,
    },
};
use kerminal_domain::commands::CommandHistoryListRequest;
use kerminal_infrastructure::{
    command_history::SqliteCommandHistoryRepository, storage::StorageCoordinator,
};
use rusqlite::Connection;
use uuid::Uuid;

const LEGACY_V1_SCHEMA: &str = include_str!("../../../tests/fixtures/command_history/v1.sql");

struct TestRoot(PathBuf);

impl TestRoot {
    /// 为旧库状态回归创建隔离目录，避免探测或改写正式配置。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-history-migration-required-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create isolated migration test root");
        Self(path)
    }
}

impl Drop for TestRoot {
    /// 仅清理本测试随机建立的数据库及 sidecar。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 旧 schema 保持原字节并以 MigrationRequired 穿过历史与偏好端口，不得伪报损坏。
#[tokio::test]
async fn legacy_schema_requires_explicit_migration_without_corrupt_mapping() {
    let root = TestRoot::new();
    let database_file = root.0.join("data/command.sqlite");
    fs::create_dir_all(database_file.parent().expect("database parent"))
        .expect("create isolated database parent");
    let connection = Connection::open(&database_file).expect("create legacy database");
    connection
        .execute_batch(LEGACY_V1_SCHEMA)
        .expect("create frozen legacy schema version 1");
    drop(connection);
    let before = fs::read(&database_file).expect("read legacy bytes before service startup");

    let repository = SqliteCommandHistoryRepository::open_or_unavailable(StorageCoordinator::new(
        root.0.clone(),
    ));
    assert_eq!(
        CommandHistoryRepository::availability(&repository),
        CommandHistoryAvailability::Unavailable {
            reason: CommandHistoryUnavailableReason::MigrationRequired(1),
        }
    );
    assert_eq!(
        SnippetPreferenceRepository::availability(&repository),
        SnippetPreferenceAvailability::Unavailable {
            reason: SnippetPreferenceUnavailableReason::MigrationRequired(1),
        }
    );

    let error = CommandHistoryRepository::list(&repository, CommandHistoryListRequest::default())
        .await
        .expect_err("old database must not appear as empty history");
    assert!(matches!(&error, AppError::Infrastructure { .. }));
    assert!(!matches!(&error, AppError::ConfigurationCorrupt { .. }));
    assert_eq!(
        fs::read(&database_file).expect("read legacy bytes after normal startup"),
        before,
        "ordinary startup must leave migration input untouched"
    );
}
