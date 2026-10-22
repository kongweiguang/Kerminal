//! 显式一次性迁移旧 command.sqlite；业务仓储不调用此模块。
//!
//! @author kongweiguang

use std::{path::Path, time::Duration};

use rusqlite::{Connection, OpenFlags};

use super::{CommandHistoryInitError, backup, schema};

/// 仅返回格式版本，迁移报告不携带路径、命令或反馈正文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHistoryMigrationResult {
    pub source_schema_version: u32,
    pub target_schema_version: u32,
}

/// 调用方持有源数据根的 StorageCoordinator；此处不重入同一路径的根锁。
/// 源只读事务固定版本与 WAL 快照；当前标记也须匹配实际结构，副本校验完成后才提交升级。
/// 只读源的完整性检查不含 CHECK，必须先在可写 staging 验证，才允许发布目标。
/// 升级仅写入不存在的目标副本，失败副本留给调用方恢复。
///
/// @author kongweiguang
pub fn migrate_legacy_command_history(
    source: impl AsRef<Path>,
    destination: impl AsRef<Path>,
) -> Result<CommandHistoryMigrationResult, CommandHistoryInitError> {
    let mut source = Connection::open_with_flags(source, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    source.busy_timeout(Duration::from_secs(5))?;
    let snapshot = source.transaction()?;
    let source_schema_version = schema::schema_version(&snapshot)?;
    if source_schema_version > schema::CURRENT_SCHEMA_VERSION {
        return Err(CommandHistoryInitError::UnsupportedSchemaVersion(
            source_schema_version,
        ));
    }
    backup::check_integrity(&snapshot)?;
    if source_schema_version == schema::CURRENT_SCHEMA_VERSION {
        schema::validate_current_structure(&snapshot)?;
    }
    backup::create_migration_backup(&snapshot, destination.as_ref()).map_err(
        |error| match error {
            CommandHistoryInitError::IntegrityCheckFailed => error,
            _ => CommandHistoryInitError::BackupFailed(Box::new(error)),
        },
    )?;
    snapshot.commit()?;

    let mut target = Connection::open_with_flags(destination, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    target.busy_timeout(Duration::from_secs(5))?;
    let tx = target.transaction()?;
    if source_schema_version < 1 {
        tx.execute_batch(COMMAND_SCHEMA_V1)?;
    }
    if source_schema_version < 2 {
        tx.execute_batch(COMMAND_SUGGESTION_PROVIDER_V2)?;
    }
    if source_schema_version < 3 {
        tx.execute_batch(SNIPPET_PREFERENCES_V3)?;
    }
    tx.pragma_update(None, "user_version", schema::CURRENT_SCHEMA_VERSION)?;
    backup::check_integrity(&tx)?;
    schema::validate_current_structure(&tx)?;
    tx.commit()?;
    target.close().map_err(|(_, error)| error)?;
    Ok(CommandHistoryMigrationResult {
        source_schema_version,
        target_schema_version: schema::CURRENT_SCHEMA_VERSION,
    })
}

const COMMAND_SCHEMA_V1: &str = r#"
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
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git')),
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
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec')),
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
    provider TEXT PRIMARY KEY NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec')),
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
    provider TEXT CHECK (provider IN ('history','remotePath','remoteCommand','git','spec')),
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
"#;

const COMMAND_SUGGESTION_PROVIDER_V2: &str = r#"
CREATE TABLE command_suggestion_provider_cache_v2 (
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    host_id TEXT NOT NULL, scope_key TEXT NOT NULL, repo_root TEXT, payload_json TEXT NOT NULL,
    cached_at_unix_ms INTEGER NOT NULL, expires_at_unix_ms INTEGER NOT NULL,
    ttl_seconds INTEGER NOT NULL CHECK (ttl_seconds >= 1),
    updated_at TEXT NOT NULL DEFAULT (datetime('now')), PRIMARY KEY (provider, host_id, scope_key)
);
INSERT INTO command_suggestion_provider_cache_v2 SELECT * FROM command_suggestion_provider_cache;
DROP TABLE command_suggestion_provider_cache;
ALTER TABLE command_suggestion_provider_cache_v2 RENAME TO command_suggestion_provider_cache;
CREATE INDEX idx_command_suggestion_provider_cache_expires ON command_suggestion_provider_cache(provider, expires_at_unix_ms);
CREATE INDEX idx_command_suggestion_provider_cache_host ON command_suggestion_provider_cache(host_id, provider);

CREATE TABLE command_suggestion_feedback_v2 (
    id TEXT PRIMARY KEY NOT NULL, action TEXT NOT NULL CHECK (action IN ('accepted','dismissed')),
    provider TEXT NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local','ssh','telnet','serial','dockerContainer')),
    replacement_text TEXT NOT NULL, input TEXT NOT NULL, source_id TEXT, session_id TEXT, pane_id TEXT,
    profile_id TEXT, remote_host_id TEXT, cwd TEXT, shell TEXT, created_at_unix_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO command_suggestion_feedback_v2 SELECT * FROM command_suggestion_feedback;
DROP TABLE command_suggestion_feedback;
ALTER TABLE command_suggestion_feedback_v2 RENAME TO command_suggestion_feedback;
CREATE INDEX idx_command_suggestion_feedback_replacement ON command_suggestion_feedback(provider, replacement_text, created_at_unix_ms DESC);
CREATE INDEX idx_command_suggestion_feedback_remote_host ON command_suggestion_feedback(remote_host_id, provider, created_at_unix_ms DESC);

CREATE TABLE command_suggestion_telemetry_v2 (
    provider TEXT PRIMARY KEY NOT NULL CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    query_count INTEGER NOT NULL DEFAULT 0 CHECK (query_count >= 0), candidate_count INTEGER NOT NULL DEFAULT 0 CHECK (candidate_count >= 0),
    total_elapsed_ms INTEGER NOT NULL DEFAULT 0 CHECK (total_elapsed_ms >= 0), cache_hit_count INTEGER NOT NULL DEFAULT 0 CHECK (cache_hit_count >= 0),
    cache_miss_count INTEGER NOT NULL DEFAULT 0 CHECK (cache_miss_count >= 0), refresh_success_count INTEGER NOT NULL DEFAULT 0 CHECK (refresh_success_count >= 0),
    refresh_failure_count INTEGER NOT NULL DEFAULT 0 CHECK (refresh_failure_count >= 0), feedback_accepted_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_accepted_count >= 0),
    feedback_dismissed_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_dismissed_count >= 0), feedback_skipped_count INTEGER NOT NULL DEFAULT 0 CHECK (feedback_skipped_count >= 0),
    first_event_unix_ms INTEGER NOT NULL, last_event_unix_ms INTEGER NOT NULL, last_error TEXT, updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO command_suggestion_telemetry_v2 SELECT * FROM command_suggestion_telemetry;
DROP TABLE command_suggestion_telemetry;
ALTER TABLE command_suggestion_telemetry_v2 RENAME TO command_suggestion_telemetry;
CREATE INDEX idx_command_suggestion_telemetry_last_event ON command_suggestion_telemetry(last_event_unix_ms DESC);

CREATE TABLE command_suggestion_audit_events_v2 (
    id TEXT PRIMARY KEY NOT NULL, event_kind TEXT NOT NULL CHECK (event_kind IN ('remoteProbeSchedule','remoteProbeRefresh','feedback')),
    provider TEXT CHECK (provider IN ('history','remotePath','remoteCommand','git','spec','snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local','ssh','telnet','serial','dockerContainer')),
    decision TEXT NOT NULL CHECK (decision IN ('allowed','skipped','succeeded','failed','recorded')),
    reason TEXT, remote_host_id TEXT, cwd TEXT, path TEXT, pane_id TEXT, session_id TEXT,
    metadata_json TEXT NOT NULL DEFAULT '{}', created_at_unix_ms INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
INSERT INTO command_suggestion_audit_events_v2 SELECT * FROM command_suggestion_audit_events;
DROP TABLE command_suggestion_audit_events;
ALTER TABLE command_suggestion_audit_events_v2 RENAME TO command_suggestion_audit_events;
CREATE INDEX idx_command_suggestion_audit_created ON command_suggestion_audit_events(created_at_unix_ms DESC);
CREATE INDEX idx_command_suggestion_audit_remote_host ON command_suggestion_audit_events(remote_host_id, created_at_unix_ms DESC);
CREATE INDEX idx_command_suggestion_audit_kind_provider ON command_suggestion_audit_events(event_kind, provider, created_at_unix_ms DESC);
"#;

const SNIPPET_PREFERENCES_V3: &str = r#"
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
