//! 迁移 journal 原子发布与失败结果的隔离测试。
//!
//! @author kongweiguang

use std::{fs, io, path::Path};

use kerminal_application::AppError;
use uuid::Uuid;

use super::*;

/// 构造最小但通过实际校验的 journal，用于注入原子发布阶段结果。
fn valid_journal() -> MigrationJournal {
    let settings = FileEdit {
        relative_path: "settings.toml".into(),
        before: None,
        after: b"theme = \"dark\"\n".to_vec(),
    };
    let marker_edit = FileEdit {
        relative_path: marker::MARKER_FILE_NAME.into(),
        before: None,
        after: marker::current_marker_bytes(),
    };
    MigrationJournal {
        schema_version: JOURNAL_SCHEMA,
        operation_id: Uuid::new_v4().to_string(),
        state: JournalState::Prepared,
        files: vec![file_entry(&settings), file_entry(&marker_edit)],
        sqlite: None,
    }
}

/// 收集 journal 根目录内的临时文件名，确认清理只触及本次随机 staging。
fn temporary_names(directory: &Path) -> Vec<String> {
    fs::read_dir(directory)
        .expect("迁移目录存在")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with(".active-") && name.ends_with(".tmp"))
        .collect()
}

/// 明确未提交时清理当前 staging，且不得移除目录内其他操作留下的文件。
#[test]
fn not_committed_removes_only_owned_active_stage() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let journal = valid_journal();
    let migration_root = root.path().join(MIGRATION_DIRECTORY);
    ensure_directory(root.path(), &migration_root).expect("迁移目录可创建");
    let unrelated_stage = migration_root.join(".active-preserve.tmp");
    fs::write(&unrelated_stage, b"another writer evidence").expect("预置其他 staging");

    let result = write_journal_with_commit(root.path(), &journal, true, |_, _, _| {
        Err(AtomicCommitError::NotCommitted(io::Error::other(
            "injected",
        )))
    });

    assert!(matches!(result, Err(AppError::Infrastructure { .. })));
    assert_eq!(
        temporary_names(&migration_root),
        vec![String::from(".active-preserve.tmp")]
    );
    assert_eq!(
        fs::read(unrelated_stage).expect("其他 staging 保留"),
        b"another writer evidence"
    );
}

/// 未知且无法回读确认时保留唯一 staging，避免把可能已发布的 journal 变成不可恢复状态。
#[test]
fn unknown_without_matching_readback_preserves_active_stage() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let journal = valid_journal();

    let result = write_journal_with_commit(root.path(), &journal, true, |_, _, _| {
        Err(AtomicCommitError::CommitUnknown(io::Error::other(
            "injected",
        )))
    });

    assert!(matches!(result, Err(AppError::OutcomeUnknown { .. })));
    let names = temporary_names(&root.path().join(MIGRATION_DIRECTORY));
    assert_eq!(names.len(), 1);
    assert!(
        fs::read(root.path().join(MIGRATION_DIRECTORY).join(&names[0]))
            .expect("未知结果 staging 内容保留")
            .starts_with(b"{\"schema_version\":1")
    );
}

/// 原子替换已落盘但目录同步结果未知时，完整回读相等即可确认发布成功。
#[test]
fn unknown_with_matching_readback_is_confirmed_committed() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let journal = valid_journal();

    let result = write_journal_with_commit(root.path(), &journal, true, |stage, target, _| {
        fs::rename(stage, target).map_err(AtomicCommitError::NotCommitted)?;
        Err(AtomicCommitError::CommitUnknown(io::Error::other(
            "injected",
        )))
    });

    assert!(result.is_ok());
    assert!(temporary_names(&root.path().join(MIGRATION_DIRECTORY)).is_empty());
    let actual = read_active_journal(root.path())
        .expect("active journal 可读取")
        .expect("active journal 已发布");
    assert!(same_journal(&actual, &journal));
}

/// active 文件损坏或其 UUID 目录丢失时必须阻断，不能误判为无迁移记录。
#[test]
fn malformed_active_and_missing_operation_are_blocked() {
    let root = tempfile::tempdir().expect("隔离迁移根可创建");
    let migration_root = root.path().join(MIGRATION_DIRECTORY);
    ensure_directory(root.path(), &migration_root).expect("迁移目录可创建");
    fs::create_dir(migration_root.join(ACTIVE_JOURNAL)).expect("用目录模拟非法 active");
    assert!(
        !inspect_journals(root.path())
            .expect("扫描返回状态")
            .blockers
            .is_empty()
    );

    fs::remove_dir(migration_root.join(ACTIVE_JOURNAL)).expect("移除非法 active");
    let journal = valid_journal();
    fs::write(
        migration_root.join(ACTIVE_JOURNAL),
        serde_json::to_vec(&journal).expect("journal 可序列化"),
    )
    .expect("创建有效 active 内容");
    assert!(
        !inspect_journals(root.path())
            .expect("扫描返回状态")
            .blockers
            .is_empty()
    );
}

/// 已发布旧契约 journal 的 after 摘要固定不变，不能为了新版本恢复而重写原计划。
#[test]
fn previous_marker_digest_is_accepted_only_as_the_original_contract() {
    let mut journal = valid_journal();
    let marker = journal.files.last_mut().expect("存在 marker 项");
    marker.after_sha256 = sha256(&marker::previous_marker_bytes());
    assert!(validate_journal(&journal));
    journal
        .files
        .last_mut()
        .expect("存在 marker 项")
        .before_sha256 = Some(sha256(b"old"));
    assert!(!validate_journal(&journal));
    journal
        .files
        .last_mut()
        .expect("存在 marker 项")
        .before_sha256 = None;
    journal
        .files
        .last_mut()
        .expect("存在 marker 项")
        .after_sha256 = sha256(b"data_format = 4\nmigration_contract = 999\n");
    assert!(!validate_journal(&journal));
}

/// 第二次升级仅替换 active，历史 UUID 目录必须有完全相等的完成收据才能继续保留。
#[test]
fn archived_committed_operations_are_not_mistaken_for_orphans() {
    let root = tempfile::tempdir().expect("创建隔离根");
    let mut previous = valid_journal();
    previous.state = JournalState::Committed;
    ensure_directory(
        root.path(),
        &operation_directory(root.path(), &previous.operation_id),
    )
    .expect("创建旧操作目录");
    write_journal(root.path(), &previous, true).expect("保存旧 active");
    archive_committed_journal(root.path(), &previous).expect("旧收据原子保存");
    let next = valid_journal();
    ensure_directory(
        root.path(),
        &operation_directory(root.path(), &next.operation_id),
    )
    .expect("创建新操作目录");
    replace_archived_journal(root.path(), &previous, &next).expect("已归档 active 可替换");

    let scan = inspect_journals(root.path()).expect("连续事务可扫描");
    assert!(scan.blockers.is_empty());
    assert!(scan.recovery_pending);
    let receipt = operation_directory(root.path(), &previous.operation_id).join(COMMITTED_RECEIPT);
    fs::write(&receipt, b"{}").expect("注入被篡改收据");
    assert!(
        !inspect_journals(root.path())
            .expect("坏收据可扫描")
            .blockers
            .is_empty()
    );
    assert_eq!(fs::read(receipt).expect("保留坏收据供恢复"), b"{}");
}

/// 旧摘要只作为原字节备份和 CAS 哨兵，journal 不能借此改写历史运行记录。
#[test]
fn legacy_forward_source_must_be_a_noop_cas_entry() {
    let mut journal = valid_journal();
    journal.files.insert(
        0,
        FileJournalEntry {
            relative_path: "data/port-forwards/sessions.json".to_owned(),
            before_sha256: Some(sha256(b"old summary")),
            after_sha256: sha256(b"old summary"),
        },
    );
    assert!(validate_journal(&journal));
    journal.files[0].after_sha256 = sha256(b"rewritten summary");
    assert!(!validate_journal(&journal));
}
