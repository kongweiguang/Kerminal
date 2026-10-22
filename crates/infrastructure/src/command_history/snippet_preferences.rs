//! 片段收藏与使用回执适配到既有 command.sqlite v3 表。
//!
//! @author kongweiguang

use std::{
    collections::HashSet,
    io,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use kerminal_application::{
    AppError, AppResult,
    commands::{
        CommandHistoryAvailability, CommandHistoryRepository, CommandHistoryUnavailableReason,
        SnippetPreferenceAvailability, SnippetPreferenceRepository,
        SnippetPreferenceUnavailableReason,
    },
};
use kerminal_domain::commands::{
    SnippetCatalogOrigin, SnippetPreference, SnippetPreferenceKey, SnippetUsageAction,
    SnippetUsageClearResult, SnippetUsageReceipt, SnippetUsageRecordRequest,
};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params, types::Type};

use super::SqliteCommandHistoryRepository;

const MAX_PREFERENCE_KEYS: usize = 500;

#[async_trait]
impl SnippetPreferenceRepository for SqliteCommandHistoryRepository {
    /// 偏好查询使用同一数据库连接，并在线程池执行，避免同步 SQLite 阻塞 UI executor。
    async fn list(&self, keys: Vec<SnippetPreferenceKey>) -> AppResult<Vec<SnippetPreference>> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.list_snippet_preferences_sync(keys))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段偏好查询任务未完成", source)
            })?
    }

    /// 收藏只更新 favorite 列，保留并发使用统计并在事务提交后返回真实数据库投影。
    async fn set_favorite(
        &self,
        key: SnippetPreferenceKey,
        favorite: bool,
    ) -> AppResult<SnippetPreference> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.set_snippet_favorite_sync(key, favorite))
            .await
            .map_err(|source| AppError::infrastructure_with_source("片段收藏任务未完成", source))?
    }

    /// 使用稳定 receipt 在一个事务内写回执和统计，取消后的同 ID 重试不会重复计数。
    async fn record_usage(
        &self,
        request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.record_snippet_usage_sync(request))
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段使用记录任务未完成", source)
            })?
    }

    /// 同一即时事务清空所有使用回执并重置统计；favorite 列不参与 UPDATE。
    async fn clear_usage(&self) -> AppResult<SnippetUsageClearResult> {
        let repository = self.clone();
        tokio::task::spawn_blocking(move || repository.clear_snippet_usage_sync())
            .await
            .map_err(|source| {
                AppError::infrastructure_with_source("片段使用清理任务未完成", source)
            })?
    }

    /// 偏好状态沿用 command.sqlite 的启动诊断，不把未来 schema 或损坏库降级为空列表。
    fn availability(&self) -> SnippetPreferenceAvailability {
        let availability = CommandHistoryRepository::availability(self);
        match availability {
            CommandHistoryAvailability::Available => SnippetPreferenceAvailability::Available,
            CommandHistoryAvailability::Unavailable { reason } => {
                SnippetPreferenceAvailability::Unavailable {
                    reason: map_unavailable_reason(reason),
                }
            }
        }
    }
}

impl SqliteCommandHistoryRepository {
    /// 只按已过滤目录页的稳定 key 读取最多 500 行，缺失行仍表示默认偏好。
    fn list_snippet_preferences_sync(
        &self,
        keys: Vec<SnippetPreferenceKey>,
    ) -> AppResult<Vec<SnippetPreference>> {
        if keys.len() > MAX_PREFERENCE_KEYS {
            return Err(AppError::invalid("单次片段偏好查询数量超限"));
        }
        for key in &keys {
            validate_key(key)?;
        }
        if keys.is_empty() {
            self.require_inner()?;
            return Ok(Vec::new());
        }
        let mut seen = HashSet::with_capacity(keys.len());

        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let mut statement = connection
            .prepare(
                "SELECT origin, snippet_id, favorite, use_count, last_action, last_used_at_unix_ms
                 FROM snippet_preferences WHERE origin = ?1 AND snippet_id = ?2",
            )
            .map_err(|source| sqlite_error("准备片段偏好查询失败", source))?;
        let mut preferences = Vec::with_capacity(keys.len());
        for key in keys {
            let identity = (key.origin, key.snippet_id.clone());
            if !seen.insert(identity) {
                continue;
            }
            let preference = statement
                .query_row(
                    params![origin_as_str(key.origin), key.snippet_id],
                    preference_from_row,
                )
                .optional()
                .map_err(|source| sqlite_error("读取片段偏好失败", source))?;
            if let Some(preference) = preference {
                preferences.push(preference);
            }
        }
        Ok(preferences)
    }

    /// 收藏写入通过即时事务串行化，并只在提交错误时按目标 favorite 回读确认语义结果。
    fn set_snippet_favorite_sync(
        &self,
        key: SnippetPreferenceKey,
        favorite: bool,
    ) -> AppResult<SnippetPreference> {
        validate_key(&key)?;
        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("开始片段收藏事务失败", source))?;
        transaction
            .execute(
                "INSERT INTO snippet_preferences (origin, snippet_id, favorite)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(origin, snippet_id) DO UPDATE SET
                    favorite = excluded.favorite,
                    updated_at = datetime('now')",
                params![origin_as_str(key.origin), key.snippet_id, favorite as i64],
            )
            .map_err(|source| sqlite_error("保存片段收藏失败", source))?;
        let preference = query_preference(&transaction, &key)
            .map_err(|source| sqlite_error("读取片段收藏结果失败", source))?
            .ok_or_else(|| AppError::infrastructure("片段收藏写入后未找到偏好行"))?;

        if let Err(commit_error) = transaction.commit() {
            return match query_preference(&connection, &key) {
                Ok(Some(current)) if current.favorite == favorite => Ok(current),
                Ok(_) => Err(sqlite_error("提交片段收藏失败", commit_error)),
                Err(read_error) => Err(AppError::outcome_unknown_with_source(
                    "片段收藏提交结果未知，请重新读取目录确认",
                    read_error,
                )),
            };
        }
        Ok(preference)
    }

    /// 回执与计数使用一笔即时事务；重复 ID 仅在来源、片段和动作完全一致时幂等返回。
    fn record_snippet_usage_sync(
        &self,
        request: SnippetUsageRecordRequest,
    ) -> AppResult<SnippetUsageReceipt> {
        validate_key(&request.key)?;
        validate_receipt_id(&request.receipt_id)?;
        let now = unix_timestamp_millis()?;
        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("开始片段使用事务失败", source))?;

        let inserted = transaction
            .execute(
                "INSERT INTO snippet_usage_receipts (
                    receipt_id, origin, snippet_id, action, created_at_unix_ms
                 ) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(receipt_id) DO NOTHING",
                params![
                    request.receipt_id,
                    origin_as_str(request.key.origin),
                    request.key.snippet_id,
                    action_as_str(request.action),
                    now,
                ],
            )
            .map_err(|source| sqlite_error("写入片段使用回执失败", source))?;

        if inserted == 0 {
            let existing = query_receipt(&transaction, &request.receipt_id)
                .map_err(|source| sqlite_error("读取已有片段使用回执失败", source))?
                .ok_or_else(|| AppError::infrastructure("重复片段回执在事务中消失"))?;
            if existing.key == request.key && existing.action == request.action {
                return Ok(existing);
            }
            return Err(AppError::conflict(
                "片段使用回执标识已用于另一项操作，原统计未改变",
            ));
        }

        transaction
            .execute(
                "INSERT INTO snippet_preferences (
                    origin, snippet_id, use_count, last_action, last_used_at_unix_ms
                 ) VALUES (?1, ?2, 1, ?3, ?4)
                 ON CONFLICT(origin, snippet_id) DO UPDATE SET
                    use_count = snippet_preferences.use_count + 1,
                    last_action = excluded.last_action,
                    last_used_at_unix_ms = excluded.last_used_at_unix_ms,
                    updated_at = datetime('now')",
                params![
                    origin_as_str(request.key.origin),
                    request.key.snippet_id,
                    action_as_str(request.action),
                    now,
                ],
            )
            .map_err(|source| sqlite_error("更新片段使用统计失败", source))?;

        let receipt = SnippetUsageReceipt {
            receipt_id: request.receipt_id.clone(),
            key: request.key.clone(),
            action: request.action,
            occurred_at_unix_ms: now,
        };
        if let Err(commit_error) = transaction.commit() {
            return match query_receipt(&connection, &request.receipt_id) {
                Ok(Some(current))
                    if current.key == request.key && current.action == request.action =>
                {
                    Ok(current)
                }
                Ok(Some(_)) => Err(AppError::conflict(
                    "片段使用回执标识已用于另一项操作，原统计未改变",
                )),
                Ok(None) => Err(sqlite_error("提交片段使用事务失败", commit_error)),
                Err(read_error) => Err(AppError::outcome_unknown_with_source(
                    "片段使用事务提交结果未知，请用相同回执标识重新读取",
                    read_error,
                )),
            };
        }
        Ok(receipt)
    }

    /// 使用统计清理原子删除回执并重置活跃字段，保留所有偏好行及 favorite 值。
    fn clear_snippet_usage_sync(&self) -> AppResult<SnippetUsageClearResult> {
        let inner = self.require_inner()?;
        let _storage_guard = inner
            .storage
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库存储锁不可用"))?;
        let mut connection = inner
            .connection
            .lock()
            .map_err(|_| AppError::infrastructure("命令数据库连接状态异常"))?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| sqlite_error("开始片段统计清理事务失败", source))?;
        let receipts_removed = transaction
            .execute("DELETE FROM snippet_usage_receipts", [])
            .map_err(|source| sqlite_error("清理片段使用回执失败", source))?;
        let preferences_reset = transaction
            .execute(
                "UPDATE snippet_preferences SET use_count = 0, last_action = NULL,
                    last_used_at_unix_ms = NULL, updated_at = datetime('now')
                 WHERE use_count <> 0 OR last_action IS NOT NULL OR last_used_at_unix_ms IS NOT NULL",
                [],
            )
            .map_err(|source| sqlite_error("重置片段使用统计失败", source))?;
        let result = SnippetUsageClearResult {
            receipts_removed: u64::try_from(receipts_removed)
                .map_err(|_| AppError::infrastructure("片段回执删除计数超出范围"))?,
            preferences_reset: u64::try_from(preferences_reset)
                .map_err(|_| AppError::infrastructure("片段偏好重置计数超出范围"))?,
        };

        if let Err(commit_error) = transaction.commit() {
            return match usage_state_is_clear(&connection) {
                Ok(true) => Ok(result),
                Ok(false) => Err(sqlite_error("提交片段统计清理事务失败", commit_error)),
                Err(read_error) => Err(AppError::outcome_unknown_with_source(
                    "片段统计清理结果未知，请重新读取后再操作",
                    read_error,
                )),
            };
        }
        Ok(result)
    }
}

/// 将历史库的不可用类别映射到偏好端口，保持本地终端可用且不隐藏故障原因。
fn map_unavailable_reason(
    reason: CommandHistoryUnavailableReason,
) -> SnippetPreferenceUnavailableReason {
    match reason {
        CommandHistoryUnavailableReason::MigrationRequired(version) => {
            SnippetPreferenceUnavailableReason::MigrationRequired(version)
        }
        CommandHistoryUnavailableReason::UnsupportedSchemaVersion(version) => {
            SnippetPreferenceUnavailableReason::UnsupportedSchemaVersion(version)
        }
        CommandHistoryUnavailableReason::CorruptDatabase => {
            SnippetPreferenceUnavailableReason::CorruptDatabase
        }
        CommandHistoryUnavailableReason::StorageUnavailable => {
            SnippetPreferenceUnavailableReason::StorageUnavailable
        }
        CommandHistoryUnavailableReason::BackupFailed => {
            SnippetPreferenceUnavailableReason::BackupFailed
        }
        CommandHistoryUnavailableReason::StorageLockUnavailable => {
            SnippetPreferenceUnavailableReason::StorageLockUnavailable
        }
    }
}

/// 校验目录 identity 的可移植文件名子集，SQLite 与 TOML 仓储使用相同边界。
fn validate_key(key: &SnippetPreferenceKey) -> AppResult<()> {
    let id = key.snippet_id.as_str();
    if id.is_empty()
        || id.len() > 160
        || matches!(id, "." | "..")
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(AppError::invalid("片段偏好标识无效"));
    }
    Ok(())
}

/// receipt 由用例分配但仍在 adapter 复核，避免无界或控制字符进入主键。
fn validate_receipt_id(receipt_id: &str) -> AppResult<()> {
    if receipt_id.is_empty()
        || receipt_id.chars().count() > 160
        || receipt_id.chars().any(char::is_control)
    {
        return Err(AppError::invalid("片段使用回执标识无效"));
    }
    Ok(())
}

/// 从偏好行映射稳定领域 DTO，未知枚举或异常数值作为数据库损坏而不是默认值。
fn preference_from_row(row: &Row<'_>) -> Result<SnippetPreference, rusqlite::Error> {
    let origin_text: String = row.get(0)?;
    let origin = parse_origin(&origin_text).ok_or_else(|| invalid_text_column(0))?;
    let snippet_id: String = row.get(1)?;
    let favorite_value: i64 = row.get(2)?;
    let favorite = match favorite_value {
        0 => false,
        1 => true,
        _ => return Err(invalid_integer_column(2)),
    };
    let use_count: i64 = row.get(3)?;
    let use_count = u64::try_from(use_count).map_err(|_| invalid_integer_column(3))?;
    let action = row
        .get::<_, Option<String>>(4)?
        .map(|value| parse_action(&value).ok_or_else(|| invalid_text_column(4)))
        .transpose()?;
    Ok(SnippetPreference {
        key: SnippetPreferenceKey { origin, snippet_id },
        favorite,
        use_count,
        last_action: action,
        last_used_at_unix_ms: row.get(5)?,
    })
}

/// 从 receipt 表读取已提交操作，用于稳定重试及提交错误后的只读确认。
fn query_receipt(
    connection: &Connection,
    receipt_id: &str,
) -> Result<Option<SnippetUsageReceipt>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT receipt_id, origin, snippet_id, action, created_at_unix_ms
             FROM snippet_usage_receipts WHERE receipt_id = ?1",
            [receipt_id],
            |row| {
                let origin_text: String = row.get(1)?;
                let action_text: String = row.get(3)?;
                Ok(SnippetUsageReceipt {
                    receipt_id: row.get(0)?,
                    key: SnippetPreferenceKey {
                        origin: parse_origin(&origin_text).ok_or_else(|| invalid_text_column(1))?,
                        snippet_id: row.get(2)?,
                    },
                    action: parse_action(&action_text).ok_or_else(|| invalid_text_column(3))?,
                    occurred_at_unix_ms: row.get(4)?,
                })
            },
        )
        .optional()
}

/// 同一查询用于事务内返回收藏结果和提交失败后的只读回读。
fn query_preference(
    connection: &Connection,
    key: &SnippetPreferenceKey,
) -> Result<Option<SnippetPreference>, rusqlite::Error> {
    connection
        .query_row(
            "SELECT origin, snippet_id, favorite, use_count, last_action, last_used_at_unix_ms
             FROM snippet_preferences WHERE origin = ?1 AND snippet_id = ?2",
            params![origin_as_str(key.origin), key.snippet_id],
            preference_from_row,
        )
        .optional()
}

/// 确认 clear 提交后的可观察状态，无法证明时调用方返回 OutcomeUnknown。
fn usage_state_is_clear(connection: &Connection) -> Result<bool, rusqlite::Error> {
    let remaining_receipts: i64 =
        connection.query_row("SELECT count(*) FROM snippet_usage_receipts", [], |row| {
            row.get(0)
        })?;
    if remaining_receipts != 0 {
        return Ok(false);
    }
    connection.query_row(
        "SELECT NOT EXISTS (
            SELECT 1 FROM snippet_preferences
            WHERE use_count <> 0 OR last_action IS NOT NULL OR last_used_at_unix_ms IS NOT NULL
         )",
        [],
        |row| row.get(0),
    )
}

/// 数据库固定文本值显式映射，避免 serde 拼写或 variant 顺序成为持久化契约。
const fn origin_as_str(origin: SnippetCatalogOrigin) -> &'static str {
    match origin {
        SnippetCatalogOrigin::User => "user",
        SnippetCatalogOrigin::Builtin => "builtin",
    }
}

/// 将 v3 存储值解析为已知来源，未知值不能归并到用户片段。
fn parse_origin(value: &str) -> Option<SnippetCatalogOrigin> {
    match value {
        "user" => Some(SnippetCatalogOrigin::User),
        "builtin" => Some(SnippetCatalogOrigin::Builtin),
        _ => None,
    }
}

/// v3 固定使用 `copyRendered`，不把领域 enum 的 serde 表示写入数据库。
const fn action_as_str(action: SnippetUsageAction) -> &'static str {
    match action {
        SnippetUsageAction::Insert => "insert",
        SnippetUsageAction::Run => "run",
        SnippetUsageAction::CopyRendered => "copyRendered",
    }
}

/// 读取旧数据库动作列，拒绝未知值以保持可见恢复而非错误投影。
fn parse_action(value: &str) -> Option<SnippetUsageAction> {
    match value {
        "insert" => Some(SnippetUsageAction::Insert),
        "run" => Some(SnippetUsageAction::Run),
        "copyRendered" => Some(SnippetUsageAction::CopyRendered),
        _ => None,
    }
}

/// 使用系统 UTC Unix 毫秒生成单调语义时间字段，时钟回拨或溢出均显式失败。
fn unix_timestamp_millis() -> AppResult<i64> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|source| {
            AppError::infrastructure_with_source("系统时间早于 Unix epoch", source)
        })?;
    i64::try_from(duration.as_millis())
        .map_err(|_| AppError::infrastructure("系统时间超出片段偏好范围"))
}

/// 为未知数据库文本值构造不回显行内容的转换错误。
fn invalid_text_column(index: usize) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Text,
        Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "unknown snippet preference value",
        )),
    )
}

/// 为超出 schema 约束的 SQLite 数值构造稳定转换错误。
fn invalid_integer_column(index: usize) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        Type::Integer,
        Box::new(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid snippet preference integer",
        )),
    )
}

/// SQLite 错误映射不包含 SQL 参数，避免错误文本泄漏本地目录或其它用户内容。
fn sqlite_error(context: &'static str, source: rusqlite::Error) -> AppError {
    AppError::infrastructure_with_source(context, source)
}
