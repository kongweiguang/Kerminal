//! 命令历史 SQLite repository 与一致性备份。
//!
//! @author kongweiguang

mod backup;
mod error;
mod legacy_migration;
mod repository;
mod schema;
mod snippet_preferences;
mod suggestions;

pub use error::CommandHistoryInitError;
pub use legacy_migration::{CommandHistoryMigrationResult, migrate_legacy_command_history};
pub use repository::SqliteCommandHistoryRepository;

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use kerminal_application::commands::CommandHistoryUnavailableReason;
use rusqlite::{Connection, OpenFlags};

use crate::storage::StorageCoordinator;

/// Repository 内部共享 SQLite connection 和配置根 coordinator。
pub(super) struct SqliteCommandHistoryInner {
    pub connection: Mutex<Connection>,
    pub storage: StorageCoordinator,
}

/// 旧库先只读预检，拒绝发生在可写连接、WAL 配置和任何备份之前；新库直接建立当前格式。
///
/// @author kongweiguang
pub(super) fn open_inner(
    storage: StorageCoordinator,
) -> Result<Arc<SqliteCommandHistoryInner>, CommandHistoryInitError> {
    let database_file = storage.root().join("data").join("command.sqlite");
    let _storage_guard = storage
        .lock()
        .map_err(|_| CommandHistoryInitError::StorageLockUnavailable)?;
    fs::create_dir_all(storage.root().join("data"))?;
    let connection = if database_file.try_exists()? {
        let preflight =
            Connection::open_with_flags(&database_file, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        schema::require_current(&preflight)?;
        drop(preflight);
        let connection =
            Connection::open_with_flags(&database_file, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        schema::require_current(&connection)?;
        connection
    } else {
        // 独占创建避免预检后出现的文件被误当新库初始化；失败保留证据供显式恢复。
        drop(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&database_file)?,
        );
        let mut connection =
            Connection::open_with_flags(&database_file, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        schema::initialize_current(&mut connection)?;
        connection
    };
    configure_connection(&connection)?;
    drop(_storage_guard);
    Ok(Arc::new(SqliteCommandHistoryInner {
        connection: Mutex::new(connection),
        storage,
    }))
}

/// 初始化失败只映射为不含路径、SQL 或命令内容的稳定 feature 状态。
///
/// @author kongweiguang
pub(super) fn unavailable_reason(
    error: &CommandHistoryInitError,
) -> CommandHistoryUnavailableReason {
    match error {
        CommandHistoryInitError::MigrationRequired(version) => {
            CommandHistoryUnavailableReason::MigrationRequired(*version)
        }
        CommandHistoryInitError::UnsupportedSchemaVersion(version) => {
            CommandHistoryUnavailableReason::UnsupportedSchemaVersion(*version)
        }
        CommandHistoryInitError::InvalidSchemaVersion
        | CommandHistoryInitError::InvalidSchemaStructure
        | CommandHistoryInitError::IntegrityCheckFailed => {
            CommandHistoryUnavailableReason::CorruptDatabase
        }
        CommandHistoryInitError::BackupFailed(_) | CommandHistoryInitError::InvalidBackupPath => {
            CommandHistoryUnavailableReason::BackupFailed
        }
        CommandHistoryInitError::StorageLockUnavailable => {
            CommandHistoryUnavailableReason::StorageLockUnavailable
        }
        CommandHistoryInitError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if matches!(
                failure.code,
                rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase
            ) =>
        {
            CommandHistoryUnavailableReason::CorruptDatabase
        }
        CommandHistoryInitError::Io(_) | CommandHistoryInitError::Sqlite(_) => {
            CommandHistoryUnavailableReason::StorageUnavailable
        }
    }
}

/// 将 SQLite 写入等待约束在五秒内，并启用 WAL 与外键校验。
fn configure_connection(connection: &Connection) -> Result<(), rusqlite::Error> {
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

impl SqliteCommandHistoryRepository {
    /// 返回当前 schema 版本，未来版本仍会在 open 阶段拒绝服务。
    pub fn schema_version(&self) -> Result<u32, CommandHistoryInitError> {
        let Some(inner) = self.inner.as_ref() else {
            return Err(CommandHistoryInitError::InvalidSchemaVersion);
        };
        let connection = inner
            .connection
            .lock()
            .map_err(|_| CommandHistoryInitError::InvalidSchemaVersion)?;
        schema::schema_version(&connection)
    }

    /// 用 SQLite online backup 生成包含 WAL 状态的一致性快照，目标不得已存在。
    pub fn create_consistent_backup(
        &self,
        destination: impl AsRef<Path>,
    ) -> kerminal_application::AppResult<()> {
        let inner = self.require_inner()?;
        let _storage_guard = inner.storage.lock().map_err(|_| {
            kerminal_application::AppError::infrastructure("命令数据库存储锁不可用")
        })?;
        let connection = inner.connection.lock().map_err(|_| {
            kerminal_application::AppError::infrastructure("命令数据库连接状态异常")
        })?;
        backup::create_backup(&connection, destination.as_ref()).map_err(|source| {
            kerminal_application::AppError::infrastructure_with_source(
                "创建命令数据库一致性备份失败",
                source,
            )
        })
    }
}
