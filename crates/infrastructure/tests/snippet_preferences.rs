//! command.sqlite v3 片段收藏、幂等回执与清理契约。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use kerminal_application::{
    AppError,
    commands::{
        SnippetPreferenceAvailability, SnippetPreferenceRepository,
        SnippetPreferenceUnavailableReason,
    },
};
use kerminal_domain::commands::{
    SnippetCatalogOrigin, SnippetPreferenceKey, SnippetUsageAction, SnippetUsageRecordRequest,
};
use kerminal_infrastructure::{
    command_history::SqliteCommandHistoryRepository, storage::StorageCoordinator,
};

struct TemporaryDatabase(PathBuf);

impl TemporaryDatabase {
    /// 每项测试使用独立 command.sqlite，避免读取或改写用户真实配置。
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "kerminal-snippet-preferences-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("创建隔离配置目录");
        Self(root)
    }

    /// 初始化真实 v3 SQLite adapter，供集成测试验证 SQL 和 application port。
    fn repository(&self) -> SqliteCommandHistoryRepository {
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(self.0.clone()))
            .expect("初始化隔离 command.sqlite")
    }
}

impl Drop for TemporaryDatabase {
    /// 仅删除当前测试创建的临时根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 真实回执按来源和 ID 幂等累计，复用 receipt 不会多加一次用量。
#[tokio::test]
async fn usage_receipt_is_idempotent_and_origin_scoped() {
    let temporary = TemporaryDatabase::new();
    let repository = temporary.repository();
    assert_eq!(
        repository.availability(),
        SnippetPreferenceAvailability::Available
    );
    let user_key = key(SnippetCatalogOrigin::User, "snippet.same-id");
    let builtin_key = key(SnippetCatalogOrigin::Builtin, "snippet.same-id");

    repository
        .set_favorite(user_key.clone(), true)
        .await
        .expect("收藏用户片段");
    repository
        .set_favorite(builtin_key.clone(), false)
        .await
        .expect("创建独立内置偏好行");
    let request = SnippetUsageRecordRequest {
        receipt_id: "receipt-1".to_owned(),
        key: user_key.clone(),
        action: SnippetUsageAction::Run,
    };
    let first = repository
        .record_usage(request.clone())
        .await
        .expect("记录首次运行");
    let retry = repository.record_usage(request).await.expect("同回执重试");
    assert_eq!(retry, first);

    let preferences = repository
        .list(vec![user_key.clone(), builtin_key.clone()])
        .await
        .expect("批量读取来源隔离偏好");
    assert_eq!(preferences.len(), 2);
    let user = preferences
        .iter()
        .find(|preference| preference.key == user_key)
        .expect("用户偏好存在");
    assert!(user.favorite);
    assert_eq!(user.use_count, 1);
    assert_eq!(user.last_action, Some(SnippetUsageAction::Run));
    assert_eq!(user.last_used_at_unix_ms, Some(first.occurred_at_unix_ms));

    let conflicting_retry = repository
        .record_usage(SnippetUsageRecordRequest {
            receipt_id: "receipt-1".to_owned(),
            key: builtin_key,
            action: SnippetUsageAction::Insert,
        })
        .await;
    assert!(matches!(conflicting_retry, Err(AppError::Conflict { .. })));
}

/// 清理在同一数据库事务中移除回执、清零用量，并完整保留收藏行。
#[tokio::test]
async fn clear_usage_preserves_favorites_and_resets_statistics() {
    let temporary = TemporaryDatabase::new();
    let repository = temporary.repository();
    let key = key(SnippetCatalogOrigin::Builtin, "snippet.keep-favorite");
    repository
        .set_favorite(key.clone(), true)
        .await
        .expect("收藏内置片段");
    repository
        .record_usage(SnippetUsageRecordRequest {
            receipt_id: "receipt-clear".to_owned(),
            key: key.clone(),
            action: SnippetUsageAction::CopyRendered,
        })
        .await
        .expect("记录使用回执");

    let cleared = repository.clear_usage().await.expect("清理使用统计");
    assert_eq!(cleared.receipts_removed, 1);
    assert_eq!(cleared.preferences_reset, 1);
    let preference = repository
        .list(vec![key])
        .await
        .expect("读取保留的偏好")
        .pop()
        .expect("favorite 行保留");
    assert!(preference.favorite);
    assert_eq!(preference.use_count, 0);
    assert_eq!(preference.last_action, None);
    assert_eq!(preference.last_used_at_unix_ms, None);
}

/// 未来 schema 的历史数据库仍让偏好入口可见 unavailable，不创建空表或覆盖版本。
#[tokio::test]
async fn future_database_schema_is_reported_without_preference_fallback() {
    let temporary = TemporaryDatabase::new();
    fs::create_dir_all(temporary.0.join("data")).expect("创建数据库目录");
    let path = temporary.0.join("data").join("command.sqlite");
    let connection = rusqlite::Connection::open(&path).expect("创建隔离 SQLite 数据库");
    connection
        .pragma_update(None, "user_version", 88)
        .expect("标记未来 schema");
    drop(connection);

    let repository = SqliteCommandHistoryRepository::open_or_unavailable(StorageCoordinator::new(
        temporary.0.clone(),
    ));
    assert_eq!(
        repository.availability(),
        SnippetPreferenceAvailability::Unavailable {
            reason: SnippetPreferenceUnavailableReason::UnsupportedSchemaVersion(88),
        }
    );
    assert!(
        repository
            .list(vec![key(SnippetCatalogOrigin::User, "snippet.any")])
            .await
            .is_err()
    );

    let verify = rusqlite::Connection::open(path).expect("复读隔离数据库");
    let version: u32 = verify
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("读取 schema version");
    assert_eq!(version, 88);
}

/// 构造包含来源的复合身份，防止用户行覆盖同 ID 的内置偏好。
fn key(origin: SnippetCatalogOrigin, snippet_id: &str) -> SnippetPreferenceKey {
    SnippetPreferenceKey {
        origin,
        snippet_id: snippet_id.to_owned(),
    }
}
