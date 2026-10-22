//! 命令历史 application port 的 SQLite adapter。
//!
//! @author kongweiguang

use std::{io, path::Path, sync::Arc};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        CommandHistoryAvailability, CommandHistoryClearRequest, CommandHistoryDraft,
        CommandHistoryRepository, CommandHistoryUnavailableReason,
    },
};
use kerminal_domain::{
    commands::{
        CommandHistoryEntry, CommandHistoryListRequest, CommandHistorySource, CommandHistoryTarget,
        NormalizedCommandHistory, validate_history_context,
    },
    identity::{HostId, PaneId, SessionId, TabId},
};
use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};
use tokio::task;
use uuid::Uuid;

use crate::storage::StorageCoordinator;

use super::{CommandHistoryInitError, SqliteCommandHistoryInner, open_inner};

const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

/// 多 async 用例共享一条串行化 SQLite connection，事务边界由 repository 显式管理。
#[derive(Clone)]
pub struct SqliteCommandHistoryRepository {
    pub(super) inner: Option<Arc<SqliteCommandHistoryInner>>,
    pub(super) availability: CommandHistoryAvailability,
    database_file: std::path::PathBuf,
}

#[async_trait]
impl CommandHistoryRepository for SqliteCommandHistoryRepository {
    /// 返回启动时锁定的 feature 状态，让 UI 区分空数据库与不可用历史。
    fn availability(&self) -> CommandHistoryAvailability {
        self.availability
    }

    /// 按过滤条件查询最多 500 项，稳定按创建时间和 rowid 倒序。
    async fn list(
        &self,
        request: CommandHistoryListRequest,
    ) -> AppResult<Vec<CommandHistoryEntry>> {
        let repository = self.clone();
        task::spawn_blocking(move || repository.list_sync(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("查询命令历史任务失败", source)
            })?
    }

    /// 持久化前再次规范化并过滤敏感命令，保证直接调用 port 也不会绕过安全规则。
    async fn append(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        let repository = self.clone();
        task::spawn_blocking(move || repository.append_sync(draft))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("写入命令历史任务失败", source)
            })?
    }

    /// 按稳定 id 幂等删除一条记录。
    async fn delete(&self, entry_id: String) -> AppResult<bool> {
        let repository = self.clone();
        task::spawn_blocking(move || repository.delete_sync(&entry_id))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("删除命令历史任务失败", source)
            })?
    }

    /// 清理明确会话/pane/host 范围；空筛选保留既有全部清空语义。
    async fn clear(&self, request: CommandHistoryClearRequest) -> AppResult<u64> {
        let repository = self.clone();
        task::spawn_blocking(move || repository.clear_sync(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("清理命令历史任务失败", source)
            })?
    }
}

impl SqliteCommandHistoryRepository {
    /// 返回专用数据库路径供 diagnostics 使用；该访问器放在字段定义模块，避免扩大字段可见性。
    pub fn database_file(&self) -> &Path {
        &self.database_file
    }

    /// 接收共享配置存储协调器，确保迁移、备份与其它持久化事务共用文件锁。
    pub fn open(storage: StorageCoordinator) -> Result<Self, CommandHistoryInitError> {
        let database_file = storage.root().join("data").join("command.sqlite");
        let inner = open_inner(storage)?;
        Ok(Self {
            inner: Some(inner),
            availability: CommandHistoryAvailability::Available,
            database_file,
        })
    }

    /// 初始化失败时保留 typed unavailable 状态，使终端与其它桌面能力仍可运行。
    pub fn open_or_unavailable(storage: StorageCoordinator) -> Self {
        let database_file = storage.root().join("data").join("command.sqlite");
        match Self::open(storage) {
            Ok(repository) => repository,
            Err(error) => Self {
                inner: None,
                availability: CommandHistoryAvailability::Unavailable {
                    reason: super::unavailable_reason(&error),
                },
                database_file,
            },
        }
    }

    /// 将不可用状态映射为稳定错误，任何历史操作都不能把失败伪装为空结果。
    pub(super) fn require_inner(&self) -> AppResult<&Arc<SqliteCommandHistoryInner>> {
        match (self.availability, self.inner.as_ref()) {
            (CommandHistoryAvailability::Available, Some(inner)) => Ok(inner),
            (
                CommandHistoryAvailability::Unavailable {
                    reason:
                        CommandHistoryUnavailableReason::CorruptDatabase
                        | CommandHistoryUnavailableReason::UnsupportedSchemaVersion(_),
                },
                _,
            ) => Err(AppError::configuration_corrupt(
                "命令历史不可用，需要检查或恢复",
            )),
            (
                CommandHistoryAvailability::Unavailable {
                    reason: CommandHistoryUnavailableReason::MigrationRequired(_),
                },
                _,
            ) => Err(AppError::infrastructure("命令历史需要先执行0.4数据迁移")),
            _ => Err(AppError::infrastructure("命令历史当前不可用")),
        }
    }

    /// 执行参数化查询，转义 LIKE 元字符并将 SQL 解析限制在稳定字段集合内。
    fn list_sync(&self, request: CommandHistoryListRequest) -> AppResult<Vec<CommandHistoryEntry>> {
        let query = request
            .query
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_lowercase)
            .map(|value| like_contains_pattern(&value));
        let limit = request.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
        let connection = self.lock_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, command, source, target, session_id, pane_id, tab_id, profile_id, remote_host_id, cwd, shell, created_at
                 FROM command_history
                 WHERE (?1 IS NULL OR source = ?1)
                   AND (?2 IS NULL OR target = ?2)
                   AND (?3 IS NULL OR pane_id = ?3)
                   AND (?4 IS NULL OR remote_host_id = ?4)
                   AND (?5 IS NULL OR session_id = ?5)
                   AND (?6 IS NULL OR lower(command) LIKE ?6 ESCAPE '\\'
                     OR lower(coalesce(cwd, '')) LIKE ?6 ESCAPE '\\'
                     OR lower(coalesce(shell, '')) LIKE ?6 ESCAPE '\\'
                     OR lower(coalesce(remote_host_id, '')) LIKE ?6 ESCAPE '\\'
                     OR lower(coalesce(pane_id, '')) LIKE ?6 ESCAPE '\\')
                 ORDER BY created_at DESC, rowid DESC LIMIT ?7",
            )
            .map_err(|source| map_sqlite_error("准备命令历史查询失败", source))?;
        let rows = statement
            .query_map(
                params![
                    request.source.map(source_as_str),
                    request.target.map(target_as_str),
                    request.pane_id.as_ref().map(PaneId::as_str),
                    request.remote_host_id.as_ref().map(HostId::as_str),
                    request.session_id.as_ref().map(SessionId::as_str),
                    query,
                    limit,
                ],
                history_from_row,
            )
            .map_err(|source| map_sqlite_error("查询命令历史失败", source))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|source| map_sqlite_error("读取命令历史结果失败", source))
    }

    /// 在一个 SQLite 事务内插入记录并读取数据库生成的时间戳。
    fn append_sync(&self, draft: CommandHistoryDraft) -> AppResult<CommandHistoryEntry> {
        let normalized = NormalizedCommandHistory::new(&draft.command)
            .map_err(|_| AppError::invalid("历史命令为空或超过长度限制"))?;
        if normalized.sensitive_skip_reason().is_some() {
            return Err(AppError::invalid("敏感命令不会写入历史"));
        }
        validate_history_context(
            draft.cwd.as_deref(),
            draft.shell.as_deref(),
            &[
                draft.session_id.as_ref().map(SessionId::as_str),
                draft.pane_id.as_ref().map(PaneId::as_str),
                draft.tab_id.as_ref().map(TabId::as_str),
                draft.profile_id.as_deref(),
                draft.remote_host_id.as_ref().map(HostId::as_str),
            ],
        )
        .map_err(|_| AppError::invalid("历史上下文超过长度限制"))?;

        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let entry_id = Uuid::new_v4().to_string();
        let transaction = connection
            .transaction()
            .map_err(|source| map_sqlite_error("开始命令历史事务失败", source))?;
        transaction
            .execute(
                "INSERT INTO command_history (id, command, source, target, session_id, pane_id, tab_id, profile_id, remote_host_id, cwd, shell)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    entry_id,
                    normalized.command,
                    source_as_str(draft.source),
                    target_as_str(draft.target),
                    draft.session_id.as_ref().map(SessionId::as_str),
                    draft.pane_id.as_ref().map(PaneId::as_str),
                    draft.tab_id.as_ref().map(TabId::as_str),
                    draft.profile_id,
                    draft.remote_host_id.as_ref().map(HostId::as_str),
                    draft.cwd,
                    draft.shell,
                ],
            )
            .map_err(|source| map_sqlite_error("写入命令历史失败", source))?;
        let entry = query_history_by_id(&transaction, &entry_id)
            .map_err(|source| map_sqlite_error("读取新命令历史失败", source))?
            .ok_or_else(|| AppError::infrastructure("新建命令历史记录丢失"))?;
        transaction
            .commit()
            .map_err(|source| map_sqlite_error("提交命令历史事务失败", source))?;
        Ok(entry)
    }

    /// 删除单行时也用共享存储锁，防止工作区同步读取到事务中间状态。
    fn delete_sync(&self, entry_id: &str) -> AppResult<bool> {
        if entry_id.is_empty()
            || entry_id.chars().count() > 160
            || entry_id.chars().any(char::is_control)
        {
            return Err(AppError::invalid("命令历史 id 无效"));
        }
        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        connection
            .execute("DELETE FROM command_history WHERE id = ?1", [entry_id])
            .map(|affected| affected > 0)
            .map_err(|source| map_sqlite_error("删除命令历史失败", source))
    }

    /// 使用参数化谓词按类型化上下文清理，不接受任意 SQL 或字符串拼接条件。
    fn clear_sync(&self, request: CommandHistoryClearRequest) -> AppResult<u64> {
        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let affected = connection
            .execute(
                "DELETE FROM command_history WHERE (?1 IS NULL OR target = ?1)
                   AND (?2 IS NULL OR session_id = ?2)
                   AND (?3 IS NULL OR pane_id = ?3) AND (?4 IS NULL OR remote_host_id = ?4)",
                params![
                    request.target.map(target_as_str),
                    request.session_id.as_ref().map(SessionId::as_str),
                    request.pane_id.as_ref().map(PaneId::as_str),
                    request.remote_host_id.as_ref().map(HostId::as_str),
                ],
            )
            .map_err(|source| map_sqlite_error("清理命令历史失败", source))?;
        u64::try_from(affected).map_err(|_| AppError::infrastructure("命令历史删除计数溢出"))
    }

    /// 统一处理 mutex poison，避免直接 panic 破坏桌面应用其它会话。
    fn lock_connection(&self) -> AppResult<std::sync::MutexGuard<'_, Connection>> {
        self.require_inner()?
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))
    }
}

/// 根据历史 id 查询已提交或当前事务中的完整领域 entry。
fn query_history_by_id(
    connection: &Connection,
    entry_id: &str,
) -> Result<Option<CommandHistoryEntry>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT id, command, source, target, session_id, pane_id, tab_id, profile_id, remote_host_id, cwd, shell, created_at
             FROM command_history WHERE id = ?1",
            [entry_id],
            history_from_row,
        )
        .optional()
}

/// 将 SQLite row 中的旧稳定字符串转换为领域新类型。
fn history_from_row(row: &Row<'_>) -> Result<CommandHistoryEntry, rusqlite::Error> {
    let source_text: String = row.get(2)?;
    let target_text: String = row.get(3)?;
    let source = parse_source(&source_text).ok_or_else(|| invalid_column_value(2))?;
    let target = parse_target(&target_text).ok_or_else(|| invalid_column_value(3))?;
    let session_id = row
        .get::<_, Option<String>>(4)?
        .map(|value| SessionId::parse(&value).map_err(|_| invalid_column_value(4)))
        .transpose()?;
    let pane_id = row
        .get::<_, Option<String>>(5)?
        .map(|value| PaneId::parse(&value).map_err(|_| invalid_column_value(5)))
        .transpose()?;
    let tab_id = row
        .get::<_, Option<String>>(6)?
        .map(|value| TabId::parse(&value).map_err(|_| invalid_column_value(6)))
        .transpose()?;
    let remote_host_id = row
        .get::<_, Option<String>>(8)?
        .map(|value| HostId::parse(&value).map_err(|_| invalid_column_value(8)))
        .transpose()?;
    Ok(CommandHistoryEntry {
        id: row.get(0)?,
        command: row.get(1)?,
        source,
        target,
        session_id,
        pane_id,
        tab_id,
        profile_id: row.get(7)?,
        remote_host_id,
        cwd: row.get(9)?,
        shell: row.get(10)?,
        created_at: row.get(11)?,
    })
}

/// 构造不包含数据库原始字段值的 SQLite 转换错误。
fn invalid_column_value(index: usize) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Text,
        Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid persisted enum or identity",
        )),
    )
}

/// 将用户输入来源映射为 v1 数据库中保存的稳定枚举文本。
const fn source_as_str(source: CommandHistorySource) -> &'static str {
    match source {
        CommandHistorySource::User => "user",
        CommandHistorySource::Snippet => "snippet",
        CommandHistorySource::Workflow => "workflow",
        CommandHistorySource::Broadcast => "broadcast",
        CommandHistorySource::Tool => "tool",
    }
}

/// 解析旧 SQLite 中的 source 值，未知值不能强行归入 user。
fn parse_source(source: &str) -> Option<CommandHistorySource> {
    match source {
        "user" => Some(CommandHistorySource::User),
        "snippet" => Some(CommandHistorySource::Snippet),
        "workflow" => Some(CommandHistorySource::Workflow),
        "broadcast" => Some(CommandHistorySource::Broadcast),
        "tool" => Some(CommandHistorySource::Tool),
        _ => None,
    }
}

/// 将领域 Container target 映射为旧版 `dockerContainer` 数据库字符串。
const fn target_as_str(target: CommandHistoryTarget) -> &'static str {
    match target {
        CommandHistoryTarget::Local => "local",
        CommandHistoryTarget::Ssh => "ssh",
        CommandHistoryTarget::Telnet => "telnet",
        CommandHistoryTarget::Serial => "serial",
        CommandHistoryTarget::Container => "dockerContainer",
    }
}

/// 解析旧 SQLite target 值，保留 dockerContainer 的现有 schema 拼写。
fn parse_target(target: &str) -> Option<CommandHistoryTarget> {
    match target {
        "local" => Some(CommandHistoryTarget::Local),
        "ssh" => Some(CommandHistoryTarget::Ssh),
        "telnet" => Some(CommandHistoryTarget::Telnet),
        "serial" => Some(CommandHistoryTarget::Serial),
        "dockerContainer" => Some(CommandHistoryTarget::Container),
        _ => None,
    }
}

/// 按 contains 语义构建 SQL LIKE 模式并转义用户输入的通配符。
fn like_contains_pattern(query: &str) -> String {
    let mut pattern = String::with_capacity(query.len() + 2);
    pattern.push('%');
    for character in query.chars() {
        if matches!(character, '\\' | '%' | '_') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern.push('%');
    pattern
}

/// 将 SQLite 错误映射为不回显 SQL 参数或命令正文的公开错误。
fn map_sqlite_error(context: &'static str, error: rusqlite::Error) -> AppError {
    AppError::infrastructure_with_source(context, error)
}
