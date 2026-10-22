//! SQLite 一次性迁移恢复闭环测试。
//!
//! 所有数据库都在随机临时目录中合成；WAL 场景验证未 checkpoint 的命令进入一致性 before。
//!
//! @author kongweiguang

use std::{fs, path::PathBuf};

use rusqlite::{Connection, OpenFlags, params};
use tempfile::TempDir;
use uuid::Uuid;

use super::*;

const LEGACY_V1_SCHEMA: &str = include_str!("../../../../tests/fixtures/command_history/v1.sql");
const LEGACY_V2_MIGRATION: &str =
    include_str!("../../../../tests/fixtures/command_history/v2-from-v1.sql");

struct MigrationFixture {
    _temporary_root: TempDir,
    root: PathBuf,
    operation_dir: PathBuf,
}

impl MigrationFixture {
    /// 每个测试使用独立 root 和 UUID operation，避免触碰用户目录或共享锁状态。
    fn new() -> Self {
        let temporary_root = tempfile::tempdir().expect("create isolated migration root");
        let root = temporary_root.path().to_path_buf();
        let migration_root = root.join(".kerminal-migration");
        fs::create_dir_all(&migration_root).expect("create migration root");
        let operation_dir = migration_root.join(Uuid::new_v4().hyphenated().to_string());
        fs::create_dir(&operation_dir).expect("create operation directory");
        Self {
            _temporary_root: temporary_root,
            root,
            operation_dir,
        }
    }

    /// 返回固定 command.sqlite 路径；测试用该路径核对迁移前后内容与 sidecar。
    fn database(&self) -> PathBuf {
        self.root.join(DATABASE_RELATIVE_PATH)
    }
}

/// 合成 schema 2 数据库并放入可识别的命令行，确保迁移验证真实记录内容而非只看版本号。
///
/// @author kongweiguang
fn create_schema_two_database(path: &std::path::Path) {
    let parent = path.parent().expect("database parent");
    fs::create_dir_all(parent).expect("create database parent");
    let connection = Connection::open(path).expect("create legacy database");
    connection
        .execute_batch(LEGACY_V1_SCHEMA)
        .expect("create legacy schema 1");
    connection
        .execute_batch(LEGACY_V2_MIGRATION)
        .expect("migrate fixture to schema 2");
    connection
        .pragma_update(None, "user_version", 2_u32)
        .expect("mark schema 2");
    connection
        .execute(
            "INSERT INTO command_history(id, command, source, target) VALUES (?1, ?2, 'user', 'local')",
            params!["migration-row", "printf migration-marker"],
        )
        .expect("insert legacy row");
}

/// 通过 SQLite API 生成当前格式副本，为 schema 3 不重写用例提供可信结构。
///
/// @author kongweiguang
fn create_current_database(path: &std::path::Path) {
    fs::create_dir_all(path.parent().expect("database parent"))
        .expect("create current database parent");
    let source_path = path.with_file_name("empty-source.sqlite");
    drop(Connection::open(&source_path).expect("create empty legacy source"));
    migrate_legacy_command_history(&source_path, path).expect("create current database copy");
    fs::remove_file(source_path).expect("remove synthetic empty source");
    let connection = Connection::open(path).expect("open current database");
    connection
        .execute(
            "INSERT INTO command_history(id, command, source, target) VALUES (?1, ?2, 'user', 'local')",
            params!["current-row", "printf current-marker"],
        )
        .expect("insert current row");
}

/// 在目标目录放入含未 checkpoint 记录的 WAL 数据库，复用真实 SQLite 生成 sidecar 状态。
///
/// @author kongweiguang
fn create_uncheckpointed_wal_database(fixture: &MigrationFixture) {
    let live_database = fixture.root.join("live-writer/command.sqlite");
    create_schema_two_database(&live_database);

    let writer = Connection::open(&live_database).expect("open live WAL source");
    writer
        .execute_batch("PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;")
        .expect("enable persistent test WAL");
    writer
        .execute(
            "INSERT INTO command_history(id, command, source, target) VALUES ('wal-only-row', 'printf wal-marker', 'user', 'local')",
            [],
        )
        .expect("commit WAL-only row");
    let live_wal = sidecar_path(&live_database, "-wal");
    let live_shm = sidecar_path(&live_database, "-shm");
    assert!(live_wal.is_file(), "fixture row must remain in WAL");
    assert!(live_shm.is_file(), "WAL fixture must have shared memory");

    fs::create_dir_all(fixture.database().parent().expect("root database parent"))
        .expect("create root data directory");
    fs::copy(&live_database, fixture.database()).expect("copy main database");
    fs::copy(&live_wal, sidecar_path(&fixture.database(), "-wal")).expect("copy WAL sidecar");
    fs::copy(&live_shm, sidecar_path(&fixture.database(), "-shm")).expect("copy SHM sidecar");
    drop(writer);
}

/// 测试迁移原子替换、双次恢复与业务记录完整性，阻止版本号更新代替真实数据验证。
///
/// @author kongweiguang
#[test]
fn legacy_database_migrates_atomically_and_preserves_rows() {
    let fixture = MigrationFixture::new();
    create_schema_two_database(&fixture.database());
    let original_hash = hash_required_regular_file(&fixture.database()).expect("hash original");

    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare migration");
    let entry = prepared.journal.expect("legacy migration journal");
    assert!(prepared.changed);
    assert_eq!(entry.source_schema_version, 2);
    assert!(matches!(
        inspect_entry(&fixture.root, &fixture.operation_dir, &entry),
        Ok(SqliteRecoveryState::Prepared)
    ));
    assert_eq!(
        hash_required_regular_file(&fixture.database()).expect("source remains untouched"),
        original_hash
    );

    apply(&fixture.root, &fixture.operation_dir, &entry).expect("apply migration");
    apply(&fixture.root, &fixture.operation_dir, &entry).expect("idempotent recovery");
    verify_committed(&fixture.root, &fixture.operation_dir, &entry)
        .expect("verify committed database");
    assert!(matches!(
        inspect_entry(&fixture.root, &fixture.operation_dir, &entry),
        Ok(SqliteRecoveryState::Committed)
    ));
    assert_ne!(
        hash_required_regular_file(&fixture.database()).expect("hash migrated database"),
        original_hash
    );

    let connection =
        Connection::open_with_flags(fixture.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open migrated database read-only");
    let version: u32 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read migrated schema");
    let command: String = connection
        .query_row(
            "SELECT command FROM command_history WHERE id='migration-row'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated command");
    assert_eq!(version, 3);
    assert_eq!(command, "printf migration-marker");
}

/// 复制仍有未 checkpoint WAL 的旧库，再验证快照含该记录及 sidecar 移动中断后的前向恢复。
///
/// @author kongweiguang
#[test]
fn uncheckpointed_wal_is_backed_up_and_sidecar_move_resumes() {
    let fixture = MigrationFixture::new();
    create_uncheckpointed_wal_database(&fixture);

    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare WAL migration");
    let entry = prepared.journal.expect("legacy WAL migration journal");
    assert!(entry.source_wal_sha256.is_some());
    assert!(entry.source_shm_sha256.is_some());
    let (before_database, _) = private_database_paths(&fixture.operation_dir);
    let (raw_wal, _) = private_sidecar_paths(&fixture.operation_dir);
    let wal = sidecar_path(&fixture.database(), "-wal");
    commit_staged_file(&wal, &raw_wal, CommitMode::MoveNoReplace)
        .expect("simulate interruption after WAL preservation");
    let raw_wal_hash = hash_required_regular_file(&raw_wal).expect("hash preserved source WAL");
    assert_eq!(
        raw_wal_hash,
        entry.source_wal_sha256.clone().expect("source WAL hash")
    );

    // before 与 raw-sidecars 分目录；重开一致性快照不得自动回放或改写原 WAL 证据。
    let before = Connection::open_with_flags(&before_database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open consistent before snapshot");
    let backup_command: String = before
        .query_row(
            "SELECT command FROM command_history WHERE id='wal-only-row'",
            [],
            |row| row.get(0),
        )
        .expect("read uncheckpointed row from before snapshot");
    assert_eq!(backup_command, "printf wal-marker");
    drop(before);
    assert_eq!(
        hash_required_regular_file(&raw_wal).expect("re-read preserved source WAL"),
        raw_wal_hash
    );
    assert!(!sidecar_path(&before_database, "-wal").exists());
    assert!(!sidecar_path(&before_database, "-shm").exists());
    assert!(matches!(
        inspect_entry(&fixture.root, &fixture.operation_dir, &entry),
        Ok(SqliteRecoveryState::CommitInterrupted)
    ));

    apply(&fixture.root, &fixture.operation_dir, &entry).expect("resume interrupted migration");
    verify_committed(&fixture.root, &fixture.operation_dir, &entry)
        .expect("verify resumed migration");
    assert!(!sidecar_path(&fixture.database(), "-wal").exists());
    assert!(!sidecar_path(&fixture.database(), "-shm").exists());
    assert_eq!(
        hash_required_regular_file(&raw_wal).expect("preserved source WAL"),
        entry.source_wal_sha256.expect("source WAL hash")
    );

    let migrated =
        Connection::open_with_flags(fixture.database(), OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("open resumed database read-only");
    let command: String = migrated
        .query_row(
            "SELECT command FROM command_history WHERE id='wal-only-row'",
            [],
            |row| row.get(0),
        )
        .expect("read migrated WAL-only row");
    assert_eq!(command, "printf wal-marker");
}

/// 当前 schema 3 只验证临时副本；数据库 main 与任何现存 sidecar 字节都不得被重写。
///
/// @author kongweiguang
#[test]
fn current_schema_is_validated_without_rewriting_user_files() {
    let fixture = MigrationFixture::new();
    create_current_database(&fixture.database());
    let original_database = fs::read(fixture.database()).expect("read current database");
    let original_wal = optional_bytes(&sidecar_path(&fixture.database(), "-wal"));
    let original_shm = optional_bytes(&sidecar_path(&fixture.database(), "-shm"));

    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("validate current copy");

    assert!(!prepared.changed);
    assert!(prepared.journal.is_none());
    assert_eq!(
        fs::read(fixture.database()).expect("read unchanged current database"),
        original_database
    );
    assert_eq!(
        optional_bytes(&sidecar_path(&fixture.database(), "-wal")),
        original_wal
    );
    assert_eq!(
        optional_bytes(&sidecar_path(&fixture.database(), "-shm")),
        original_shm
    );
}

/// staging 已进入固定 pending 后模拟进程中断；再次 apply 应沿用 pending 完成替换。
///
/// @author kongweiguang
#[test]
fn apply_resumes_after_pending_publish_interruption() {
    let fixture = MigrationFixture::new();
    create_schema_two_database(&fixture.database());
    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare migration");
    let entry = prepared.journal.expect("migration journal");
    let (_, staged_database) = private_database_paths(&fixture.operation_dir);
    let pending =
        pending_database_path(&fixture.root, &fixture.operation_dir).expect("derive pending path");

    commit_staged_file(&staged_database, &pending, CommitMode::MoveStagedNoReplace)
        .expect("publish pending before simulated interruption");
    assert!(!staged_database.exists());
    assert_eq!(
        inspect_entry(&fixture.root, &fixture.operation_dir, &entry)
            .expect("inspect pending state"),
        SqliteRecoveryState::Prepared
    );

    apply(&fixture.root, &fixture.operation_dir, &entry).expect("resume pending replacement");
    verify_committed(&fixture.root, &fixture.operation_dir, &entry)
        .expect("verify resumed pending replacement");
}

/// 单独移动 SHM 后模拟中断，恢复时应识别部分 sidecar 布局并完成 WAL 与数据库提交。
///
/// @author kongweiguang
#[test]
fn apply_resumes_after_shm_only_move() {
    let fixture = MigrationFixture::new();
    create_uncheckpointed_wal_database(&fixture);
    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare WAL migration");
    let entry = prepared.journal.expect("migration journal");
    let (_, raw_shm) = private_sidecar_paths(&fixture.operation_dir);
    let root_shm = sidecar_path(&fixture.database(), "-shm");

    commit_staged_file(&root_shm, &raw_shm, CommitMode::MoveNoReplace)
        .expect("simulate interruption after only SHM moved");
    assert_eq!(
        inspect_entry(&fixture.root, &fixture.operation_dir, &entry)
            .expect("inspect one-sidecar interruption"),
        SqliteRecoveryState::CommitInterrupted
    );

    apply(&fixture.root, &fixture.operation_dir, &entry).expect("resume after SHM move");
    verify_committed(&fixture.root, &fixture.operation_dir, &entry)
        .expect("verify SHM-only recovery");
    assert!(!sidecar_path(&fixture.database(), "-wal").exists());
    assert!(!root_shm.exists());
    assert_eq!(
        hash_required_regular_file(&raw_shm).expect("preserved SHM hash"),
        entry.source_shm_sha256.expect("source SHM hash")
    );
}

/// pending 发布后出现新的 root WAL 时必须拒绝替换，并保留 main、pending 和新 sidecar 证据。
///
/// @author kongweiguang
#[test]
fn pending_replace_blocks_new_root_sidecar_without_overwrite() {
    let fixture = MigrationFixture::new();
    create_schema_two_database(&fixture.database());
    let original_database = fs::read(fixture.database()).expect("read original database");
    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare migration");
    let entry = prepared.journal.expect("migration journal");
    let (_, staged_database) = private_database_paths(&fixture.operation_dir);
    let pending =
        pending_database_path(&fixture.root, &fixture.operation_dir).expect("derive pending path");
    commit_staged_file(&staged_database, &pending, CommitMode::MoveStagedNoReplace)
        .expect("publish pending before sidecar race");
    let pending_hash = hash_required_regular_file(&pending).expect("hash pending stage");
    let root_wal = sidecar_path(&fixture.database(), "-wal");
    let unexpected_wal: &[u8] = b"new root WAL after pending publication";
    fs::write(&root_wal, unexpected_wal).expect("inject new root sidecar");

    assert!(validate_pending_replace(&fixture.root, &fixture.operation_dir, &entry).is_err());
    assert!(apply(&fixture.root, &fixture.operation_dir, &entry).is_err());
    assert_eq!(
        fs::read(fixture.database()).expect("main remains original"),
        original_database
    );
    assert_eq!(
        hash_required_regular_file(&pending).expect("pending retained"),
        pending_hash
    );
    assert_eq!(
        fs::read(&root_wal).expect("new WAL retained").as_slice(),
        unexpected_wal
    );
}

/// 用户库出现 before/after 之外的内容时必须拒绝恢复，并保留该内容供人工诊断。
///
/// @author kongweiguang
#[test]
fn third_database_value_blocks_recovery_without_overwrite() {
    let fixture = MigrationFixture::new();
    create_schema_two_database(&fixture.database());
    let prepared = prepare(&fixture.root, &fixture.operation_dir).expect("prepare migration");
    let entry = prepared.journal.expect("migration journal");
    let mut changed = fs::read(fixture.database()).expect("read source database");
    changed.extend_from_slice(b"unrecognized-third-value");
    fs::write(fixture.database(), &changed).expect("inject third database value");

    assert!(inspect_entry(&fixture.root, &fixture.operation_dir, &entry).is_err());
    assert!(apply(&fixture.root, &fixture.operation_dir, &entry).is_err());
    assert_eq!(
        fs::read(fixture.database()).expect("read retained third value"),
        changed
    );
}

/// 读取可选 sidecar 的原始字节，测试确认 schema 3 快速验证不触碰数据库伴随文件。
///
/// @author kongweiguang
fn optional_bytes(path: &std::path::Path) -> Option<Vec<u8>> {
    fs::read(path).ok()
}
