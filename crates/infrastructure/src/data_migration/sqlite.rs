//! command.sqlite 一次性迁移准备、CAS 提交与崩溃恢复。
//!
//! @author kongweiguang

use std::{
    fs::{self, File},
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use kerminal_application::{AppError, AppResult};
use rusqlite::{Connection, OpenFlags, backup::Backup};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    command_history::{CommandHistoryMigrationResult, migrate_legacy_command_history},
    storage::{AtomicCommitError, CommitMode, commit_staged_file},
};

const DATABASE_RELATIVE_PATH: &str = "data/command.sqlite";
const SQLITE_PRIVATE_DIRECTORY: &str = "sqlite";
const BEFORE_DIRECTORY: &str = "before";
const STAGED_DIRECTORY: &str = "staged";
const RAW_SIDECARS_DIRECTORY: &str = "raw-sidecars";
const DATABASE_FILE_NAME: &str = "command.sqlite";
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次性准备结果；schema 3 只验证副本，不要求调用方提交数据库替换。
#[derive(Debug, Clone)]
pub(crate) struct PreparedSqliteMigration {
    pub(crate) journal: Option<SqliteJournalEntry>,
    pub(crate) changed: bool,
}

/// SQLite 文件提交的 CAS 材料只记录固定 schema 与 SHA-256，不允许日志指定路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SqliteJournalEntry {
    pub(crate) source_schema_version: u32,
    pub(crate) source_database_sha256: String,
    pub(crate) consistent_before_sha256: String,
    pub(crate) staged_database_sha256: String,
    pub(crate) source_wal_sha256: Option<String>,
    pub(crate) source_shm_sha256: Option<String>,
}

/// 只读检查把可恢复布局与已完成布局区分开，外层事务据此决定是否继续提交 marker。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SqliteRecoveryState {
    Prepared,
    CommitInterrupted,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SourceFingerprint {
    database_sha256: String,
    wal_sha256: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SidecarLocation {
    Root,
    Backup,
    Absent,
}

/// 为 schema 0..2 生成相同只读快照上的 before/stage，并在用户库仍为原值时冻结 CAS 指纹。
///
/// schema 3 也走 SQLite online backup 校验，但只清理 operation 私有副本并返回无变更结果。
///
/// @author kongweiguang
pub(crate) fn prepare(root: &Path, operation_dir: &Path) -> AppResult<PreparedSqliteMigration> {
    validate_operation_directory(root, operation_dir)?;
    let database = root.join(DATABASE_RELATIVE_PATH);
    let Some(initial_fingerprint) = source_fingerprint(root)? else {
        return Ok(PreparedSqliteMigration {
            journal: None,
            changed: false,
        });
    };

    let (before_database, staged_database) = private_database_paths(operation_dir);
    let (before_wal, before_shm) = private_sidecar_paths(operation_dir);
    ensure_private_layout(operation_dir)?;
    let pending = pending_database_path(root, operation_dir)?;
    ensure_absent(&before_database)?;
    ensure_absent(&staged_database)?;
    ensure_absent(&sidecar_path(&staged_database, "-wal"))?;
    ensure_absent(&sidecar_path(&staged_database, "-shm"))?;
    ensure_absent(&sidecar_path(&before_database, "-wal"))?;
    ensure_absent(&sidecar_path(&before_database, "-shm"))?;
    ensure_absent(&before_wal)?;
    ensure_absent(&before_shm)?;
    ensure_absent(&pending)?;

    let preparation = (|| {
        let (source_schema_version, consistent_before_sha256) =
            create_consistent_before(&database, &before_database)?;
        let migration_result = migrate_legacy_command_history(&before_database, &staged_database)
            .map_err(|error| {
            AppError::infrastructure_with_source("命令数据库迁移副本校验失败", error)
        })?;
        validate_migration_result(source_schema_version, migration_result)?;

        let final_fingerprint = source_fingerprint(root)?
            .ok_or_else(|| AppError::conflict("命令数据库在迁移期间消失，恢复材料已保留"))?;
        if final_fingerprint.database_sha256 != initial_fingerprint.database_sha256
            || final_fingerprint.wal_sha256 != initial_fingerprint.wal_sha256
        {
            return Err(AppError::conflict(
                "命令数据库在迁移期间发生变化，恢复材料已保留",
            ));
        }

        if source_schema_version == 3 {
            cleanup_private_database(&staged_database)?;
            cleanup_private_database(&before_database)?;
            return Ok(PreparedSqliteMigration {
                journal: None,
                changed: false,
            });
        }

        let entry = SqliteJournalEntry {
            source_schema_version,
            source_database_sha256: final_fingerprint.database_sha256,
            consistent_before_sha256,
            staged_database_sha256: hash_required_regular_file(&staged_database)?,
            source_wal_sha256: final_fingerprint.wal_sha256,
            source_shm_sha256: optional_regular_file_hash(&sidecar_path(&database, "-shm"))?,
        };
        validate_journal(&entry)?;
        Ok(PreparedSqliteMigration {
            journal: Some(entry),
            changed: true,
        })
    })();

    if preparation.is_err() {
        // 失败时只清理本函数刚创建的私有 SQLite 副本，绝不触碰 root 内原库或其 sidecar。
        cleanup_private_database_best_effort(&before_database);
        cleanup_private_database_best_effort(&staged_database);
    }
    preparation
}

/// 验证迁移日志字段与 hash 格式；固定路径由代码构造，避免持久化日志变成任意文件操作。
///
/// @author kongweiguang
pub(crate) fn validate_journal(entry: &SqliteJournalEntry) -> AppResult<()> {
    if !(0..=2).contains(&entry.source_schema_version)
        || !is_sha256(&entry.source_database_sha256)
        || !is_sha256(&entry.consistent_before_sha256)
        || !is_sha256(&entry.staged_database_sha256)
        || entry
            .source_wal_sha256
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
        || entry
            .source_shm_sha256
            .as_deref()
            .is_some_and(|hash| !is_sha256(hash))
    {
        return Err(AppError::invalid("SQLite 迁移日志结构或校验值无效"));
    }
    Ok(())
}

/// 只读核对 before、stage/pending、用户数据库和 sidecar 是否仍处于可继续的 CAS 状态。
///
/// @author kongweiguang
pub(crate) fn inspect_entry(
    root: &Path,
    operation_dir: &Path,
    entry: &SqliteJournalEntry,
) -> AppResult<SqliteRecoveryState> {
    validate_journal(entry)?;
    validate_operation_directory(root, operation_dir)?;
    validate_private_layout(operation_dir)?;
    validate_before_snapshot(operation_dir, entry)?;
    validate_data_directory(root, true)?;

    let database = root.join(DATABASE_RELATIVE_PATH);
    let source_hash = optional_regular_file_hash(&database)?
        .ok_or_else(|| AppError::conflict("命令数据库缺失，迁移恢复已阻断"))?;
    let database_is_before = source_hash == entry.source_database_sha256;
    let database_is_after = source_hash == entry.staged_database_sha256;
    if !database_is_before && !database_is_after {
        return Err(AppError::conflict(
            "命令数据库与迁移日志中的前后版本都不匹配，已保留现场",
        ));
    }

    let pending = pending_database_path(root, operation_dir)?;
    let (_, staged_database) = private_database_paths(operation_dir);
    let (before_wal, before_shm) = private_sidecar_paths(operation_dir);
    let private_stage_hash = optional_regular_file_hash(&staged_database)?;
    let pending_hash = optional_regular_file_hash(&pending)?;
    let stage_available = match (private_stage_hash, pending_hash) {
        (Some(private_hash), None) if private_hash == entry.staged_database_sha256 => true,
        (None, Some(pending_hash)) if pending_hash == entry.staged_database_sha256 => true,
        (None, None) if database_is_after => false,
        (Some(_), Some(_)) => {
            return Err(AppError::conflict(
                "SQLite 迁移 staging 同时出现在两个位置，已保留现场",
            ));
        }
        _ => {
            return Err(AppError::conflict(
                "SQLite 迁移 staging 缺失或校验不匹配，已保留现场",
            ));
        }
    };
    if database_is_before && !stage_available {
        return Err(AppError::conflict(
            "命令数据库尚未提交但迁移 staging 缺失，已保留现场",
        ));
    }
    if database_is_after && stage_available {
        return Err(AppError::conflict(
            "命令数据库已提交但 staging 仍存在，已保留现场",
        ));
    }

    let wal_location = inspect_sidecar(
        &sidecar_path(&database, "-wal"),
        &before_wal,
        entry.source_wal_sha256.as_deref(),
    )?;
    let shm_location = inspect_sidecar(
        &sidecar_path(&database, "-shm"),
        &before_shm,
        entry.source_shm_sha256.as_deref(),
    )?;
    let all_sidecars_backed = [wal_location, shm_location]
        .into_iter()
        .all(|location| matches!(location, SidecarLocation::Backup | SidecarLocation::Absent));
    let all_sidecars_at_source = [wal_location, shm_location]
        .into_iter()
        .all(|location| matches!(location, SidecarLocation::Root | SidecarLocation::Absent));

    match (
        database_is_before,
        all_sidecars_backed,
        all_sidecars_at_source,
    ) {
        (true, _, true) => Ok(SqliteRecoveryState::Prepared),
        (true, _, false) => Ok(SqliteRecoveryState::CommitInterrupted),
        (false, true, _) => Ok(SqliteRecoveryState::Committed),
        (false, false, _) => Ok(SqliteRecoveryState::CommitInterrupted),
    }
}

/// 按固定顺序移走原 WAL/SHM，再将同目录 pending 原子替换数据库；未知提交结果必须回读。
///
/// @author kongweiguang
pub(crate) fn apply(
    root: &Path,
    operation_dir: &Path,
    entry: &SqliteJournalEntry,
) -> AppResult<()> {
    match inspect_entry(root, operation_dir, entry)? {
        SqliteRecoveryState::Committed => return verify_committed(root, operation_dir, entry),
        SqliteRecoveryState::Prepared | SqliteRecoveryState::CommitInterrupted => {}
    }

    let database = root.join(DATABASE_RELATIVE_PATH);
    let (_, staged_database) = private_database_paths(operation_dir);
    let (before_wal, before_shm) = private_sidecar_paths(operation_dir);
    move_original_sidecar(
        &sidecar_path(&database, "-wal"),
        &before_wal,
        entry.source_wal_sha256.as_deref(),
    )?;
    move_original_sidecar(
        &sidecar_path(&database, "-shm"),
        &before_shm,
        entry.source_shm_sha256.as_deref(),
    )?;

    let pending = pending_database_path(root, operation_dir)?;
    match inspect_entry(root, operation_dir, entry)? {
        SqliteRecoveryState::Committed => return verify_committed(root, operation_dir, entry),
        SqliteRecoveryState::Prepared | SqliteRecoveryState::CommitInterrupted => {}
    }

    let current_database_hash = hash_required_regular_file(&database)?;
    if current_database_hash != entry.source_database_sha256 {
        return Err(AppError::conflict(
            "命令数据库在替换前发生变化，迁移恢复已阻断",
        ));
    }
    match optional_regular_file_hash(&pending)? {
        Some(hash) if hash == entry.staged_database_sha256 => {}
        None => {
            let stage_hash = hash_required_regular_file(&staged_database)?;
            if stage_hash != entry.staged_database_sha256 {
                return Err(AppError::conflict("SQLite 迁移 staging 校验失败"));
            }
            commit_file(&staged_database, &pending, CommitMode::MoveStagedNoReplace)?;
        }
        Some(_) => {
            return Err(AppError::conflict(
                "SQLite 原子替换 pending 已被其它内容占用，已保留现场",
            ));
        }
    }

    // 从 private stage 移到同卷同目录 pending 后复核全部 CAS 输入，再调用 ReplaceExisting。
    validate_pending_replace(root, operation_dir, entry)?;
    match commit_staged_file(&pending, &database, CommitMode::ReplaceExisting) {
        Ok(()) => verify_committed(root, operation_dir, entry),
        Err(AtomicCommitError::NotCommitted(error)) => Err(AppError::infrastructure_with_source(
            "命令数据库原子替换未完成",
            error,
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => {
            if verify_committed(root, operation_dir, entry).is_ok() {
                Ok(())
            } else {
                Err(AppError::outcome_unknown_with_source(
                    "命令数据库原子替换结果未知，恢复材料已保留",
                    error,
                ))
            }
        }
    }
}

/// ReplaceExisting 前重新核对 main、pending、stage 与 WAL/SHM 的完整 CAS 布局。
///
/// 复核紧邻原子替换以缩短外部进程并发修改的窗口；不声称能约束不遵守存储锁的写入者。
///
/// @author kongweiguang
fn validate_pending_replace(
    root: &Path,
    operation_dir: &Path,
    entry: &SqliteJournalEntry,
) -> AppResult<()> {
    let _recovery_state = inspect_entry(root, operation_dir, entry)?;
    let database = root.join(DATABASE_RELATIVE_PATH);
    if hash_required_regular_file(&database)? != entry.source_database_sha256 {
        return Err(AppError::conflict(
            "命令数据库已不处于迁移前状态，原子替换已阻断",
        ));
    }

    let pending = pending_database_path(root, operation_dir)?;
    if hash_required_regular_file(&pending)? != entry.staged_database_sha256 {
        return Err(AppError::conflict(
            "SQLite 原子替换 pending 校验失败，已保留现场",
        ));
    }
    let (_, staged_database) = private_database_paths(operation_dir);
    if optional_regular_file_hash(&staged_database)?.is_some() {
        return Err(AppError::conflict(
            "SQLite 原子替换前 private stage 未清空，已保留现场",
        ));
    }

    let (before_wal, before_shm) = private_sidecar_paths(operation_dir);
    for (suffix, backup, expected_hash) in [
        ("-wal", before_wal, entry.source_wal_sha256.as_deref()),
        ("-shm", before_shm, entry.source_shm_sha256.as_deref()),
    ] {
        let location = inspect_sidecar(&sidecar_path(&database, suffix), &backup, expected_hash)?;
        if !matches!(location, SidecarLocation::Backup | SidecarLocation::Absent) {
            return Err(AppError::conflict(
                "原始 WAL/SHM 尚未移入校验过的备份，原子替换已阻断",
            ));
        }
    }
    Ok(())
}

/// marker 提交前再次确认数据库已是 stage 且原 sidecar 均只存在于校验过的 before 备份。
///
/// @author kongweiguang
pub(crate) fn verify_committed(
    root: &Path,
    operation_dir: &Path,
    entry: &SqliteJournalEntry,
) -> AppResult<()> {
    if inspect_entry(root, operation_dir, entry)? == SqliteRecoveryState::Committed {
        Ok(())
    } else {
        Err(AppError::conflict(
            "命令数据库迁移尚未完成，不能提交格式标记",
        ))
    }
}

/// 用 SQLite online backup 固定源事务快照；不能用 main 文件的裸复制充当 before。
///
/// @author kongweiguang
fn create_consistent_before(source_path: &Path, destination: &Path) -> AppResult<(u32, String)> {
    let mut source = Connection::open_with_flags(source_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(sqlite_error("读取命令数据库快照失败"))?;
    source
        .busy_timeout(SQLITE_BUSY_TIMEOUT)
        .map_err(sqlite_error("读取命令数据库快照失败"))?;
    let snapshot = source
        .transaction()
        .map_err(sqlite_error("读取命令数据库快照失败"))?;
    let schema_version = snapshot
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(sqlite_error("读取命令数据库 schema 失败"))?;
    let schema_version = u32::try_from(schema_version)
        .map_err(|_| AppError::conflict("命令数据库 schema 版本无效"))?;
    if schema_version > 3 {
        return Err(AppError::unsupported("命令数据库 schema 版本高于当前版本"));
    }

    let mut target =
        Connection::open(destination).map_err(sqlite_error("创建命令数据库一致性备份失败"))?;
    {
        let backup = Backup::new(&snapshot, &mut target)
            .map_err(sqlite_error("创建命令数据库一致性备份失败"))?;
        backup
            .run_to_completion(128, Duration::from_millis(20), None)
            .map_err(sqlite_error("创建命令数据库一致性备份失败"))?;
    }
    target
        .pragma_update(None, "journal_mode", "DELETE")
        .map_err(sqlite_error("封存命令数据库一致性备份失败"))?;
    target
        .close()
        .map_err(|(_, error)| sqlite_error("封存命令数据库一致性备份失败")(error))?;
    snapshot
        .commit()
        .map_err(sqlite_error("关闭命令数据库快照失败"))?;
    source
        .close()
        .map_err(|(_, error)| sqlite_error("关闭命令数据库快照失败")(error))?;

    let backup_hash = hash_required_regular_file(destination)?;
    Ok((schema_version, backup_hash))
}

/// 验证迁移函数没有跨越固定 0..2→3 的数据契约。
///
/// @author kongweiguang
fn validate_migration_result(
    expected_source_version: u32,
    result: CommandHistoryMigrationResult,
) -> AppResult<()> {
    if result.source_schema_version != expected_source_version || result.target_schema_version != 3
    {
        return Err(AppError::conflict("命令数据库迁移结果与预检版本不一致"));
    }
    Ok(())
}

/// 恢复检查只按固定 before 路径验证摘要和源 schema，不打开或改写用户数据库。
///
/// @author kongweiguang
fn validate_before_snapshot(operation_dir: &Path, entry: &SqliteJournalEntry) -> AppResult<()> {
    let (before_database, _) = private_database_paths(operation_dir);
    if hash_required_regular_file(&before_database)? != entry.consistent_before_sha256 {
        return Err(AppError::conflict(
            "命令数据库一致性 before 备份缺失或校验失败，已保留现场",
        ));
    }
    Ok(())
}

/// 按 journal 精确识别 sidecar 在用户根或 before 中的位置；丢失/第三值都不推测恢复。
///
/// @author kongweiguang
fn inspect_sidecar(
    root_path: &Path,
    backup_path: &Path,
    expected_hash: Option<&str>,
) -> AppResult<SidecarLocation> {
    let root_hash = optional_regular_file_hash(root_path)?;
    let backup_hash = optional_regular_file_hash(backup_path)?;
    match (expected_hash, root_hash.as_deref(), backup_hash.as_deref()) {
        (None, None, None) => Ok(SidecarLocation::Absent),
        (Some(expected), Some(root), None) if expected == root => Ok(SidecarLocation::Root),
        (Some(expected), None, Some(backup)) if expected == backup => Ok(SidecarLocation::Backup),
        _ => Err(AppError::conflict(
            "命令数据库 WAL/SHM 与迁移日志不匹配，已保留恢复材料",
        )),
    }
}

/// 将一个原 sidecar 原子移入 before；遇到未知提交结果时只接受回读证明已移动。
///
/// @author kongweiguang
fn move_original_sidecar(
    root_path: &Path,
    backup_path: &Path,
    expected_hash: Option<&str>,
) -> AppResult<()> {
    match inspect_sidecar(root_path, backup_path, expected_hash)? {
        SidecarLocation::Backup | SidecarLocation::Absent => Ok(()),
        SidecarLocation::Root => {
            match commit_staged_file(root_path, backup_path, CommitMode::MoveNoReplace) {
                Ok(()) => Ok(()),
                Err(AtomicCommitError::NotCommitted(error)) => {
                    match inspect_sidecar(root_path, backup_path, expected_hash) {
                        Ok(SidecarLocation::Backup) => Ok(()),
                        _ => Err(AppError::infrastructure_with_source(
                            "命令数据库 sidecar 备份未完成",
                            error,
                        )),
                    }
                }
                Err(AtomicCommitError::CommitUnknown(error)) => {
                    match inspect_sidecar(root_path, backup_path, expected_hash) {
                        Ok(SidecarLocation::Backup) => Ok(()),
                        _ => Err(AppError::outcome_unknown_with_source(
                            "命令数据库 sidecar 备份结果未知，恢复材料已保留",
                            error,
                        )),
                    }
                }
            }
        }
    }
}

/// 调用统一的原子提交适配器，并保留可区分的“未提交/结果未知”语义。
///
/// @author kongweiguang
fn commit_file(source: &Path, destination: &Path, mode: CommitMode) -> AppResult<()> {
    match commit_staged_file(source, destination, mode) {
        Ok(()) => Ok(()),
        Err(AtomicCommitError::NotCommitted(error)) => Err(AppError::infrastructure_with_source(
            "命令数据库原子 pending 发布失败",
            error,
        )),
        Err(AtomicCommitError::CommitUnknown(error)) => Err(AppError::outcome_unknown_with_source(
            "命令数据库原子 pending 发布结果未知，恢复材料已保留",
            error,
        )),
    }
}

/// 捕获 main/WAL 原值；SHM 的读标记可由只读事务更新，因此只在连接全部关闭后冻结。
///
/// @author kongweiguang
fn source_fingerprint(root: &Path) -> AppResult<Option<SourceFingerprint>> {
    if !validate_data_directory(root, false)? {
        return Ok(None);
    }
    let database = root.join(DATABASE_RELATIVE_PATH);
    let Some(database_sha256) = optional_regular_file_hash(&database)? else {
        return Ok(None);
    };
    Ok(Some(SourceFingerprint {
        database_sha256,
        wal_sha256: optional_regular_file_hash(&sidecar_path(&database, "-wal"))?,
    }))
}

/// 确认 operation_dir 是根目录下由核心创建的 UUID 目录，避免 pending 文件名受外部路径影响。
///
/// @author kongweiguang
fn validate_operation_directory(root: &Path, operation_dir: &Path) -> AppResult<()> {
    let operation_name = operation_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::invalid("SQLite 迁移 operation 目录无效"))?;
    let parsed = uuid::Uuid::parse_str(operation_name)
        .map_err(|_| AppError::invalid("SQLite 迁移 operation 目录无效"))?;
    if parsed.hyphenated().to_string() != operation_name
        || operation_dir.parent().and_then(Path::file_name)
            != Some(std::ffi::OsStr::new(".kerminal-migration"))
        || operation_dir.parent().and_then(Path::parent) != Some(root)
    {
        return Err(AppError::invalid("SQLite 迁移 operation 目录无效"));
    }
    ensure_existing_directory(root)?;
    ensure_existing_directory(&root.join(".kerminal-migration"))?;
    ensure_existing_directory(operation_dir)
}

/// 只在新 operation 私有空间创建目录，已有链接或非目录一律阻断。
///
/// @author kongweiguang
fn ensure_private_layout(operation_dir: &Path) -> AppResult<()> {
    let sqlite_directory = operation_dir.join(SQLITE_PRIVATE_DIRECTORY);
    ensure_directory(&sqlite_directory)?;
    ensure_directory(&sqlite_directory.join(BEFORE_DIRECTORY))?;
    ensure_directory(
        &sqlite_directory
            .join(BEFORE_DIRECTORY)
            .join(RAW_SIDECARS_DIRECTORY),
    )?;
    ensure_directory(&sqlite_directory.join(STAGED_DIRECTORY))
}

/// 恢复时只接受已存在的普通私有目录，避免读取链接目标或悄悄重建丢失证据。
///
/// @author kongweiguang
fn validate_private_layout(operation_dir: &Path) -> AppResult<()> {
    let sqlite_directory = operation_dir.join(SQLITE_PRIVATE_DIRECTORY);
    ensure_existing_directory(&sqlite_directory)?;
    ensure_existing_directory(&sqlite_directory.join(BEFORE_DIRECTORY))?;
    ensure_existing_directory(
        &sqlite_directory
            .join(BEFORE_DIRECTORY)
            .join(RAW_SIDECARS_DIRECTORY),
    )?;
    ensure_existing_directory(&sqlite_directory.join(STAGED_DIRECTORY))
}

/// 验证 root/data 为普通目录；prepare 可接受缺失目录，提交恢复必须要求目录存在。
///
/// @author kongweiguang
fn validate_data_directory(root: &Path, required: bool) -> AppResult<bool> {
    ensure_existing_directory(root)?;
    let data_directory = root.join("data");
    match fs::symlink_metadata(&data_directory) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(true),
        Ok(_) => Err(AppError::conflict("命令数据库目录不是安全目录，已保留现场")),
        Err(error) if error.kind() == io::ErrorKind::NotFound && !required => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(AppError::conflict("命令数据库目录缺失，迁移恢复已阻断"))
        }
        Err(error) => Err(io_error("检查命令数据库目录失败")(error)),
    }
}

/// 确保路径是普通目录；不跟随符号链接，避免迁移副本逃逸 operation 目录。
///
/// @author kongweiguang
fn ensure_directory(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(()),
        Ok(_) => Err(AppError::conflict("SQLite 迁移目录不是安全目录")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(io_error("创建 SQLite 迁移私有目录失败"))
        }
        Err(error) => Err(io_error("检查 SQLite 迁移私有目录失败")(error)),
    }
}

/// 检查 operation 路径本身及其固定父目录，恢复时只读取、不隐式创建缺失状态。
///
/// @author kongweiguang
fn ensure_existing_directory(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_symlink() && metadata.is_dir() => Ok(()),
        Ok(_) => Err(AppError::conflict("SQLite 迁移目录不是安全目录")),
        Err(error) => Err(io_error("SQLite 迁移目录不可用")(error)),
    }
}

/// 生成仅由固定文件名组成的 private before/staged 路径。
///
/// @author kongweiguang
fn private_database_paths(operation_dir: &Path) -> (PathBuf, PathBuf) {
    let sqlite_directory = operation_dir.join(SQLITE_PRIVATE_DIRECTORY);
    (
        sqlite_directory
            .join(BEFORE_DIRECTORY)
            .join(DATABASE_FILE_NAME),
        sqlite_directory
            .join(STAGED_DIRECTORY)
            .join(DATABASE_FILE_NAME),
    )
}

/// 将源 WAL/SHM 放入与 before 数据库不同目录，避免 SQLite 自动把原始 sidecar 关联到一致性快照。
///
/// @author kongweiguang
fn private_sidecar_paths(operation_dir: &Path) -> (PathBuf, PathBuf) {
    let raw_sidecars = operation_dir
        .join(SQLITE_PRIVATE_DIRECTORY)
        .join(BEFORE_DIRECTORY)
        .join(RAW_SIDECARS_DIRECTORY);
    (
        sidecar_path(&raw_sidecars.join(DATABASE_FILE_NAME), "-wal"),
        sidecar_path(&raw_sidecars.join(DATABASE_FILE_NAME), "-shm"),
    )
}

/// 为同卷 ReplaceExisting 生成每个 operation 唯一的 pending 路径，且绝不覆盖既有文件。
///
/// @author kongweiguang
fn pending_database_path(root: &Path, operation_dir: &Path) -> AppResult<PathBuf> {
    let operation_name = operation_dir
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| AppError::invalid("SQLite 迁移 operation 目录无效"))?;
    Ok(root.join("data").join(format!(
        ".command.sqlite-migration-{operation_name}.pending"
    )))
}

/// 以 sibling 后缀组合固定 sidecar 路径，不解析数据库内容或跟随链接。
///
/// @author kongweiguang
fn sidecar_path(database: &Path, suffix: &str) -> PathBuf {
    let mut path = database.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

/// 冲突时不清空文件；调用方只允许针对本次刚创建的私有文件调用。
///
/// @author kongweiguang
fn ensure_absent(path: &Path) -> AppResult<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("检查 SQLite 迁移文件失败")(error)),
        Ok(_) => Err(AppError::conflict("SQLite 迁移私有文件已存在，已保留现场")),
    }
}

/// 读取普通文件并计算 SHA-256；链接、目录与特殊文件都不参与迁移恢复。
///
/// @author kongweiguang
fn optional_regular_file_hash(path: &Path) -> AppResult<Option<String>> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("读取 SQLite 迁移文件失败")(error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(AppError::conflict(
            "SQLite 迁移路径不是普通文件，已保留现场",
        ));
    }
    hash_file(path).map(Some)
}

/// 对必须存在的普通文件计算摘要，缺失按恢复材料损坏处理。
///
/// @author kongweiguang
fn hash_required_regular_file(path: &Path) -> AppResult<String> {
    optional_regular_file_hash(path)?
        .ok_or_else(|| AppError::conflict("SQLite 迁移所需文件缺失，已保留现场"))
}

/// 以固定缓冲区流式计算文件摘要，避免大型命令历史数据库一次性读入内存。
///
/// @author kongweiguang
fn hash_file(path: &Path) -> AppResult<String> {
    let mut file = File::open(path).map_err(io_error("读取 SQLite 迁移文件失败"))?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(io_error("读取 SQLite 迁移文件失败"))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// 拒绝大写、非十六进制或非 64 字符的日志 hash，保持序列化值规范唯一。
///
/// @author kongweiguang
fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 构造统一的脱敏基础设施错误，同时保留内部 I/O 原因。
///
/// @author kongweiguang
fn io_error(context: &'static str) -> impl FnOnce(io::Error) -> AppError {
    move |error| AppError::infrastructure_with_source(context, error)
}

/// 构造统一的脱敏 SQLite 错误，同时保留内部 rusqlite 原因。
///
/// @author kongweiguang
fn sqlite_error(context: &'static str) -> impl FnOnce(rusqlite::Error) -> AppError {
    move |error| AppError::infrastructure_with_source(context, error)
}

/// 迁移失败后清理本次私有副本，清理故障只留在结构化诊断而不替换原错误。
///
/// @author kongweiguang
fn cleanup_private_database(database: &Path) -> AppResult<()> {
    let mut first_error = None;
    for path in [
        database.to_path_buf(),
        sidecar_path(database, "-wal"),
        sidecar_path(database, "-shm"),
    ] {
        if let Err(error) = fs::remove_file(&path)
            && error.kind() != io::ErrorKind::NotFound
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    match first_error {
        Some(error) => Err(io_error("清理 SQLite 迁移私有副本失败")(error)),
        None => Ok(()),
    }
}

/// 失败路径不覆盖主要错误；清理失败会给出不含文件名的诊断事件。
///
/// @author kongweiguang
fn cleanup_private_database_best_effort(database: &Path) {
    if let Err(error) = cleanup_private_database(database) {
        tracing::warn!(
            event = "sqlite_migration_private_cleanup_failed",
            error_code = error.public_code().as_str(),
        );
    }
}

#[cfg(test)]
#[path = "../../tests/support/data_migration_sqlite_unit.rs"]
mod tests;
