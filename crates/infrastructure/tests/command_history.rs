//! 命令历史 SQLite adapter、旧 schema 迁移和一致性备份集成测试。
//!
//! 数据库仅写入人工测试命令，并始终位于随机临时配置根。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
};

use kerminal_application::{
    AppError,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandHistoryUnavailableReason,
    },
};
use kerminal_domain::{
    commands::{CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget},
    identity::{HostId, PaneId, SessionId, TabId},
};
use kerminal_infrastructure::{
    command_history::{
        CommandHistoryInitError, SqliteCommandHistoryRepository, migrate_legacy_command_history,
    },
    storage::StorageCoordinator,
};
use rusqlite::{Connection, OpenFlags, params, types::Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

const LEGACY_V1_SCHEMA: &str = include_str!("../../../tests/fixtures/command_history/v1.sql");
const LEGACY_V2_MIGRATION: &str =
    include_str!("../../../tests/fixtures/command_history/v2-from-v1.sql");

const HISTORY_TABLES: &[&str] = &[
    "command_history",
    "command_suggestion_provider_cache",
    "command_suggestion_feedback",
    "command_suggestion_telemetry",
    "command_suggestion_audit_events",
];

/// 每项测试只创建随机数据库根，退出时清理该路径及 SQLite sidecar。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 创建独立工作区，避免并发测试共享 command.sqlite 或使用正式配置。
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("kerminal-history-test-{}", Uuid::new_v4().simple()));
        fs::create_dir_all(&path).expect("create isolated test root");
        Self(path)
    }

    /// 返回该测试专属配置根，不从环境或用户目录推导路径。
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    /// 删除当前随机测试目录；即使 SQLite 尚有 WAL 文件也只清理本测试范围。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 创建一个没有额外上下文的命令历史写入 draft。
fn draft(command: &str, target: CommandHistoryTarget) -> CommandHistoryDraft {
    CommandHistoryDraft {
        command: command.to_owned(),
        source: CommandHistorySource::User,
        target,
        session_id: None,
        pane_id: None,
        tab_id: None,
        profile_id: None,
        remote_host_id: None,
        cwd: None,
        shell: None,
    }
}

/// 验证写入、过滤和清理闭环；搜索输入是字面子串，LIKE 元字符不能变成 SQL 通配符。
///
/// @author kongweiguang
#[tokio::test]
async fn repository_filters_searches_deletes_and_clears_by_target() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history");
    assert_eq!(repository.schema_version().expect("schema version"), 3);

    let local = repository
        .append(draft("echo local marker", CommandHistoryTarget::Local))
        .await
        .expect("append local command");
    let ssh = repository
        .append(draft("echo remote marker", CommandHistoryTarget::Ssh))
        .await
        .expect("append ssh command");
    let container = repository
        .append(draft("docker ps marker", CommandHistoryTarget::Container))
        .await
        .expect("append container command");

    let all = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect("list history");
    assert_eq!(all.len(), 3);
    assert_eq!(
        all[0].id, container.id,
        "same-second entries use descending rowid"
    );
    assert_eq!(all[0].target, CommandHistoryTarget::Container);
    assert_eq!(all[1].id, ssh.id);

    // LIKE 元字符按字面量搜索，不能扩大到其它命令。
    let wildcard_literal = repository
        .list(CommandHistoryListRequest {
            query: Some("% marker".to_owned()),
            target: Some(CommandHistoryTarget::Ssh),
            ..CommandHistoryListRequest::default()
        })
        .await
        .expect("search literal wildcard");
    assert!(wildcard_literal.is_empty());

    let filtered = repository
        .list(CommandHistoryListRequest {
            query: Some("remote marker".to_owned()),
            target: Some(CommandHistoryTarget::Ssh),
            ..CommandHistoryListRequest::default()
        })
        .await
        .expect("search and target filter");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, ssh.id);

    let removed = repository
        .clear(CommandHistoryClearRequest {
            target: Some(CommandHistoryTarget::Ssh),
            ..CommandHistoryClearRequest::default()
        })
        .await
        .expect("clear only ssh target");
    assert_eq!(removed, 1);
    let remaining = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect("list after scoped clear");
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|entry| entry.id == local.id));
    assert!(remaining.iter().any(|entry| entry.id == container.id));

    assert!(
        repository
            .delete(local.id.clone())
            .await
            .expect("delete existing entry")
    );
    assert!(!repository.delete(local.id).await.expect("repeat delete"));
    let remaining = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect("list after delete");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].target, CommandHistoryTarget::Container);

    let sensitive = repository
        .append(draft(
            "echo password=manual-test-token",
            CommandHistoryTarget::Local,
        ))
        .await
        .expect_err("reject sensitive command at repository boundary");
    assert!(matches!(sensitive, AppError::Invalid { .. }));
    assert!(!format!("{sensitive:?}").contains("manual-test-token"));
    assert_eq!(
        repository
            .list(CommandHistoryListRequest::default())
            .await
            .expect("final list")
            .len(),
        1
    );
}

/// 创建一个冻结 v1 数据库并植入各旧表记录，供 1→2→3 迁移验证。
///
/// @author kongweiguang
fn create_v1_database(database_file: &Path) {
    if let Some(parent) = database_file.parent() {
        fs::create_dir_all(parent).expect("create database parent");
    }
    let connection = Connection::open(database_file).expect("open fixture database");
    connection
        .execute_batch(LEGACY_V1_SCHEMA)
        .expect("create frozen v1 schema");
    seed_history_rows(&connection);
}

/// 用同一组人工数据比较全新当前库与旧库迁移，避免只比较记录数而漏掉字段损失。
///
/// @author kongweiguang
fn seed_history_rows(connection: &Connection) {
    connection
        .execute(
            "INSERT INTO command_history (id, command, source, target, created_at) VALUES (?1, ?2, 'user', 'dockerContainer', '2025-01-02 03:04:05')",
            params!["legacy-command", "echo legacy fixture"],
        )
        .expect("seed legacy command row");
    connection
        .execute(
            "INSERT INTO command_suggestion_provider_cache (provider, host_id, scope_key, payload_json, cached_at_unix_ms, expires_at_unix_ms, ttl_seconds) VALUES ('history', 'host-legacy', 'default', '{}', 100, 200, 30)",
            [],
        )
        .expect("seed provider cache row");
    connection
        .execute(
            "INSERT INTO command_suggestion_feedback (id, action, provider, target, replacement_text, input, created_at_unix_ms) VALUES ('legacy-feedback', 'accepted', 'history', 'local', 'echo', 'ec', 100)",
            [],
        )
        .expect("seed suggestion feedback row");
    connection
        .execute(
            "INSERT INTO command_suggestion_telemetry (provider, first_event_unix_ms, last_event_unix_ms) VALUES ('history', 100, 100)",
            [],
        )
        .expect("seed telemetry row");
    connection
        .execute(
            "INSERT INTO command_suggestion_audit_events (id, event_kind, provider, target, decision, created_at_unix_ms) VALUES ('legacy-audit', 'remoteProbeSchedule', 'history', 'ssh', 'allowed', 100)",
            [],
        )
        .expect("seed audit row");
}

/// 摘要覆盖主文件全部字节，用于验证拒绝和副本迁移不会改写源库。
///
/// @author kongweiguang
fn database_digest(path: &Path) -> [u8; 32] {
    Sha256::digest(fs::read(path).expect("read isolated database bytes")).into()
}

/// 比较稳定测试表的每列值；不依赖迁移后的 SQL 排版或只核对行数。
///
/// @author kongweiguang
fn table_rows(connection: &Connection, tables: &[&str]) -> Vec<Vec<Vec<Value>>> {
    tables
        .iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .expect("read fixture table");
            let column_count = statement.column_count();
            statement
                .query_map([], |row| {
                    (0..column_count).map(|index| row.get(index)).collect()
                })
                .expect("query fixture rows")
                .collect::<Result<_, _>>()
                .expect("read every fixture field")
        })
        .collect()
}

/// 显式迁移写入另一配置根，逐字段核对旧数据并通过普通仓储与在线备份回读。
///
/// @author kongweiguang
#[tokio::test]
async fn legacy_v1_migrates_transactionally_and_online_backup_contains_rows() {
    let root = TestRoot::new();
    let database_file = root.path().join("data/command.sqlite");
    let source_root = root.path().join("legacy");
    let source_file = source_root.join("data/command.sqlite");
    create_v1_database(&source_file);
    let before = database_digest(&source_file);
    let source = Connection::open_with_flags(&source_file, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("read legacy rows");
    let original_rows = table_rows(&source, HISTORY_TABLES);
    drop(source);
    let storage = StorageCoordinator::new(source_root);
    let guard = storage
        .lock()
        .expect("hold source root lock during migration");
    let result = migrate_legacy_command_history(&source_file, &database_file)
        .expect("explicitly migrate legacy v1 copy without reentering root lock");
    drop(guard);
    assert_eq!(result.source_schema_version, 1);
    assert_eq!(result.target_schema_version, 3);
    assert_eq!(database_digest(&source_file), before);

    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open explicitly migrated v3 database");
    assert_eq!(
        repository.schema_version().expect("schema after migration"),
        3
    );
    let entries = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect("read migrated command");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].id, "legacy-command");
    assert_eq!(entries[0].target, CommandHistoryTarget::Container);

    let connection = Connection::open(&database_file).expect("open migrated database");
    assert_eq!(table_rows(&connection, HISTORY_TABLES), original_rows);
    for (table, expected) in [
        ("command_suggestion_provider_cache", 1_i64),
        ("command_suggestion_feedback", 1_i64),
        ("command_suggestion_telemetry", 1_i64),
        ("command_suggestion_audit_events", 1_i64),
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("read preserved migrated table");
        assert_eq!(count, expected, "preserve {table}");
    }
    let preferences: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='snippet_preferences'",
            [],
            |row| row.get(0),
        )
        .expect("check v3 snippet table");
    assert_eq!(preferences, 1);
    drop(connection);

    let backup_file = root.path().join("manual-backup.sqlite");
    repository
        .create_consistent_backup(&backup_file)
        .expect("create online backup");
    let backup = Connection::open(&backup_file).expect("open online backup");
    let backup_version: i64 = backup
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("backup version");
    assert_eq!(backup_version, 3);
    let backup_count: i64 = backup
        .query_row("SELECT count(*) FROM command_history", [], |row| row.get(0))
        .expect("backup command row");
    assert_eq!(backup_count, 1);

    assert!(!root.path().join("data/backups").exists());
}

/// v0/v2 只能经显式副本迁移后打开，v3 可重复读取，未来版本仍拒绝且保留原库。
///
/// @author kongweiguang
#[test]
fn schema_versions_zero_two_three_and_future_are_handled_explicitly() {
    let root = TestRoot::new();
    let v0 = root.path().join("v0/data/command.sqlite");
    fs::create_dir_all(v0.parent().expect("v0 parent")).expect("create v0 database parent");
    let connection = Connection::open(&v0).expect("create v0 database");
    drop(connection);
    let migrated_v0_root = root.path().join("migrated-v0");
    let v0_result =
        migrate_legacy_command_history(&v0, migrated_v0_root.join("data/command.sqlite"))
            .expect("explicitly migrate schema zero");
    assert_eq!(v0_result.source_schema_version, 0);
    let migrated_v0 =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(migrated_v0_root))
            .expect("open migrated schema zero copy");
    assert_eq!(migrated_v0.schema_version().expect("v0 current version"), 3);

    let v2_root = root.path().join("v2");
    let v2_file = v2_root.join("data/command.sqlite");
    create_v1_database(&v2_file);
    let v2 = Connection::open(&v2_file).expect("open v1 fixture for v2 conversion");
    v2.execute_batch(LEGACY_V2_MIGRATION)
        .expect("apply frozen v1-to-v2 migration");
    drop(v2);
    let migrated_v2_root = root.path().join("migrated-v2");
    let v2_result =
        migrate_legacy_command_history(&v2_file, migrated_v2_root.join("data/command.sqlite"))
            .expect("explicitly migrate schema two");
    assert_eq!(v2_result.source_schema_version, 2);
    let migrated_v2 =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(migrated_v2_root))
            .expect("open migrated schema two copy");
    assert_eq!(migrated_v2.schema_version().expect("v2 current version"), 3);

    let v3_root = root.path().join("v3");
    let current = SqliteCommandHistoryRepository::open(StorageCoordinator::new(v3_root.clone()))
        .expect("initialize current schema three");
    assert_eq!(current.schema_version().expect("v3 current version"), 3);
    drop(current);
    let reopened = SqliteCommandHistoryRepository::open(StorageCoordinator::new(v3_root))
        .expect("reopen current schema without migration");
    assert_eq!(reopened.schema_version().expect("reopened version"), 3);

    let future_root = root.path().join("future");
    fs::create_dir_all(future_root.join("data")).expect("create future database parent");
    let future_file = future_root.join("data/command.sqlite");
    let future = Connection::open(&future_file).expect("create future database");
    future
        .pragma_update(None, "user_version", 4_i64)
        .expect("set future schema");
    drop(future);
    let error = SqliteCommandHistoryRepository::open(StorageCoordinator::new(future_root.clone()))
        .err()
        .expect("reject future schema");
    assert!(matches!(
        error,
        CommandHistoryInitError::UnsupportedSchemaVersion(4)
    ));
    let unchanged = Connection::open(future_file).expect("reopen rejected database");
    let version: i64 = unchanged
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read unchanged version");
    assert_eq!(version, 4);
}

/// 未知 schema 保持原库不变，同时以可见状态开放桌面其它能力。
#[tokio::test]
async fn future_schema_is_visible_without_migrating_or_faking_empty_history() {
    let root = TestRoot::new();
    let database_file = root.path().join("data/command.sqlite");
    fs::create_dir_all(database_file.parent().expect("database parent"))
        .expect("create future database parent");
    let future = Connection::open(&database_file).expect("create future database");
    future
        .pragma_update(None, "user_version", 4_i64)
        .expect("set future schema");
    drop(future);
    let before = fs::read(&database_file).expect("read original future database");

    let repository = SqliteCommandHistoryRepository::open_or_unavailable(StorageCoordinator::new(
        root.path().to_path_buf(),
    ));
    assert_eq!(
        repository.availability(),
        CommandHistoryAvailability::Unavailable {
            reason: CommandHistoryUnavailableReason::UnsupportedSchemaVersion(4),
        }
    );
    assert!(
        repository
            .list(CommandHistoryListRequest::default())
            .await
            .is_err(),
        "unavailable history must not appear as an empty successful query"
    );
    assert_eq!(
        fs::read(&database_file).expect("read future database after open"),
        before,
        "future database bytes are preserved"
    );
}

/// 损坏 SQLite 以 typed unavailable 状态呈现，历史用例失败且原始证据未被覆盖。
#[tokio::test]
async fn corrupt_database_is_visible_and_preserved_until_external_restore() {
    let root = TestRoot::new();
    let database_file = root.path().join("data/command.sqlite");
    fs::create_dir_all(database_file.parent().expect("database parent"))
        .expect("create corrupt database parent");
    let corrupt_bytes = b"manual synthetic corrupt sqlite fixture";
    fs::write(&database_file, corrupt_bytes).expect("write corrupt fixture");

    let repository = SqliteCommandHistoryRepository::open_or_unavailable(StorageCoordinator::new(
        root.path().to_path_buf(),
    ));
    assert_eq!(
        repository.availability(),
        CommandHistoryAvailability::Unavailable {
            reason: CommandHistoryUnavailableReason::CorruptDatabase,
        }
    );
    let error = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect_err("corrupt history cannot report a successful empty list");
    assert!(matches!(error, AppError::ConfigurationCorrupt { .. }));
    assert_eq!(
        fs::read(&database_file).expect("read preserved corrupt fixture"),
        corrupt_bytes
    );
}

/// 验证同步 command history 的 session、pane、host 等身份仍使用领域类型化值。
#[test]
fn history_draft_accepts_typed_context_without_stringly_typed_conversion() {
    let session = SessionId::parse("session-1").expect("session id");
    let pane = PaneId::parse("pane-1").expect("pane id");
    let tab = TabId::parse("tab-1").expect("tab id");
    let host = HostId::parse("host-1").expect("host id");
    let mut command = draft("echo typed context", CommandHistoryTarget::Ssh);
    command.source = CommandHistorySource::Tool;
    command.session_id = Some(session);
    command.pane_id = Some(pane);
    command.tab_id = Some(tab);
    command.remote_host_id = Some(host);
    assert_eq!(command.source, CommandHistorySource::Tool);
    assert_eq!(command.target, CommandHistoryTarget::Ssh);
    assert!(command.session_id.is_some() && command.pane_id.is_some() && command.tab_id.is_some());
}

/// 旧格式启动失败必须可见，且拒绝过程不生成备份、WAL 或 SHM，也不修改主文件。
///
/// @author kongweiguang
#[tokio::test]
async fn ordinary_open_rejects_legacy_versions_without_storage_mutation() {
    for version in 0..=2_u32 {
        let root = TestRoot::new();
        let database_file = root.path().join("data/command.sqlite");
        create_v1_database(&database_file);
        let connection = Connection::open(&database_file).expect("open legacy fixture");
        if version == 2 {
            connection
                .execute_batch(LEGACY_V2_MIGRATION)
                .expect("create schema two fixture");
        } else {
            connection
                .pragma_update(None, "user_version", version)
                .expect("set legacy fixture version");
        }
        drop(connection);
        let before = database_digest(&database_file);
        let error = SqliteCommandHistoryRepository::open(StorageCoordinator::new(
            root.path().to_path_buf(),
        ))
        .err()
        .expect("ordinary open must reject legacy schema");
        assert!(
            matches!(error, CommandHistoryInitError::MigrationRequired(actual) if actual == version)
        );

        let repository = SqliteCommandHistoryRepository::open_or_unavailable(
            StorageCoordinator::new(root.path().to_path_buf()),
        );
        assert_eq!(
            repository.availability(),
            CommandHistoryAvailability::Unavailable {
                reason: CommandHistoryUnavailableReason::MigrationRequired(version),
            }
        );
        assert!(
            repository
                .list(CommandHistoryListRequest::default())
                .await
                .is_err()
        );
        assert!(
            repository
                .append(draft(
                    "echo rejected legacy write",
                    CommandHistoryTarget::Local
                ))
                .await
                .is_err()
        );
        assert_eq!(database_digest(&database_file), before);
        let entries = fs::read_dir(root.path().join("data"))
            .expect("inspect legacy data directory")
            .map(|entry| entry.expect("read data entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, vec![std::ffi::OsString::from("command.sqlite")]);

        let source = Connection::open_with_flags(&database_file, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("read rejected legacy rows");
        let original_rows = table_rows(&source, HISTORY_TABLES);
        drop(source);
        let migrated_root = root.path().join("explicit-copy");
        let migrated_file = migrated_root.join("data/command.sqlite");
        migrate_legacy_command_history(&database_file, &migrated_file)
            .expect("explicit migration supports every legacy version without dropping rows");
        let copy = Connection::open_with_flags(&migrated_file, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("inspect explicitly migrated legacy rows");
        assert_eq!(table_rows(&copy, HISTORY_TABLES), original_rows);
        drop(copy);
        let migrated = SqliteCommandHistoryRepository::open(StorageCoordinator::new(migrated_root))
            .expect("ordinary open accepts explicit migration result");
        assert_eq!(
            migrated
                .list(CommandHistoryListRequest::default())
                .await
                .expect("read migrated history")
                .len(),
            1
        );
        assert_eq!(database_digest(&database_file), before);
    }
}

/// 源 WAL 中尚未 checkpoint 的提交也必须进入副本；源主文件与 WAL 字节保持不变。
///
/// @author kongweiguang
#[tokio::test]
async fn explicit_migration_captures_wal_without_rewriting_source() {
    let root = TestRoot::new();
    let source_file = root.path().join("legacy/data/command.sqlite");
    create_v1_database(&source_file);
    let source = Connection::open(&source_file).expect("keep source writer open");
    source
        .execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA wal_autocheckpoint=0;
             INSERT INTO command_history (id, command, source, target)
             VALUES ('wal-command', 'echo uncheckpointed fixture', 'user', 'ssh');",
        )
        .expect("commit synthetic row only to WAL");
    let wal_file = source_file.with_file_name("command.sqlite-wal");
    assert!(wal_file.exists());
    let before_database = database_digest(&source_file);
    let before_wal = database_digest(&wal_file);
    let expected_rows = table_rows(&source, HISTORY_TABLES);
    let destination_root = root.path().join("migrated");
    let destination = destination_root.join("data/command.sqlite");
    migrate_legacy_command_history(&source_file, &destination)
        .expect("migrate committed WAL snapshot");
    assert_eq!(database_digest(&source_file), before_database);
    assert_eq!(database_digest(&wal_file), before_wal);
    let target = Connection::open(&destination).expect("read migrated WAL snapshot");
    assert_eq!(table_rows(&target, HISTORY_TABLES), expected_rows);
    drop(target);
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(destination_root))
            .expect("open migrated WAL snapshot normally");
    let entries = repository
        .list(CommandHistoryListRequest::default())
        .await
        .expect("read both committed command rows");
    assert_eq!(entries.len(), 2);
    assert!(entries.iter().any(|entry| entry.id == "wal-command"));
}

/// 当前格式允许附加表与列，正常读写和复制仍保留它们；重复目标与源同目标均拒绝覆盖。
///
/// @author kongweiguang
#[tokio::test]
async fn explicit_migration_preserves_current_rows_and_rejects_existing_destination() {
    let root = TestRoot::new();
    let source_root = root.path().join("current");
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(source_root.clone()))
            .expect("initialize current schema directly");
    drop(repository);
    let source_file = source_root.join("data/command.sqlite");
    let source = Connection::open(&source_file).expect("seed current schema");
    seed_history_rows(&source);
    source
        .execute_batch(
            "INSERT INTO snippet_preferences
             (origin, snippet_id, favorite, use_count, last_action, last_used_at_unix_ms)
             VALUES ('user', 'fixture-snippet', 1, 2, 'run', 123);
             INSERT INTO snippet_usage_receipts
             (receipt_id, origin, snippet_id, action, created_at_unix_ms)
             VALUES ('fixture-receipt', 'user', 'fixture-snippet', 'run', 123);
             ALTER TABLE command_history ADD COLUMN external_note TEXT;
             UPDATE command_history SET external_note='synthetic extension';
             CREATE TABLE external_extension (value TEXT);
             INSERT INTO external_extension VALUES ('synthetic extension');",
        )
        .expect("seed current snippet data");
    let mut tables = HISTORY_TABLES.to_vec();
    tables.extend(["snippet_preferences", "snippet_usage_receipts"]);
    let original_rows = table_rows(&source, &tables);
    drop(source);
    let repository = SqliteCommandHistoryRepository::open(StorageCoordinator::new(source_root))
        .expect("ordinary open allows unrelated tables and columns");
    assert_eq!(
        repository
            .list(CommandHistoryListRequest::default())
            .await
            .expect("query extended current schema")
            .len(),
        1
    );
    drop(repository);
    let before_source = database_digest(&source_file);
    let destination = root.path().join("copy.sqlite");
    let result = migrate_legacy_command_history(&source_file, &destination)
        .expect("copy current schema without rebuilding tables");
    assert_eq!(result.source_schema_version, 3);
    assert_eq!(result.target_schema_version, 3);
    assert_eq!(database_digest(&source_file), before_source);
    let copy = Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("read current format copy");
    assert_eq!(table_rows(&copy, &tables), original_rows);
    let extension: String = copy
        .query_row("SELECT value FROM external_extension", [], |row| row.get(0))
        .expect("preserve unrelated extension table");
    assert_eq!(extension, "synthetic extension");
    drop(copy);
    let before_copy = database_digest(&destination);
    for existing in [&destination, &source_file] {
        let error = migrate_legacy_command_history(&source_file, existing)
            .expect_err("never overwrite a destination that already exists");
        assert!(matches!(error, CommandHistoryInitError::BackupFailed(cause)
            if matches!(cause.as_ref(), CommandHistoryInitError::Io(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists)));
    }
    assert_eq!(database_digest(&destination), before_copy);
    assert_eq!(database_digest(&source_file), before_source);
}

/// 不支持或损坏的源在发布目标前拒绝；CHECK 损坏须由可写 staging 检查并清理副本。
/// 检查正文不得进入公开错误摘要，源文件与既有拒绝断言保持不变。
///
/// @author kongweiguang
#[test]
fn explicit_migration_rejects_future_corrupt_and_missing_sources() {
    let root = TestRoot::new();
    for (name, version) in [("future", 4_i64), ("negative", -1_i64)] {
        let source = root.path().join(format!("{name}.sqlite"));
        let connection = Connection::open(&source).expect("create rejected version fixture");
        connection
            .pragma_update(None, "user_version", version)
            .expect("mark rejected version");
        drop(connection);
        let before = database_digest(&source);
        let destination = root.path().join(format!("{name}-copy.sqlite"));
        let error = migrate_legacy_command_history(&source, &destination)
            .expect_err("reject source version");
        if version > 3 {
            assert!(matches!(
                error,
                CommandHistoryInitError::UnsupportedSchemaVersion(4)
            ));
        } else {
            assert!(matches!(
                error,
                CommandHistoryInitError::InvalidSchemaVersion
            ));
        }
        assert_eq!(database_digest(&source), before);
        assert!(!destination.exists());
    }
    let corrupt = root.path().join("corrupt.sqlite");
    fs::write(&corrupt, b"synthetic invalid SQLite fixture").expect("write corrupt source");
    let before = database_digest(&corrupt);
    let destination = root.path().join("corrupt-copy.sqlite");
    assert!(migrate_legacy_command_history(&corrupt, &destination).is_err());
    assert_eq!(database_digest(&corrupt), before);
    assert!(!destination.exists());

    let invalid_constraint = root.path().join("invalid-constraint.sqlite");
    let connection =
        Connection::open(&invalid_constraint).expect("create invalid constraint fixture");
    connection
        .execute_batch(
            "CREATE TABLE synthetic_check (value TEXT CHECK(value = 'valid'));
         PRAGMA ignore_check_constraints=ON;
         INSERT INTO synthetic_check VALUES ('manual-test-private-marker');",
        )
        .expect("seed physically readable but invalid CHECK value");
    drop(connection);
    let before = database_digest(&invalid_constraint);
    let destination = root.path().join("constraint-copy.sqlite");
    let error = migrate_legacy_command_history(&invalid_constraint, &destination)
        .expect_err("integrity check must reject invalid CHECK values");
    assert!(matches!(
        error,
        CommandHistoryInitError::IntegrityCheckFailed
    ));
    assert!(!error.to_string().contains("manual-test-private-marker"));
    assert_eq!(database_digest(&invalid_constraint), before);
    assert!(!destination.exists());
    assert!(
        fs::read_dir(root.path())
            .expect("inspect failed migration staging cleanup")
            .all(|entry| !entry
                .expect("read isolated migration directory entry")
                .file_name()
                .to_string_lossy()
                .starts_with(".constraint-copy.sqlite.pending-")),
        "invalid CHECK data must not leave staging database or sidecars"
    );

    let missing = root.path().join("missing.sqlite");
    let destination = root.path().join("missing-copy.sqlite");
    assert!(migrate_legacy_command_history(&missing, &destination).is_err());
    assert!(!missing.exists());
    assert!(!destination.exists());
}

/// 中途 DDL 失败时整个副本升级回滚，不能留下已重建一半的表或声称 schema 3 成功。
///
/// @author kongweiguang
#[test]
fn explicit_migration_rolls_back_the_entire_copy_on_upgrade_failure() {
    let root = TestRoot::new();
    let source = root.path().join("legacy.sqlite");
    create_v1_database(&source);
    let connection = Connection::open(&source).expect("open rollback fixture");
    connection
        .execute_batch("CREATE TABLE command_suggestion_feedback_v2 (collision TEXT);")
        .expect("force failure after the first legacy table rebuild");
    let original_rows = table_rows(&connection, HISTORY_TABLES);
    drop(connection);
    let before = database_digest(&source);
    let destination = root.path().join("failed-copy.sqlite");
    assert!(migrate_legacy_command_history(&source, &destination).is_err());
    assert_eq!(database_digest(&source), before);
    let copy = Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("inspect retained failed migration copy");
    let version: u32 = copy
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("copy version after rollback");
    assert_eq!(version, 1);
    assert_eq!(table_rows(&copy, HISTORY_TABLES), original_rows);
    let leftover: i64 = copy
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name='command_suggestion_provider_cache_v2'",
            [],
            |row| row.get(0),
        )
        .expect("look for partial table rebuild");
    assert_eq!(leftover, 0);
}

/// 当前版本标记不能代替实际结构；拒绝时不修表、不创建副本或 SQLite sidecar。
///
/// @author kongweiguang
#[tokio::test]
async fn current_schema_with_missing_structure_is_rejected_without_writes() {
    for (name, mutation) in [
        ("empty", None),
        ("missing-table", Some("DROP TABLE snippet_usage_receipts;")),
        (
            "missing-column",
            Some("ALTER TABLE command_history DROP COLUMN shell;"),
        ),
        (
            "missing-index",
            Some("DROP INDEX idx_command_history_created_at;"),
        ),
        (
            "wrong-index",
            Some(
                "DROP INDEX idx_command_history_created_at; CREATE INDEX idx_command_history_created_at ON command_history(command);",
            ),
        ),
        (
            "missing-primary-key",
            Some(
                "ALTER TABLE snippet_usage_receipts RENAME TO receipts_backup; CREATE TABLE snippet_usage_receipts AS SELECT * FROM receipts_backup; DROP TABLE receipts_backup; CREATE INDEX idx_snippet_usage_receipts_created ON snippet_usage_receipts(created_at_unix_ms DESC);",
            ),
        ),
    ] {
        let root = TestRoot::new();
        if mutation.is_some() {
            drop(
                SqliteCommandHistoryRepository::open(StorageCoordinator::new(
                    root.path().to_path_buf(),
                ))
                .expect("initialize valid current fixture before structural damage"),
            );
        }
        fs::create_dir_all(root.path().join("data"))
            .expect("create isolated schema fixture directory");
        let database_file = root.path().join("data/command.sqlite");
        let connection = Connection::open(&database_file).expect("open structural fixture");
        connection
            .pragma_update(None, "journal_mode", "DELETE")
            .expect("keep fixture free of WAL sidecars");
        connection
            .pragma_update(None, "user_version", 3)
            .expect("claim current version");
        if let Some(mutation) = mutation {
            connection
                .execute_batch(mutation)
                .expect("apply structural fixture damage");
        }
        drop(connection);
        let before = database_digest(&database_file);
        let error = SqliteCommandHistoryRepository::open(StorageCoordinator::new(
            root.path().to_path_buf(),
        ))
        .err()
        .expect("ordinary current open rejects structural mismatch");
        assert!(
            matches!(error, CommandHistoryInitError::InvalidSchemaStructure),
            "{name}: {error}"
        );
        let unavailable = SqliteCommandHistoryRepository::open_or_unavailable(
            StorageCoordinator::new(root.path().to_path_buf()),
        );
        assert_eq!(
            unavailable.availability(),
            CommandHistoryAvailability::Unavailable {
                reason: CommandHistoryUnavailableReason::CorruptDatabase,
            }
        );
        assert!(
            unavailable
                .list(CommandHistoryListRequest::default())
                .await
                .is_err()
        );
        let destination = root.path().join("copy.sqlite");
        let error = migrate_legacy_command_history(&database_file, &destination).expect_err(
            "migration must not certify a current version marker without its structure",
        );
        assert!(
            matches!(error, CommandHistoryInitError::InvalidSchemaStructure),
            "{name}: {error}"
        );
        assert!(!destination.exists());
        assert_eq!(database_digest(&database_file), before, "{name}");
        let entries = fs::read_dir(root.path().join("data"))
            .expect("inspect rejected database directory")
            .map(|entry| entry.expect("read fixture directory entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![std::ffi::OsString::from("command.sqlite")],
            "{name}"
        );
    }
}

/// 旧库升级 SQL 可执行也不代表当前查询可读，最终结构不匹配必须回滚整个副本升级。
///
/// @author kongweiguang
#[test]
fn explicit_migration_rolls_back_when_final_structure_is_incomplete() {
    let root = TestRoot::new();
    let source = root.path().join("legacy.sqlite");
    create_v1_database(&source);
    let connection = Connection::open(&source).expect("open incomplete legacy fixture");
    connection
        .execute_batch("ALTER TABLE command_history DROP COLUMN shell;")
        .expect("remove a column untouched by historical upgrades");
    let original_rows = table_rows(&connection, HISTORY_TABLES);
    drop(connection);
    let before = database_digest(&source);
    let destination = root.path().join("incomplete-copy.sqlite");
    let error = migrate_legacy_command_history(&source, &destination)
        .expect_err("final structure check rejects an otherwise successful upgrade");
    assert!(matches!(
        error,
        CommandHistoryInitError::InvalidSchemaStructure
    ));
    assert_eq!(database_digest(&source), before);
    let copy = Connection::open_with_flags(&destination, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("inspect retained copy after failed validation");
    let version: u32 = copy
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read rollback schema version");
    assert_eq!(version, 1);
    assert_eq!(table_rows(&copy, HISTORY_TABLES), original_rows);
    let preferences: i64 = copy
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='snippet_preferences'",
            [],
            |row| row.get(0),
        )
        .expect("check rollback of newly created snippet tables");
    assert_eq!(preferences, 0);
}
