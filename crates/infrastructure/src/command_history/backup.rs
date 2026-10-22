//! SQLite 在线一致性备份，读取事务页而非复制 WAL 主文件。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use rusqlite::{Connection, backup::Backup};
use uuid::Uuid;

use crate::storage::{AtomicCommitError, CommitMode, commit_staged_file};

use super::error::CommandHistoryInitError;

/// 普通手动备份保留原有复制语义；迁移的写前检查由独立入口显式选择。
///
/// @author kongweiguang
pub(super) fn create_backup(
    source: &Connection,
    destination: &Path,
) -> Result<(), CommandHistoryInitError> {
    create_backup_inner(source, destination, false)
}

/// SQLite 只读源不会加载 CHECK 表达式，必须在可写 staging 上检查后再发布迁移副本。
///
/// @author kongweiguang
pub(super) fn create_migration_backup(
    source: &Connection,
    destination: &Path,
) -> Result<(), CommandHistoryInitError> {
    create_backup_inner(source, destination, true)
}

/// 复用同一 staging、原子新建和清理边界；迁移校验失败不能留下已发布的目标。
///
/// @author kongweiguang
fn create_backup_inner(
    source: &Connection,
    destination: &Path,
    validate_migration: bool,
) -> Result<(), CommandHistoryInitError> {
    let parent = destination
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let _ = destination
        .file_name()
        .ok_or(CommandHistoryInitError::InvalidBackupPath)?;
    std::fs::create_dir_all(parent)?;
    match fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(CommandHistoryInitError::Io(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "backup destination already exists",
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CommandHistoryInitError::Io(error)),
    }
    let staged = staged_path(destination)?;
    let backup_result = (|| {
        let mut target = Connection::open(&staged)?;
        {
            let backup = Backup::new(source, &mut target)?;
            backup.run_to_completion(128, Duration::from_millis(20), None)?;
        }
        target.pragma_update(None, "journal_mode", "DELETE")?;
        if validate_migration {
            check_integrity(&target)?;
        }
        drop(target);
        match commit_staged_file(&staged, destination, CommitMode::CreateNew) {
            Ok(()) => Ok(()),
            Err(
                AtomicCommitError::NotCommitted(error) | AtomicCommitError::CommitUnknown(error),
            ) => Err(CommandHistoryInitError::Io(error)),
        }
    })();
    let cleanup_result = remove_staging_files(&staged);
    match (backup_result, cleanup_result) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// 只读源可检查页结构，但 CHECK 约束须在可写副本检查；错误摘要不回显 SQLite 检查正文。
///
/// @author kongweiguang
pub(super) fn check_integrity(connection: &Connection) -> Result<(), CommandHistoryInitError> {
    let result: String =
        connection.pragma_query_value(None, "integrity_check", |row| row.get(0))?;
    if result != "ok" {
        return Err(CommandHistoryInitError::IntegrityCheckFailed);
    }
    Ok(())
}

/// 将 staging 文件置于目标目录，保证 rename 与最终备份位于同一卷。
fn staged_path(destination: &Path) -> Result<PathBuf, CommandHistoryInitError> {
    let name = destination
        .file_name()
        .ok_or(CommandHistoryInitError::InvalidBackupPath)?
        .to_string_lossy();
    Ok(destination.with_file_name(format!(".{name}.pending-{}", Uuid::new_v4().simple())))
}

/// 删除失败或中止的 staging database 及 WAL sidecar，不遗留命令正文副本。
fn remove_staging_files(staged: &Path) -> Result<(), CommandHistoryInitError> {
    let mut first_error = None;
    for path in [
        staged.to_path_buf(),
        PathBuf::from(format!("{}-wal", staged.display())),
        PathBuf::from(format!("{}-shm", staged.display())),
    ] {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if first_error.is_none() => first_error = Some(error),
            Err(_) => {}
        }
    }
    first_error.map_or(Ok(()), |error| Err(CommandHistoryInitError::Io(error)))
}
