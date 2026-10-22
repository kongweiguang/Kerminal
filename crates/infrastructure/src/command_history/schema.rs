//! 当前 command.sqlite schema；旧格式仅由独立迁移入口处理。
//!
//! @author kongweiguang

use rusqlite::Connection;

use super::error::CommandHistoryInitError;

pub(super) const CURRENT_SCHEMA_VERSION: u32 = 3;

/// 新库直接建立当前完整约束，避免业务启动再执行历史版本的重建与数据复制。
///
/// @author kongweiguang
pub(super) fn initialize_current(conn: &mut Connection) -> Result<(), CommandHistoryInitError> {
    let tx = conn.transaction()?;
    tx.execute_batch(CURRENT_SCHEMA)?;
    tx.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION)?;
    validate_current_structure(&tx)?;
    tx.commit()?;
    Ok(())
}

/// 版本与实际结构必须同时匹配；标记为当前版的残缺库不能进入可写启动流程。
///
/// @author kongweiguang
pub(super) fn require_current(conn: &Connection) -> Result<(), CommandHistoryInitError> {
    let version = schema_version(conn)?;
    if version > CURRENT_SCHEMA_VERSION {
        return Err(CommandHistoryInitError::UnsupportedSchemaVersion(version));
    }
    if version < CURRENT_SCHEMA_VERSION {
        return Err(CommandHistoryInitError::MigrationRequired(version));
    }
    validate_current_structure(conn)
}

/// 通过 SQLite 元数据核对读写所需列、主键和索引；历史排序依赖 rowid，不能接受 WITHOUT ROWID。
/// 允许无关表与额外列，不比较 DDL 文本。
///
/// @author kongweiguang
pub(super) fn validate_current_structure(conn: &Connection) -> Result<(), CommandHistoryInitError> {
    let mut columns = conn.prepare("SELECT name FROM pragma_table_info(?1)")?;
    for (table, required) in REQUIRED_COLUMNS {
        let is_table: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_list WHERE schema='main' AND type='table'
             AND name=?1 COLLATE NOCASE AND wr=0)",
            [table],
            |row| row.get(0),
        )?;
        let actual = columns
            .query_map([table], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        if !is_table
            || required
                .iter()
                .any(|column| !actual.iter().any(|name| name.eq_ignore_ascii_case(column)))
        {
            return Err(CommandHistoryInitError::InvalidSchemaStructure);
        }
    }
    let indexes_match: bool = conn.query_row(REQUIRED_INDEXES, [], |row| row.get(0))?;
    if !indexes_match {
        return Err(CommandHistoryInitError::InvalidSchemaStructure);
    }
    Ok(())
}

/// 读取文件自身的版本标记；负值不能被转换成受支持的历史格式。
///
/// @author kongweiguang
pub(super) fn schema_version(conn: &Connection) -> Result<u32, CommandHistoryInitError> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    u32::try_from(version).map_err(|_| CommandHistoryInitError::InvalidSchemaVersion)
}

const REQUIRED_COLUMNS: &[(&str, &[&str])] = &[
    (
        "command_history",
        &[
            "id",
            "command",
            "source",
            "target",
            "session_id",
            "pane_id",
            "tab_id",
            "profile_id",
            "remote_host_id",
            "cwd",
            "shell",
            "created_at",
        ],
    ),
    (
        "command_suggestion_provider_cache",
        &[
            "provider",
            "host_id",
            "scope_key",
            "repo_root",
            "payload_json",
            "cached_at_unix_ms",
            "expires_at_unix_ms",
            "ttl_seconds",
            "updated_at",
        ],
    ),
    (
        "command_suggestion_feedback",
        &[
            "id",
            "action",
            "provider",
            "target",
            "replacement_text",
            "input",
            "source_id",
            "session_id",
            "pane_id",
            "profile_id",
            "remote_host_id",
            "cwd",
            "shell",
            "created_at_unix_ms",
            "created_at",
        ],
    ),
    (
        "command_suggestion_telemetry",
        &[
            "provider",
            "query_count",
            "candidate_count",
            "total_elapsed_ms",
            "cache_hit_count",
            "cache_miss_count",
            "refresh_success_count",
            "refresh_failure_count",
            "feedback_accepted_count",
            "feedback_dismissed_count",
            "feedback_skipped_count",
            "first_event_unix_ms",
            "last_event_unix_ms",
            "last_error",
            "updated_at",
        ],
    ),
    (
        "command_suggestion_audit_events",
        &[
            "id",
            "event_kind",
            "provider",
            "target",
            "decision",
            "reason",
            "remote_host_id",
            "cwd",
            "path",
            "pane_id",
            "session_id",
            "metadata_json",
            "created_at_unix_ms",
            "created_at",
        ],
    ),
    (
        "snippet_preferences",
        &[
            "origin",
            "snippet_id",
            "favorite",
            "use_count",
            "last_action",
            "last_used_at_unix_ms",
            "updated_at",
        ],
    ),
    (
        "snippet_usage_receipts",
        &[
            "receipt_id",
            "origin",
            "snippet_id",
            "action",
            "created_at_unix_ms",
            "created_at",
        ],
    ),
];

const REQUIRED_INDEXES: &str = r#"
WITH required_index(table_name, index_name, keys) AS (VALUES
    ('command_history', 'idx_command_history_created_at', 'created_at:1'),
    ('command_history', 'idx_command_history_target_created_at', 'target:0,created_at:1'),
    ('command_history', 'idx_command_history_remote_host', 'remote_host_id:0,created_at:1'),
    ('command_history', 'idx_command_history_session', 'session_id:0,created_at:1'),
    ('command_history', 'idx_command_history_target_command_created', 'target:0,command:0,created_at:1'),
    ('command_history', 'idx_command_history_target_host_command_created', 'target:0,remote_host_id:0,command:0,created_at:1'),
    ('command_history', 'idx_command_history_target_recent', 'target:0,created_at:1'),
    ('command_history', 'idx_command_history_target_host_recent', 'target:0,remote_host_id:0,created_at:1'),
    ('command_suggestion_provider_cache', 'idx_command_suggestion_provider_cache_expires', 'provider:0,expires_at_unix_ms:0'),
    ('command_suggestion_provider_cache', 'idx_command_suggestion_provider_cache_host', 'host_id:0,provider:0'),
    ('command_suggestion_feedback', 'idx_command_suggestion_feedback_replacement', 'provider:0,replacement_text:0,created_at_unix_ms:1'),
    ('command_suggestion_feedback', 'idx_command_suggestion_feedback_remote_host', 'remote_host_id:0,provider:0,created_at_unix_ms:1'),
    ('command_suggestion_telemetry', 'idx_command_suggestion_telemetry_last_event', 'last_event_unix_ms:1'),
    ('command_suggestion_audit_events', 'idx_command_suggestion_audit_created', 'created_at_unix_ms:1'),
    ('command_suggestion_audit_events', 'idx_command_suggestion_audit_remote_host', 'remote_host_id:0,created_at_unix_ms:1'),
    ('command_suggestion_audit_events', 'idx_command_suggestion_audit_kind_provider', 'event_kind:0,provider:0,created_at_unix_ms:1'),
    ('snippet_preferences', 'idx_snippet_preferences_recent', 'last_used_at_unix_ms:1'),
    ('snippet_preferences', 'idx_snippet_preferences_favorite', 'favorite:1,origin:0,snippet_id:0'),
    ('snippet_usage_receipts', 'idx_snippet_usage_receipts_created', 'created_at_unix_ms:1')
), required_primary_key(table_name, keys) AS (VALUES
    ('command_history', 'id'),
    ('command_suggestion_provider_cache', 'provider,host_id,scope_key'),
    ('command_suggestion_feedback', 'id'),
    ('command_suggestion_telemetry', 'provider'),
    ('command_suggestion_audit_events', 'id'),
    ('snippet_preferences', 'origin,snippet_id'),
    ('snippet_usage_receipts', 'receipt_id')
)
SELECT NOT EXISTS (
    SELECT 1 FROM required_index r
    WHERE NOT EXISTS (
        SELECT 1 FROM pragma_index_list(r.table_name) i
        WHERE i.name=r.index_name COLLATE NOCASE AND i.partial=0 AND i."unique"=0
    ) OR r.keys IS NOT (
        SELECT group_concat(lower(name) || ':' || "desc", ',') FROM (
            SELECT name, "desc" FROM pragma_index_xinfo(r.index_name) WHERE key=1 ORDER BY seqno
        )
    )
) AND NOT EXISTS (
    SELECT 1 FROM required_primary_key r
    WHERE r.keys IS NOT (
        SELECT group_concat(lower(name), ',') FROM (
            SELECT name FROM pragma_table_info(r.table_name) WHERE pk>0 ORDER BY pk
        )
    )
);
"#;

const CURRENT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS command_history (
    id TEXT PRIMARY KEY NOT NULL,
    command TEXT NOT NULL,
    source TEXT NOT NULL DEFAULT 'user' CHECK (source IN ('user','snippet','workflow','broadcast','tool')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local','ssh','telnet','serial','dockerContainer')),
    session_id TEXT,
    pane_id TEXT,
    tab_id TEXT,
    profile_id TEXT,
    remote_host_id TEXT,
    cwd TEXT,
    shell TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_command_history_created_at ON command_history(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_target_created_at ON command_history(target, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_remote_host ON command_history(remote_host_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_session ON command_history(session_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_target_command_created ON command_history(target, command, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_target_host_command_created ON command_history(target, remote_host_id, command, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_target_recent ON command_history(target, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_command_history_target_host_recent ON command_history(target, remote_host_id, created_at DESC);

CREATE TABLE IF NOT EXISTS command_suggestion_provider_cache (
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    host_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    repo_root TEXT,
    payload_json TEXT NOT NULL,
    cached_at_unix_ms INTEGER NOT NULL,
    expires_at_unix_ms INTEGER NOT NULL,
    ttl_seconds INTEGER NOT NULL CHECK (ttl_seconds >= 1),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (provider, host_id, scope_key)
);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_provider_cache_expires ON command_suggestion_provider_cache(provider, expires_at_unix_ms);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_provider_cache_host ON command_suggestion_provider_cache(host_id, provider);

CREATE TABLE IF NOT EXISTS command_suggestion_feedback (
    id TEXT PRIMARY KEY NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('accepted','dismissed')),
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local','ssh','telnet','serial','dockerContainer')),
    replacement_text TEXT NOT NULL,
    input TEXT NOT NULL,
    source_id TEXT,
    session_id TEXT,
    pane_id TEXT,
    profile_id TEXT,
    remote_host_id TEXT,
    cwd TEXT,
    shell TEXT,
    created_at_unix_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_feedback_replacement ON command_suggestion_feedback(provider, replacement_text, created_at_unix_ms DESC);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_feedback_remote_host ON command_suggestion_feedback(remote_host_id, provider, created_at_unix_ms DESC);

CREATE TABLE IF NOT EXISTS command_suggestion_telemetry (
    provider TEXT PRIMARY KEY NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    query_count INTEGER NOT NULL DEFAULT 0 CHECK (query_count >= 0),
    candidate_count INTEGER NOT NULL DEFAULT 0 CHECK (candidate_count >= 0),
    total_elapsed_ms INTEGER NOT NULL DEFAULT 0 CHECK (total_elapsed_ms >= 0),
    cache_hit_count INTEGER NOT NULL DEFAULT 0 CHECK (cache_hit_count >= 0),
    cache_miss_count INTEGER NOT NULL DEFAULT 0 CHECK (cache_miss_count >= 0),
    refresh_success_count INTEGER NOT NULL DEFAULT 0 CHECK (refresh_success_count >= 0),
    refresh_failure_count INTEGER NOT NULL DEFAULT 0 CHECK (refresh_failure_count >= 0),
    feedback_accepted_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_accepted_count >= 0),
    feedback_dismissed_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_dismissed_count >= 0),
    feedback_skipped_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_skipped_count >= 0),
    first_event_unix_ms INTEGER NOT NULL,
    last_event_unix_ms INTEGER NOT NULL,
    last_error TEXT,
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_telemetry_last_event ON command_suggestion_telemetry(last_event_unix_ms DESC);

CREATE TABLE IF NOT EXISTS command_suggestion_audit_events (
    id TEXT PRIMARY KEY NOT NULL,
    event_kind TEXT NOT NULL CHECK (event_kind IN ('remoteProbeSchedule','remoteProbeRefresh','feedback')),
    provider TEXT CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local','ssh','telnet','serial','dockerContainer')),
    decision TEXT NOT NULL CHECK (decision IN ('allowed','skipped','succeeded','failed','recorded')),
    reason TEXT,
    remote_host_id TEXT,
    cwd TEXT,
    path TEXT,
    pane_id TEXT,
    session_id TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}',
    created_at_unix_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_audit_created ON command_suggestion_audit_events(created_at_unix_ms DESC);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_audit_remote_host ON command_suggestion_audit_events(remote_host_id, created_at_unix_ms DESC);
CREATE INDEX IF NOT EXISTS idx_command_suggestion_audit_kind_provider ON command_suggestion_audit_events(event_kind, provider, created_at_unix_ms DESC);

CREATE TABLE IF NOT EXISTS snippet_preferences (
    origin TEXT NOT NULL CHECK (origin IN ('user','builtin')),
    snippet_id TEXT NOT NULL,
    favorite INTEGER NOT NULL DEFAULT 0 CHECK (favorite IN (0,1)),
    use_count INTEGER NOT NULL DEFAULT 0 CHECK (use_count >= 0),
    last_action TEXT CHECK (last_action IN ('insert','run','copyRendered')),
    last_used_at_unix_ms INTEGER,
    updated_at TEXT NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (origin, snippet_id)
);
CREATE INDEX IF NOT EXISTS idx_snippet_preferences_recent ON snippet_preferences(last_used_at_unix_ms DESC);
CREATE INDEX IF NOT EXISTS idx_snippet_preferences_favorite ON snippet_preferences(favorite DESC, origin, snippet_id);

CREATE TABLE IF NOT EXISTS snippet_usage_receipts (
    receipt_id TEXT PRIMARY KEY NOT NULL,
    origin TEXT NOT NULL CHECK (origin IN ('user','builtin')),
    snippet_id TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('insert','run','copyRendered')),
    created_at_unix_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX IF NOT EXISTS idx_snippet_usage_receipts_created ON snippet_usage_receipts(created_at_unix_ms DESC);
"#;
