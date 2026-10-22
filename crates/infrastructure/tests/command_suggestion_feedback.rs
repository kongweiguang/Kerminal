//! 命令建议反馈、审计与 SQLite 保留期适配集成测试。
//!
//! 测试只使用随机临时配置根和人工内容，不读取用户配置或真实命令历史。
//!
//! @author kongweiguang

use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_application::commands::CommandSuggestionFeedbackRepository;
use kerminal_domain::commands::{
    CommandHistoryTarget, CommandSuggestionFeedbackAction, CommandSuggestionFeedbackRequest,
    SuggestionProviderKind,
};
use kerminal_infrastructure::{
    command_history::SqliteCommandHistoryRepository, storage::StorageCoordinator,
};
use rusqlite::{Connection, params};
use uuid::Uuid;

const DAY_MS: i64 = 86_400_000;

/// 每个用例独占临时配置根，退出时仅删除本用例创建的文件。
struct TestRoot(PathBuf);

impl TestRoot {
    /// 通过随机目录隔离 command.sqlite、WAL 与 storage lock。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "kerminal-suggestion-feedback-test-{}",
            Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create isolated test root");
        Self(path)
    }

    /// 返回供 adapter 初始化使用的临时配置根。
    fn path(&self) -> &Path {
        &self.0
    }

    /// 返回当前测试数据库路径，便于只读断言持久化结果。
    fn database_file(&self) -> PathBuf {
        self.0.join("data").join("command.sqlite")
    }
}

impl Drop for TestRoot {
    /// 清理随机目录及 SQLite sidecar，不触碰测试根以外的内容。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 构造带人工候选文本的反馈请求，避免依赖 UI 或全局配置。
fn request(
    action: CommandSuggestionFeedbackAction,
    provider: SuggestionProviderKind,
    input: &str,
    replacement_text: &str,
) -> CommandSuggestionFeedbackRequest {
    CommandSuggestionFeedbackRequest {
        action,
        provider,
        replacement_text: replacement_text.to_owned(),
        input: input.to_owned(),
        target: CommandHistoryTarget::Ssh,
        source_id: Some("candidate-01".to_owned()),
        session_id: Some("session-01".to_owned()),
        pane_id: Some("pane-01".to_owned()),
        profile_id: Some("profile-01".to_owned()),
        remote_host_id: Some("host-01".to_owned()),
        cwd: Some("/srv/app".to_owned()),
        shell: Some("bash".to_owned()),
    }
}

/// 读取 SQLite UTC 毫秒时间，用于验证保留期边界而不依赖固定日期。
fn now_unix_millis() -> i64 {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch");
    i64::try_from(duration.as_millis()).expect("test clock fits SQLite integer")
}

/// 验证正常 accepted/dismissed 写入精确旧枚举，并与最小审计事件原子配对。
#[tokio::test]
async fn accepted_and_dismissed_feedback_write_minimal_audit_pairs() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");

    repository
        .record_feedback(request(
            CommandSuggestionFeedbackAction::Accepted,
            SuggestionProviderKind::RemotePath,
            "/srv",
            "/srv/app",
        ))
        .await
        .expect("record accepted feedback");
    repository
        .record_feedback(request(
            CommandSuggestionFeedbackAction::Dismissed,
            SuggestionProviderKind::RemoteCommand,
            "git st",
            "git status",
        ))
        .await
        .expect("record dismissed feedback");

    let connection = Connection::open(root.database_file()).expect("read database");
    let feedback_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_feedback",
            [],
            |row| row.get(0),
        )
        .expect("count feedback rows");
    let audit_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_audit_events WHERE event_kind = 'feedback'",
            [],
            |row| row.get(0),
        )
        .expect("count feedback audit rows");
    assert_eq!(feedback_count, 2);
    assert_eq!(audit_count, 2, "each feedback has one atomic audit row");

    let accepted = connection
        .query_row(
            "SELECT action, provider, target, replacement_text, input, remote_host_id
             FROM command_suggestion_feedback WHERE action = 'accepted'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                ))
            },
        )
        .expect("read accepted feedback");
    assert_eq!(
        accepted,
        (
            "accepted".to_owned(),
            "remotePath".to_owned(),
            "ssh".to_owned(),
            "/srv/app".to_owned(),
            "/srv".to_owned(),
            "host-01".to_owned(),
        )
    );

    let audit = connection
        .query_row(
            "SELECT provider, target, decision, reason, metadata_json, cwd, path
             FROM command_suggestion_audit_events WHERE event_kind = 'feedback' AND provider = 'remotePath'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .expect("read feedback audit");
    assert_eq!(audit.0, "remotePath");
    assert_eq!(audit.1, "ssh");
    assert_eq!(audit.2, "recorded");
    assert_eq!(audit.3, "feedback-recorded");
    assert_eq!(audit.4, r#"{"action":"accepted"}"#);
    assert_eq!(audit.5, "/srv/app");
    assert_eq!(audit.6, None, "candidate text is not copied to audit path");
    let audit_json: String = connection
        .query_row(
            "SELECT group_concat(metadata_json, '') FROM command_suggestion_audit_events",
            [],
            |row| row.get(0),
        )
        .expect("read audit metadata");
    assert!(!audit_json.contains("git status"));
    assert!(!audit_json.contains("/srv/app"));
}

/// 验证片段反馈只保存 `@snippet` 标记和安全 ID，不保留变量展开或输入正文。
#[tokio::test]
async fn snippet_feedback_discards_expanded_command_text() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");
    let mut feedback = request(
        CommandSuggestionFeedbackAction::Accepted,
        SuggestionProviderKind::Snippet,
        "echo api_key=manual-secret",
        "echo api_key=manual-secret",
    );
    feedback.source_id = Some("snippet-01".to_owned());

    repository
        .record_feedback(feedback)
        .await
        .expect("record redacted snippet feedback");

    let connection = Connection::open(root.database_file()).expect("read database");
    let row = connection
        .query_row(
            "SELECT replacement_text, input, source_id FROM command_suggestion_feedback",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .expect("read snippet feedback");
    assert_eq!(row.0, "@snippet:snippet-01");
    assert_eq!(row.1, "@snippet");
    assert_eq!(row.2.as_deref(), Some("snippet-01"));
    let all_rows = format!("{row:?}");
    assert!(!all_rows.contains("manual-secret"));
    assert!(!all_rows.contains("echo"));
}

/// 验证 adapter 边界再次拒绝敏感候选，并且反馈与审计都不留下行。
#[tokio::test]
async fn sensitive_non_snippet_feedback_is_rejected_without_persistence() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");
    let secret = "manual-api_key-marker";
    let error = repository
        .record_feedback(request(
            CommandSuggestionFeedbackAction::Accepted,
            SuggestionProviderKind::History,
            "echo ordinary",
            &format!("echo api_key={secret}"),
        ))
        .await
        .expect_err("sensitive candidate must be rejected");
    assert!(!format!("{error:?}").contains(secret));

    let connection = Connection::open(root.database_file()).expect("read database");
    for table in [
        "command_suggestion_feedback",
        "command_suggestion_audit_events",
    ] {
        let count: i64 = connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rows after rejected feedback");
        assert_eq!(count, 0, "sensitive feedback leaves no {table} row");
    }
}

/// 验证审计插入失败会回滚同一事务中的反馈行，不留下无法解释的半条记录。
#[tokio::test]
async fn audit_failure_rolls_back_feedback_insert() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");
    let connection = Connection::open(root.database_file()).expect("open database for trigger");
    connection
        .execute_batch(
            "CREATE TRIGGER reject_feedback_audit
             BEFORE INSERT ON command_suggestion_audit_events
             WHEN NEW.event_kind = 'feedback'
             BEGIN SELECT RAISE(ABORT, 'manual audit failure'); END;",
        )
        .expect("create deterministic audit failure");
    drop(connection);

    assert!(
        repository
            .record_feedback(request(
                CommandSuggestionFeedbackAction::Accepted,
                SuggestionProviderKind::History,
                "git st",
                "git status",
            ))
            .await
            .is_err()
    );

    let connection = Connection::open(root.database_file()).expect("verify rollback");
    let feedback_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_feedback",
            [],
            |row| row.get(0),
        )
        .expect("count rolled-back feedback");
    let audit_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_audit_events",
            [],
            |row| row.get(0),
        )
        .expect("count audit rows");
    assert_eq!(feedback_count, 0);
    assert_eq!(audit_count, 0);
}

/// 验证并发 async 调用串行提交到共享 connection，反馈与审计数量保持一致。
#[tokio::test]
async fn concurrent_feedback_writes_keep_audit_pairs_complete() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");
    let left = request(
        CommandSuggestionFeedbackAction::Accepted,
        SuggestionProviderKind::History,
        "git st",
        "git status",
    );
    let right = request(
        CommandSuggestionFeedbackAction::Dismissed,
        SuggestionProviderKind::RemoteCommand,
        "cargo t",
        "cargo test",
    );

    let (left, right) = tokio::join!(
        repository.record_feedback(left),
        repository.record_feedback(right)
    );
    left.expect("commit accepted feedback");
    right.expect("commit dismissed feedback");

    let connection = Connection::open(root.database_file()).expect("verify concurrent writes");
    let feedback_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_feedback",
            [],
            |row| row.get(0),
        )
        .expect("count feedback rows");
    let audit_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_audit_events WHERE event_kind = 'feedback'",
            [],
            |row| row.get(0),
        )
        .expect("count feedback audit rows");
    assert_eq!(feedback_count, 2);
    assert_eq!(audit_count, 2);
}

/// 验证 UTC 毫秒清理与 1/3650 天边界，反馈和所有过期审计同事务删除。
#[tokio::test]
async fn retention_prunes_expired_rows_with_clamped_day_ranges() {
    let root = TestRoot::new();
    let repository =
        SqliteCommandHistoryRepository::open(StorageCoordinator::new(root.path().to_path_buf()))
            .expect("open command history database");
    let now = now_unix_millis();
    let connection = Connection::open(root.database_file()).expect("seed database");
    for (id, created_at) in [
        ("feedback-old", now - 2 * DAY_MS),
        ("feedback-new", now - 3_600_000),
    ] {
        connection
            .execute(
                "INSERT INTO command_suggestion_feedback (
                    id, action, provider, target, replacement_text, input, created_at_unix_ms
                 ) VALUES (?1, 'accepted', 'history', 'local', 'echo safe', 'echo', ?2)",
                params![id, created_at],
            )
            .expect("seed feedback row");
    }
    for (id, event_kind, created_at) in [
        ("audit-old", "feedback", now - 11 * 365 * DAY_MS),
        ("audit-new", "remoteProbeRefresh", now - DAY_MS),
    ] {
        connection
            .execute(
                "INSERT INTO command_suggestion_audit_events (
                    id, event_kind, target, decision, created_at_unix_ms
                 ) VALUES (?1, ?2, 'local', 'recorded', ?3)",
                params![id, event_kind, created_at],
            )
            .expect("seed audit row");
    }
    drop(connection);

    let result = repository
        .prune_expired(0, u32::MAX)
        .await
        .expect("prune with bounded retention");
    assert_eq!(result.feedback_deleted, 1, "zero days clamps to one day");
    assert_eq!(
        result.audit_deleted, 1,
        "audit retention clamps to 3650 days"
    );

    let connection = Connection::open(root.database_file()).expect("verify database");
    let remaining_feedback: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_feedback",
            [],
            |row| row.get(0),
        )
        .expect("count retained feedback");
    let remaining_audit: i64 = connection
        .query_row(
            "SELECT count(*) FROM command_suggestion_audit_events",
            [],
            |row| row.get(0),
        )
        .expect("count retained audit");
    assert_eq!(remaining_feedback, 1);
    assert_eq!(remaining_audit, 1);
}

/// 验证 future schema 的只读恢复状态不会被反馈或 retention 操作伪装成成功。
#[tokio::test]
async fn unavailable_future_schema_rejects_feedback_and_retention() {
    let root = TestRoot::new();
    let database_file = root.database_file();
    fs::create_dir_all(database_file.parent().expect("database parent"))
        .expect("create database parent");
    let future = Connection::open(&database_file).expect("create future database");
    future
        .pragma_update(None, "user_version", 4_i64)
        .expect("set future schema");
    drop(future);

    let repository = SqliteCommandHistoryRepository::open_or_unavailable(StorageCoordinator::new(
        root.path().to_path_buf(),
    ));
    assert!(
        repository
            .record_feedback(request(
                CommandSuggestionFeedbackAction::Accepted,
                SuggestionProviderKind::History,
                "git st",
                "git status",
            ))
            .await
            .is_err()
    );
    assert!(repository.prune_expired(30, 365).await.is_err());
    let connection = Connection::open(database_file).expect("reopen future database");
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("read unchanged version");
    assert_eq!(version, 4);
}
