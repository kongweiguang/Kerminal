//! SQLite command history 初始化与备份错误。
//!
//! @author kongweiguang

use std::{error::Error, fmt, io};

/// 初始化错误公开 schema 版本但不包含命令正文、路径或 SQL 参数。
#[derive(Debug)]
pub enum CommandHistoryInitError {
    Io(io::Error),
    Sqlite(rusqlite::Error),
    BackupFailed(Box<CommandHistoryInitError>),
    MigrationRequired(u32),
    UnsupportedSchemaVersion(u32),
    InvalidSchemaVersion,
    InvalidSchemaStructure,
    IntegrityCheckFailed,
    InvalidBackupPath,
    StorageLockUnavailable,
}

impl fmt::Display for CommandHistoryInitError {
    /// 给启动恢复界面稳定摘要，底层数据库原因保留给 source 链。
    ///
    /// @author kongweiguang
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(_) => formatter.write_str("command history storage I/O failed"),
            Self::Sqlite(_) => formatter.write_str("command history database failed"),
            Self::BackupFailed(_) => formatter.write_str("command history migration backup failed"),
            Self::MigrationRequired(version) => write!(
                formatter,
                "command history schema version {version} requires explicit migration"
            ),
            Self::UnsupportedSchemaVersion(version) => {
                write!(
                    formatter,
                    "unsupported command history schema version {version}"
                )
            }
            Self::InvalidSchemaVersion => {
                formatter.write_str("command history schema version is invalid")
            }
            Self::InvalidSchemaStructure => {
                formatter.write_str("command history schema structure is invalid")
            }
            Self::IntegrityCheckFailed => {
                formatter.write_str("command history database integrity check failed")
            }
            Self::InvalidBackupPath => {
                formatter.write_str("command history backup path is invalid")
            }
            Self::StorageLockUnavailable => {
                formatter.write_str("command history storage lock is unavailable")
            }
        }
    }
}

impl Error for CommandHistoryInitError {
    /// 保留 SQLite/IO 来源以供本机诊断；公开摘要不含数据库行内容。
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::BackupFailed(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<io::Error> for CommandHistoryInitError {
    /// 将文件系统故障统一到不暴露实际 command data 的初始化错误。
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for CommandHistoryInitError {
    /// 归一化 rusqlite 初始化错误，调用方可通过 source 追踪故障类型。
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
