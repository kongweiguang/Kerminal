CREATE TABLE command_suggestion_provider_cache_v2 (
    provider TEXT NOT NULL CHECK (provider IN ('history', 'remotePath', 'remoteCommand', 'git', 'spec', 'snippet')),
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
    id TEXT PRIMARY KEY NOT NULL, action TEXT NOT NULL CHECK (action IN ('accepted', 'dismissed')),
    provider TEXT NOT NULL CHECK (provider IN ('history', 'remotePath', 'remoteCommand', 'git', 'spec', 'snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local', 'ssh', 'telnet', 'serial', 'dockerContainer')),
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
    provider TEXT PRIMARY KEY NOT NULL CHECK (provider IN ('history', 'remotePath', 'remoteCommand', 'git', 'spec', 'snippet')),
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
    id TEXT PRIMARY KEY NOT NULL, event_kind TEXT NOT NULL CHECK (event_kind IN ('remoteProbeSchedule', 'remoteProbeRefresh', 'feedback')),
    provider TEXT CHECK (provider IN ('history', 'remotePath', 'remoteCommand', 'git', 'spec', 'snippet')),
    target TEXT NOT NULL DEFAULT 'local' CHECK (target IN ('local', 'ssh', 'telnet', 'serial', 'dockerContainer')),
    decision TEXT NOT NULL CHECK (decision IN ('allowed', 'skipped', 'succeeded', 'failed', 'recorded')),
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
PRAGMA user_version = 2;
