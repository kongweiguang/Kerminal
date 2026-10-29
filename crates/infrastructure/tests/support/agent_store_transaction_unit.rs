//! Agent workspace journal 恢复测试；临时目录隔离且不接触用户配置根。
//!
//! @author kongweiguang

use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use kerminal_application::agents::AgentWorkspaceDocumentKind;
use kerminal_domain::identity::AgentSessionId;
use uuid::Uuid;

use super::*;

struct TestRoot(PathBuf);

impl TestRoot {
    /// 使用唯一临时路径承载故障恢复样本，避免测试共享或删除用户数据。
    fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time follows the Unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "kerminal-agent-journal-{timestamp}-{}",
            Uuid::new_v4()
        ));
        fs::create_dir_all(&path).expect("temporary root is created");
        Self(path)
    }
}

impl Drop for TestRoot {
    /// 仅删除本测试通过随机名创建的临时根目录。
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// 新 session 显式保存权限兼容字段；旧 session 缺字段时从 CLI argv 恢复相同模式。
///
/// @author kongweiguang
#[test]
fn permission_mode_round_trips_and_legacy_files_infer_from_launch_args() {
    use kerminal_domain::agents::{
        AgentLaunchPermissionMode, AgentProvider, AgentSession, AgentSessionLaunch,
        AgentSessionScope,
    };

    let session = AgentSession::new(
        AgentSessionId::new(),
        AgentProvider::Codex,
        None,
        "Codex".to_owned(),
        "2026-10-27T00:00:00Z".to_owned(),
        "C:\\Users\\test\\.kerminal".to_owned(),
        "C:\\Users\\test\\.kerminal\\agents\\session".to_owned(),
        AgentSessionLaunch {
            command_label: "Codex".to_owned(),
            shell: "codex".to_owned(),
            args: vec!["--dangerously-bypass-approvals-and-sandbox".to_owned()],
            cwd: "C:\\Users\\test\\.kerminal\\agents\\session".to_owned(),
        },
        Some(AgentSessionScope::Global),
        None,
    )
    .expect("session launch snapshot is valid");
    let encoded = encode_agent_session_document(&session).expect("session encodes");
    let encoded_text = String::from_utf8(encoded).expect("TOML is UTF-8");
    assert!(encoded_text.contains("permission_mode = \"skipPermissions\""));

    let old_text = encoded_text.replace("permission_mode = \"skipPermissions\"\n", "");
    let restored = toml::from_str::<AgentSessionFile>(&old_text)
        .expect("legacy session without compatibility field decodes")
        .restore()
        .expect("legacy session restores from launch argv");
    assert_eq!(
        restored.permission_mode,
        AgentLaunchPermissionMode::SkipPermissions
    );
}

/// 建立 store 与 session 目录并返回 canonical 根，journal 路径仍由实现固定生成。
async fn prepare_store() -> (
    TestRoot,
    AgentSessionStore,
    AgentSessionId,
    PathBuf,
    PathBuf,
) {
    let root = TestRoot::new();
    let store = AgentSessionStore::new(root.0.clone());
    let id = AgentSessionId::new();
    let session_root = store
        .create_session_root(&id)
        .await
        .expect("session directory is created");
    let canonical_root = fs::canonicalize(&root.0).expect("root canonicalizes");
    (root, store, id, canonical_root, session_root)
}

/// 生成固定 kind 对应的同目录兄弟 staging，并将正文和目录项同步后再建 journal。
fn staged_entry(
    session_root: &std::path::Path,
    kind: AgentWorkspaceDocumentKind,
    before: Option<&[u8]>,
    after: &[u8],
) -> (WorkspaceTransactionEntry, PathBuf, PathBuf) {
    let target = session_root.join(relative_document_path(kind).iter().collect::<PathBuf>());
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent).expect("target parent is created");
    }
    if let Some(before) = before {
        fs::write(&target, before).expect("before document is written");
    }
    let stage = stage_path_for(&target, &Uuid::new_v4()).expect("stage path is generated");
    write_new_synced_file(&stage, after).expect("stage content is synced");
    sync_directory(stage.parent().expect("stage has a parent")).expect("stage entry is synced");
    let entry = WorkspaceTransactionEntry {
        kind: document_kind_id(kind).to_owned(),
        target: relative_document_path_string(kind),
        staging: relative_to_session(session_root, &stage).expect("stage is in session root"),
        before_sha256: before.map(sha256_hex),
        after_sha256: sha256_hex(after),
    };
    (entry, target, stage)
}

/// 进程在一次提交后退出时，下一次读取按 journal 前滚其余文件并清除恢复材料。
#[tokio::test]
async fn prepared_journal_rolls_forward_partial_commit() {
    let (_root, _store, id, canonical_root, session_root) = prepare_store().await;
    let (first, first_target, first_stage) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::AgentInstructions,
        Some(b"old instructions"),
        b"new instructions",
    );
    let (second, second_target, second_stage) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::ClaudeInstructions,
        Some(b"old Claude rules"),
        b"new Claude rules",
    );
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: None,
        entries: vec![first, second],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("prepared journal is durable");
    commit_staged_file(&first_stage, &first_target, CommitMode::ReplaceExisting)
        .expect("first document simulates the completed prefix");

    recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect("next access completes the transaction");

    assert_eq!(
        fs::read(&first_target).expect("first file remains"),
        b"new instructions"
    );
    assert_eq!(
        fs::read(&second_target).expect("second file is recovered"),
        b"new Claude rules"
    );
    assert!(!first_stage.exists());
    assert!(!second_stage.exists());
    assert!(!session_root.join(JOURNAL_FILE).exists());
}

/// 每个 Prepared entry 遇到第三种目标 hash 都必须停下，并保留未完成事务与外部正文。
#[tokio::test]
async fn third_hash_conflict_preserves_recovery_material() {
    for conflict_index in 0..2 {
        let (_root, _store, id, canonical_root, session_root) = prepare_store().await;
        let (first, first_target, first_stage) = staged_entry(
            &session_root,
            AgentWorkspaceDocumentKind::AgentInstructions,
            Some(b"before first"),
            b"prepared first",
        );
        let (second, second_target, second_stage) = staged_entry(
            &session_root,
            AgentWorkspaceDocumentKind::ClaudeInstructions,
            Some(b"before second"),
            b"prepared second",
        );
        let journal = WorkspaceTransactionJournal {
            schema_version: JOURNAL_SCHEMA_VERSION,
            agent_session_id: id.clone(),
            phase: WorkspaceTransactionPhase::Prepared,
            resume_attempt_id: None,
            entries: vec![first, second],
        };
        write_journal(&session_root, &journal, CommitMode::CreateNew)
            .expect("prepared journal is durable");
        let journal_path = session_root.join(JOURNAL_FILE);
        let journal_bytes = fs::read(&journal_path).expect("journal bytes are readable");

        // 第二项冲突时，第一项代表已提交且 staging 已被 rename 移走的前缀。
        if conflict_index == 1 {
            commit_staged_file(&first_stage, &first_target, CommitMode::ReplaceExisting)
                .expect("first document simulates the committed prefix");
        }
        let (conflict_target, external_contents) = if conflict_index == 0 {
            (&first_target, b"external first edit".as_slice())
        } else {
            (&second_target, b"external second edit".as_slice())
        };
        fs::write(conflict_target, external_contents).expect("external edit is simulated");

        let result = recover_workspace_transaction(&canonical_root, &session_root, &id);

        assert_eq!(
            result
                .expect_err("conflicting content blocks automatic recovery")
                .public_code(),
            kerminal_application::PublicErrorCode::Conflict
        );
        assert_eq!(
            fs::read(&journal_path).expect("conflict retains original journal"),
            journal_bytes
        );

        let targets = [&first_target, &second_target];
        let stages = [&first_stage, &second_stage];
        let before = [b"before first".as_slice(), b"before second".as_slice()];
        let after = [b"prepared first".as_slice(), b"prepared second".as_slice()];
        for index in 0..2 {
            if index < conflict_index {
                assert_eq!(
                    fs::read(targets[index])
                        .expect("committed target is readable")
                        .as_slice(),
                    after[index]
                );
                assert!(!stages[index].exists(), "committed stage was renamed away");
            } else if index == conflict_index {
                assert_eq!(
                    fs::read(targets[index])
                        .expect("external target remains readable")
                        .as_slice(),
                    external_contents
                );
                assert!(stages[index].is_file(), "conflicting stage is preserved");
            } else {
                assert_eq!(
                    fs::read(targets[index])
                        .expect("uncommitted target remains")
                        .as_slice(),
                    before[index]
                );
                assert!(stages[index].is_file(), "later stage is preserved");
            }
        }
    }
}

/// 崩溃发生在 Prepared 前时仅清理由 journal 登记的 staging，目标文档保持原样。
#[tokio::test]
async fn preparing_journal_removes_only_its_partial_staging() {
    let (_root, _store, id, canonical_root, session_root) = prepare_store().await;
    let (entry, target, stage) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::AgentInstructions,
        Some(b"user content"),
        b"incomplete prepared output",
    );
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Preparing,
        resume_attempt_id: None,
        entries: vec![entry],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("preparing journal is durable");

    recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect("preparing transaction is safely discarded");

    assert_eq!(
        fs::read(&target).expect("user document remains"),
        b"user content"
    );
    assert!(!stage.exists());
    assert!(!session_root.join(JOURNAL_FILE).exists());
}

/// Windows 用真实写句柄恢复已提交目标，随后确认普通读写都能继续推进事务。
#[cfg(windows)]
#[tokio::test]
async fn committed_target_with_leftover_journal_recovers_for_next_read_and_write() {
    let (_root, store, id, _canonical_root, session_root) = prepare_store().await;
    let kind = AgentWorkspaceDocumentKind::McpEndpointContext;
    let before = br#"{"schemaVersion":1,"endpoint":null}"#;
    let after = br#"{"schemaVersion":1,"endpoint":"http://127.0.0.1:40137/mcp"}"#;
    let (entry, target, stage) = staged_entry(&session_root, kind, Some(before), after);
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: None,
        entries: vec![entry],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("prepared journal is durable");
    commit_staged_file(&stage, &target, CommitMode::ReplaceExisting)
        .expect("the target commit is complete but cleanup has not run");
    assert!(!stage.exists(), "rename consumed the staging file");

    let (contents, revision) = store
        .read_document_blocking(&id, kind, &target)
        .expect("the next read completes recovery");
    assert_eq!(
        contents.as_deref(),
        Some(std::str::from_utf8(after).unwrap())
    );
    assert!(!session_root.join(JOURNAL_FILE).exists());

    let updated = br#"{"schemaVersion":1,"endpoint":"http://127.0.0.1:40138/mcp"}"#;
    store
        .write_files_blocking(
            &id,
            vec![AgentWorkspaceFile {
                kind: AgentWorkspaceFileKind::Document(kind),
                path: target.clone(),
                contents: updated.to_vec(),
                expected_revision: revision,
            }],
        )
        .expect("a CAS write succeeds after recovery");
    assert_eq!(
        fs::read(&target).expect("updated document is readable"),
        updated
    );
    assert!(!session_root.join(JOURNAL_FILE).exists());
}

/// 写权限暂不可得时继续报告未知并保留 journal，释放占用后同一事务可重试恢复。
#[cfg(windows)]
#[tokio::test]
async fn committed_target_flush_failure_retains_journal_for_retry() {
    use std::os::windows::fs::OpenOptionsExt;

    const FILE_SHARE_READ: u32 = 0x0000_0001;

    let (_root, _store, id, canonical_root, session_root) = prepare_store().await;
    let kind = AgentWorkspaceDocumentKind::McpEndpointContext;
    let before = br#"{"schemaVersion":1,"endpoint":null}"#;
    let after = br#"{"schemaVersion":1,"endpoint":"http://127.0.0.1:40137/mcp"}"#;
    let (entry, target, stage) = staged_entry(&session_root, kind, Some(before), after);
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: None,
        entries: vec![entry],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("prepared journal is durable");
    commit_staged_file(&stage, &target, CommitMode::ReplaceExisting)
        .expect("the target commit is complete but cleanup has not run");
    let journal_path = session_root.join(JOURNAL_FILE);
    let journal_bytes = fs::read(&journal_path).expect("journal bytes are readable");
    let target_was_readonly = fs::metadata(&target)
        .expect("committed target metadata is readable")
        .permissions()
        .readonly();
    let read_handle = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(&target)
        .expect("test handle allows reads while denying concurrent writes");

    let error = recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect_err("write-handle flush failure must leave recovery pending");
    let target_remains_readonly = fs::metadata(&target)
        .expect("failed recovery leaves target metadata readable")
        .permissions()
        .readonly();
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert_eq!(
        fs::read(&journal_path).expect("failed recovery retains journal"),
        journal_bytes
    );
    assert_eq!(fs::read(&target).expect("committed target remains"), after);
    assert_eq!(target_remains_readonly, target_was_readonly);

    drop(read_handle);
    recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect("the same journal is recoverable after write access returns");
    assert!(!journal_path.exists());
    assert_eq!(fs::read(&target).expect("committed target remains"), after);
}

/// Windows 目标文档为只读时保留提交证据与原权限，解除限制后可恢复同一事务。
#[cfg(windows)]
#[tokio::test]
async fn committed_readonly_target_retains_journal_and_permissions_for_retry() {
    let (_root, _store, id, canonical_root, session_root) = prepare_store().await;
    let kind = AgentWorkspaceDocumentKind::McpEndpointContext;
    let before = br#"{"schemaVersion":1,"endpoint":null}"#;
    let after = br#"{"schemaVersion":1,"endpoint":"http://127.0.0.1:40137/mcp"}"#;
    let (entry, target, stage) = staged_entry(&session_root, kind, Some(before), after);
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: None,
        entries: vec![entry],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("prepared journal is durable");
    commit_staged_file(&stage, &target, CommitMode::ReplaceExisting)
        .expect("the target commit is complete but cleanup has not run");

    let journal_path = session_root.join(JOURNAL_FILE);
    let journal_bytes = fs::read(&journal_path).expect("journal bytes are readable");
    let original_permissions = fs::metadata(&target)
        .expect("committed target metadata is readable")
        .permissions();
    let mut readonly_permissions = original_permissions.clone();
    readonly_permissions.set_readonly(true);
    fs::set_permissions(&target, readonly_permissions).expect("temporary target becomes read-only");

    let recovery = recover_workspace_transaction(&canonical_root, &session_root, &id);
    let readonly_after_recovery =
        fs::metadata(&target).map(|metadata| metadata.permissions().readonly());
    let restore_permissions = fs::set_permissions(&target, original_permissions);

    restore_permissions.expect("restore only the temporary test file permissions");
    let error = recovery.expect_err("read-only target must keep recovery pending");
    assert_eq!(
        error.public_code(),
        kerminal_application::PublicErrorCode::OutcomeUnknown
    );
    assert!(
        readonly_after_recovery.expect("target metadata remains readable"),
        "recovery must not change the user's target permissions"
    );
    assert_eq!(
        fs::read(&journal_path).expect("failed recovery retains journal"),
        journal_bytes
    );
    assert_eq!(fs::read(&target).expect("committed target remains"), after);

    recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect("the same journal is recoverable after write access returns");
    assert!(!journal_path.exists());
    assert_eq!(fs::read(&target).expect("committed target remains"), after);
}

/// 恢复真实的 Prepared checkpoint journal 时核验 after hashes，并让最后的 Committed marker 一起前滚。
///
/// @author kongweiguang
#[test]
fn resume_checkpoint_journal_recovers_intended_files_and_committed_marker() {
    let root = TestRoot::new();
    let store = AgentSessionStore::new(root.0.clone());
    let id = AgentSessionId::new();
    let session_root = store.session_root(&id).expect("session path is safe");
    fs::create_dir_all(&session_root).expect("isolated session directory is created");
    let canonical_root = fs::canonicalize(&root.0).expect("root canonicalizes");

    let session_before = b"base session bytes";
    let session_after = b"intended session bytes";
    let provider_before = b"base provider bytes";
    let provider_after = b"intended provider bytes";
    let (session_entry, session_target, session_stage) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::Session,
        Some(session_before),
        session_after,
    );
    let (provider_entry, provider_target, _) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::Provider,
        Some(provider_before),
        provider_after,
    );
    let attempt_id = Uuid::new_v4();
    let prepared_checkpoint = AgentResumeCheckpointFile::from_checkpoint(
        &id,
        AgentResumeCheckpoint {
            attempt_id,
            terminal_session_id: kerminal_domain::identity::SessionId::new(),
            base_session_revision: revision_for_bytes(session_before),
            base_provider_revision: revision_for_bytes(provider_before),
            intended_session_revision: revision_for_bytes(session_after),
            intended_provider_revision: revision_for_bytes(provider_after),
            phase: AgentResumeCheckpointPhase::Prepared,
        },
    )
    .expect("typed checkpoint hashes are valid");
    let prepared_bytes = encode_resume_checkpoint_file(&prepared_checkpoint)
        .expect("prepared checkpoint serializes");
    let mut committed_checkpoint = prepared_checkpoint;
    committed_checkpoint.phase = AgentResumeCheckpointFilePhase::Committed;
    let committed_bytes = encode_resume_checkpoint_file(&committed_checkpoint)
        .expect("committed checkpoint serializes");
    let checkpoint_target = session_root.join(RESUME_CHECKPOINT_FILE);
    fs::write(&checkpoint_target, &prepared_bytes).expect("prepared marker is durable");
    let checkpoint_stage = stage_path_for(&checkpoint_target, &Uuid::new_v4())
        .expect("checkpoint staging path is generated");
    write_new_synced_file(&checkpoint_stage, &committed_bytes)
        .expect("committed marker staging is synced");
    let checkpoint_entry = WorkspaceTransactionEntry {
        kind: "resume_checkpoint".to_owned(),
        target: RESUME_CHECKPOINT_FILE.to_owned(),
        staging: relative_to_session(&session_root, &checkpoint_stage)
            .expect("checkpoint stage is contained"),
        before_sha256: Some(sha256_hex(&prepared_bytes)),
        after_sha256: sha256_hex(&committed_bytes),
    };
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: Some(attempt_id),
        entries: vec![session_entry, provider_entry, checkpoint_entry],
    };
    write_journal(&session_root, &journal, CommitMode::CreateNew)
        .expect("resume journal is durable");
    commit_staged_file(&session_stage, &session_target, CommitMode::ReplaceExisting)
        .expect("the transaction has a committed prefix before recovery");

    recover_workspace_transaction(&canonical_root, &session_root, &id)
        .expect("the same journal rolls forward remaining intended files");

    assert_eq!(
        fs::read(&session_target).expect("session is readable"),
        session_after
    );
    assert_eq!(
        fs::read(&provider_target).expect("provider is readable"),
        provider_after
    );
    let snapshot = read_resume_checkpoint_snapshot(&canonical_root, &session_root, &id)
        .expect("committed checkpoint validates")
        .expect("committed checkpoint remains");
    assert_eq!(snapshot.checkpoint.attempt_id, attempt_id);
    assert_eq!(
        snapshot.checkpoint.phase,
        AgentResumeCheckpointPhase::Committed
    );
    assert!(!session_root.join(JOURNAL_FILE).exists());
}

/// 缺少新增 attemptId 字段的旧 journal 必须继续反序列化为普通 workspace transaction。
///
/// @author kongweiguang
#[test]
fn legacy_journal_without_resume_attempt_id_remains_compatible() {
    let id = AgentSessionId::new();
    let bytes = serde_json::json!({
        "schemaVersion": JOURNAL_SCHEMA_VERSION,
        "agentSessionId": id,
        "phase": "prepared",
        "entries": []
    });
    let journal = serde_json::from_value::<WorkspaceTransactionJournal>(bytes)
        .expect("legacy journal without attemptId still parses");
    assert_eq!(journal.resume_attempt_id, None);
}

/// 带 attempt 的新 journal 必须严格使用 Session、Provider、Committed checkpoint 的顺序。
///
/// @author kongweiguang
#[tokio::test]
async fn resume_journal_requires_committed_checkpoint_last() {
    let (_root, _store, id, _canonical_root, session_root) = prepare_store().await;
    let (session, _, _) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::Session,
        Some(b"old session"),
        b"new session",
    );
    let (provider, _, _) = staged_entry(
        &session_root,
        AgentWorkspaceDocumentKind::Provider,
        Some(b"old provider"),
        b"new provider",
    );
    let checkpoint_target = session_root.join(RESUME_CHECKPOINT_FILE);
    let checkpoint_stage = stage_path_for(&checkpoint_target, &Uuid::new_v4())
        .expect("checkpoint staging path is generated");
    let checkpoint = WorkspaceTransactionEntry {
        kind: "resume_checkpoint".to_owned(),
        target: RESUME_CHECKPOINT_FILE.to_owned(),
        staging: relative_to_session(&session_root, &checkpoint_stage)
            .expect("checkpoint stage stays inside session root"),
        before_sha256: Some(sha256_hex(b"prepared marker")),
        after_sha256: sha256_hex(b"committed marker"),
    };
    let journal = WorkspaceTransactionJournal {
        schema_version: JOURNAL_SCHEMA_VERSION,
        agent_session_id: id.clone(),
        phase: WorkspaceTransactionPhase::Prepared,
        resume_attempt_id: Some(Uuid::new_v4()),
        entries: vec![session.clone(), provider.clone(), checkpoint.clone()],
    };
    validate_journal(&journal, &session_root, &id)
        .expect("committed checkpoint is valid only as final journal entry");

    let reordered = WorkspaceTransactionJournal {
        entries: vec![session, checkpoint, provider],
        ..journal
    };
    assert!(validate_journal(&reordered, &session_root, &id).is_err());
}
